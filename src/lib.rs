pub mod approval;
#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "runtime")]
pub mod commands;
pub mod config;
#[cfg(feature = "connectors")]
pub mod connectors;
#[cfg(feature = "runtime")]
pub mod engine;
pub mod esaa;
pub mod events;
#[cfg(feature = "runtime")]
pub mod handlers;
pub mod mcp;
#[cfg(feature = "runtime")]
pub mod llm;
pub mod plugin;
pub mod policy;
#[cfg(feature = "python")]
#[allow(clippy::useless_conversion)]
pub mod python;
#[cfg(feature = "runtime")]
pub mod runtime;
pub mod types;
#[cfg(feature = "services")]
pub mod services;
#[cfg(feature = "webhook")]
pub mod webhook;

// Re-export core types at crate root for convenience
pub use approval::{ApprovalChannel, ChannelHandle, TerminalApprovalChannel};
pub use esaa::{
    contracts::{ContractEngine, FileWriteContract, RateLimitConfig},
    orchestrator::{Orchestrator, OrchestratorResult},
    projections::{ConversationProjection, MemoryProjection, MetricsProjection, Projection},
    Intention, IntentionVerdict,
};
pub use events::{
    bus::EventBus,
    connector::{ConnectorError, EventDrivenConnector},
    store::{
        open_store, EventStore, SqliteEventStore, StoreBackend, StoreTailWatcher,
        TailWatcherPolicy, TailedEvent, WatcherHandle,
    },
    stream_registry::StreamRegistry,
    Event, EventKind, EventStoreError,
};
pub use policy::{PolicyConfig, PolicyDecision, PolicyEngine};
pub use config::{
    build_execution_plan, load_project_dir, AgentConfig, ConfigError, ContractsConfig,
    DefaultsConfig, ExecutionPlan, LangfuseConfig, LlmConfig, PipelineConfig, PipelineInput,
    PipelineOutput, PipelineStep, ProjectConfig, ServicesConfig, WorkspaceConfig, SCHEMA_VERSION,
};
pub use plugin::{PluginBuildError, PluginBuilder, PluginRegistry};
#[cfg(feature = "services")]
pub use services::{
    langfuse::LangfuseService, start_configured_services, EventService, ServiceError,
    ServiceRunner,
};
#[cfg(feature = "runtime")]
pub use llm::{create_provider, ChatRequest, ChatResponse, LlmError, LlmProvider};
#[cfg(feature = "runtime")]
pub use commands::RunPipeline;
#[cfg(feature = "runtime")]
pub use runtime::{
    ActionContext, ActionExecutor, BuiltinActionExecutor, CatalogActionExecutor,
    EventCommandRouter, Runtime, RuntimeBuilder, ToolCatalog,
};
pub use types::{Message, StreamEvent, TokenUsage, ToolCallInfo, ToolDefinition};
