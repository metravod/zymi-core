use std::path::Path;

use chrono::Local;

use crate::events::store::{open_store, StoreBackend};

use super::event_fmt::{BOLD, DIM, GREEN, RED, RESET, YELLOW};
use super::runs_data::{format_duration, list_runs, RunStatus, RunSummary};
use super::{runtime, store_path};

pub fn exec(
    pipeline_filter: Option<&str>,
    limit: usize,
    raw: bool,
    root: impl AsRef<Path>,
) -> Result<(), String> {
    let db_path = store_path(root.as_ref());
    if !db_path.exists() {
        return Err(format!(
            "no event store found at {}. Run a pipeline first or check --dir.",
            db_path.display()
        ));
    }

    let store = open_store(StoreBackend::Sqlite { path: db_path })
        .map_err(|e| format!("failed to open event store: {e}"))?;

    let rt = runtime();
    let runs = rt.block_on(list_runs(store, pipeline_filter))?;
    let shown: Vec<_> = runs.iter().take(limit).collect();

    if shown.is_empty() {
        if !raw {
            println!("No runs recorded yet.");
        }
        return Ok(());
    }

    if raw {
        for run in &shown {
            print_raw(run)?;
        }
        return Ok(());
    }

    println!(
        "{BOLD}Runs{RESET}: {} total, showing {}{}",
        runs.len(),
        shown.len(),
        if let Some(p) = pipeline_filter {
            format!(" {DIM}(pipeline={p}){RESET}")
        } else {
            String::new()
        }
    );
    println!();

    let max_pipeline = shown
        .iter()
        .map(|r| r.pipeline.len())
        .max()
        .unwrap_or(0)
        .max(8);
    let max_stream = shown
        .iter()
        .map(|r| r.stream_id.len())
        .max()
        .unwrap_or(0)
        .clamp(6, 40);

    println!(
        "  {DIM}{:<max_stream$}  {:<max_pipeline$}  {:<19}  {:<8}  {:<7}  prompt{RESET}",
        "stream", "pipeline", "started", "duration", "status",
    );

    for run in shown {
        print_row(run, max_stream, max_pipeline);
    }

    Ok(())
}

fn print_row(run: &RunSummary, max_stream: usize, max_pipeline: usize) {
    let stream = truncate_mid(&run.stream_id, max_stream);
    let started = run.started_at.with_timezone(&Local).format("%Y-%m-%d %H:%M");
    let duration = run
        .duration
        .map(format_duration)
        .unwrap_or_else(|| "—".into());

    let status_color = match run.status {
        RunStatus::Running => YELLOW,
        RunStatus::Ok => GREEN,
        RunStatus::Failed => RED,
    };

    let suffix = match &run.fork {
        Some(f) => format!(
            "  {DIM}↩ from {}:{}{RESET}",
            truncate_mid(&f.parent_stream_id, 24),
            f.fork_at_step
        ),
        None => String::new(),
    };

    println!(
        "  {stream:<max_stream$}  {:<max_pipeline$}  {started}  {:<8}  {status_color}{} {}{RESET}  {DIM}{}{RESET}{suffix}",
        run.pipeline,
        duration,
        run.status.glyph(),
        run.status.label(),
        truncate(&run.prompt_preview, 60),
    );
}

fn print_raw(run: &RunSummary) -> Result<(), String> {
    let value = serde_json::json!({
        "stream_id": run.stream_id,
        "pipeline": run.pipeline,
        "started_at": run.started_at.to_rfc3339(),
        "duration_ms": run.duration.map(|d| d.num_milliseconds()),
        "status": run.status.label(),
        "prompt": run.prompt_preview,
        "error": run.error,
        "fork": run.fork.as_ref().map(|f| serde_json::json!({
            "parent_stream_id": f.parent_stream_id,
            "fork_at_step": f.fork_at_step,
        })),
    });
    println!(
        "{}",
        serde_json::to_string(&value).map_err(|e| format!("serialization error: {e}"))?
    );
    Ok(())
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let end = s.floor_char_boundary(max);
        &s[..end]
    }
}

/// Shorten middle of long IDs: `pipeline-research-abcd…ef01`.
fn truncate_mid(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head = max.saturating_sub(1) / 2;
    let tail = max.saturating_sub(1) - head;
    let head_end = s.floor_char_boundary(head);
    let tail_start = s.len().saturating_sub(tail);
    let tail_start = s.ceil_char_boundary(tail_start);
    format!("{}…{}", &s[..head_end], &s[tail_start..])
}
