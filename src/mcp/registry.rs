//! Live registry of MCP server connections, indexed by server name.
//!
//! Slice 3 (per ADR-0023): the registry is the runtime-side counterpart to
//! the catalog's `mcp__server__tool` entries. The catalog answers "is this a
//! known tool?"; the registry answers "where do I send `tools/call`?".

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::mcp::connection::{McpCallResult, McpError, McpServerConnection};

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

/// Read-only handle to the set of live MCP server connections.
#[derive(Clone, Default)]
pub struct McpRegistry {
    servers: HashMap<String, Arc<McpServerConnection>>,
}

impl std::fmt::Debug for McpRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRegistry")
            .field("servers", &self.servers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl McpRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a connection. Caller is expected to have validated the server
    /// name with [`validate_segment`] before reaching this point.
    pub fn insert(&mut self, name: impl Into<String>, conn: Arc<McpServerConnection>) {
        self.servers.insert(name.into(), conn);
    }

    pub fn get(&self, server: &str) -> Option<&Arc<McpServerConnection>> {
        self.servers.get(server)
    }

    pub fn server_names(&self) -> impl Iterator<Item = &str> {
        self.servers.keys().map(|s| s.as_str())
    }

    /// Dispatch a `tools/call` to the server identified by `server`.
    pub async fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<McpCallResult, McpError> {
        let conn = self
            .servers
            .get(server)
            .ok_or_else(|| McpError::Config(format!("unknown mcp server '{server}'")))?;
        conn.call_tool(tool, arguments).await
    }

    /// Best-effort shutdown of every server in the registry.
    pub async fn shutdown_all(&self, grace: std::time::Duration) {
        for (_, conn) in self.servers.iter() {
            conn.shutdown(grace).await;
        }
    }
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
