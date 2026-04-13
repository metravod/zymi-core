# Context window management and compaction

Date: 2026-04-12
Updated: 2026-04-12 — restructured around observation masking as primary mechanism (research-backed), LLM summarization demoted to last-resort fallback. Added §1a event enrichment for context reconstruction (full payloads in ToolCallCompleted/LlmCallCompleted instead of truncated previews). Placeholder format enriched with argument metadata for trajectory awareness.

## Context

zymi-core's agent loop is in `src/handlers/run_pipeline.rs::run_agent_step` (line 238). The shape of the loop is straightforward:

1. Build a `messages: Vec<Message>` with the system prompt (line 264) and the user task (line 271).
2. Iterate up to `max_iterations` times.
3. On each iteration: build a `ChatRequest { messages: messages.clone(), ... }` (line 333) and send it to the provider.
4. If the response has tool calls, push the assistant message and each tool result onto `messages`, then loop.
5. If the response has no tool calls, return.

This is the simplest thing that could work. It is also exactly wrong for any nontrivial workflow, for three independent reasons.

**1. The message list grows without bound.** There is no truncation, no compaction, no summarization, no eviction. A 30-step coding agent that performs 50 tool calls accumulates ~50 assistant messages, ~50 tool result messages, and the original system + user pair, all of which are sent to the LLM on every single iteration. The token cost grows quadratically in the number of iterations (each iteration sends *all prior turns*, so iteration N costs O(N) tokens, and the total cost across N iterations is O(N²)). For an agent doing real work this is the dominant cost line item before the agent has even finished its task.

**2. The engine knows the problem exists but does nothing about it.** `LlmCallStarted` (lines 305–331) computes `approx_context_chars` by summing the length of every message and emits it on the bus. The number is excellent observability — it appears in `zymi events` output and in the Langfuse projection — but it is *only* observability. The agent loop never reads it back. There is no threshold, no "if context is too big, do X", no eviction trigger. The engine can *tell you* the context is exploding while the context is exploding, and that is all it can do.

**3. There is no event for "the engine compressed history".** `EventKind` (`src/events/mod.rs:72`) has 21 variants covering message receipt, agent processing, LLM calls, tool calls, intentions, ESAA, workflow, pipeline, shell sessions, and approvals. None of them represent context compaction. This is not an oversight — there has been no compaction mechanism, so there has been no event for one. But it means that any future compaction work is also a new event type and a new projection, not just a code change in `run_agent_step`.

There is a related, equally pressing problem in the same area. `src/engine/tools.rs` mutates an `Arc<Mutex<HashMap<String, String>>>` directly when the agent calls `write_memory`. The hashmap is **not** part of the event log — it is a side channel. This is already an open goal in drift ("Event-sourced state for workflow memory"), and it is tightly coupled to context management because both are about "what state does the agent see on the next turn, and where does that state come from". Trying to fix one without the other produces a half-fixed system: a `ContextBuilder` that reads memory from a hashmap is not a context builder, it is a wrapper around the same broken thing.

The two also share a structural choice: today, runtime state lives in *whatever data structure was easiest to write at the time* (`Vec<Message>` for context, `HashMap<String, String>` for memory). The fix is the same in both cases: state is a projection over the event log, not a mutable container next to it. This ADR therefore covers context management *and* the workflow-memory lift, because separating them would force us to design the same projection layer twice.

zymi-core's invariant is that the event log is the source of truth. Today the agent loop quietly violates that invariant for both conversation context and workflow memory. This ADR fixes both.

### Research grounding

The design is informed by two external findings that change the priority order relative to our original plan:

**JetBrains research (December 2025) + arXiv 2508.21433 (August 2025):** Simple observation masking — keeping the last N tool results in full and replacing older ones with one-line placeholders — delivers ~2x cost reduction with no quality loss. Critically, it outperforms LLM-based summarization on cost because summarized agents tend to take more steps (they "trust" their compressed history and explore more), so the per-iteration savings are eaten by longer trajectories. The optimal window is approximately 10 recent turns in full.

**ContextEvolve (arXiv 2602.02597, February 2026):** Layer-based decomposition — where each concern gets its own context slice with independent retention policy — achieves -29% tokens at +33.3% quality on ADRS benchmark. The key insight is that different types of context (identity, working memory, tool observations) have different lifetimes and different information density. Treating them uniformly wastes budget.

These findings restructure the implementation: **observation masking is the primary mechanism; LLM summarization is the graduated fallback for genuinely long-horizon workflows that exceed masking capacity.**

## Decision

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
    /// Bytes saved by this compaction, for telemetry.
    /// Negative is allowed and indicates a bad compaction the next
    /// trigger should be more aggressive about.
    bytes_saved: i64,
},
```

`MemoryWritten` and `MemoryDeleted` are **state events**: replay must consume them to produce the correct projections. `ContextCompacted` is also a state event — replay honours compaction to reconstruct the correct working context.

### 1a. Event enrichment for context reconstruction

Today's events store truncated previews optimised for display and telemetry, not for reconstruction:

- `ToolCallCompleted.result_preview` — truncated to 200 chars (`truncate(&tool_result, 200)` at `run_pipeline.rs:447`). The full tool output (up to 8KB for `read_file`) goes only into the in-memory `Vec<Message>` and is lost after the run.
- `LlmCallCompleted` — stores `content_preview: Option<String>` (100 chars) and `has_tool_calls: bool`. The full `Message::Assistant { content, tool_calls }` with actual tool call names, IDs, and arguments is never persisted.

The `ContextBuilder` (§4) reads from the event store to reconstruct the agent's conversation. It cannot do this from previews — a 200-char truncation of an 8000-char file read loses 97.5% of the content. **If the event log is the source of truth, it must contain the truth, not a preview of it.**

**Change to `ToolCallCompleted`:**

```rust
ToolCallCompleted {
    call_id: String,
    /// Full tool output, untruncated. The ContextBuilder reads this
    /// to reconstruct the conversation; masking (§3) replaces it
    /// with a placeholder in older turns.
    result: String,
    /// First 200 chars for display (CLI, Langfuse). Derived from result.
    result_preview: String,
    is_error: bool,
    duration_ms: u64,
},
```

**Change to `LlmCallCompleted`:**

```rust
LlmCallCompleted {
    /// Full assistant message including content and tool_calls.
    /// The ContextBuilder reads this to reconstruct the conversation.
    response_message: Message,
    usage: Option<TokenUsage>,
    /// First 100 chars for display. Derived from response_message.content.
    content_preview: Option<String>,
},
```

`has_tool_calls: bool` is removed — derivable from `response_message` tool_calls.

**Backward compatibility:** Both changes add fields. Old events stored before the enrichment lack `result` / `response_message`. Serde deserialisation handles this via `#[serde(default)]` on the new fields (`result` defaults to empty string, `response_message` defaults to `None`-wrapped optional). The ContextBuilder falls back to `result_preview` / `content_preview` when the full field is absent, appending a `[partial: pre-enrichment event]` suffix so the model knows the data is incomplete. This is a graceful degradation, not a migration — old event stores continue to work, just with lower-fidelity reconstruction for pre-enrichment events.

**Storage impact:** Full tool results in SQLite. A `read_file` call stores ~8KB instead of ~200 bytes. For a 50-iteration run with 50 tool calls averaging 2KB each, total storage is ~100KB — trivial for SQLite. `zymi events` default view continues to display previews; `zymi events --verbose` shows full payloads. If storage becomes a concern in long-running serve sessions, event archival (future work) addresses it at the store level, not by truncating the source of truth.

**Traceability benefit:** The event log becomes a complete, auditable record of every tool output the agent saw and every response the agent produced. This directly enables: (a) post-hoc debugging of agent decisions ("why did it do X after reading Y?"), (b) faithful replay that reproduces the exact conversation, (c) regression testing by diffing event logs across engine versions.

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

- `write_memory(key, value)` no longer mutates a hashmap directly. It emits a `MemoryWritten` event onto the bus and returns `Ok("stored")`. The next iteration's context reflects the write through the projection.
- `read_memory(key)` reads from a `MemoryProjection` that the runtime keeps up-to-date by subscribing to the bus.

The `Arc<Mutex<HashMap<...>>>` in the engine is deleted. Tests that previously asserted on the hashmap directly now assert on the projection.

### 3. Observation masking policy

This is the primary context management mechanism. It operates on the `messages: Vec<Message>` before each LLM call.

**Algorithm:**

```rust
fn mask_observations(messages: &[Message], observation_window: usize) -> Vec<Message> {
    // 1. Identify "turns": each turn is an Assistant message + its ToolResult messages.
    //    System and User messages at the start are prefix — never masked.
    //
    // 2. Count turns from the end. The last `observation_window` turns are kept in full.
    //
    // 3. Older turns are replaced:
    //    - The Assistant message is kept (its content + tool_call names/ids preserved),
    //      but any long `content` text is truncated to a preview.
    //    - Each ToolResult is replaced with a compact placeholder:
    //      ToolResult { tool_call_id, content: "[masked: {tool_name} → {status}, {n} chars]" }
    //
    // 4. Invariant: tool_use/tool_result pairs are NEVER split. If the Assistant message
    //    with tool_calls is present, all its ToolResults are present (masked or full).
    //    Anthropic's API rejects unmatched pairs; OpenAI produces incoherent responses.
}
```

**Placeholder format:** `[masked: {tool_name}({args_preview}) → {status}, {original_chars} chars]`

The `args_preview` is the most identifying argument, truncated to 60 chars: file path for `read_file`/`write_file`, command for `execute_shell_command`, key for `write_memory`, query for `web_search`. This is critical — the model needs to know not just "I ran a shell command" but "I ran `cargo test`", not just "I read a file" but "I read `src/main.rs`". Without argument context, the agent re-executes tools it has already run.

Examples:
- `[masked: execute_shell_command("cargo test --lib") → exit 0, 2847 chars]`
- `[masked: read_file("src/handlers/run_pipeline.rs") → ok, 8000 chars]`
- `[masked: write_file("src/main.rs") → ok, 142 chars]`
- `[masked: write_memory("research_findings") → ok, 34 chars]`
- `[masked: execute_shell_command("curl -s https://api.example.com/v1/lo…") → error, 1203 chars]`
- `[masked: web_search("rust async trait object safety") → ok, 3200 chars]`

The placeholder preserves: which tool ran, what it was called with, whether it succeeded, how large the original output was. Cost: ~20 tokens per placeholder vs ~500-2000 tokens for a full tool result — a 25-100x reduction per masked turn while preserving the agent's sense of trajectory.

The argument preview is extracted from the preceding `Message::Assistant`'s `tool_calls` (in slice 2, operating on `Vec<Message>`) or from `ToolCallRequested.arguments` (in slice 3, operating on events). The masking function parses the JSON arguments to find the primary argument by tool name:
- `execute_shell_command` → `args["command"]`
- `read_file` / `write_file` → `args["path"]`
- `write_memory` → `args["key"]`
- `web_search` → `args["query"]`
- `web_scrape` → `args["url"]`
- fallback → first string-valued argument

**Why not just drop old turns entirely?** The model needs to know what it already tried. Without even a placeholder, it re-reads the same files and re-runs the same commands. The placeholder is ~15 tokens vs ~500-2000 tokens for a full tool result — a 30-100x reduction per masked turn while preserving the agent's sense of trajectory.

**Default observation window:** 10 turns. Empirical optimum from the JetBrains study. Configurable via `runtime.context.observation_window` (slice 5).

### 4. Layer-based `ContextBuilder`

A new module `src/runtime/context_builder.rs`. The builder materialises the agent's working context as a *view* over the event store with three layers, each with independent retention:

```rust
pub struct ContextBuilder<'a> {
    store: &'a dyn EventStore,
    memory: &'a MemoryProjection,
    stream_id: &'a str,
    config: ContextConfig,
    agent_identity: AgentIdentity<'a>,
}

pub struct ContextConfig {
    /// Number of recent turns to keep in full (observation window).
    /// Turns older than this get their tool results masked.
    pub observation_window: usize,
    /// Soft cap on total context size (chars). When exceeded after masking,
    /// hybrid compaction (LLM summarization) is triggered as a fallback.
    pub soft_cap_chars: usize,
    /// Hard cap. If exceeded even after compaction, the builder returns
    /// ContextError::HardCapExceeded and the agent loop bails out.
    pub hard_cap_chars: usize,
    /// Minimum recent turns to preserve even under compaction pressure.
    pub min_tail_turns: usize,
}

pub struct AgentIdentity<'a> {
    pub system_prompt: &'a str,
    pub agent_name: &'a str,
    pub tool_definitions: &'a [ToolDefinition],
}

impl<'a> ContextBuilder<'a> {
    pub fn build(&self) -> Result<Vec<Message>, ContextError> { ... }
}
```

`build()` produces the message list in three layers:

**Layer A — stable prefix.** Built from `AgentIdentity`. Deterministic, never masked, never compacted. Always exactly:

```
Message::System(system_prompt)
Message::System(format!("agent: {agent_name}"))
```

(Tool definitions go on `ChatRequest.tools`, not in `messages`, so they are not part of this layer's budget.)

**Layer B — memory snapshot.** Current state from `MemoryProjection`, formatted as a compact system message:

```
Message::System("[memory]\nkey1: value1_preview\nkey2: value2_preview\n...")
```

Only included when the projection is non-empty. Values are previewed (first 200 chars) — the full value is available to the agent via the `read_memory` tool. This layer is rebuilt from the projection on every `build()` call, so it always reflects the latest state. Never compacted — it is already a projection.

**Layer C — observation tail.** The conversation turns from the event store, with the observation masking policy applied:

1. Read events for this `stream_id` from the store.
2. Convert significant events to `Message` variants (see mapping below).
3. Apply `mask_observations()` with `observation_window` from config.
4. If total context chars > `soft_cap_chars` after masking: trigger hybrid compaction (§5).
5. If total context chars > `hard_cap_chars` after compaction: return `ContextError::HardCapExceeded`.

**Event → Message mapping (significant kinds for the tail):**
- `LlmCallCompleted` → uses `response_message` (§1a) directly as the `Message::Assistant { content, tool_calls }`. For pre-enrichment events without `response_message`, falls back to `Message::Assistant { content: content_preview, tool_calls: vec![] }`.
- `ToolCallCompleted` → `Message::ToolResult { tool_call_id: call_id, content: result }`. Uses full `result` field (§1a). For pre-enrichment events, falls back to `result_preview`.
- `UserMessageReceived` → `Message::User(content)`.
- `ShellSessionStarted` / `ShellSessionClosed` (ADR-0015) → `Message::System(...)` so the agent knows whether its previous-turn shell state survived.

**Excluded from the tail (noise):**
- `IntentionEmitted`, `IntentionEvaluated` — internal ESAA bookkeeping.
- `ApprovalRequested`, `ApprovalDecided` — observable to operators, not to the model. (Open question, see §8.)
- `LlmCallStarted` — only the *completion* matters for context.
- `WorkflowNodeStarted`, `WorkflowNodeCompleted` — workflow-level structure, not turn-level signal.

**Tool-use/tool-result adjacency invariant:** The builder's hardest contract is that a `tool_use` and its matching `tool_result` are always adjacent in the output and always either both present or both absent. This is enforced in code, not in convention. Masking preserves the pair — only the `content` of the ToolResult changes, not its presence.

### 5. Hybrid compaction trigger (graduated fallback)

Observation masking is the default and handles most workflows. LLM summarization activates only when masking alone cannot keep the context under `soft_cap_chars`. This is the "hybrid" policy.

**Trigger:** `build()` first materialises the masked context. If `total_chars > soft_cap_chars`:

1. Identify the oldest contiguous slice of already-masked turns (not the recent window). These are already placeholders, so the model is already not seeing the full content — but the *number* of placeholder turns itself takes up space in long workflows.
2. Send the placeholder slice to the *same provider the agent is using*, with a fixed compaction prompt:

   > You are compressing a sequence of masked observations from an autonomous agent's history. Each entry shows what tool was called and whether it succeeded. Produce a single paragraph summarizing: (a) what the agent accomplished in this span, (b) any errors or dead ends, (c) any decisions the agent committed to. Do NOT preserve individual tool call details — the summary replaces them entirely.

3. The compaction call's response becomes the `summary` field of `ContextCompacted`.
4. Emit `ContextCompacted { replaces_seq_range, summary, bytes_saved }` onto the bus.
5. Re-materialise the context. If still over `hard_cap_chars`, return `ContextError::HardCapExceeded`.

**Key difference from pure-summarization approaches:** The compaction prompt operates on *masked placeholders*, not on full tool outputs. This means the compaction LLM call is itself cheap (the input is small), and the summary is a summary of summaries — a second level of compression. Full tool outputs were already shed by masking; compaction only fires when even the placeholder count is too high.

The compaction call uses a fresh `correlation_id` and its own `LlmCallStarted` / `LlmCallCompleted` so it appears in the event log as a normal LLM call attributable to "compaction", not as a hidden cost. Telemetry stays honest.

### 6. `run_agent_step` migration

The current agent loop's `let mut messages: Vec<Message> = Vec::new()` (`src/handlers/run_pipeline.rs:258`) goes away. In its place:

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
    let messages = context_builder.build()?;
    let request = ChatRequest { messages, tools: tool_defs.clone(), ... };
    let response = provider.chat_completion(&request).await?;
    // emit LlmCallCompleted, ToolCallRequested/Completed events as today;
    // the next iteration's context_builder.build() reads them back.
    ...
}
```

The crucial change: there is no `messages.push(...)` anywhere in `run_agent_step`. Everything that previously went into the in-memory `Vec` now goes onto the event log via the existing `emit_event` calls, and the next iteration's builder picks it up. **The event log is the source of truth, and the builder is the view.**

### 7. Tokens vs characters

`approx_context_chars` is what `LlmCallStarted` measures today. It is wrong for budgeting (a 4000-character JSON tool result is not 4000 tokens), but it is fast, model-agnostic, and stable across providers. For v1 the ADR keeps `chars` as the unit and sets `soft_cap_chars` / `hard_cap_chars` to chars-based defaults derived empirically (~3.5 chars/token for English, more for code).

A future iteration may swap in a real tokeniser per provider (`tiktoken` for OpenAI, the Anthropic tokeniser for Claude). The builder's API takes a generic `ContextSize` type that today is `usize` chars but is intentionally newtyped so the swap is local.

### 8. Open questions deliberately left for the slice review

- Should `ApprovalRequested` / `ApprovalDecided` appear in the model context? Argument for: gives the model self-awareness about its own gating, useful for "the user denied my last request, try a different approach". Argument against: noise the model does not need to make decisions. **Tentative answer:** include them, behind a config flag, default off; revisit after slice 3 ships and we have real workflows to check against.
- Should the compaction LLM call use a *different* (cheaper) model than the agent? Argument for: compaction is a well-defined task and a small model is plenty. Argument against: introducing a second model in `agents/*.yml` is a much bigger config change than this ADR wants to land. **Tentative answer:** v1 uses the agent's own provider; v2 may add `compaction_provider:` as a sibling field on the agent.
- Where does `min_tail_turns` come from when the user has a 30-step pipeline whose every step is significant? Compaction will eventually be forced to compress meaningful work. **Tentative answer:** `min_tail_turns` is a config knob, not a guarantee; if the hard cap is hit even with `min_tail_turns` items in the tail, the loop bails out with a clear error. Operators tune the cap to their workload.
- Should masking preserve the `content` field of Assistant messages (the model's reasoning text), or only the tool_call metadata? **Tentative answer:** preserve it in full for the observation window, truncate to a preview (first 200 chars) for masked turns. The model's own prior reasoning is cheap (it generated it) and helps maintain coherence.

## Alternatives Considered

- **Do nothing; trust the LLM provider's auto-truncation.** Anthropic and OpenAI both fail loud when context exceeds their max, with no automatic truncation. "Doing nothing" means the agent hits a wall mid-run with a 400 error. Rejected: that is the bug, not the fix.

- **LLM summarization as the primary mechanism (original ADR-0016 design).** Research shows this is counterproductive as a default: agents with summarized history take more steps because they trust the compressed context, and the per-step savings are eaten by longer trajectories. Summarization also adds an extra LLM call per compaction. **Demoted to graduated fallback** — only triggers when masking alone cannot keep context under the soft cap. This covers genuinely long-horizon workflows (50+ tool calls) where even masked placeholders accumulate.

- **Sliding window only — drop the oldest N turns entirely, no placeholders.** Simpler than masking. Rejected: the model needs to know what it already tried. Without even a placeholder, it re-reads the same files and re-runs the same commands. Masking preserves trajectory awareness at ~15 tokens per masked turn.

- **Keep `Vec<Message>` in `run_agent_step`, add only a mask/summarize hook.** The smallest possible diff. Rejected for two reasons: (a) it leaves workflow memory in its hashmap, perpetuating the "state lives in two places, one of which is event-sourced and one of which is not" problem, and (b) the in-memory `Vec` and the event log will inevitably drift, making replay produce a different conversation than the one that actually ran.

- **Build the context in the provider layer** (`src/llm/openai.rs`, `src/llm/anthropic.rs`). Each provider knows its own token budget and tokenizer. Rejected: this puts context policy into N adapters instead of one shared place, and the *event filtering* logic ("which events count as significant") is an engine concern, not a provider concern.

- **Lift workflow memory in a separate ADR; do context management standalone.** Rejected: see the Context section. The two share the projection layer. Splitting them forces the projection to be designed twice.

- **Use a vector store for "long-term memory" / semantic file retrieval instead of masking.** Pull the most relevant past events or file chunks by embedding similarity. Rejected for this ADR: introduces an embedding dependency, an embedding model, a vector index, and a similarity threshold — none of which exist in zymi-core today. Both are natural extensions once `ContextBuilder` exists (a future `SemanticRetriever` injects `Message::System("[recall: ...]")` into the prefix, or a `SmartFileReader` replaces full file contents with relevant chunks), but they deserve their own ADRs.

- **Agent isolation / decomposition (ContextEvolve-style).** Split context by agent role with handoff via compact state message. Rejected for this ADR: zymi-core's current pipeline model is sequential single-agent-per-step, and agent isolation requires a multi-agent handoff protocol that does not yet exist. The `ContextBuilder`'s `stream_id` filtering is the foundation — isolation becomes "each agent gets its own stream_id and receives only a summary of the previous agent's stream" — but the handoff protocol is a separate design.

## Consequences

- **Pro.** The O(N²) token explosion in long workflows goes away. A 50-iteration agent run pays ~10 full turns + ~40 placeholder turns per iteration instead of ~50 full turns.
- **Pro.** The primary mechanism (masking) requires zero extra LLM calls. Most workflows never trigger summarization at all — only genuinely long-horizon runs (50+ tool calls) that exceed masking capacity.
- **Pro.** Workflow memory is finally event-sourced. Replay reconstructs the agent's full state from the log alone, with no side channels. The "Recovery and projections" goal in drift becomes tractable in the same pass.
- **Pro.** `ContextCompacted` is a first-class event, so compaction (when it does trigger) is auditable. Operators can see exactly when and why the engine compressed history.
- **Pro.** The compaction LLM call operates on masked placeholders, not full tool outputs, so even the fallback is cheap.
- **Pro.** `ContextBuilder` is the natural integration point for future features: semantic file retrieval, agent isolation, vector recall, streaming context updates. Each becomes a method or layer on the builder, not a patch to `run_agent_step`.
- **Pro.** Removes the `Arc<Mutex<HashMap<...>>>` side channel from `engine/tools.rs`. One less place where state lives.
- **Pro.** Enriched events (§1a) make the event log a complete, auditable record of every tool output and every LLM response. Post-hoc debugging ("why did the agent do X after reading Y?"), faithful replay, and regression testing by diffing event logs across engine versions all become possible without side channels.
- **Con.** Enriched events increase event store size — full tool outputs instead of 200-char previews. A 50-iteration run grows from ~50KB to ~150KB. Mitigation: trivial for SQLite; `zymi events` continues to display previews by default; event archival (future work) addresses long-running serve sessions at the store level.
- **Con.** Significant rewrite of `run_agent_step`. Easy to break the `tool_use` / `tool_result` adjacency invariant in subtle ways. Mitigation: a builder-level test that takes a sequence of synthetic events with tool calls and verifies the output never splits a pair, plus a real end-to-end test against a recorded agent run.
- **Con.** Masking discards information. An agent that masked a file read 15 turns ago cannot recover the file content without re-reading it. Mitigation: (a) the placeholder tells the model what was read and whether it succeeded, so it can decide whether to re-read; (b) the observation window of 10 turns is generous enough for most workflows; (c) semantic file retrieval (future ADR) addresses the file-specific case.
- **Con.** Compaction quality (when it triggers) is a function of the prompt and the model. A bad summary loses information the agent later needs. Mitigation: `bytes_saved: i64` allows the trigger to detect and react to bad compactions; future iterations can A/B compaction prompts.
- **Con.** `ContextBuilder::build()` runs on every iteration and walks the event store. For long sessions this is more work than `messages.clone()`. Mitigation: the store is local SQLite, walks are fast, and the builder caches the materialised context inside one iteration. Profile before optimising.
- **Con.** The change to memory tools is observable to existing pipelines: a `write_memory` call followed immediately by a `read_memory` in the *same* iteration may not see the write, because the write goes through the bus and is only picked up by the projection asynchronously. Mitigation: the projection is updated synchronously by the same task that emits the event before the next iteration starts. This is testable and the test exists in slice 1.
- **Con.** `ContextConfig` is a new knob surface. Operators have to tune `observation_window`, `soft_cap_chars`, `hard_cap_chars`. Defaults are picked from research (window=10, soft_cap and hard_cap from empirical workloads), but bad defaults will hurt people. Mitigation: ship conservative defaults, document the knobs in `project.yml` schema, and surface "N turns masked, M compacted" in `zymi events` summary.

## Implementation slices

Five slices, each independently shippable, ordered so each one leaves the codebase in a compiling, passing-tests state.

1. **Slice 1 — Memory lift.** Add `MemoryWritten` and `MemoryDeleted` to `EventKind`. Add `MemoryProjection`. Rewrite `write_memory` in `src/engine/tools.rs` to emit a `MemoryWritten` event and read from the projection. Delete `Arc<Mutex<HashMap<...>>>`. Add the synchronous-update test. **No `ContextBuilder` yet.** The agent loop still uses `Vec<Message>`. This slice only touches memory, and is enough on its own to close the existing "Event-sourced state for workflow memory" goal in drift.

2. **Slice 2 — Observation masking.** Add `fn mask_observations(messages: &[Message], observation_window: usize) -> Vec<Message>` to `src/handlers/run_pipeline.rs` (or a new `src/runtime/context_window.rs`). Apply it in `run_agent_step` before building `ChatRequest`. No `ContextBuilder` yet — the `Vec<Message>` still exists, masking is a pure function applied to it. Tests: (a) prefix messages are never masked, (b) last N turns are in full, (c) older turns have placeholder ToolResults, (d) tool_use/tool_result pairs are never split, (e) masked output is strictly smaller than input. This slice delivers the ~2x cost reduction with minimal code change.

3. **Slice 3 — Event enrichment + layer-based `ContextBuilder`.** Two steps, shipped together because the builder depends on enriched events. **Step A:** Enrich `ToolCallCompleted` with full `result` field and `LlmCallCompleted` with full `response_message` (§1a). Update `run_agent_step` emit sites to store full payloads. Add `#[serde(default)]` for backward compat with pre-enrichment events. **Step B:** Add `src/runtime/context_builder.rs` with `build()` that produces prefix + memory snapshot + masked tail, reading from the enriched event store. Migrate `run_agent_step` to use the builder instead of the in-memory `Vec`. Observation masking from slice 2 moves into the builder as Layer C's drop policy. No compaction yet — the builder replays enriched events and applies masking. The hardest tests live here: tool_use/tool_result adjacency across event → message conversion, graceful degradation for pre-enrichment events.

4. **Slice 4 — Hybrid compaction trigger + hard cap.** Add `ContextCompacted` event variant. Implement Layer B (compacted summaries) in the builder. Implement the soft-cap trigger: if masked context exceeds soft_cap, summarize the oldest masked batch via an LLM call. Add `hard_cap_chars` enforcement and `ContextError::HardCapExceeded`. Add `cli/events.rs` renderers for `ContextCompacted`, `MemoryWritten`, `MemoryDeleted`. This slice handles the long tail of genuinely long-horizon workflows.

5. **Slice 5 — Configuration in `project.yml`.** Surface `ContextConfig` as `runtime.context:` in `project.yml`: `observation_window`, `soft_cap_chars`, `hard_cap_chars`, `min_tail_turns`. schemars-derive on the new config struct. Defaults baked into the engine, knobs available to operators who outgrow them.

Each slice updates this ADR with a "Slice N — what landed" section. Slice 1 is independently valuable and closes the "Event-sourced state for workflow memory" drift goal even if the rest of the ADR slips. Slice 2 is independently valuable and delivers the primary cost reduction even without the full ContextBuilder.

## Relationship to other ADRs

- **ADR-0011 (Pipeline execution engine).** This ADR is the first significant rewrite of `run_agent_step` since ADR-0011. Behaviour is preserved (same iteration count, same approval flow, same tool dispatch) but the data flow changes from "in-memory Vec mutated by the loop" to "view over the event store rebuilt each iteration".

- **ADR-0013 (Target runtime architecture).** `ContextBuilder` is a new component owned by `Runtime`, accessed via a new `runtime.context_builder()` method. `MemoryProjection` is a new long-lived state owned by `Runtime`, kept up-to-date by a bus subscription started by `RuntimeBuilder::build`. Both fit into the existing builder pattern without changes to `RuntimeBuilder`'s public API.

- **ADR-0015 (Persistent shell session).** Mutually reinforcing. `ContextBuilder` surfaces `ShellSessionStarted` / `ShellSessionClosed` events to the model so the agent knows whether its previous-turn shell state survived a compaction or a session reap.

- **Open goal: Recovery and projections.** Slice 1 of this ADR closes the prerequisite. Once memory is event-sourced, recovery becomes "replay events to rebuild `MemoryProjection` and any future projections". `ContextCompacted` from slice 4 then becomes a snapshot point.

- **Open goal: Streaming runtime contract.** The `ContextBuilder` is the natural attachment point for streaming — streaming becomes a property of the builder, not a parallel mechanism.

- **Future: Semantic file retrieval.** A natural extension of the builder. A `SmartFileReader` middleware between the `read_file` tool and the agent replaces full file contents with relevant chunks by embedding similarity. Plugs into the builder as a content transformation on Layer C. Separate ADR when embedding infrastructure exists.

- **Future: Agent isolation.** Each agent in a pipeline gets its own `stream_id`. Handoff between agents is a compact state message (summary of the previous agent's stream) injected as a `Message::System` in the next agent's prefix. The builder's `stream_id` scoping is the foundation. Separate ADR when multi-agent handoff protocol is designed.

## Slice 1 — what landed

**Date:** 2026-04-13

Memory is now event-sourced. The legacy `Arc<Mutex<HashMap<String, String>>>` (`MemoryStore` type alias, `new_memory_store()` factory) is replaced by `MemoryBridge` — a wrapper around `MemoryProjection` + `EventBus` that emits `MemoryWritten` events on write and reads from the projection.

**New code:**

- `src/events/mod.rs` — `EventKind::MemoryWritten { key, value, previous_value_seq }` and `EventKind::MemoryDeleted { key, previous_value_seq }`. Both are state events with `tag()` mappings and serialization roundtrip tests.
- `src/esaa/projections.rs` — `MemoryProjection` with `apply()` (via `Projection` trait), `get()`, `snapshot()`, `seq_for()`, `is_empty()`. Six tests covering write, overwrite, delete, snapshot, and unrelated-event filtering.
- `src/engine/tools.rs` — `MemoryBridge` struct (replaces `Arc<Mutex<HashMap>>`). `write()` emits `MemoryWritten` via bus then applies synchronously to local projection. `get()` reads from projection. `delete()` emits `MemoryDeleted`. `new_memory_store(bus)` factory now takes `Arc<EventBus>`. Added `read_memory` as a new builtin tool (was only `write_memory` before). Eight new tests including event persistence verification and `previous_value_seq` tracking on overwrite.
- `src/runtime/action_executor.rs` — `ActionContext` gains `correlation_id: Uuid` field. Both `BuiltinActionExecutor` and `CatalogActionExecutor` pass it through to `execute_builtin_tool`.
- `src/handlers/run_pipeline.rs` — `new_memory_store(Arc::clone(rt.bus()))`, `ActionContext` construction includes `correlation_id`.
- `src/cli/events.rs` — `MemoryWritten` rendered at indent level 1 (cyan), `MemoryDeleted` at level 1 (yellow). Overwrite is annotated.
- `src/lib.rs` — re-exports `MemoryProjection`.

**Synchronous update guarantee:** `MemoryBridge::write()` publishes the event to the bus (which persists it to SQLite and assigns a sequence number), then immediately calls `projection.apply()` on the local copy. The next `get()` in the same iteration always sees the write. This is tested in `memory_write_read_roundtrip`.

**284 tests pass, clippy clean.** 30 new tests added across events, projections, tools, and action_executor modules.

## Slice 2 — what landed

**Date:** 2026-04-13

Observation masking is now applied before every LLM call. Older tool results are replaced with compact placeholders (`[masked: tool_name("primary_arg") → ok, N chars]`), keeping the last 10 turns in full. This is the primary cost-reduction mechanism — ~2× savings with no quality loss (JetBrains research).

**New code:**

- `src/runtime/context_window.rs` — new module. `mask_observations(messages, observation_window) -> Vec<Message>` pure function. Groups messages into a prefix (System/User, never masked) and turns (Assistant + ToolResults). The last `observation_window` turns are kept in full; older turns get their ToolResult content replaced with placeholders preserving tool name, primary argument (extracted from JSON args by tool-specific key), status (ok/error), and original char count. Assistant content in masked turns is truncated to 200 chars. `approx_chars()` utility for telemetry. `DEFAULT_OBSERVATION_WINDOW = 10`.
- `src/runtime/mod.rs` — registers `pub mod context_window`.
- `src/handlers/run_pipeline.rs` — `run_agent_step` applies `mask_observations()` before building `ChatRequest`. `LlmCallStarted.approx_context_chars` now reflects the masked (actually-sent) message count, not the full accumulation.

**Invariants enforced:**

- `tool_use`/`tool_result` pairs are never split — masking replaces content, not presence.
- Prefix messages (System, User, UserMultimodal) are never masked.
- The placeholder format includes the primary argument (file path, command, key, query, URL, or fallback to first string arg), truncated to 60 chars with ellipsis.
- The full unmasked `Vec<Message>` is preserved across iterations; masking is a view transformation applied fresh before each LLM call.

**304 tests pass, clippy clean.** 20 new tests covering: prefix preservation, recent-turn retention, placeholder generation, pair integrity, size reduction, args extraction for each tool type, error status, multi-tool turns, assistant content truncation, and interspersed user messages.

## Slice 3 — what landed

**Date:** 2026-04-13

Event enrichment + ContextBuilder + `run_agent_step` migration. The in-memory `Vec<Message>` is gone — the event log is the source of truth, and the builder is the view.

### Step A — Event enrichment (ADR-0016 §1a)

`ToolCallCompleted` gains `result: String` (full tool output, untruncated). `LlmCallCompleted` gains `response_message: Option<Message>` (full assistant message including content and tool_calls). Both have `#[serde(default)]` for backward compat — pre-enrichment events stored before this change deserialize with `result = ""` / `response_message = None`. Existing fields (`result_preview`, `content_preview`, `has_tool_calls`) are kept for display (CLI, Langfuse) and backward compat.

**Files changed:** `src/events/mod.rs` (new fields + 3 new tests: enriched roundtrips + backward compat), `src/handlers/run_pipeline.rs` (emit sites populate new fields), `src/cli/events.rs` / `src/services/langfuse.rs` / `src/esaa/projections.rs` / `src/events/store/sqlite.rs` (pattern matches and test fixtures updated).

### Step B — ContextBuilder (ADR-0016 §4)

New `src/runtime/context_builder.rs`. `ContextBuilder` materialises the agent's working context as a view over the event store with three layers:

- **Layer A (prefix):** `Message::System(system_prompt)` + `Message::User(task)` — deterministic, never masked.
- **Layer B (memory snapshot):** Current state from `MemoryBridge`, formatted as `Message::System("[memory]\nkey: value\n...")`. Only included when non-empty. Values previewed to 200 chars.
- **Layer C (observation tail):** Conversation turns reconstructed from enriched events (`LlmCallCompleted` → `Message::Assistant`, `ToolCallCompleted` → `Message::ToolResult`), with observation masking applied. Pre-enrichment events gracefully degrade to `content_preview` / `result_preview`.

**7 tests:** prefix-only build, memory snapshot inclusion, enriched event reconstruction, pre-enrichment fallback, observation masking integration, tool_use/tool_result pair preservation, noise event exclusion.

### Step C — `run_agent_step` migration (ADR-0016 §6)

The core refactor. `let mut messages: Vec<Message>` is deleted. Each iteration calls `context_builder.build()` which reads events from the store, converts significant ones to messages, and applies masking.

Per-step sub-streams (`{pipeline_stream_id}:step:{step_id}`) isolate each step's LLM/tool events from other parallel steps. `WorkflowNodeStarted` / `WorkflowNodeCompleted` stay on the pipeline stream for observability. `ActionContext.stream_id` remains the pipeline stream (memory events are pipeline-scoped since all steps share one `MemoryStore`).

`messages.push(response.message)` and `messages.push(ToolResult { .. })` are both gone — the enriched `LlmCallCompleted` and `ToolCallCompleted` events are the records, and the next `build()` reads them back.

**314 tests pass, clippy clean.** 10 new tests (3 enrichment + 7 builder).
