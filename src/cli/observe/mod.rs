//! Interactive TUI: runs list + pipeline graph + event timeline.
//!
//! Three vertical panels over the event store:
//! - **Runs**: all pipeline runs, newest first.
//! - **Pipeline graph**: the DAG of the selected run, with per-node status.
//! - **Events**: chronological events of the selected run, with per-kind
//!   summary and expand-to-full-detail.
//!
//! The formatter ([`super::event_fmt`]) is shared with `zymi events` so both
//! views describe the same event kind identically.

mod app;
mod data;
mod graph;
mod ui;

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::config::load_project_dir;
use crate::events::store::{open_store_async, StoreBackend};
use crate::handlers::resume_pipeline::{self, ResumePipeline};
use crate::runtime::Runtime;

use super::{resolve_store_backend_for_cli, runtime};

use self::app::{App, ForkPrompt, ForkState, Focus};

/// Result delivered back to the TUI loop after a fork-resume task finishes.
struct ForkResult {
    parent_stream_id: String,
    fork_at_step: String,
    outcome: Result<String, String>, // Ok(new_stream_id) | Err(message)
}

pub fn exec(initial_run: Option<&str>, root: impl AsRef<Path>) -> Result<(), String> {
    let root = root.as_ref().to_path_buf();
    let backend = resolve_store_backend_for_cli(&root)?;
    if let StoreBackend::Sqlite { path } = &backend {
        if !path.exists() {
            return Err(format!(
                "no event store found at {}. Run a pipeline first or check --dir.",
                path.display()
            ));
        }
    }

    let rt = runtime();
    rt.block_on(run_loop(root, backend, initial_run.map(|s| s.to_string())))
}

async fn run_loop(
    root: PathBuf,
    backend: StoreBackend,
    initial_run: Option<String>,
) -> Result<(), String> {
    let store = open_store_async(backend)
        .await
        .map_err(|e| format!("failed to open event store: {e}"))?;

    let mut app = App::new(root, store.clone());
    app.reload_runs().await?;
    if let Some(stream) = initial_run {
        app.select_run_by_stream(&stream);
    }
    app.load_selected_run().await?;

    enable_raw_mode().map_err(|e| format!("failed to enable raw mode: {e}"))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)
        .map_err(|e| format!("failed to enter alternate screen: {e}"))?;

    // Restore terminal on panic.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)
        .map_err(|e| format!("failed to create terminal: {e}"))?;

    let mut reader = EventStream::new();
    let mut refresh_tick = tokio::time::interval(Duration::from_secs(2));
    let mut fork_tail_tick = tokio::time::interval(Duration::from_millis(500));
    let (fork_tx, mut fork_rx) = mpsc::channel::<ForkResult>(4);

    let result = loop {
        if let Err(e) = terminal.draw(|f| ui::draw(f, &mut app)) {
            break Err(format!("draw error: {e}"));
        }

        if app.should_quit {
            break Ok(());
        }

        tokio::select! {
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(CtEvent::Key(key))) => {
                        if let Err(e) = handle_key(&mut app, key.code, key.modifiers, &fork_tx).await {
                            break Err(e);
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(format!("input error: {e}")),
                    None => break Ok(()),
                }
            }
            _ = refresh_tick.tick() => {
                if app.follow_tail {
                    if let Err(e) = app.reload_runs().await {
                        break Err(e);
                    }
                    if let Err(e) = app.load_selected_run().await {
                        break Err(e);
                    }
                }
            }
            Some(fork_result) = fork_rx.recv() => {
                if let Err(e) = apply_fork_result(&mut app, fork_result).await {
                    break Err(e);
                }
            }
            _ = fork_tail_tick.tick() => {
                if matches!(
                    app.fork_prompt.as_ref().map(|p| &p.state),
                    Some(ForkState::Running)
                ) {
                    if let Err(e) = app.poll_fork_tail().await {
                        break Err(e);
                    }
                }
            }
        }
    };

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

async fn handle_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    fork_tx: &mpsc::Sender<ForkResult>,
) -> Result<(), String> {
    // Modal popup grabs the keys while it's open. Esc/Enter close or advance
    // it depending on state; everything else passes through (but we still
    // suppress quit-on-q to avoid surprise exit while the popup is busy).
    if let Some(prompt) = app.fork_prompt.clone() {
        match (&prompt.state, code, mods) {
            (ForkState::Confirm, KeyCode::Enter, _) => {
                let parent = prompt.parent_stream_id.clone();
                let step = prompt.fork_at_step.clone();
                if let Some(p) = app.fork_prompt.as_mut() {
                    p.state = ForkState::Running;
                }
                spawn_fork(app.root.clone(), parent, step, fork_tx.clone());
            }
            (ForkState::Confirm, KeyCode::Esc, _)
            | (ForkState::Confirm, KeyCode::Char('q'), _) => {
                app.clear_fork_prompt();
            }
            (ForkState::Done { new_stream_id, .. }, KeyCode::Enter, _)
            | (ForkState::Done { new_stream_id, .. }, KeyCode::Esc, _) => {
                let stream = new_stream_id.clone();
                app.clear_fork_prompt();
                app.select_run_by_stream(&stream);
                app.load_selected_run().await?;
            }
            (ForkState::Failed(_), KeyCode::Enter, _)
            | (ForkState::Failed(_), KeyCode::Esc, _) => {
                app.clear_fork_prompt();
            }
            _ => {}
        }
        return Ok(());
    }

    match (code, mods) {
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => app.should_quit = true,
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => app.should_quit = true,

        (KeyCode::Tab, _) => app.focus = app.focus.next(),
        (KeyCode::BackTab, _) => app.focus = app.focus.prev(),

        (KeyCode::Char('f'), _) => app.follow_tail = !app.follow_tail,
        (KeyCode::Char('r'), _) => {
            app.reload_runs().await?;
            app.load_selected_run().await?;
        }

        // Shift+R on the graph node: open fork-resume confirm popup.
        (KeyCode::Char('R'), _) => {
            if matches!(app.focus, Focus::Graph) {
                app.start_fork_prompt();
            }
        }

        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
            match app.focus {
                Focus::Runs => {
                    app.move_run_cursor(-1);
                    app.load_selected_run().await?;
                }
                Focus::Graph => app.move_graph_cursor(-1),
                Focus::Events => app.move_event_cursor(-1),
            }
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
            match app.focus {
                Focus::Runs => {
                    app.move_run_cursor(1);
                    app.load_selected_run().await?;
                }
                Focus::Graph => app.move_graph_cursor(1),
                Focus::Events => app.move_event_cursor(1),
            }
        }

        (KeyCode::Enter, _) => match app.focus {
            Focus::Runs => app.load_selected_run().await?,
            Focus::Graph => app.focus_event_on_selected_node(),
            Focus::Events => app.toggle_event_expanded(),
        },

        _ => {}
    }
    Ok(())
}

/// Spawn the fork-resume task off the UI thread. Sends the result back
/// through `tx`; the main loop picks it up via `apply_fork_result`.
///
/// No approval handler is attached — fork-resume from the TUI is fail-closed
/// for any step tagged `RequiresHumanApproval`. Use `zymi resume` from a
/// regular shell if the fork point requires interactive approval.
fn spawn_fork(
    root: PathBuf,
    parent_stream_id: String,
    fork_at_step: String,
    tx: mpsc::Sender<ForkResult>,
) {
    let parent_for_msg = parent_stream_id.clone();
    let step_for_msg = fork_at_step.clone();
    tokio::spawn(async move {
        let outcome = run_fork(root, parent_stream_id.clone(), fork_at_step.clone()).await;
        let _ = tx
            .send(ForkResult {
                parent_stream_id: parent_for_msg,
                fork_at_step: step_for_msg,
                outcome,
            })
            .await;
    });
}

async fn run_fork(
    root: PathBuf,
    parent_stream_id: String,
    fork_at_step: String,
) -> Result<String, String> {
    let workspace = load_project_dir(&root)
        .map_err(|e| format!("failed to load project: {e}"))?;
    let runtime_for_resume = Runtime::builder(workspace, root).build()?;
    let cmd = ResumePipeline {
        parent_stream_id,
        fork_at_step,
    };
    let outcome = resume_pipeline::handle(&runtime_for_resume, cmd).await?;
    if !outcome.result.success {
        return Err(format!(
            "pipeline completed with errors on stream {}",
            outcome.new_stream_id
        ));
    }
    Ok(outcome.new_stream_id)
}

async fn apply_fork_result(app: &mut App, result: ForkResult) -> Result<(), String> {
    // The popup may already have been dismissed by the user; only update it
    // if it's still the same fork we kicked off.
    let still_relevant = matches!(
        &app.fork_prompt,
        Some(ForkPrompt {
            parent_stream_id,
            fork_at_step,
            state: ForkState::Running,
            ..
        }) if *parent_stream_id == result.parent_stream_id
            && *fork_at_step == result.fork_at_step
    );

    // Refresh the runs list either way so the new fork shows up.
    app.reload_runs().await?;

    if !still_relevant {
        return Ok(());
    }

    match result.outcome {
        Ok(new_stream_id) => {
            if let Some(p) = app.fork_prompt.as_mut() {
                p.state = ForkState::Done {
                    new_stream_id: new_stream_id.clone(),
                    message: "Forked to new stream.".to_string(),
                };
            }
        }
        Err(msg) => {
            if let Some(p) = app.fork_prompt.as_mut() {
                p.state = ForkState::Failed(msg);
            }
        }
    }
    Ok(())
}
