//! Observability tool *port* for `zymi mcp serve` (ADR-0034).
//!
//! The four `zymi.runs.*` introspection tools read the event store and reuse
//! projections (`list_runs`, `format_event`, `events_to_messages`) that live in
//! the `cli` layer. But `mcp::server` is `runtime`-gated and can compile without
//! `cli` (e.g. `--features webhook`), so it must not import `cli` directly.
//!
//! This trait is the dependency-free seam: `mcp::server` defines the port over
//! `serde_json::Value` + [`RpcError`]; the `cli` layer provides the adapter
//! (`crate::cli::mcp_observability`) that actually reads runs. `zymi mcp serve`
//! is a `cli` command, so the adapter is always present where the tools matter.

use std::sync::Arc;

use serde_json::Value;

use super::protocol::RpcError;

/// Adapter that answers the `zymi.runs.*` observability tools. Implemented in
/// the `cli` layer (which owns the run/event projections). Passed into the
/// serve loop as `Option<Arc<dyn ObservabilityProvider>>` — `None` when
/// `--expose-observability` is off or the server runs without the `cli` layer.
#[async_trait::async_trait]
pub trait ObservabilityProvider: Send + Sync {
    /// Tool descriptors to append to `tools/list` (the four `zymi.runs.*`).
    fn tool_descriptors(&self) -> Vec<Value>;

    /// Whether this provider serves the given tool name.
    fn handles(&self, tool_name: &str) -> bool;

    /// Dispatch a `tools/call` for one of our tools, returning a
    /// `CallToolResult` value (the `{ content, isError }` shape).
    async fn call(&self, tool_name: &str, arguments: &Value) -> Result<Value, RpcError>;

    /// Record that the server initiated a run with this `run_id` (stream id),
    /// so default (`session`) scope can restrict introspection to runs this
    /// serve process started. No-op for providers scoped to `all`.
    fn record_run(&self, run_id: &str);
}

/// Convenience alias for the optional, shared provider threaded through serve.
pub type SharedObservability = Option<Arc<dyn ObservabilityProvider>>;
