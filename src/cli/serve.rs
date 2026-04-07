use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::approval::ApprovalHandler;
use crate::config::load_project_dir;
use crate::engine;
use crate::events::bus::EventBus;
use crate::events::store::{open_store, StoreBackend, StoreTailWatcher};
use crate::events::{Event, EventKind};

use super::approval::TerminalApprovalHandler;
use super::store_path;

/// Run a pipeline as a long-lived event-driven service.
///
/// On startup, opens the project's event store, spawns a [`StoreTailWatcher`]
/// to fan out cross-process events into the local bus, then subscribes to
/// [`EventKind::PipelineRequested`] events targeted at `pipeline_name`.
/// Each matching request spawns a pipeline run; on completion the service
/// publishes a [`EventKind::PipelineCompleted`] event with the same
/// `correlation_id`, allowing clients (Django, scripts, etc.) to await
/// the result with [`EventBus::subscribe_correlation`] or via
/// [`crate::events::EventDrivenConnector`].
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
    let store_dir = root.join(".zymi");
    std::fs::create_dir_all(&store_dir)
        .map_err(|e| format!("failed to create .zymi directory: {e}"))?;
    let db_path = store_path(&root);

    let store = open_store(StoreBackend::Sqlite { path: db_path.clone() })
        .map_err(|e| format!("failed to open event store: {e}"))?;
    let bus = Arc::new(EventBus::new(Arc::clone(&store)));

    let watcher = StoreTailWatcher::new(Arc::clone(&store), Arc::clone(&bus))
        .with_interval(Duration::from_millis(poll_interval_ms))
        .spawn();

    // Single shared handler so prompts from concurrent pipeline runs are
    // serialised on the operator's terminal.
    let approval_handler: Arc<dyn ApprovalHandler> = Arc::new(TerminalApprovalHandler::new());

    let mut requests: mpsc::Receiver<Arc<Event>> = bus.subscribe().await;

    println!(
        "zymi serve: listening for PipelineRequested events targeting '{pipeline_name}'\n  store: {}\n  poll:  {poll_interval_ms}ms\n  press Ctrl+C to stop",
        db_path.display()
    );

    let workspace = Arc::new(workspace);
    let pipeline_filter = pipeline_name.clone();

    let run_loop = async {
        while let Some(event) = requests.recv().await {
            let (pipeline, inputs) = match &event.kind {
                EventKind::PipelineRequested { pipeline, inputs }
                    if pipeline == &pipeline_filter =>
                {
                    (pipeline.clone(), inputs.clone())
                }
                _ => continue,
            };

            // Reuse the originating correlation_id so the response can be
            // matched by clients. Fall back to a fresh one if missing.
            let correlation_id = event.correlation_id.unwrap_or_else(Uuid::new_v4);
            let stream_id = event.stream_id.clone();

            let workspace = Arc::clone(&workspace);
            let bus = Arc::clone(&bus);
            let store = Arc::clone(&store);
            let root = root.clone();
            let pipeline_name = pipeline.clone();
            let approval_handler = Arc::clone(&approval_handler);

            println!(
                "  -> received PipelineRequested for '{pipeline}' (corr={correlation_id}, stream={stream_id})"
            );

            tokio::spawn(async move {
                let pipeline_config = match workspace.pipelines.get(&pipeline_name) {
                    Some(p) => p.clone(),
                    None => {
                        publish_completion(
                            &bus,
                            &stream_id,
                            correlation_id,
                            &pipeline_name,
                            false,
                            None,
                            Some(format!("pipeline '{pipeline_name}' not found")),
                        )
                        .await;
                        return;
                    }
                };

                let result = engine::run_pipeline_for_request(
                    &workspace,
                    &pipeline_config,
                    &root,
                    &inputs,
                    Arc::clone(&bus),
                    Arc::clone(&store),
                    correlation_id,
                    Some(approval_handler),
                )
                .await;

                match result {
                    Ok(pr) => {
                        publish_completion(
                            &bus,
                            &stream_id,
                            correlation_id,
                            &pipeline_name,
                            pr.success,
                            pr.final_output,
                            None,
                        )
                        .await;
                    }
                    Err(err) => {
                        publish_completion(
                            &bus,
                            &stream_id,
                            correlation_id,
                            &pipeline_name,
                            false,
                            None,
                            Some(err),
                        )
                        .await;
                    }
                }
            });
        }
    };

    tokio::select! {
        _ = run_loop => {}
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

#[allow(clippy::too_many_arguments)]
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

