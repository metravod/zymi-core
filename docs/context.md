# Context window

Every LLM call zymi makes ships a list of messages: system prompt, user task, prior assistant responses, prior tool results. As an agent loop runs (or a chat session grows), that list grows. Left unmanaged it grows quadratically — each iteration sends *all* prior turns, so iteration N costs O(N) tokens and the run total is O(N²). Tool results dominate, often by 10×.

zymi keeps this in check with five knobs that compose into the context an LLM actually sees. They live in `project.yml` under `runtime.context:`. Defaults target long coding-agent runs; conversational workloads (chat bots, RAG assistants) want them cranked down.

## How history flows to the model

`ContextBuilder::build()` runs at the top of every agent iteration and produces the message list as a *view* over the event store, in three layers (ADR-0016 §4):

1. **Prefix** — system prompt + user task. Deterministic, never touched.
2. **Memory snapshot** — current state of the workflow `MemoryProjection`, formatted as a compact `[memory]` block. Only emitted when non-empty.
3. **Observation tail** — every `LlmCallCompleted` / `ToolCallCompleted` / `UserMessageReceived` event for this stream, converted back into messages. This layer is where the five knobs operate.

The tail is transformed by the knobs in this order:

```
events_to_messages(tail)
  → forget_old_turns(keep=forget_after_turns)    # opt-in hard drop
  → mask_observations(window=observation_window) # placeholder for old tool results
  → check soft_cap_chars                          # trigger ContextCompacted if over
  → check hard_cap_chars                          # fail loud if still over
```

## The five knobs

| Knob | Type | Default | What it does |
|------|------|---------|--------------|
| `observation_window` | `usize` | `10` | Number of recent turns whose `tool_result` is sent in full. Older turns get a `[masked: tool_name("arg") → ok, N chars]` placeholder. Trajectory-aware: the model still knows what it ran. |
| `soft_cap_chars` | `usize` | `400_000` | When total chars after masking exceed this, the engine fires an LLM compaction call that turns the oldest masked turns into one `[compacted history]\n…` summary message. Emits a `ContextCompacted` event for replay. |
| `hard_cap_chars` | `usize` | `600_000` | Absolute ceiling. If context is still over this after compaction, the agent step fails with `HardCapExceeded` rather than sending a request the provider would reject. |
| `min_tail_turns` | `usize` | `4` | Floor on compaction: the engine never compacts away the last N turns. Counterpart to `observation_window` for the summarization path. |
| `forget_after_turns` | `Option<usize>` | `null` (off) | **Opt-in.** Drop turns older than the last N entirely — no placeholder. Runs *before* masking. Use this for chat where ancient turns ("Привет!" from 30 messages ago) carry no trajectory information worth ~20 tokens of placeholder. Leave off for coding agents (ADR-0016 §3 reasoning applies). |

Notes:
- `soft_cap_chars` / `hard_cap_chars` are **characters, not tokens**. Roughly `chars × 0.28 ≈ tokens` for English; less for code. We use chars because they are model-agnostic and stable across providers.
- Counts are by `Assistant + ToolResult` turn (one LLM iteration), not by user message. A single user input can produce several turns if the agent uses tools.

## Three recommended profiles

### Coding agent — engine defaults

```yaml
# project.yml — implied if you omit runtime.context entirely.
runtime:
  context:
    observation_window: 10
    soft_cap_chars: 400000
    hard_cap_chars: 600000
    min_tail_turns: 4
    # forget_after_turns: null      # default
```

For research / code-review / refactor / ETL agents doing many tool calls per run. Trajectory awareness matters — masking preserves "I already read `src/main.rs`, no point reading again" at ~20 tokens per turn.

### Chat bot (RAG / Telegram / Slack)

```yaml
runtime:
  context:
    observation_window: 2
    soft_cap_chars: 40000
    hard_cap_chars: 80000
    min_tail_turns: 2
    forget_after_turns: 12
```

For conversational use over a long-lived stream. Why each value:
- `observation_window: 2` — RAG tool results are typically 10–20k chars each. Holding more than 2 in full blows the budget within a few turns.
- `forget_after_turns: 12` — old greetings, topic switches, and small-talk add nothing on turn 30. Drop them outright; masking placeholders are still a non-trivial cost when accumulated.
- `soft_cap_chars: 40_000 / hard_cap_chars: 80_000` — chat models respond faster on smaller contexts; the steep latency curve past ~30k chars is what users perceive as "the bot got slow".

### Evals / one-shot

```yaml
runtime:
  context:
    observation_window: 4
    soft_cap_chars: 60000
    hard_cap_chars: 100000
    min_tail_turns: 2
    # forget_after_turns: null      # default
```

For non-interactive evaluation runs (`zymi eval`, batch processing). Tighter than coding-agent defaults so failing runs don't burn a full context budget before timing out, but still keeps enough trajectory for typical eval prompts.

## Per-step `context.mode: fresh` (ADR-0031)

The five knobs above run on every agent step uniformly. That's right for conversational steps — smalltalk should remember the last turn — and wrong for retrieval / analytical steps. In a chat router pipeline where every turn lands in the same `stream_id` (the chat id), the `retrieve` step's sub-stream piles up across runs: yesterday's question-and-chunks polluting today's retrieval.

Opt out per step:

```yaml
steps:
  - id: smalltalk
    agent: chat
    task: ${inputs.user_message}
    # context: omitted → inherit (conversational tail, the default)

  - id: retrieve
    agent: researcher
    task: ${inputs.user_message}
    context:
      mode: fresh    # drop prior runs' tail from Layer C
```

Semantics:

- `mode: inherit` (default, absent block ≡ this): no change. The full sub-stream is reconstructed as Layer C.
- `mode: fresh`: at the top of the step, `ContextBuilder` captures `step_start = now()` and filters Layer C to events with `timestamp >= step_start`. Within the step, ReAct iterations still see each other's tool calls (their events timestamp after the cutoff). Across runs, the previous run's events are dropped.

What is **not** changed:

- `step_stream_id` stays `{stream_id}:step:{step_id}`. The events still land in the same sub-stream — `zymi observe` and `zymi events` see them. `zymi resume` keeps working.
- Layer A (system + task) and Layer B (memory snapshot) are untouched.
- Tool steps (`kind: Tool`) build no context; the block is accepted on tool steps but inert.

## What happens if you skip the block

When `project.yml` has no `runtime.context:` section (or no `runtime:` block at all), the engine uses defaults and `zymi serve` prints one info-line on startup:

```
zymi serve: listening for PipelineRequested events ...
  store: sqlite:.zymi/events.db
  poll:  100ms
  note:  runtime.context not set — using engine defaults; see docs/context.md for chat/coding profiles
```

This is informational, not a warning. Existing v0.6.x projects keep working; the line is a nudge to peek at this guide if you're running a chat workload.

## Observability

The knobs are surfaced in two event streams you can replay or `zymi events` against:

- `LlmCallStarted.approx_context_chars` — the actually-sent context size after all five knobs have run. Watch this curve over a session; if it grows linearly, your knobs aren't biting.
- `ContextCompacted { replaces_seq_range, summary, bytes_saved }` — emitted whenever the soft cap fires summarization. Negative `bytes_saved` means the summary came back larger than the source — a signal to lower `soft_cap_chars` so compaction triggers earlier on a smaller batch.

## See also

- **ADR-0016** — original design (`adr/0016-context-window-management.md`), including §3 (observation masking) and §Slice 6 (the `forget_after_turns` rationale for conversational use).
- **ADR-0031** — per-step `context.mode: fresh` opt-out (`adr/0031-step-context-mode-fresh.md`).
- **`docs/project-yaml.md`** — full `runtime:` block schema alongside `connectors:`, `outputs:`, `approvals:`.
- **`docs/events-and-replay.md`** — how `ContextCompacted` works during replay and forks.
