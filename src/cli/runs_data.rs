//! Shared queries for pipeline runs over the event store.
//!
//! A "run" is a stream that contains at least one [`EventKind::PipelineRequested`]
//! event. The matching [`EventKind::PipelineCompleted`] (correlated by
//! `correlation_id`) gives duration and final status; if absent, the run is
//! considered still in progress.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};

use crate::events::store::EventStore;
use crate::events::EventKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Ok,
    Failed,
}

impl RunStatus {
    pub fn glyph(&self) -> &'static str {
        match self {
            RunStatus::Running => "◷",
            RunStatus::Ok => "✓",
            RunStatus::Failed => "✗",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Ok => "ok",
            RunStatus::Failed => "FAIL",
        }
    }
}

/// Pointer to the parent run that this stream was forked from (ADR-0018).
#[derive(Debug, Clone)]
pub struct ForkLineage {
    pub parent_stream_id: String,
    pub fork_at_step: String,
}

#[derive(Debug, Clone)]
pub struct RunSummary {
    pub stream_id: String,
    pub pipeline: String,
    pub started_at: DateTime<Utc>,
    pub duration: Option<Duration>,
    pub status: RunStatus,
    pub prompt_preview: String,
    pub error: Option<String>,
    /// Set when the stream contains a `ResumeForked` event — i.e. it is a
    /// fork-resume of an earlier run rather than a fresh execution.
    pub fork: Option<ForkLineage>,
}

/// Scan the store for all pipeline runs, newest first.
pub async fn list_runs(
    store: Arc<dyn EventStore>,
    pipeline_filter: Option<&str>,
) -> Result<Vec<RunSummary>, String> {
    let streams = store
        .list_streams()
        .await
        .map_err(|e| format!("failed to list streams: {e}"))?;

    let mut runs = Vec::new();

    for (stream_id, _count) in streams {
        let events = store
            .read_stream(&stream_id, 1)
            .await
            .map_err(|e| format!("failed to read stream {stream_id}: {e}"))?;

        let req = events.iter().find_map(|e| match &e.kind {
            EventKind::PipelineRequested { pipeline, inputs } => {
                Some((e, pipeline.clone(), inputs.clone()))
            }
            _ => None,
        });

        let Some((req_event, pipeline, inputs)) = req else {
            continue;
        };

        if let Some(filter) = pipeline_filter {
            if pipeline != filter {
                continue;
            }
        }

        let completed = events.iter().find_map(|e| match &e.kind {
            EventKind::PipelineCompleted {
                pipeline: p,
                success,
                error,
                ..
            } if *p == pipeline => Some((e, *success, error.clone())),
            _ => None,
        });

        let (status, duration, error) = match completed {
            Some((ev, success, err)) => (
                if success { RunStatus::Ok } else { RunStatus::Failed },
                Some(ev.timestamp - req_event.timestamp),
                err,
            ),
            None => (RunStatus::Running, None, None),
        };

        let fork = events.iter().find_map(|e| match &e.kind {
            EventKind::ResumeForked {
                parent_stream_id,
                fork_at_step,
                ..
            } => Some(ForkLineage {
                parent_stream_id: parent_stream_id.clone(),
                fork_at_step: fork_at_step.clone(),
            }),
            _ => None,
        });

        runs.push(RunSummary {
            stream_id,
            pipeline,
            started_at: req_event.timestamp,
            duration,
            status,
            prompt_preview: render_inputs(&inputs),
            error,
            fork,
        });
    }

    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    Ok(runs)
}

fn render_inputs(inputs: &HashMap<String, String>) -> String {
    let mut parts: Vec<String> = inputs
        .iter()
        .map(|(k, v)| {
            let short = if v.chars().count() > 40 {
                let end = v.floor_char_boundary(40);
                format!("{}…", &v[..end])
            } else {
                v.clone()
            };
            format!("{k}={short}")
        })
        .collect();
    parts.sort();
    parts.join(" ")
}

/// Format a duration as `H:MM:SS` or `M:SS` or `Ss`.
pub fn format_duration(d: Duration) -> String {
    let total_secs = d.num_seconds().max(0);
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else if m > 0 {
        format!("{m}:{s:02}")
    } else {
        let ms = d.num_milliseconds() % 1000;
        format!("{s}.{ms:03}s")
    }
}
