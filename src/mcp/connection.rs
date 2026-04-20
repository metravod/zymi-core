//! Connection to a single MCP server: subprocess + transport + handshake.
//!
//! Slice 2 (per ADR-0023): spawn subprocess with explicit env (parent env NOT
//! inherited), perform `initialize` handshake, expose `tools/list` and
//! `tools/call`. Restart policy and `mcp_servers:` config wiring land in
//! later slices.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::Mutex;

use crate::mcp::transport::{Transport, TransportError};

/// MCP protocol version this client speaks. Servers that negotiate a different
/// version still answer; we record but don't reject — wire compatibility for
/// `initialize`+`tools/*` has been stable across recent revisions.
const PROTOCOL_VERSION: &str = "2024-11-05";
const CLIENT_NAME: &str = "zymi-core";

#[derive(Debug, Error)]
pub enum McpError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("spawn failed: {0}")]
    Spawn(std::io::Error),
    #[error("server missing stdio handles")]
    MissingStdio,
    #[error("server config invalid: {0}")]
    Config(String),
    #[error("malformed server response: {0}")]
    Malformed(String),
}

/// User-facing description of one MCP server. Maps 1:1 to a `mcp_servers:`
/// entry in `project.yml` (wired in a later slice).
#[derive(Debug, Clone)]
pub struct McpServerSpec {
    pub name: String,
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
}

/// Tool metadata returned by `tools/list`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// Result of a `tools/call`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct McpCallResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// Live connection to one MCP server.
///
/// `Sync` so an `Arc<McpServerConnection>` can be shared across the registry
/// and the action executor. The child handle is wrapped in a [`Mutex`] so the
/// otherwise-`!Sync` `tokio::process::Child` does not infect the public type.
pub struct McpServerConnection {
    name: String,
    transport: Transport,
    /// `None` only in tests that drive a transport over duplex streams.
    child: Mutex<Option<Child>>,
    call_timeout: Duration,
}

impl std::fmt::Debug for McpServerConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerConnection")
            .field("name", &self.name)
            .field("call_timeout", &self.call_timeout)
            .finish_non_exhaustive()
    }
}

impl McpServerConnection {
    /// Spawn the server subprocess and complete the MCP `initialize` handshake.
    pub async fn connect(
        spec: McpServerSpec,
        init_timeout: Duration,
        call_timeout: Duration,
    ) -> Result<Self, McpError> {
        if spec.command.is_empty() {
            return Err(McpError::Config("empty command".into()));
        }

        let mut cmd = Command::new(&spec.command[0]);
        cmd.args(&spec.command[1..])
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(McpError::Spawn)?;
        let stdin = child.stdin.take().ok_or(McpError::MissingStdio)?;
        let stdout = child.stdout.take().ok_or(McpError::MissingStdio)?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr(spec.name.clone(), stderr));
        }

        let transport = Transport::new(BufReader::new(stdout), stdin);
        perform_handshake(&transport, init_timeout).await?;

        Ok(Self {
            name: spec.name,
            transport,
            child: Mutex::new(Some(child)),
            call_timeout,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `tools/list`. Returns the server's full advertised tool set; filtering
    /// against `allow:` / `deny:` happens one layer up (catalog wiring).
    pub async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        let result = self
            .transport
            .request("tools/list", None, self.call_timeout)
            .await?;
        let tools_val = result
            .get("tools")
            .cloned()
            .ok_or_else(|| McpError::Malformed("response missing `tools` field".into()))?;
        serde_json::from_value(tools_val).map_err(|e| McpError::Malformed(e.to_string()))
    }

    /// `tools/call`. Errors reported by the server as `{ isError: true }` are
    /// returned as `Ok(McpCallResult { is_error: true, .. })`, not as `Err`.
    /// Transport / RPC failures are `Err`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<McpCallResult, McpError> {
        let params = json!({ "name": name, "arguments": arguments });
        let result = self
            .transport
            .request("tools/call", Some(params), self.call_timeout)
            .await?;
        serde_json::from_value(result).map_err(|e| McpError::Malformed(e.to_string()))
    }

    /// Best-effort shutdown: wait for the child up to `grace` (relies on the
    /// transport being dropped or the server reacting to lack of traffic),
    /// SIGKILL on timeout. Idempotent — second call is a no-op. MCP does not
    /// define a `shutdown` RPC; transport close is the signal.
    pub async fn shutdown(&self, grace: Duration) {
        let mut guard = self.child.lock().await;
        let Some(mut child) = guard.take() else {
            return;
        };
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn from_transport_for_test(
        name: String,
        transport: Transport,
        init_timeout: Duration,
        call_timeout: Duration,
    ) -> Result<Self, McpError> {
        perform_handshake(&transport, init_timeout).await?;
        Ok(Self {
            name,
            transport,
            child: Mutex::new(None),
            call_timeout,
        })
    }
}

async fn perform_handshake(
    transport: &Transport,
    init_timeout: Duration,
) -> Result<(), McpError> {
    let params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": CLIENT_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        }
    });
    transport.request("initialize", Some(params), init_timeout).await?;
    transport
        .notify("notifications/initialized", None)
        .await?;
    Ok(())
}

async fn drain_stderr(name: String, stderr: ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let trimmed = line.trim_end();
                if !trimmed.is_empty() {
                    log::debug!("mcp[{name}] stderr: {trimmed}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncWriteExt, BufReader};

    /// Spin up a fake MCP server over duplex streams. The `script` closure
    /// receives the server-side reader and writer and runs the desired
    /// protocol exchange.
    async fn with_fake_server<F, Fut>(
        init_timeout: Duration,
        call_timeout: Duration,
        script: F,
    ) -> Result<(McpServerConnection, tokio::task::JoinHandle<()>), McpError>
    where
        F: FnOnce(BufReader<tokio::io::DuplexStream>, tokio::io::DuplexStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (client_to_server_w, client_to_server_r) = duplex(8192);
        let (server_to_client_w, server_to_client_r) = duplex(8192);
        let server_handle = tokio::spawn(script(BufReader::new(client_to_server_r), server_to_client_w));
        let transport = Transport::new(BufReader::new(server_to_client_r), client_to_server_w);
        let conn = McpServerConnection::from_transport_for_test(
            "test".into(),
            transport,
            init_timeout,
            call_timeout,
        )
        .await?;
        Ok((conn, server_handle))
    }

    async fn read_one(reader: &mut BufReader<tokio::io::DuplexStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).expect("server got invalid json")
    }

    async fn write_response(writer: &mut tokio::io::DuplexStream, id: u64, result: Value) {
        let mut data =
            serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": id, "result": result})).unwrap();
        data.push(b'\n');
        writer.write_all(&data).await.unwrap();
        writer.flush().await.unwrap();
    }

    #[tokio::test]
    async fn handshake_sends_initialize_then_initialized_notification() {
        let (_conn, server) = with_fake_server(
            Duration::from_secs(2),
            Duration::from_secs(2),
            |mut r, mut w| async move {
                let init = read_one(&mut r).await;
                assert_eq!(init["method"], "initialize");
                let id = init["id"].as_u64().unwrap();
                assert_eq!(init["params"]["protocolVersion"], "2024-11-05");
                assert_eq!(init["params"]["clientInfo"]["name"], "zymi-core");
                write_response(
                    &mut w,
                    id,
                    json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fake", "version": "0.0.0"}
                    }),
                )
                .await;

                let initialized = read_one(&mut r).await;
                assert_eq!(initialized["method"], "notifications/initialized");
                assert!(initialized.get("id").is_none());
            },
        )
        .await
        .expect("handshake should succeed");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn list_tools_parses_advertised_tools() {
        let (conn, server) = with_fake_server(
            Duration::from_secs(2),
            Duration::from_secs(2),
            |mut r, mut w| async move {
                let init = read_one(&mut r).await;
                write_response(
                    &mut w,
                    init["id"].as_u64().unwrap(),
                    json!({"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "f", "version": "0"}}),
                )
                .await;
                let _initialized = read_one(&mut r).await;

                let req = read_one(&mut r).await;
                assert_eq!(req["method"], "tools/list");
                write_response(
                    &mut w,
                    req["id"].as_u64().unwrap(),
                    json!({
                        "tools": [
                            {"name": "search", "description": "Search the web",
                             "inputSchema": {"type": "object"}},
                            {"name": "fetch"}
                        ]
                    }),
                )
                .await;
            },
        )
        .await
        .unwrap();

        let tools = conn.list_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].description.as_deref(), Some("Search the web"));
        assert_eq!(tools[0].input_schema, json!({"type": "object"}));
        assert_eq!(tools[1].name, "fetch");
        assert_eq!(tools[1].description, None);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn call_tool_sends_name_and_arguments() {
        let (conn, server) = with_fake_server(
            Duration::from_secs(2),
            Duration::from_secs(2),
            |mut r, mut w| async move {
                let init = read_one(&mut r).await;
                write_response(
                    &mut w,
                    init["id"].as_u64().unwrap(),
                    json!({"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "f", "version": "0"}}),
                )
                .await;
                let _initialized = read_one(&mut r).await;

                let call = read_one(&mut r).await;
                assert_eq!(call["method"], "tools/call");
                assert_eq!(call["params"]["name"], "search");
                assert_eq!(call["params"]["arguments"]["q"], "rust mcp");
                write_response(
                    &mut w,
                    call["id"].as_u64().unwrap(),
                    json!({
                        "content": [{"type": "text", "text": "result body"}],
                        "isError": false
                    }),
                )
                .await;
            },
        )
        .await
        .unwrap();

        let result = conn.call_tool("search", json!({"q": "rust mcp"})).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.content[0]["text"], "result body");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn server_reported_tool_error_is_not_transport_error() {
        let (conn, server) = with_fake_server(
            Duration::from_secs(2),
            Duration::from_secs(2),
            |mut r, mut w| async move {
                let init = read_one(&mut r).await;
                write_response(
                    &mut w,
                    init["id"].as_u64().unwrap(),
                    json!({"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "f", "version": "0"}}),
                )
                .await;
                let _initialized = read_one(&mut r).await;

                let call = read_one(&mut r).await;
                write_response(
                    &mut w,
                    call["id"].as_u64().unwrap(),
                    json!({
                        "content": [{"type": "text", "text": "rate limited"}],
                        "isError": true
                    }),
                )
                .await;
            },
        )
        .await
        .unwrap();

        let result = conn.call_tool("search", json!({})).await.unwrap();
        assert!(result.is_error);
        assert_eq!(result.content[0]["text"], "rate limited");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn handshake_times_out_when_server_silent() {
        let (client_to_server_w, _client_to_server_r) = duplex(1024);
        let (_server_to_client_w, server_to_client_r) = duplex(1024);
        let transport = Transport::new(BufReader::new(server_to_client_r), client_to_server_w);
        let err = McpServerConnection::from_transport_for_test(
            "silent".into(),
            transport,
            Duration::from_millis(50),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, McpError::Transport(TransportError::Timeout)), "got {err:?}");
    }

    #[tokio::test]
    async fn connect_rejects_empty_command() {
        let err = McpServerConnection::connect(
            McpServerSpec {
                name: "x".into(),
                command: vec![],
                env: HashMap::new(),
            },
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, McpError::Config(_)), "got {err:?}");
    }

    /// Real-subprocess smoke test: spawn `cat` so `read_line` returns the
    /// initialize request unchanged, which fails handshake parsing — but the
    /// spawn + stdio wiring is exercised end-to-end. Skipped on platforms
    /// without `cat`.
    #[tokio::test]
    async fn connect_spawns_real_subprocess() {
        if which_cat().is_none() {
            return;
        }
        let err = McpServerConnection::connect(
            McpServerSpec {
                name: "echo".into(),
                command: vec!["cat".into()],
                env: HashMap::new(),
            },
            Duration::from_millis(200),
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        // `cat` echoes the initialize JSON back; the client sees it as a frame
        // with `method=initialize` (server-initiated) and drops it, then
        // times out waiting for a real response. Either Timeout or Rpc would
        // be an acceptable failure here.
        assert!(matches!(
            err,
            McpError::Transport(TransportError::Timeout)
                | McpError::Transport(TransportError::Rpc { .. })
        ));
    }

    fn which_cat() -> Option<()> {
        std::process::Command::new("cat")
            .arg("--version")
            .output()
            .ok()
            .map(|_| ())
    }
}
