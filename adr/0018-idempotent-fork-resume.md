# Idempotent Fork-Based Pipeline Resume

Date: 2026-04-18

## Context

`.drift/project.json` lists "[P3] Idempotent resume" as the next major slice. Concrete user scenario:

> I run a research pipeline. Data collection (researcher agent) is great, but the writer agent's output is poor. I edit the writer's prompt, then re-run *just* the writer step against the data the researcher already produced — without spending another LLM minute and another web_scrape on collecting the same sources.

This requires a precise definition of "resume":

- Frozen upstream — the researcher's tool calls, LLM responses and final output are *facts in the event log* and must not be re-executed. Idempotency means the writer sees byte-identical input on resume.
- Live downstream — the writer (and anything that depends on it) re-runs from scratch using the *current* `pipelines/*.yml` and `agents/*.yml` from disk, picking up the new prompt.

Two designs were considered:

1. **Replay-vs-reexec policy** — single mutable run, with per-EventKind rules ("LLM events replay if prompt matches, tool events re-execute, …"). Rejected: it gives the user N policies to memorise, breaks immutability of the event log, and the *default* (re-execute upstream tools) violates the core idempotency requirement — the researcher would silently produce a different output.
2. **Step-granular fork** (chosen) — resume = fork. Forking from step `S` mints a *new* stream that physically copies the upstream events from the parent and re-executes `S` and its DAG-descendants with the current configs.

## Decision

### Model

- **Granularity: step, not event.** Forking inside a `tool_use` / `tool_result` pair, or mid-LLM-turn, would leave the next iteration's `ContextBuilder` with a half-formed conversation. Forking by step keeps the contract simple: every frozen step has a complete, well-formed sub-stream.
- **Storage: physical copy.** The new stream is self-contained — it owns copies of every frozen step's sub-stream events plus its own `WorkflowNodeStarted/Completed` markers on the top-level stream. Trade-off accepted: ~2× storage on the frozen prefix in exchange for unchanged `EventStore`, `ContextBuilder`, `MemoryProjection`, `observe`, `runs`, Langfuse code paths. At zymi pipeline scale (≤20 steps, ≤MB-class event payloads) the duplication is negligible.
- **Configs are read from disk at resume time, for downstream steps only.** Frozen step configs are not consulted — those steps are not re-executed. If the user has also edited an upstream config, that edit is silently ignored (see *Config drift note* below).

### Semantics

Given parent stream `P` and fork point step `F`:

1. Re-executed set `R = {F} ∪ transitive_dependents(F)` from the *current* pipeline DAG.
2. Frozen set `Z = all_steps − R`.
3. Pre-flight checks:
   - Every `s ∈ Z` must appear in `P` as a `WorkflowNodeCompleted{node_id=s, success=true}`. If not — hard error, since the new run cannot reconstruct that step's output.
   - Every `s ∈ Z` must still exist in the current pipeline config. If a frozen step was deleted from disk, hard error.
   - No `s ∈ R` may have `depends_on` containing a step that did not run in `P`. If the new pipeline DAG introduces a dependency on a step that never existed in the parent, hard error with the message "step `X` now depends on `Y`, which did not run in the parent stream — start a new run instead."
4. Mint new `stream_id = pipeline-<name>-<fresh-uuid>` and new `correlation_id`.
5. Bootstrap the new stream by appending, in order:
   - `PipelineRequested` (fresh timestamp, inputs copied from `P`).
   - `ResumeForked` *(new EventKind)* with `{parent_stream_id, parent_correlation_id, fork_at_step}`. Marker only — projections ignore it; `observe` and `runs` may surface it.
   - `WorkflowStarted` (fresh).
   - For each `s ∈ Z` in DAG order: `WorkflowNodeStarted{node_id=s}` and `WorkflowNodeCompleted{node_id=s, success=true}` on the top-level stream, plus a copy of `<P>:step:<s>` to `<new>:step:<s>` (each event re-id'd, `correlation_id` rewritten, `timestamp` preserved from the parent).
6. Reconstruct `step_outputs: HashMap<String, String>` for `Z` by reading each frozen sub-stream and extracting the content of the last `LlmCallCompleted{has_tool_calls=false}`.
7. Dispatch the normal pipeline execution loop, seeded with `step_outputs` and a "skip" set of frozen step ids. The loop never spawns a step whose id is in the skip set; for steps in `R` it executes them as if they were a fresh run — current configs, current prompts, current tools.
8. On completion, append `WorkflowCompleted` and `PipelineCompleted` to the new stream, same as a fresh run.

### Config drift note

If, on resume, the contents of any frozen step's agent config (prompt, model, tool list) differ from what's on disk now, those edits are ignored by definition (the step is not re-executed). To avoid silent surprise, the resume CLI always prints the plan to stdout *before* execution:

```
Resume plan
  pipeline: research    fork at: writer
  parent:   pipeline-research-abc123…
  frozen (2):    researcher, fact_check
  ↳ events copied verbatim from parent; current configs NOT applied
  re-execute (1): writer
```

The same block prints under `--dry-run` (with a `(dry-run)` tag) and exits without writing any new events. Users who care about a config diff can `git diff` themselves; the goal of the printout is to make the frozen vs re-executed split visible before the run starts.

### CLI surface

```
zymi resume <stream_id> --from-step <step_id> [--dry-run] [-d <dir>]
```

`<stream_id>` is the parent run's stream id (visible in `zymi runs` and `zymi observe`). `--from-step` is the step id at which to fork. `--dry-run` prints the plan and exits — no events are written, and the project does not need an `llm` block configured. Without `--dry-run`, the new stream id is echoed on stdout for piping into `zymi observe --run`.

### TUI surface

In `zymi observe`, with the pipeline-graph panel focused on a node, **`Shift+R`** opens a modal popup that confirms the fork from that step against the currently selected run. On confirmation:

- The resume task is spawned off the UI thread (`tokio::spawn`) and reports back through an `mpsc` channel — the TUI does not freeze while a long pipeline runs.
- Popup states cycle `Confirm → Running → Done | Failed`. On `Done`, `Enter` moves the cursor in the runs panel to the new stream and loads it.
- **No approval handler is attached** (the TUI owns the terminal, so terminal-mode prompts cannot draw). Steps tagged `RequiresHumanApproval` will be denied (fail-closed). Use `zymi resume` from a regular shell for approval-gated forks.

### Command shape

`commands::RunPipeline` gains an optional `resume: Option<ResumeContext>` field carrying:

```rust
pub struct ResumeContext {
    pub parent_stream_id: String,
    pub fork_at_step: String,
    pub frozen_outputs: HashMap<String, String>, // step_id → output, for context building
    pub frozen_step_ids: HashSet<String>,        // skip set for the executor
}
```

The CLI orchestrates bootstrap + dispatches a `RunPipeline` with `stream_id = Some(new_id)` and `resume = Some(ctx)`. `run_pipeline::handle` checks `resume`:

- if `Some`, skip its own `PipelineRequested` emission (caller already did it), seed `step_outputs` from `frozen_outputs`, never spawn a step whose id is in `frozen_step_ids`;
- always emit `PipelineCompleted` at the end when `resume.is_some()` (parallel to the local-CLI path), so `zymi runs` shows the new run cleanly.

### EventKind addition

```rust
ResumeForked {
    parent_stream_id: String,
    parent_correlation_id: Uuid,
    fork_at_step: String,
}
```

History-only, no state effect on any projection.

## Alternatives Considered

- **Reference / chain (parent_stream_id pointer, no copy)** — each new fork is an empty stream that says "for events ≤ N, see parent". Zero duplication, but every reader (`ContextBuilder`, `MemoryProjection`, `observe`, Langfuse mapper) must be taught to follow the chain, and fork-of-fork becomes recursive. Cost spread across the codebase outweighs the storage win at this scale.
- **In-place truncate-and-rerun** — drop events after `fork_step` from the parent stream, continue. Breaks the append-only invariant of the event store, makes `verify_chain` harder, and destroys the audit log of what failed. Not seriously considered.
- **Event-granular fork** — fork from any sequence number, not just step boundaries. Useful for "rerun this one tool call" but requires synthesising a context that ends mid-turn. Step-granular covers the writer-rewrite use case; event-granular can be added later as `--from-event <seq>` if a real demand appears.
- **Re-execute upstream (no resume, just fast retry)** — simpler implementation but loses idempotency: rate-limited APIs, paid LLM calls, web scrapes that have since changed all give different results. Defeats the user's stated goal.
- **Preserve original timestamps on copied events** vs. **re-stamp with `Utc::now()`** — chose preserve. The event happened at T₀; the new run inherits the evidence. Re-stamping would make the timeline lie.

## Consequences

- **One new EventKind (`ResumeForked`).** Backward-compatible (additive enum variant); existing JSON streams roundtrip unchanged.
- **`commands::RunPipeline` gains `resume: Option<ResumeContext>`.** Default is `None`; existing call sites (`cli::run`, `cli::serve`, Python bridge) compile unchanged.
- **`run_pipeline::handle` gains a skip-set + frozen-outputs branch.** Tested under both shapes (with and without resume).
- **Storage cost for the frozen prefix is duplicated.** At pipeline scale this is on the order of kilobytes per fork; if it ever matters, switch to chain semantics behind a feature flag.
- **Resume from a `Failed` parent run is supported** — only `success=true` for *frozen* steps is required; the failed step itself is in the re-executed set and runs again with current configs.
- **TUI integration shipped (Shift+R in `zymi observe`).** Same code path as the CLI — the TUI dispatches `resume_pipeline::handle` with the focused graph node as `fork_at_step`. Approval is fail-closed inside the TUI by design.
- **Resume of a resume works for free.** A forked run is just another stream with `PipelineRequested` and `WorkflowNodeCompleted` events; the algorithm has no special case for it. The `ResumeForked` marker chain is informational only.
