//! Deprecated thin wrappers around the [`crate::runtime::Runtime`] +
//! [`crate::handlers::run_pipeline`] command/handler shape introduced in
//! slice 1 of the runtime unification (ADR-0013).
//!
//! New callers should construct a [`Runtime`] once per project and dispatch
//! [`RunPipeline`] commands at it directly. These wrappers exist purely so
//! existing call sites (and the engine's own integration tests) keep
//! compiling while the migration completes.
//!
//! [`Runtime`]: crate::runtime::Runtime
//! [`RunPipeline`]: crate::commands::RunPipeline

pub mod tools;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use uuid::Uuid;

use crate::approval::ApprovalHandler;
use crate::commands::RunPipeline;
use crate::config::{PipelineConfig, WorkspaceConfig};
use crate::events::bus::EventBus;
use crate::events::store::EventStore;
use crate::handlers::run_pipeline as handler;
use crate::runtime::Runtime;

pub use handler::{PipelineResult, StepResult};

/// Execute a full pipeline end-to-end. Used by `zymi run` historically —
/// now a thin wrapper that builds a [`Runtime`] and dispatches a
/// [`RunPipeline`] command.
///
/// `approval_handler` is honoured for any tool call that the contract layer
/// flags as `RequiresHumanApproval`. Pass `None` only in non-interactive
/// contexts (tests, dry runs); production entrypoints must pass a real
/// handler so the safety contract is not silently downgraded.
#[deprecated(
    since = "0.1.3",
    note = "use Runtime + commands::RunPipeline + handlers::run_pipeline::handle"
)]
pub async fn run_pipeline(
    workspace: &WorkspaceConfig,
    pipeline: &PipelineConfig,
    project_root: &Path,
    inputs: &HashMap<String, String>,
    approval_handler: Option<Arc<dyn ApprovalHandler>>,
) -> Result<PipelineResult, String> {
    let mut builder = Runtime::builder(workspace.clone(), project_root.to_path_buf());
    if let Some(h) = approval_handler {
        builder = builder.with_approval_handler(h);
    }
    let runtime = builder.build()?;
    let cmd = RunPipeline::new(pipeline.name.clone(), inputs.clone());
    handler::handle(&runtime, cmd).await
}

/// Execute a pipeline using a pre-existing bus/store and a caller-supplied
/// `correlation_id`. Historically used by `zymi serve`; now a thin wrapper
/// over the [`Runtime`] / [`RunPipeline`] handler.
///
/// The wrapper generates a fresh stream id internally (`stream_id = None`)
/// to match the previous behaviour where each request started a fresh
/// stream while sharing the originating `correlation_id`.
///
/// See [`run_pipeline`] for the `approval_handler` contract.
#[deprecated(
    since = "0.1.3",
    note = "use Runtime::builder().with_store/.with_bus + commands::RunPipeline::from_request"
)]
#[allow(clippy::too_many_arguments)]
pub async fn run_pipeline_for_request(
    workspace: &WorkspaceConfig,
    pipeline: &PipelineConfig,
    project_root: &Path,
    inputs: &HashMap<String, String>,
    bus: Arc<EventBus>,
    store: Arc<dyn EventStore>,
    correlation_id: Uuid,
    approval_handler: Option<Arc<dyn ApprovalHandler>>,
) -> Result<PipelineResult, String> {
    let mut builder = Runtime::builder(workspace.clone(), project_root.to_path_buf())
        .with_store(store)
        .with_bus(bus);
    if let Some(h) = approval_handler {
        builder = builder.with_approval_handler(h);
    }
    let runtime = builder.build()?;
    let cmd = RunPipeline {
        pipeline_name: pipeline.name.clone(),
        inputs: inputs.clone(),
        correlation_id,
        stream_id: None,
    };
    handler::handle(&runtime, cmd).await
}
