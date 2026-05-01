//! `RunPipeline` command handler.
//!
//! This is the canonical pipeline execution path. Both `zymi run` and
//! `zymi serve` dispatch [`crate::commands::RunPipeline`] commands at this
//! handler via a [`Runtime`]. The body is the same loop that used to live
//! in `engine::run_pipeline_for_request`, with three differences:
//!
//! 1. All infrastructure (store, bus, provider, contracts, orchestrator,
//!    approval handler) comes from the [`Runtime`] instead of being built
//!    inline.
//! 2. Approved tool calls go through [`crate::runtime::ActionExecutor`]
//!    instead of calling `execute_builtin_tool` directly. The engine no
//!    longer owns built-in tool details.
//! 3. The `correlation_id` and `stream_id` come from the command, so a
//!    `PipelineRequested` event delivered cross-process can be answered on
//!    the same stream the requester used.
//!
//! The deprecated `engine::run_pipeline*` entrypoints are now thin wrappers
//! that build a `Runtime` and call this function. New callers should
//! construct a `Runtime` once per project and dispatch
//! [`crate::commands::RunPipeline`] directly.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use uuid::Uuid;

use crate::commands::RunPipeline;
use crate::config::AgentConfig;
use crate::engine::tools::{new_memory_store, MemoryStore};
use crate::runtime::ToolCatalog;
use crate::runtime::context_builder::{ContextBuilder, ContextConfig};
use crate::runtime::context_window::approx_chars;
use crate::esaa::orchestrator::{Orchestrator, OrchestratorResult};
use crate::events::bus::EventBus;
use crate::events::store::EventStore;
use crate::events::{Event, EventKind};
use crate::llm::{ChatRequest, ChatResponse, LlmProvider};
use crate::runtime::{ActionContext, ActionExecutor, Runtime};
use crate::types::Message;

/// Result of a single pipeline step execution.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub step_id: String,
    pub agent_name: String,
    pub output: String,
    pub iterations: usize,
    pub success: bool,
}

/// Result of the full pipeline execution.
#[derive(Debug)]
pub struct PipelineResult {
    pub pipeline_name: String,
    pub step_results: HashMap<String, StepResult>,
    pub final_output: Option<String>,
    pub success: bool,
}

/// Execute a [`RunPipeline`] command against the given runtime.
pub async fn handle(rt: &Runtime, cmd: RunPipeline) -> Result<PipelineResult, String> {
    let workspace = rt.workspace();

    let pipeline = workspace
        .pipelines
        .get(&cmd.pipeline_name)
        .ok_or_else(|| {
            let available: Vec<&str> =
                workspace.pipelines.keys().map(|s| s.as_str()).collect();
            format!(
                "pipeline '{}' not found. Available: {}",
                cmd.pipeline_name,
                if available.is_empty() {
                    "(none)".to_string()
                } else {
                    available.join(", ")
                }
            )
        })?
        .clone();

    let plan = crate::config::build_execution_plan(&pipeline)
        .map_err(|e| format!("failed to build execution plan: {e}"))?;

    let memory = new_memory_store(Arc::clone(rt.bus()));
    let is_local_cli_run = cmd.stream_id.is_none();
    let resume = cmd.resume.clone();
    let is_resume = resume.is_some();
    let stream_id = cmd
        .stream_id
        .clone()
        .unwrap_or_else(|| format!("pipeline-{}-{}", pipeline.name, Uuid::new_v4()));
    let correlation_id = cmd.correlation_id;

    // For local CLI runs, emit a `PipelineRequested` marker so `zymi runs`
    // / `zymi observe` can identify the run the same way as cross-process
    // runs (which already receive one from the event router). Resume runs
    // have their PipelineRequested + WorkflowStarted + frozen-step bootstrap
    // emitted by the resume orchestrator before dispatch (ADR-0018).
    if is_local_cli_run {
        emit_event(
            rt.bus(),
            &stream_id,
            correlation_id,
            EventKind::PipelineRequested {
                pipeline: pipeline.name.clone(),
                inputs: cmd.inputs.clone(),
            },
        )
        .await;
    }

    if !is_resume {
        emit_event(
            rt.bus(),
            &stream_id,
            correlation_id,
            EventKind::WorkflowStarted {
                user_message: format!("pipeline: {}", pipeline.name),
                node_count: plan.step_count(),
            },
        )
        .await;
    }

    if is_local_cli_run {
        println!(
            "  Execution plan: {} steps, {} levels\n",
            plan.step_count(),
            plan.levels.len()
        );
    }

    let mut step_outputs: HashMap<String, String> = HashMap::new();
    let mut all_results: HashMap<String, StepResult> = HashMap::new();
    let mut overall_success = true;

    if let Some(ctx) = &resume {
        for (step_id, output) in &ctx.frozen_outputs {
            step_outputs.insert(step_id.clone(), output.clone());
            all_results.insert(
                step_id.clone(),
                StepResult {
                    step_id: step_id.clone(),
                    agent_name: pipeline
                        .steps
                        .iter()
                        .find(|s| &s.id == step_id)
                        .map(|s| s.agent.clone())
                        .unwrap_or_default(),
                    output: output.clone(),
                    iterations: 0,
                    success: true,
                },
            );
        }
    }

    for (level_idx, level) in plan.levels.iter().enumerate() {
        let level_names: Vec<&str> = level.iter().map(|s| s.as_str()).collect();
        if is_local_cli_run {
            if level.len() == 1 {
                println!("  Level {}: {}", level_idx + 1, level_names[0]);
            } else {
                println!(
                    "  Level {} (parallel): {}",
                    level_idx + 1,
                    level_names.join(", ")
                );
            }
        }

        let mut handles = Vec::new();

        for step_id in level {
            if let Some(ctx) = &resume {
                if ctx.frozen_step_ids.contains(step_id) {
                    if is_local_cli_run {
                        println!("    [{step_id}] frozen (resumed from parent)");
                    }
                    continue;
                }
            }

            let step = pipeline
                .steps
                .iter()
                .find(|s| &s.id == step_id)
                .ok_or_else(|| format!("step '{step_id}' not found in pipeline config"))?;

            let agent = workspace
                .agents
                .get(&step.agent)
                .ok_or_else(|| {
                    format!("agent '{}' not found for step '{step_id}'", step.agent)
                })?;

            let task = resolve_task_template(&step.task, &cmd.inputs, &step_outputs);
            let context = build_step_context(step_id, &step.depends_on, &step_outputs);

            let step_id = step_id.clone();
            let agent = agent.clone();
            let provider = Arc::clone(rt.provider());
            let orchestrator = Arc::clone(rt.orchestrator());
            let bus = Arc::clone(rt.bus());
            let store = Arc::clone(rt.store());
            let action_executor = Arc::clone(rt.action_executor());
            let tool_catalog = Arc::clone(rt.tool_catalog());
            let memory = memory.clone();
            let stream_id = stream_id.clone();
            let project_root = rt.project_root().to_path_buf();
            let defaults = workspace.project.defaults.clone();
            let approval_channel = rt.approval_channel().map(|s| s.to_string());
            let approval_timeout = rt.approval_timeout();
            let context_config: ContextConfig = workspace
                .project
                .runtime
                .as_ref()
                .map(|r| r.context.clone().into())
                .unwrap_or_default();

            handles.push(tokio::spawn(async move {
                run_agent_step(
                    &step_id,
                    &agent,
                    &task,
                    &context,
                    Arc::clone(&provider),
                    &orchestrator,
                    Arc::clone(&bus),
                    &store,
                    action_executor.as_ref(),
                    &tool_catalog,
                    &memory,
                    &stream_id,
                    correlation_id,
                    &project_root,
                    defaults.max_iterations,
                    approval_channel,
                    approval_timeout,
                    context_config,
                )
                .await
            }));
        }

        for handle in handles {
            let result = handle
                .await
                .map_err(|e| format!("step task panicked: {e}"))??;

            if is_local_cli_run {
                println!(
                    "    [{}] {} ({} iterations)",
                    result.step_id,
                    if result.success { "done" } else { "FAILED" },
                    result.iterations
                );
            }

            if !result.success {
                overall_success = false;
            }

            step_outputs.insert(result.step_id.clone(), result.output.clone());
            all_results.insert(result.step_id.clone(), result);
        }

        if is_local_cli_run {
            println!();
        }
    }

    let final_output = pipeline
        .output
        .as_ref()
        .and_then(|o| step_outputs.get(&o.step).cloned())
        .or_else(|| {
            plan.levels
                .last()
                .and_then(|level| level.last())
                .and_then(|step_id| step_outputs.get(step_id).cloned())
        });

    emit_event(
        rt.bus(),
        &stream_id,
        correlation_id,
        EventKind::WorkflowCompleted {
            success: overall_success,
        },
    )
    .await;

    // ADR-0021: declarative `outputs:` (e.g. http_post) subscribe to
    // `ResponseReady`. Translate a successful pipeline's final output into
    // that shape so connector-driven chat loops (Telegram, Slack, …) round-
    // trip without extra glue. Internal state tracking uses
    // `PipelineCompleted` / `WorkflowCompleted`; `ResponseReady` is purely
    // the external contract.
    if overall_success {
        if let Some(content) = final_output.as_deref() {
            emit_event(
                rt.bus(),
                &stream_id,
                correlation_id,
                EventKind::ResponseReady {
                    conversation_id: stream_id.clone(),
                    content: content.to_string(),
                },
            )
            .await;
        }
    }

    // Matching PipelineCompleted for the local CLI marker above. The serve
    // path publishes its own PipelineCompleted at the event router level, so
    // we skip it here to avoid duplicate completions on the same stream.
    // Resume runs are launched directly from CLI (no router), so we emit the
    // completion envelope here as well (ADR-0018).
    if is_local_cli_run || is_resume {
        emit_event(
            rt.bus(),
            &stream_id,
            correlation_id,
            EventKind::PipelineCompleted {
                pipeline: pipeline.name.clone(),
                success: overall_success,
                final_output: final_output.clone(),
                error: None,
            },
        )
        .await;
    }

    // Close the persistent shell session for this stream (ADR-0015 §3).
    rt.shell_pool()
        .close_session(&stream_id, "workflow_end")
        .await;

    Ok(PipelineResult {
        pipeline_name: pipeline.name.clone(),
        step_results: all_results,
        final_output,
        success: overall_success,
    })
}

/// Run a single agent step: LLM loop with tool calls.
///
/// Context is built from the event store on each iteration via
/// [`ContextBuilder`] (ADR-0016 §4/§6). There is no in-memory
/// `Vec<Message>` — the event log is the source of truth.
#[allow(clippy::too_many_arguments)]
async fn run_agent_step(
    step_id: &str,
    agent: &AgentConfig,
    task: &str,
    context: &str,
    provider: Arc<dyn LlmProvider>,
    orchestrator: &Orchestrator,
    bus: Arc<EventBus>,
    store: &Arc<dyn EventStore>,
    action_executor: &dyn ActionExecutor,
    tool_catalog: &ToolCatalog,
    memory: &MemoryStore,
    stream_id: &str,
    correlation_id: Uuid,
    project_root: &std::path::Path,
    default_max_iterations: usize,
    approval_channel: Option<String>,
    approval_timeout: std::time::Duration,
    context_config: ContextConfig,
) -> Result<StepResult, String> {
    let max_iterations = agent.max_iterations.unwrap_or(default_max_iterations);
    let tool_defs = tool_catalog.definitions_for_agent(&agent.tools);

    // Per-step sub-stream isolates this step's LLM/tool events from other
    // parallel steps on the same pipeline stream (ADR-0016 §6).
    let step_stream_id = format!("{stream_id}:step:{step_id}");

    let system_prompt = agent
        .system_prompt
        .clone()
        .unwrap_or_else(|| format!("You are the '{}' agent.", agent.name));

    let user_msg = if context.is_empty() {
        task.to_string()
    } else {
        format!("{task}\n\n---\nContext from previous steps:\n{context}")
    };

    // The ContextBuilder reads from the event store and reconstructs the
    // conversation on every iteration — no mutable Vec<Message>.
    let context_builder = ContextBuilder::new(
        Arc::clone(store),
        memory.clone(),
        step_stream_id.clone(),
        system_prompt,
        user_msg,
        context_config,
    )
    .with_compaction(Arc::clone(&provider), Arc::clone(&bus));

    // Reborrow for the rest of the function (emit_event expects &EventBus).
    let bus: &EventBus = &bus;

    // Workflow-level event on the pipeline stream.
    emit_event(
        bus,
        stream_id,
        correlation_id,
        EventKind::WorkflowNodeStarted {
            node_id: step_id.to_string(),
            description: format!("agent={}, task=\"{}\"", agent.name, truncate(task, 80)),
        },
    )
    .await;

    let mut iteration = 0;
    let mut final_output = String::new();

    loop {
        iteration += 1;
        if iteration > max_iterations {
            let success = !final_output.is_empty();
            let output = if final_output.is_empty() {
                format!("[max iterations ({max_iterations}) reached without final answer]")
            } else {
                final_output
            };
            return Ok(StepResult {
                step_id: step_id.to_string(),
                agent_name: agent.name.clone(),
                output,
                iterations: iteration - 1,
                success,
            });
        }

        // Build context from the event store (ADR-0016 §4).
        // Includes prefix (Layer A), memory snapshot (Layer B), and
        // observation-masked tail (Layer C).
        let messages = context_builder
            .build()
            .await
            .map_err(|e| format!("[{step_id}] context build failed: {e}"))?;

        emit_event(
            bus,
            &step_stream_id,
            correlation_id,
            EventKind::LlmCallStarted {
                iteration,
                message_count: messages.len(),
                approx_context_chars: approx_chars(&messages),
            },
        )
        .await;

        let request = ChatRequest {
            messages,
            tools: tool_defs.clone(),
            temperature: Some(0.7),
            max_tokens: Some(4096),
        };

        let response: ChatResponse = provider
            .chat_completion(&request)
            .await
            .map_err(|e| format!("[{step_id}] LLM call failed: {e}"))?;

        match &response.message {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let has_tool_calls = !tool_calls.is_empty();

                // Enriched event: stores the full response_message (ADR-0016 §1a).
                // The next iteration's context_builder.build() reads it back.
                emit_event(
                    bus,
                    &step_stream_id,
                    correlation_id,
                    EventKind::LlmCallCompleted {
                        response_message: Some(response.message.clone()),
                        has_tool_calls,
                        usage: Some(response.usage.clone()),
                        content_preview: content.as_ref().map(|c| truncate(c, 100).to_string()),
                    },
                )
                .await;

                if !has_tool_calls {
                    final_output = content.clone().unwrap_or_default();

                    emit_event(
                        bus,
                        stream_id,
                        correlation_id,
                        EventKind::WorkflowNodeCompleted {
                            node_id: step_id.to_string(),
                            success: true,
                        },
                    )
                    .await;

                    return Ok(StepResult {
                        step_id: step_id.to_string(),
                        agent_name: agent.name.clone(),
                        output: final_output,
                        iterations: iteration,
                        success: true,
                    });
                }

                // No messages.push(response.message) — the LlmCallCompleted
                // event IS the record. The next build() reads it back.

                for tc in tool_calls {
                    let start = Instant::now();

                    emit_event(
                        bus,
                        &step_stream_id,
                        correlation_id,
                        EventKind::ToolCallRequested {
                            tool_name: tc.name.clone(),
                            arguments: truncate(&tc.arguments, 200).to_string(),
                            call_id: tc.id.clone(),
                        },
                    )
                    .await;

                    let intention = tool_catalog.intention(&tc.name, &tc.arguments);
                    let verdict = orchestrator
                        .process_intention(
                            &intention,
                            stream_id,
                            correlation_id,
                            crate::esaa::orchestrator::ApprovalContext {
                                channel: approval_channel.as_deref(),
                                timeout: approval_timeout,
                            },
                        )
                        .await;

                    let tool_result = match verdict {
                        OrchestratorResult::Approved => {
                            let ctx = ActionContext {
                                project_root,
                                memory,
                                stream_id,
                                correlation_id,
                            };
                            match action_executor.execute(&tc.name, &tc.arguments, &ctx).await {
                                Ok(output) => output,
                                Err(e) => format!("[tool error] {e}"),
                            }
                        }
                        OrchestratorResult::Denied { reason } => {
                            format!("[denied by policy] {reason}")
                        }
                        OrchestratorResult::HumanRejected => "[rejected by human]".to_string(),
                        OrchestratorResult::NoApprovalHandler => {
                            "[requires approval but no approval handler configured]".to_string()
                        }
                    };

                    let duration_ms = start.elapsed().as_millis() as u64;
                    let is_error = tool_result.starts_with("[tool error]")
                        || tool_result.starts_with("[denied")
                        || tool_result.starts_with("[rejected")
                        || tool_result.starts_with("[requires approval");

                    // Enriched event: stores full tool result (ADR-0016 §1a).
                    // The next build() reads it back as a ToolResult message.
                    emit_event(
                        bus,
                        &step_stream_id,
                        correlation_id,
                        EventKind::ToolCallCompleted {
                            call_id: tc.id.clone(),
                            result: tool_result.clone(),
                            result_preview: truncate(&tool_result, 200).to_string(),
                            is_error,
                            duration_ms,
                        },
                    )
                    .await;

                    // No messages.push(ToolResult) — the event IS the record.
                }
            }
            _ => {
                return Err(format!("[{step_id}] unexpected message type from LLM"));
            }
        }
    }
}

/// Resolve `${inputs.*}` and `${steps.<id>.output}` in a task template.
fn resolve_task_template(
    task: &str,
    inputs: &HashMap<String, String>,
    step_outputs: &HashMap<String, String>,
) -> String {
    let mut result = task.to_string();

    for (key, value) in inputs {
        result = result.replace(&format!("${{inputs.{key}}}"), value);
    }

    for (step_id, output) in step_outputs {
        result = result.replace(
            &format!("${{steps.{step_id}.output}}"),
            truncate(output, 2000),
        );
    }

    result
}

/// Build context string from dependency step outputs.
fn build_step_context(
    _step_id: &str,
    depends_on: &[String],
    step_outputs: &HashMap<String, String>,
) -> String {
    if depends_on.is_empty() {
        return String::new();
    }

    let mut context = String::new();
    for dep in depends_on {
        if let Some(output) = step_outputs.get(dep) {
            context.push_str(&format!("## Output from step '{dep}':\n{output}\n\n"));
        }
    }
    context
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let end = s.floor_char_boundary(max);
        &s[..end]
    }
}

async fn emit_event(bus: &EventBus, stream_id: &str, correlation_id: Uuid, kind: EventKind) {
    let event = Event::new(stream_id.to_string(), kind, "engine".into())
        .with_correlation(correlation_id);
    if let Err(e) = bus.publish(event).await {
        log::warn!("Engine failed to emit event: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_template_inputs() {
        let mut inputs = HashMap::new();
        inputs.insert("query".into(), "rust async".into());

        let result =
            resolve_task_template("Search for: ${inputs.query}", &inputs, &HashMap::new());
        assert_eq!(result, "Search for: rust async");
    }

    #[test]
    fn resolve_template_step_outputs() {
        let mut outputs = HashMap::new();
        outputs.insert("search".into(), "found 10 results".into());

        let result = resolve_task_template(
            "Summarize: ${steps.search.output}",
            &HashMap::new(),
            &outputs,
        );
        assert_eq!(result, "Summarize: found 10 results");
    }

    #[test]
    fn build_context_empty_deps() {
        let ctx = build_step_context("s1", &[], &HashMap::new());
        assert!(ctx.is_empty());
    }

    #[test]
    fn build_context_with_deps() {
        let mut outputs = HashMap::new();
        outputs.insert("search".into(), "results here".into());

        let ctx = build_step_context("summarize", &["search".into()], &outputs);
        assert!(ctx.contains("Output from step 'search'"));
        assert!(ctx.contains("results here"));
    }

    #[test]
    fn catalog_intention_shell() {
        let catalog = ToolCatalog::builtin_only();
        let intention = catalog.intention("execute_shell_command", r#"{"command":"ls"}"#);
        assert_eq!(intention.tag(), "execute_shell_command");
    }

    #[test]
    fn catalog_intention_custom() {
        let catalog = ToolCatalog::builtin_only();
        let intention = catalog.intention("my_custom_tool", r#"{"arg":"val"}"#);
        assert_eq!(intention.tag(), "call_custom_tool");
    }
}
