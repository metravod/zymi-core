# CLI Architecture: Rust Binary Behind Feature Flag

Date: 2026-04-05

## Context

zymi needs a CLI for project scaffolding, pipeline execution, event inspection, hash chain verification, and event replay. The question is where the CLI lives and how it relates to the library crate.

## Decision

The CLI is implemented as a Rust binary using `clap` (derive API), gated behind an optional `cli` Cargo feature. Library consumers don't pull in clap unless they opt in.

### Structure

- `src/cli/mod.rs` — clap definitions, dispatch, shared helpers
- `src/cli/{init,run,events,verify,replay}.rs` — one file per subcommand
- `src/bin/zymi.rs` — 3-line entry point calling `zymi_core::cli::run()`

### Subcommands

| Command | Purpose |
|---------|---------|
| `zymi init [--name]` | Scaffold project.yml, agents/, pipelines/, .zymi/ |
| `zymi run <pipeline> [-d dir]` | Load workspace, build execution plan, dry-run (execution TBD) |
| `zymi events [--stream] [--kind] [--limit] [--json] [-d dir]` | List/filter events from SQLite store |
| `zymi verify [--stream] [-d dir]` | Verify hash chain integrity (per-stream or all) |
| `zymi replay <stream> [--from] [--json] [-d dir]` | Replay events with human-readable detail output |

### Conventions

- Event store at `.zymi/events.db` in project root
- All commands accept `--dir` to override project root (defaults to cwd)
- `events` and `replay` support `--json` for machine-readable output
- Async operations (event store queries) use a per-command `current_thread` tokio runtime

### Feature Flag

```toml
[features]
cli = ["clap"]

[[bin]]
name = "zymi"
required-features = ["cli"]
```

## Alternatives Considered

- **Python CLI wrapping Rust**: Would allow Python tool loading in `run`, but adds a Python dependency for basic operations (init, events, verify). Rust binary covers the core commands; Python integration can be layered on later via maturin.
- **Separate binary crate in a workspace**: Cleaner dependency isolation, but premature for a single-binary project. Can migrate later if needed.
- **No feature flag (always include clap)**: Wastes ~1MB+ compile time and binary size for library-only consumers.

## Consequences

- `cargo install zymi-core --features cli` provides the `zymi` binary.
- Library consumers (`cargo add zymi-core`) get zero CLI overhead.
- `zymi run` is currently dry-run only — prints the execution plan but doesn't execute. Pipeline execution will be added when the runtime loop is built.
- `list_streams()` was added to the `EventStore` trait to support `verify --all` and `events` stream listing.
