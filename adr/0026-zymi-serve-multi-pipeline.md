# `zymi serve` accepts multiple pipelines and `--all`

Date: 2026-05-07

## Context

`zymi serve <pipeline>` took a single positional pipeline name. Projects with N pipelines had to run N `serve` processes side by side — one event store, N tail watchers, N runtimes, N approval channels. For the common shape (a couple of pipelines per project, all sharing the same store), this is wasteful and operationally awkward (each process needs its own systemd unit / supervisor entry).

`EventCommandRouter` already knew how to filter `PipelineRequested` events by name (`pipeline_filter: Option<String>`), so the multi-process workaround existed only because the CLI surface required exactly one name and the filter held a single string.

## Decision

CLI:

```
zymi serve foo bar   # whitelist of names (positional varargs)
zymi serve --all     # every pipeline declared in the project
zymi serve           # error: pass names or --all
zymi serve foo --all # error: clap-enforced conflict
```

Backwards compatible: `zymi serve foo` keeps working.

Implementation:

- `Command::Serve` carries `pipelines: Vec<String>` plus `all: bool` with mutual `conflicts_with` on both sides.
- `cli::serve::exec` resolves the served set: `--all` expands to every workspace pipeline; positional names are deduped and validated against the workspace, with one error listing all unknown names plus available ones.
- `EventCommandRouter::pipeline_filter` becomes `Option<HashSet<String>>`. `with_pipeline_filter(name)` is replaced by `with_pipeline_filters(impl IntoIterator)`. `--all` uses the `None` branch ("accept any workspace pipeline"); positional names use the whitelist.

One process now holds one runtime, one tail watcher, and one approval channel for all served pipelines, and the operator log line names every pipeline it accepts.

## Consequences

- Single-process multi-pipeline serving is the new default for projects that previously needed a process per pipeline.
- Positional and `--all` are clap-mutually-exclusive at parse time, not at validation time, so the error is surfaced before any project load.
- The `EventCommandRouter` API broke (`with_pipeline_filter` → `with_pipeline_filters`). Only one in-tree caller (`cli/serve.rs`); external embedders, if any, have to update the call. Acceptable given pre-1.0 status.
- `--all` follows the workspace at load time. Adding a new pipeline file requires restarting `serve` — the watcher does not hot-reload `pipelines/*.yml`. Same constraint as today; called out so it is not mistaken for a regression.
