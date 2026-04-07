//! Command types — slice 1 of the runtime unification (ADR-0013).
//!
//! Commands are the inputs to [`crate::handlers`]. They carry the user
//! intent, never the wiring: handlers always pull store/bus/providers
//! from a [`crate::runtime::Runtime`].
//!
//! ADR-0013 v1.1 lists four target commands: `RunPipeline`,
//! `ProcessConversation`, `DecideApproval`, `ReplayStream`. Slice 1 only
//! ships [`RunPipeline`]; the others are added when a real caller appears.

use std::collections::HashMap;

use uuid::Uuid;

/// Execute a pipeline by name with the given inputs.
///
/// `correlation_id` and `stream_id` exist so that commands originating from
/// a cross-process [`crate::events::EventKind::PipelineRequested`] event can
/// be reattached to the same correlation/stream as their request, instead of
/// minting fresh ones. The synchronous `zymi run` path generates them
/// locally via [`RunPipeline::new`].
#[derive(Debug, Clone)]
pub struct RunPipeline {
    pub pipeline_name: String,
    pub inputs: HashMap<String, String>,
    pub correlation_id: Uuid,
    pub stream_id: Option<String>,
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
        }
    }
}
