pub mod approval;
#[cfg(feature = "cli")]
pub mod cli;
pub mod config;
#[cfg(feature = "runtime")]
pub mod engine;
pub mod esaa;
pub mod events;
#[cfg(feature = "runtime")]
pub mod llm;
pub mod policy;
#[cfg(feature = "python")]
#[allow(clippy::useless_conversion)]
pub mod python;
pub mod types;
#[cfg(feature = "services")]
pub mod services;
#[cfg(feature = "webhook")]
pub mod webhook;

// Re-export core types at crate root for convenience
pub use approval::{ApprovalHandler, SharedApprovalHandler};
pub use esaa::{
    contracts::{ContractEngine, FileWriteContract, RateLimitConfig},
    orchestrator::{Orchestrator, OrchestratorResult},
    projections::{ConversationProjection, MetricsProjection, Projection},
    Intention, IntentionVerdict,
};
pub use events::{
    bus::EventBus,
    connector::{ConnectorError, EventDrivenConnector},
    store::{
        open_store, EventStore, SqliteEventStore, StoreBackend, StoreTailWatcher, TailedEvent,
        WatcherHandle,
    },
    stream_registry::StreamRegistry,
    Event, EventKind, EventStoreError,
};
pub use policy::{PolicyConfig, PolicyDecision, PolicyEngine};
pub use config::{
    build_execution_plan, load_project_dir, AgentConfig, ConfigError, ContractsConfig,
    DefaultsConfig, ExecutionPlan, LangfuseConfig, LlmConfig, PipelineConfig, PipelineInput,
    PipelineOutput, PipelineStep, ProjectConfig, ServicesConfig, WorkspaceConfig,
};
#[cfg(feature = "services")]
pub use services::{
    langfuse::LangfuseService, start_configured_services, EventService, ServiceError,
    ServiceRunner,
};
#[cfg(feature = "runtime")]
pub use llm::{create_provider, ChatRequest, ChatResponse, LlmError, LlmProvider};
pub use types::{Message, StreamEvent, TokenUsage, ToolCallInfo, ToolDefinition};
