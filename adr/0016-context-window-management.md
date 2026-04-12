# Context window management and compaction

Date: 2026-04-12

## Context

zymi-core's agent loop is in `src/handlers/run_pipeline.rs::run_agent_step` (around line 230 onward). The shape of the loop is straightforward:

1. Build a `messages: Vec<Message>` with the system prompt (line 259) and the user task (line 266).
2. Iterate up to `max_iterations` times.
3. On each iteration: build a `ChatRequest { messages: messages.clone(), ... }` (line 328) and send it to the provider.
4. If the response has tool calls, push the assistant message and each tool result onto `messages`, then loop.
5. If the response has no tool calls, return.

This is the simplest thing that could work. It is also exactly wrong for any nontrivial workflow, for three independent reasons.

**1. The message list grows without bound.** There is no truncation, no compaction, no summarization, no eviction. A 30-step coding agent that performs 50 tool calls accumulates ~50 assistant messages, ~50 tool result messages, and the original system + user pair, all of which are sent to the LLM on every single iteration. The token cost grows quadratically in the number of iterations (each iteration sends *all prior turns*, so iteration N costs O(N) tokens, and the total cost across N iterations is O(N²)). For an agent doing real work this is the dominant cost line item before the agent has even finished its task.

**2. The engine knows the problem exists but does nothing about it.** `LlmCallStarted` (lines 304–326) computes `approx_context_chars` by summing the length of every message and emits it on the bus. The number is excellent observability — it appears in `zymi events` output and in the Langfuse projection — but it is *only* observability. The agent loop never reads it back. There is no threshold, no "if context is too big, do X", no eviction trigger. The engine can *tell you* the context is exploding while the context is exploding, and that is all it can do.

**3. There is no event for "the engine compressed history".** `EventKind` (`src/events/mod.rs:72-170`) has 19 variants covering message receipt, agent processing, LLM calls, tool calls, intentions, ESAA, workflow, pipeline, and approvals. None of them represent context compaction. This is not an oversight — there has been no compaction mechanism, so there has been no event for one. But it means that any future compaction work is also a new event type and a new projection, not just a code change in `run_agent_step`.

There is a related, equally pressing problem in the same area. `src/engine/tools.rs` mutates an `Arc<Mutex<HashMap<String, String>>>` directly when the agent calls `write_memory` / `read_memory`. The hashmap is **not** part of the event log — it is a side channel. This is already an open goal in drift ("Event-sourced state for workflow memory"), and it is tightly coupled to context management because both are about "what state does the agent see on the next turn, and where does that state come from". Trying to fix one without the other produces a half-fixed system: a `ContextBuilder` that reads memory from a hashmap is not a context builder, it is a wrapper around the same broken thing.

The two also share a structural choice: today, runtime state lives in *whatever data structure was easiest to write at the time* (`Vec<Message>` for context, `HashMap<String, String>` for memory). The fix is the same in both cases: state is a projection over the event log, not a mutable container next to it. This ADR therefore covers context management *and* the workflow-memory lift, because separating them would force us to design the same projection layer twice.

zymi-core's invariant is that the event log is the source of truth. Today the agent loop quietly violates that invariant for both conversation context and workflow memory. This ADR fixes both.

## Decision

Introduce a **`ContextBuilder`** as a first-class runtime component that materialises the agent's working context as a *view* over the event store, not as a mutable buffer accumulated inside `run_agent_step`. The builder owns three responsibilities — prefix selection, tail selection, and compaction — and exposes a single method that returns the `Vec<Message>` for the next LLM call. Alongside the builder, lift workflow memory into the event log so that it, too, is a projection rather than a side hashmap.

### 1. New event types

Three new variants on `EventKind` in `src/events/mod.rs`:

```rust
MemoryWritten {
    key: String,
    value: String,
    /// Set when the write is overwriting a previous value.
    /// None on first write for this key.
    previous_value_seq: Option<u64>,
},
MemoryDeleted {
    key: String,
    previous_value_seq: u64,
},
ContextCompacted {
    /// The inclusive event sequence range that this summary replaces
    /// in the working context. Inclusive on both ends.
    replaces_seq_range: (u64, u64),
    /// The compacted summary text. Exact format is the engine's choice;
    /// the test contract is "tail + summary + prefix is shorter than
    /// the original event range and preserves tool_use/tool_result
    /// pairs and final answers verbatim".
    summary: String,
    /// Tokens (or chars; see §6) saved by this compaction, for telemetry.
    /// Negative is allowed and indicates a bad compaction the next
    /// trigger should be more aggressive about.
    bytes_saved: i64,
},
```

These are **state events**: replay must consume them to produce the correct projections. They are not "history" in the sense that `ShellSessionStarted` (ADR-0015) is — replay does rebuild memory state and does honour compaction.

### 2. `MemoryProjection`

A new file `src/esaa/projections/memory.rs` (or extension of the existing `src/esaa/projections.rs`):

```rust
pub struct MemoryProjection {
    state: HashMap<String, MemoryEntry>,
}

pub struct MemoryEntry {
    pub value: String,
    pub written_at_seq: u64,
}

impl MemoryProjection {
    pub fn apply(&mut self, event: &Event) {
        match &event.kind {
            EventKind::MemoryWritten { key, value, .. } => {
                self.state.insert(key.clone(), MemoryEntry {
                    value: value.clone(),
                    written_at_seq: event.sequence,
                });
            }
            EventKind::MemoryDeleted { key, .. } => {
                self.state.remove(key);
            }
            _ => {}
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> { ... }
    pub fn snapshot(&self) -> HashMap<String, String> { ... }
}
```

The projection is the *only* readable surface for workflow memory. `read_memory` and `write_memory` tools in `src/engine/tools.rs` change shape:

- `write_memory(key, value)` no longer mutates a hashmap directly. It emits a `MemoryWritten` event onto the bus and returns `Ok("stored")`. The next iteration's `ContextBuilder` will reflect the write through the projection.
- `read_memory(key)` reads from a `MemoryProjection` that the runtime keeps up-to-date by subscribing to the bus.

The `Arc<Mutex<HashMap<...>>>` in the engine is deleted. Tests that previously asserted on the hashmap directly now assert on the projection.

### 3. `ContextBuilder`

A new module `src/runtime/context_builder.rs`:

```rust
pub struct ContextBuilder<'a> {
    store: &'a dyn EventStore,
    memory: &'a MemoryProjection,
    stream_id: &'a str,
    config: ContextConfig,
    agent_identity: AgentIdentity<'a>,
}

pub struct ContextConfig {
    /// Soft cap on context size. When the materialised context exceeds
    /// this, compaction is triggered. Measured in approx_context_chars
    /// to start with (see §6 on tokens vs chars).
    pub soft_cap_chars: usize,
    /// Hard cap. If we exceed this even after compaction, the builder
    /// returns an error and the agent loop bails out instead of
    /// silently sending an over-budget request.
    pub hard_cap_chars: usize,
    /// Number of recent events to always keep in the tail, even if
    /// the soft cap suggests compacting them.
    pub min_tail_events: usize,
    /// Event kinds that the tail considers "significant" and includes;
    /// others are filtered out as noise.
    pub significant_kinds: HashSet<EventKindTag>,
}

pub struct AgentIdentity<'a> {
    pub system_prompt: &'a str,
    pub agent_name: &'a str,
    pub tool_definitions: &'a [ToolDefinition],
}

impl<'a> ContextBuilder<'a> {
    pub async fn build(&self) -> Result<Vec<Message>, ContextError> { ... }
}
```

`build()` produces the message list in three layers:

**Layer A — stable prefix.** Built from `AgentIdentity`. Deterministic, never compacted, never sliced. Always exactly:

```
Message::System(system_prompt)
Message::System(format!("agent: {agent_name}"))
```

(The tool definitions go on `ChatRequest.tools`, not in `messages`, so they are not part of this layer's character budget.)

**Layer B — compacted summaries.** Read all `ContextCompacted` events for this `stream_id` from the store, in order. Each one becomes a `Message::System(format!("[summary {start}..{end}]\n{summary}"))`. Multiple `ContextCompacted` events stack newest-last; if a later compaction subsumes an earlier one (its `replaces_seq_range` overlaps), the older one is dropped.

**Layer C — tail.** Read events for this `stream_id` from `max(last_compacted_seq, store_start)` to `store_end`. Filter to `significant_kinds`. Group `ToolCallRequested` + `ToolCallCompleted` pairs together; the builder's hardest invariant is that **a `tool_use` and its matching `tool_result` are always adjacent in the output and always either both present or both absent**. Anthropic's API rejects unmatched pairs; OpenAI tolerates them but produces incoherent responses. The builder enforces this in code, not in convention.

Significant kinds for the tail (default):
- `LlmCallCompleted` — becomes a `Message::Assistant { content, tool_calls }`.
- `ToolCallRequested` + `ToolCallCompleted` — paired, become `Message::Assistant { tool_calls: [..] }` followed by `Message::ToolResult { call_id, content, ... }`.
- `MemoryWritten` — surfaced as a `Message::System(format!("[memory:{key} = {preview}]"))` so the agent can see what was just written. The full value is also available via `read_memory`.
- `ShellSessionStarted` / `ShellSessionClosed` (from ADR-0015) — surfaced as `Message::System` so the agent knows whether its previous-turn shell state survived.
- The original `Message::User(task)` — sourced from a synthetic event the runtime emits at the start of `run_agent_step`. (Today the user task is built directly into the `Vec<Message>` and never lives in the event log; this changes — the task becomes a `EventKind::WorkflowNodeStarted` field already, but the builder needs the full text, so the runtime emits a `UserMessageReceived` for the task.)

Excluded from the tail (noise):
- `IntentionEmitted`, `IntentionEvaluated` — internal ESAA bookkeeping.
- `ApprovalRequested`, `ApprovalDecided` — observable to operators, not to the model. (Open question, see §7.)
- `LlmCallStarted` — only the *completion* matters for context.
- `WorkflowNodeStarted`, `WorkflowNodeCompleted` — workflow-level structure, not turn-level signal.

### 4. Compaction trigger and policy

`build()` first materialises the full context. If `total_chars > soft_cap_chars`, it triggers compaction *before* returning:

1. Identify the oldest contiguous slice of the tail that, if replaced by a summary, would bring the total under `soft_cap_chars * 0.7` (target a 30% margin to avoid re-triggering on the very next call). The slice always preserves `min_tail_events` recent events.
2. Send the slice as a system + user pair to the *same provider the agent is using*, with a fixed compaction prompt:

   > You are compressing the working context of an autonomous agent. Below is a sequence of events from its history. Produce a single concise summary that preserves: (a) any final answers the agent committed to, (b) the names and outcomes of tool calls, (c) any user-facing decisions, (d) any errors encountered. Do NOT preserve: chain-of-thought, intermediate reasoning, redundant tool results.

3. The compaction call's response becomes the `summary` field.
4. Emit `ContextCompacted { replaces_seq_range, summary, bytes_saved }` onto the bus. The next `build()` call will pick it up via Layer B.
5. Re-materialise the context. If still over `hard_cap_chars`, return `ContextError::HardCapExceeded` and let `run_agent_step` decide (today: bail out; future: trigger more aggressive compaction).

The compaction call uses a fresh `correlation_id` and its own `LlmCallStarted` / `LlmCallCompleted` so it appears in the event log as a normal LLM call attributable to "compaction", not as a hidden cost. Telemetry stays honest.

### 5. `run_agent_step` migration

The current agent loop's `let mut messages: Vec<Message> = Vec::new()` (`src/handlers/run_pipeline.rs:253`) goes away. In its place:

```rust
let context_builder = ContextBuilder::new(
    runtime.event_store(),
    runtime.memory_projection(),
    stream_id,
    runtime.context_config(),
    AgentIdentity {
        system_prompt: &system_prompt,
        agent_name: &agent.name,
        tool_definitions: &tool_defs,
    },
);

loop {
    let messages = context_builder.build().await?;
    let request = ChatRequest { messages, tools: tool_defs.clone(), ... };
    let response = provider.chat_completion(&request).await?;
    // emit LlmCallCompleted, ToolCallRequested/Completed events as today;
    // the next iteration's context_builder.build() reads them back.
    ...
}
```

The crucial change: there is no `messages.push(...)` anywhere in `run_agent_step`. Everything that previously went into the in-memory `Vec` now goes onto the event log via the existing `emit_event` calls, and the next iteration's builder picks it up. **The event log is the source of truth, and the builder is the view.**

### 6. Tokens vs characters

`approx_context_chars` is what `LlmCallStarted` measures today. It is wrong for budgeting (a 4000-character JSON tool result is not 4000 tokens), but it is fast, model-agnostic, and stable across providers. For v1 the ADR keeps `chars` as the unit and sets `soft_cap_chars` / `hard_cap_chars` to chars-based defaults derived empirically (~3.5 chars/token for English, more for code).

A future iteration may swap in a real tokeniser per provider (`tiktoken` for OpenAI, the Anthropic tokeniser for Claude). The builder's API takes a generic `ContextSize` type that today is `usize` chars but is intentionally newtyped so the swap is local.

### 7. Open questions deliberately left for the slice review

- Should `ApprovalRequested` / `ApprovalDecided` appear in the model context? Argument for: gives the model self-awareness about its own gating, useful for "the user denied my last request, try a different approach". Argument against: noise the model does not need to make decisions. **Tentative answer:** include them, behind a config flag, default off; revisit after slice 2 ships and we have real workflows to check against.
- Should the compaction LLM call use a *different* (cheaper) model than the agent? Argument for: compaction is a well-defined task and a small model is plenty. Argument against: introducing a second model in `agents/*.yml` is a much bigger config change than this ADR wants to land. **Tentative answer:** v1 uses the agent's own provider; v2 may add `compaction_provider:` as a sibling field on the agent.
- Where does `min_tail_events` come from when the user has a 30-step pipeline whose every step is significant? Compaction will eventually be forced to compress meaningful work. **Tentative answer:** `min_tail_events` is a config knob, not a guarantee; if the hard cap is hit even with `min_tail_events` items in the tail, the loop bails out with a clear error. Operators tune the cap to their workload.

## Alternatives Considered

- **Do nothing; trust the LLM provider's auto-truncation.** Anthropic and OpenAI both fail loud when context exceeds their max, with no automatic truncation. "Doing nothing" means the agent hits a wall mid-run with a 400 error. Rejected: that is the bug, not the fix.

- **Sliding window only — drop the oldest N events, no summarization.** Simpler than compaction. Rejected: the oldest events in an agent run are usually the original task, the system prompt's elaborations, and the early tool calls that established context. Dropping them produces an agent that has forgotten what it was asked to do. Summarisation preserves the *information* while shedding the *tokens*.

- **Keep `Vec<Message>` in `run_agent_step`, add only a truncate-or-summarize hook.** The smallest possible diff. Rejected for two reasons: (a) it leaves workflow memory in its hashmap, perpetuating the "state lives in two places, one of which is event-sourced and one of which is not" problem, and (b) the in-memory `Vec` and the event log will inevitably drift, making replay produce a different conversation than the one that actually ran.

- **Build the context in the provider layer** (`src/llm/openai.rs`, `src/llm/anthropic.rs`). Each provider knows its own token budget and tokenizer. Rejected: this puts compaction into N adapters instead of one shared place, and the *event filtering* logic ("which events count as significant") is an engine concern, not a provider concern. Providers already know how to reject over-budget requests; they should not also be in charge of deciding which messages to keep.

- **Lift workflow memory in a separate ADR; do context management standalone.** Rejected: see the Context section. The two share the projection layer. Splitting them forces the projection to be designed twice, and forces `ContextBuilder` v1 to wrap the legacy hashmap, only to be rewritten when memory is finally lifted.

- **Use a vector store for "long-term memory" instead of summarization.** Pull the most relevant past events by embedding similarity. Rejected for v1: introduces an embedding dependency, an embedding model, a vector index, and a similarity threshold — none of which exist in zymi-core today. The right time to add it is after `ContextBuilder` exists and we have a clean place to plug it in. The builder's API leaves room for a future `MemoryRetriever` that injects `Message::System("[recall: ...]")` into the prefix.

## Consequences

- **Pro.** The O(N²) token explosion in long workflows goes away. A 50-iteration agent run sends a bounded amount of context per iteration, not a quadratically-growing one.
- **Pro.** Workflow memory is finally event-sourced. Replay reconstructs the agent's full state from the log alone, with no side channels. The "Recovery and projections" goal in drift becomes tractable in the same pass.
- **Pro.** `ContextCompacted` is a first-class event, so compaction is auditable. Operators can see exactly when and why the engine compressed history, and how many bytes it saved.
- **Pro.** The compaction LLM call appears in the event log as a normal LLM call, with its own correlation_id. Token spend stays attributable; compaction does not become a hidden cost line.
- **Pro.** `ContextBuilder` is the natural integration point for future features: vector recall, multi-agent shared context, streaming context updates from `StreamRegistry`. Each becomes a method on the builder, not a patch to `run_agent_step`.
- **Pro.** Removes the `Arc<Mutex<HashMap<...>>>` side channel from `engine/tools.rs`. One less place where state lives.
- **Con.** Significant rewrite of `run_agent_step`. Easy to break the `tool_use` / `tool_result` adjacency invariant in subtle ways. Mitigation: a builder-level test that takes a sequence of synthetic events with tool calls and verifies the output never splits a pair, plus a real end-to-end test against a recorded agent run.
- **Con.** Compaction is itself an LLM call. It is fast and small relative to the agent's own calls, but it is not free. Workflows with very few iterations now pay a cost they did not pay before. Mitigation: compaction only triggers above `soft_cap_chars`; short workflows never hit the trigger.
- **Con.** Compaction quality is a function of the prompt and the model. A bad summary loses information the agent later needs. Mitigation: `bytes_saved: i64` allows the trigger to detect and react to bad compactions; future iterations can A/B compaction prompts against a test corpus.
- **Con.** `ContextBuilder::build()` runs on every iteration and walks the event store. For long sessions this is more work than `messages.clone()`. Mitigation: the store is local SQLite, walks are fast, and the builder caches the materialised context inside one iteration. Profile before optimising.
- **Con.** The change to memory tools is observable to existing pipelines: a `write_memory` call followed immediately by a `read_memory` in the *same* iteration may not see the write, because the write goes through the bus and is only picked up by the projection asynchronously. Mitigation: the projection is updated synchronously by the same task that emits the event before the next iteration starts. This is testable and the test exists in slice 1.
- **Con.** `ContextConfig` is a new knob surface. Operators have to tune `soft_cap_chars`, `hard_cap_chars`, `min_tail_events`. Defaults are picked from empirical workloads, but bad defaults will hurt people. Mitigation: ship defaults that are conservative (compact early), document the knobs in `project.yml` schema, and surface "context compacted N times this run" in `zymi events` summary.

## Implementation slices

Five slices, each independently shippable, ordered so each one leaves the codebase in a compiling, passing-tests state.

1. **Slice 1 — Memory lift.** Add `MemoryWritten` and `MemoryDeleted` to `EventKind`. Add `MemoryProjection`. Rewrite `read_memory` / `write_memory` in `src/engine/tools.rs` to go through the bus and the projection. Delete `Arc<Mutex<HashMap<...>>>`. Add the synchronous-update test. **No `ContextBuilder` yet.** The agent loop still uses `Vec<Message>`. This slice only touches memory, and is enough on its own to close the existing "Event-sourced state for workflow memory" goal in drift.

2. **Slice 2 — `ContextBuilder` skeleton, tail-only, no compaction.** Add `src/runtime/context_builder.rs` with `build()` that produces prefix + tail (no Layer B). Migrate `run_agent_step` to use the builder. No `ContextCompacted` event yet, no compaction trigger — the builder simply replays the full event tail every iteration. Behaviour identical to today *except* messages now come from the event log, not the in-memory `Vec`. This is the "prove the view-over-events approach works without losing tool-use/tool-result correctness" slice. The hardest tests live here.

3. **Slice 3 — `ContextCompacted` + compaction trigger.** Add the third event variant. Implement Layer B in the builder. Implement the soft-cap trigger and the compaction prompt. Emit compaction LLM calls as normal events with their own correlation_id. Add `bytes_saved` telemetry. The agent loop now has bounded context. This is the "actually fix the O(N²) bug" slice.

4. **Slice 4 — Hard cap + bailout.** Add `hard_cap_chars` enforcement and `ContextError::HardCapExceeded`. Add a `cli/events.rs` renderer for `ContextCompacted`. Add operator-facing tuning docs. This slice is mostly hardening and DX.

5. **Slice 5 — Configuration in `project.yml`.** Surface `ContextConfig` as `runtime.context:` in `project.yml`. schemars-derive on the new config struct. Defaults baked into the engine, knobs available to operators who outgrow them. This slice may land before slice 4 if operator demand for tuning is high.

Each slice updates this ADR with a "Slice N — what landed" section in the same way ADR-0013 and ADR-0014 do. Slice 1 is independently valuable and closes the "Event-sourced state for workflow memory" drift goal even if the rest of the ADR slips.

## Relationship to other ADRs

- **ADR-0011 (Pipeline execution engine).** This ADR is the first significant rewrite of `run_agent_step` since ADR-0011. Behaviour is preserved (same iteration count, same approval flow, same tool dispatch) but the data flow changes from "in-memory Vec mutated by the loop" to "view over the event store rebuilt each iteration".

- **ADR-0013 (Target runtime architecture).** `ContextBuilder` is a new component owned by `Runtime`, accessed via a new `runtime.context_builder()` method. `MemoryProjection` is a new long-lived state owned by `Runtime`, kept up-to-date by a bus subscription started by `RuntimeBuilder::build`. Both fit into the existing builder pattern without changes to `RuntimeBuilder`'s public API.

- **ADR-0015 (Persistent shell session).** Mutually reinforcing. `ContextBuilder` surfaces `ShellSessionStarted` / `ShellSessionClosed` events to the model so the agent knows whether its previous-turn shell state survived a compaction or a session reap. ADR-0015 explicitly relies on this — its replay semantics ("resumed runs do not preserve shell state") are only safe if the model can *see* that the shell was reset.

- **Open goal: Recovery and projections.** Slice 1 of this ADR closes the prerequisite. Once memory is event-sourced, recovery becomes "replay events to rebuild `MemoryProjection` and any future projections". `ContextCompacted` from slice 3 then becomes a snapshot point, so recovery does not need to replay all the way from `seq=0` — it can replay from the latest `ContextCompacted` and use the summary as the prefix.

- **Open goal: Streaming runtime contract.** The `ContextBuilder` is the natural attachment point for streaming. Today `StreamEvent` is a side path; after this ADR, streaming a `Message::Assistant` is "the builder publishes the in-progress message to a stream and the agent loop reads it back on the next build". Streaming becomes a property of the builder, not a parallel mechanism.
