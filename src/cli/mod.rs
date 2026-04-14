mod events;
mod init;
mod run;
mod schema;
mod serve;
mod verify;

use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;

use clap::{Parser, Subcommand};

/// zymi — event-sourced agent engine CLI
#[derive(Parser)]
#[command(name = "zymi", version, about)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a new zymi project in the current directory
    Init {
        /// Project name (defaults to directory name)
        #[arg(short, long)]
        name: Option<String>,

        /// Scaffold from a built-in example (e.g. "research")
        #[arg(long)]
        example: Option<String>,
    },

    /// Run a pipeline
    Run {
        /// Pipeline name to execute
        pipeline: String,

        /// Pipeline input in KEY=VALUE format (repeatable)
        #[arg(short = 'i', long = "input", value_name = "KEY=VALUE")]
        inputs: Vec<String>,

        /// Approval mode: "terminal" (interactive, default) or "webhook"
        #[arg(long, default_value = "terminal")]
        approval: String,

        /// Webhook callback URL for approval notifications (requires --approval=webhook)
        #[arg(long)]
        callback_url: Option<String>,

        /// Project root directory (defaults to cwd)
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
    },

    /// List and inspect events from the store
    Events {
        /// Filter by stream ID (replaces the old `replay` command)
        #[arg(short, long)]
        stream: Option<String>,

        /// Filter by event kind tag
        #[arg(short, long)]
        kind: Option<String>,

        /// Maximum number of events to show
        #[arg(short, long, default_value = "50")]
        limit: usize,

        /// Output as JSON (one event per line)
        #[arg(long)]
        json: bool,

        /// Show extended detail for each event
        #[arg(short, long)]
        verbose: bool,

        /// Project root directory
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
    },

    /// Verify hash chain integrity of the event store
    Verify {
        /// Verify a specific stream (otherwise all streams)
        #[arg(short, long)]
        stream: Option<String>,

        /// Project root directory
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
    },

    /// Run a pipeline as a long-lived service that reacts to PipelineRequested events
    Serve {
        /// Pipeline name to serve
        pipeline: String,

        /// Polling interval for the cross-process store watcher (ms)
        #[arg(long, default_value = "100")]
        poll_interval_ms: u64,

        /// Approval mode: "terminal" (interactive, default) or "webhook"
        #[arg(long, default_value = "terminal")]
        approval: String,

        /// Webhook callback URL for approval notifications (requires --approval=webhook)
        #[arg(long)]
        callback_url: Option<String>,

        /// Project root directory (defaults to cwd)
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
    },

    /// Print JSON Schema for a config kind (project, agent, pipeline, tool)
    Schema {
        /// Config kind: project, agent, pipeline, tool
        kind: Option<String>,

        /// Print schemas for all config kinds
        #[arg(long)]
        all: bool,
    },

}

/// Run the CLI. Called from the binary entry point.
pub fn run() {
    dispatch(Cli::parse());
}

/// Run the CLI with explicit arguments (used by the Python entry point).
pub fn run_from_args(args: impl IntoIterator<Item = String>) {
    dispatch(Cli::parse_from(args));
}

fn dispatch(cli: Cli) {
    let result = match cli.command {
        Command::Init { name, example } => init::exec(name, example.as_deref()),
        Command::Run {
            pipeline,
            inputs,
            approval,
            callback_url,
            dir,
        } => run::exec(&pipeline, &inputs, &approval, callback_url.as_deref(), resolve_root(dir.as_deref())),
        Command::Events {
            stream,
            kind,
            limit,
            json,
            verbose,
            dir,
        } => events::exec(
            stream.as_deref(),
            kind.as_deref(),
            limit,
            json,
            verbose,
            resolve_root(dir.as_deref()),
        ),
        Command::Verify { stream, dir } => {
            verify::exec(stream.as_deref(), resolve_root(dir.as_deref()))
        }
        Command::Serve {
            pipeline,
            poll_interval_ms,
            approval,
            callback_url,
            dir,
        } => serve::exec(&pipeline, poll_interval_ms, &approval, callback_url.as_deref(), resolve_root(dir.as_deref())),
        Command::Schema { kind, all } => schema::exec(kind.as_deref(), all),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

/// Resolve the project root: use --dir if given, otherwise cwd.
fn resolve_root(dir: Option<&Path>) -> PathBuf {
    dir.map(|d| d.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().expect("cannot determine current directory"))
}

/// Path to the event store database within a project.
pub(crate) fn store_path(root: &Path) -> PathBuf {
    root.join(".zymi").join("events.db")
}

/// Build an approval handler from CLI flags.
///
/// `--approval=terminal` (default): interactive stdin prompt.
/// `--approval=webhook`: HTTP webhook server (requires `webhook` feature).
pub(crate) async fn build_approval_handler(
    mode: &str,
    callback_url: Option<&str>,
) -> Result<Arc<dyn crate::approval::ApprovalHandler>, String> {
    match mode {
        "terminal" => Ok(Arc::new(
            crate::approval::TerminalApprovalHandler::new(),
        )),
        #[cfg(feature = "webhook")]
        "webhook" => {
            let addr: std::net::SocketAddr = "127.0.0.1:0"
                .parse()
                .map_err(|e| format!("invalid webhook addr: {e}"))?;
            let handler = crate::webhook::WebhookApprovalHandler::start(
                addr,
                std::time::Duration::from_secs(300),
                callback_url.map(|s| s.to_string()),
            )
            .await
            .map_err(|e| format!("failed to start webhook server: {e}"))?;
            Ok(handler)
        }
        #[cfg(not(feature = "webhook"))]
        "webhook" => Err(
            "webhook approval requires the 'webhook' feature. \
             Rebuild with --features webhook"
                .into(),
        ),
        other => Err(format!(
            "unknown approval mode '{other}'. Expected 'terminal' or 'webhook'"
        )),
    }
}

/// Create a tokio runtime for async operations.
pub(crate) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
}
