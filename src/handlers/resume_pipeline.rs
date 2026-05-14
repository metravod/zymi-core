//! `ResumePipeline` orchestrator (ADR-0018).
//!
//! Forks a *new* pipeline stream from a parent run at a chosen step. Steps
//! topologically before the fork point are frozen — their events are copied
//! verbatim onto the new stream and they are not re-executed. The fork step
//! and all its DAG-descendants run from scratch against the *current*
//! `pipelines/*.yml` and `agents/*.yml` from disk.
//!
//! See `adr/0018-idempotent-fork-resume.md` for the full design.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::commands::{ResumeContext, RunPipeline};
use crate::config::pipeline::PipelineStepKind;
use crate::config::{build_execution_plan, PipelineConfig, WorkspaceConfig};
use crate::events::store::EventStore;
use crate::events::{Event, EventKind};
use crate::handlers::run_pipeline::{self, PipelineResult};
use crate::runtime::{Runtime, ToolCatalog};
use crate::types::Message;

/// Request to fork-resume a pipeline run.
#[derive(Debug, Clone)]
pub struct ResumePipeline {
    pub parent_stream_id: String,
    pub fork_at_step: String,
}

/// Outcome of a successful resume — the new stream id (so the CLI can echo
/// it for piping into `zymi observe --run`) plus the underlying pipeline
/// result.
#[derive(Debug)]
pub struct ResumeOutcome {
    pub new_stream_id: String,
    pub frozen_step_ids: Vec<String>,
    pub result: PipelineResult,
}

/// What a resume is *about* to do, before any new events are written. Cheap
/// to compute, side-effect-free, and validates everything `handle` validates
/// (fork step exists, frozen prefix completed in parent, no DAG divergence).
/// CLI's `--dry-run` exposes this; `handle` uses it internally.
#[derive(Debug, Clone)]
pub struct ResumePlan {
    pub pipeline_name: String,
    pub parent_stream_id: String,
    pub parent_correlation_id: Uuid,
    pub fork_at_step: String,
    pub inputs: HashMap<String, String>,
    /// Steps copied verbatim from the parent stream, in DAG order.
    pub frozen_in_dag_order: Vec<String>,
    /// Steps that will re-execute against current configs, in DAG order.
    pub re_executed_in_dag_order: Vec<String>,
    /// Tools in the re-execute set that are marked `no_resume: true` and
    /// will be shadowed (body skipped, placeholder result). One entry per
    /// `(step_id, tool_name)` pair. Best-effort over what the planner can
    /// see — when `plan_with` is called without a tool catalog (sync
    /// `--dry-run`), Python and MCP tools are not included.
    pub shadowed_tools: Vec<ShadowedToolWarning>,
}

/// One row in `ResumePlan::shadowed_tools` — a tool inside a re-executed
/// step that will be shadowed on resume.
#[derive(Debug, Clone)]
pub struct ShadowedToolWarning {
    pub step_id: String,
    pub tool: String,
}

/// Compute (and validate) the resume plan. Read-only over the store; does
/// not need a full [`Runtime`] (so `--dry-run` can run before LLM config
/// even exists).
pub async fn plan(rt: &Runtime, cmd: &ResumePipeline) -> Result<ResumePlan, String> {
    plan_with(rt.store().as_ref(), rt.workspace(), Some(rt.tool_catalog()), cmd).await
}

/// Plan variant for callers that have the store + workspace but not a
/// fully-built [`Runtime`] (e.g. CLI `--dry-run` before LLM is configured).
///
/// `tool_catalog` is optional: when provided, the planner consults it to
/// flag `no_resume` Python and MCP tools in the re-execute set. When
/// `None` (sync `--dry-run` paths), only declarative `tools/*.yml` tools
/// are inspected.
pub async fn plan_with(
    store: &dyn EventStore,
    workspace: &WorkspaceConfig,
    tool_catalog: Option<&ToolCatalog>,
    cmd: &ResumePipeline,
) -> Result<ResumePlan, String> {
    let parent_top = store
        .read_stream(&cmd.parent_stream_id, 1)
        .await
        .map_err(|e| format!("failed to read parent stream: {e}"))?;

    if parent_top.is_empty() {
        return Err(format!(
            "parent stream '{}' has no events",
            cmd.parent_stream_id
        ));
    }

    let (pipeline_name, inputs, parent_correlation_id) = parent_top
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::PipelineRequested { pipeline, inputs } => Some((
                pipeline.clone(),
                inputs.clone(),
                e.correlation_id.unwrap_or_else(Uuid::new_v4),
            )),
            _ => None,
        })
        .ok_or_else(|| {
            format!(
                "parent stream '{}' has no PipelineRequested event — not a pipeline run",
                cmd.parent_stream_id
            )
        })?;

    let pipeline = workspace
        .pipelines
        .get(&pipeline_name)
        .ok_or_else(|| {
            format!(
                "pipeline '{pipeline_name}' from parent stream not found in current project"
            )
        })?
        .clone();

    if !pipeline.steps.iter().any(|s| s.id == cmd.fork_at_step) {
        return Err(format!(
            "fork step '{}' not found in pipeline '{}'. Available: [{}]",
            cmd.fork_at_step,
            pipeline_name,
            pipeline
                .steps
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // Validate the DAG we'll actually execute (also catches cycles introduced
    // by edits to pipelines/*.yml after the parent run).
    let exec_plan = build_execution_plan(&pipeline)
        .map_err(|e| format!("failed to build execution plan: {e}"))?;

    let re_executed: HashSet<String> = transitive_dependents(&pipeline, &cmd.fork_at_step);
    let frozen_step_ids: HashSet<String> = pipeline
        .steps
        .iter()
        .map(|s| s.id.clone())
        .filter(|id| !re_executed.contains(id))
        .collect();

    let parent_completed: HashSet<String> = parent_top
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::WorkflowNodeCompleted {
                node_id,
                success: true,
            } => Some(node_id.clone()),
            _ => None,
        })
        .collect();

    for step_id in &frozen_step_ids {
        if !parent_completed.contains(step_id) {
            return Err(format!(
                "step '{step_id}' is upstream of fork point '{}' but did not complete \
                 successfully in parent stream — cannot resume",
                cmd.fork_at_step
            ));
        }
    }

    for step in &pipeline.steps {
        if !re_executed.contains(&step.id) {
            continue;
        }
        for dep in &step.depends_on {
            let in_re = re_executed.contains(dep);
            let in_frozen = frozen_step_ids.contains(dep);
            if !in_re && !in_frozen {
                return Err(format!(
                    "step '{}' now depends on '{dep}', which did not run in the parent \
                     stream — start a new run instead",
                    step.id
                ));
            }
            if !in_re && !parent_completed.contains(dep) {
                return Err(format!(
                    "step '{}' depends on frozen step '{dep}', which did not complete \
                     in parent stream — cannot resume",
                    step.id
                ));
            }
        }
    }

    let frozen_in_dag_order: Vec<String> = exec_plan
        .levels
        .iter()
        .flatten()
        .filter(|id| frozen_step_ids.contains(*id))
        .cloned()
        .collect();
    let re_executed_in_dag_order: Vec<String> = exec_plan
        .levels
        .iter()
        .flatten()
        .filter(|id| re_executed.contains(*id))
        .cloned()
        .collect();

    let shadowed_tools =
        collect_shadowed_tools(&pipeline, workspace, tool_catalog, &re_executed_in_dag_order);

    Ok(ResumePlan {
        pipeline_name,
        parent_stream_id: cmd.parent_stream_id.clone(),
        parent_correlation_id,
        fork_at_step: cmd.fork_at_step.clone(),
        inputs,
        frozen_in_dag_order,
        re_executed_in_dag_order,
        shadowed_tools,
    })
}

/// Scan the re-execute set for tools marked `no_resume` and return one
/// `ShadowedToolWarning` per (step, tool) pair. When `tool_catalog` is
/// `Some`, it is the authoritative source (covers builtins / declarative
/// / Python / MCP); otherwise we fall back to `workspace.tools` and only
/// catch declarative `tools/*.yml` entries.
fn collect_shadowed_tools(
    pipeline: &PipelineConfig,
    workspace: &WorkspaceConfig,
    tool_catalog: Option<&ToolCatalog>,
    re_executed_in_dag_order: &[String],
) -> Vec<ShadowedToolWarning> {
    let is_no_resume = |name: &str| -> bool {
        if let Some(cat) = tool_catalog {
            return cat.no_resume(name);
        }
        workspace
            .tools
            .get(name)
            .map(|t| t.effective_no_resume())
            .unwrap_or(false)
    };

    let mut out = Vec::new();
    for step_id in re_executed_in_dag_order {
        let Some(step) = pipeline.steps.iter().find(|s| &s.id == step_id) else {
            continue;
        };
        let candidate_tools: Vec<&str> = match &step.kind {
            PipelineStepKind::Tool { tool, .. } => vec![tool.as_str()],
            PipelineStepKind::Agent { agent, .. } => workspace
                .agents
                .get(agent)
                .map(|a| a.tools.iter().map(|s| s.as_str()).collect())
                .unwrap_or_default(),
        };
        for tool in candidate_tools {
            if is_no_resume(tool) {
                out.push(ShadowedToolWarning {
                    step_id: step_id.clone(),
                    tool: tool.to_string(),
                });
            }
        }
    }
    out
}

/// Execute a [`ResumePipeline`] command against the given runtime.
pub async fn handle(rt: &Runtime, cmd: ResumePipeline) -> Result<ResumeOutcome, String> {
    let store = rt.store();
    let ResumePlan {
        pipeline_name,
        parent_correlation_id,
        inputs,
        frozen_in_dag_order,
        ..
    } = plan(rt, &cmd).await?;

    let pipeline = rt
        .workspace()
        .pipelines
        .get(&pipeline_name)
        .expect("plan() validated pipeline exists")
        .clone();
    let exec_plan = build_execution_plan(&pipeline)
        .map_err(|e| format!("failed to build execution plan: {e}"))?;

    let frozen_step_ids: HashSet<String> = frozen_in_dag_order.iter().cloned().collect();

    let new_stream_id = format!("pipeline-{}-{}", pipeline_name, Uuid::new_v4());
    let new_correlation_id = Uuid::new_v4();

    // We still need parent_top events for per-step descriptions and sub-stream copy.
    let parent_top = store
        .read_stream(&cmd.parent_stream_id, 1)
        .await
        .map_err(|e| format!("failed to re-read parent stream: {e}"))?;

    let mut frozen_outputs: HashMap<String, String> = HashMap::new();
    for step_id in &frozen_in_dag_order {
        let sub_stream = format!("{}:step:{}", cmd.parent_stream_id, step_id);
        let events = store
            .read_stream(&sub_stream, 1)
            .await
            .map_err(|e| format!("failed to read frozen sub-stream '{sub_stream}': {e}"))?;
        let output = extract_step_output(&events).ok_or_else(|| {
            format!(
                "could not reconstruct output for frozen step '{step_id}' from parent \
                 stream — missing terminal LlmCallCompleted event"
            )
        })?;
        frozen_outputs.insert(step_id.clone(), output);
    }

    append_event(
        store.as_ref(),
        &new_stream_id,
        new_correlation_id,
        EventKind::PipelineRequested {
            pipeline: pipeline_name.clone(),
            inputs: inputs.clone(),
        },
    )
    .await?;

    append_event(
        store.as_ref(),
        &new_stream_id,
        new_correlation_id,
        EventKind::ResumeForked {
            parent_stream_id: cmd.parent_stream_id.clone(),
            parent_correlation_id,
            fork_at_step: cmd.fork_at_step.clone(),
        },
    )
    .await?;

    append_event(
        store.as_ref(),
        &new_stream_id,
        new_correlation_id,
        EventKind::WorkflowStarted {
            user_message: format!("pipeline: {pipeline_name} (resumed)"),
            node_count: exec_plan.step_count(),
        },
    )
    .await?;

    for step_id in &frozen_in_dag_order {
        replay_frozen_step(
            store.as_ref(),
            &cmd.parent_stream_id,
            &new_stream_id,
            new_correlation_id,
            step_id,
            &parent_top,
        )
        .await?;
    }

    let resume_ctx = ResumeContext {
        parent_stream_id: cmd.parent_stream_id.clone(),
        fork_at_step: cmd.fork_at_step.clone(),
        frozen_outputs,
        frozen_step_ids: frozen_step_ids.clone(),
    };

    let run_cmd = RunPipeline::resume(
        pipeline_name,
        inputs,
        new_correlation_id,
        new_stream_id.clone(),
        resume_ctx,
    );

    let result = run_pipeline::handle(rt, run_cmd).await?;

    Ok(ResumeOutcome {
        new_stream_id,
        frozen_step_ids: frozen_in_dag_order,
        result,
    })
}

/// Set of steps that must be re-executed when forking from `root`: `root`
/// itself plus everything that transitively depends on it in the *current*
/// pipeline DAG.
fn transitive_dependents(pipeline: &PipelineConfig, root: &str) -> HashSet<String> {
    let mut result: HashSet<String> = HashSet::new();
    let mut stack = vec![root.to_string()];
    while let Some(current) = stack.pop() {
        if !result.insert(current.clone()) {
            continue;
        }
        for step in &pipeline.steps {
            if step.depends_on.iter().any(|d| d == &current) {
                stack.push(step.id.clone());
            }
        }
    }
    result
}

/// Extract the "output" of a completed agent step from its sub-stream:
/// the content of the last `LlmCallCompleted` whose `has_tool_calls` is
/// false (i.e. the iteration that produced the final answer). Falls back to
/// `content_preview` for pre-enrichment events that have no
/// `response_message`.
fn extract_step_output(events: &[Event]) -> Option<String> {
    let mut last: Option<&EventKind> = None;
    for ev in events {
        if let EventKind::LlmCallCompleted {
            has_tool_calls: false,
            ..
        } = &ev.kind
        {
            last = Some(&ev.kind);
        }
    }
    let kind = last?;
    if let EventKind::LlmCallCompleted {
        response_message,
        content_preview,
        ..
    } = kind
    {
        if let Some(Message::Assistant {
            content: Some(text),
            ..
        }) = response_message
        {
            return Some(text.clone());
        }
        return content_preview.clone();
    }
    None
}

/// Append a freshly-built event directly to the store, no bus publish.
///
/// Mirrors the shape of `run_pipeline::emit_event` (same arg list) so the two
/// handlers' bootstrap-event sites read in parallel. The bus is intentionally
/// skipped: bootstrap events are reconstructed from the store by `observe`
/// and `runs` queries, and bypassing the bus avoids a feedback loop where
/// the resume orchestrator would re-trigger its own pipeline router.
async fn append_event(
    store: &dyn EventStore,
    stream_id: &str,
    correlation_id: Uuid,
    kind: EventKind,
) -> Result<(), String> {
    let mut event =
        Event::new(stream_id.to_string(), kind, "engine".into()).with_correlation(correlation_id);
    store
        .append(&mut event)
        .await
        .map_err(|e| format!("failed to append event: {e}"))?;
    Ok(())
}

/// Copy one frozen step's events from the parent stream to the new stream
/// (ADR-0018 §"frozen replay"): bracket with WorkflowNodeStarted/Completed,
/// then physically copy every event in the step's sub-stream.
async fn replay_frozen_step(
    store: &dyn EventStore,
    parent_stream_id: &str,
    new_stream_id: &str,
    new_correlation_id: Uuid,
    step_id: &str,
    parent_top: &[Event],
) -> Result<(), String> {
    let description = parent_top
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::WorkflowNodeStarted { node_id, description } if node_id == step_id => {
                Some(description.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| format!("frozen from parent stream {parent_stream_id}"));

    append_event(
        store,
        new_stream_id,
        new_correlation_id,
        EventKind::WorkflowNodeStarted {
            node_id: step_id.to_string(),
            description,
        },
    )
    .await?;

    let sub_stream_src = format!("{parent_stream_id}:step:{step_id}");
    let sub_stream_dst = format!("{new_stream_id}:step:{step_id}");
    let events = store
        .read_stream(&sub_stream_src, 1)
        .await
        .map_err(|e| format!("failed to re-read sub-stream '{sub_stream_src}': {e}"))?;
    for src in events {
        let mut copy = Event {
            id: Uuid::new_v4(),
            stream_id: sub_stream_dst.clone(),
            sequence: 0,
            timestamp: src.timestamp,
            kind: src.kind,
            correlation_id: Some(new_correlation_id),
            causation_id: None,
            source: src.source,
        };
        store
            .append(&mut copy)
            .await
            .map_err(|e| format!("failed to copy frozen event: {e}"))?;
    }

    append_event(
        store,
        new_stream_id,
        new_correlation_id,
        EventKind::WorkflowNodeCompleted {
            node_id: step_id.to_string(),
            success: true,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::pipeline::PipelineStepKind;
    use crate::config::PipelineStep;

    fn pipe(steps: &[(&str, &[&str])]) -> PipelineConfig {
        PipelineConfig {
            name: "t".into(),
            description: None,
            inputs: vec![],
            steps: steps
                .iter()
                .map(|(id, deps)| PipelineStep {
                    id: (*id).into(),
                    kind: PipelineStepKind::Agent {
                        agent: "a".into(),
                        task: "".into(),
                    },
                    depends_on: deps.iter().map(|s| (*s).into()).collect(),
                    when: None,
                })
                .collect(),
            output: None,
            approval_channel: None,
        }
    }

    #[test]
    fn dependents_include_root() {
        let p = pipe(&[("a", &[]), ("b", &["a"])]);
        let d = transitive_dependents(&p, "b");
        assert!(d.contains("b"));
        assert!(!d.contains("a"));
    }

    #[test]
    fn dependents_chain() {
        let p = pipe(&[("a", &[]), ("b", &["a"]), ("c", &["b"]), ("d", &["c"])]);
        let d = transitive_dependents(&p, "b");
        assert_eq!(d.len(), 3);
        assert!(d.contains("b"));
        assert!(d.contains("c"));
        assert!(d.contains("d"));
    }

    #[test]
    fn dependents_diamond() {
        // a -> b -> d
        // a -> c -> d
        let p = pipe(&[
            ("a", &[]),
            ("b", &["a"]),
            ("c", &["a"]),
            ("d", &["b", "c"]),
        ]);
        let from_b = transitive_dependents(&p, "b");
        assert!(from_b.contains("b") && from_b.contains("d"));
        assert!(!from_b.contains("c") && !from_b.contains("a"));
    }

    #[test]
    fn extract_output_picks_last_no_tool_call() {
        use crate::types::TokenUsage;
        let mk = |has_tools: bool, content: &str| Event {
            id: Uuid::new_v4(),
            stream_id: "s".into(),
            sequence: 0,
            timestamp: chrono::Utc::now(),
            kind: EventKind::LlmCallCompleted {
                response_message: Some(Message::Assistant {
                    content: Some(content.into()),
                    tool_calls: vec![],
                }),
                has_tool_calls: has_tools,
                usage: Some(TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                }),
                content_preview: Some(content.into()),
            },
            correlation_id: None,
            causation_id: None,
            source: "engine".into(),
        };
        let events = vec![mk(true, "tools first"), mk(false, "final answer")];
        assert_eq!(extract_step_output(&events).as_deref(), Some("final answer"));
    }

    #[test]
    fn extract_output_falls_back_to_preview() {
        let ev = Event {
            id: Uuid::new_v4(),
            stream_id: "s".into(),
            sequence: 0,
            timestamp: chrono::Utc::now(),
            kind: EventKind::LlmCallCompleted {
                response_message: None,
                has_tool_calls: false,
                usage: None,
                content_preview: Some("pre-enrichment".into()),
            },
            correlation_id: None,
            causation_id: None,
            source: "engine".into(),
        };
        assert_eq!(
            extract_step_output(std::slice::from_ref(&ev)).as_deref(),
            Some("pre-enrichment")
        );
    }

    // ── End-to-end integration tests ───────────────────────────────────────
    //
    // These tests exercise the full resume flow against a real SqliteEventStore
    // and a counting mock LLM provider. The pipeline has two sequential steps
    // (`search` -> `summarize`); we run it once, then fork-resume from
    // `summarize` and assert idempotency (search is not re-executed) plus
    // config-pickup (summarize sees the latest mock response).

    use crate::config::{
        AgentConfig, ContractsConfig, DefaultsConfig, ProjectConfig, RuntimeConfig,
        ServicesConfig, ShellConfig, WorkspaceConfig,
    };
    use crate::events::store::{open_store, StoreBackend};
    use crate::handlers::run_pipeline as run_pipeline_handler;
    use crate::llm::{ChatRequest, ChatResponse, LlmError, LlmProvider};
    use crate::policy::PolicyConfig;
    use crate::runtime::Runtime;
    use crate::types::TokenUsage;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    #[derive(Debug)]
    struct CountingProvider {
        call_count: AtomicUsize,
        response: std::sync::Mutex<String>,
    }

    impl CountingProvider {
        fn new(response: &str) -> Self {
            Self {
                call_count: AtomicUsize::new(0),
                response: std::sync::Mutex::new(response.into()),
            }
        }

        fn calls(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }

        fn set_response(&self, s: &str) {
            *self.response.lock().unwrap() = s.into();
        }
    }

    #[async_trait]
    impl LlmProvider for CountingProvider {
        async fn chat_completion(
            &self,
            _request: &ChatRequest,
        ) -> Result<ChatResponse, LlmError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let body = self.response.lock().unwrap().clone();
            Ok(ChatResponse {
                message: Message::Assistant {
                    content: Some(body),
                    tool_calls: vec![],
                },
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                },
                model: "mock".into(),
            })
        }
    }

    fn make_pipeline() -> PipelineConfig {
        pipe(&[("search", &[]), ("summarize", &["search"])])
    }

    fn make_workspace() -> WorkspaceConfig {
        let mut agents = HashMap::new();
        agents.insert(
            "researcher".into(),
            AgentConfig {
                name: "researcher".into(),
                description: None,
                model: Some("mock".into()),
                system_prompt: Some("you are researcher".into()),
                tools: vec![],
                max_iterations: Some(3),
                timeout_secs: None,
                policy: None,
            },
        );
        let mut pipelines = HashMap::new();
        let mut p = make_pipeline();
        p.name = "research".into();
        for s in &mut p.steps {
            let task = match s.id.as_str() {
                "search" => "search the web".into(),
                "summarize" => "summarize ${steps.search.output}".into(),
                _ => "".into(),
            };
            s.kind = PipelineStepKind::Agent {
                agent: "researcher".into(),
                task,
            };
        }
        pipelines.insert("research".into(), p);

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

    async fn run_initial(rt: &Runtime) -> Result<String, String> {
        let cmd = crate::commands::RunPipeline::new("research", HashMap::new());
        let result = run_pipeline_handler::handle(rt, cmd).await?;
        // The handler picks an internal stream id; we recover it from the
        // store by scanning for the most recent PipelineRequested.
        let streams = rt.store().list_streams().await.unwrap();
        for (s, _) in streams {
            let evs = rt.store().read_stream(&s, 1).await.unwrap();
            if evs.iter().any(|e| matches!(&e.kind, EventKind::PipelineRequested { .. })) {
                if !result.success {
                    return Err("initial run failed".into());
                }
                return Ok(s);
            }
        }
        Err("no parent stream found".into())
    }

    #[tokio::test]
    async fn resume_skips_frozen_step() {
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("events.db");
        let store = open_store(StoreBackend::Sqlite { path: store_path }).unwrap();
        let provider = Arc::new(CountingProvider::new("answer-1"));

        let rt = Runtime::builder(make_workspace(), dir.path().to_path_buf())
            .with_store(store.clone())
            .with_llm_provider(provider.clone() as Arc<dyn LlmProvider>)
            .build()
            .unwrap();

        let parent_stream = run_initial(&rt).await.unwrap();
        assert_eq!(provider.calls(), 2, "initial run should LLM once per step");

        provider.set_response("answer-2-rewrite");

        let outcome = handle(
            &rt,
            ResumePipeline {
                parent_stream_id: parent_stream.clone(),
                fork_at_step: "summarize".into(),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            provider.calls(),
            3,
            "resume should re-run only the summarize step (1 extra LLM call)"
        );
        assert_eq!(outcome.frozen_step_ids, vec!["search".to_string()]);
        assert_ne!(outcome.new_stream_id, parent_stream);
        assert_eq!(
            outcome.result.final_output.as_deref(),
            Some("answer-2-rewrite")
        );

        // Frozen step events were physically copied to the new sub-stream.
        let new_search_sub = format!("{}:step:search", outcome.new_stream_id);
        let copied = store.read_stream(&new_search_sub, 1).await.unwrap();
        assert!(
            !copied.is_empty(),
            "frozen step's sub-stream should be populated on the new stream"
        );

        // ResumeForked marker present on the new stream.
        let new_top = store
            .read_stream(&outcome.new_stream_id, 1)
            .await
            .unwrap();
        assert!(new_top
            .iter()
            .any(|e| matches!(&e.kind, EventKind::ResumeForked { .. })));
        assert!(new_top
            .iter()
            .any(|e| matches!(&e.kind, EventKind::PipelineCompleted { .. })));
    }

    /// `no_resume` tools in the re-execute set must be shadowed: their
    /// body is not invoked and the recorded `ToolCallCompleted` carries
    /// `replayed: true`. Cf. the doc-comment on `EventKind::ToolCallCompleted`.
    #[tokio::test]
    async fn resume_shadows_no_resume_tools_in_re_execute_set() {
        use crate::config::tool::{HttpMethod, ImplementationConfig, ToolConfig};
        use crate::runtime::action_executor::{ActionContext, ActionExecutor};
        use std::collections::HashSet;

        // Mock executor counts how many times `send_email` was actually
        // dispatched. The shadow path must bypass it entirely.
        #[derive(Debug, Default)]
        struct CountingExecutor {
            calls: AtomicUsize,
        }

        #[async_trait]
        impl ActionExecutor for CountingExecutor {
            async fn execute(
                &self,
                tool_name: &str,
                _arguments_json: &str,
                _ctx: &ActionContext<'_>,
            ) -> Result<String, String> {
                if tool_name == "send_email" {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    Ok("{\"sent\": true, \"message_id\": \"msg-1\"}".into())
                } else {
                    Err(format!("unexpected tool {tool_name}"))
                }
            }
        }

        let dir = TempDir::new().unwrap();
        let store = open_store(StoreBackend::Sqlite {
            path: dir.path().join("events.db"),
        })
        .unwrap();
        let provider = Arc::new(CountingProvider::new("prepared"));
        let executor = Arc::new(CountingExecutor::default());

        // Workspace: `prep` agent + `send` deterministic tool step. The
        // tool is declarative with `no_resume: true`.
        let mut ws = make_workspace();
        let mut send_tool_cfg = ToolConfig {
            name: "send_email".into(),
            description: "Send an email — irreversible side-effect".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": {"type": "string"},
                    "body": {"type": "string"},
                },
                "required": ["to", "body"],
            }),
            requires_approval: Some(false),
            no_resume: Some(true),
            implementation: ImplementationConfig::Http {
                method: HttpMethod::Post,
                url: "http://mock.local/send".into(),
                headers: Default::default(),
                body_template: None,
            },
        };
        // Borrow-checker pacifier: keep cfg movable.
        send_tool_cfg.requires_approval = Some(false);
        ws.tools.insert("send_email".into(), send_tool_cfg);

        // Replace the pipeline with prep -> send (tool step).
        let mut pipeline = pipe(&[("prep", &[]), ("send", &["prep"])]);
        pipeline.name = "research".into();
        for step in &mut pipeline.steps {
            step.kind = match step.id.as_str() {
                "prep" => PipelineStepKind::Agent {
                    agent: "researcher".into(),
                    task: "prepare".into(),
                },
                "send" => PipelineStepKind::Tool {
                    tool: "send_email".into(),
                    args: serde_yml::from_str(
                        "to: user@example.com\nbody: \"${steps.prep.output}\"\n",
                    )
                    .unwrap(),
                },
                _ => unreachable!(),
            };
        }
        ws.pipelines.insert("research".into(), pipeline);

        let rt = Runtime::builder(ws, dir.path().to_path_buf())
            .with_store(store.clone())
            .with_llm_provider(provider.clone() as Arc<dyn LlmProvider>)
            .with_action_executor(executor.clone() as Arc<dyn ActionExecutor>)
            .build()
            .unwrap();

        // 1. Sanity: ResumePlan flags the shadowed tool when forking from
        //    `send`. Doing this before `run_initial` to keep the assertion
        //    independent of the actual execution.
        let plan_only = plan(
            &rt,
            &ResumePipeline {
                parent_stream_id: "irrelevant".into(),
                fork_at_step: "send".into(),
            },
        )
        .await;
        // plan() fails because parent stream is missing — that's fine, we
        // only wanted to confirm types compile. The dry-run path with a
        // real parent is exercised below.
        assert!(plan_only.is_err());

        // 2. Initial run: send_email is dispatched once.
        let parent_stream = run_initial(&rt).await.unwrap();
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);

        // 3. Resume from `send`. Re-execute set = {send}; the tool is
        //    marked no_resume so the body must NOT be invoked again.
        let outcome = handle(
            &rt,
            ResumePipeline {
                parent_stream_id: parent_stream.clone(),
                fork_at_step: "send".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            1,
            "no_resume tool body must be shadowed on resume — counter unchanged"
        );

        // 4. ResumePlan computed during handle() flagged the shadow.
        let post_plan = plan(
            &rt,
            &ResumePipeline {
                parent_stream_id: parent_stream.clone(),
                fork_at_step: "send".into(),
            },
        )
        .await
        .unwrap();
        let shadow_set: HashSet<(String, String)> = post_plan
            .shadowed_tools
            .iter()
            .map(|w| (w.step_id.clone(), w.tool.clone()))
            .collect();
        assert!(
            shadow_set.contains(&("send".into(), "send_email".into())),
            "ResumePlan should flag send→send_email as shadowed, got {shadow_set:?}"
        );

        // 5. The new stream's send sub-stream carries a
        //    ToolCallCompleted{replayed:true} event.
        let send_sub = format!("{}:step:send", outcome.new_stream_id);
        let new_send_events = store.read_stream(&send_sub, 1).await.unwrap();
        let replayed_completed = new_send_events
            .iter()
            .find(|e| matches!(&e.kind, EventKind::ToolCallCompleted { replayed: true, .. }));
        assert!(
            replayed_completed.is_some(),
            "expected ToolCallCompleted with replayed=true in new send sub-stream, \
             saw kinds: {:?}",
            new_send_events.iter().map(|e| e.kind_tag()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn resume_unknown_step_errors() {
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("events.db");
        let store = open_store(StoreBackend::Sqlite { path: store_path }).unwrap();
        let provider = Arc::new(CountingProvider::new("ok"));
        let rt = Runtime::builder(make_workspace(), dir.path().to_path_buf())
            .with_store(store)
            .with_llm_provider(provider as Arc<dyn LlmProvider>)
            .build()
            .unwrap();

        let parent = run_initial(&rt).await.unwrap();
        let err = handle(
            &rt,
            ResumePipeline {
                parent_stream_id: parent,
                fork_at_step: "ghost".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn resume_dag_divergence_when_frozen_dep_missing() {
        // Set up: parent had pipeline with one step `search`; current pipeline
        // adds a brand-new dep `extra` that summarize references. Resuming
        // from summarize must hard-error (the new dep didn't run in parent).
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("events.db");
        let store = open_store(StoreBackend::Sqlite { path: store_path }).unwrap();
        let provider = Arc::new(CountingProvider::new("ok"));

        let mut ws_initial = make_workspace();
        // Parent run uses original pipeline (search -> summarize).
        let rt_parent = Runtime::builder(ws_initial.clone(), dir.path().to_path_buf())
            .with_store(store.clone())
            .with_llm_provider(provider.clone() as Arc<dyn LlmProvider>)
            .build()
            .unwrap();
        let parent = run_initial(&rt_parent).await.unwrap();
        drop(rt_parent);

        // Mutate workspace: introduce a new step `extra` that summarize now
        // depends on. `extra` never ran in the parent.
        let p = ws_initial.pipelines.get_mut("research").unwrap();
        p.steps.push(crate::config::PipelineStep {
            id: "extra".into(),
            kind: PipelineStepKind::Agent {
                agent: "researcher".into(),
                task: "do extra".into(),
            },
            depends_on: vec![],
            when: None,
        });
        if let Some(sum) = p.steps.iter_mut().find(|s| s.id == "summarize") {
            sum.depends_on.push("extra".into());
        }

        let rt_resume = Runtime::builder(ws_initial, dir.path().to_path_buf())
            .with_store(store)
            .with_llm_provider(provider as Arc<dyn LlmProvider>)
            .build()
            .unwrap();

        let err = handle(
            &rt_resume,
            ResumePipeline {
                parent_stream_id: parent,
                fork_at_step: "summarize".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("did not complete") || err.contains("did not run"),
            "expected DAG divergence error, got: {err}"
        );
    }
}
