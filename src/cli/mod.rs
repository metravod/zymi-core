mod events;
mod init;
mod replay;
mod run;
mod verify;

use std::path::{Path, PathBuf};
use std::process;

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

        /// Project root directory (defaults to cwd)
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
    },

    /// List and inspect events from the store
    Events {
        /// Filter by stream ID
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

    /// Replay events from a stream
    Replay {
        /// Stream ID to replay
        stream_id: String,

        /// Start from this sequence number
        #[arg(short, long, default_value = "1")]
        from: u64,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Project root directory
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
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
            dir,
        } => run::exec(&pipeline, &inputs, resolve_root(dir.as_deref())),
        Command::Events {
            stream,
            kind,
            limit,
            json,
            dir,
        } => events::exec(
            stream.as_deref(),
            kind.as_deref(),
            limit,
            json,
            resolve_root(dir.as_deref()),
        ),
        Command::Verify { stream, dir } => {
            verify::exec(stream.as_deref(), resolve_root(dir.as_deref()))
        }
        Command::Replay {
            stream_id,
            from,
            json,
            dir,
        } => replay::exec(&stream_id, from, json, resolve_root(dir.as_deref())),
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

/// Create a tokio runtime for async operations.
pub(crate) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
}
