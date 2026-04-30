//! Built-in connector/output registrations and startup helpers.

use std::sync::Arc;

use crate::connectors::cursor_store::CursorStore;
use crate::connectors::http_inbound::HttpInboundBuilder;
use crate::connectors::http_poll::HttpPollBuilder;
use crate::connectors::http_post::HttpPostBuilder;
use crate::connectors::{InboundConnector, OutboundSink, PluginContext, PluginHandle};
use crate::events::bus::EventBus;
use crate::plugin::PluginRegistry;

/// Populate the inbound registry with every built-in connector type.
pub fn build_core_connectors() -> PluginRegistry<dyn InboundConnector> {
    let mut r: PluginRegistry<dyn InboundConnector> = PluginRegistry::new();
    r.register(Box::new(HttpInboundBuilder));
    r.register(Box::new(HttpPollBuilder));
    r
}

/// Populate the outbound registry with every built-in sink type.
pub fn build_core_outputs() -> PluginRegistry<dyn OutboundSink> {
    let mut r: PluginRegistry<dyn OutboundSink> = PluginRegistry::new();
    r.register(Box::new(HttpPostBuilder));
    r
}

/// Result of starting all configured inbound connectors.
pub struct ConnectorStartup {
    pub handles: Vec<PluginHandle>,
    pub failures: Vec<(String, String)>,
}

/// Result of starting all configured outbound sinks.
pub struct OutputStartup {
    pub handles: Vec<PluginHandle>,
    pub failures: Vec<(String, String)>,
}

/// Build every `project.connectors` entry via the registry and start them.
///
/// Startup is best-effort, matching the MCP pattern: a builder or start
/// failure is captured in `failures` and surfaces as a log line, but does
/// not abort runtime construction. Config-shape errors (duplicate names,
/// missing `type:`) are hard errors returned upfront.
pub async fn spawn_connectors(
    entries: &[serde_yml::Value],
    bus: Arc<EventBus>,
    project_root: std::path::PathBuf,
) -> Result<ConnectorStartup, String> {
    if entries.is_empty() {
        return Ok(ConnectorStartup {
            handles: Vec::new(),
            failures: Vec::new(),
        });
    }

    let registry = build_core_connectors();
    let built = registry
        .build_all(entries.to_vec())
        .map_err(|e| format!("connectors: {e}"))?;

    // Pre-create the shared cursor store once so http_poll instances don't
    // race to initialise it.
    if entries.iter().any(is_http_poll) {
        // Validate the cursor store can open before we spawn.
        CursorStore::open(&project_root).map_err(|e| format!("connectors: {e}"))?;
    }

    let ctx = PluginContext {
        bus,
        project_root,
    };

    let mut handles = Vec::new();
    let mut failures = Vec::new();
    for (name, plugin) in built {
        match plugin.start(name.clone(), ctx.clone()).await {
            Ok(handle) => handles.push(handle),
            Err(err) => {
                log::warn!("connector '{name}' failed to start: {err}");
                failures.push((name, err.to_string()));
            }
        }
    }
    Ok(ConnectorStartup { handles, failures })
}

/// Same pattern for outbound sinks.
pub async fn spawn_outputs(
    entries: &[serde_yml::Value],
    bus: Arc<EventBus>,
    project_root: std::path::PathBuf,
) -> Result<OutputStartup, String> {
    if entries.is_empty() {
        return Ok(OutputStartup {
            handles: Vec::new(),
            failures: Vec::new(),
        });
    }

    let registry = build_core_outputs();
    let built = registry
        .build_all(entries.to_vec())
        .map_err(|e| format!("outputs: {e}"))?;

    let ctx = PluginContext { bus, project_root };

    let mut handles = Vec::new();
    let mut failures = Vec::new();
    for (name, sink) in built {
        match sink.start(name.clone(), ctx.clone()).await {
            Ok(handle) => handles.push(handle),
            Err(err) => {
                log::warn!("output '{name}' failed to start: {err}");
                failures.push((name, err.to_string()));
            }
        }
    }
    Ok(OutputStartup { handles, failures })
}

fn is_http_poll(entry: &serde_yml::Value) -> bool {
    entry
        .as_mapping()
        .and_then(|m| m.get(serde_yml::Value::String("type".into())))
        .and_then(|v| v.as_str())
        == Some("http_poll")
}
