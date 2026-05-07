# Contributing to zymi-core

Thanks for taking a look. This guide covers how to set up a dev environment, run the test matrix, and land a change.

## Prerequisites

- **Rust** 1.75+ (`rustup update stable`)
- **Python** 3.9+ (only for the PyO3 bridge)
- **maturin** (`pip install maturin`) — only for building the Python wheel

## Dev loop

```bash
git clone https://github.com/metravod/zymi-core.git
cd zymi-core

# Default build — core library, no optional features
cargo build
cargo test

# With the CLI + connectors (what `pip install zymi-core` ships)
cargo build --features cli
cargo test  --features cli

# Run the CLI directly from source
cargo run --features cli -- init --example telegram
cargo run --features cli -- run main
```

The `cli` feature pulls in `runtime`, `connectors`, and the TUI deps. For isolated work on the event store or ESAA you can stay on the default feature set.

### Python bridge

```bash
maturin develop --features python,cli
python -c "from zymi import Runtime; print(Runtime)"
```

`maturin develop` builds the extension into your current virtualenv. For release wheels (not usually needed locally) use `maturin build --release --features python,cli`.

## Test matrix

CI runs each of these; reproduce them locally before opening a PR:

```bash
cargo test                                  # default features
cargo test --features cli                   # CLI + runtime + connectors
cargo test --features webhook               # HTTP approval handler
cargo test --features services,webhook      # LangFuse + webhook
cargo test --features python                # Python bridge (stubs only)

cargo clippy --                    -D warnings
cargo clippy --features cli        -- -D warnings
cargo clippy --features webhook    -- -D warnings
cargo clippy --all-features        -- -D warnings
```

**Clippy warnings are errors in CI.** Always run `cargo clippy -- -D warnings` before pushing.

## Coding guidelines

- **Edition 2021, Rust stable.** No nightly features.
- **No `unwrap()` / `expect()` on request-path code.** Use them freely in tests and one-shot CLI error paths where panicking is the correct failure mode.
- **Log, don't println.** The `log` crate is wired via `env_logger` in the CLI; tests can set `RUST_LOG=debug` if needed.
- **Event-sourced first.** New state that needs to survive a restart or be auditable belongs in an event, not in a mutex-guarded HashMap on the runtime.
- **Config goes through `serde_yml` + a typed struct.** Raw YAML surfaces are OK only at plugin-registry boundaries (see ADR-0020), and even there the builder deserialises into a typed config.

## Architecture Decision Records

Non-trivial design choices land as ADRs in [`adr/`](adr/). Each ADR is a short markdown file with this shape:

```
# [Title]

Date: YYYY-MM-DD

## Context
Why we need this.

## Decision
What we decided.

## Consequences
Pros, cons, and what this obliges us to do.
```

When to write one:

- Adding a new top-level YAML section (`mcp_servers:`, `connectors:`, …).
- Changing a public Rust or Python contract (new `EventKind` variant, new `Runtime` method family).
- Picking between two reasonable alternatives you know you'll be asked about again.

When **not** to write one:

- Refactors without behaviour change.
- Bug fixes — put the context in the commit message.
- Anything a reader can reconstruct from the diff in under a minute.

If your change contradicts an existing ADR, update the old one with a `Status: Superseded by <n>` line rather than silently editing it — historical context stays honest that way.

Scan before deciding: `ls adr/` and read the titles. Only open a specific ADR if its filename looks directly relevant to what you're working on.

## Commit style

Conventional Commits, roughly:

```
feat(connectors): http_poll with per-connector cursor store
fix(mcp): env_clear was killing PATH — auto-forward PATH only
docs(adr-0021): record slice 3 (http_poll) implementation
refactor(plugin): extract stdio JSON-RPC transport from src/mcp
chore: bump to 0.2.3
```

Types we actually use: `feat`, `fix`, `refactor`, `docs`, `chore`, `test`. Scopes are free-form — prefer the crate path (`connectors`, `mcp`, `cli`) over generic scopes (`core`, `module`).

One logical change per commit. It's fine to have several commits in a PR — easier to review than a single megacommit.

## Pull requests

- Target `main`.
- Include a short rationale in the description — what and why, not what-the-diff-already-says.
- Add or update tests for behavioural changes.
- If the change is user-visible, update the README section that describes the feature (or add one).
- Run the full clippy + test matrix locally before pushing.

## Release versioning

Version lives in **two** manifests that must stay in sync on every bump:

- `Cargo.toml` — `version = "..."` (crate / `cargo publish`)
- `pyproject.toml` — `version = "..."` (maturin → PyPI wheel name)

The GitHub release workflow triggers on `v*` tags and maturin builds wheels from `pyproject.toml`. If you only bump `Cargo.toml`, the tag will rebuild the *previous* PyPI version and PyPI will reject it with `400 File already exists`. Always bump both before tagging.

## Security

If you think you've found a security issue, please email the maintainer privately rather than opening a public issue. A fix and disclosure plan will follow.

## License

By submitting a PR, you agree that your contribution is licensed under the project's [MIT license](LICENSE).
