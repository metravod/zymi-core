//! Action execution port.
//!
//! Splits "what an agent wants to do" (an [`Intention`] approved by the
//! [`Orchestrator`](crate::esaa::orchestrator::Orchestrator)) from "how it
//! actually happens" (running the underlying tool against host resources).
//!
//! Slice 1 of the runtime unification (ADR-0013) lifts the inline
//! `OrchestratorResult::Approved → execute_builtin_tool(...)` branch out of
//! the engine loop and behind a trait so the engine no longer owns built-in
//! tool details. Custom executors (Python `@tool`, MCP, sandboxed runners)
//! plug in by implementing the same trait.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use crate::config::tool::{HttpMethod, ImplementationConfig};
use crate::engine::tools::{execute_builtin_tool, MemoryStore};
use crate::mcp::McpRegistry;
use super::shell_session::{ShellSessionPool};
use super::tool_catalog::ToolCatalog;

use log::warn;

/// Per-call context for an [`ActionExecutor`].
///
/// Lifetimes are tied to a single tool invocation: `project_root` is the
/// pipeline's working directory and `memory` is the per-run [`MemoryStore`]
/// (event-sourced via [`crate::engine::tools::MemoryBridge`], ADR-0016 §2).
/// Keeping memory in the context (rather than inside the executor) preserves
/// today's behaviour where each pipeline run starts with a fresh memory store,
/// even when many runs share the same `Runtime` (as in `zymi serve`).
pub struct ActionContext<'a> {
    pub project_root: &'a Path,
    pub memory: &'a MemoryStore,
    /// Stream identifier — used to look up the persistent shell session
    /// and to scope memory events.
    pub stream_id: &'a str,
    /// Correlation ID for the current pipeline run — used to link memory
    /// events back to the originating request.
    pub correlation_id: Uuid,
}

/// Executes an approved tool call. Implementors are expected to be
/// side-effect-only — policy/approval/event emission is the orchestrator's
/// job, not the executor's.
#[async_trait]
pub trait ActionExecutor: Send + Sync {
    /// Run a tool by name. `arguments_json` is the raw JSON arguments string
    /// produced by the LLM, identical to what the previous inline path
    /// passed to `execute_builtin_tool`.
    async fn execute(
        &self,
        tool_name: &str,
        arguments_json: &str,
        ctx: &ActionContext<'_>,
    ) -> Result<String, String>;
}

/// Default executor: dispatches to the built-in tool implementations in
/// [`crate::engine::tools`]. Stateless — all per-run state lives in
/// [`ActionContext`].
///
/// **Deprecated**: use [`CatalogActionExecutor`] instead. This type is
/// kept for backwards compatibility with callers that construct an executor
/// without a [`super::ToolCatalog`].
#[derive(Debug, Default, Clone, Copy)]
pub struct BuiltinActionExecutor;

impl BuiltinActionExecutor {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ActionExecutor for BuiltinActionExecutor {
    async fn execute(
        &self,
        tool_name: &str,
        arguments_json: &str,
        ctx: &ActionContext<'_>,
    ) -> Result<String, String> {
        execute_builtin_tool(
            tool_name,
            arguments_json,
            ctx.project_root,
            ctx.memory,
            ctx.stream_id,
            ctx.correlation_id,
        )
        .await
    }
}

/// Catalog-aware executor: dispatches approved tool calls through the
/// [`ToolCatalog`]. Built-in tools delegate to [`execute_builtin_tool`];
/// `execute_shell_command` is routed to the persistent [`ShellSessionPool`];
/// declarative `kind: http` tools are dispatched via reqwest; MCP-backed
/// tools are dispatched via the [`McpRegistry`].
#[derive(Clone)]
pub struct CatalogActionExecutor {
    catalog: Arc<ToolCatalog>,
    shell_pool: Arc<ShellSessionPool>,
    mcp: Option<Arc<McpRegistry>>,
}

impl CatalogActionExecutor {
    pub fn new(catalog: Arc<ToolCatalog>, shell_pool: Arc<ShellSessionPool>) -> Self {
        Self {
            catalog,
            shell_pool,
            mcp: None,
        }
    }

    /// Attach an MCP registry. Calls to tools registered under `mcp__*` ids
    /// will be routed via this registry.
    pub fn with_mcp(mut self, registry: Arc<McpRegistry>) -> Self {
        self.mcp = Some(registry);
        self
    }
}

#[async_trait]
impl ActionExecutor for CatalogActionExecutor {
    async fn execute(
        &self,
        tool_name: &str,
        arguments_json: &str,
        ctx: &ActionContext<'_>,
    ) -> Result<String, String> {
        if tool_name == "execute_shell_command" {
            return self.execute_shell(arguments_json, ctx).await;
        }
        if self.catalog.is_mcp(tool_name) {
            return self.execute_mcp(tool_name, arguments_json).await;
        }
        #[cfg(feature = "python")]
        if self.catalog.is_python(tool_name) {
            return self.execute_python(tool_name, arguments_json).await;
        }
        if self.catalog.is_declarative(tool_name) {
            let config = self.catalog.declarative_config(tool_name)
                .ok_or_else(|| format!("declarative tool '{tool_name}' config missing"))?;
            match &config.implementation {
                ImplementationConfig::Http { .. } => {
                    execute_declarative_http(config, arguments_json).await
                }
                ImplementationConfig::Shell { .. } => {
                    self.execute_declarative_shell(config, arguments_json, ctx)
                        .await
                }
            }
        } else {
            execute_builtin_tool(
                tool_name,
                arguments_json,
                ctx.project_root,
                ctx.memory,
                ctx.stream_id,
                ctx.correlation_id,
            )
            .await
        }
    }
}

impl CatalogActionExecutor {
    /// Route an `mcp__server__tool` call through the [`McpRegistry`].
    ///
    /// Server-reported tool errors (`isError: true`) are returned as `Err` so
    /// the orchestrator records a `ToolCallCompleted { is_error: true }`,
    /// matching how declarative tool failures already behave.
    async fn execute_mcp(&self, tool_name: &str, arguments_json: &str) -> Result<String, String> {
        let (server, tool) = self
            .catalog
            .mcp_route(tool_name)
            .ok_or_else(|| format!("mcp tool '{tool_name}' not registered"))?;
        let registry = self
            .mcp
            .as_ref()
            .ok_or_else(|| format!("mcp tool '{tool_name}' called but no registry configured"))?;

        let arguments: serde_json::Value = if arguments_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(arguments_json)
                .map_err(|e| format!("mcp tool '{tool_name}' invalid arguments json: {e}"))?
        };

        let result = registry
            .call(server, tool, arguments)
            .await
            .map_err(|e| format!("mcp tool '{tool_name}' transport error: {e}"))?;

        let text = render_mcp_content(&result.content);
        if result.is_error {
            Err(text)
        } else {
            Ok(text)
        }
    }
}

/// Flatten the MCP `content` array into a single string for the LLM.
///
/// MCP content blocks are heterogeneous (`text`, `image`, `resource`, …).
/// v1 surfaces the text portions concatenated; non-text blocks are rendered
/// as a JSON debug stub so the LLM at least sees that something arrived.
fn render_mcp_content(content: &[serde_json::Value]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(content.len());
    for block in content {
        let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if kind == "text" {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                parts.push(text.to_string());
                continue;
            }
        }
        parts.push(block.to_string());
    }
    if parts.is_empty() {
        "(no content)".to_string()
    } else {
        parts.join("\n")
    }
}

#[cfg(feature = "python")]
impl CatalogActionExecutor {
    /// Dispatch an auto-discovered Python tool through the embedded
    /// interpreter (ADR-0014 slice 3).
    ///
    /// Acquires the GIL, deserialises `arguments_json` into a Python
    /// dict, calls the registered callable with `**kwargs`, and returns
    /// `str(result)`. Async functions are resolved via the same
    /// `asyncio.run`-with-fallback path the user-facing `ToolRegistry`
    /// uses.
    async fn execute_python(
        &self,
        tool_name: &str,
        arguments_json: &str,
    ) -> Result<String, String> {
        // Parse args outside the GIL closure so a malformed JSON body
        // fails before we cross into Python.
        let args_value: serde_json::Value = if arguments_json.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(arguments_json)
                .map_err(|e| format!("python tool '{tool_name}' invalid arguments json: {e}"))?
        };

        // Hold the catalog as an Arc so the closure can keep it alive
        // and reach back to the entry inside with_gil — that's where
        // we can safely clone the Py<PyAny> handle and call it.
        let catalog = std::sync::Arc::clone(&self.catalog);
        let tool_label = tool_name.to_string();

        // spawn_blocking: pyo3 calls park the tokio worker on the GIL
        // anyway; the blocking pool is the right home for a "may take a
        // while" call. From-the-blocking-pool back-into-tokio is fine
        // because we await the join handle here.
        let result = tokio::task::spawn_blocking(move || -> Result<String, String> {
            pyo3::Python::with_gil(|py| {
                let entry = catalog
                    .python_entry(&tool_label)
                    .ok_or_else(|| format!("python tool '{tool_label}' not registered"))?;
                let kwargs = crate::python::tool::json_value_to_pydict_pub(py, &args_value)
                    .map_err(|e| format!("python tool '{tool_label}' arg conversion: {e}"))?;
                let raw = entry
                    .callable
                    .call_bound(py, (), Some(&kwargs))
                    .map_err(|e| format!("python tool '{tool_label}' raised: {e}"))?;
                let resolved = if entry.is_async {
                    crate::python::tool::run_coroutine_pub(py, &raw)
                        .map_err(|e| format!("python tool '{tool_label}' coroutine: {e}"))?
                } else {
                    raw
                };
                use pyo3::types::PyAnyMethods;
                let s: String = resolved
                    .bind(py)
                    .str()
                    .map_err(|e| format!("python tool '{tool_label}' str(): {e}"))?
                    .to_string();
                Ok(s)
            })
        })
        .await
        .map_err(|e| format!("python tool '{tool_name}' join: {e}"))??;

        Ok(result)
    }
}

impl CatalogActionExecutor {
    /// Route a declarative `kind: shell` tool through the persistent pool.
    ///
    /// Resolves `${args.X}` placeholders in the command template, then
    /// dispatches into the same [`ShellSessionPool`] as the built-in
    /// `execute_shell_command`.
    async fn execute_declarative_shell(
        &self,
        config: &crate::config::tool::ToolConfig,
        arguments_json: &str,
        ctx: &ActionContext<'_>,
    ) -> Result<String, String> {
        let (command_template, timeout_secs) = match &config.implementation {
            ImplementationConfig::Shell {
                command_template,
                timeout_secs,
            } => (command_template.as_str(), timeout_secs.unwrap_or(30)),
            _ => return Err(format!("tool '{}' is not kind: shell", config.name)),
        };

        // Log a warning if requires_approval was explicitly set to false on a
        // shell tool (ADR-0014 §4 unsafe-shell warning).
        if config.requires_approval == Some(false) {
            warn!(
                "tool '{}' is kind=shell with requires_approval=false — \
                 bypasses the execute_shell_command policy gate; \
                 verify this is intentional",
                config.name
            );
        }

        let args = parse_args_for_interpolation(arguments_json);
        let resolved_command = resolve_args(command_template, &args);

        let output = self
            .shell_pool
            .run_command(
                ctx.stream_id,
                &resolved_command,
                Duration::from_secs(timeout_secs),
                ctx.project_root,
            )
            .await
            .map_err(|e| format!("shell session error: {e}"))?;

        if output.timed_out {
            Err(format!(
                "command timed out after {timeout_secs}s\nstdout: {}\nstderr: {}",
                truncate_output(&output.stdout, 2000),
                truncate_output(&output.stderr, 2000),
            ))
        } else if output.exit_code == 0 {
            Ok(if output.stdout.is_empty() {
                "(no output)".to_string()
            } else {
                truncate_output(&output.stdout, 4000)
            })
        } else {
            Err(format!(
                "exit code {}\nstdout: {}\nstderr: {}",
                output.exit_code,
                truncate_output(&output.stdout, 2000),
                truncate_output(&output.stderr, 2000),
            ))
        }
    }

    /// Route `execute_shell_command` through the persistent shell pool.
    async fn execute_shell(
        &self,
        arguments_json: &str,
        ctx: &ActionContext<'_>,
    ) -> Result<String, String> {
        let args: serde_json::Value = serde_json::from_str(arguments_json)
            .map_err(|e| format!("invalid tool arguments JSON: {e}"))?;

        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or("missing 'command' argument")?;
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(30);

        let output = self
            .shell_pool
            .run_command(
                ctx.stream_id,
                command,
                Duration::from_secs(timeout_secs),
                ctx.project_root,
            )
            .await
            .map_err(|e| format!("shell session error: {e}"))?;

        if output.timed_out {
            Err(format!(
                "command timed out after {timeout_secs}s\nstdout: {}\nstderr: {}",
                truncate_output(&output.stdout, 2000),
                truncate_output(&output.stderr, 2000),
            ))
        } else if output.exit_code == 0 {
            Ok(if output.stdout.is_empty() {
                "(no output)".to_string()
            } else {
                truncate_output(&output.stdout, 4000)
            })
        } else {
            Err(format!(
                "exit code {}\nstdout: {}\nstderr: {}",
                output.exit_code,
                truncate_output(&output.stdout, 2000),
                truncate_output(&output.stderr, 2000),
            ))
        }
    }
}

/// Execute a declarative `kind: http` tool.
async fn execute_declarative_http(
    config: &crate::config::tool::ToolConfig,
    arguments_json: &str,
) -> Result<String, String> {
    let args: HashMap<String, String> = parse_args_for_interpolation(arguments_json);

    match &config.implementation {
        ImplementationConfig::Http {
            method,
            url,
            headers,
            body_template,
        } => {
            let resolved_url = resolve_args(url, &args);

            let client = reqwest::Client::new();
            let mut builder = match method {
                HttpMethod::Get => client.get(&resolved_url),
                HttpMethod::Post => client.post(&resolved_url),
                HttpMethod::Put => client.put(&resolved_url),
                HttpMethod::Patch => client.patch(&resolved_url),
                HttpMethod::Delete => client.delete(&resolved_url),
            };

            for (key, value) in headers {
                builder = builder.header(key, resolve_args(value, &args));
            }

            if let Some(template) = body_template {
                let body = resolve_args(template, &args);
                builder = builder.body(body);
            }

            let resp = builder
                .send()
                .await
                .map_err(|e| format!("HTTP request failed: {e}"))?;

            let status = resp.status();
            let body = resp
                .text()
                .await
                .map_err(|e| format!("failed to read response body: {e}"))?;

            if status.is_success() {
                Ok(truncate_output(&body, 8000))
            } else {
                Err(format!("HTTP {status}: {}", truncate_output(&body, 2000)))
            }
        }
        // Shell tools are dispatched by CatalogActionExecutor before reaching
        // this function; if we get here it's a bug.
        ImplementationConfig::Shell { .. } => {
            Err("kind: shell tools should be dispatched via execute_declarative_shell".into())
        }
    }
}

/// Parse LLM arguments JSON into a flat string map for `${args.X}` substitution.
/// Nested objects/arrays are serialized as JSON strings.
pub(crate) fn parse_args_for_interpolation(arguments_json: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(serde_json::Value::Object(obj)) = serde_json::from_str(arguments_json) {
        for (key, value) in obj {
            let s = match &value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            map.insert(key, s);
        }
    }
    map
}

/// Resolve `${args.X}` placeholders in a template string.
pub(crate) fn resolve_args(template: &str, args: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in args {
        result = result.replace(&format!("${{args.{key}}}"), value);
    }
    result
}

fn truncate_output(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max_chars);
        format!("{}...\n[truncated at {max_chars} chars]", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tools::new_memory_store;
    use crate::events::bus::EventBus;
    use crate::events::store::{open_store, StoreBackend};
    use std::path::PathBuf;

    fn test_memory_store() -> crate::engine::tools::MemoryStore {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_events.db");
        let store = open_store(StoreBackend::Sqlite { path: db_path }).unwrap();
        let bus = Arc::new(EventBus::new(store));
        std::mem::forget(dir);
        new_memory_store(bus)
    }

    #[tokio::test]
    async fn builtin_executor_runs_write_memory() {
        let memory = test_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
            stream_id: "test",
            correlation_id: Uuid::new_v4(),
        };
        let executor = BuiltinActionExecutor::new();
        let result = executor
            .execute("write_memory", r#"{"key":"k","content":"v"}"#, &ctx)
            .await
            .unwrap();
        assert!(result.contains("k"));
        // Verify via read_memory tool instead of direct HashMap access
        let read_result = executor
            .execute("read_memory", r#"{"key":"k"}"#, &ctx)
            .await
            .unwrap();
        assert_eq!(read_result, "v");
    }

    #[tokio::test]
    async fn builtin_executor_unknown_tool_errors() {
        let memory = test_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
            stream_id: "test",
            correlation_id: Uuid::new_v4(),
        };
        let executor = BuiltinActionExecutor::new();
        let err = executor
            .execute("nope", "{}", &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("unknown built-in tool"));
    }

    #[tokio::test]
    async fn catalog_executor_dispatches_builtin() {
        let catalog = Arc::new(ToolCatalog::builtin_only());
        let pool = Arc::new(ShellSessionPool::new(Duration::from_secs(60)));
        let memory = test_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
            stream_id: "test",
            correlation_id: Uuid::new_v4(),
        };
        let executor = CatalogActionExecutor::new(catalog, pool);
        let result = executor
            .execute("write_memory", r#"{"key":"k2","content":"v2"}"#, &ctx)
            .await
            .unwrap();
        assert!(result.contains("k2"));
    }

    #[test]
    fn resolve_args_basic() {
        let mut args = HashMap::new();
        args.insert("channel".into(), "C123".into());
        args.insert("text".into(), "hello world".into());

        let template = r#"{"channel": "${args.channel}", "text": "${args.text}"}"#;
        let result = resolve_args(template, &args);
        let expected = r#"{"channel": "C123", "text": "hello world"}"#;
        assert_eq!(result, expected);
    }

    #[test]
    fn resolve_args_no_placeholders() {
        let args = HashMap::new();
        let template = "https://example.com/api";
        assert_eq!(resolve_args(template, &args), template);
    }

    #[test]
    fn parse_args_flat_object() {
        let args = parse_args_for_interpolation(r#"{"name":"test","count":42}"#);
        assert_eq!(args.get("name").unwrap(), "test");
        assert_eq!(args.get("count").unwrap(), "42");
    }

    #[test]
    fn parse_args_invalid_json() {
        let args = parse_args_for_interpolation("not json");
        assert!(args.is_empty());
    }

    // ── MCP dispatch ──────────────────────────────────────────────────────
    //
    // Spawns a duplex-stream "fake MCP server" task and wires it into a
    // McpServerConnection without ever launching a subprocess. The fake
    // answers `initialize`, swallows `notifications/initialized`, and replies
    // to `tools/call` according to a per-test script.

    use crate::mcp::{McpRegistry, McpServerConnection, Transport};
    use serde_json::Value;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn spawn_fake_mcp(
        responder: impl Fn(Value) -> Value + Send + Sync + 'static,
    ) -> (Arc<McpServerConnection>, tokio::task::JoinHandle<()>) {
        let (client_to_server_w, client_to_server_r) = duplex(8192);
        let (server_to_client_w, server_to_client_r) = duplex(8192);

        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(client_to_server_r);
            let mut writer = server_to_client_w;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let req: Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if req.get("id").is_none() {
                    // notification — drop it
                    continue;
                }
                let id = req["id"].clone();
                let method = req["method"].as_str().unwrap_or("").to_string();
                let response = if method == "initialize" {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "fake", "version": "0"}
                        }
                    })
                } else if method == "tools/call" {
                    let tool_result = responder(req["params"].clone());
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": tool_result})
                } else {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": "method not found"}
                    })
                };
                let mut data = serde_json::to_vec(&response).unwrap();
                data.push(b'\n');
                if writer.write_all(&data).await.is_err() {
                    return;
                }
                let _ = writer.flush().await;
            }
        });

        let transport = Transport::new(BufReader::new(server_to_client_r), client_to_server_w);
        let conn = McpServerConnection::from_transport_for_test(
            "fake".into(),
            transport,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .await
        .expect("handshake should succeed");
        (Arc::new(conn), server)
    }

    fn mcp_text_result(text: &str, is_error: bool) -> Value {
        serde_json::json!({
            "content": [{"type": "text", "text": text}],
            "isError": is_error
        })
    }

    #[tokio::test]
    async fn catalog_executor_dispatches_mcp_tool() {
        let (conn, _server) = spawn_fake_mcp(|params| {
            assert_eq!(params["name"], "create_issue");
            assert_eq!(params["arguments"]["title"], "bug");
            mcp_text_result("issue #42 created", false)
        })
        .await;

        let mut registry = McpRegistry::new();
        registry.insert("github", conn);

        let mut catalog = ToolCatalog::builtin_only();
        catalog
            .add_mcp_server(
                "github",
                &[crate::mcp::McpTool {
                    name: "create_issue".into(),
                    description: Some("create an issue".into()),
                    input_schema: serde_json::json!({"type": "object"}),
                }],
                false,
            )
            .unwrap();

        let pool = Arc::new(ShellSessionPool::new(Duration::from_secs(60)));
        let executor = CatalogActionExecutor::new(Arc::new(catalog), pool)
            .with_mcp(Arc::new(registry));

        let memory = test_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
            stream_id: "test",
            correlation_id: Uuid::new_v4(),
        };

        let result = executor
            .execute("mcp__github__create_issue", r#"{"title":"bug"}"#, &ctx)
            .await
            .unwrap();
        assert_eq!(result, "issue #42 created");
    }

    #[tokio::test]
    async fn mcp_server_reported_tool_error_surfaces_as_err() {
        let (conn, _server) =
            spawn_fake_mcp(|_| mcp_text_result("rate limited", true)).await;

        let mut registry = McpRegistry::new();
        registry.insert("github", conn);

        let mut catalog = ToolCatalog::builtin_only();
        catalog
            .add_mcp_server(
                "github",
                &[crate::mcp::McpTool {
                    name: "create_issue".into(),
                    description: None,
                    input_schema: serde_json::json!({}),
                }],
                false,
            )
            .unwrap();

        let pool = Arc::new(ShellSessionPool::new(Duration::from_secs(60)));
        let executor = CatalogActionExecutor::new(Arc::new(catalog), pool)
            .with_mcp(Arc::new(registry));

        let memory = test_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
            stream_id: "test",
            correlation_id: Uuid::new_v4(),
        };

        let err = executor
            .execute("mcp__github__create_issue", "{}", &ctx)
            .await
            .unwrap_err();
        assert_eq!(err, "rate limited");
    }

    #[tokio::test]
    async fn mcp_call_without_registry_is_explicit_error() {
        let mut catalog = ToolCatalog::builtin_only();
        catalog
            .add_mcp_server(
                "github",
                &[crate::mcp::McpTool {
                    name: "issue".into(),
                    description: None,
                    input_schema: serde_json::json!({}),
                }],
                false,
            )
            .unwrap();

        let pool = Arc::new(ShellSessionPool::new(Duration::from_secs(60)));
        let executor = CatalogActionExecutor::new(Arc::new(catalog), pool);

        let memory = test_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
            stream_id: "test",
            correlation_id: Uuid::new_v4(),
        };

        let err = executor
            .execute("mcp__github__issue", "{}", &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("no registry"), "got: {err}");
    }

    #[test]
    fn render_mcp_content_concatenates_text_blocks() {
        let blocks = vec![
            serde_json::json!({"type": "text", "text": "first"}),
            serde_json::json!({"type": "text", "text": "second"}),
        ];
        assert_eq!(super::render_mcp_content(&blocks), "first\nsecond");
    }

    #[test]
    fn render_mcp_content_falls_back_for_non_text_blocks() {
        let blocks = vec![
            serde_json::json!({"type": "text", "text": "hi"}),
            serde_json::json!({"type": "image", "data": "..."}),
        ];
        let rendered = super::render_mcp_content(&blocks);
        assert!(rendered.starts_with("hi\n"), "got: {rendered}");
        assert!(rendered.contains("image"));
    }

    #[test]
    fn render_mcp_content_empty_returns_placeholder() {
        assert_eq!(super::render_mcp_content(&[]), "(no content)");
    }
}
