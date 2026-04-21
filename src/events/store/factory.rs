use std::path::PathBuf;
use std::sync::Arc;

use crate::events::EventStoreError;

use super::event_store::EventStore;
use super::sqlite::SqliteEventStore;

/// Backend selector for [`open_store`]. Add a new variant when introducing
/// another EventStore implementation (libSQL, Postgres, ...).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum StoreBackend {
    Sqlite { path: PathBuf },
}

/// Open an [`EventStore`] for the given backend.
///
/// This is the **only** place in the crate that constructs a concrete
/// store implementation. All other call sites should depend on
/// `Arc<dyn EventStore>` so backends are swappable without code changes.
pub fn open_store(backend: StoreBackend) -> Result<Arc<dyn EventStore>, EventStoreError> {
    match backend {
        StoreBackend::Sqlite { path } => {
            let store = SqliteEventStore::new(&path)?;
            Ok(Arc::new(store))
        }
    }
}
