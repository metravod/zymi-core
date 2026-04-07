//! Event → command router (slice 3 of the runtime unification, ADR-0013).
//!
//! ADR-0013 v1.1 draws an explicit `BUS → CMD` edge: events on the in-process
//! bus may carry commands, not just notifications. Today the only such event
//! is [`EventKind::PipelineRequested`], which `zymi serve` translates into a
//! [`RunPipeline`] command and dispatches at
//! [`crate::handlers::run_pipeline::handle`].
//!
//! Slices 1 and 2 left that translation inlined inside `cli/serve.rs` because
//! a single mapping is not yet a router — it is a match arm. Slice 3 promotes
//! the mapping into a standalone [`EventCommandRouter`] anyway, even though
//! `PipelineRequested → RunPipeline` is still the only route. The reason is
//! recorded in ADR-0013 ("Slice 3 — what landed"): the extraction is small,
//! it removes ~120 lines of orchestration from `cli/serve.rs`, and it gives
//! the second async command type — when it appears — an obvious extension
//! point (`dispatch` match + a sibling `handle_*` method) instead of forcing
//! a future contributor to reverse-engineer the inline version.
//!
//! What the router owns:
//! - the bus subscription loop,
//! - filtering events for the routes it knows,
//! - translating each accepted event into a [`crate::commands`] command,
//! - spawning the handler task per event so concurrent requests don't block
//!   the dispatch loop,
//! - publishing the completion event back onto the bus on the originating
//!   correlation/stream.
//!
//! What it does **not** own: building the runtime, spawning the
//! [`crate::events::store::StoreTailWatcher`], or shutdown signalling. Those
//! stay in the adapter (`cli/serve.rs` today, future schedulers tomorrow) so
//! the router can be reused unchanged.

use std::sync::Arc;

use uuid::Uuid;

use crate::commands::RunPipeline;
use crate::events::bus::EventBus;
use crate::events::{Event, EventKind};
use crate::handlers::run_pipeline;

use super::Runtime;

/// Translate cross-process command events on the bus into command dispatches
/// on a [`Runtime`].
///
/// Construct via [`EventCommandRouter::new`], optionally narrow the scope
/// with [`Self::with_pipeline_filter`], then call [`Self::run`] from a tokio
/// task. The router owns its own subscription, so callers do not need to
/// touch [`EventBus::subscribe`] directly.
#[derive(Clone)]
pub struct EventCommandRouter {
    runtime: Arc<Runtime>,
    /// When set, only [`EventKind::PipelineRequested`] events whose
    /// `pipeline` matches are translated. Mirrors the behaviour of
    /// `zymi serve <pipeline>`: a serve process is bound to one pipeline so
    /// it does not race with sibling serves on unrelated requests.
    pipeline_filter: Option<String>,
}

impl EventCommandRouter {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self {
            runtime,
            pipeline_filter: None,
        }
    }

    /// Restrict [`EventKind::PipelineRequested`] handling to a specific
    /// pipeline name. Without a filter the router accepts every requested
    /// pipeline known to the runtime's workspace.
    pub fn with_pipeline_filter(mut self, pipeline: impl Into<String>) -> Self {
        self.pipeline_filter = Some(pipeline.into());
        self
    }

    /// Subscribe to the runtime's bus and dispatch matching events until the
    /// subscription closes (which happens when the bus is dropped). Each
    /// accepted event is handled in a spawned task so the dispatch loop never
    /// blocks on pipeline execution.
    pub async fn run(self) {
        let mut rx = self.runtime.bus().subscribe().await;
        while let Some(event) = rx.recv().await {
            self.dispatch(event).await;
        }
    }

    /// Dispatch a single event. Public for tests; the production loop in
    /// [`Self::run`] is the only intended caller.
    pub async fn dispatch(&self, event: Arc<Event>) {
        match &event.kind {
            EventKind::PipelineRequested { pipeline, inputs }
                if self.pipeline_matches(pipeline) =>
            {
                let pipeline = pipeline.clone();
                let inputs = inputs.clone();
                let correlation_id = event.correlation_id.unwrap_or_else(Uuid::new_v4);
                let stream_id = event.stream_id.clone();
                self.spawn_pipeline_request(pipeline, inputs, correlation_id, stream_id);
            }
            _ => {}
        }
    }

    fn pipeline_matches(&self, pipeline: &str) -> bool {
        match &self.pipeline_filter {
            Some(target) => target == pipeline,
            None => true,
        }
    }

    fn spawn_pipeline_request(
        &self,
        pipeline: String,
        inputs: std::collections::HashMap<String, String>,
        correlation_id: Uuid,
        stream_id: String,
    ) {
        let runtime = Arc::clone(&self.runtime);

        log::info!(
            "EventCommandRouter: PipelineRequested for '{pipeline}' (corr={correlation_id}, stream={stream_id})"
        );
        // Mirrored to stdout so `zymi serve` operators see the live request
        // log without enabling RUST_LOG=info. Cheap and only fires per
        // request, not per event.
        println!(
            "  -> received PipelineRequested for '{pipeline}' (corr={correlation_id}, stream={stream_id})"
        );

        tokio::spawn(async move {
            // Workspace check is repeated here (the handler also checks)
            // because we want a clean PipelineCompleted with an explicit
            // "pipeline not found" error on the same correlation_id, instead
            // of the handler's plain Err string with no completion event.
            if !runtime.workspace().pipelines.contains_key(&pipeline) {
                publish_completion(
                    runtime.bus(),
                    &stream_id,
                    correlation_id,
                    &pipeline,
                    false,
                    None,
                    Some(format!("pipeline '{pipeline}' not found")),
                )
                .await;
                return;
            }

            let cmd = RunPipeline::from_request(
                pipeline.clone(),
                inputs,
                correlation_id,
                stream_id.clone(),
            );

            match run_pipeline::handle(&runtime, cmd).await {
                Ok(pr) => {
                    publish_completion(
                        runtime.bus(),
                        &stream_id,
                        correlation_id,
                        &pipeline,
                        pr.success,
                        pr.final_output,
                        None,
                    )
                    .await;
                }
                Err(err) => {
                    publish_completion(
                        runtime.bus(),
                        &stream_id,
                        correlation_id,
                        &pipeline,
                        false,
                        None,
                        Some(err),
                    )
                    .await;
                }
            }
        });
    }
}

async fn publish_completion(
    bus: &EventBus,
    stream_id: &str,
    correlation_id: Uuid,
    pipeline: &str,
    success: bool,
    final_output: Option<String>,
    error: Option<String>,
) {
    let event = Event::new(
        stream_id.to_string(),
        EventKind::PipelineCompleted {
            pipeline: pipeline.to_string(),
            success,
            final_output,
            error,
        },
        "zymi-serve".into(),
    )
    .with_correlation(correlation_id);
    if let Err(e) = bus.publish(event).await {
        log::error!("failed to publish PipelineCompleted: {e}");
    }
}
