//! Command types — slice 1 of the runtime unification (ADR-0013).
//!
//! Commands are the inputs to [`crate::handlers`]. They carry the user
//! intent, never the wiring: handlers always pull store/bus/providers
//! from a [`crate::runtime::Runtime`].
//!
//! ADR-0013 v1.1 lists four target commands: `RunPipeline`,
//! `ProcessConversation`, `DecideApproval`, `ReplayStream`. Slice 1 only
//! ships [`RunPipeline`]; the others are added when a real caller appears.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

/// Execute a pipeline by name with the given inputs.
///
/// `correlation_id` and `stream_id` exist so that commands originating from
/// a cross-process [`crate::events::EventKind::PipelineRequested`] event can
/// be reattached to the same correlation/stream as their request, instead of
/// minting fresh ones. The synchronous `zymi run` path generates them
/// locally via [`RunPipeline::new`].
///
/// The optional [`resume`](Self::resume) field carries fork-resume context
/// (ADR-0018). When present, the handler skips its own
/// [`crate::events::EventKind::PipelineRequested`] emission (the resume
/// orchestrator already wrote it), seeds `step_outputs` from
/// [`ResumeContext::frozen_outputs`], does not execute any step whose id is
/// in [`ResumeContext::frozen_step_ids`], and still emits a
/// `PipelineCompleted` envelope at the end (so `zymi runs` and `zymi observe`
/// see the new stream as a complete run).
#[derive(Debug, Clone)]
pub struct RunPipeline {
    pub pipeline_name: String,
    pub inputs: HashMap<String, String>,
    pub correlation_id: Uuid,
    pub stream_id: Option<String>,
    pub resume: Option<ResumeContext>,
}

/// Frozen prefix carried over from a parent stream when forking a resume.
///
/// Built by [`crate::handlers::resume_pipeline`] from the parent stream and
/// the current pipeline DAG. The handler treats every step in
/// `frozen_step_ids` as an upstream dependency whose output is already known
/// (`frozen_outputs[step_id]`) and must not be re-executed.
#[derive(Debug, Clone)]
pub struct ResumeContext {
    pub parent_stream_id: String,
    pub fork_at_step: String,
    pub frozen_outputs: HashMap<String, String>,
    pub frozen_step_ids: HashSet<String>,
}

impl RunPipeline {
    /// Construct a sync command with a fresh correlation id and an
    /// auto-generated stream id (the handler will derive one from the
    /// pipeline name).
    pub fn new(pipeline_name: impl Into<String>, inputs: HashMap<String, String>) -> Self {
        Self {
            pipeline_name: pipeline_name.into(),
            inputs,
            correlation_id: Uuid::new_v4(),
            stream_id: None,
            resume: None,
        }
    }

    /// Construct a command pre-bound to an existing correlation/stream —
    /// used by `zymi serve` when translating a `PipelineRequested` event.
    pub fn from_request(
        pipeline_name: impl Into<String>,
        inputs: HashMap<String, String>,
        correlation_id: Uuid,
        stream_id: impl Into<String>,
    ) -> Self {
        Self {
            pipeline_name: pipeline_name.into(),
            inputs,
            correlation_id,
            stream_id: Some(stream_id.into()),
            resume: None,
        }
    }

    /// Construct a resume command. The caller must already have appended
    /// `PipelineRequested`, `ResumeForked`, `WorkflowStarted`, and the
    /// frozen-step bootstrap events to `stream_id` before dispatching.
    pub fn resume(
        pipeline_name: impl Into<String>,
        inputs: HashMap<String, String>,
        correlation_id: Uuid,
        stream_id: impl Into<String>,
        resume: ResumeContext,
    ) -> Self {
        Self {
            pipeline_name: pipeline_name.into(),
            inputs,
            correlation_id,
            stream_id: Some(stream_id.into()),
            resume: Some(resume),
        }
    }
}
