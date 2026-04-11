//! Runtime / AppContext — slice 1 of the runtime unification (ADR-0013).
//!
//! A [`Runtime`] is a per-project bundle of all the infrastructure a command
//! handler needs to execute against:
//!
//! - the [`EventStore`] (single source of truth, per ADR-0012)
//! - the in-process [`EventBus`] fan-out
//! - the configured [`LlmProvider`]
//! - the [`ContractEngine`] / [`Orchestrator`] for ESAA
//! - the operator-supplied [`ApprovalHandler`], if any
//! - the [`ActionExecutor`] used to run approved tool calls
//! - the cross-process [`StoreTailWatcher`] poll interval
//!
//! Both `zymi run` and `zymi serve` build one `Runtime` per project and
//! dispatch [`crate::commands::RunPipeline`] commands at it via
//! [`crate::handlers::run_pipeline::handle`]. Earlier, each entrypoint
//! constructed its own copy of contracts/orchestrator/provider on every
//! pipeline run; the `Runtime` is the single canonical wiring point those
//! entrypoints share.
//!
//! Slices 1–4 of the runtime unification have landed: slice 1 introduced
//! `Runtime` + `RunPipeline` handler + `ActionExecutor`, slice 2 ported the
//! Python bridge to the same wiring (see `python/py_runtime.rs`), slice 3
//! lifted the `BUS → CMD` translation out of `cli/serve.rs` into
//! [`event_router::EventCommandRouter`], and slice 4 promoted the
//! [`crate::events::store::StoreTailWatcher`] poll/lag policy from a hidden
//! constant into a typed runtime contract via
//! [`crate::events::store::TailWatcherPolicy`]. What this module
//! deliberately still does **not** do (tracked under remaining open goals
//! in `.drift/project.json`): multi-provider routing, projection-backed
//! aggregates, and recovery.

pub mod action_executor;
pub mod event_router;
pub mod tool_catalog;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::approval::ApprovalHandler;
use crate::config::WorkspaceConfig;
use crate::esaa::contracts::ContractEngine;
use crate::esaa::orchestrator::Orchestrator;
use crate::events::bus::EventBus;
use crate::events::store::{open_store, EventStore, StoreBackend, TailWatcherPolicy};
use crate::llm::{self, LlmProvider};
use crate::policy::PolicyEngine;

pub use action_executor::{ActionContext, ActionExecutor, BuiltinActionExecutor, CatalogActionExecutor};
pub use event_router::EventCommandRouter;
pub use tool_catalog::ToolCatalog;

/// Per-project bundle of infrastructure shared by command handlers.
///
/// Construct via [`RuntimeBuilder`]; never instantiate the fields by hand
/// from outside this module.
#[derive(Clone)]
pub struct Runtime {
    workspace: Arc<WorkspaceConfig>,
    project_root: PathBuf,
    store: Arc<dyn EventStore>,
    bus: Arc<EventBus>,
    provider: Arc<dyn LlmProvider>,
    contracts: Arc<ContractEngine>,
    orchestrator: Arc<Orchestrator>,
    approval: Option<Arc<dyn ApprovalHandler>>,
    action_executor: Arc<dyn ActionExecutor>,
    tool_catalog: Arc<ToolCatalog>,
    tail_policy: TailWatcherPolicy,
}

impl Runtime {
    /// Start a builder for a project loaded from disk. The caller is
    /// responsible for ensuring `project_root/.zymi/` is creatable.
    pub fn builder(workspace: WorkspaceConfig, project_root: impl Into<PathBuf>) -> RuntimeBuilder {
        RuntimeBuilder {
            workspace,
            project_root: project_root.into(),
            store: None,
            bus: None,
            approval: None,
            action_executor: None,
            tail_policy: None,
        }
    }

    pub fn workspace(&self) -> &WorkspaceConfig {
        &self.workspace
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn store(&self) -> &Arc<dyn EventStore> {
        &self.store
    }

    pub fn bus(&self) -> &Arc<EventBus> {
        &self.bus
    }

    pub fn provider(&self) -> &Arc<dyn LlmProvider> {
        &self.provider
    }

    pub fn contracts(&self) -> &Arc<ContractEngine> {
        &self.contracts
    }

    pub fn orchestrator(&self) -> &Arc<Orchestrator> {
        &self.orchestrator
    }

    pub fn approval_handler(&self) -> Option<&Arc<dyn ApprovalHandler>> {
        self.approval.as_ref()
    }

    pub fn action_executor(&self) -> &Arc<dyn ActionExecutor> {
        &self.action_executor
    }

    pub fn tool_catalog(&self) -> &Arc<ToolCatalog> {
        &self.tool_catalog
    }

    /// The cross-process [`crate::events::store::StoreTailWatcher`] policy
    /// (poll interval, batch size, catch-up cap, lag warn threshold) used
    /// when this runtime spawns a watcher. Slice 4 of the runtime
    /// unification (ADR-0013) lifted these knobs from a hidden constant in
    /// `events::store::watcher.rs` into this typed runtime contract.
    pub fn tail_policy(&self) -> &TailWatcherPolicy {
        &self.tail_policy
    }
}

/// Builder for [`Runtime`]. The defaults match what the old engine
/// entrypoints constructed inline:
///
/// - SQLite event store at `<project_root>/.zymi/events.db`
/// - Fresh in-process [`EventBus`] over that store
/// - LLM provider built from `workspace.project.llm`
/// - [`BuiltinActionExecutor`] for approved tool calls
/// - No approval handler (callers that need one pass it explicitly)
/// - Default [`TailWatcherPolicy`] (100 ms poll, 256-event batches,
///   16-batch catch-up cap, warn at 4096 events lag)
///
/// Tests / advanced callers may inject their own store, bus, action
/// executor, or tail policy via the `with_*` methods.
pub struct RuntimeBuilder {
    workspace: WorkspaceConfig,
    project_root: PathBuf,
    store: Option<Arc<dyn EventStore>>,
    bus: Option<Arc<EventBus>>,
    approval: Option<Arc<dyn ApprovalHandler>>,
    action_executor: Option<Arc<dyn ActionExecutor>>,
    tail_policy: Option<TailWatcherPolicy>,
}

impl RuntimeBuilder {
    /// Inject an existing event store. When omitted, the builder opens a
    /// SQLite store at `<project_root>/.zymi/events.db` and creates the
    /// directory if needed.
    pub fn with_store(mut self, store: Arc<dyn EventStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Inject an existing event bus. Must wrap the same store passed via
    /// [`Self::with_store`] (the builder does not check this — caller's
    /// responsibility). When omitted, a fresh bus over the configured store
    /// is created.
    pub fn with_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Attach an approval handler. Without one, intentions tagged
    /// `RequiresHumanApproval` resolve to [`crate::esaa::orchestrator::OrchestratorResult::NoApprovalHandler`]
    /// — the safety contract is fail-closed by design.
    pub fn with_approval_handler(mut self, handler: Arc<dyn ApprovalHandler>) -> Self {
        self.approval = Some(handler);
        self
    }

    /// Inject a custom action executor (e.g. an MCP-backed runner). When
    /// omitted, [`BuiltinActionExecutor`] is used.
    pub fn with_action_executor(mut self, executor: Arc<dyn ActionExecutor>) -> Self {
        self.action_executor = Some(executor);
        self
    }

    /// Override the cross-process [`crate::events::store::StoreTailWatcher`]
    /// policy. Use this to tune poll interval, batch size, the catch-up
    /// cap, or the lag-warn threshold; defaults to
    /// [`TailWatcherPolicy::default`].
    pub fn with_tail_policy(mut self, policy: TailWatcherPolicy) -> Self {
        self.tail_policy = Some(policy);
        self
    }

    /// Finalise the runtime. Returns an error if the project's LLM section
    /// is missing or the SQLite store cannot be opened.
    pub fn build(self) -> Result<Runtime, String> {
        let RuntimeBuilder {
            workspace,
            project_root,
            store,
            bus,
            approval,
            action_executor,
            tail_policy,
        } = self;

        // 1. Store + bus — either injected together, or build the SQLite
        //    default at <project_root>/.zymi/events.db.
        let store: Arc<dyn EventStore> = match store {
            Some(s) => s,
            None => {
                let store_dir = project_root.join(".zymi");
                std::fs::create_dir_all(&store_dir)
                    .map_err(|e| format!("failed to create .zymi directory: {e}"))?;
                let db_path = store_dir.join("events.db");
                open_store(StoreBackend::Sqlite { path: db_path })
                    .map_err(|e| format!("failed to open event store: {e}"))?
            }
        };
        let bus = bus.unwrap_or_else(|| Arc::new(EventBus::new(Arc::clone(&store))));

        // 2. Policy → contracts → orchestrator. Same wiring the old
        //    engine::run_pipeline_for_request did inline, just hoisted up.
        let policy = Arc::new(PolicyEngine::new(workspace.project.policy.clone()));
        let contracts = Arc::new(ContractEngine::new(
            policy,
            workspace.project.contracts.file_write.clone(),
        ));
        let orchestrator = Arc::new(Orchestrator::new(Arc::clone(&contracts), Arc::clone(&bus)));

        // 3. LLM provider. Required: a Runtime that cannot run any pipeline
        //    is not a useful Runtime, so fail at build time rather than at
        //    first command dispatch.
        let llm_config = workspace
            .project
            .llm
            .as_ref()
            .ok_or("no 'llm' section in project.yml — configure a provider to run pipelines")?;
        let provider: Arc<dyn LlmProvider> = Arc::from(
            llm::create_provider(llm_config)
                .map_err(|e| format!("failed to create LLM provider: {e}"))?,
        );

        // 4. Tool catalog — built-in tools + declarative tools from
        //    workspace.tools (tools/*.yml). Programmatic registrations
        //    come later via with_programmatic_tool().
        let tool_catalog = Arc::new(
            ToolCatalog::with_declarative(&workspace.tools)
                .map_err(|e| format!("tool catalog error: {e}"))?,
        );

        // 5. Action executor — defaults to the catalog-aware dispatcher.
        let action_executor: Arc<dyn ActionExecutor> = action_executor
            .unwrap_or_else(|| Arc::new(CatalogActionExecutor::new(Arc::clone(&tool_catalog))) as Arc<dyn ActionExecutor>);

        Ok(Runtime {
            workspace: Arc::new(workspace),
            project_root,
            store,
            bus,
            provider,
            contracts,
            orchestrator,
            approval,
            action_executor,
            tool_catalog,
            tail_policy: tail_policy.unwrap_or_default(),
        })
    }
}
