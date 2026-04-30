//! Per-connector cursor persistence (ADR-0021 §http_poll).
//!
//! `http_poll` needs a small, restart-safe value keyed by connector name
//! (Telegram `update_id`, Gmail `pageToken`, …). A cursor is just a string
//! the connector hands back on every tick; we don't interpret it.
//!
//! We deliberately do **not** extend the [`crate::events::store::EventStore`]
//! trait for this — cursors aren't part of the event stream, and forcing
//! every backend (SQLite, future Postgres) to own a cursor table bleeds
//! connector state into the core contract. Instead this module owns a
//! sibling SQLite file at `<project_root>/.zymi/connectors.db`.

use std::path::Path;
use std::sync::Arc;

use rusqlite::{params, Connection};
use thiserror::Error;
use tokio::sync::Mutex;

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
#[derive(Clone)]
pub struct CursorStore {
    conn: Arc<Mutex<Connection>>,
}

impl CursorStore {
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

    pub async fn get(&self, name: &str) -> Result<Option<String>, CursorError> {
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

    pub async fn set(&self, name: &str, cursor: &str) -> Result<(), CursorError> {
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

/// Resolve a shared cursor store lazily. The runtime calls this once per
/// startup; every `http_poll` instance shares the same handle.
pub fn resolve_cursor_store(project_root: &Path) -> Result<CursorStore, CursorError> {
    CursorStore::open(project_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_then_get_roundtrips() {
        let store = CursorStore::in_memory();
        assert!(store.get("tg").await.unwrap().is_none());
        store.set("tg", "42").await.unwrap();
        assert_eq!(store.get("tg").await.unwrap().as_deref(), Some("42"));
        store.set("tg", "43").await.unwrap();
        assert_eq!(store.get("tg").await.unwrap().as_deref(), Some("43"));
    }

    #[tokio::test]
    async fn independent_keys_do_not_clobber() {
        let store = CursorStore::in_memory();
        store.set("a", "1").await.unwrap();
        store.set("b", "2").await.unwrap();
        assert_eq!(store.get("a").await.unwrap().as_deref(), Some("1"));
        assert_eq!(store.get("b").await.unwrap().as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn open_at_persists_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connectors.db");
        let first = CursorStore::open_at(&path).unwrap();
        first.set("gmail", "page-1").await.unwrap();
        drop(first);
        let second = CursorStore::open_at(&path).unwrap();
        assert_eq!(
            second.get("gmail").await.unwrap().as_deref(),
            Some("page-1")
        );
    }
}
