# Conditional DAG edges

Date: 2026-05-14

## Context

A real-world build (concierge-style agent: triage the user query and either answer directly or fan out to a RAG lookup) showed that the pipeline DAG is currently fully static. `depends_on:` is a hard, unconditional edge: every declared step runs once its parents complete. The only way to express "answer briefly OR do retrieval, depending on the agent's call" today is either

- collapse both branches into a single agent step's ReAct loop, hiding the decision inside the LLM and out of the event-store trace, or
- run both branches unconditionally and discard one downstream, wasting tokens and producing misleading traces.

Both miss the point of having a declarative DAG in the first place: traceability. The native event-store / TUI is zymi-core's observability story (see ADR-0017, and the deliberate non-adoption of Langfuse) — branching decisions must be first-class events, not LLM-internal state.

The relevant current shape:

- `PipelineStepRaw` (`src/config/pipeline.rs`) — wire format with `id`, `agent|tool`, `depends_on`. No conditional field.
- `build_execution_plan` (`src/config/dag.rs`) — Kahn topological sort into levels. Static.
- `StepResult { output: String, … }` (`src/handlers/run_pipeline.rs`) — a step's output is a flat string. No structured / typed agent output exists yet.
- Template resolver: `${steps.<id>.output}` substituted by `str.replace` in `resolve_str_template` and `resolve_task_template`. No expression language, no field paths.

This ADR adds conditional edges without growing either of the last two surfaces.

## Decision

Add an optional `when:` predicate to a pipeline step. When present, the step runs only if the predicate evaluates true after dependencies complete; otherwise it is skipped. Skips cascade: any step whose `depends_on` contains a skipped step is itself skipped without evaluating its own `when:`.

### Schema

```yaml
steps:
  - id: concierge
    agent: concierge
    task: "Decide route for: ${inputs.q}"

  - id: short_answer
    agent: helper
    task: "Answer: ${inputs.q}"
    depends_on: [concierge]
    when: "${steps.concierge.output} == 'short'"

  - id: rag_lookup
    tool: pinecone_query
    args: { query: "${inputs.q}" }
    depends_on: [concierge]
    when: "${steps.concierge.output} == 'rag'"
```

`when: Option<String>` lands on `PipelineStepRaw` and `PipelineStep`. Topology (and therefore the Kahn execution plan) is unchanged — `when:` is a runtime filter, not a DAG edge.

### Router contract

A routing agent emits its decision through the existing tool/output channel — no new structured-output mechanism. Two practical patterns, both supported with no engine change:

1. The concierge agent has a `route(label)` tool in its catalog; the tool's return value becomes the agent step's `output`. The system prompt instructs the agent to finalise with `route(...)`.
2. A `kind: tool` step (e.g. `tool: pick_route`) computes the label deterministically — no LLM hop at all.

Both keep the branch key as a plain string in `${steps.<id>.output}`. Multi-field routing (`when: route == 'rag' && lang == 'ru'`) is deferred — it requires structured step output, which is a separate ADR.

### Expression grammar

Minimal, hand-written parser. No external crate.

```
EXPR := COMP (LOGIC COMP)*
COMP := VALUE OP VALUE
OP    := '==' | '!='
LOGIC := '&&' | '||'
VALUE := single-quoted-string | unquoted-token
```

`&&` and `||` have equal precedence and evaluate left-to-right. No parentheses, no `not`, no regex, no `in [...]`, no numeric comparison. Template substitution (`${inputs.*}`, `${steps.*.output}`) runs **before** parsing; the comparison sees fully-resolved strings.

Whitespace around tokens is ignored. Comparison is byte-exact on the resolved strings — no normalisation, no case-folding.

### Skip semantics

- **Cascade**: if any direct dependency in `depends_on` was skipped, the step is skipped with `reason: "ancestor_skipped"`. `when:` is not evaluated.
- **Predicate false**: `when:` evaluates to false → skipped with `reason: "when=false"`.
- **Predicate true / no `when:`**: normal execution.
- Skipped steps produce no `StepResult` entry in `PipelineResult.step_results`. References to a skipped step's `${steps.<id>.output}` in surviving siblings resolve to the literal unsubstituted token (existing resolver behaviour for unknown keys) — a survivor that needs a skipped step's output should itself carry a `when:` that excludes that case.
- A new event `StepSkipped { step_id, reason }` is emitted at the position the step would have run, before the level moves on. Visible in `zymi observe` and the TUI.
- If `PipelineOutput.step` resolves to a skipped step, the pipeline fails: `PipelineCompleted { success: false, error: Some("output step '<id>' was skipped"), … }` and `handle()` returns `Err(...)`. This is consistent with ADR-0027's fail-fast posture — silently returning empty output would be the worst of both worlds.

### Validation

`src/config/validate.rs` gains:

- `when:` is only meaningful on a step with at least one entry in `depends_on`. A `when:` on a root step is a config error (nothing has happened yet to branch on).
- Parse the expression at config-load time and surface syntax errors with the step id, same shape as existing validation errors.
- `${steps.X.output}` references inside `when:` must point at a declared `depends_on` of the same step. Referencing a non-ancestor is a config error — otherwise the predicate would race against an unrelated branch.

## Consequences

- Concierge / triage pipelines become expressible declaratively. Every routing decision lands in the event log as either a `route(...)` tool call (existing event) or a `StepSkipped` event for the branches that didn't fire — that is the trace.
- The DAG topology stays static; only runtime liveness is conditional. Kahn's plan, parallelism within a level, and replay/resume (ADR-0018) all keep working. Resume treats a skipped step as terminal in the same way as a completed one.
- `${steps.X.<field>}` is **not** introduced. Multi-field routing remains out of reach until structured agent output ships in a future ADR. Acceptable: the concrete consumer (concierge → answer | RAG) is single-label.
- Out of scope, by design: `depends_on_any` / join-after-branch (no consumer; cascade-skip covers terminal-branch shape), loops/cycles, regex / `in [...]` in `when:`, `on_error: continue` (same reasoning as ADR-0027). Each of these is an explicit follow-up ADR with a real driver, not a speculative extension now.
- Breaking changes: none. Pipelines without `when:` are byte-identical to today.
