mod event_fmt;
mod events;
mod init;
mod mcp;
mod observe;
mod pipelines;
mod resume;
mod run;
mod runs;
mod runs_data;
mod schema;
mod serve;
mod verify;

use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;

use clap::{Parser, Subcommand};

#[derive(Subcommand)]
enum McpCommand {
    /// Spawn an MCP server, handshake, list its tools, and shut down.
    ///
    /// Does not require a project directory or an LLM — use it to discover
    /// what a server advertises before writing an `allow:` whitelist.
    ///
    /// Example:
    ///   zymi mcp probe fs -- npx -y @modelcontextprotocol/server-filesystem /tmp
    ///   zymi mcp probe fetch -- uvx mcp-server-fetch
    Probe {
        /// Name used in the handshake (cosmetic — doesn't have to match project.yml)
        name: String,

        /// Extra env var for the child process, repeatable. Only PATH is
        /// auto-forwarded from the parent; everything else stays out of the
        /// child unless named here.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,

        /// Handshake deadline in seconds
        #[arg(long, default_value = "15")]
        init_timeout_secs: u64,

        /// Per-call deadline in seconds (affects tools/list here)
        #[arg(long, default_value = "30")]
        call_timeout_secs: u64,

        /// Command and args for the MCP server (prefix with `--`)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

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

        /// Emit raw output (one JSON event per line, pipe-friendly)
        #[arg(long)]
        raw: bool,

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

    /// List pipelines defined in the project
    Pipelines {
        /// Project root directory
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
    },

    /// List pipeline runs recorded in the event store
    Runs {
        /// Filter by pipeline name
        #[arg(short, long)]
        pipeline: Option<String>,

        /// Maximum number of runs to show
        #[arg(short, long, default_value = "50")]
        limit: usize,

        /// Emit raw JSON output (one run per line)
        #[arg(long)]
        raw: bool,

        /// Project root directory
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
    },

    /// Interactive TUI for browsing runs, pipeline graph, and event timeline
    Observe {
        /// Pre-select a run by stream id
        #[arg(short, long)]
        run: Option<String>,

        /// Project root directory
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
    },

    /// Fork-resume a previous pipeline run from a chosen step (ADR-0018).
    ///
    /// Steps upstream of `--from-step` are frozen — their events from the
    /// parent stream are copied to a new stream and they are not re-executed.
    /// The fork step and its DAG-descendants run from scratch using the
    /// current pipelines/agents config from disk.
    Resume {
        /// Parent stream id (see `zymi runs` / `zymi observe`)
        stream: String,

        /// Step id at which to fork — this step and everything downstream re-run
        #[arg(long = "from-step")]
        from_step: String,

        /// Print the resume plan (frozen vs re-executed steps) and exit
        /// without writing any new events.
        #[arg(long)]
        dry_run: bool,

        /// Approval mode: "terminal" (interactive, default) or "webhook"
        #[arg(long, default_value = "terminal")]
        approval: String,

        /// Webhook callback URL for approval notifications
        #[arg(long)]
        callback_url: Option<String>,

        /// Project root directory (defaults to cwd)
        #[arg(short = 'd', long)]
        dir: Option<PathBuf>,
    },

    /// MCP server utilities (probe a server, inspect tools)
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
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
            raw,
            verbose,
            dir,
        } => events::exec(
            stream.as_deref(),
            kind.as_deref(),
            limit,
            raw,
            verbose,
            resolve_root(dir.as_deref()),
        ),
        Command::Pipelines { dir } => pipelines::exec(resolve_root(dir.as_deref())),
        Command::Runs {
            pipeline,
            limit,
            raw,
            dir,
        } => runs::exec(
            pipeline.as_deref(),
            limit,
            raw,
            resolve_root(dir.as_deref()),
        ),
        Command::Observe { run, dir } => {
            observe::exec(run.as_deref(), resolve_root(dir.as_deref()))
        }
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
        Command::Resume {
            stream,
            from_step,
            dry_run,
            approval,
            callback_url,
            dir,
        } => resume::exec(
            &stream,
            &from_step,
            dry_run,
            &approval,
            callback_url.as_deref(),
            resolve_root(dir.as_deref()),
        ),
        Command::Mcp { command } => match command {
            McpCommand::Probe {
                name,
                env,
                init_timeout_secs,
                call_timeout_secs,
                command,
            } => mcp::exec_probe(&name, &command, &env, init_timeout_secs, call_timeout_secs),
        },
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
    #[cfg_attr(not(feature = "webhook"), allow(unused_variables))] callback_url: Option<&str>,
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
