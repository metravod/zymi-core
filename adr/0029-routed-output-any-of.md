# Routed pipeline output via `any_of`

Date: 2026-05-14

## Context

ADR-0028 introduced conditional DAG edges (`when:` + cascade-skip), making concierge-style routing expressible: a router step emits a label, and downstream branches each guard on that label. Exactly one branch survives. The remaining piece was the *output* contract.

Today `output: step: <id>` declares a single, fixed terminal step. When that step is on a branch that was skipped, the run hard-fails — by design (ADR-0028 §"Skip semantics"), since silently swapping in another branch's output would defeat traceability. That correctly catches author mistakes for static pipelines, but for the routed shape it punishes the very pattern ADR-0028 made expressible: in `concierge → smalltalk | knowledge`, exactly one of `smalltalk` / `knowledge` survives, and which one is decided at runtime.

Worked examples motivating this: `zymi-rag-bot`'s `pipelines/chat.yml` (router agent + small-talk arm + RAG-backed arm). The chat router is the concrete consumer that ADR-0028 mentioned was missing for `depends_on_any` / join-after-branch. With the consumer in hand, the gap closes.

The relevant current shape:

- `PipelineOutput { step: String }` — single-step terminal declaration (`src/config/pipeline.rs`).
- `run_pipeline.rs` end-of-run: hard-fails if the declared output step is in the `skipped` set; otherwise picks `step_outputs[output.step]`, with a soft fallback to "last plan level, last step" if missing.
- No event records the resolution decision; the trace shows `StepSkipped` for the dead branches and a final `ResponseReady`, but nothing names *which* step became the final output.

## Decision

Add a second form of `output:` that picks the first surviving step from an ordered list:

```yaml
output:
  any_of: [smalltalk, knowledge]
```

Semantics:

- Walk `any_of` in declared order.
- The first id that was **not** skipped and has a `step_outputs` entry becomes the final output.
- If every id is skipped → hard fail (`PipelineCompleted { success: false, error: "all any_of outputs were skipped: [...]" }`), consistent with ADR-0028's "fail-fast over silent fallback" stance.
- If every id ran but produced no output (e.g. early halt), behaviour matches the legacy `step:` form: fall back to plan-last-level/plan-last-step output and surface the halt reason — no new failure mode here.

Existing `output: step: <id>` is unchanged, including its hard-fail-when-skipped behaviour. Wire format:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PipelineOutput {
    Step(StepOutput),       // { step: <id> }
    AnyOf(AnyOfOutput),     // { any_of: [<id>, ...] }
}
```

`#[serde(untagged)]` keeps existing pipelines byte-identical. Untagged-enum errors on typos are notoriously poor, so a custom `Deserialize` wrapper produces "expected `step:` or `any_of:`, got `<keys>`" instead of "data did not match any variant".

### Why not `switch:` / `cases:`

A `switch: "${steps.route.output}" / cases: {SMALLTALK: smalltalk, KNOWLEDGE: knowledge}` form was considered and rejected for v1. The router output already drives the `when:` predicates on the branches; encoding the same mapping a second time under `cases:` creates two registries that can silently drift, with no validator able to cross-check them (it would have to parse `when:` expressions and pattern-match literal equalities against `cases` keys). The concrete shape — N branches all guarded on a single label, mutually exclusive — is fully covered by "first non-skipped wins". `switch:` becomes worth its surface area only when a real consumer appears whose `output` decision differs from its `when:` decisions (e.g. group multiple route values into one terminal step). Until then, `any_of:` is the minimum shape that closes the gap.

### Validation

`validate_pipeline_refs` is extended:

- `any_of:` must be non-empty.
- Every id in `any_of` must reference a declared step (same error shape as the existing `output references unknown step` check).
- Duplicates inside `any_of` are an author error and rejected at load time — the ordered-first-wins semantics make a duplicate id either unreachable or a typo.

No check is performed that the listed steps are mutex / terminal / on different branches. The runtime "first non-skipped" rule means a non-mutex configuration just biases toward the earlier entry, which is well-defined; warning on it would require flow analysis with no payoff.

### Observability: `OutputResolved` event

A new history-only event records which step was chosen:

```rust
EventKind::OutputResolved {
    chosen_step: String,
    via: OutputResolutionVia,
}

pub enum OutputResolutionVia {
    Step,                                // legacy `step:` form
    AnyOf { skipped: Vec<String> },      // ordered list of any_of ids skipped before the winner
}
```

Emitted at the end of `run_pipeline.rs`, after `WorkflowCompleted` and before `ResponseReady` / `PipelineCompleted`. Not emitted when no `output:` is declared (no decision was made — `final_output` came from the plan-last fallback) and not emitted when resolution failed (no winner to name). Replay does not reconstruct the decision — it's a flat record of what the resolver picked at run time, mirroring `StepSkipped`'s "history-only" stance.

This is the missing trace link between `StepSkipped` and `ResponseReady`: the user sees *which branch became the answer*, not just which branches did not.

## Consequences

- Concierge / triage pipelines can now declare their output without lying about the topology. The chat-router pattern in `zymi-rag-bot` works on stock zymi-core without YAML gymnastics or post-pipeline glue.
- `output: step:` semantics are unchanged. Existing pipelines parse and behave byte-identically (verified by the back-compat test).
- The event store gains one row per pipeline run when an `output:` was declared. Negligible volume, and worth it for debug — without `OutputResolved`, "smalltalk answered" vs "knowledge answered" is reconstructable only by replaying `step_outputs` matching against `ResponseReady.content`.
- `#[serde(untagged)]` is a known foot-gun for error messages; the custom `Deserialize` impl localises that cost (~25 lines) rather than letting it leak into user-facing config errors.
- Out of scope, by design: `switch:` / `cases:` (see "Why not" above — no current consumer; revisit when one appears whose routing key ≠ branch boundaries); `any_of:` with predicates / fallthrough rules (predicates already live in `when:`; the extra surface is unjustified); a `default:` keyword for `any_of:` (would silently mask "all branches skipped", which is the one failure mode we want to keep loud).
- Breaking changes: none. Pipelines without `any_of:` keep their exact current behaviour, including the ADR-0028 hard-fail on skipped output step.
