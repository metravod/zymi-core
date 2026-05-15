# Per-step `context.mode: fresh` opt-out

Date: 2026-05-15

Status: Accepted. Extension of ADR-0016 (context window management).

## Context

ADR-0016's `step_stream_id` is `{stream_id}:step:{step_id}`. When `stream_id == chat_id` (the natural choice for a long-lived conversational deployment), every run for that chat lands in the same sub-stream per step. ContextBuilder reads the whole sub-stream and reconstructs the conversation tail. That is exactly right for conversational steps — the smalltalk agent should remember it just answered the same user a minute ago.

It is wrong for retrieval / analytical steps. A `retrieve` step that takes a question, hits a vector store, returns chunks, and exits has no business carrying forward the prior run's question-and-chunks. Bleeding prior context in means the retrieval LLM either re-uses stale arguments, drifts toward unrelated topics, or wastes the budget masking observations from yesterday.

The same gap surfaces from two angles:
- Memory entry `project-step-stream-id-stateless`: user-reported papercut, captured the temptation to "fix" this by floating `step_stream_id` per run. That fix breaks event-store coherence, resume, and cross-run observability — three contracts ADR-0016 deliberately leans on.
- Real consumer (zymi-rag-bot chat-router): the routed `knowledge` branch is a retrieval step, hit on most turns, and currently carries history.

The user does not want a fresh *stream*. They want a fresh *context*. ContextBuilder already owns context — extend its surface, leave stream identity alone.

## Decision

Add an optional `context:` block to `PipelineStep` (config), with one field for now:

```yaml
steps:
  - id: retrieve
    agent: researcher
    task: ...
    context:
      mode: fresh    # default: inherit
```

Semantics:

- `mode: inherit` (default, absent block ≡ this): no change. ContextBuilder reads the full sub-stream as today.
- `mode: fresh`: Layer C (the observation tail) is filtered to events with `timestamp >= step_start`. `step_start` is captured by `run_agent_step` immediately before its first iteration. Within the run, ReAct iterations still see each other's tool calls (they timestamp after `step_start`). Across runs, the previous run's events are dropped.

Layer A (system + task) and Layer B (memory snapshot) are untouched. Compaction and masking still run, just over the filtered tail.

Only agent steps are affected. Tool steps (`kind: Tool`) build no context; the block is accepted but inert there. No validation error — keeps the config additive and the same step shape can be flipped between agent and tool without losing the block.

`step_stream_id` is untouched. Resume reads it the same way. Cross-run observability (`zymi observe` reading the sub-stream) is untouched — the events are still there, only Layer C reconstruction filters them. The event store remains the source of truth.

## Consequences

- Pro: solves the conversational-vs-retrieval distinction at the right layer, with one knob, no new event types, no new stream namespaces.
- Pro: per-step granularity. A pipeline can mix conversational and retrieval steps freely.
- Pro: cheap to roll back — `mode: fresh` becomes `mode: inherit` and the runtime behaves as it did pre-ADR.
- Con: another field on `PipelineStep`. Within budget — the surface is already small and this is the natural place.
- Con: `step_start` is wall-clock. If two steps with `mode: fresh` start within the same nanosecond on the same sub-stream (impossible in practice — same stream is sequential), filtering is order-dependent. Documented, not a real risk.
- Future: this is the right hook for richer per-step context policy (e.g. `context.window: N`, `context.forget_after_turns: N` overrides). Not in scope here — wait for a concrete consumer before adding.

## Rejected alternatives

- Float `step_stream_id` per run (`{stream_id}:step:{step_id}:{correlation_id}`). Breaks resume (resume locates events by stream_id), creates orphan sub-streams nobody re-reads, and forces cross-run observability to stitch by correlation_id. Three contracts broken to bypass one.
- Reuse `forget_after_turns` from ADR-0016 §Slice 6 with a per-step override. Conflates a turn cap (graceful drop of old turns) with a hard run boundary; the user really wants the boundary, not a cap.
- `stateless: true` boolean alias. Considered, deferred — `mode:` extends cleanly to future modes, a boolean does not.
