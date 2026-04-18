# Observability TUI and CLI Command Restructure

Date: 2026-04-17

## Context

Two related gaps in day-to-day ergonomics:

1. **Discoverability**. There is no way to see at a glance *what pipelines exist in this project* or *what runs have been executed*. Users have to `ls pipelines/` and `zymi events --limit 200 | grep pipeline_requested` respectively.
2. **Forensic debugging**. `zymi events` dumps a flat chronological list. For a broken run you have to scroll through a wall of text to reconstruct what happened. The original zymi (sibling project, full chat TUI) had an event-stream side panel that proved valuable — but the full chat TUI doesn't fit zymi-core's non-interactive pipeline engine.

We need structured introspection commands and a dedicated read-only TUI.

## Decision

### CLI surface (restructure)

| Command | Purpose |
|---------|---------|
| `zymi pipelines` | List pipelines in `pipelines/*.yml`: name, description, step count, inputs |
| `zymi runs [--pipeline NAME] [--limit N]` | List runs: stream_id, pipeline, started_at, duration, status, prompt preview |
| `zymi events [--stream] [--kind] [--limit] [--raw] [-v]` | Unchanged semantics; `--json` renamed to `--raw` |
| `zymi observe [--run ID]` | Interactive 3-panel TUI over the event store |

`run`, `verify`, `serve`, `schema`, `init` are unchanged.

### --json → --raw (flag rename)

One way to get machine-readable output. `--raw` emits one JSON object per line (same as the old `--json`). The name "raw" generalises past JSON — if we ever emit msgpack or similar, the semantics (unformatted, complete, pipe-friendly) still hold.

Old `--json` is removed, not aliased. Users pipe into `jq`; a one-release breakage with a clear error message is cheaper than permanent flag duplication.

### observe TUI layout

Three vertical panels:

```
┌─ Runs ──────┐┌─ Pipeline graph ────┐┌─ Events ──────────────┐
│ run list   │ │ vertical DAG with  │ │ chronological events │
│ (selector) │ │ per-node status    │ │ of the selected run  │
└────────────┘└─────────────────────┘└──────────────────────┘
 q quit  Tab cycle focus  ↑↓ nav  Enter expand/focus-node  f follow  r refresh  R fork-resume
```

- **Runs panel**: one row per run (one `PipelineRequested`+`PipelineCompleted` pair, matched by `correlation_id`). Running runs show `⏳`. Forked runs (ADR-0018) get a third line `↩ from <parent>:<step>`.
- **Graph panel**: loads `PipelineConfig` from project `pipelines/<name>.yml`, renders the DAG. Each node overlaid with status derived from `WorkflowNodeStarted` / `WorkflowNodeCompleted` events of the selected run. `Enter` on a node scrolls the events panel to the first event with that `node_id`. **`Shift+R`** opens a fork-resume modal that re-runs the focused step + DAG-descendants against the current configs (see ADR-0018 for semantics + approval caveat).
- **Events panel**: same formatting as `zymi events` output (shared formatter module). `Enter` expands full detail of the highlighted event. `f` follows tail via `StoreTailWatcher`.

Built on `ratatui` 0.29 + `crossterm` 0.28 (same versions used in sibling zymi project). Deps are gated under the `cli` feature — library consumers stay lean.

### Vertical DAG layout algorithm

Pipelines are DAGs declared as `steps[].depends_on`. Rendering:

1. **Rank assignment**: `rank(n) = max(rank(p) for p in depends_on(n)) + 1`, roots at rank 0.
2. **Per-rank layout**: all nodes at rank `r` are drawn horizontally on the same row, top-to-bottom across ranks.
3. **Connectors**: straight `│` for single dep; `├─┐`/`└─┤` for fork/join. Crossings are accepted — pipeline DAGs are small (≤10 nodes typical), optimal edge ordering is over-engineering at this scale.
4. **Fallback**: if a rank has more than 4 nodes, collapse to a flat `rank r: [a, b, c, d, e, f]` line. Forensics use case is served either way.
5. **Status overlay**: `✓` done / `⏳` running / `✗` fail / `·` pending, sourced from `WorkflowNodeStarted`/`WorkflowNodeCompleted` events of the selected run. If the pipeline config is missing (old run, file deleted), the graph panel shows a linear fallback built from `WorkflowNodeStarted` events only, with a warning banner.

### Shared event formatter

Extract the per-kind `(icon, label, short_detail, full_detail, color)` mapping from `src/cli/events.rs` into `src/cli/event_fmt.rs`. Both `events` and `observe` consume it. This prevents the two views from drifting in how they describe the same event kind.

## Alternatives Considered

- **Single-panel live tail** (`zymi observe --follow`, one pane, no runs list, no graph). Simpler (~400 LoC) but collapses to a fancier `zymi events --follow | less`. The forensic use case ("what happened in *this* run?") needs a run selector and a visual of the pipeline shape, otherwise the TUI doesn't earn its complexity over tmux + `watch`.
- **Port the original zymi chat TUI wholesale**. ~96KB of code, 90% of which (chat messages, approval flow, model selector, markdown rendering, input textarea) is meaningless in a non-interactive pipeline engine. Only the right-panel observability maps across, and even that needs restructuring around runs rather than a single open chat.
- **Full Sugiyama layered graph with edge-crossing minimisation**. Optimal for dense DAGs but pipeline DAGs are tiny. A simple rank-based layout with occasional crossings is readable for anything a human wrote by hand.
- **`zymi events --json` kept alongside `--raw`**. Two flags that mean the same thing is permanent cognitive debt. Breaking once is cheaper than never recovering the name.

## Consequences

- **New deps on ratatui + crossterm** (gated under `cli`) — adds ~2s to cold build with `--features cli`, zero impact on library consumers.
- **`--json` → `--raw` is a breaking flag change.** Called out in release notes. Script breakage is detectable at first failing run with a clear "unexpected argument" error from clap.
- **`runs` command adds one new query pattern** (`PipelineRequested` pairs joined to `PipelineCompleted` by `correlation_id`). Implementable on the existing `EventStore` trait — no schema changes. Pattern can be lifted into a helper used by both `runs` and `observe`.
- **`observe` gives us a forensic tool for reported bugs**. Scrolling timestamps in `events` output is the current alternative; this collapses "find failing run → see which node failed → read that node's events" to three keypresses.
- **Graph renderer is a new module with its own complexity envelope.** Kept isolated under `src/cli/observe/graph.rs` so it's easy to rewrite if the layout algorithm proves insufficient — no other code depends on its internals.
- **Fork-resume from the graph** is now wired (`Shift+R`, see ADR-0018). The TUI dispatches the same `resume_pipeline::handle` as `zymi resume`, off the UI thread via `tokio::spawn` + `mpsc` so the screen does not freeze during execution.
- **Future extension points** (not in this ADR): `zymi observe --run ID --export html` for shareable postmortems; in-TUI prompt diff between the parent run's frozen step configs and current disk.
