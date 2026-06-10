//! CLI-layer adapter for the MCP observability tools (ADR-0034).
//!
//! Implements [`ObservabilityProvider`] (the `mcp::server` port) using the
//! run/event projections that live in the `cli` layer (`list_runs`,
//! `format_event`) plus the runtime `events_to_messages` / `resolve_task_template`
//! reconstruction helpers. `zymi mcp serve` constructs one of these when
//! `--expose-observability` is set and hands it to the serve loop.
//!
//! Four read-only tools, designed for *narrow-before-deepen* agent use:
//! `zymi.runs.list` → `get` → `events` (filtered) → `step_io` (the expensive,
//! full-fidelity call). Per-step I/O is reconstructed from the
//! `{run_id}:step:{step_id}` substream that `run_pipeline` already writes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::cli::event_fmt::format_event;
use crate::cli::runs_data::{list_runs, RunStatus};
use crate::config::pipeline::PipelineStepKind;
use crate::events::store::EventStore;
use crate::events::{Event, EventKind};
use crate::handlers::run_pipeline::resolve_task_template;
use crate::mcp::server::observability::ObservabilityProvider;
use crate::mcp::server::protocol::{RpcError, ERR_INTERNAL, ERR_INVALID_PARAMS};
use crate::runtime::context_builder::events_to_messages;
use crate::runtime::Runtime;
use crate::types::Message;

/// Default-`session` restricts introspection to runs this serve process
/// started; `all` exposes every run in the store (single-user dev).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservabilityScope {
    Session,
    All,
}

impl ObservabilityScope {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "session" => Ok(Self::Session),
            "all" => Ok(Self::All),
            other => Err(format!(
                "invalid --observability-scope '{other}' (expected 'session' or 'all')"
            )),
        }
    }
}

const TOOL_LIST: &str = "zymi.runs.list";
const TOOL_GET: &str = "zymi.runs.get";
const TOOL_EVENTS: &str = "zymi.runs.events";
const TOOL_STEP_IO: &str = "zymi.runs.step_io";

pub struct CliObservability {
    runtime: Arc<Runtime>,
    scope: ObservabilityScope,
    /// Run ids (stream ids) this serve process initiated, for `session` scope.
    session_runs: Mutex<HashSet<String>>,
}

impl CliObservability {
    pub fn new(runtime: Arc<Runtime>, scope: ObservabilityScope) -> Self {
        Self {
            runtime,
            scope,
            session_runs: Mutex::new(HashSet::new()),
        }
    }

    fn store(&self) -> Arc<dyn EventStore> {
        Arc::clone(self.runtime.bus().store())
    }

    /// Whether `run_id` is visible under the active scope.
    fn in_scope(&self, run_id: &str) -> bool {
        match self.scope {
            ObservabilityScope::All => true,
            ObservabilityScope::Session => self
                .session_runs
                .lock()
                .map(|s| s.contains(run_id))
                .unwrap_or(false),
        }
    }

    async fn run_list(&self, args: &Value) -> Result<Value, RpcError> {
        let pipeline = args.get("pipeline").and_then(Value::as_str);
        let status = args.get("status").and_then(Value::as_str);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;

        let mut runs = list_runs(self.store(), pipeline)
            .await
            .map_err(|e| RpcError::new(ERR_INTERNAL, e))?;

        if self.scope == ObservabilityScope::Session {
            let session = self.session_runs.lock().unwrap();
            runs.retain(|r| session.contains(&r.stream_id));
        }
        if let Some(want) = status.map(status_filter).transpose()? {
            runs.retain(|r| r.status == want);
        }
        runs.truncate(limit);

        let arr: Vec<Value> = runs
            .iter()
            .map(|r| {
                json!({
                    "run_id": r.stream_id,
                    "pipeline": r.pipeline,
                    "status": status_str(&r.status),
                    "started_at": r.started_at.to_rfc3339(),
                    "duration_ms": r.duration.map(|d| d.num_milliseconds()),
                    "error": r.error,
                })
            })
            .collect();
        Ok(json!({ "runs": arr }))
    }

    async fn run_get(&self, args: &Value) -> Result<Value, RpcError> {
        let run_id = require_str(args, "run_id")?;
        self.require_scope(&run_id)?;

        let events = self
            .store()
            .read_stream(&run_id, 0)
            .await
            .map_err(|e| RpcError::new(ERR_INTERNAL, e.to_string()))?;
        if events.is_empty() {
            return Err(RpcError::new(ERR_INVALID_PARAMS, format!("unknown run `{run_id}`")));
        }

        let pipeline = events.iter().find_map(|e| match &e.kind {
            EventKind::PipelineRequested { pipeline, .. } => Some(pipeline.clone()),
            _ => None,
        });
        let Some(pipeline) = pipeline else {
            return Err(RpcError::new(
                ERR_INVALID_PARAMS,
                format!("stream `{run_id}` is not a pipeline run"),
            ));
        };

        let total_steps = self
            .runtime
            .workspace()
            .pipelines
            .get(&pipeline)
            .map(|p| p.steps.len());

        let completed = events.iter().find_map(|e| match &e.kind {
            EventKind::PipelineCompleted { success, error, .. } => Some((*success, error.clone())),
            _ => None,
        });
        let (status, error) = match &completed {
            Some((true, _)) => ("ok", None),
            Some((false, err)) => ("failed", err.clone()),
            None => ("running", None),
        };

        let started_at = events.first().map(|e| e.timestamp.to_rfc3339());
        let finished_at = events.iter().rev().find_map(|e| match &e.kind {
            EventKind::PipelineCompleted { .. } => Some(e.timestamp.to_rfc3339()),
            _ => None,
        });
        let current_step = current_step(&events);

        Ok(json!({
            "run_id": run_id,
            "pipeline": pipeline,
            "status": status,
            "current_step": current_step,
            "total_steps": total_steps,
            "started_at": started_at,
            "finished_at": finished_at,
            "error": error,
        }))
    }

    async fn run_events(&self, args: &Value) -> Result<Value, RpcError> {
        let run_id = require_str(args, "run_id")?;
        self.require_scope(&run_id)?;
        let since_seq = args.get("since_seq").and_then(Value::as_u64).unwrap_or(0);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
        let types: Option<Vec<String>> = args.get("types").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });

        let prefix = format!("{run_id}:step:");
        let store = self.store();
        let mut out: Vec<Value> = Vec::new();
        let mut cursor = since_seq;
        'pages: loop {
            let batch = store
                .tail(cursor, 256)
                .await
                .map_err(|e| RpcError::new(ERR_INTERNAL, e.to_string()))?;
            if batch.is_empty() {
                break;
            }
            for t in &batch {
                let sid = &t.event.stream_id;
                if sid != &run_id && !sid.starts_with(&prefix) {
                    continue;
                }
                let ty = t.event.kind_tag();
                if let Some(filter) = &types {
                    if !filter.iter().any(|x| x == ty) {
                        continue;
                    }
                }
                let step_id = sid.strip_prefix(&prefix).map(str::to_string);
                let fe = format_event(&t.event);
                out.push(json!({
                    "seq": t.global_seq,
                    "ts": t.event.timestamp.to_rfc3339(),
                    "type": ty,
                    "step_id": step_id,
                    "summary": format!("{} — {}", fe.label, fe.short_detail),
                }));
                if out.len() >= limit {
                    break 'pages;
                }
            }
            let last = batch.last().expect("non-empty").global_seq;
            if batch.len() < 256 {
                break;
            }
            cursor = last;
        }
        Ok(json!({ "events": out, "count": out.len() }))
    }

    async fn run_step_io(&self, args: &Value) -> Result<Value, RpcError> {
        let run_id = require_str(args, "run_id")?;
        let step_id = require_str(args, "step_id")?;
        self.require_scope(&run_id)?;

        let store = self.store();
        let step_stream = format!("{run_id}:step:{step_id}");
        let step_events = store
            .read_stream(&step_stream, 0)
            .await
            .map_err(|e| RpcError::new(ERR_INTERNAL, e.to_string()))?;

        // Pipeline + inputs from the run stream.
        let run_events = store
            .read_stream(&run_id, 0)
            .await
            .map_err(|e| RpcError::new(ERR_INTERNAL, e.to_string()))?;
        let (pipeline, inputs) = run_events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::PipelineRequested { pipeline, inputs } => {
                    Some((pipeline.clone(), inputs.clone()))
                }
                _ => None,
            })
            .ok_or_else(|| {
                RpcError::new(ERR_INVALID_PARAMS, format!("unknown run `{run_id}`"))
            })?;

        // Locate the step + its agent in the workspace config.
        let workspace = self.runtime.workspace();
        let step = workspace
            .pipelines
            .get(&pipeline)
            .and_then(|p| p.steps.iter().find(|s| s.id == step_id));

        if step_events.is_empty() {
            // No recorded execution — skipped, still pending, or unknown step.
            let skipped = run_events.iter().any(|e| {
                matches!(&e.kind, EventKind::StepSkipped { step_id: s, .. } if s == &step_id)
            });
            let note = if step.is_none() {
                "no such step in this pipeline"
            } else if skipped {
                "step was skipped (no execution recorded)"
            } else {
                "no execution recorded for this step yet"
            };
            return Ok(json!({
                "run_id": run_id,
                "step_id": step_id,
                "note": note,
                "prompt_in": [],
                "output": Value::Null,
            }));
        }

        let (agent_name, system_prompt, task) = match step.map(|s| &s.kind) {
            Some(PipelineStepKind::Agent { agent, task }) => {
                let system = workspace
                    .agents
                    .get(agent)
                    .and_then(|a| a.system_prompt.clone())
                    .unwrap_or_default();
                let prior = self.reconstruct_step_outputs(&run_id).await;
                let resolved = resolve_task_template(task, &inputs, &prior);
                (Some(agent.clone()), system, resolved)
            }
            _ => (None, String::new(), String::new()),
        };

        let messages = events_to_messages(&step_events);
        let mut prompt_in: Vec<Value> = Vec::new();
        if !system_prompt.is_empty() {
            prompt_in.push(json!({ "role": "system", "content": system_prompt }));
        }
        if !task.is_empty() {
            prompt_in.push(json!({ "role": "user", "content": task }));
        }
        prompt_in.extend(messages.iter().map(message_json));

        let output = step_output(&messages);

        Ok(json!({
            "run_id": run_id,
            "step_id": step_id,
            "agent": agent_name,
            "prompt_in": prompt_in,
            "output": output,
            "note": "prompt_in is reconstructed unmasked from the step substream; \
                     the exact masked context sent per LLM iteration is not stored verbatim",
        }))
    }

    /// Best-effort: final text output of every recorded step on this run, for
    /// resolving `${steps.<id>.output}` references in a step's task template.
    async fn reconstruct_step_outputs(&self, run_id: &str) -> HashMap<String, String> {
        let prefix = format!("{run_id}:step:");
        let store = self.store();
        let mut out = HashMap::new();
        let streams = match store.list_streams().await {
            Ok(s) => s,
            Err(_) => return out,
        };
        for (sid, _) in streams {
            let Some(step) = sid.strip_prefix(&prefix) else {
                continue;
            };
            if let Ok(events) = store.read_stream(&sid, 0).await {
                if let Some(text) = step_output(&events_to_messages(&events)) {
                    out.insert(step.to_string(), text);
                }
            }
        }
        out
    }

    fn require_scope(&self, run_id: &str) -> Result<(), RpcError> {
        if self.in_scope(run_id) {
            Ok(())
        } else {
            Err(RpcError::new(
                ERR_INVALID_PARAMS,
                format!(
                    "run `{run_id}` is not visible in the current observability scope \
                     (this serve session did not start it; pass --observability-scope all to widen)"
                ),
            ))
        }
    }
}

#[async_trait]
impl ObservabilityProvider for CliObservability {
    fn tool_descriptors(&self) -> Vec<Value> {
        vec![
            json!({
                "name": TOOL_LIST,
                "description": "List recent zymi pipeline runs (newest first). Start here when \
                    investigating a run; then narrow with zymi.runs.get / events / step_io.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pipeline": { "type": "string", "description": "Filter to one pipeline name" },
                        "status": { "type": "string", "enum": ["running", "ok", "failed"] },
                        "limit": { "type": "integer", "default": 20 }
                    }
                }
            }),
            json!({
                "name": TOOL_GET,
                "description": "Current state of one run: status, current_step, total_steps, \
                    timing, and error if failed.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "run_id": { "type": "string" } },
                    "required": ["run_id"]
                }
            }),
            json!({
                "name": TOOL_EVENTS,
                "description": "Flat, compact event trace for a run (no nested payloads). Use \
                    since_seq/types/limit to narrow. Reach for step_io only after locating the \
                    failing step here.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "run_id": { "type": "string" },
                        "since_seq": { "type": "integer", "description": "Only events with global seq > this" },
                        "types": { "type": "array", "items": { "type": "string" }, "description": "Filter to these event type tags" },
                        "limit": { "type": "integer", "default": 50 }
                    },
                    "required": ["run_id"]
                }
            }),
            json!({
                "name": TOOL_STEP_IO,
                "description": "Full reconstructed prompt-in / output-out for ONE step. Expensive \
                    (large) — call only after narrowing to a specific step via events.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "run_id": { "type": "string" },
                        "step_id": { "type": "string" }
                    },
                    "required": ["run_id", "step_id"]
                }
            }),
        ]
    }

    fn handles(&self, tool_name: &str) -> bool {
        matches!(tool_name, TOOL_LIST | TOOL_GET | TOOL_EVENTS | TOOL_STEP_IO)
    }

    async fn call(&self, tool_name: &str, arguments: &Value) -> Result<Value, RpcError> {
        let result = match tool_name {
            TOOL_LIST => self.run_list(arguments).await,
            TOOL_GET => self.run_get(arguments).await,
            TOOL_EVENTS => self.run_events(arguments).await,
            TOOL_STEP_IO => self.run_step_io(arguments).await,
            other => Err(RpcError::new(
                ERR_INVALID_PARAMS,
                format!("unknown observability tool `{other}`"),
            )),
        }?;
        // Pack as a CallToolResult text block (pretty JSON).
        let text = serde_json::to_string_pretty(&result)
            .unwrap_or_else(|_| result.to_string());
        Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        }))
    }

    fn record_run(&self, run_id: &str) {
        if let Ok(mut s) = self.session_runs.lock() {
            s.insert(run_id.to_string());
        }
    }
}

fn require_str(args: &Value, key: &str) -> Result<String, RpcError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RpcError::new(ERR_INVALID_PARAMS, format!("missing `{key}`")))
}

fn status_str(s: &RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "running",
        RunStatus::Ok => "ok",
        RunStatus::Failed => "failed",
    }
}

fn status_filter(s: &str) -> Result<RunStatus, RpcError> {
    match s {
        "running" => Ok(RunStatus::Running),
        "ok" => Ok(RunStatus::Ok),
        "failed" => Ok(RunStatus::Failed),
        other => Err(RpcError::new(
            ERR_INVALID_PARAMS,
            format!("invalid status `{other}` (expected running|ok|failed)"),
        )),
    }
}

/// Last started workflow node not yet completed; `None` once the pipeline
/// has completed or no node has started.
fn current_step(events: &[Event]) -> Option<String> {
    if events
        .iter()
        .any(|e| matches!(e.kind, EventKind::PipelineCompleted { .. }))
    {
        return None;
    }
    let mut completed: HashSet<&str> = HashSet::new();
    let mut last_open: Option<&str> = None;
    for e in events {
        match &e.kind {
            EventKind::WorkflowNodeCompleted { node_id, .. } => {
                completed.insert(node_id.as_str());
            }
            EventKind::WorkflowNodeStarted { node_id, .. } => {
                last_open = Some(node_id.as_str());
            }
            _ => {}
        }
    }
    last_open
        .filter(|n| !completed.contains(n))
        .map(str::to_string)
}

/// Final text output of a step: last assistant content, else last tool result.
fn step_output(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|m| match m {
        Message::Assistant { content: Some(c), .. } if !c.is_empty() => Some(c.clone()),
        Message::ToolResult { content, .. } => Some(content.clone()),
        _ => None,
    })
}

fn message_json(m: &Message) -> Value {
    match m {
        Message::System(s) => json!({ "role": "system", "content": s }),
        Message::User(s) => json!({ "role": "user", "content": s }),
        Message::UserMultimodal { .. } => json!({ "role": "user", "content": "[multimodal]" }),
        Message::Assistant { content, tool_calls } => json!({
            "role": "assistant",
            "content": content,
            "tool_calls": tool_calls.iter().map(|tc| json!({
                "id": tc.id, "name": tc.name, "arguments": tc.arguments
            })).collect::<Vec<_>>(),
        }),
        Message::ToolResult { tool_call_id, content } => json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": content,
        }),
    }
}
