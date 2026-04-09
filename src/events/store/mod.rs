//! Event store: trait, SQLite implementation, factory, and the
//! cross-process tail watcher.
//!
//! # Layout
//!
//! - [`event_store`] — the [`EventStore`] trait and [`TailedEvent`] type.
//! - [`sqlite`] — [`SqliteEventStore`], the default file-backed implementation.
//! - [`factory`] — [`open_store`] / [`StoreBackend`] for backend-agnostic construction.
//! - [`watcher`] — [`StoreTailWatcher`], the polling task that fans out
//!   events written by other processes into the in-process [`EventBus`](crate::events::bus::EventBus).
//!
//! All call sites outside this module should depend on `Arc<dyn EventStore>`
//! and construct stores via [`open_store`]; concrete types are an
//! implementation detail.

pub mod event_store;
pub mod factory;
pub mod sqlite;
pub mod watcher;

pub use event_store::{EventStore, TailedEvent};
pub use factory::{open_store, StoreBackend};
pub use sqlite::SqliteEventStore;
pub use watcher::{StoreTailWatcher, TailWatcherPolicy, WatcherHandle};
