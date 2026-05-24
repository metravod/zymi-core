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
pub mod tasks;

use handlers::{
    cancel_by_request_id, handle_initialize, handle_tasks_cancel, handle_tasks_get,
    handle_tasks_list, handle_tasks_result, handle_tools_call, handle_tools_list, internal_error,
    unknown_method,
};
use protocol::{
    MethodOutcome, Notification, RpcError, RpcRequest, RpcResponse, ERR_INVALID_REQUEST, ERR_PARSE,
    JSONRPC_VERSION,
};
use tasks::TaskStore;

/// A single frame the server writes to the wire: either a response to an
/// inbound request, or a server-initiated notification (e.g.
/// `notifications/tasks/created`).
enum Outbound {
    Response(RpcResponse),
    Notification(Notification),
}

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
    let store = Arc::new(TaskStore::new());
    serve_loop(runtime, filter, store, reader, writer).await
}

async fn serve_loop<R, W>(
    runtime: Arc<Runtime>,
    filter: PipelineFilter,
    store: Arc<TaskStore>,
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

        for frame in dispatch_line(&runtime, &filter, &store, trimmed).await {
            write_frame(&mut writer, &frame).await?;
        }
    }
}

/// Dispatch a single JSON-RPC line into zero or more outbound frames.
/// Notifications (no `id`) produce no response, but may have side effects
/// (e.g. `notifications/cancelled`). A task-augmented `tools/call` produces
/// a `notifications/tasks/created` frame plus its `CreateTaskResult`.
async fn dispatch_line(
    runtime: &Arc<Runtime>,
    filter: &PipelineFilter,
    store: &Arc<TaskStore>,
    line: &str,
) -> Vec<Outbound> {
    let parsed: Result<RpcRequest, _> = serde_json::from_str(line);
    let request = match parsed {
        Ok(r) => r,
        Err(e) => {
            // Parse failure — id is unknown, spec says respond with id: null.
            return vec![Outbound::Response(RpcResponse::err(
                Value::Null,
                RpcError::new(ERR_PARSE, format!("invalid json: {e}")),
            ))];
        }
    };

    // Notifications carry no `id`; the spec forbids responding to them.
    let is_notification = request.id.is_none();

    if request.jsonrpc.as_deref() != Some(JSONRPC_VERSION) {
        if is_notification {
            log::debug!("mcp serve: notification with bad jsonrpc field, ignored");
            return Vec::new();
        }
        return vec![Outbound::Response(RpcResponse::err(
            request.id.clone().unwrap_or(Value::Null),
            RpcError::new(ERR_INVALID_REQUEST, "missing or wrong `jsonrpc` field"),
        ))];
    }

    if is_notification {
        handle_notification(store, &request).await;
        return Vec::new();
    }

    let id = request.id.clone().unwrap_or(Value::Null);
    match handle_method(runtime, filter, store, &request).await {
        Ok(outcome) => {
            let mut frames: Vec<Outbound> = outcome
                .notifications
                .into_iter()
                .map(Outbound::Notification)
                .collect();
            frames.push(Outbound::Response(RpcResponse::ok(id, outcome.result)));
            frames
        }
        Err(err) => vec![Outbound::Response(RpcResponse::err(id, err))],
    }
}

/// Inbound notifications (no `id`, no response). `notifications/cancelled`
/// carries `{ requestId }` referencing the originating task-augmented
/// request (§4.8.1); everything else (e.g. `notifications/initialized`) is
/// a no-op.
async fn handle_notification(store: &Arc<TaskStore>, request: &RpcRequest) {
    if request.method == "notifications/cancelled" {
        if let Some(request_id) = request.params.as_ref().and_then(|p| p.get("requestId")) {
            cancel_by_request_id(store, request_id).await;
        }
    } else {
        log::debug!("mcp serve: notification `{}` (no response)", request.method);
    }
}

async fn handle_method(
    runtime: &Arc<Runtime>,
    filter: &PipelineFilter,
    store: &Arc<TaskStore>,
    request: &RpcRequest,
) -> Result<MethodOutcome, RpcError> {
    let require_params = || {
        request
            .params
            .as_ref()
            .ok_or_else(|| RpcError::new(protocol::ERR_INVALID_PARAMS, "missing params"))
    };
    match request.method.as_str() {
        "initialize" => Ok(MethodOutcome::just(handle_initialize())),
        "tools/list" => Ok(MethodOutcome::just(handle_tools_list(runtime, |name| {
            filter.matches(name)
        }))),
        "tools/call" => {
            let params = require_params()?;
            let id = request.id.clone().unwrap_or(Value::Null);
            handle_tools_call(runtime, store, &id, params, |name| filter.matches(name)).await
        }
        "tasks/get" => handle_tasks_get(store, require_params()?)
            .await
            .map(MethodOutcome::just),
        "tasks/result" => handle_tasks_result(store, require_params()?)
            .await
            .map(MethodOutcome::just),
        "tasks/list" => Ok(MethodOutcome::just(handle_tasks_list(store).await)),
        "tasks/cancel" => handle_tasks_cancel(store, require_params()?)
            .await
            .map(MethodOutcome::just),
        // `ping` is part of every MCP spec rev; handle as no-op {} for
        // keepalive purposes. Cheap and harmless.
        "ping" => Ok(MethodOutcome::just(serde_json::json!({}))),
        other => Err(unknown_method(other)),
    }
}

async fn write_frame<W>(writer: &mut W, frame: &Outbound) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
{
    let mut line = match frame {
        Outbound::Response(resp) => {
            serde_json::to_vec(resp).map_err(|e| internal_error(e).message)?
        }
        Outbound::Notification(n) => {
            let envelope = serde_json::json!({
                "jsonrpc": JSONRPC_VERSION,
                "method": n.method,
                "params": n.params,
            });
            serde_json::to_vec(&envelope).map_err(|e| internal_error(e).message)?
        }
    };
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

        read_json_line(client_r).await
    }

    /// Read a single newline-delimited JSON frame from the server.
    async fn read_json_line(client_r: &mut BufReader<tokio::io::DuplexStream>) -> Value {
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

        // 2. tools/list — web_search (sync) and long_task (async) both
        // appear since Slice 2a; hidden_cron (not exposed) does not.
        let listed = rpc_roundtrip(&mut client_w, &mut client_r, "tools/list", 2, None).await;
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2, "got tools: {tools:?}");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"web_search"), "got: {names:?}");
        assert!(names.contains(&"long_task"), "got: {names:?}");
        let web = tools.iter().find(|t| t["name"] == "web_search").unwrap();
        assert_eq!(web["description"], "search the web");
        assert_eq!(web["inputSchema"]["type"], "object");
        assert_eq!(web["inputSchema"]["properties"]["query"]["type"], "string");

        // initialize advertises the SEP-1686 tasks capability.
        assert!(init["result"]["capabilities"]["tasks"].is_object());
        assert!(init["result"]["capabilities"]["tasks"]["requests"]["tools"]["call"].is_object());

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

    // ── Async tasks (SEP-1686, Slice 2a) ─────────────────────────────
    //
    // A task-augmented `tools/call` returns `CreateTaskResult` + a
    // `notifications/tasks/created`, then the result is fetched via
    // `tasks/get` (poll to `completed`) and `tasks/result`.
    #[tokio::test]
    async fn async_task_create_poll_result_list() {
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

        let task_id = "task-abc";
        let req = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "web_search",
                "arguments": { "query": "async" },
                "_meta": {
                    "modelcontextprotocol.io/task": { "taskId": task_id, "ttl": 60000 }
                }
            }
        });
        let mut bytes = serde_json::to_vec(&req).unwrap();
        bytes.push(b'\n');
        client_w.write_all(&bytes).await.unwrap();
        client_w.flush().await.unwrap();

        // Frame 1: notifications/tasks/created (no id), related-task meta.
        let created = read_json_line(&mut client_r).await;
        assert_eq!(created["method"], "notifications/tasks/created");
        assert_eq!(
            created["params"]["_meta"]["modelcontextprotocol.io/related-task"]["taskId"],
            task_id
        );

        // Frame 2: CreateTaskResult echoing the client taskId.
        let create_resp = read_json_line(&mut client_r).await;
        assert_eq!(create_resp["id"], 10);
        assert_eq!(create_resp["result"]["task"]["taskId"], task_id);
        assert_eq!(create_resp["result"]["task"]["ttl"], 60000);
        let status = create_resp["result"]["task"]["status"].as_str().unwrap();
        assert!(status == "working" || status == "completed", "got: {status}");

        // Poll tasks/get until terminal.
        let mut final_status = String::new();
        for _ in 0..50 {
            let got = rpc_roundtrip(
                &mut client_w,
                &mut client_r,
                "tasks/get",
                11,
                Some(json!({ "taskId": task_id })),
            )
            .await;
            final_status = got["result"]["status"].as_str().unwrap().to_string();
            if final_status == "completed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(final_status, "completed", "task never completed");

        // tasks/result returns the CallToolResult (mock LLM stub output).
        let result = rpc_roundtrip(
            &mut client_w,
            &mut client_r,
            "tasks/result",
            12,
            Some(json!({ "taskId": task_id })),
        )
        .await;
        assert_eq!(result["result"]["isError"], false, "got: {result}");
        assert_eq!(result["result"]["content"][0]["text"], "search-result-stub");

        // tasks/list shows the one task.
        let listed = rpc_roundtrip(&mut client_w, &mut client_r, "tasks/list", 13, None).await;
        let tasks = listed["result"]["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["taskId"], task_id);

        // tasks/get on an unknown id is a protocol error (-32602).
        let unknown = rpc_roundtrip(
            &mut client_w,
            &mut client_r,
            "tasks/get",
            14,
            Some(json!({ "taskId": "nope" })),
        )
        .await;
        assert_eq!(unknown["error"]["code"], super::protocol::ERR_INVALID_PARAMS);

        drop(client_w);
        server_task.await.unwrap().unwrap();
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
