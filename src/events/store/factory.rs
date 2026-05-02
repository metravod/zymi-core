use std::path::PathBuf;
use std::sync::Arc;

use crate::events::EventStoreError;

use super::event_store::EventStore;
#[cfg(feature = "postgres")]
use super::postgres::PostgresEventStore;
use super::sqlite::SqliteEventStore;

/// Backend selector for [`open_store`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum StoreBackend {
    /// File-backed embedded SQLite — the zero-config default.
    Sqlite { path: PathBuf },
    /// Networked Postgres backend (ADR-0012, v0.4 sprint 3). Behind the
    /// `postgres` Cargo feature. The variant is always defined (so config
    /// parsing can recognise `postgres://...` URLs without conditional
    /// compilation), but [`open_store`] returns an error when the feature
    /// is not enabled.
    Postgres { url: String },
}

/// Open an [`EventStore`] for the given backend (sync entry point).
///
/// This is the legacy entry retained for sqlite-only call sites that have
/// no async context handy. **Postgres requires async**: callers that may
/// see a `StoreBackend::Postgres` must use [`open_store_async`] instead;
/// this function returns a clear error rather than panicking via a nested
/// runtime trick.
pub fn open_store(backend: StoreBackend) -> Result<Arc<dyn EventStore>, EventStoreError> {
    match backend {
        StoreBackend::Sqlite { path } => {
            let store = SqliteEventStore::new(&path)?;
            Ok(Arc::new(store))
        }
        StoreBackend::Postgres { .. } => Err(EventStoreError::Connection(
            "Postgres backend requires async open — use open_store_async".into(),
        )),
    }
}

/// Async entry point. The canonical way to open a store when the call
/// site is already inside a tokio runtime (RuntimeBuilder, CLI commands
/// that wrap their work in `rt.block_on`).
pub async fn open_store_async(
    backend: StoreBackend,
) -> Result<Arc<dyn EventStore>, EventStoreError> {
    match backend {
        StoreBackend::Sqlite { path } => {
            let store = SqliteEventStore::new(&path)?;
            Ok(Arc::new(store))
        }
        StoreBackend::Postgres { url } => open_postgres(&url).await,
    }
}

#[cfg(feature = "postgres")]
async fn open_postgres(url: &str) -> Result<Arc<dyn EventStore>, EventStoreError> {
    let store = PostgresEventStore::connect(url).await?;
    Ok(Arc::new(store))
}

#[cfg(not(feature = "postgres"))]
async fn open_postgres(_url: &str) -> Result<Arc<dyn EventStore>, EventStoreError> {
    Err(EventStoreError::Connection(
        "Postgres backend not compiled in — rebuild with `--features postgres`".into(),
    ))
}
