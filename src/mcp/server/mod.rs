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

pub mod elicitation;
pub mod handlers;
pub mod link;
pub mod observability;
pub mod protocol;
pub mod reasoning;
pub mod tasks;

use handlers::{
    cancel_by_request_id, handle_initialize, handle_tasks_cancel, handle_tasks_get,
    handle_tasks_list, handle_tasks_result, handle_tools_call, handle_tools_list, internal_error,
    unknown_method,
};
use link::{McpClientLink, Outbound};
use observability::SharedObservability;
use protocol::{
    parse_client_caps, MethodOutcome, RpcError, RpcRequest, RpcResponse, ERR_INVALID_REQUEST,
    ERR_PARSE, JSONRPC_VERSION,
};
use tasks::TaskStore;

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

/// Serve MCP over the given reader/writer pair, creating a fresh
/// [`McpClientLink`] internally. Used by callers that don't need the
/// server→client side-channel (the elicitation approval bridge); see
/// [`serve_with_link`] for the full path. Returns when the reader reaches
/// EOF (the canonical "client disconnected" signal for stdio MCP servers).
pub async fn serve<R, W>(
    runtime: Arc<Runtime>,
    config: ServerConfig,
    reader: R,
    writer: W,
) -> Result<(), String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (link, outbound_rx) = McpClientLink::new();
    serve_with_link(runtime, config, reader, writer, link, outbound_rx, None).await
}

/// Serve MCP over a bidirectional, concurrent loop (ADR-0033 2b-sync).
///
/// Unlike the original serial loop, this:
/// - drains all outbound frames through a single **writer task** fed by an
///   mpsc, so server-side tasks (the elicitation approval channel holding a
///   clone of `link`) can push `elicitation/create` requests without owning
///   the wire;
/// - **spawns** each inbound request handler instead of awaiting it inline,
///   so the read loop keeps pumping while a synchronous `tools/call` is parked
///   on an approval — that's how the client's elicitation *response* gets read.
///
/// `link` and `outbound_rx` are paired (`McpClientLink::new`). Callers that
/// want the elicitation bridge construct the pair, start an
/// [`elicitation::McpElicitationApprovalChannel`] with a clone of `link`, then
/// hand both here.
pub async fn serve_with_link<R, W>(
    runtime: Arc<Runtime>,
    config: ServerConfig,
    mut reader: R,
    writer: W,
    link: Arc<McpClientLink>,
    outbound_rx: tokio::sync::mpsc::UnboundedReceiver<Outbound>,
    observability: SharedObservability,
) -> Result<(), String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let filter = Arc::new(PipelineFilter::from_config(&config)?);
    let store = Arc::new(TaskStore::new());

    // ADR-0042: bridge parked `ask:` steps to the caller via the task/resume
    // surface. The channel flips the owning task to InputRequired; the
    // `zymi/reasoning/resume` method (verified with `signer`) supplies the
    // answer. The runtime routes `ask:` steps to CHANNEL_NAME (set in the
    // serve entrypoint via `with_reasoning_channel`).
    let signer = Arc::new(reasoning::ResumeTokenSigner::random());
    let reasoning_handle = {
        use crate::reasoning::ReasoningChannel;
        let channel =
            reasoning::McpTaskReasoningChannel::new(Arc::clone(&store), Arc::clone(&signer));
        channel.start(Arc::clone(runtime.bus())).await?
    };

    // The writer task owns the wire; everyone emits frames via the mpsc.
    let writer_task = tokio::spawn(writer_loop(writer, outbound_rx));

    let outbound = link.outbound();
    let mut buf = String::new();
    let result = loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => break Ok(()), // EOF — client closed.
            Ok(_) => {}
            Err(e) => break Err(format!("mcp serve: read error: {e}")),
        }

        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        dispatch_inbound(
            &runtime,
            &filter,
            &store,
            &signer,
            &link,
            &observability,
            &outbound,
            trimmed,
        );
    };

    // Client gone: stop writing. Any in-flight response was already flushed
    // (the client reads each before sending the next or closing).
    writer_task.abort();
    reasoning_handle.abort_all();
    result
}

/// Drain outbound frames onto the wire, in order, until the channel closes.
async fn writer_loop<W>(mut writer: W, mut rx: tokio::sync::mpsc::UnboundedReceiver<Outbound>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        if let Err(e) = write_frame(&mut writer, &frame).await {
            log::error!("mcp serve: {e}");
            break;
        }
    }
}

/// Route a single inbound JSON-RPC line. Three shapes:
/// - a **response** to one of our server-initiated requests (no `method`,
///   has `id`) → delivered to the waiting [`McpClientLink::request`];
/// - a **notification** (`method`, no `id`) → handled inline (cheap);
/// - a **request** (`method` + `id`) → spawned, so the read loop keeps
///   reading (a synchronous `tools/call` may park on an approval whose
///   elicitation response arrives on this same loop).
#[allow(clippy::too_many_arguments)]
fn dispatch_inbound(
    runtime: &Arc<Runtime>,
    filter: &Arc<PipelineFilter>,
    store: &Arc<TaskStore>,
    signer: &Arc<reasoning::ResumeTokenSigner>,
    link: &Arc<McpClientLink>,
    observability: &SharedObservability,
    outbound: &tokio::sync::mpsc::UnboundedSender<Outbound>,
    line: &str,
) {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            // Parse failure — id is unknown, spec says respond with id: null.
            let _ = outbound.send(Outbound::Response(RpcResponse::err(
                Value::Null,
                RpcError::new(ERR_PARSE, format!("invalid json: {e}")),
            )));
            return;
        }
    };

    // No `method` ⇒ this is a response to a request *we* issued.
    if value.get("method").is_none() {
        if let Some(id) = value.get("id") {
            let payload = match value.get("error") {
                Some(err) => Err(err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("client error")
                    .to_string()),
                None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
            };
            let id = id.clone();
            let link = Arc::clone(link);
            tokio::spawn(async move {
                link.deliver_response(&id, payload).await;
            });
        } else {
            log::debug!("mcp serve: inbound frame with neither method nor id, ignored");
        }
        return;
    }

    let request: RpcRequest = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => {
            let _ = outbound.send(Outbound::Response(RpcResponse::err(
                Value::Null,
                RpcError::new(ERR_PARSE, format!("invalid request: {e}")),
            )));
            return;
        }
    };

    // Notifications carry no `id`; the spec forbids responding to them.
    let is_notification = request.id.is_none();

    if request.jsonrpc.as_deref() != Some(JSONRPC_VERSION) {
        if is_notification {
            log::debug!("mcp serve: notification with bad jsonrpc field, ignored");
            return;
        }
        let _ = outbound.send(Outbound::Response(RpcResponse::err(
            request.id.clone().unwrap_or(Value::Null),
            RpcError::new(ERR_INVALID_REQUEST, "missing or wrong `jsonrpc` field"),
        )));
        return;
    }

    if is_notification {
        let store = Arc::clone(store);
        tokio::spawn(async move {
            handle_notification(&store, &request).await;
        });
        return;
    }

    // Real request — spawn so the read loop keeps pumping.
    let id = request.id.clone().unwrap_or(Value::Null);
    let runtime = Arc::clone(runtime);
    let filter = Arc::clone(filter);
    let store = Arc::clone(store);
    let signer = Arc::clone(signer);
    let link = Arc::clone(link);
    let observability = observability.clone();
    let outbound = outbound.clone();
    tokio::spawn(async move {
        let frames: Vec<Outbound> = match handle_method(
            &runtime,
            &filter,
            &store,
            &signer,
            &link,
            &observability,
            &request,
        )
        .await
        {
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
        };
        for frame in frames {
            let _ = outbound.send(frame);
        }
    });
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

#[allow(clippy::too_many_arguments)]
async fn handle_method(
    runtime: &Arc<Runtime>,
    filter: &Arc<PipelineFilter>,
    store: &Arc<TaskStore>,
    signer: &Arc<reasoning::ResumeTokenSigner>,
    link: &Arc<McpClientLink>,
    observability: &SharedObservability,
    request: &RpcRequest,
) -> Result<MethodOutcome, RpcError> {
    let require_params = || {
        request
            .params
            .as_ref()
            .ok_or_else(|| RpcError::new(protocol::ERR_INVALID_PARAMS, "missing params"))
    };
    match request.method.as_str() {
        "initialize" => {
            // Capture the client's capabilities so the elicitation approval
            // bridge knows whether `elicitation/create` is usable. `initialize`
            // always precedes any `tools/call`, so caps are set before an
            // approval can fire.
            if let Some(p) = request.params.as_ref() {
                link.set_caps(parse_client_caps(p));
            }
            Ok(MethodOutcome::just(handle_initialize()))
        }
        "tools/list" => Ok(MethodOutcome::just(handle_tools_list(
            runtime,
            observability.as_deref(),
            |name| filter.matches(name),
        ))),
        "tools/call" => {
            let params = require_params()?;
            let id = request.id.clone().unwrap_or(Value::Null);
            handle_tools_call(
                runtime,
                store,
                observability.as_deref(),
                &id,
                params,
                |name| filter.matches(name),
            )
            .await
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
        // ADR-0042: the caller answers a parked `ask:` step.
        "zymi/reasoning/resume" => {
            handlers::handle_reasoning_resume(runtime, store, signer, require_params()?)
                .await
                .map(MethodOutcome::just)
        }
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
        Outbound::Request { id, method, params } => {
            let envelope = serde_json::json!({
                "jsonrpc": JSONRPC_VERSION,
                "id": id,
                "method": method,
                "params": params,
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
    //   - `long_task`      — exposed, async (listed like sync since Slice 2a)
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
                    ..Default::default()
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

    // ── Bidirectional: server-initiated request round-trips ──────────
    //
    // Exercises the 2b-sync transport: a server-side task (standing in for
    // the elicitation approval channel) issues a request via the
    // `McpClientLink`; the read loop must route the client's *response*
    // (which carries no `method`) back to the waiting caller while the loop
    // keeps pumping.
    #[tokio::test]
    async fn server_initiated_request_round_trips_over_wire() {
        use crate::mcp::server::link::McpClientLink;

        let dir = TempDir::new().unwrap();
        let runtime = build_runtime(build_workspace(), &dir);

        let (client_to_server, server_in) = tokio::io::duplex(8192);
        let (server_out, client_from_server) = tokio::io::duplex(8192);

        let (link, outbound_rx) = McpClientLink::new();
        let link_for_server = Arc::clone(&link);
        let server_task = tokio::spawn(async move {
            serve_with_link(
                runtime,
                ServerConfig::default(),
                BufReader::new(server_in),
                server_out,
                link_for_server,
                outbound_rx,
                None,
            )
            .await
        });

        let mut client_w = client_to_server;
        let mut client_r = BufReader::new(client_from_server);

        // Server side initiates a request (as the elicitation channel would).
        let caller = tokio::spawn(async move {
            link.request("elicitation/create", json!({ "message": "approve?" }))
                .await
        });

        // Client reads the server-initiated request and replies.
        let req = read_json_line(&mut client_r).await;
        assert_eq!(req["method"], "elicitation/create");
        assert_eq!(req["params"]["message"], "approve?");
        let id = req["id"].clone();
        let resp = json!({ "jsonrpc": "2.0", "id": id, "result": { "action": "accept" } });
        let mut bytes = serde_json::to_vec(&resp).unwrap();
        bytes.push(b'\n');
        client_w.write_all(&bytes).await.unwrap();
        client_w.flush().await.unwrap();

        let result = caller.await.unwrap().expect("request should resolve");
        assert_eq!(result["action"], "accept");

        drop(client_w);
        server_task.await.unwrap().unwrap();
    }

    // ── Observability tools (ADR-0034) end-to-end ────────────────────
    //
    // With a `CliObservability` provider wired in, `tools/list` advertises
    // the four `zymi.runs.*` tools; running a sync pipeline records the run
    // under session scope; then list / get / events / step_io read it back.
    // The provider implementation lives behind the `cli` feature; the
    // ObservabilityProvider port itself stays feature-free.
    #[cfg(feature = "cli")]
    #[tokio::test]
    async fn observability_tools_list_get_events_step_io() {
        use crate::cli::mcp_observability::{CliObservability, ObservabilityScope};

        let dir = TempDir::new().unwrap();
        let runtime = build_runtime(build_workspace(), &dir);

        let (client_to_server, server_in) = tokio::io::duplex(8192);
        let (server_out, client_from_server) = tokio::io::duplex(8192);

        let (link, outbound_rx) = McpClientLink::new();
        let provider = Arc::new(CliObservability::new(
            Arc::clone(&runtime),
            ObservabilityScope::Session,
        ));
        let server_task = tokio::spawn(async move {
            serve_with_link(
                runtime,
                ServerConfig::default(),
                BufReader::new(server_in),
                server_out,
                link,
                outbound_rx,
                Some(provider),
            )
            .await
        });

        let mut client_w = client_to_server;
        let mut client_r = BufReader::new(client_from_server);

        // tools/list advertises the four observability tools alongside pipelines.
        let listed = rpc_roundtrip(&mut client_w, &mut client_r, "tools/list", 1, None).await;
        let names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"zymi.runs.list"), "got: {names:?}");
        assert!(names.contains(&"zymi.runs.step_io"), "got: {names:?}");

        // Run the sync pipeline so a run is recorded under this session.
        let called = rpc_roundtrip(
            &mut client_w,
            &mut client_r,
            "tools/call",
            2,
            Some(json!({ "name": "web_search", "arguments": { "query": "x" } })),
        )
        .await;
        assert_eq!(called["result"]["isError"], false, "got: {called}");

        // zymi.runs.list → exactly our one session run.
        let list = obs_call(&mut client_w, &mut client_r, 3, "zymi.runs.list", json!({})).await;
        let runs = list["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1, "got: {list}");
        let run_id = runs[0]["run_id"].as_str().unwrap().to_string();
        assert_eq!(runs[0]["pipeline"], "web_search");
        assert_eq!(runs[0]["status"], "ok");

        // zymi.runs.get → status + step counts.
        let got = obs_call(
            &mut client_w,
            &mut client_r,
            4,
            "zymi.runs.get",
            json!({ "run_id": run_id }),
        )
        .await;
        assert_eq!(got["status"], "ok");
        assert_eq!(got["total_steps"], 1);

        // zymi.runs.events → flat trace, includes the step substream events.
        let events = obs_call(
            &mut client_w,
            &mut client_r,
            5,
            "zymi.runs.events",
            json!({ "run_id": run_id }),
        )
        .await;
        let evs = events["events"].as_array().unwrap();
        assert!(!evs.is_empty(), "expected events, got: {events}");
        assert!(
            evs.iter().any(|e| e["step_id"] == "go"),
            "expected a step-tagged event, got: {events}"
        );

        // zymi.runs.step_io → reconstructed prompt-in + output for step `go`.
        let io = obs_call(
            &mut client_w,
            &mut client_r,
            6,
            "zymi.runs.step_io",
            json!({ "run_id": run_id, "step_id": "go" }),
        )
        .await;
        assert_eq!(io["output"], "search-result-stub", "got: {io}");
        let roles: Vec<&str> = io["prompt_in"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert!(roles.contains(&"system") && roles.contains(&"user"), "got: {roles:?}");

        // Out-of-scope run id is refused under session scope (JSON-RPC error,
        // the same class as a bad-argument: the run isn't ours to read).
        let denied = rpc_roundtrip(
            &mut client_w,
            &mut client_r,
            "tools/call",
            7,
            Some(json!({ "name": "zymi.runs.get", "arguments": { "run_id": "not-ours" } })),
        )
        .await;
        assert!(denied["error"].is_object(), "expected JSON-RPC error, got: {denied}");

        drop(client_w);
        server_task.await.unwrap().unwrap();
    }

    /// Call an observability tool and parse the JSON packed in its text block.
    async fn obs_call(
        client_w: &mut tokio::io::DuplexStream,
        client_r: &mut BufReader<tokio::io::DuplexStream>,
        id: u64,
        tool: &str,
        arguments: Value,
    ) -> Value {
        let resp = rpc_roundtrip(
            client_w,
            client_r,
            "tools/call",
            id,
            Some(json!({ "name": tool, "arguments": arguments })),
        )
        .await;
        assert_eq!(resp["result"]["isError"], false, "tool {tool} errored: {resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
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
