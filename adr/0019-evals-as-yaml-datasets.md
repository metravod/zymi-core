# Evals as YAML datasets over pipelines/agents

Date: 2026-04-18

## Context

`.drift/project.json` lists "[P8] Add evals — create from yaml too (prompt, result, judge), run for pipelines/agent and get pretty result". Now that we have (a) event-sourced runs with enriched LLM/tool payloads (ADR-0016 §1a), (b) the observability TUI + `zymi runs` (ADR-0017), and (c) idempotent fork-resume from any step (ADR-0018), an eval layer becomes cheap: an eval is just *a batch of runs with a verdict attached*, and the existing traceability machinery carries all of that for free.

Concrete user scenario:

> I have `pipelines/research.yml`. I want to define 10 benchmark prompts in `evals/research_smoke.yml`, run them all against the current pipeline, let an LLM-as-judge score each output against a rubric, and get back a summary table (pass/fail, latency, cost). When a case fails, I want to open it in the TUI as a normal run and fork-resume from the weak step with a new prompt.

Two shapes for where eval events live were considered:

1. **Inline** — eval judge events go on the same stream as the case's pipeline run. Rejected: pollutes the "pure pipeline" stream (so every `observe` view of a run now mixes domain events with eval-meta events), and forces `ContextBuilder` to know which events are "real" vs "judging".
2. **Eval stream as parent of case streams** (chosen) — eval run has its own `stream_id` carrying `EvalRequested`/`EvalCaseCompleted`/`EvalCompleted`; each case's pipeline run is a separate stream, linked by `correlation_id` and an explicit `parent_eval_stream_id` field on `EvalCaseCompleted`. Reads like a run-of-runs in the TUI, keeps pipeline streams untouched.

## Decision

### YAML schema

An eval file lives at `evals/<name>.yml`:

```yaml
name: research_smoke
description: "smoke test — 10 prompts the research pipeline must not fail"

target:
  # exactly one of:
  pipeline: research         # runs the full pipeline per case
  # agent: writer            # runs a single agent step (future slice)

cases:
  - id: case_1
    inputs:                  # map<string, string>, same keys as pipeline inputs
      topic: "rust event sourcing"
    expect:                  # optional short-circuit checks (all optional)
      contains: ["event log", "projection"]
      not_contains: ["i cannot"]
      regex: null
    judge:                   # optional LLM judge; omitted ⇒ only `expect` runs
      rubric: |
        Does the answer explain event sourcing in Rust concretely,
        with at least one code-shaped example? Score 1-5.
      pass_threshold: 4       # integer score the judge must return to pass
```

Top-level `judge:` block may set defaults (`model`, `rubric`, `pass_threshold`) that individual cases inherit and override. `target.agent:` is schema-reserved for a follow-up slice; slice 1 ships pipeline targets only.

### CLI

`zymi eval <file> [--parallel N] [--raw] [--dir <root>]`.

- `<file>` is a path to `evals/*.yml` relative to project root (or absolute).
- `--parallel N` — run N cases concurrently (default 1; cases are independent runs).
- `--raw` — JSONL one record per case (for pipes, CI, Langfuse ingest).
- Default output: a pretty table — `case_id | status | score | latency_ms | cost_usd? | error`, plus a summary line (`8/10 passed, 2 failed`). Exit non-zero if any case fails.

A new `Resume` sibling already exists on the TUI (ADR-0018 Shift+R); the `observe` TUI gets an `evals` section in slice 2 — out of scope for slice 1.

### Event model

Three new `EventKind` variants, all on the *eval* stream (not the case pipeline streams):

```rust
EvalRequested {
    eval_name: String,
    target: EvalTarget,          // enum { Pipeline(name) | Agent(name) }
    case_count: usize,
}

EvalCaseCompleted {
    case_id: String,
    case_stream_id: String,       // the child pipeline stream for this case
    passed: bool,
    score: Option<i64>,           // LLM judge score if judge ran
    latency_ms: u64,
    verdicts: Vec<EvalVerdict>,   // {kind: "contains"|"regex"|"judge", passed, detail}
    final_output_preview: String, // truncated, for TUI
}

EvalCompleted {
    eval_name: String,
    passed: usize,
    failed: usize,
    success: bool,                // = failed == 0
}
```

- `correlation_id` on all three is the eval run's correlation id, freshly minted per `zymi eval` invocation.
- Each case's `RunPipeline` command uses `from_request(..., correlation_id = <fresh per case>, stream_id = <fresh per case>)`. The child run looks indistinguishable from a normal `zymi run` from `zymi runs`' perspective — the linkage is via `EvalCaseCompleted.case_stream_id`, not via shared streams.
- `EvalVerdict.kind = "judge"` events capture the judge's raw response text in `detail` for audit; the judge never calls tools.

Backward compat: new variants with `#[serde(default)]` fallbacks on optional fields, same pattern as ADR-0016 §1a.

### Judge contract

LLM-as-judge is a simple `ChatRequest`:

- `temperature: 0.0` (not 0.7) — determinism matters more than phrasing.
- System prompt: hard-coded template that tells the judge to output `SCORE=<int>\nREASONING=<short>` and nothing else; `score` is parsed with a strict regex, parse failure ⇒ `passed=false` with `detail="judge output unparseable"`.
- No tools offered to the judge.
- Judge uses the same `LlmProvider` as the pipeline by default (`runtime.provider()`); a `judge.model:` override at top-level / case level can later spin up a second provider, but slice 1 keeps it to one provider to avoid doubling the config surface.
- Short-circuit: if `expect.contains`/`not_contains`/`regex` already failed, the judge is *not* called — saves a token and gives cleaner failure reasons.

### Integration with fork-resume (ADR-0018)

Zero new code. Because each case is a normal pipeline stream, `zymi resume <case_stream_id> --from-step <step>` already works on failed cases. Intended workflow: eval fails ⇒ `zymi observe` shows the case run ⇒ user fork-resumes from the weak step with a new agent prompt ⇒ re-runs the *single* case to verify the fix before re-running the full eval.

### Slicing

- **Slice 1** — YAML schema + loader, `EvalRequested/EvalCaseCompleted/EvalCompleted` events, `zymi eval <file>` pipeline-target only, `contains`/`not_contains`/`regex` short-circuits, LLM judge with rubric + pass_threshold, pretty table + `--raw`. `zymi init` scaffolds `evals/example.yml`.
- **Slice 2** — `target.agent:` for single-agent evals (simpler setup: bypass pipeline loader, invoke one `run_agent_step`-equivalent). Evals panel in the observe TUI.
- **Slice 3 (optional)** — cost tracking (sum `TokenUsage` across case sub-streams), baseline snapshots (`zymi eval ... --baseline <prior-run-id>`), and drift reporting.

## Consequences

**Pros**

- Re-uses every piece of infrastructure already built: event store, bus, `RunPipeline` handler, `ContextBuilder`, TUI, `runs` listing. Net new code is the YAML loader, three events, and the judge harness.
- Each case is a full first-class pipeline run — fork-resume, audit, Langfuse export all work unchanged.
- Eval stream / case stream separation keeps domain streams clean; `observe` for a failing case shows the pipeline, not the judge.

**Cons**

- LLM-as-judge is non-deterministic in absolute terms even at `temperature=0` across model revisions; pass/fail on a given rubric can flip between model versions. Partially mitigated by `expect.*` short-circuits (cheap, deterministic) and by letting the user pin `judge.model:` in a follow-up slice.
- 2× event volume vs. inline (eval stream + case streams). Acceptable given event payloads at our scale are ≤MB; the eval stream itself is small (O(cases) events).
- `--parallel > 1` means case streams can interleave. They never share `stream_id`, so the event log is still correct, but a user tailing the DB in real time sees concurrent runs. Same concern as any parallel `zymi run`, not new.

**Risks to watch**

- If a case pipeline emits a `ShellSessionStarted` and is force-killed, the shell reaper must still close it — existing behaviour (ADR-0015 §3) covers this but should be spot-checked under `zymi eval --parallel 4`.
- `judge.rubric` + `final_output` can be large; truncate the output in the judge prompt (e.g. last 4 KB) rather than blindly concatenating.
