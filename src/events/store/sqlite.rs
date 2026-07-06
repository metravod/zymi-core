use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension};
use tokio::sync::Mutex;

use crate::events::{Event, EventStoreError};

use super::event_store::{
    compute_hash_v2, recompute_hash, ChainVerification, EventStore, TailedEvent,
};

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
                 ON events(kind_tag);
             CREATE TABLE IF NOT EXISTS stream_heads (
                 stream_id TEXT PRIMARY KEY,
                 last_sequence INTEGER NOT NULL,
                 last_hash TEXT NOT NULL
             );",
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

                let hash = compute_hash_v2(&event_id, next_seq, &data, &prev_hash);

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

                // Advance the stream head in the same transaction. verify uses
                // this to detect tail-truncation and whole-stream deletion
                // (ADR-0040): the events rows and the head then disagree.
                conn.execute(
                    "INSERT INTO stream_heads (stream_id, last_sequence, last_hash)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(stream_id) DO UPDATE SET
                         last_sequence = excluded.last_sequence,
                         last_hash = excluded.last_hash",
                    rusqlite::params![stream_id, next_seq, hash],
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

    async fn verify_chain(&self, stream_id: &str) -> Result<ChainVerification, EventStoreError> {
        let conn = self.conn.clone();
        let stream_id = stream_id.to_owned();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT event_id, sequence, data, prev_hash, hash FROM events
                     WHERE stream_id = ?1 ORDER BY sequence ASC",
                )
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let rows = stmt
                .query_map([&stream_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            let mut expected_prev_hash = String::new();
            let mut result = ChainVerification::default();
            let mut last_seq: u64 = 0;
            let mut last_hash = String::new();

            for row in rows {
                let (event_id, sequence, data, prev_hash, stored_hash) =
                    row.map_err(|e| EventStoreError::Connection(e.to_string()))?;

                // Legacy rows predate the hash chain: they carry the migration
                // DEFAULT '' in `hash`. They can't be verified, so exempt them
                // (ADR-0035) rather than reporting the whole stream as broken.
                // Such a row also leaves the expectation unchanged (empty), so
                // the first genuinely-hashed event chains from a clean root.
                let Some(computed) =
                    recompute_hash(&event_id, sequence, &data, &prev_hash, &stored_hash)
                else {
                    result.legacy += 1;
                    continue;
                };

                if prev_hash != expected_prev_hash {
                    return Err(EventStoreError::Connection(format!(
                        "Hash chain broken at event {event_id}: prev_hash mismatch \
                         (expected '{expected_prev_hash}', got '{prev_hash}')"
                    )));
                }

                if computed != stored_hash {
                    return Err(EventStoreError::Connection(format!(
                        "Hash chain broken at event {event_id}: hash mismatch \
                         (computed '{computed}', stored '{stored_hash}')"
                    )));
                }

                expected_prev_hash = stored_hash.clone();
                last_seq = sequence;
                last_hash = stored_hash;
                result.verified += 1;
            }

            // Truncation / deletion check (ADR-0040): compare the walked tail
            // against the recorded head. A head ahead of the rows means the
            // tail — or the whole stream — was removed.
            let head: Option<(u64, String)> = conn
                .query_row(
                    "SELECT last_sequence, last_hash FROM stream_heads WHERE stream_id = ?1",
                    [&stream_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;

            if let Some((head_seq, head_hash)) = head {
                if result.verified == 0 {
                    return Err(EventStoreError::Connection(format!(
                        "Stream truncated: head records sequence {head_seq} but no hashed \
                         events remain (whole-stream deletion)"
                    )));
                }
                if last_seq != head_seq || last_hash != head_hash {
                    return Err(EventStoreError::Connection(format!(
                        "Stream truncated: tail is sequence {last_seq} but head records \
                         sequence {head_seq} (missing {} event(s))",
                        head_seq.saturating_sub(last_seq)
                    )));
                }
            }

            Ok(result)
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

    async fn head_stream_ids(&self) -> Result<Vec<String>, EventStoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare("SELECT stream_id FROM stream_heads ORDER BY stream_id")
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| EventStoreError::Connection(e.to_string()))?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row.map_err(|e| EventStoreError::Connection(e.to_string()))?);
            }
            Ok(ids)
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

    async fn checkpoint(&self) -> Result<(), EventStoreError> {
        // ADR-0030: move committed WAL frames into the main db file so a
        // separate reader process sees the run's terminal events without
        // waiting for an auto-checkpoint. PASSIVE never blocks on readers and
        // runs against the writer's own connection, so it is cheap per run.
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
                .map_err(|e| EventStoreError::Connection(e.to_string()))
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

    // ADR-0030: after the writer checkpoints, a *separate* reader connection
    // against the same file must see the run's terminal event. This mirrors a
    // fresh `zymi events` process reading a db just written by `zymi run`.
    #[tokio::test]
    async fn checkpoint_makes_tail_visible_to_separate_connection() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("events.db");

        let writer = SqliteEventStore::new(&db_path).unwrap();
        let mut started = make_event(
            "run-1",
            EventKind::AgentProcessingStarted {
                conversation_id: "run-1".into(),
            },
        );
        let mut completed = make_event(
            "run-1",
            EventKind::PipelineCompleted {
                pipeline: "main".into(),
                success: true,
                final_output: Some("done".into()),
                error: None,
            },
        );
        writer.append(&mut started).await.unwrap();
        writer.append(&mut completed).await.unwrap();
        writer.checkpoint().await.unwrap();

        // Fresh connection — the reader process in the bug report.
        let reader = SqliteEventStore::new(&db_path).unwrap();
        let events = reader.read_stream("run-1", 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events.last().unwrap().kind,
            EventKind::PipelineCompleted { .. }
        ));
    }

    #[tokio::test]
    async fn checkpoint_is_idempotent_on_empty_store() {
        let (_dir, store) = setup().await;
        store.checkpoint().await.unwrap();
        store.checkpoint().await.unwrap();
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
                replayed: false,
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

        let result = store.verify_chain("s1").await.unwrap();
        assert_eq!(result.verified, 3);
        assert_eq!(result.legacy, 0);
    }

    #[tokio::test]
    async fn hash_chain_empty_stream_is_valid() {
        let (_dir, store) = setup().await;
        let result = store.verify_chain("nonexistent").await.unwrap();
        assert_eq!(result.total(), 0);
    }

    #[tokio::test]
    async fn hash_chain_exempts_legacy_rows() {
        // Rows written before the hash feature carry the migration DEFAULT ''
        // in prev_hash/hash. verify_chain must exempt them, not fail the stream,
        // and a genuinely-hashed event appended afterwards must still verify.
        let (_dir, store) = setup().await;

        // Simulate a legacy row by inserting one directly with empty hashes,
        // bypassing append()'s hashing.
        {
            let conn = store.conn.lock().await;
            conn.execute(
                "INSERT INTO events (event_id, stream_id, sequence, timestamp, kind_tag, data, source, prev_hash, hash)
                 VALUES ('legacy-1', 'mixed', 1, '2020-01-01T00:00:00Z', 'ResponseReady', '{}', 'legacy', '', '')",
                [],
            )
            .unwrap();
        }

        // Fully-legacy stream: exempted, not broken.
        let legacy_only = store.verify_chain("mixed").await.unwrap();
        assert_eq!(legacy_only.legacy, 1);
        assert_eq!(legacy_only.verified, 0);

        // Append a real (hashed) event after the legacy prefix.
        let mut e = make_event(
            "mixed",
            EventKind::ResponseReady {
                conversation_id: "mixed".into(),
                content: "post-upgrade".into(),
            },
        );
        store.append(&mut e).await.unwrap();

        let mixed = store.verify_chain("mixed").await.unwrap();
        assert_eq!(mixed.legacy, 1);
        assert_eq!(mixed.verified, 1);
    }

    #[tokio::test]
    async fn hash_chain_detects_tail_truncation() {
        let (_dir, store) = setup().await;
        for i in 0..3 {
            let mut e = make_event(
                "s1",
                EventKind::ResponseReady {
                    conversation_id: "s1".into(),
                    content: format!("m{i}"),
                },
            );
            store.append(&mut e).await.unwrap();
        }
        // Intact: verifies.
        assert_eq!(store.verify_chain("s1").await.unwrap().verified, 3);

        // Delete the last event but leave the head recording sequence 3.
        {
            let conn = store.conn.lock().await;
            conn.execute("DELETE FROM events WHERE stream_id='s1' AND sequence=3", [])
                .unwrap();
        }
        let err = store.verify_chain("s1").await.unwrap_err().to_string();
        assert!(err.contains("truncated"), "expected truncation error: {err}");
    }

    #[tokio::test]
    async fn hash_chain_detects_whole_stream_deletion() {
        let (_dir, store) = setup().await;
        let mut e = make_event(
            "gone",
            EventKind::ResponseReady {
                conversation_id: "gone".into(),
                content: "x".into(),
            },
        );
        store.append(&mut e).await.unwrap();

        {
            let conn = store.conn.lock().await;
            conn.execute("DELETE FROM events WHERE stream_id='gone'", []).unwrap();
        }
        // list_streams no longer sees it, but the head remains.
        assert!(store.head_stream_ids().await.unwrap().contains(&"gone".to_string()));
        let err = store.verify_chain("gone").await.unwrap_err().to_string();
        assert!(
            err.contains("whole-stream deletion"),
            "expected deletion error: {err}"
        );
    }

    #[tokio::test]
    async fn hash_chain_verifies_v1_rows() {
        use super::super::event_store::compute_hash_v1;
        // A row written by 0.7.x: bare-hex v1 hash, no version prefix, with a
        // matching head. It must still verify under the v2-aware verifier.
        let (_dir, store) = setup().await;
        let v1 = compute_hash_v1("v1-evt", "{}", "");
        {
            let conn = store.conn.lock().await;
            conn.execute(
                "INSERT INTO events (event_id, stream_id, sequence, timestamp, kind_tag, data, source, prev_hash, hash)
                 VALUES ('v1-evt', 'old', 1, '2026-01-01T00:00:00Z', 'ResponseReady', '{}', 'engine', '', ?1)",
                [&v1],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO stream_heads (stream_id, last_sequence, last_hash) VALUES ('old', 1, ?1)",
                [&v1],
            )
            .unwrap();
        }
        let r = store.verify_chain("old").await.unwrap();
        assert_eq!(r.verified, 1);
        assert_eq!(r.legacy, 0);
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

        assert_eq!(store.verify_chain("s1").await.unwrap().verified, 1);
        assert_eq!(store.verify_chain("s2").await.unwrap().verified, 1);
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
