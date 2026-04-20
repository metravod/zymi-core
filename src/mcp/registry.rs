//! Live registry of MCP server connections, indexed by server name.
//!
//! Slice 3 (per ADR-0023): the registry is the runtime-side counterpart to
//! the catalog's `mcp__server__tool` entries. The catalog answers "is this a
//! known tool?"; the registry answers "where do I send `tools/call`?".
//!
//! Slice 5 adds reactive restart: on a transport failure during `call`, the
//! registry consults each entry's [`RestartPolicy`] and — if budget
//! permits — re-spawns the subprocess, re-performs the `initialize`
//! handshake, and retries the call exactly once. Restart attempts are
//! serialised per server via the `RwLock` around the connection slot; a
//! concurrent caller that arrives mid-restart blocks on the read lock and
//! then transparently uses the fresh connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::RwLock;

use crate::events::bus::EventBus;
use crate::events::{Event, EventKind};
use crate::mcp::connection::{
    McpCallResult, McpError, McpServerConnection, McpServerSpec,
};
use crate::mcp::transport::TransportError;

/// Catalog-id prefix that marks a tool as MCP-backed. See ADR-0023.
pub const MCP_PREFIX: &str = "mcp__";
const SEPARATOR: &str = "__";

/// Build the canonical catalog id for an MCP-backed tool.
pub fn make_id(server: &str, tool: &str) -> String {
    format!("{MCP_PREFIX}{server}{SEPARATOR}{tool}")
}

/// Reverse of [`make_id`]. Returns `(server, tool)` or `None` if `name` is not
/// a well-formed MCP id. The server segment is everything between the prefix
/// and the **first** `__` separator after it; the tool is the remainder.
pub fn parse_id(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix(MCP_PREFIX)?;
    let sep = rest.find(SEPARATOR)?;
    let server = &rest[..sep];
    let tool = &rest[sep + SEPARATOR.len()..];
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// Validate that a server or tool segment is safe to embed in an MCP id.
/// `__` would shift the parse boundary and silently misroute calls.
pub fn validate_segment(segment: &str) -> Result<(), String> {
    if segment.is_empty() {
        return Err("empty segment".into());
    }
    if segment.contains(SEPARATOR) {
        return Err(format!("segment '{segment}' contains '__' (reserved separator)"));
    }
    Ok(())
}

/// Per-server reactive restart policy (ADR-0023 §Lifecycle).
///
/// `max_restarts == 0` disables restart entirely — on a transport failure,
/// the caller gets the error and the server stays down for the rest of the
/// runtime. Backoff is applied *before* each restart attempt (attempt 1
/// sleeps `backoff_secs[0]`, attempt 2 sleeps `backoff_secs[1]`, …; the last
/// element is reused once attempts exceed the list).
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    pub spec: McpServerSpec,
    pub init_timeout: Duration,
    pub call_timeout: Duration,
    pub max_restarts: u32,
    pub backoff_secs: Vec<u64>,
}

impl RestartPolicy {
    fn backoff_for(&self, attempt: u32) -> Duration {
        if self.backoff_secs.is_empty() {
            return Duration::from_secs(1);
        }
        let idx = (attempt.saturating_sub(1) as usize).min(self.backoff_secs.len() - 1);
        Duration::from_secs(self.backoff_secs[idx])
    }
}

/// One server slot in the registry. The connection is wrapped in an
/// [`RwLock`] so a restarting caller can swap it while readers wait.
struct Entry {
    conn: RwLock<Arc<McpServerConnection>>,
    restart: Option<RestartPolicy>,
    attempts: AtomicU32,
}

/// Read-only handle to the set of live MCP server connections.
#[derive(Default)]
pub struct McpRegistry {
    servers: HashMap<String, Arc<Entry>>,
    bus: Option<Arc<EventBus>>,
}

impl std::fmt::Debug for McpRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRegistry")
            .field("servers", &self.servers.keys().collect::<Vec<_>>())
            .field("bus", &self.bus.is_some())
            .finish()
    }
}

impl McpRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach an event bus. Crash / restart lifecycle events
    /// ([`EventKind::McpServerDisconnected`]) are published here as they
    /// happen; without a bus, they're logged and dropped.
    pub fn with_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Insert a connection with no restart policy. Transport failures on
    /// this server surface to the caller as-is.
    pub fn insert(&mut self, name: impl Into<String>, conn: Arc<McpServerConnection>) {
        self.servers.insert(
            name.into(),
            Arc::new(Entry {
                conn: RwLock::new(conn),
                restart: None,
                attempts: AtomicU32::new(0),
            }),
        );
    }

    /// Insert a connection with a restart policy. On a transport error
    /// during [`Self::call`], the registry re-spawns the subprocess, redoes
    /// the handshake, and retries the call once (subject to the policy's
    /// `max_restarts` budget).
    pub fn insert_with_restart(
        &mut self,
        name: impl Into<String>,
        conn: Arc<McpServerConnection>,
        policy: RestartPolicy,
    ) {
        self.servers.insert(
            name.into(),
            Arc::new(Entry {
                conn: RwLock::new(conn),
                restart: Some(policy),
                attempts: AtomicU32::new(0),
            }),
        );
    }

    pub fn server_names(&self) -> impl Iterator<Item = &str> {
        self.servers.keys().map(|s| s.as_str())
    }

    /// Dispatch a `tools/call` to the server identified by `server`. On
    /// transport failure, attempts one reactive restart if the server has a
    /// [`RestartPolicy`] and budget remains.
    pub async fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<McpCallResult, McpError> {
        let entry = self
            .servers
            .get(server)
            .ok_or_else(|| McpError::Config(format!("unknown mcp server '{server}'")))?
            .clone();

        let conn = entry.conn.read().await.clone();
        match conn.call_tool(tool, arguments.clone()).await {
            Ok(result) => Ok(result),
            Err(McpError::Transport(ref te)) if is_restart_triggering(te) => {
                match self.try_restart(server, &entry, te.to_string()).await {
                    Ok(fresh) => fresh.call_tool(tool, arguments).await,
                    Err(restart_err) => Err(restart_err),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Best-effort shutdown of every server in the registry.
    pub async fn shutdown_all(&self, grace: Duration) {
        for (_, entry) in self.servers.iter() {
            let conn = entry.conn.read().await.clone();
            conn.shutdown(grace).await;
        }
    }

    /// Take the writer lock for `entry.conn`, confirm nobody else already
    /// replaced the connection, then re-spawn and swap. Returns the fresh
    /// `Arc<McpServerConnection>` for the retry. Publishes a
    /// `McpServerDisconnected` with `reason = "crash: …"` before attempting
    /// the restart, and a second `McpServerDisconnected` with
    /// `reason = "restart_failed: …"` if the fresh spawn fails.
    async fn try_restart(
        &self,
        server: &str,
        entry: &Entry,
        failure_reason: String,
    ) -> Result<Arc<McpServerConnection>, McpError> {
        let Some(policy) = entry.restart.clone() else {
            return Err(McpError::Transport(TransportError::Closed));
        };

        // Serialize restarts: first writer wins. A later caller who arrives
        // after the swap sees `Arc::ptr_eq` false and reuses the new conn.
        let mut guard = entry.conn.write().await;
        let observed = (*guard).clone();
        // If a prior caller already reconnected, re-check the current conn
        // vs the one we saw when we failed; if it changed, surface it to the
        // caller without consuming an additional restart budget.
        // (Caller's `conn` was captured before taking the write lock.)
        if !Arc::ptr_eq(&observed, &*guard) {
            return Ok(guard.clone());
        }

        let prior = entry.attempts.load(Ordering::SeqCst);
        if prior >= policy.max_restarts {
            self.publish_disconnect(server, format!("restart_exhausted: {failure_reason}"))
                .await;
            return Err(McpError::Config(format!(
                "mcp server '{server}' restart budget exhausted after {prior} attempt(s)"
            )));
        }
        let attempt = prior + 1;
        entry.attempts.store(attempt, Ordering::SeqCst);

        self.publish_disconnect(server, format!("crash: {failure_reason}"))
            .await;

        // Tear down the dead subprocess explicitly. Its `call_tool` already
        // failed, but the Child handle may still be present. A short grace
        // here avoids a zombie before the re-spawn reclaims stdio.
        observed.shutdown(Duration::from_millis(100)).await;

        tokio::time::sleep(policy.backoff_for(attempt)).await;

        let fresh = McpServerConnection::connect(
            policy.spec.clone(),
            policy.init_timeout,
            policy.call_timeout,
        )
        .await;

        match fresh {
            Ok(conn) => {
                let conn = Arc::new(conn);
                *guard = conn.clone();
                Ok(conn)
            }
            Err(e) => {
                self.publish_disconnect(server, format!("restart_failed: {e}"))
                    .await;
                Err(e)
            }
        }
    }

    async fn publish_disconnect(&self, server: &str, reason: String) {
        let Some(bus) = self.bus.as_ref() else {
            log::warn!("mcp[{server}] {reason} (no event bus wired)");
            return;
        };
        let event = Event::new(
            "system".into(),
            EventKind::McpServerDisconnected {
                server: server.to_string(),
                reason,
            },
            "mcp".into(),
        );
        if let Err(e) = bus.publish(event).await {
            log::warn!("failed to publish mcp lifecycle event: {e}");
        }
    }
}

/// Which transport errors should trigger a restart attempt. Config-level
/// errors (e.g. malformed responses during the first initialize) are *not*
/// restart-worthy; they'd just loop. A closed pipe, a send failure, or a
/// read timeout mid-session almost always mean the subprocess died.
fn is_restart_triggering(err: &TransportError) -> bool {
    matches!(
        err,
        TransportError::Closed | TransportError::Io(_) | TransportError::Timeout
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_and_parse_roundtrip() {
        let id = make_id("github", "create_issue");
        assert_eq!(id, "mcp__github__create_issue");
        let (server, tool) = parse_id(&id).unwrap();
        assert_eq!(server, "github");
        assert_eq!(tool, "create_issue");
    }

    #[test]
    fn parse_id_handles_underscores_in_tool_name() {
        let id = make_id("postgres", "query_with_args");
        let (server, tool) = parse_id(&id).unwrap();
        assert_eq!(server, "postgres");
        // The first `__` after the prefix is the boundary; later single `_`
        // characters in the tool name are preserved.
        assert_eq!(tool, "query_with_args");
    }

    #[test]
    fn parse_id_rejects_non_prefixed() {
        assert!(parse_id("write_file").is_none());
        assert!(parse_id("mcp_github_x").is_none()); // single underscore
    }

    #[test]
    fn parse_id_rejects_empty_segments() {
        assert!(parse_id("mcp____tool").is_none()); // empty server
        assert!(parse_id("mcp__server__").is_none()); // empty tool
        assert!(parse_id("mcp__server").is_none()); // no separator
    }

    #[test]
    fn validate_segment_rejects_double_underscore() {
        assert!(validate_segment("github").is_ok());
        assert!(validate_segment("query_one").is_ok());
        assert!(validate_segment("evil__name").is_err());
        assert!(validate_segment("").is_err());
    }

    #[test]
    fn restart_backoff_saturates_on_last_entry() {
        let policy = RestartPolicy {
            spec: McpServerSpec {
                name: "x".into(),
                command: vec!["true".into()],
                env: HashMap::new(),
            },
            init_timeout: Duration::from_secs(1),
            call_timeout: Duration::from_secs(1),
            max_restarts: 10,
            backoff_secs: vec![1, 5, 30],
        };
        assert_eq!(policy.backoff_for(1), Duration::from_secs(1));
        assert_eq!(policy.backoff_for(2), Duration::from_secs(5));
        assert_eq!(policy.backoff_for(3), Duration::from_secs(30));
        assert_eq!(policy.backoff_for(99), Duration::from_secs(30));
    }

    #[test]
    fn restart_backoff_empty_list_defaults_one_second() {
        let policy = RestartPolicy {
            spec: McpServerSpec {
                name: "x".into(),
                command: vec!["true".into()],
                env: HashMap::new(),
            },
            init_timeout: Duration::from_secs(1),
            call_timeout: Duration::from_secs(1),
            max_restarts: 3,
            backoff_secs: Vec::new(),
        };
        assert_eq!(policy.backoff_for(1), Duration::from_secs(1));
        assert_eq!(policy.backoff_for(99), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn registry_unknown_server_is_config_error() {
        let registry = McpRegistry::new();
        let err = registry
            .call("absent", "x", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Config(_)), "got {err:?}");
    }
}
