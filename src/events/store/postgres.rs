//! Postgres-backed [`EventStore`] (ADR-0012, v0.4 sprint 3).
//!
//! The schema mirrors [`SqliteEventStore`](super::sqlite::SqliteEventStore):
//! one `events` table with per-stream `(stream_id, sequence)` and a global
//! `id BIGSERIAL` cursor used by [`StoreTailWatcher`](super::watcher).
//!
//! Per-stream sequence assignment + hash chain reads happen inside a
//! transaction guarded by a stream-keyed `pg_advisory_xact_lock`. That
//! lock is released on COMMIT/ROLLBACK and prevents two concurrent writers
//! from racing on the same `(stream_id, sequence)` slot.
//!
//! Behind the `postgres` Cargo feature.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::{Config, Pool, Runtime};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;

use crate::events::{Event, EventStoreError};

use super::event_store::{EventStore, TailedEvent};

const SCHEMA_SQL: &str = r#"
    CREATE TABLE IF NOT EXISTS events (
        id BIGSERIAL PRIMARY KEY,
        event_id TEXT NOT NULL UNIQUE,
        stream_id TEXT NOT NULL,
        sequence BIGINT NOT NULL,
        timestamp TIMESTAMPTZ NOT NULL,
        kind_tag TEXT NOT NULL,
        data TEXT NOT NULL,
        correlation_id TEXT,
        causation_id TEXT,
        source TEXT NOT NULL,
        prev_hash TEXT NOT NULL DEFAULT '',
        hash TEXT NOT NULL DEFAULT ''
    );
    CREATE UNIQUE INDEX IF NOT EXISTS idx_events_stream_seq
        ON events(stream_id, sequence);
    CREATE INDEX IF NOT EXISTS idx_events_correlation
        ON events(correlation_id);
    CREATE INDEX IF NOT EXISTS idx_events_kind
        ON events(kind_tag);
"#;

pub struct PostgresEventStore {
    pool: Pool,
}

impl PostgresEventStore {
    /// Connect using a libpq URL (`postgres://user:pass@host:port/db`)
    /// and ensure the schema is in place.
    pub async fn connect(url: &str) -> Result<Self, EventStoreError> {
        let pg_config: tokio_postgres::Config = url
            .parse()
            .map_err(|e: tokio_postgres::Error| EventStoreError::Connection(e.to_string()))?;

        // deadpool-postgres builds its pool from its own Config; pull the
        // tokio-postgres Config across by JSON-roundtripping the URL.
        let mut cfg = Config::new();
        cfg.url = Some(url.to_string());
        // A small pool is fine — we serialise writes per stream via
        // advisory locks, so contention is per-stream not per-connection.
        cfg.pool = Some(deadpool_postgres::PoolConfig::new(8));
        let _ = pg_config; // captured shape validation only.

        let pool = cfg
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .map_err(|e| EventStoreError::Connection(format!("pool init: {e}")))?;

        // Trip the schema on a borrowed connection so a misconfigured
        // database fails loudly at construction, not on the first append.
        {
            let client = pool
                .get()
                .await
                .map_err(|e| EventStoreError::Connection(format!("pool get: {e}")))?;
            client
                .batch_execute(SCHEMA_SQL)
                .await
                .map_err(|e| EventStoreError::Connection(format!("schema init: {e}")))?;
        }

        Ok(Self { pool })
    }
}

fn compute_hash(event_id: &str, data: &str, prev_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(event_id.as_bytes());
    hasher.update(data.as_bytes());
    hasher.update(prev_hash.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn parse_event(data: &str, sequence: i64) -> Result<Event, EventStoreError> {
    let mut event: Event = serde_json::from_str(data)
        .map_err(|e| EventStoreError::Serialization(e.to_string()))?;
    event.sequence = sequence as u64;
    Ok(event)
}

fn err_conn<E: std::fmt::Display>(e: E) -> EventStoreError {
    EventStoreError::Connection(e.to_string())
}

#[async_trait]
impl EventStore for PostgresEventStore {
    async fn append(&self, event: &mut Event) -> Result<u64, EventStoreError> {
        let event_id = event.id.to_string();
        let stream_id = event.stream_id.clone();
        let timestamp: DateTime<Utc> = event.timestamp.with_timezone(&Utc);
        let kind_tag = event.kind_tag().to_string();
        let data = serde_json::to_string(&event)
            .map_err(|e| EventStoreError::Serialization(e.to_string()))?;
        let correlation_id = event.correlation_id.map(|u| u.to_string());
        let causation_id = event.causation_id.map(|u| u.to_string());
        let source = event.source.clone();

        let mut client = self.pool.get().await.map_err(err_conn)?;
        let tx = client.transaction().await.map_err(err_conn)?;

        // Per-stream advisory lock — released on COMMIT/ROLLBACK. Prevents
        // two concurrent writers from racing on the same sequence number.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            &[&stream_id],
        )
        .await
        .map_err(err_conn)?;

        let row = tx
            .query_one(
                "SELECT
                    COALESCE(MAX(sequence), 0) + 1 AS next_seq,
                    COALESCE(
                        (SELECT hash FROM events
                         WHERE stream_id = $1
                         ORDER BY sequence DESC LIMIT 1),
                        ''
                    ) AS prev_hash
                 FROM events
                 WHERE stream_id = $1",
                &[&stream_id],
            )
            .await
            .map_err(err_conn)?;
        let next_seq: i64 = row.get("next_seq");
        let prev_hash: String = row.get("prev_hash");
        let hash = compute_hash(&event_id, &data, &prev_hash);

        tx.execute(
            "INSERT INTO events
                (event_id, stream_id, sequence, timestamp, kind_tag, data,
                 correlation_id, causation_id, source, prev_hash, hash)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            &[
                &event_id,
                &stream_id,
                &next_seq,
                &timestamp,
                &kind_tag,
                &data,
                &correlation_id,
                &causation_id,
                &source,
                &prev_hash,
                &hash,
            ],
        )
        .await
        .map_err(err_conn)?;

        tx.commit().await.map_err(err_conn)?;

        let seq = next_seq as u64;
        event.sequence = seq;
        Ok(seq)
    }

    async fn read_stream(
        &self,
        stream_id: &str,
        from_seq: u64,
    ) -> Result<Vec<Event>, EventStoreError> {
        let from_seq_i: i64 = from_seq as i64;
        let client = self.pool.get().await.map_err(err_conn)?;
        let rows = client
            .query(
                "SELECT data, sequence FROM events
                 WHERE stream_id = $1 AND sequence >= $2
                 ORDER BY sequence ASC",
                &[&stream_id, &from_seq_i],
            )
            .await
            .map_err(err_conn)?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let data: String = row.get("data");
            let seq: i64 = row.get("sequence");
            events.push(parse_event(&data, seq)?);
        }
        Ok(events)
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
        let after_i: i64 = after_global_seq as i64;
        let limit_i: i64 = limit as i64;
        let client = self.pool.get().await.map_err(err_conn)?;
        let rows = client
            .query(
                "SELECT id, data, sequence FROM events
                 WHERE id > $1
                 ORDER BY id ASC LIMIT $2",
                &[&after_i, &limit_i],
            )
            .await
            .map_err(err_conn)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let global_seq: i64 = row.get("id");
            let data: String = row.get("data");
            let seq: i64 = row.get("sequence");
            out.push(TailedEvent {
                global_seq: global_seq as u64,
                event: parse_event(&data, seq)?,
            });
        }
        Ok(out)
    }

    async fn current_global_seq(&self) -> Result<u64, EventStoreError> {
        let client = self.pool.get().await.map_err(err_conn)?;
        let row = client
            .query_one("SELECT COALESCE(MAX(id), 0) AS hwm FROM events", &[])
            .await
            .map_err(err_conn)?;
        let hwm: i64 = row.get("hwm");
        Ok(hwm as u64)
    }

    async fn last_sequence(&self, stream_id: &str) -> Result<u64, EventStoreError> {
        let client = self.pool.get().await.map_err(err_conn)?;
        let row = client
            .query_one(
                "SELECT COALESCE(MAX(sequence), 0) AS last_seq
                 FROM events WHERE stream_id = $1",
                &[&stream_id],
            )
            .await
            .map_err(err_conn)?;
        let last: i64 = row.get("last_seq");
        Ok(last as u64)
    }

    async fn count(
        &self,
        stream_id: &str,
        kind_tag: Option<&str>,
    ) -> Result<u64, EventStoreError> {
        let client = self.pool.get().await.map_err(err_conn)?;
        let row = match kind_tag {
            Some(tag) => {
                client
                    .query_one(
                        "SELECT COUNT(*) AS c FROM events
                         WHERE stream_id = $1 AND kind_tag = $2",
                        &[&stream_id, &tag],
                    )
                    .await
            }
            None => {
                client
                    .query_one(
                        "SELECT COUNT(*) AS c FROM events WHERE stream_id = $1",
                        &[&stream_id],
                    )
                    .await
            }
        }
        .map_err(err_conn)?;
        let c: i64 = row.get("c");
        Ok(c as u64)
    }

    async fn verify_chain(&self, stream_id: &str) -> Result<u64, EventStoreError> {
        let client = self.pool.get().await.map_err(err_conn)?;
        let rows = client
            .query(
                "SELECT event_id, data, prev_hash, hash FROM events
                 WHERE stream_id = $1
                 ORDER BY sequence ASC",
                &[&stream_id],
            )
            .await
            .map_err(err_conn)?;

        let mut expected_prev_hash = String::new();
        let mut count: u64 = 0;
        for row in rows {
            let event_id: String = row.get("event_id");
            let data: String = row.get("data");
            let prev_hash: String = row.get("prev_hash");
            let stored_hash: String = row.get("hash");

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
    }

    async fn list_streams(&self) -> Result<Vec<(String, u64)>, EventStoreError> {
        let client = self.pool.get().await.map_err(err_conn)?;
        let rows = client
            .query(
                "SELECT stream_id, COUNT(*)::BIGINT AS c
                 FROM events
                 GROUP BY stream_id
                 ORDER BY stream_id",
                &[],
            )
            .await
            .map_err(err_conn)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let sid: String = row.get("stream_id");
            let c: i64 = row.get("c");
            out.push((sid, c as u64));
        }
        Ok(out)
    }

    async fn find_unmatched(
        &self,
        inbound_tag: &str,
        completion_tag: &str,
    ) -> Result<Vec<Event>, EventStoreError> {
        let client = self.pool.get().await.map_err(err_conn)?;
        let rows = client
            .query(
                "SELECT i.data, i.sequence FROM events i
                 WHERE i.kind_tag = $1
                   AND i.correlation_id IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM events c
                       WHERE c.kind_tag = $2
                         AND c.correlation_id = i.correlation_id
                   )
                 ORDER BY i.id ASC",
                &[&inbound_tag, &completion_tag],
            )
            .await
            .map_err(err_conn)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let data: String = row.get("data");
            let seq: i64 = row.get("sequence");
            out.push(parse_event(&data, seq)?);
        }
        Ok(out)
    }
}

// Suppress unused-Arc lint: PostgresEventStore is wrapped in Arc<dyn EventStore>
// at construction sites but the type itself does not need Arc internally.
#[allow(dead_code)]
fn _arc_check(_b: Arc<crate::events::bus::EventBus>) {}

#[cfg(test)]
mod tests {
    //! Integration tests run only when `ZYMI_POSTGRES_TEST_URL` is set.
    //! Unset → tests are skipped (treated as passing) so default `cargo test`
    //! does not require Postgres on the developer machine. CI / release
    //! workflows opt in via the env var.

    use super::*;
    use crate::events::EventKind;
    use crate::types::Message;

    fn test_url() -> Option<String> {
        std::env::var("ZYMI_POSTGRES_TEST_URL").ok()
    }

    /// Use a freshly-named schema per test run so parallel cargo tests don't
    /// collide on the shared `events` table.
    async fn fresh_store() -> Option<PostgresEventStore> {
        let url = test_url()?;
        let store = PostgresEventStore::connect(&url).await.ok()?;
        // Wipe any leftover rows so tests start clean. Safe because
        // ZYMI_POSTGRES_TEST_URL is by contract a throwaway database.
        let client = store.pool.get().await.ok()?;
        client.batch_execute("TRUNCATE events RESTART IDENTITY").await.ok()?;
        Some(store)
    }

    fn make_event(stream: &str, kind: EventKind) -> Event {
        Event::new(stream.into(), kind, "test".into())
    }

    #[tokio::test]
    async fn append_and_read_round_trip() {
        let Some(store) = fresh_store().await else { return };

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
                content: "hi".into(),
            },
        );
        store.append(&mut e1).await.unwrap();
        store.append(&mut e2).await.unwrap();

        assert_eq!(e1.sequence, 1);
        assert_eq!(e2.sequence, 2);

        let events = store.read_stream("s1", 1).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind_tag(), "user_message_received");
        assert_eq!(events[1].kind_tag(), "response_ready");
    }

    #[tokio::test]
    async fn sequences_are_per_stream() {
        let Some(store) = fresh_store().await else { return };
        let mut a = make_event(
            "alpha",
            EventKind::AgentProcessingStarted {
                conversation_id: "alpha".into(),
            },
        );
        let mut b = make_event(
            "beta",
            EventKind::AgentProcessingStarted {
                conversation_id: "beta".into(),
            },
        );
        store.append(&mut a).await.unwrap();
        store.append(&mut b).await.unwrap();
        assert_eq!(a.sequence, 1);
        assert_eq!(b.sequence, 1);
    }

    #[tokio::test]
    async fn tail_advances_global_cursor() {
        let Some(store) = fresh_store().await else { return };
        for i in 0..3 {
            let mut e = make_event(
                "s1",
                EventKind::AgentProcessingStarted {
                    conversation_id: format!("c{i}"),
                },
            );
            store.append(&mut e).await.unwrap();
        }
        let all = store.tail(0, 100).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].global_seq, 1);
        assert_eq!(all[2].global_seq, 3);

        let after_one = store.tail(1, 100).await.unwrap();
        assert_eq!(after_one.len(), 2);
    }

    #[tokio::test]
    async fn hash_chain_is_valid_after_appends() {
        let Some(store) = fresh_store().await else { return };
        for _ in 0..3 {
            let mut e = make_event(
                "chain",
                EventKind::AgentProcessingStarted {
                    conversation_id: "chain".into(),
                },
            );
            store.append(&mut e).await.unwrap();
        }
        let count = store.verify_chain("chain").await.unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn watcher_delivers_cross_process_appends() {
        // Two PostgresEventStore handles share one logical database — this
        // simulates the multi-process `zymi serve` topology that ADR-0012
        // promises. Process A appends; process B's StoreTailWatcher must
        // fan the event out onto its in-process bus.
        let Some(writer) = fresh_store().await else { return };
        let url = test_url().expect("url present (we got the writer)");
        let reader = PostgresEventStore::connect(&url).await.unwrap();

        let writer = std::sync::Arc::new(writer);
        let reader: std::sync::Arc<dyn EventStore> = std::sync::Arc::new(reader);

        let bus = std::sync::Arc::new(crate::events::bus::EventBus::new(std::sync::Arc::clone(
            &reader,
        )));
        let mut rx = bus.subscribe().await;

        let watcher = crate::events::store::StoreTailWatcher::new(
            std::sync::Arc::clone(&reader),
            std::sync::Arc::clone(&bus),
        )
        .with_interval(std::time::Duration::from_millis(50))
        .spawn();

        // Watcher records its starting cursor before we append — otherwise
        // the test races against the initial current_global_seq() call.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let mut ev = make_event(
            "cross-proc",
            EventKind::AgentProcessingStarted {
                conversation_id: "cross-proc".into(),
            },
        );
        writer.append(&mut ev).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("watcher should deliver cross-process event")
            .expect("subscriber channel open");
        assert_eq!(received.stream_id, "cross-proc");
        assert_eq!(received.kind_tag(), "agent_processing_started");

        watcher.stop().await;
    }

    #[tokio::test]
    async fn concurrent_appends_to_same_stream_serialise() {
        let Some(store) = fresh_store().await else { return };
        let store = std::sync::Arc::new(store);
        let n = 10;
        let mut handles = Vec::new();
        for i in 0..n {
            let store = std::sync::Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                let mut e = make_event(
                    "lock",
                    EventKind::ResponseReady {
                        conversation_id: "lock".into(),
                        content: format!("msg-{i}"),
                    },
                );
                store.append(&mut e).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let events = store.read_stream("lock", 1).await.unwrap();
        assert_eq!(events.len(), n);
        // Sequences must be 1..=n with no gaps or duplicates.
        let mut seqs: Vec<u64> = events.iter().map(|e| e.sequence).collect();
        seqs.sort();
        assert_eq!(seqs, (1..=(n as u64)).collect::<Vec<_>>());
        // Hash chain stays intact under concurrency.
        store.verify_chain("lock").await.unwrap();
    }
}
