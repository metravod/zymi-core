//! Per-connector cursor persistence (ADR-0021 §http_poll).
//!
//! `http_poll` needs a small, restart-safe value keyed by connector name
//! (Telegram `update_id`, Gmail `pageToken`, …). A cursor is just a string
//! the connector hands back on every tick; we don't interpret it.
//!
//! We deliberately do **not** extend the [`crate::events::store::EventStore`]
//! trait for this — cursors aren't part of the event stream, and forcing
//! every backend (SQLite, future Postgres) to own a cursor table inside
//! the event-log contract bleeds connector state into the audit surface.
//!
//! Instead, [`CursorStore`] is its own pluggable trait with two backends
//! that **pair** with the event-store backend:
//!
//! * [`SqliteCursorStore`] — sibling SQLite file at
//!   `<project_root>/.zymi/connectors.db`. Default path.
//! * [`PostgresCursorStore`] — `connector_cursors` table in the same
//!   Postgres instance the event store points at. Behind the `postgres`
//!   feature. Multi-process `zymi serve` against shared Postgres needs
//!   this so two processes don't both fetch the same Telegram update id.
//!
//! Backend selection is automatic: whichever [`StoreBackend`] the event
//! store opened with, the cursor store opens with the matching one. No
//! extra YAML knob — one backend dimension, not two.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{params, Connection};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::events::store::StoreBackend;

/// Errors bubbling out of cursor I/O.
#[derive(Debug, Error)]
pub enum CursorError {
    #[error("cursor store connection: {0}")]
    Connection(String),
    #[error("cursor I/O: {0}")]
    Io(String),
}

/// Lightweight key-value store for connector cursors. Shared across all
/// `http_poll` instances in a single runtime.
#[async_trait]
pub trait CursorStore: Send + Sync {
    async fn get(&self, name: &str) -> Result<Option<String>, CursorError>;
    async fn set(&self, name: &str, cursor: &str) -> Result<(), CursorError>;
}

// --- SQLite backend (default) ------------------------------------------

pub struct SqliteCursorStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteCursorStore {
    /// Open (or create) the cursor DB at `<project_root>/.zymi/connectors.db`.
    pub fn open(project_root: &Path) -> Result<Self, CursorError> {
        let dir = project_root.join(".zymi");
        std::fs::create_dir_all(&dir).map_err(|e| CursorError::Io(e.to_string()))?;
        Self::open_at(&dir.join("connectors.db"))
    }

    pub fn open_at(path: &Path) -> Result<Self, CursorError> {
        let conn = Connection::open(path).map_err(|e| CursorError::Connection(e.to_string()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS connector_cursors (
                 name TEXT PRIMARY KEY,
                 cursor TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );",
        )
        .map_err(|e| CursorError::Connection(e.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Ephemeral in-memory store for tests.
    pub fn in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("sqlite in-memory");
        conn.execute_batch(
            "CREATE TABLE connector_cursors (
                 name TEXT PRIMARY KEY,
                 cursor TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );",
        )
        .expect("create table");
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }
}

#[async_trait]
impl CursorStore for SqliteCursorStore {
    async fn get(&self, name: &str) -> Result<Option<String>, CursorError> {
        let conn = self.conn.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare("SELECT cursor FROM connector_cursors WHERE name = ?1")
                .map_err(|e| CursorError::Connection(e.to_string()))?;
            let mut rows = stmt
                .query(params![name])
                .map_err(|e| CursorError::Connection(e.to_string()))?;
            match rows.next().map_err(|e| CursorError::Connection(e.to_string()))? {
                Some(row) => {
                    let v: String = row
                        .get(0)
                        .map_err(|e| CursorError::Connection(e.to_string()))?;
                    Ok(Some(v))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| CursorError::Io(e.to_string()))?
    }

    async fn set(&self, name: &str, cursor: &str) -> Result<(), CursorError> {
        let conn = self.conn.clone();
        let name = name.to_string();
        let cursor = cursor.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO connector_cursors(name, cursor, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET cursor=excluded.cursor, updated_at=excluded.updated_at",
                params![name, cursor, now],
            )
            .map_err(|e| CursorError::Connection(e.to_string()))?;
            Ok(())
        })
        .await
        .map_err(|e| CursorError::Io(e.to_string()))?
    }
}

// --- Postgres backend (paired with PostgresEventStore) ----------------

#[cfg(feature = "postgres")]
pub use self::postgres_impl::PostgresCursorStore;

#[cfg(feature = "postgres")]
mod postgres_impl {
    use super::{async_trait, CursorError, CursorStore};
    use deadpool_postgres::{Config, Pool, Runtime};
    use tokio_postgres::NoTls;

    const SCHEMA_SQL: &str = r#"
        CREATE TABLE IF NOT EXISTS connector_cursors (
            name TEXT PRIMARY KEY,
            cursor TEXT NOT NULL,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )
    "#;

    pub struct PostgresCursorStore {
        pool: Pool,
    }

    impl PostgresCursorStore {
        pub async fn connect(url: &str) -> Result<Self, CursorError> {
            let mut cfg = Config::new();
            cfg.url = Some(url.to_string());
            cfg.pool = Some(deadpool_postgres::PoolConfig::new(4));
            let pool = cfg
                .create_pool(Some(Runtime::Tokio1), NoTls)
                .map_err(|e| CursorError::Connection(format!("pool init: {e}")))?;
            {
                let client = pool
                    .get()
                    .await
                    .map_err(|e| CursorError::Connection(format!("pool get: {e}")))?;
                client
                    .batch_execute(SCHEMA_SQL)
                    .await
                    .map_err(|e| CursorError::Connection(format!("schema init: {e}")))?;
            }
            Ok(Self { pool })
        }
    }

    #[async_trait]
    impl CursorStore for PostgresCursorStore {
        async fn get(&self, name: &str) -> Result<Option<String>, CursorError> {
            let client = self
                .pool
                .get()
                .await
                .map_err(|e| CursorError::Connection(e.to_string()))?;
            let row = client
                .query_opt(
                    "SELECT cursor FROM connector_cursors WHERE name = $1",
                    &[&name],
                )
                .await
                .map_err(|e| CursorError::Connection(e.to_string()))?;
            Ok(row.map(|r| r.get::<_, String>("cursor")))
        }

        async fn set(&self, name: &str, cursor: &str) -> Result<(), CursorError> {
            let client = self
                .pool
                .get()
                .await
                .map_err(|e| CursorError::Connection(e.to_string()))?;
            client
                .execute(
                    "INSERT INTO connector_cursors(name, cursor, updated_at)
                     VALUES ($1, $2, now())
                     ON CONFLICT (name) DO UPDATE
                       SET cursor = EXCLUDED.cursor,
                           updated_at = EXCLUDED.updated_at",
                    &[&name, &cursor],
                )
                .await
                .map_err(|e| CursorError::Connection(e.to_string()))?;
            Ok(())
        }
    }
}

// --- Factory -----------------------------------------------------------

/// Open a [`CursorStore`] paired with the given event-store backend.
///
/// * [`StoreBackend::Sqlite`] → [`SqliteCursorStore`] at
///   `<project_root>/.zymi/connectors.db` (sibling to `events.db`).
/// * [`StoreBackend::Postgres`] → [`PostgresCursorStore`] using the same
///   URL the event store opened with, so multi-process `zymi serve`
///   sees one shared cursor table and doesn't double-fire on
///   `update_id`-style cursors.
pub async fn open_cursor_store(
    backend: &StoreBackend,
    project_root: &Path,
) -> Result<Arc<dyn CursorStore>, CursorError> {
    match backend {
        StoreBackend::Sqlite { .. } => Ok(Arc::new(SqliteCursorStore::open(project_root)?)),
        StoreBackend::Postgres { url } => open_postgres_cursor(url).await,
    }
}

#[cfg(feature = "postgres")]
async fn open_postgres_cursor(url: &str) -> Result<Arc<dyn CursorStore>, CursorError> {
    let store = PostgresCursorStore::connect(url).await?;
    Ok(Arc::new(store))
}

#[cfg(not(feature = "postgres"))]
async fn open_postgres_cursor(_url: &str) -> Result<Arc<dyn CursorStore>, CursorError> {
    Err(CursorError::Connection(
        "Postgres cursor store not compiled in — rebuild with `--features postgres`".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_set_then_get_roundtrips() {
        let store = SqliteCursorStore::in_memory();
        assert!(store.get("tg").await.unwrap().is_none());
        store.set("tg", "42").await.unwrap();
        assert_eq!(store.get("tg").await.unwrap().as_deref(), Some("42"));
        store.set("tg", "43").await.unwrap();
        assert_eq!(store.get("tg").await.unwrap().as_deref(), Some("43"));
    }

    #[tokio::test]
    async fn sqlite_independent_keys_do_not_clobber() {
        let store = SqliteCursorStore::in_memory();
        store.set("a", "1").await.unwrap();
        store.set("b", "2").await.unwrap();
        assert_eq!(store.get("a").await.unwrap().as_deref(), Some("1"));
        assert_eq!(store.get("b").await.unwrap().as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn sqlite_open_at_persists_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connectors.db");
        let first = SqliteCursorStore::open_at(&path).unwrap();
        first.set("gmail", "page-1").await.unwrap();
        drop(first);
        let second = SqliteCursorStore::open_at(&path).unwrap();
        assert_eq!(
            second.get("gmail").await.unwrap().as_deref(),
            Some("page-1")
        );
    }

    #[cfg(feature = "postgres")]
    mod pg {
        use super::super::PostgresCursorStore;
        use super::CursorStore;

        fn test_url() -> Option<String> {
            std::env::var("ZYMI_POSTGRES_TEST_URL").ok()
        }

        async fn fresh_store() -> Option<PostgresCursorStore> {
            let url = test_url()?;
            let store = PostgresCursorStore::connect(&url).await.ok()?;
            // Wipe any leftover rows so tests start clean.
            // Reuse the existing pool by calling a privileged statement
            // through a fresh client.
            let cli = deadpool_postgres::Config {
                url: Some(url),
                ..Default::default()
            };
            let pool = cli
                .create_pool(
                    Some(deadpool_postgres::Runtime::Tokio1),
                    tokio_postgres::NoTls,
                )
                .ok()?;
            let c = pool.get().await.ok()?;
            c.batch_execute("TRUNCATE connector_cursors").await.ok()?;
            Some(store)
        }

        #[tokio::test]
        async fn postgres_set_then_get_roundtrips() {
            let Some(store) = fresh_store().await else { return };
            assert!(store.get("tg").await.unwrap().is_none());
            store.set("tg", "100").await.unwrap();
            assert_eq!(store.get("tg").await.unwrap().as_deref(), Some("100"));
            store.set("tg", "101").await.unwrap();
            assert_eq!(store.get("tg").await.unwrap().as_deref(), Some("101"));
        }

        #[tokio::test]
        async fn postgres_two_handles_share_state() {
            // The whole point of paring with Postgres event store: two
            // processes (here, two store handles) see the same cursor.
            let Some(store_a) = fresh_store().await else { return };
            let url = test_url().unwrap();
            let store_b = PostgresCursorStore::connect(&url).await.unwrap();
            store_a.set("shared", "v1").await.unwrap();
            assert_eq!(store_b.get("shared").await.unwrap().as_deref(), Some("v1"));
            store_b.set("shared", "v2").await.unwrap();
            assert_eq!(store_a.get("shared").await.unwrap().as_deref(), Some("v2"));
        }
    }
}
