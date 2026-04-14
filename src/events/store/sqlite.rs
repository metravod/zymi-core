use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::events::{Event, EventStoreError};

use super::event_store::{EventStore, TailedEvent};

/// SQLite-backed event store. Persists every event with a hash chain
/// per stream and an autoincrement global cursor used by the watcher.
pub struct SqliteEventStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteEventStore {
    pub fn new(path: &Path) -> Result<Self, EventStoreError> {
        let conn =
            Connection::open(path).map_err(|e| EventStoreError::Connection(e.to_string()))?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;",
        )
        .map_err(|e| EventStoreError::Connection(e.to_string()))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id TEXT NOT NULL UNIQUE,
                 stream_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL,
                 timestamp TEXT NOT NULL,
                 kind_tag TEXT NOT NULL,
                 data TEXT NOT NULL,
                 correlation_id TEXT,
                 causation_id TEXT,
                 source TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_events_stream_seq
                 ON events(stream_id, sequence);
             CREATE INDEX IF NOT EXISTS idx_events_correlation
                 ON events(correlation_id);
             CREATE INDEX IF NOT EXISTS idx_events_kind
                 ON events(kind_tag);",
        )
        .map_err(|e| EventStoreError::Connection(e.to_string()))?;

        // Migration: add hash chain columns if they don't exist yet
        let has_hash: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('events') WHERE name = 'hash'")
            .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
            .map(|c| c > 0)
            .unwrap_or(false);

        if !has_hash {
            conn.execute_batch(
                "ALTER TABLE events ADD COLUMN prev_hash TEXT NOT NULL DEFAULT '';
                 ALTER TABLE events ADD COLUMN hash TEXT NOT NULL DEFAULT '';",
            )
            .map_err(|e| EventStoreError::Connection(e.to_string()))?;
        }

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Create from an existing shared connection. Caller must ensure the events table exists.
    pub fn from_connection(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }
}

/// Compute SHA-256(event_id || data || prev_hash) as hex string.
fn compute_hash(event_id: &str, data: &str, prev_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(event_id.as_bytes());
    hasher.update(data.as_bytes());
    hasher.update(prev_hash.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[async_trait]
impl EventStore for SqliteEventStore {
    async fn append(&self, event: &mut Event) -> Result<u64, EventStoreError> {
        let conn = self.conn.clone();
        let event_id = event.id.to_string();
        let stream_id = event.stream_id.clone();
        let timestamp = event.timestamp.to_rfc3339();
        let kind_tag = event.kind_tag().to_string();
        let data = serde_json::to_string(&event)
            .map_err(|e| EventStoreError::Serialization(e.to_string()))?;
        let correlation_id = event.correlation_id.map(|u| u.to_string());
        let causation_id = event.causation_id.map(|u| u.to_string());
        let source = event.source.clone();

        let seq = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            // IMMEDIATE transaction: acquires a write lock upfront, preventing
            // race conditions on sequence assignment and hash chain reads.
            conn.execute_batch("BEGIN IMMEDIATE")
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let result = (|| -> Result<u64, EventStoreError> {
                // Assign next sequence + get prev_hash in a single query
                let (next_seq, prev_hash): (u64, String) = conn
                    .query_row(
                        "SELECT COALESCE(MAX(sequence), 0) + 1, COALESCE(
                            (SELECT hash FROM events WHERE stream_id = ?1 ORDER BY sequence DESC LIMIT 1),
                            ''
                         ) FROM events WHERE stream_id = ?1",
                        [&stream_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(|e| EventStoreError::Connection(e.to_string()))?;

                let hash = compute_hash(&event_id, &data, &prev_hash);

                conn.execute(
                    "INSERT INTO events (event_id, stream_id, sequence, timestamp, kind_tag, data, correlation_id, causation_id, source, prev_hash, hash)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    rusqlite::params![
                        event_id,
                        stream_id,
                        next_seq,
                        timestamp,
                        kind_tag,
                        data,
                        correlation_id,
                        causation_id,
                        source,
                        prev_hash,
                        hash,
                    ],
                )
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

                Ok(next_seq)
            })();

            match &result {
                Ok(_) => conn.execute_batch("COMMIT")
                    .map_err(|e| EventStoreError::Connection(e.to_string()))?,
                Err(_) => { let _ = conn.execute_batch("ROLLBACK"); }
            }

            result
        })
        .await
        .map_err(|e| EventStoreError::Connection(e.to_string()))??;

        event.sequence = seq;
        Ok(seq)
    }

    async fn read_stream(
        &self,
        stream_id: &str,
        from_seq: u64,
    ) -> Result<Vec<Event>, EventStoreError> {
        let conn = self.conn.clone();
        let stream_id = stream_id.to_owned();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT data, sequence FROM events WHERE stream_id = ?1 AND sequence >= ?2 ORDER BY sequence ASC",
                )
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let rows = stmt
                .query_map(rusqlite::params![stream_id, from_seq], |row| {
                    let data: String = row.get(0)?;
                    let seq: u64 = row.get(1)?;
                    Ok((data, seq))
                })
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let mut events = Vec::new();
            for row in rows {
                let (data, seq) = row.map_err(|e| EventStoreError::Connection(e.to_string()))?;
                let mut event: Event = serde_json::from_str(&data)
                    .map_err(|e| EventStoreError::Serialization(e.to_string()))?;
                event.sequence = seq;
                events.push(event);
            }
            Ok(events)
        })
        .await
        .map_err(|e| EventStoreError::Connection(e.to_string()))?
    }

    async fn read_all(
        &self,
        from_global_seq: u64,
        limit: usize,
    ) -> Result<Vec<Event>, EventStoreError> {
        let tailed = self.tail(from_global_seq, limit).await?;
        Ok(tailed.into_iter().map(|t| t.event).collect())
    }

    async fn tail(
        &self,
        after_global_seq: u64,
        limit: usize,
    ) -> Result<Vec<TailedEvent>, EventStoreError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, data, sequence FROM events WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
                )
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let rows = stmt
                .query_map(rusqlite::params![after_global_seq, limit], |row| {
                    let id: u64 = row.get(0)?;
                    let data: String = row.get(1)?;
                    let seq: u64 = row.get(2)?;
                    Ok((id, data, seq))
                })
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let mut events = Vec::new();
            for row in rows {
                let (global_seq, data, seq) =
                    row.map_err(|e| EventStoreError::Connection(e.to_string()))?;
                let mut event: Event = serde_json::from_str(&data)
                    .map_err(|e| EventStoreError::Serialization(e.to_string()))?;
                event.sequence = seq;
                events.push(TailedEvent { global_seq, event });
            }
            Ok(events)
        })
        .await
        .map_err(|e| EventStoreError::Connection(e.to_string()))?
    }

    async fn current_global_seq(&self) -> Result<u64, EventStoreError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let seq: u64 = conn
                .prepare("SELECT COALESCE(MAX(id), 0) FROM events")
                .and_then(|mut s| s.query_row([], |row| row.get(0)))
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;
            Ok(seq)
        })
        .await
        .map_err(|e| EventStoreError::Connection(e.to_string()))?
    }

    async fn last_sequence(&self, stream_id: &str) -> Result<u64, EventStoreError> {
        let conn = self.conn.clone();
        let stream_id = stream_id.to_owned();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let seq: u64 = conn
                .prepare("SELECT COALESCE(MAX(sequence), 0) FROM events WHERE stream_id = ?1")
                .and_then(|mut s| s.query_row([&stream_id], |row| row.get(0)))
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;
            Ok(seq)
        })
        .await
        .map_err(|e| EventStoreError::Connection(e.to_string()))?
    }

    async fn count(
        &self,
        stream_id: &str,
        kind_tag: Option<&str>,
    ) -> Result<u64, EventStoreError> {
        let conn = self.conn.clone();
        let stream_id = stream_id.to_owned();
        let kind_tag = kind_tag.map(|s| s.to_owned());

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let count: u64 = match kind_tag {
                Some(tag) => conn
                    .prepare(
                        "SELECT COUNT(*) FROM events WHERE stream_id = ?1 AND kind_tag = ?2",
                    )
                    .and_then(|mut s| s.query_row(rusqlite::params![stream_id, tag], |row| row.get(0)))
                    .map_err(|e| EventStoreError::Connection(e.to_string()))?,
                None => conn
                    .prepare("SELECT COUNT(*) FROM events WHERE stream_id = ?1")
                    .and_then(|mut s| s.query_row([&stream_id], |row| row.get(0)))
                    .map_err(|e| EventStoreError::Connection(e.to_string()))?,
            };
            Ok(count)
        })
        .await
        .map_err(|e| EventStoreError::Connection(e.to_string()))?
    }

    async fn verify_chain(&self, stream_id: &str) -> Result<u64, EventStoreError> {
        let conn = self.conn.clone();
        let stream_id = stream_id.to_owned();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT event_id, data, prev_hash, hash FROM events
                     WHERE stream_id = ?1 ORDER BY sequence ASC",
                )
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let rows = stmt
                .query_map([&stream_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let mut expected_prev_hash = String::new();
            let mut count: u64 = 0;

            for row in rows {
                let (event_id, data, prev_hash, stored_hash) =
                    row.map_err(|e| EventStoreError::Connection(e.to_string()))?;

                if prev_hash != expected_prev_hash {
                    return Err(EventStoreError::Connection(format!(
                        "Hash chain broken at event {event_id}: prev_hash mismatch \
                         (expected '{expected_prev_hash}', got '{prev_hash}')"
                    )));
                }

                let computed = compute_hash(&event_id, &data, &prev_hash);
                if computed != stored_hash {
                    return Err(EventStoreError::Connection(format!(
                        "Hash chain broken at event {event_id}: hash mismatch \
                         (computed '{computed}', stored '{stored_hash}')"
                    )));
                }

                expected_prev_hash = stored_hash;
                count += 1;
            }

            Ok(count)
        })
        .await
        .map_err(|e| EventStoreError::Connection(e.to_string()))?
    }

    async fn list_streams(&self) -> Result<Vec<(String, u64)>, EventStoreError> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT stream_id, COUNT(*) FROM events GROUP BY stream_id ORDER BY stream_id",
                )
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let mut streams = Vec::new();
            for row in rows {
                streams.push(row.map_err(|e| EventStoreError::Connection(e.to_string()))?);
            }
            Ok(streams)
        })
        .await
        .map_err(|e| EventStoreError::Connection(e.to_string()))?
    }

    async fn find_unmatched(
        &self,
        inbound_tag: &str,
        completion_tag: &str,
    ) -> Result<Vec<Event>, EventStoreError> {
        let conn = self.conn.clone();
        let inbound_tag = inbound_tag.to_owned();
        let completion_tag = completion_tag.to_owned();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT i.data FROM events i
                     WHERE i.kind_tag = ?1
                       AND i.correlation_id IS NOT NULL
                       AND NOT EXISTS (
                           SELECT 1 FROM events c
                           WHERE c.kind_tag = ?2
                             AND c.correlation_id = i.correlation_id
                       )
                     ORDER BY i.id ASC",
                )
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let rows = stmt
                .query_map(rusqlite::params![inbound_tag, completion_tag], |row| {
                    let data: String = row.get(0)?;
                    Ok(data)
                })
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let mut events = Vec::new();
            for row in rows {
                let data = row.map_err(|e| EventStoreError::Connection(e.to_string()))?;
                let event: Event = serde_json::from_str(&data)
                    .map_err(|e| EventStoreError::Serialization(e.to_string()))?;
                events.push(event);
            }
            Ok(events)
        })
        .await
        .map_err(|e| EventStoreError::Connection(e.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventKind;
    use crate::types::Message;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, SqliteEventStore) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_events.db");
        let store = SqliteEventStore::new(&db_path).unwrap();
        (dir, store)
    }

    fn make_event(stream: &str, kind: EventKind) -> Event {
        Event::new(stream.into(), kind, "test".into())
    }

    #[tokio::test]
    async fn append_assigns_sequence() {
        let (_dir, store) = setup().await;
        let mut e1 = make_event(
            "s1",
            EventKind::AgentProcessingStarted {
                conversation_id: "s1".into(),
            },
        );
        let mut e2 = make_event(
            "s1",
            EventKind::AgentProcessingCompleted {
                conversation_id: "s1".into(),
                success: true,
            },
        );

        let seq1 = store.append(&mut e1).await.unwrap();
        let seq2 = store.append(&mut e2).await.unwrap();

        assert_eq!(seq1, 1);
        assert_eq!(seq2, 2);
        assert_eq!(e1.sequence, 1);
        assert_eq!(e2.sequence, 2);
    }

    #[tokio::test]
    async fn sequences_are_per_stream() {
        let (_dir, store) = setup().await;
        let mut e1 = make_event(
            "s1",
            EventKind::AgentProcessingStarted {
                conversation_id: "s1".into(),
            },
        );
        let mut e2 = make_event(
            "s2",
            EventKind::AgentProcessingStarted {
                conversation_id: "s2".into(),
            },
        );

        let seq1 = store.append(&mut e1).await.unwrap();
        let seq2 = store.append(&mut e2).await.unwrap();

        assert_eq!(seq1, 1);
        assert_eq!(seq2, 1);
    }

    #[tokio::test]
    async fn read_stream_returns_ordered_events() {
        let (_dir, store) = setup().await;
        let mut e1 = make_event(
            "s1",
            EventKind::UserMessageReceived {
                content: Message::User("hello".into()),
                connector: "test".into(),
            },
        );
        let mut e2 = make_event(
            "s1",
            EventKind::ResponseReady {
                conversation_id: "s1".into(),
                content: "world".into(),
            },
        );
        let mut e3 = make_event(
            "s2",
            EventKind::AgentProcessingStarted {
                conversation_id: "s2".into(),
            },
        );

        store.append(&mut e1).await.unwrap();
        store.append(&mut e2).await.unwrap();
        store.append(&mut e3).await.unwrap();

        let events = store.read_stream("s1", 1).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind_tag(), "user_message_received");
        assert_eq!(events[1].kind_tag(), "response_ready");

        let events = store.read_stream("s1", 2).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind_tag(), "response_ready");

        let events = store.read_stream("s2", 1).await.unwrap();
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn read_all_returns_global_order() {
        let (_dir, store) = setup().await;
        let mut e1 = make_event(
            "s1",
            EventKind::AgentProcessingStarted {
                conversation_id: "s1".into(),
            },
        );
        let mut e2 = make_event(
            "s2",
            EventKind::AgentProcessingStarted {
                conversation_id: "s2".into(),
            },
        );
        let mut e3 = make_event(
            "s1",
            EventKind::AgentProcessingCompleted {
                conversation_id: "s1".into(),
                success: true,
            },
        );

        store.append(&mut e1).await.unwrap();
        store.append(&mut e2).await.unwrap();
        store.append(&mut e3).await.unwrap();

        let all = store.read_all(0, 100).await.unwrap();
        assert_eq!(all.len(), 3);

        let after_first = store.read_all(1, 100).await.unwrap();
        assert_eq!(after_first.len(), 2);

        let limited = store.read_all(0, 1).await.unwrap();
        assert_eq!(limited.len(), 1);
    }

    #[tokio::test]
    async fn tail_returns_in_global_order() {
        let (_dir, store) = setup().await;
        let mut e1 = make_event(
            "s1",
            EventKind::AgentProcessingStarted {
                conversation_id: "s1".into(),
            },
        );
        let mut e2 = make_event(
            "s2",
            EventKind::AgentProcessingStarted {
                conversation_id: "s2".into(),
            },
        );
        let mut e3 = make_event(
            "s1",
            EventKind::AgentProcessingCompleted {
                conversation_id: "s1".into(),
                success: true,
            },
        );

        store.append(&mut e1).await.unwrap();
        store.append(&mut e2).await.unwrap();
        store.append(&mut e3).await.unwrap();

        let tailed = store.tail(0, 100).await.unwrap();
        assert_eq!(tailed.len(), 3);
        assert_eq!(tailed[0].global_seq, 1);
        assert_eq!(tailed[1].global_seq, 2);
        assert_eq!(tailed[2].global_seq, 3);
        assert_eq!(tailed[0].event.kind_tag(), "agent_processing_started");
        assert_eq!(tailed[2].event.kind_tag(), "agent_processing_completed");
    }

    #[tokio::test]
    async fn tail_respects_after_cursor() {
        let (_dir, store) = setup().await;
        for i in 0..5 {
            let mut e = make_event(
                "s1",
                EventKind::AgentProcessingStarted {
                    conversation_id: format!("c{i}"),
                },
            );
            store.append(&mut e).await.unwrap();
        }

        let after_two = store.tail(2, 100).await.unwrap();
        assert_eq!(after_two.len(), 3);
        assert_eq!(after_two[0].global_seq, 3);

        let limited = store.tail(0, 2).await.unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].global_seq, 1);
        assert_eq!(limited[1].global_seq, 2);
    }

    #[tokio::test]
    async fn current_global_seq_matches_last_tailed() {
        let (_dir, store) = setup().await;
        assert_eq!(store.current_global_seq().await.unwrap(), 0);

        let mut e1 = make_event(
            "s1",
            EventKind::AgentProcessingStarted {
                conversation_id: "s1".into(),
            },
        );
        let mut e2 = make_event(
            "s2",
            EventKind::AgentProcessingStarted {
                conversation_id: "s2".into(),
            },
        );
        store.append(&mut e1).await.unwrap();
        store.append(&mut e2).await.unwrap();

        assert_eq!(store.current_global_seq().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn last_sequence_empty_stream() {
        let (_dir, store) = setup().await;
        let seq = store.last_sequence("nonexistent").await.unwrap();
        assert_eq!(seq, 0);
    }

    #[tokio::test]
    async fn count_with_kind_filter() {
        let (_dir, store) = setup().await;
        let mut e1 = make_event(
            "s1",
            EventKind::ToolCallRequested {
                tool_name: "shell".into(),
                arguments: "ls".into(),
                call_id: "tc-1".into(),
            },
        );
        let mut e2 = make_event(
            "s1",
            EventKind::ToolCallCompleted {
                call_id: "tc-1".into(),
                result: "ok".into(),
                result_preview: "ok".into(),
                is_error: false,
                duration_ms: 10,
            },
        );
        let mut e3 = make_event(
            "s1",
            EventKind::ToolCallRequested {
                tool_name: "web_search".into(),
                arguments: "rust".into(),
                call_id: "tc-2".into(),
            },
        );

        store.append(&mut e1).await.unwrap();
        store.append(&mut e2).await.unwrap();
        store.append(&mut e3).await.unwrap();

        let total = store.count("s1", None).await.unwrap();
        assert_eq!(total, 3);

        let requested = store.count("s1", Some("tool_call_requested")).await.unwrap();
        assert_eq!(requested, 2);

        let completed = store.count("s1", Some("tool_call_completed")).await.unwrap();
        assert_eq!(completed, 1);
    }

    #[tokio::test]
    async fn correlation_id_preserved() {
        let (_dir, store) = setup().await;
        let corr = uuid::Uuid::new_v4();
        let mut event = Event::new(
            "s1".into(),
            EventKind::UserMessageReceived {
                content: Message::User("test".into()),
                connector: "telegram".into(),
            },
            "telegram".into(),
        )
        .with_correlation(corr);

        store.append(&mut event).await.unwrap();

        let events = store.read_stream("s1", 1).await.unwrap();
        assert_eq!(events[0].correlation_id, Some(corr));
    }

    #[tokio::test]
    async fn hash_chain_valid_after_appends() {
        let (_dir, store) = setup().await;
        let mut e1 = make_event(
            "s1",
            EventKind::AgentProcessingStarted {
                conversation_id: "s1".into(),
            },
        );
        let mut e2 = make_event(
            "s1",
            EventKind::AgentProcessingCompleted {
                conversation_id: "s1".into(),
                success: true,
            },
        );
        let mut e3 = make_event(
            "s1",
            EventKind::ResponseReady {
                conversation_id: "s1".into(),
                content: "done".into(),
            },
        );

        store.append(&mut e1).await.unwrap();
        store.append(&mut e2).await.unwrap();
        store.append(&mut e3).await.unwrap();

        let count = store.verify_chain("s1").await.unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn hash_chain_empty_stream_is_valid() {
        let (_dir, store) = setup().await;
        let count = store.verify_chain("nonexistent").await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn hash_chain_per_stream_independent() {
        let (_dir, store) = setup().await;
        let mut e1 = make_event(
            "s1",
            EventKind::AgentProcessingStarted {
                conversation_id: "s1".into(),
            },
        );
        let mut e2 = make_event(
            "s2",
            EventKind::AgentProcessingStarted {
                conversation_id: "s2".into(),
            },
        );

        store.append(&mut e1).await.unwrap();
        store.append(&mut e2).await.unwrap();

        assert_eq!(store.verify_chain("s1").await.unwrap(), 1);
        assert_eq!(store.verify_chain("s2").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn list_streams_returns_all_with_counts() {
        let (_dir, store) = setup().await;
        let mut e1 = make_event(
            "alpha",
            EventKind::AgentProcessingStarted {
                conversation_id: "alpha".into(),
            },
        );
        let mut e2 = make_event(
            "alpha",
            EventKind::AgentProcessingCompleted {
                conversation_id: "alpha".into(),
                success: true,
            },
        );
        let mut e3 = make_event(
            "beta",
            EventKind::AgentProcessingStarted {
                conversation_id: "beta".into(),
            },
        );

        store.append(&mut e1).await.unwrap();
        store.append(&mut e2).await.unwrap();
        store.append(&mut e3).await.unwrap();

        let streams = store.list_streams().await.unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0], ("alpha".to_string(), 2));
        assert_eq!(streams[1], ("beta".to_string(), 1));
    }

    #[tokio::test]
    async fn list_streams_empty_store() {
        let (_dir, store) = setup().await;
        let streams = store.list_streams().await.unwrap();
        assert!(streams.is_empty());
    }

    /// Concurrent appends to different streams must never leak events
    /// across stream boundaries (session isolation, ADR-0016 drift goal).
    #[tokio::test]
    async fn concurrent_streams_are_isolated() {
        let (_dir, store) = setup().await;
        let store = Arc::new(store);
        let n = 20; // events per stream

        let mut handles = Vec::new();
        for stream_idx in 0..3 {
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let stream = format!("concurrent-{stream_idx}");
                for i in 0..n {
                    let mut event = make_event(
                        &stream,
                        EventKind::ResponseReady {
                            conversation_id: stream.clone(),
                            content: format!("msg-{i}"),
                        },
                    );
                    store.append(&mut event).await.unwrap();
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // Each stream must contain exactly `n` events, all with the
        // matching stream_id — no cross-contamination.
        for stream_idx in 0..3 {
            let stream = format!("concurrent-{stream_idx}");
            let events = store.read_stream(&stream, 1).await.unwrap();
            assert_eq!(
                events.len(),
                n,
                "stream '{stream}' should have {n} events, got {}",
                events.len()
            );
            for event in &events {
                assert_eq!(
                    event.stream_id, stream,
                    "event on stream '{stream}' has wrong stream_id: {}",
                    event.stream_id
                );
            }
        }
    }
}
