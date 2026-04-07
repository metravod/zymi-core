use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::approval::{ApprovalHandler, TerminalApprovalHandler};
use crate::config::load_project_dir;
use crate::events::store::StoreTailWatcher;
use crate::runtime::{EventCommandRouter, Runtime};

use super::store_path;

/// Run a pipeline as a long-lived event-driven service.
///
/// Wires the runtime once at startup, spawns a [`StoreTailWatcher`] so the
/// in-process bus sees events appended by other processes (per ADR-0012),
/// then hands the dispatch loop to an [`EventCommandRouter`] bound to this
/// pipeline. Cross-process clients submit work by appending
/// [`crate::events::EventKind::PipelineRequested`] events to the same store
/// (via `EventDrivenConnector` or by hand) and await the matching
/// [`crate::events::EventKind::PipelineCompleted`] on their `correlation_id`.
///
/// Slice 3 of the runtime unification (ADR-0013) lifted the
/// `PipelineRequested → RunPipeline` translation out of this file into
/// [`EventCommandRouter`]; what stays here is just startup wiring,
/// shutdown signalling, and operator-facing log lines.
pub fn exec(
    pipeline_name: &str,
    poll_interval_ms: u64,
    root: impl AsRef<Path>,
) -> Result<(), String> {
    let root = root.as_ref().to_path_buf();

    if !root.join("project.yml").exists() {
        return Err(format!(
            "no project.yml found in {}. Run `zymi init` first.",
            root.display()
        ));
    }

    let workspace =
        load_project_dir(&root).map_err(|e| format!("failed to load project: {e}"))?;

    if !workspace.pipelines.contains_key(pipeline_name) {
        let available: Vec<&str> = workspace.pipelines.keys().map(|s| s.as_str()).collect();
        return Err(format!(
            "pipeline '{pipeline_name}' not found. Available: {}",
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        ));
    }

    // Multi-thread runtime: the watcher runs concurrently with pipeline tasks.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create runtime: {e}"))?;

    rt.block_on(serve_loop(
        workspace,
        pipeline_name.to_string(),
        root,
        poll_interval_ms,
    ))
}

async fn serve_loop(
    workspace: crate::config::WorkspaceConfig,
    pipeline_name: String,
    root: PathBuf,
    poll_interval_ms: u64,
) -> Result<(), String> {
    // Single shared handler so prompts from concurrent pipeline runs are
    // serialised on the operator's terminal.
    let approval_handler: Arc<dyn ApprovalHandler> = Arc::new(TerminalApprovalHandler::new());

    let runtime = Arc::new(
        Runtime::builder(workspace, root.clone())
            .with_approval_handler(Arc::clone(&approval_handler))
            .with_tail_poll_interval(Duration::from_millis(poll_interval_ms))
            .build()?,
    );

    let watcher = StoreTailWatcher::new(Arc::clone(runtime.store()), Arc::clone(runtime.bus()))
        .with_interval(runtime.tail_poll_interval())
        .spawn();

    let db_path = store_path(&root);
    println!(
        "zymi serve: listening for PipelineRequested events targeting '{pipeline_name}'\n  store: {}\n  poll:  {poll_interval_ms}ms\n  press Ctrl+C to stop",
        db_path.display()
    );

    let router = EventCommandRouter::new(Arc::clone(&runtime)).with_pipeline_filter(&pipeline_name);

    tokio::select! {
        _ = router.run() => {}
        ctrlc = tokio::signal::ctrl_c() => {
            if let Err(e) = ctrlc {
                log::warn!("ctrl_c handler error: {e}");
            }
            println!("\nshutting down…");
        }
    }

    watcher.stop().await;
    Ok(())
}
