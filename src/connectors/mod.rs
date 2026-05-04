//! Declarative HTTP connectors and outbound sinks (ADR-0021).
//!
//! Three built-in plugin types, all driven by YAML:
//! * [`http_inbound`] — axum webhook receiver (bearer/HMAC auth, JSONPath
//!   extract, publishes [`EventKind::UserMessageReceived`]).
//! * [`http_poll`] — Tokio timer that fetches a URL on an interval, extracts
//!   each item with JSONPath, filters on configurable field values, and
//!   persists a per-connector cursor so restarts don't double-fire.
//! * [`http_post`] — bus subscriber that filters on an `EventKind` subset,
//!   renders a MiniJinja template, and POSTs with retry/backoff.
//!
//! Each category has its own trait ([`InboundConnector`], [`OutboundSink`])
//! and is dispatched by a [`PluginRegistry`]. Runtime wiring lives in
//! [`crate::runtime::RuntimeBuilder::build_async`].

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::connectors::cursor_store::CursorStore;
use crate::events::bus::EventBus;

pub mod context;
pub mod cron;
pub mod cursor_store;
pub mod file_append;
pub mod file_read;
pub mod http_inbound;
pub mod http_poll;
pub mod http_post;
pub mod registry;
pub mod stdio;

pub use registry::{
    build_core_connectors, build_core_outputs, spawn_connectors, spawn_outputs, ConnectorStartup,
    OutputStartup,
};

/// Handle returned by a started connector/output.
///
/// The `shutdown` token is fired by the runtime at shutdown; the connector is
/// expected to observe it, finish any in-flight work, and let the
/// `JoinHandle` complete. A handle whose task has already finished is safe
/// to drop.
pub struct PluginHandle {
    pub name: String,
    pub type_name: &'static str,
    pub shutdown: CancellationToken,
    pub join: JoinHandle<()>,
}

/// Context every connector / output task receives on startup.
#[derive(Clone)]
pub struct PluginContext {
    /// The in-process event bus — connectors publish; outputs subscribe.
    pub bus: Arc<EventBus>,
    /// Project root. Used by stateful plugins (e.g. for resolving
    /// relative paths in `file_read` / `file_append`).
    pub project_root: std::path::PathBuf,
    /// Shared cursor store handle. Backend (sqlite vs postgres) is
    /// chosen once at runtime startup to pair with the event-store
    /// backend, so multi-process `zymi serve` against shared Postgres
    /// sees one cursor table instead of N local sqlite files.
    pub cursor_store: Arc<dyn CursorStore>,
}

/// Inbound connector: an event *source*. Starts a long-running task that
/// publishes events onto the bus until the cancellation token fires.
#[async_trait]
pub trait InboundConnector: Send + Sync {
    /// Short type discriminator (`"http_inbound"` / `"http_poll"` / …),
    /// duplicated from the plugin registry for log / error surfaces.
    fn type_name(&self) -> &'static str;

    /// Begin running. Implementations spawn their own tokio task and return
    /// the resulting `PluginHandle`. Returning `Err` aborts startup.
    async fn start(
        &self,
        name: String,
        ctx: PluginContext,
    ) -> Result<PluginHandle, ConnectorError>;
}

/// Outbound sink: an event *consumer*. Subscribes to the bus on startup and
/// reacts to events matching its filter.
#[async_trait]
pub trait OutboundSink: Send + Sync {
    fn type_name(&self) -> &'static str;

    async fn start(
        &self,
        name: String,
        ctx: PluginContext,
    ) -> Result<PluginHandle, ConnectorError>;
}

#[derive(Debug, Error)]
pub enum ConnectorError {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to bind listener on {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error("internal error: {0}")]
    Internal(String),
}
