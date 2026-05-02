//! Legacy module — kept only for `engine::tools` (memory-store helpers and
//! built-in tool execution) which the runtime modules still reach into.
//!
//! The deprecated `engine::run_pipeline` / `engine::run_pipeline_for_request`
//! wrappers were removed alongside the ApprovalHandler trait when ADR-0022
//! moved approvals onto the bus (see [`crate::approval::ApprovalChannel`] and
//! [`crate::esaa::orchestrator::ApprovalContext`]). Callers should construct a
//! [`crate::runtime::Runtime`] directly and dispatch
//! [`crate::commands::RunPipeline`] commands at it via
//! [`crate::handlers::run_pipeline::handle`].

pub mod tools;

pub use crate::handlers::run_pipeline::{PipelineResult, StepResult};
