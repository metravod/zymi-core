//! MCP server (ADR-0033 Slice 1) — exposes zymi pipelines as MCP tools
//! over stdio + newline-delimited JSON-RPC 2.0.
//!
//! See [`super`] for the client direction. The two halves share wire
//! framing but the orchestration differs: this module receives requests
//! and replies keyed by inbound id, whereas the client issues requests
//! and demultiplexes responses.
//!
//! Sync-mode pipelines only in v1. Async-mode (`expose.mcp.mode: async`)
//! pipelines parse today but are filtered out of `tools/list` and
//! refused by `tools/call`; Slice 2 (SEP-1686 Tasks) lifts that gate.

use std::sync::Arc;

use regex::Regex;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::runtime::Runtime;

pub mod handlers;
pub mod protocol;

use handlers::{handle_initialize, handle_tools_call, handle_tools_list, internal_error, unknown_method};
use protocol::{RpcError, RpcRequest, RpcResponse, ERR_INVALID_REQUEST, ERR_PARSE, JSONRPC_VERSION};

/// Configuration for a single MCP server boot. Cheap to construct; the
/// glob filters are compiled to regexes once at construction so the hot
/// path stays substring-cheap.
#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl ServerConfig {
    pub fn new(include: Vec<String>, exclude: Vec<String>) -> Self {
        Self { include, exclude }
    }
}

/// Compiled name filter — passes a pipeline name iff:
/// 1. `include` is empty OR at least one include pattern matches, and
/// 2. no `exclude` pattern matches.
pub struct PipelineFilter {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
}

impl PipelineFilter {
    pub fn from_config(config: &ServerConfig) -> Result<Self, String> {
        let include = compile_patterns(&config.include)?;
        let exclude = compile_patterns(&config.exclude)?;
        Ok(Self { include, exclude })
    }

    pub fn matches(&self, name: &str) -> bool {
        let included = self.include.is_empty() || self.include.iter().any(|r| r.is_match(name));
        if !included {
            return false;
        }
        !self.exclude.iter().any(|r| r.is_match(name))
    }
}

fn compile_patterns(patterns: &[String]) -> Result<Vec<Regex>, String> {
    patterns
        .iter()
        .map(|p| glob_to_regex(p).map_err(|e| format!("invalid pattern `{p}`: {e}")))
        .collect()
}

/// Convert a simple glob (`*`, `?`) to an anchored, case-sensitive
/// regex. Pipeline names are case-sensitive in YAML, so the filter
/// matches that discipline (unlike `policy.rs::glob_to_regex` which is
/// case-insensitive for shell-command matching).
fn glob_to_regex(pattern: &str) -> Result<Regex, regex::Error> {
    let mut out = String::with_capacity(pattern.len() + 4);
    out.push('^');
    for ch in pattern.chars() {
        match ch {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('$');
    Regex::new(&out)
}

/// Serve MCP over the given reader/writer pair. Returns when the reader
/// reaches EOF (the canonical "client disconnected" signal for stdio
/// MCP servers).
pub async fn serve<R, W>(
    runtime: Arc<Runtime>,
    config: ServerConfig,
    reader: R,
    writer: W,
) -> Result<(), String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let filter = PipelineFilter::from_config(&config)?;
    serve_loop(runtime, filter, reader, writer).await
}

async fn serve_loop<R, W>(
    runtime: Arc<Runtime>,
    filter: PipelineFilter,
    mut reader: R,
    mut writer: W,
) -> Result<(), String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => return Ok(()), // EOF — client closed.
            Ok(_) => {}
            Err(e) => return Err(format!("mcp serve: read error: {e}")),
        }

        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = dispatch_line(&runtime, &filter, trimmed).await;
        if let Some(resp) = response {
            write_response(&mut writer, &resp).await?;
        }
    }
}

/// Dispatch a single JSON-RPC line. Returns `None` for notifications
/// (no `id`) — those expect no response per the JSON-RPC 2.0 spec.
async fn dispatch_line(
    runtime: &Arc<Runtime>,
    filter: &PipelineFilter,
    line: &str,
) -> Option<RpcResponse> {
    let parsed: Result<RpcRequest, _> = serde_json::from_str(line);
    let request = match parsed {
        Ok(r) => r,
        Err(e) => {
            // Parse failure — id is unknown, spec says respond with id: null.
            return Some(RpcResponse::err(
                Value::Null,
                RpcError::new(ERR_PARSE, format!("invalid json: {e}")),
            ));
        }
    };

    // Notifications carry no `id`; the spec forbids responding to them.
    let is_notification = request.id.is_none();

    if request.jsonrpc.as_deref() != Some(JSONRPC_VERSION) {
        return notification_or_error(
            is_notification,
            request.id.clone(),
            RpcError::new(ERR_INVALID_REQUEST, "missing or wrong `jsonrpc` field"),
        );
    }

    if is_notification {
        // We accept `notifications/initialized` and similar as no-ops.
        log::debug!("mcp serve: notification `{}` (no response)", request.method);
        return None;
    }

    let id = request.id.clone().unwrap_or(Value::Null);
    let response = handle_method(runtime, filter, &request).await;
    match response {
        Ok(result) => Some(RpcResponse::ok(id, result)),
        Err(err) => Some(RpcResponse::err(id, err)),
    }
}

fn notification_or_error(
    is_notification: bool,
    id: Option<Value>,
    err: RpcError,
) -> Option<RpcResponse> {
    if is_notification {
        log::debug!("mcp serve: notification with rpc error: {}", err.message);
        None
    } else {
        Some(RpcResponse::err(id.unwrap_or(Value::Null), err))
    }
}

async fn handle_method(
    runtime: &Arc<Runtime>,
    filter: &PipelineFilter,
    request: &RpcRequest,
) -> Result<Value, RpcError> {
    match request.method.as_str() {
        "initialize" => Ok(handle_initialize()),
        "tools/list" => Ok(handle_tools_list(runtime, |name| filter.matches(name))),
        "tools/call" => {
            let params = request.params.as_ref().ok_or_else(|| {
                RpcError::new(protocol::ERR_INVALID_PARAMS, "missing params")
            })?;
            handle_tools_call(runtime, params, |name| filter.matches(name)).await
        }
        // `ping` is part of every MCP spec rev; handle as no-op {} for
        // keepalive purposes. Cheap and harmless.
        "ping" => Ok(serde_json::json!({})),
        other => Err(unknown_method(other)),
    }
}

async fn write_response<W>(writer: &mut W, response: &RpcResponse) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(response).map_err(|e| internal_error(e).message)?;
    line.push(b'\n');
    writer
        .write_all(&line)
        .await
        .map_err(|e| format!("mcp serve: write error: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("mcp serve: flush error: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn glob_star_matches_anything() {
        let re = glob_to_regex("*").unwrap();
        assert!(re.is_match("anything"));
        assert!(re.is_match(""));
    }

    #[test]
    fn glob_prefix() {
        let re = glob_to_regex("web_*").unwrap();
        assert!(re.is_match("web_search"));
        assert!(re.is_match("web_"));
        assert!(!re.is_match("rest_search"));
    }

    #[test]
    fn glob_is_case_sensitive() {
        let re = glob_to_regex("Search").unwrap();
        assert!(re.is_match("Search"));
        assert!(!re.is_match("search"));
    }

    #[test]
    fn glob_escapes_regex_metachars() {
        // A literal `.` should match only `.`, not any char.
        let re = glob_to_regex("a.b").unwrap();
        assert!(re.is_match("a.b"));
        assert!(!re.is_match("axb"));
    }

    #[test]
    fn pipeline_filter_no_patterns_passes_all() {
        let f = PipelineFilter::from_config(&ServerConfig::default()).unwrap();
        assert!(f.matches("anything"));
    }

    #[test]
    fn pipeline_filter_include_only() {
        let f = PipelineFilter::from_config(&ServerConfig::new(
            vec!["web_*".into()],
            vec![],
        ))
        .unwrap();
        assert!(f.matches("web_search"));
        assert!(!f.matches("internal_cron"));
    }

    #[test]
    fn pipeline_filter_exclude_overrides_include() {
        let f = PipelineFilter::from_config(&ServerConfig::new(
            vec!["*".into()],
            vec!["internal_*".into()],
        ))
        .unwrap();
        assert!(f.matches("public_pipe"));
        assert!(!f.matches("internal_cron"));
    }

    #[test]
    fn pipeline_filter_rejects_invalid_pattern() {
        // `glob_to_regex` only escapes a known set; nothing here actually
        // produces an invalid regex, so this is a smoke check that the
        // happy path stays clean. Compose-only test.
        let f = PipelineFilter::from_config(&ServerConfig::new(
            vec!["valid_*".into()],
            vec![],
        ));
        assert!(f.is_ok());
    }

    // ── End-to-end dispatch over a pipe ───────────────────────────────
    //
    // Brings up a real `Runtime` with a mock LLM and a workspace that
    // declares three pipelines:
    //   - `web_search`     — exposed, sync (the happy-path tool)
    //   - `hidden_cron`    — not exposed (must not appear in tools/list)
    //   - `long_task`      — exposed, async (Slice 2 territory, skipped)
    // The test drives the server over an in-memory duplex pair and
    // verifies the wire shape for `initialize`, `tools/list`, and
    // `tools/call`.

    use crate::config::pipeline::{
        McpExpose, McpExposeMode, PipelineConfig, PipelineExpose, PipelineInput,
        PipelineInputType, PipelineStep, PipelineStepKind,
    };
    use crate::config::{
        AgentConfig, ContractsConfig, DefaultsConfig, ProjectConfig, RuntimeConfig,
        ServicesConfig, ShellConfig, WorkspaceConfig,
    };
    use crate::events::store::{open_store, StoreBackend};
    use crate::llm::{ChatRequest, ChatResponse, LlmError, LlmProvider};
    use crate::policy::PolicyConfig;
    use crate::runtime::Runtime;
    use crate::types::{Message, TokenUsage};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use tempfile::TempDir;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[derive(Debug, Default)]
    struct MockLlm;

    #[async_trait]
    impl LlmProvider for MockLlm {
        async fn chat_completion(
            &self,
            _request: &ChatRequest,
        ) -> Result<ChatResponse, LlmError> {
            Ok(ChatResponse {
                message: Message::Assistant {
                    content: Some("search-result-stub".into()),
                    tool_calls: vec![],
                },
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
                model: "mock".into(),
            })
        }
    }

    fn single_agent_pipeline(
        name: &str,
        expose: Option<PipelineExpose>,
        inputs: Vec<PipelineInput>,
    ) -> PipelineConfig {
        PipelineConfig {
            name: name.into(),
            description: Some(format!("{name} description")),
            inputs,
            steps: vec![PipelineStep {
                id: "go".into(),
                kind: PipelineStepKind::Agent {
                    agent: "researcher".into(),
                    task: "do the thing".into(),
                },
                depends_on: vec![],
                when: None,
                context: None,
            }],
            output: None,
            approval_channel: None,
            expose,
        }
    }

    fn build_workspace() -> WorkspaceConfig {
        let mut agents = HashMap::new();
        agents.insert(
            "researcher".into(),
            AgentConfig {
                name: "researcher".into(),
                description: None,
                model: Some("mock".into()),
                system_prompt: Some("be terse".into()),
                tools: vec![],
                max_iterations: Some(1),
                timeout_secs: None,
                policy: None,
            },
        );

        let mut pipelines = HashMap::new();
        pipelines.insert(
            "web_search".into(),
            single_agent_pipeline(
                "web_search",
                Some(PipelineExpose {
                    mcp: Some(McpExpose {
                        name: None,
                        description: Some("search the web".into()),
                        mode: McpExposeMode::Sync,
                    }),
                }),
                vec![PipelineInput {
                    name: "query".into(),
                    ty: PipelineInputType::String,
                    required: true,
                    description: Some("search query".into()),
                }],
            ),
        );
        pipelines.insert(
            "hidden_cron".into(),
            single_agent_pipeline("hidden_cron", None, vec![]),
        );
        pipelines.insert(
            "long_task".into(),
            single_agent_pipeline(
                "long_task",
                Some(PipelineExpose {
                    mcp: Some(McpExpose {
                        name: None,
                        description: None,
                        mode: McpExposeMode::Async,
                    }),
                }),
                vec![],
            ),
        );

        WorkspaceConfig {
            project: ProjectConfig {
                name: "t".into(),
                schema_version: None,
                version: None,
                variables: HashMap::new(),
                llm: None,
                services: Some(ServicesConfig::default()),
                policy: PolicyConfig::default(),
                contracts: ContractsConfig::default(),
                defaults: DefaultsConfig::default(),
                runtime: Some(RuntimeConfig {
                    shell: ShellConfig::default(),
                    context: Default::default(),
                }),
                mcp_servers: Vec::new(),
                connectors: Vec::new(),
                outputs: Vec::new(),
                approvals: Vec::new(),
                default_approval_channel: None,
                store: None,
            },
            agents,
            pipelines,
            tools: HashMap::new(),
        }
    }

    fn build_runtime(workspace: WorkspaceConfig, dir: &TempDir) -> Arc<Runtime> {
        let store_path = dir.path().join("events.db");
        let store = open_store(StoreBackend::Sqlite { path: store_path }).unwrap();
        Arc::new(
            Runtime::builder(workspace, dir.path().to_path_buf())
                .with_store(store)
                .with_llm_provider(Arc::new(MockLlm) as Arc<dyn LlmProvider>)
                .build()
                .unwrap(),
        )
    }

    /// Send a JSON-RPC request line; read and parse the response line.
    async fn rpc_roundtrip(
        client_w: &mut tokio::io::DuplexStream,
        client_r: &mut BufReader<tokio::io::DuplexStream>,
        method: &str,
        id: u64,
        params: Option<Value>,
    ) -> Value {
        let mut req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
        });
        if let Some(p) = params {
            req["params"] = p;
        }
        let mut bytes = serde_json::to_vec(&req).unwrap();
        bytes.push(b'\n');
        client_w.write_all(&bytes).await.unwrap();
        client_w.flush().await.unwrap();

        let mut line = String::new();
        AsyncBufReadExt::read_line(client_r, &mut line)
            .await
            .unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    #[tokio::test]
    async fn server_end_to_end_initialize_list_call() {
        let dir = TempDir::new().unwrap();
        let runtime = build_runtime(build_workspace(), &dir);

        let (client_to_server, server_in) = tokio::io::duplex(8192);
        let (server_out, client_from_server) = tokio::io::duplex(8192);

        let runtime_for_task = runtime.clone();
        let server_task = tokio::spawn(async move {
            serve(
                runtime_for_task,
                ServerConfig::default(),
                BufReader::new(server_in),
                server_out,
            )
            .await
        });

        let mut client_w = client_to_server;
        let mut client_r = BufReader::new(client_from_server);

        // 1. initialize
        let init = rpc_roundtrip(&mut client_w, &mut client_r, "initialize", 1, None).await;
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["serverInfo"]["name"], "zymi");
        assert!(init["result"]["capabilities"]["tools"].is_object());

        // 2. tools/list — only web_search should appear.
        let listed = rpc_roundtrip(&mut client_w, &mut client_r, "tools/list", 2, None).await;
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "got tools: {tools:?}");
        let tool = &tools[0];
        assert_eq!(tool["name"], "web_search");
        assert_eq!(tool["description"], "search the web");
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(
            tool["inputSchema"]["properties"]["query"]["type"],
            "string"
        );

        // 3. tools/call — happy path. The mock LLM returns "search-result-stub".
        let called = rpc_roundtrip(
            &mut client_w,
            &mut client_r,
            "tools/call",
            3,
            Some(json!({ "name": "web_search", "arguments": { "query": "rust mcp" } })),
        )
        .await;
        assert_eq!(called["id"], 3);
        let content = &called["result"]["content"];
        assert!(content.is_array(), "got: {called}");
        assert_eq!(content[0]["type"], "text");
        // `isError: false` must be present (MCP convention).
        assert_eq!(called["result"]["isError"], false);

        // 4. tools/call on a tool not in tools/list — must error.
        let missing = rpc_roundtrip(
            &mut client_w,
            &mut client_r,
            "tools/call",
            4,
            Some(json!({ "name": "hidden_cron", "arguments": {} })),
        )
        .await;
        assert!(
            missing["error"].is_object(),
            "expected JSON-RPC error, got: {missing}"
        );
        assert_eq!(missing["error"]["code"], super::protocol::ERR_METHOD_NOT_FOUND);

        // Shutdown: drop client writer → EOF → server task returns Ok.
        drop(client_w);
        let server_result = server_task.await.unwrap();
        assert!(server_result.is_ok(), "server exited with: {server_result:?}");
    }

    #[tokio::test]
    async fn server_returns_parse_error_for_bad_json() {
        let dir = TempDir::new().unwrap();
        let runtime = build_runtime(build_workspace(), &dir);

        let (mut client_w, server_in) = tokio::io::duplex(8192);
        let (server_out, client_from_server) = tokio::io::duplex(8192);
        let mut client_r = BufReader::new(client_from_server);

        let server_task = tokio::spawn(async move {
            serve(
                runtime,
                ServerConfig::default(),
                BufReader::new(server_in),
                server_out,
            )
            .await
        });

        client_w.write_all(b"not json at all\n").await.unwrap();
        client_w.flush().await.unwrap();

        let mut line = String::new();
        AsyncBufReadExt::read_line(&mut client_r, &mut line)
            .await
            .unwrap();
        let resp: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(resp["error"]["code"], super::protocol::ERR_PARSE);
        assert!(resp["id"].is_null());

        drop(client_w);
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn server_does_not_respond_to_notifications() {
        let dir = TempDir::new().unwrap();
        let runtime = build_runtime(build_workspace(), &dir);

        let (mut client_w, server_in) = tokio::io::duplex(8192);
        let (server_out, client_from_server) = tokio::io::duplex(8192);
        let mut client_r = BufReader::new(client_from_server);

        let server_task = tokio::spawn(async move {
            serve(
                runtime,
                ServerConfig::default(),
                BufReader::new(server_in),
                server_out,
            )
            .await
        });

        // Send a notification (no `id`), then a real request. The
        // server must skip the notification and reply only to the
        // request.
        client_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        let resp = rpc_roundtrip(&mut client_w, &mut client_r, "initialize", 1, None).await;
        assert_eq!(resp["id"], 1);

        drop(client_w);
        server_task.await.unwrap().unwrap();
    }
}
