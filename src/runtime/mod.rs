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
pub mod context_builder;
pub mod context_window;
pub mod event_router;
pub mod shell_session;
pub mod tool_catalog;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::collections::HashMap;
use std::time::Duration;

use crate::approval::ApprovalHandler;
use crate::config::{McpServerConfig, WorkspaceConfig};
use crate::esaa::contracts::ContractEngine;
use crate::esaa::orchestrator::Orchestrator;
use crate::events::bus::EventBus;
use crate::events::store::{open_store, EventStore, StoreBackend, TailWatcherPolicy};
use crate::events::{Event, EventKind};
use crate::llm::{self, LlmProvider};
use crate::mcp::{McpRegistry, McpServerConnection, McpServerSpec, McpTool};
use crate::policy::PolicyEngine;

pub use action_executor::{ActionContext, ActionExecutor, BuiltinActionExecutor, CatalogActionExecutor};
pub use event_router::EventCommandRouter;
pub use shell_session::ShellSessionPool;
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
    shell_pool: Arc<ShellSessionPool>,
    tail_policy: TailWatcherPolicy,
    /// Live MCP connections, if any were spawned at startup via
    /// [`RuntimeBuilder::build_async`]. `None` when the project's
    /// `mcp_servers:` list was empty or when the sync [`RuntimeBuilder::build`]
    /// path was used.
    mcp_registry: Option<Arc<McpRegistry>>,
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
            provider: None,
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

    pub fn shell_pool(&self) -> &Arc<ShellSessionPool> {
        &self.shell_pool
    }

    /// The cross-process [`crate::events::store::StoreTailWatcher`] policy
    /// (poll interval, batch size, catch-up cap, lag warn threshold) used
    /// when this runtime spawns a watcher. Slice 4 of the runtime
    /// unification (ADR-0013) lifted these knobs from a hidden constant in
    /// `events::store::watcher.rs` into this typed runtime contract.
    pub fn tail_policy(&self) -> &TailWatcherPolicy {
        &self.tail_policy
    }

    /// Live MCP server registry, if any MCP servers were spawned at
    /// startup via [`RuntimeBuilder::build_async`].
    pub fn mcp_registry(&self) -> Option<&Arc<McpRegistry>> {
        self.mcp_registry.as_ref()
    }

    /// Best-effort shutdown of every live MCP server. Publishes one
    /// [`EventKind::McpServerDisconnected`] per server with
    /// `reason = "shutdown"`. Idempotent; a second call is a no-op because
    /// [`crate::mcp::McpServerConnection::shutdown`] is idempotent.
    pub async fn shutdown_mcp(&self) {
        let Some(registry) = self.mcp_registry.as_ref() else {
            return;
        };
        let names: Vec<String> = registry.server_names().map(|s| s.to_string()).collect();
        registry.shutdown_all(MCP_SHUTDOWN_GRACE).await;
        for name in names {
            publish_system_event(
                self.bus(),
                EventKind::McpServerDisconnected {
                    server: name,
                    reason: "shutdown".into(),
                },
            )
            .await;
        }
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
    provider: Option<Arc<dyn LlmProvider>>,
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

    /// Inject an LLM provider, bypassing the `project.llm` config. Primarily
    /// for tests that drive the engine with a mock provider; production
    /// callers should let the builder construct the provider from config.
    pub fn with_llm_provider(mut self, provider: Arc<dyn LlmProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Finalise the runtime. Returns an error if the project's LLM section
    /// is missing or the SQLite store cannot be opened.
    ///
    /// Note: this synchronous path does **not** spawn MCP servers even when
    /// `project.mcp_servers` is populated — startup requires async I/O. Use
    /// [`Self::build_async`] instead whenever the project may declare MCP
    /// servers (all CLI entrypoints do).
    pub fn build(self) -> Result<Runtime, String> {
        self.build_inner(McpStartState::default())
    }

    /// Async finalise: spawns every entry in `project.mcp_servers`, filters
    /// their tools against `allow:` / `deny:`, registers them in the
    /// [`ToolCatalog`], and publishes one
    /// [`EventKind::McpServerConnected`] per server onto the bus.
    ///
    /// Equivalent to [`Self::build`] when the project declares no MCP
    /// servers; otherwise this is the only path that wires the
    /// [`McpRegistry`] into the action executor.
    pub async fn build_async(self) -> Result<Runtime, String> {
        let mcp_specs = self.workspace.project.mcp_servers.clone();
        if mcp_specs.is_empty() {
            return self.build_inner(McpStartState::default());
        }

        // We need the bus early so `spawn_mcp_servers` can wire it into the
        // registry (restart/crash events must be publishable mid-call). If
        // the caller didn't inject one, mirror `build_inner`'s default: reuse
        // the injected store or open the SQLite default.
        let bus_for_mcp = self.resolve_bus_for_startup()?;

        let outcome =
            spawn_mcp_servers(&mcp_specs, &self.project_root, Arc::clone(&bus_for_mcp)).await?;

        // Publish per-server startup failures before proceeding. The runtime
        // keeps building with whatever servers did connect — partial MCP
        // failure is not a startup error (ADR-0023 §Lifecycle).
        for failure in &outcome.failures {
            publish_system_event(
                &bus_for_mcp,
                EventKind::McpServerDisconnected {
                    server: failure.name.clone(),
                    reason: failure.reason.clone(),
                },
            )
            .await;
        }

        let self_with_bus = self.with_event_bus(Arc::clone(&bus_for_mcp));
        let runtime = self_with_bus.build_inner(outcome.state)?;

        // Publish one McpServerConnected per live server so the TUI / `zymi
        // runs` surface observes the startup lifecycle (ADR-0023).
        if let Some(registry) = runtime.mcp_registry.as_ref() {
            for name in registry.server_names() {
                let tool_count = runtime.tool_catalog.mcp_tool_count(name);
                publish_system_event(
                    runtime.bus(),
                    EventKind::McpServerConnected {
                        server: name.to_string(),
                        tool_count,
                    },
                )
                .await;
            }
        }

        Ok(runtime)
    }

    /// Resolve the event bus `build_inner` will end up using, so that
    /// [`spawn_mcp_servers`] can share it. If the caller injected both a bus
    /// and a store, reuse them. If the caller injected only a store, build
    /// the bus over it. Otherwise open the default SQLite store and bus —
    /// matching the untouched happy path in `build_inner`.
    fn resolve_bus_for_startup(&self) -> Result<Arc<EventBus>, String> {
        if let Some(bus) = self.bus.as_ref() {
            return Ok(Arc::clone(bus));
        }
        let store: Arc<dyn EventStore> = match self.store.as_ref() {
            Some(s) => Arc::clone(s),
            None => {
                let store_dir = self.project_root.join(".zymi");
                std::fs::create_dir_all(&store_dir)
                    .map_err(|e| format!("failed to create .zymi directory: {e}"))?;
                let db_path = store_dir.join("events.db");
                open_store(StoreBackend::Sqlite { path: db_path })
                    .map_err(|e| format!("failed to open event store: {e}"))?
            }
        };
        Ok(Arc::new(EventBus::new(store)))
    }

    fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        // Keep any already-injected store; only pin the bus so `build_inner`
        // doesn't open a second one.
        self.bus = Some(bus);
        self
    }

    fn build_inner(self, mcp_state: McpStartState) -> Result<Runtime, String> {
        let RuntimeBuilder {
            workspace,
            project_root,
            store,
            bus,
            approval,
            action_executor,
            tail_policy,
            provider: provider_override,
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
        //    first command dispatch. Tests may inject a mock via
        //    `with_llm_provider`, in which case `project.llm` is not consulted.
        let provider: Arc<dyn LlmProvider> = match provider_override {
            Some(p) => p,
            None => {
                let llm_config = workspace.project.llm.as_ref().ok_or(
                    "no 'llm' section in project.yml — configure a provider to run pipelines",
                )?;
                Arc::from(
                    llm::create_provider(llm_config)
                        .map_err(|e| format!("failed to create LLM provider: {e}"))?,
                )
            }
        };

        // 4. Tool catalog — built-in tools + declarative tools from
        //    workspace.tools (tools/*.yml) + MCP-backed tools from any
        //    server that completed its handshake. Programmatic
        //    registrations come later via with_programmatic_tool().
        let mut catalog = ToolCatalog::with_declarative(&workspace.tools)
            .map_err(|e| format!("tool catalog error: {e}"))?;
        for (server, (tools, requires_approval)) in &mcp_state.tools_by_server {
            catalog
                .add_mcp_server(server, tools, *requires_approval)
                .map_err(|e| format!("mcp catalog error: {e}"))?;
        }
        let tool_catalog = Arc::new(catalog);

        // 5. Persistent shell session pool (ADR-0015).
        //    Reads runtime.shell config from project.yml; falls back to
        //    ShellConfig::default() when the block is absent.
        let shell_config = workspace
            .project
            .runtime
            .as_ref()
            .map(|r| &r.shell)
            .cloned()
            .unwrap_or_default();
        let shell_pool = Arc::new(
            ShellSessionPool::from_config(&shell_config)
                .with_bus(Arc::clone(&bus)),
        );
        shell_pool.start_reaper();

        // 6. Action executor — defaults to the catalog-aware dispatcher.
        //    When MCP servers are wired, the dispatcher also owns an Arc to
        //    the registry so `mcp__*` tool calls route back to their
        //    originating subprocess.
        let action_executor: Arc<dyn ActionExecutor> = action_executor
            .unwrap_or_else(|| {
                let mut exec = CatalogActionExecutor::new(
                    Arc::clone(&tool_catalog),
                    Arc::clone(&shell_pool),
                );
                if let Some(reg) = mcp_state.registry.clone() {
                    exec = exec.with_mcp(reg);
                }
                Arc::new(exec) as Arc<dyn ActionExecutor>
            });

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
            shell_pool,
            tail_policy: tail_policy.unwrap_or_default(),
            mcp_registry: mcp_state.registry,
        })
    }
}

/// Intermediate state produced by [`spawn_mcp_servers`] and consumed by
/// [`RuntimeBuilder::build_inner`]. The default value represents "no MCP
/// servers configured" and is used by the sync [`RuntimeBuilder::build`]
/// path.
#[derive(Debug, Default)]
struct McpStartState {
    registry: Option<Arc<McpRegistry>>,
    tools_by_server: HashMap<String, (Vec<McpTool>, bool)>,
}

/// MCP shutdown grace. Kept local to the runtime: per-server handshake and
/// call timeouts live in [`crate::config::McpServerConfig`] (Slice 5).
const MCP_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

/// Outcome of `spawn_mcp_servers` — the live registry plus a list of
/// per-server startup failures. Failures are surfaced as
/// [`EventKind::McpServerDisconnected`] by the caller so the TUI /
/// `zymi runs` can show them.
#[derive(Debug, Default)]
struct SpawnOutcome {
    state: McpStartState,
    failures: Vec<FailedServer>,
}

#[derive(Debug, Clone)]
struct FailedServer {
    name: String,
    reason: String,
}

/// Spawn every MCP server in `specs`, perform the `initialize` handshake,
/// enumerate tools, and apply the `allow:` / `deny:` filter. Returns the
/// live registry plus the filtered tool lists for catalog registration.
///
/// **Best-effort (Slice 5):** a subprocess that fails to spawn or fails its
/// handshake no longer aborts startup — the registry proceeds with the
/// servers that did connect, and the caller publishes
/// [`EventKind::McpServerDisconnected`] with a machine-readable `reason`
/// prefix (`spawn_failed`, `init_timeout`, `init_failed`,
/// `tools_list_failed`).
///
/// Config-shape errors (duplicate names, `allow` AND `deny`, `__` in names,
/// empty command) remain hard errors — they're programmer errors, not
/// runtime failures.
async fn spawn_mcp_servers(
    specs: &[McpServerConfig],
    project_root: &Path,
    bus: Arc<EventBus>,
) -> Result<SpawnOutcome, String> {
    // Pre-flight validation — all config-shape errors surface before we
    // spawn any subprocess, so a later duplicate-name doesn't get masked
    // by an earlier subprocess's unrelated spawn failure.
    let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for cfg in specs {
        if cfg.allow.is_some() && cfg.deny.is_some() {
            return Err(format!(
                "mcp server '{}': 'allow' and 'deny' are mutually exclusive",
                cfg.name
            ));
        }
        if !seen_names.insert(cfg.name.as_str()) {
            return Err(format!("duplicate mcp_servers entry '{}'", cfg.name));
        }
        crate::mcp::validate_segment(&cfg.name)
            .map_err(|e| format!("mcp server name '{}': {e}", cfg.name))?;
        if cfg.command.is_empty() {
            return Err(format!("mcp server '{}': command is empty", cfg.name));
        }
    }

    let mut registry = McpRegistry::new().with_bus(bus);
    let mut tools_by_server: HashMap<String, (Vec<McpTool>, bool)> = HashMap::new();
    let mut failures: Vec<FailedServer> = Vec::new();

    for cfg in specs {
        let spec = McpServerSpec {
            name: cfg.name.clone(),
            command: cfg.command.clone(),
            env: cfg.env.clone(),
            cwd: Some(project_root.to_path_buf()),
        };
        let init_timeout = Duration::from_secs(cfg.effective_init_timeout_secs());
        let call_timeout = Duration::from_secs(cfg.effective_call_timeout_secs());

        let conn = match McpServerConnection::connect(spec.clone(), init_timeout, call_timeout)
            .await
        {
            Ok(c) => c,
            Err(err) => {
                failures.push(FailedServer {
                    name: cfg.name.clone(),
                    reason: classify_startup_error(&err),
                });
                continue;
            }
        };

        let advertised = match conn.list_tools().await {
            Ok(t) => t,
            Err(err) => {
                // The subprocess is alive but not usable; tear it down before
                // moving on so we don't leak a zombie.
                conn.shutdown(MCP_SHUTDOWN_GRACE).await;
                failures.push(FailedServer {
                    name: cfg.name.clone(),
                    reason: format!("tools_list_failed: {err}"),
                });
                continue;
            }
        };

        // Per ADR-0023 §security posture: opt-in UX. When a server
        // advertises tools but neither `allow:` nor `deny:` was specified,
        // import nothing and warn loudly — matches the "first-run"
        // message from the ADR.
        if cfg.allow.is_none() && cfg.deny.is_none() && !advertised.is_empty() {
            log::warn!(
                "mcp server '{}' exposes {} tool(s) but no `allow:` or `deny:` was specified — \
                 none registered. Add `allow: [tool1, tool2]` to opt in, or `deny: []` to accept all",
                cfg.name,
                advertised.len()
            );
        }

        let filtered =
            filter_mcp_tools(&cfg.name, cfg.allow.as_deref(), cfg.deny.as_deref(), advertised);

        let conn = Arc::new(conn);
        match build_restart_policy(cfg, &spec, init_timeout, call_timeout) {
            Some(policy) => registry.insert_with_restart(cfg.name.clone(), conn, policy),
            None => registry.insert(cfg.name.clone(), conn),
        }

        tools_by_server.insert(
            cfg.name.clone(),
            (filtered, cfg.requires_approval.unwrap_or(false)),
        );
    }

    let state = McpStartState {
        registry: Some(Arc::new(registry)),
        tools_by_server,
    };
    Ok(SpawnOutcome { state, failures })
}

fn classify_startup_error(err: &crate::mcp::McpError) -> String {
    use crate::mcp::McpError;
    match err {
        McpError::Spawn(e) => format!("spawn_failed: {e}"),
        McpError::Transport(crate::mcp::TransportError::Timeout) => "init_timeout".into(),
        McpError::Config(msg) => format!("init_failed: {msg}"),
        other => format!("init_failed: {other}"),
    }
}

fn build_restart_policy(
    cfg: &McpServerConfig,
    spec: &McpServerSpec,
    init_timeout: Duration,
    call_timeout: Duration,
) -> Option<crate::mcp::RestartPolicy> {
    let restart_cfg = cfg.restart.as_ref()?;
    let max = restart_cfg.effective_max_restarts();
    if max == 0 {
        return None;
    }
    Some(crate::mcp::RestartPolicy {
        spec: spec.clone(),
        init_timeout,
        call_timeout,
        max_restarts: max,
        backoff_secs: restart_cfg.backoff_secs.clone().unwrap_or_else(|| vec![1]),
    })
}

/// Apply ADR-0023 §YAML surface filter semantics:
/// * `allow: None, deny: None` → import zero tools (opt-in UX).
/// * `allow: Some(list)` → import only names in the list.
/// * `deny: Some(list)` → import everything not in the list.
/// * Both set → caller rejects before we get here.
fn filter_mcp_tools(
    server: &str,
    allow: Option<&[String]>,
    deny: Option<&[String]>,
    advertised: Vec<McpTool>,
) -> Vec<McpTool> {
    match (allow, deny) {
        (None, None) => Vec::new(),
        (Some(allow), None) => {
            let advertised_names: std::collections::HashSet<&str> =
                advertised.iter().map(|t| t.name.as_str()).collect();
            for requested in allow {
                if !advertised_names.contains(requested.as_str()) {
                    log::warn!(
                        "mcp server '{server}' allow: requested tool '{requested}' not advertised by server — skipping"
                    );
                }
            }
            advertised
                .into_iter()
                .filter(|t| allow.iter().any(|n| n == &t.name))
                .collect()
        }
        (None, Some(deny)) => advertised
            .into_iter()
            .filter(|t| !deny.iter().any(|n| n == &t.name))
            .collect(),
        (Some(_), Some(_)) => Vec::new(),
    }
}

async fn publish_system_event(bus: &EventBus, kind: EventKind) {
    let event = Event::new("system".into(), kind, "mcp".into());
    if let Err(e) = bus.publish(event).await {
        log::warn!("failed to publish mcp lifecycle event: {e}");
    }
}

#[cfg(test)]
mod mcp_tests {
    use super::*;

    fn tool(name: &str) -> McpTool {
        McpTool {
            name: name.into(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn filter_no_allow_no_deny_imports_nothing() {
        let got = filter_mcp_tools("srv", None, None, vec![tool("a"), tool("b")]);
        assert!(got.is_empty(), "opt-in UX: empty allow/deny imports zero tools");
    }

    #[test]
    fn filter_allow_keeps_only_listed() {
        let allow = vec!["a".to_string(), "c".to_string()];
        let got = filter_mcp_tools(
            "srv",
            Some(&allow),
            None,
            vec![tool("a"), tool("b"), tool("c")],
        );
        let names: Vec<&str> = got.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn filter_deny_empty_keeps_everything() {
        let deny: Vec<String> = Vec::new();
        let got = filter_mcp_tools("srv", None, Some(&deny), vec![tool("a"), tool("b")]);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn filter_deny_removes_listed() {
        let deny = vec!["b".to_string()];
        let got = filter_mcp_tools(
            "srv",
            None,
            Some(&deny),
            vec![tool("a"), tool("b"), tool("c")],
        );
        let names: Vec<&str> = got.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn filter_allow_for_unknown_tool_is_warned_not_errored() {
        // Requesting a tool the server doesn't expose is not fatal — the
        // warning is logged, the missing tool is simply absent. This
        // matches how declarative HTTP tools tolerate config drift.
        let allow = vec!["present".to_string(), "ghost".to_string()];
        let got = filter_mcp_tools("srv", Some(&allow), None, vec![tool("present")]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "present");
    }

    fn make_cfg(name: &str, command: Vec<String>) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            command,
            env: HashMap::new(),
            allow: None,
            deny: None,
            requires_approval: None,
            init_timeout_secs: Some(1),
            call_timeout_secs: Some(1),
            restart: None,
        }
    }

    fn test_bus() -> Arc<EventBus> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp_test.db");
        // Leak the tempdir — the file is only opened once per test and
        // teardown on process exit is fine.
        Box::leak(Box::new(dir));
        let store = open_store(StoreBackend::Sqlite { path }).expect("open store");
        Arc::new(EventBus::new(store))
    }

    #[tokio::test]
    async fn spawn_rejects_allow_and_deny_together() {
        let mut cfg = make_cfg("x", vec!["true".into()]);
        cfg.allow = Some(vec!["a".into()]);
        cfg.deny = Some(vec!["b".into()]);
        let err = spawn_mcp_servers(&[cfg], Path::new("."), test_bus()).await.unwrap_err();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[tokio::test]
    async fn spawn_rejects_duplicate_server_names() {
        let cfgs = vec![
            make_cfg("gh", vec!["true".into()]),
            make_cfg("gh", vec!["true".into()]),
        ];
        let err = spawn_mcp_servers(&cfgs, Path::new("."), test_bus()).await.unwrap_err();
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[tokio::test]
    async fn spawn_rejects_double_underscore_in_server_name() {
        let cfg = make_cfg("evil__name", vec!["true".into()]);
        let err = spawn_mcp_servers(&[cfg], Path::new("."), test_bus()).await.unwrap_err();
        assert!(err.contains("__"), "got: {err}");
    }

    #[tokio::test]
    async fn spawn_collects_nonexistent_binary_as_failure_not_error() {
        // Slice 5: per-server spawn failures no longer abort startup — they
        // go into the `failures` list so the caller can publish
        // McpServerDisconnected events and keep running.
        let cfg = make_cfg(
            "ghost",
            vec!["/definitely/does/not/exist/zymi-mcp-test".into()],
        );
        let outcome = spawn_mcp_servers(&[cfg], Path::new("."), test_bus()).await.unwrap();
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].name, "ghost");
        assert!(
            outcome.failures[0].reason.starts_with("spawn_failed:"),
            "reason should prefix 'spawn_failed:', got {}",
            outcome.failures[0].reason
        );
        // The registry is still created — just with zero servers.
        let registry = outcome.state.registry.expect("registry populated");
        assert_eq!(registry.server_names().count(), 0);
    }

    #[tokio::test]
    async fn spawn_classifies_init_timeout() {
        // `cat` is alive and speaks stdio, but never answers `initialize`,
        // so the handshake times out. We classify that as `init_timeout`,
        // not `init_failed` — observability distinguishes "server is up but
        // broken" from "server returned a structured error".
        if std::process::Command::new("cat")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // skip on platforms without cat
        }
        let cfg = make_cfg("silent", vec!["cat".into()]);
        let outcome = spawn_mcp_servers(&[cfg], Path::new("."), test_bus()).await.unwrap();
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].reason, "init_timeout");
    }

    #[test]
    fn classify_spawn_failed() {
        use std::io;
        let err = crate::mcp::McpError::Spawn(io::Error::new(io::ErrorKind::NotFound, "no such"));
        let reason = classify_startup_error(&err);
        assert!(reason.starts_with("spawn_failed:"), "got: {reason}");
    }

    #[test]
    fn classify_transport_timeout() {
        let err = crate::mcp::McpError::Transport(crate::mcp::TransportError::Timeout);
        assert_eq!(classify_startup_error(&err), "init_timeout");
    }

    #[test]
    fn classify_other_init_errors() {
        let err = crate::mcp::McpError::Config("bad shape".into());
        let reason = classify_startup_error(&err);
        assert!(reason.starts_with("init_failed:"), "got: {reason}");
    }

    #[test]
    fn build_restart_policy_none_when_no_restart_cfg() {
        let cfg = make_cfg("x", vec!["true".into()]);
        let spec = McpServerSpec {
            name: "x".into(),
            command: vec!["true".into()],
            env: HashMap::new(),
            cwd: None,
        };
        let policy = build_restart_policy(
            &cfg,
            &spec,
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        assert!(policy.is_none());
    }

    #[test]
    fn build_restart_policy_none_when_max_restarts_zero() {
        let mut cfg = make_cfg("x", vec!["true".into()]);
        cfg.restart = Some(crate::config::McpRestartConfig {
            max_restarts: Some(0),
            backoff_secs: Some(vec![1]),
        });
        let spec = McpServerSpec {
            name: "x".into(),
            command: vec!["true".into()],
            env: HashMap::new(),
            cwd: None,
        };
        let policy = build_restart_policy(
            &cfg,
            &spec,
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        assert!(policy.is_none(), "max_restarts=0 disables restart entirely");
    }

    #[test]
    fn build_restart_policy_carries_fields() {
        let mut cfg = make_cfg("x", vec!["true".into()]);
        cfg.restart = Some(crate::config::McpRestartConfig {
            max_restarts: Some(3),
            backoff_secs: Some(vec![2, 4, 8]),
        });
        let spec = McpServerSpec {
            name: "x".into(),
            command: vec!["true".into()],
            env: HashMap::new(),
            cwd: None,
        };
        let policy = build_restart_policy(
            &cfg,
            &spec,
            Duration::from_secs(5),
            Duration::from_secs(30),
        )
        .expect("policy built");
        assert_eq!(policy.max_restarts, 3);
        assert_eq!(policy.backoff_secs, vec![2, 4, 8]);
        assert_eq!(policy.init_timeout, Duration::from_secs(5));
        assert_eq!(policy.call_timeout, Duration::from_secs(30));
    }
}
