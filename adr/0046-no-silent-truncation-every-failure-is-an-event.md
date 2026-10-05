# No silent truncation; every failure is an event

Date: 2026-10-05

## Context

The first live runs of the `subbotnik` home pipeline (ADR-0044) produced
reports that looked finished and weren't. Each cause was the engine quietly
handing on less than it had, or quietly losing a failure:

1. **Tool output cut at 4000 bytes, before the event.** `truncate_output(…,
   4000)` in `engine/tools.rs` / `runtime/action_executor.rs` ran at
   execution time, so `ToolCallCompleted.result` itself was the cut text. A
   5-host recon (~6 KB with Cyrillic and box-drawing) reached the model
   without two hosts — and the model filled them in from the cleanup log,
   even though the cut carried an explicit `[truncated at 4000 chars]`
   marker. The full output existed nowhere. (`read_file` was cut at 8000,
   HTTP bodies at 8000, error streams at 2000.)
2. **Tool-call arguments cut at 200 chars in the event** — for agent tool
   calls and deterministic tool steps alike (a `write_file` step's `content`
   was reduced to a preview in the record).
3. **Answers cut at `max_tokens` passed as complete.** Agent steps sent a
   hard-coded `max_tokens: 4096`; a reasoning model spent most of it
   thinking, the visible report ended mid-word, and the run showed `✓ ok`
   because `finish_reason` was never read.
4. **A failed LLM call left no terminal event** — `LlmCallStarted`, then
   nothing (EventKind had no failure variant). 0.9.2 (ADR-0045) sealed the
   *run*; the *call* still had no end and its cause was only in stderr.
5. **`zymi events --stream <run>` hid the step sub-streams** where tool and
   LLM calls live, which made (4) look like lost events during diagnosis.

Rejected alternative: **keep the journal whole but truncate what enters the
model's context.** Rejected by the owner, on the evidence of (1): a marked
cut still produced hallucinated hosts. Partial input to a model is the
failure mode, not the remedy.

## Decision

The engine never hands a model, or the record, less than it has without
saying so loudly. Concretely:

1. **Tool output is passed whole** — into `ToolCallCompleted.result` and into
   the model's context. Above `MAX_TOOL_OUTPUT` (1 MiB) the call **fails**
   with "<what> is N MB … narrow it (grep/head/tail/jq) or write it to a
   file". No middle ground. An over-large assembled context is already a loud
   failure via ADR-0016 `hard_cap_chars`; ADR-0016 placeholder masking of
   *older* observations stays (the model has already read them).
2. **`ToolCallRequested.arguments` is the whole argument JSON.**
3. **`max_tokens` is configurable** — `max_tokens:` in `agents/*.yml`,
   falling back to `defaults.max_tokens` (default 4096). Providers report
   `finish_reason` (Anthropic `stop_reason` normalised: `max_tokens`→`length`,
   `end_turn`→`stop`, `tool_use`→`tool_calls`); it is recorded on
   `LlmCallCompleted`. `length` **fails the step**, with a message naming the
   cap and where to raise it; the partial answer stays in the record.
4. **New `EventKind::LlmCallFailed { iteration, error, elapsed_ms }`**, emitted
   in the agent loop whenever the provider call errors.
5. **`zymi events --stream <run>`** merges `<run>:step:*` sub-streams in time
   order, tagging their events with the step id.

## Consequences

- A report is either complete or the run fails, and the log says why.
- The journal holds what actually happened: full outputs, full arguments,
  failed calls with their cause and duration.
- **Minus: bigger journals and contexts.** Tool results up to 1 MiB are
  stored and sent whole; a chatty tool now costs tokens instead of being
  silently clipped. That is the intended trade — fix the tool, not the cut.
- **Minus: runs that used to "succeed" now fail** — an over-1 MiB output, or
  an answer that hits `max_tokens`. Both were wrong results before.
- **Minus:** `LlmCallFailed` is a new event kind and `LlmCallCompleted` gains an
  optional field; older zymi versions reading a newer store skip/ignore them
  per serde defaults, but a consumer that matches exhaustively on kinds must
  add the arm.
- Agent `model:` is still parsed and unused (ADR-0044 note); per-agent
  provider/model selection remains open.
