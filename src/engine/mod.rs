pub mod tools;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use uuid::Uuid;

use crate::config::{AgentConfig, PipelineConfig, WorkspaceConfig};
use crate::esaa::contracts::ContractEngine;
use crate::esaa::orchestrator::{Orchestrator, OrchestratorResult};
use crate::esaa::Intention;
use crate::events::bus::EventBus;
use crate::events::store::{open_store, EventStore, StoreBackend};
use crate::events::{Event, EventKind};
use crate::llm::{self, ChatRequest, ChatResponse, LlmProvider};
use crate::policy::PolicyEngine;
use crate::types::Message;

use tools::{execute_builtin_tool, tool_definitions_for_agent, MemoryStore};

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

/// Execute a full pipeline end-to-end. Used by `zymi run` — owns its own
/// store/bus and generates a fresh correlation_id.
pub async fn run_pipeline(
    workspace: &WorkspaceConfig,
    pipeline: &PipelineConfig,
    project_root: &Path,
    inputs: &HashMap<String, String>,
) -> Result<PipelineResult, String> {
    // 1. Create infrastructure
    let store_dir = project_root.join(".zymi");
    std::fs::create_dir_all(&store_dir)
        .map_err(|e| format!("failed to create .zymi directory: {e}"))?;
    let db_path = store_dir.join("events.db");
    let store = open_store(StoreBackend::Sqlite { path: db_path })
        .map_err(|e| format!("failed to create event store: {e}"))?;
    let bus = Arc::new(EventBus::new(Arc::clone(&store)));

    run_pipeline_for_request(
        workspace,
        pipeline,
        project_root,
        inputs,
        bus,
        store,
        Uuid::new_v4(),
    )
    .await
}

/// Execute a pipeline using a pre-existing bus/store and a caller-supplied
/// correlation_id. This is the entry point used by `zymi serve` so that the
/// store and bus are shared across many requests, and correlation_ids match
/// the originating `PipelineRequested` events.
pub async fn run_pipeline_for_request(
    workspace: &WorkspaceConfig,
    pipeline: &PipelineConfig,
    project_root: &Path,
    inputs: &HashMap<String, String>,
    bus: Arc<EventBus>,
    _store: Arc<dyn EventStore>,
    correlation_id: Uuid,
) -> Result<PipelineResult, String> {
    let policy = Arc::new(PolicyEngine::new(workspace.project.policy.clone()));
    let contracts = Arc::new(ContractEngine::new(
        policy,
        workspace.project.contracts.file_write.clone(),
    ));
    let orchestrator = Arc::new(Orchestrator::new(contracts, bus.clone()));

    // 2. Create LLM provider
    let llm_config = workspace
        .project
        .llm
        .as_ref()
        .ok_or("no 'llm' section in project.yml — configure a provider to run pipelines")?;
    let provider: Arc<dyn LlmProvider> = Arc::from(
        llm::create_provider(llm_config).map_err(|e| format!("failed to create LLM provider: {e}"))?,
    );

    // 3. Build execution plan
    let plan = crate::config::build_execution_plan(pipeline)
        .map_err(|e| format!("failed to build execution plan: {e}"))?;

    let memory = tools::new_memory_store();
    let stream_id = format!("pipeline-{}-{}", pipeline.name, Uuid::new_v4());

    // Emit pipeline start event
    emit_event(
        &bus,
        &stream_id,
        correlation_id,
        EventKind::WorkflowStarted {
            user_message: format!("pipeline: {}", pipeline.name),
            node_count: plan.step_count(),
        },
    )
    .await;

    println!(
        "  Execution plan: {} steps, {} levels\n",
        plan.step_count(),
        plan.levels.len()
    );

    // 4. Execute levels sequentially, steps within a level in parallel
    let mut step_outputs: HashMap<String, String> = HashMap::new();
    let mut all_results: HashMap<String, StepResult> = HashMap::new();
    let mut overall_success = true;

    for (level_idx, level) in plan.levels.iter().enumerate() {
        let level_names: Vec<&str> = level.iter().map(|s| s.as_str()).collect();
        if level.len() == 1 {
            println!("  Level {}: {}", level_idx + 1, level_names[0]);
        } else {
            println!(
                "  Level {} (parallel): {}",
                level_idx + 1,
                level_names.join(", ")
            );
        }

        let mut handles = Vec::new();

        for step_id in level {
            let step = pipeline
                .steps
                .iter()
                .find(|s| &s.id == step_id)
                .ok_or_else(|| format!("step '{step_id}' not found in pipeline config"))?;

            let agent = workspace
                .agents
                .get(&step.agent)
                .ok_or_else(|| format!("agent '{}' not found for step '{step_id}'", step.agent))?;

            // Resolve task template with inputs and prior step outputs
            let task = resolve_task_template(&step.task, inputs, &step_outputs);

            // Build context from dependency outputs
            let context = build_step_context(step_id, &step.depends_on, &step_outputs);

            let step_id = step_id.clone();
            let agent = agent.clone();
            let provider = provider.clone();
            let orchestrator = orchestrator.clone();
            let bus = bus.clone();
            let memory = memory.clone();
            let stream_id = stream_id.clone();
            let project_root = project_root.to_path_buf();
            let defaults = workspace.project.defaults.clone();

            handles.push(tokio::spawn(async move {
                run_agent_step(
                    &step_id,
                    &agent,
                    &task,
                    &context,
                    provider.as_ref(),
                    &orchestrator,
                    &bus,
                    &memory,
                    &stream_id,
                    correlation_id,
                    &project_root,
                    defaults.max_iterations,
                )
                .await
            }));
        }

        for handle in handles {
            let result = handle
                .await
                .map_err(|e| format!("step task panicked: {e}"))??;

            println!(
                "    [{}] {} ({} iterations)",
                result.step_id,
                if result.success { "done" } else { "FAILED" },
                result.iterations
            );

            if !result.success {
                overall_success = false;
            }

            step_outputs.insert(result.step_id.clone(), result.output.clone());
            all_results.insert(result.step_id.clone(), result);
        }

        println!();
    }

    // 5. Determine final output
    let final_output = pipeline
        .output
        .as_ref()
        .and_then(|o| step_outputs.get(&o.step).cloned())
        .or_else(|| {
            // Fall back to last step's output
            plan.levels
                .last()
                .and_then(|level| level.last())
                .and_then(|step_id| step_outputs.get(step_id).cloned())
        });

    // Emit pipeline completion event
    emit_event(
        &bus,
        &stream_id,
        correlation_id,
        EventKind::WorkflowCompleted {
            success: overall_success,
        },
    )
    .await;

    Ok(PipelineResult {
        pipeline_name: pipeline.name.clone(),
        step_results: all_results,
        final_output,
        success: overall_success,
    })
}

/// Run a single agent step: LLM loop with tool calls.
#[allow(clippy::too_many_arguments)]
async fn run_agent_step(
    step_id: &str,
    agent: &AgentConfig,
    task: &str,
    context: &str,
    provider: &dyn LlmProvider,
    orchestrator: &Orchestrator,
    bus: &EventBus,
    memory: &MemoryStore,
    stream_id: &str,
    correlation_id: Uuid,
    project_root: &Path,
    default_max_iterations: usize,
) -> Result<StepResult, String> {
    let max_iterations = agent.max_iterations.unwrap_or(default_max_iterations);
    let tool_defs = tool_definitions_for_agent(&agent.tools);

    // Build initial messages
    let mut messages: Vec<Message> = Vec::new();

    // System prompt
    let system_prompt = agent
        .system_prompt
        .clone()
        .unwrap_or_else(|| format!("You are the '{}' agent.", agent.name));
    messages.push(Message::System(system_prompt));

    // User task (with context from dependencies)
    let user_msg = if context.is_empty() {
        task.to_string()
    } else {
        format!("{task}\n\n---\nContext from previous steps:\n{context}")
    };
    messages.push(Message::User(user_msg));

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

        emit_event(
            bus,
            stream_id,
            correlation_id,
            EventKind::LlmCallStarted {
                iteration,
                message_count: messages.len(),
                approx_context_chars: messages
                    .iter()
                    .map(|m| match m {
                        Message::System(s) | Message::User(s) => s.len(),
                        Message::Assistant { content, .. } => {
                            content.as_ref().map_or(0, |c| c.len())
                        }
                        Message::ToolResult { content, .. } => content.len(),
                        Message::UserMultimodal { parts } => parts
                            .iter()
                            .map(|p| match p {
                                crate::types::ContentPart::Text(t) => t.len(),
                                _ => 0,
                            })
                            .sum(),
                    })
                    .sum(),
            },
        )
        .await;

        // Call LLM
        let request = ChatRequest {
            messages: messages.clone(),
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

                emit_event(
                    bus,
                    stream_id,
                    correlation_id,
                    EventKind::LlmCallCompleted {
                        has_tool_calls,
                        usage: Some(response.usage.clone()),
                        content_preview: content
                            .as_ref()
                            .map(|c| truncate(c, 100).to_string()),
                    },
                )
                .await;

                if !has_tool_calls {
                    // Agent is done — return text response
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

                // Process tool calls
                messages.push(response.message.clone());

                for tc in tool_calls {
                    let start = Instant::now();

                    emit_event(
                        bus,
                        stream_id,
                        correlation_id,
                        EventKind::ToolCallRequested {
                            tool_name: tc.name.clone(),
                            arguments: truncate(&tc.arguments, 200).to_string(),
                            call_id: tc.id.clone(),
                        },
                    )
                    .await;

                    // Map to intention and evaluate via orchestrator
                    let intention = tool_call_to_intention(&tc.name, &tc.arguments);
                    let verdict = orchestrator
                        .process_intention(
                            &intention,
                            stream_id,
                            correlation_id,
                            None, // No approval handler in CLI mode for now
                        )
                        .await;

                    let tool_result = match verdict {
                        OrchestratorResult::Approved => {
                            match execute_builtin_tool(
                                &tc.name,
                                &tc.arguments,
                                project_root,
                                memory,
                            )
                            .await
                            {
                                Ok(output) => output,
                                Err(e) => format!("[tool error] {e}"),
                            }
                        }
                        OrchestratorResult::Denied { reason } => {
                            format!("[denied by policy] {reason}")
                        }
                        OrchestratorResult::HumanRejected => {
                            "[rejected by human]".to_string()
                        }
                        OrchestratorResult::NoApprovalHandler => {
                            "[requires approval but no approval handler configured]".to_string()
                        }
                    };

                    let duration_ms = start.elapsed().as_millis() as u64;
                    let is_error = tool_result.starts_with("[tool error]")
                        || tool_result.starts_with("[denied")
                        || tool_result.starts_with("[rejected")
                        || tool_result.starts_with("[requires approval");

                    emit_event(
                        bus,
                        stream_id,
                        correlation_id,
                        EventKind::ToolCallCompleted {
                            call_id: tc.id.clone(),
                            result_preview: truncate(&tool_result, 200).to_string(),
                            is_error,
                            duration_ms,
                        },
                    )
                    .await;

                    messages.push(Message::ToolResult {
                        tool_call_id: tc.id.clone(),
                        content: tool_result,
                    });
                }
            }
            _ => {
                return Err(format!(
                    "[{step_id}] unexpected message type from LLM"
                ));
            }
        }
    }
}

/// Map a tool call to an ESAA Intention for contract evaluation.
fn tool_call_to_intention(tool_name: &str, arguments_json: &str) -> Intention {
    let args: serde_json::Value =
        serde_json::from_str(arguments_json).unwrap_or(serde_json::Value::Null);

    match tool_name {
        "execute_shell_command" => Intention::ExecuteShellCommand {
            command: args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            timeout_secs: args.get("timeout_secs").and_then(|v| v.as_u64()),
        },
        "write_file" => Intention::WriteFile {
            path: args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            content: args
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        "read_file" => Intention::ReadFile {
            path: args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        "web_search" => Intention::WebSearch {
            query: args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        "web_scrape" => Intention::WebScrape {
            url: args
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        "write_memory" => Intention::WriteMemory {
            key: args
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            content: args
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        _ => Intention::CallCustomTool {
            tool_name: tool_name.to_string(),
            arguments: arguments_json.to_string(),
        },
    }
}

/// Resolve `${inputs.*}` and `${steps.<id>.output}` in a task template.
fn resolve_task_template(
    task: &str,
    inputs: &HashMap<String, String>,
    step_outputs: &HashMap<String, String>,
) -> String {
    let mut result = task.to_string();

    // Replace ${inputs.X}
    for (key, value) in inputs {
        result = result.replace(&format!("${{inputs.{key}}}"), value);
    }

    // Replace ${steps.X.output}
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

        let result = resolve_task_template(
            "Search for: ${inputs.query}",
            &inputs,
            &HashMap::new(),
        );
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
    fn tool_call_to_intention_shell() {
        let intention =
            tool_call_to_intention("execute_shell_command", r#"{"command":"ls"}"#);
        assert_eq!(intention.tag(), "execute_shell_command");
    }

    #[test]
    fn tool_call_to_intention_custom() {
        let intention = tool_call_to_intention("my_custom_tool", r#"{"arg":"val"}"#);
        assert_eq!(intention.tag(), "call_custom_tool");
    }
}
