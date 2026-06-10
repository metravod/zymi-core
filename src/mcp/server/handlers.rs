//! MCP method handlers for `zymi mcp serve` (ADR-0033).
//!
//! - `initialize` — protocol handshake, server info, tools + tasks capabilities.
//! - `tools/list` — enumerate exposed pipelines (sync and async alike) as MCP tools.
//! - `tools/call` — invoke an exposed pipeline: plain calls block until
//!   completion; task-augmented calls (SEP-1686, `_meta.task`) return a
//!   task handle immediately.
//! - `tasks/get` / `tasks/result` / `tasks/list` / `tasks/cancel` — the
//!   SEP-1686 task surface over backgrounded pipeline runs.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::commands::RunPipeline;
use crate::config::pipeline::PipelineConfig;
use crate::events::{Event, EventKind};
use crate::handlers::run_pipeline;
use crate::runtime::Runtime;

use super::observability::ObservabilityProvider;
use super::protocol::{
    related_task_meta, task_augmentation, MethodOutcome, Notification, RpcError, Task, TaskStatus,
    ERR_INTERNAL, ERR_INVALID_PARAMS, ERR_METHOD_NOT_FOUND,
};
use super::tasks::{TaskHandle, TaskState, TaskStore};

/// Server-side protocol version advertised in `initialize`. Tracks the
/// 2025-11-25 MCP spec (the version with SEP-1686 accepted); the v1
/// surface only uses methods/capabilities that are stable across
/// 2025-06-18 → 2025-11-25, so older clients still negotiate cleanly.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Server identity surfaced to MCP clients via `initialize`.
pub const SERVER_NAME: &str = "zymi";

/// `initialize` result: capabilities + server info + protocol version.
///
/// Declares the SEP-1686 `tasks` capability: any `tools/call` may be
/// task-augmented (`_meta["modelcontextprotocol.io/task"]`), and we support
/// `tasks/list` and cancellation. The shape mirrors the `2025-11-25` schema
/// (`tasks: { list, cancel, requests: { tools: { call } } }`).
pub fn handle_initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": {},
            "tasks": {
                "list": {},
                "cancel": {},
                "requests": { "tools": { "call": {} } }
            }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// Enumerate exposed pipelines (`expose.mcp:` opt-in, ADR-0033 §2) as
/// MCP tool descriptors.
///
/// Since Slice 2a, async-mode pipelines are listed alongside sync ones:
/// task execution is driven by the client augmenting `tools/call` with
/// `_meta.task` (SEP-1686), not by the exposure mode. `mode: async` is now
/// a hint that callers SHOULD task-augment the call; the tool descriptor is
/// identical either way.
///
/// `filter` decides which pipeline names are exposed at boot time
/// (`--include` / `--exclude` glob filters); it's applied *before* the
/// `expose:` check so that names which never pass the filter cannot
/// leak through.
pub fn handle_tools_list<F>(
    runtime: &Runtime,
    observability: Option<&dyn ObservabilityProvider>,
    filter: F,
) -> Value
where
    F: Fn(&str) -> bool,
{
    let mut tools: Vec<Value> = Vec::new();
    let mut names: Vec<&String> = runtime.workspace().pipelines.keys().collect();
    names.sort();
    for name in names {
        if !filter(name) {
            continue;
        }
        let config = &runtime.workspace().pipelines[name];
        if config.mcp_exposure().is_none() {
            continue;
        }
        let mut tool = Map::new();
        let tool_name = config
            .mcp_tool_name()
            .map(str::to_string)
            .unwrap_or_else(|| config.name.clone());
        tool.insert("name".into(), Value::String(tool_name));
        if let Some(desc) = config.mcp_tool_description() {
            tool.insert("description".into(), Value::String(desc.into()));
        }
        tool.insert("inputSchema".into(), config.inputs_json_schema());
        tools.push(Value::Object(tool));
    }
    // ADR-0034: append the read-only observability tools when enabled.
    if let Some(obs) = observability {
        tools.extend(obs.tool_descriptors());
    }
    json!({ "tools": tools })
}

/// Invoke an exposed pipeline.
///
/// Two modes, decided by whether the client task-augmented the call
/// (`_meta["modelcontextprotocol.io/task"]`, SEP-1686):
///
/// - **No augmentation (sync):** block until the pipeline terminates and
///   return the `CallToolResult` directly (the Slice 1 path).
/// - **Augmented (async):** spawn the pipeline as a tracked task, return
///   `CreateTaskResult { task }` immediately, and emit a
///   `notifications/tasks/created`. The result is later fetched via
///   `tasks/result`.
///
/// Per MCP convention, pipeline failures surface inside the tool result
/// (`isError: true`) rather than as a JSON-RPC error. JSON-RPC errors are
/// reserved for protocol-level problems: unknown tool name, malformed
/// arguments, duplicate `taskId`.
pub async fn handle_tools_call<F>(
    runtime: &Arc<Runtime>,
    store: &Arc<TaskStore>,
    observability: Option<&dyn ObservabilityProvider>,
    request_id: &Value,
    params: &Value,
    filter: F,
) -> Result<MethodOutcome, RpcError>
where
    F: Fn(&str) -> bool,
{
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::new(ERR_INVALID_PARAMS, "missing tool `name`"))?;

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));

    // ADR-0034: observability tools (`zymi.runs.*`) are not pipelines —
    // dispatch them to the provider before pipeline resolution.
    if let Some(obs) = observability {
        if obs.handles(name) {
            return obs.call(name, &arguments).await.map(MethodOutcome::just);
        }
    }

    let (pipeline_name, _config) = resolve_exposed_pipeline(runtime, name, filter)
        .ok_or_else(|| RpcError::new(ERR_METHOD_NOT_FOUND, format!("unknown tool `{name}`")))?;

    let inputs = coerce_arguments(&arguments)
        .map_err(|e| RpcError::new(ERR_INVALID_PARAMS, e))?;

    match task_augmentation(params) {
        Some(aug) => {
            // §4.2.3: a re-used taskId is a protocol error.
            if store.contains(&aug.task_id).await {
                return Err(RpcError::new(
                    ERR_INVALID_PARAMS,
                    format!("taskId `{}` is already in use", aug.task_id),
                ));
            }
            // ADR-0034 session scope: the async task runs on stream
            // `mcp-task-{task_id}` (see spawn_pipeline_task).
            if let Some(obs) = observability {
                obs.record_run(&format!("mcp-task-{}", aug.task_id));
            }
            let task = spawn_pipeline_task(
                runtime,
                store,
                request_id,
                pipeline_name,
                inputs,
                aug.ttl,
                aug.task_id.clone(),
            )
            .await;
            let result = json!({
                "task": task,
                "_meta": related_task_meta(&aug.task_id),
            });
            // SEP-1686 §3.4: signal the task exists so polling can begin
            // without racing creation.
            let created = Notification {
                method: "notifications/tasks/created".to_string(),
                params: json!({ "_meta": related_task_meta(&aug.task_id) }),
            };
            Ok(MethodOutcome {
                result,
                notifications: vec![created],
            })
        }
        None => {
            let cmd = RunPipeline::new(pipeline_name.clone(), inputs);
            let result = match run_pipeline::handle(runtime, cmd).await {
                Ok(pr) => {
                    // ADR-0034 session scope: record the run_id the handler
                    // derived so the agent can introspect this sync run.
                    if let Some(obs) = observability {
                        obs.record_run(&pr.stream_id);
                    }
                    tool_call_result(pr.success, pr.final_output.as_deref(), None)
                }
                Err(err) => tool_call_result(false, None, Some(err.as_str())),
            };
            Ok(MethodOutcome::just(result))
        }
    }
}

/// Spawn a pipeline as a backgrounded task and register it. Returns the
/// initial `working` [`Task`] snapshot. The spawned wrapper writes the
/// terminal outcome into the task's shared state and emits a
/// `PipelineCompleted` envelope so `zymi runs` / `zymi observe` see a
/// complete run.
async fn spawn_pipeline_task(
    runtime: &Arc<Runtime>,
    store: &Arc<TaskStore>,
    request_id: &Value,
    pipeline_name: String,
    inputs: HashMap<String, String>,
    ttl: Option<i64>,
    task_id: String,
) -> Task {
    let correlation_id = Uuid::new_v4();
    let stream_id = format!("mcp-task-{task_id}");
    let now = Utc::now();

    // Observability marker (run_pipeline with a pre-bound stream skips its
    // own PipelineRequested; we emit it here, like the serve router does).
    let req_ev = Event::new(
        stream_id.clone(),
        EventKind::PipelineRequested {
            pipeline: pipeline_name.clone(),
            inputs: inputs.clone(),
        },
        "mcp".into(),
    )
    .with_correlation(correlation_id);
    let _ = runtime.bus().publish(req_ev).await;

    let state = Arc::new(Mutex::new(TaskState {
        status: TaskStatus::Working,
        status_message: None,
        result: None,
        last_updated_at: now,
    }));

    let cmd = RunPipeline::from_request(
        pipeline_name.clone(),
        inputs,
        correlation_id,
        stream_id.clone(),
    );

    let rt = Arc::clone(runtime);
    let state_for_task = Arc::clone(&state);
    let pipeline_for_task = pipeline_name.clone();
    let join = tokio::spawn(async move {
        let res = run_pipeline::handle(&rt, cmd).await;

        let (success, final_output, error) = match &res {
            Ok(pr) => (pr.success, pr.final_output.clone(), None),
            Err(e) => (false, None, Some(e.clone())),
        };
        let done_ev = Event::new(
            stream_id,
            EventKind::PipelineCompleted {
                pipeline: pipeline_for_task,
                success,
                final_output,
                error,
            },
            "mcp".into(),
        )
        .with_correlation(correlation_id);
        let _ = rt.bus().publish(done_ev).await;

        let mut s = state_for_task.lock().await;
        // A concurrent cancel wins — don't overwrite a terminal cancel.
        if s.status == TaskStatus::Cancelled {
            return;
        }
        s.last_updated_at = Utc::now();
        // A business failure (Ok{success:false} or Err) is still a
        // lifecycle-`completed` task whose CallToolResult carries
        // `isError: true` — mirrors the sync path. `failed` lifecycle is
        // reserved for task-machinery errors, which this path can't hit.
        match res {
            Ok(pr) => {
                s.status = TaskStatus::Completed;
                if !pr.success {
                    s.status_message = Some("pipeline reported failure".into());
                }
                s.result = Some(tool_call_result(
                    pr.success,
                    pr.final_output.as_deref(),
                    None,
                ));
            }
            Err(e) => {
                s.status = TaskStatus::Completed;
                s.status_message = Some(e.clone());
                s.result = Some(tool_call_result(false, None, Some(&e)));
            }
        }
    });

    let handle = Arc::new(TaskHandle {
        task_id,
        pipeline: pipeline_name,
        request_id: request_id.clone(),
        ttl,
        created_at: now,
        abort: join.abort_handle(),
        state,
    });
    let snapshot = handle.snapshot().await;
    store.register(handle).await;
    snapshot
}

/// `tasks/get` → `GetTaskResult` (`Result & Task`: the Task fields at the
/// top level plus a related-task `_meta`).
pub async fn handle_tasks_get(store: &Arc<TaskStore>, params: &Value) -> Result<Value, RpcError> {
    let task_id = require_task_id(params)?;
    let handle = store
        .get(&task_id)
        .await
        .ok_or_else(|| RpcError::new(ERR_INVALID_PARAMS, format!("unknown taskId `{task_id}`")))?;
    let task = handle.snapshot().await;
    let mut v = serde_json::to_value(&task).map_err(internal_error)?;
    if let Value::Object(ref mut m) = v {
        m.insert("_meta".into(), related_task_meta(&task_id));
    }
    Ok(v)
}

/// `tasks/result` → the original `CallToolResult`. §4.6: MUST error unless
/// the task is `completed`.
pub async fn handle_tasks_result(
    store: &Arc<TaskStore>,
    params: &Value,
) -> Result<Value, RpcError> {
    let task_id = require_task_id(params)?;
    let handle = store
        .get(&task_id)
        .await
        .ok_or_else(|| RpcError::new(ERR_INVALID_PARAMS, format!("unknown taskId `{task_id}`")))?;
    let s = handle.state.lock().await;
    if s.status != TaskStatus::Completed {
        return Err(RpcError::new(
            ERR_INVALID_PARAMS,
            format!(
                "task `{task_id}` is not completed (status: {})",
                status_str(s.status)
            ),
        ));
    }
    let mut result = s
        .result
        .clone()
        .unwrap_or_else(|| tool_call_result(true, None, None));
    if let Value::Object(ref mut m) = result {
        m.insert("_meta".into(), related_task_meta(&task_id));
    }
    Ok(result)
}

/// `tasks/list` → all tasks, single page (2a does not paginate).
pub async fn handle_tasks_list(store: &Arc<TaskStore>) -> Value {
    json!({ "tasks": store.list().await })
}

/// `tasks/cancel` (capability-gated explicit cancel). Idempotent ack.
pub async fn handle_tasks_cancel(
    store: &Arc<TaskStore>,
    params: &Value,
) -> Result<Value, RpcError> {
    let task_id = require_task_id(params)?;
    let handle = store
        .get(&task_id)
        .await
        .ok_or_else(|| RpcError::new(ERR_INVALID_PARAMS, format!("unknown taskId `{task_id}`")))?;
    cancel_handle(&handle).await;
    Ok(json!({ "_meta": related_task_meta(&task_id) }))
}

/// Cancel a task by the JSON-RPC id of its originating request — the path
/// taken for `notifications/cancelled { requestId }` (§4.8.1). No-op if no
/// task maps to that id (the cancel may target a non-task request).
pub async fn cancel_by_request_id(store: &Arc<TaskStore>, request_id: &Value) {
    if let Some(handle) = store.find_by_request_id(request_id).await {
        cancel_handle(&handle).await;
    }
}

/// Best-effort cancel: flip to `cancelled` (unless already terminal, §4.8.3)
/// and abort the spawned wrapper. Inner step tasks already spawned by the
/// engine are detached and may run on — a real engine-level cancellation
/// token is deferred (see ADR-0033 addendum §5).
async fn cancel_handle(handle: &Arc<TaskHandle>) {
    {
        let mut s = handle.state.lock().await;
        if s.status.is_terminal() {
            return;
        }
        s.status = TaskStatus::Cancelled;
        s.status_message = Some("cancelled by requestor".into());
        s.last_updated_at = Utc::now();
    }
    handle.abort.abort();
}

fn require_task_id(params: &Value) -> Result<String, RpcError> {
    params
        .get("taskId")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| RpcError::new(ERR_INVALID_PARAMS, "missing `taskId`"))
}

fn status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Working => "working",
        TaskStatus::InputRequired => "input_required",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn resolve_exposed_pipeline<'r, F>(
    runtime: &'r Runtime,
    tool_name: &str,
    filter: F,
) -> Option<(String, &'r PipelineConfig)>
where
    F: Fn(&str) -> bool,
{
    runtime
        .workspace()
        .pipelines
        .iter()
        .find(|(name, config)| {
            filter(name)
                && config.mcp_exposure().is_some()
                && config.mcp_tool_name() == Some(tool_name)
        })
        .map(|(name, config)| (name.clone(), config))
}

/// Coerce MCP `arguments` (`Map<String, Value>`) into the
/// `HashMap<String, String>` that the pipeline runtime ingests today.
/// Decision recorded in [[project_install_ux_fetch]] adjacent: stringify
/// at the MCP boundary, leave runtime types untouched (Slice 1 scope).
fn coerce_arguments(arguments: &Value) -> Result<HashMap<String, String>, String> {
    let obj = match arguments {
        Value::Object(o) => o,
        Value::Null => return Ok(HashMap::new()),
        _ => return Err("`arguments` must be an object".into()),
    };
    let mut out = HashMap::with_capacity(obj.len());
    for (k, v) in obj {
        let s = match v {
            Value::Null => continue,
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Array(_) | Value::Object(_) => serde_json::to_string(v)
                .map_err(|e| format!("failed to serialize `{k}`: {e}"))?,
        };
        out.insert(k.clone(), s);
    }
    Ok(out)
}

fn tool_call_result(success: bool, output: Option<&str>, error: Option<&str>) -> Value {
    let text = match (output, error) {
        (Some(o), _) => o.to_string(),
        (None, Some(e)) => format!("pipeline failed: {e}"),
        (None, None) => {
            if success {
                "(no output)".to_string()
            } else {
                "pipeline failed".to_string()
            }
        }
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": !success,
    })
}

pub fn unknown_method(method: &str) -> RpcError {
    RpcError::new(ERR_METHOD_NOT_FOUND, format!("unknown method `{method}`"))
}

pub fn internal_error(err: impl std::fmt::Display) -> RpcError {
    RpcError::new(ERR_INTERNAL, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_advertises_tools_capability() {
        let result = handle_initialize();
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[test]
    fn coerce_arguments_handles_scalar_types() {
        let v = json!({
            "str": "hello",
            "num": 42,
            "flag": true,
            "null_field": null,
        });
        let out = coerce_arguments(&v).unwrap();
        assert_eq!(out.get("str").map(String::as_str), Some("hello"));
        assert_eq!(out.get("num").map(String::as_str), Some("42"));
        assert_eq!(out.get("flag").map(String::as_str), Some("true"));
        assert!(!out.contains_key("null_field"));
    }

    #[test]
    fn coerce_arguments_serialises_compound_values() {
        let v = json!({
            "items": ["a", "b"],
            "obj": { "k": 1 },
        });
        let out = coerce_arguments(&v).unwrap();
        assert_eq!(out.get("items").unwrap(), r#"["a","b"]"#);
        assert_eq!(out.get("obj").unwrap(), r#"{"k":1}"#);
    }

    #[test]
    fn coerce_arguments_rejects_non_object() {
        let err = coerce_arguments(&json!("not an object")).unwrap_err();
        assert!(err.contains("must be an object"), "got: {err}");
    }

    #[test]
    fn coerce_arguments_accepts_null_and_missing() {
        assert!(coerce_arguments(&Value::Null).unwrap().is_empty());
        assert!(coerce_arguments(&json!({})).unwrap().is_empty());
    }

    #[test]
    fn tool_call_result_packs_success_payload() {
        let v = tool_call_result(true, Some("done"), None);
        assert_eq!(v["isError"], false);
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "done");
    }

    #[test]
    fn tool_call_result_packs_error_payload() {
        let v = tool_call_result(false, None, Some("boom"));
        assert_eq!(v["isError"], true);
        assert!(v["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("boom"));
    }
}
