# Events and replay

zymi-core is event-sourced. Every state change — message received, agent thought, tool call, approval decision, pipeline step started/completed — is an immutable event in an append-only log. The log is hash-chained per stream, queryable with `zymi events`, observable in real time with `zymi observe`, and replayable with `zymi resume`.

## Overview

Three things make this useful:

1. **Audit trail by construction.** Nothing the runtime does goes un-recorded. The trail is tamper-evident (hash chain).
2. **Forward debugging.** Re-play a failed run, fork from any step, change a config on disk, re-run only the descendants — without re-burning the expensive upstream steps.
3. **Single source of truth.** Approvals, pipeline state, agent ReAct turns — all derive from the event stream, never from in-memory caches or sidecars.

The full model is in ADR-0009 (event sourcing) and ADR-0018 (fork-resume).

## The event log

Every event has:

- `stream_id` — the conceptual entity it belongs to (a pipeline run, an approval, a connector instance).
- `sequence` — monotonic per-stream counter (1-based).
- `kind` — the variant tag (`UserMessageReceived`, `WorkflowNodeCompleted`, `ApprovalRequested`, …). `EventKind` is `#[non_exhaustive]` so adding variants is non-breaking.
- `payload` — kind-specific data (JSON-encoded).
- `prev_hash` / `hash` — links to the previous event in the same stream (BLAKE3); enables `zymi verify` to detect tampering.
- `correlation_id` — optional cross-stream link (e.g. webhook → pipeline run).
- `timestamp`, `source`.

Streams are independent: each pipeline run is its own stream; approvals get their own; connectors emit into per-instance streams. The append order within a stream is what matters; cross-stream order is lossy.

## Inspect

```bash
# All pipeline runs, newest first.
zymi runs [--pipeline NAME] [--limit N]

# Every event in a single stream.
zymi events --stream pipeline-chat-abc

# Filter by event-kind across streams.
zymi events --kind WorkflowNodeCompleted [--limit N] [--raw]

# Verify the hash chain.
zymi verify                              # all streams
zymi verify --stream pipeline-chat-abc   # one stream

# Live TUI: 3 panels — runs / pipeline DAG / event timeline.
zymi observe [--run STREAM_ID]
```

`--raw` on `events` and `runs` produces one JSON document per line — pipe into `jq` for ad-hoc analysis.

## Fork-resume

You ran a 4-step pipeline. Step 4 produced a bad output because the system_prompt was off. Edit `agents/writer.yml`. Re-running the whole pipeline burns LLM cost on steps 1-3 unnecessarily.

```bash
# Re-run only step 4 (and any DAG-descendants of it). Steps 1-3 are
# frozen — their events are copied from the parent stream into a new
# stream, no replay, no LLM cost.
zymi resume pipeline-research-abc --from-step write_report

# Print the resume plan and exit (frozen vs re-executed steps) without
# writing any new events.
zymi resume pipeline-research-abc --from-step write_report --dry-run
```

Semantics:

- The fork creates a **new stream** so the original run is unmodified.
- All steps **upstream** of `--from-step` are frozen: their events are copied into the new stream verbatim, in original order.
- The fork step and all its DAG-descendants run from scratch using the **current configs on disk** (`agents/*.yml`, `pipelines/*.yml`, `tools/*.yml`).
- Approvals re-fire — a fork step that needed approval in the parent run will need it again on the new stream.
- The original stream is reachable in `zymi runs` / `zymi events` forever.

ADR-0018 has the full idempotency contract.

## Replay & restart safety

Beyond explicit fork-resume, the event store underpins automatic restart safety:

- **`zymi serve` restart:** unfulfilled approvals are re-subscribed (within timeout) or sealed as `ApprovalDenied{reason: restart_timeout}` — no orphaned state.
- **Connector cursors** persist in `connectors.db` (sqlite) or `connector_cursors` table (postgres) so `http_poll` doesn't double-fire on restart.
- **Multi-process `zymi serve` against a shared Postgres store** sees one cursor table and one event log — no double work.

## Event kinds (selected)

| Kind | Stream type | Meaning |
|------|------------|---------|
| `UserMessageReceived` | connector / pipeline | Inbound message from a connector |
| `PipelineRequested` | pipeline | Connector / CLI asks for a pipeline run |
| `WorkflowNodeStarted` / `WorkflowNodeCompleted` | pipeline | Pipeline step lifecycle |
| `AgentProcessingStarted` / `AgentProcessingCompleted` | pipeline | Agent ReAct turn |
| `ToolCallRequested` / `ToolCallCompleted` | pipeline | Any tool dispatch (declarative / Python / MCP / builtin) |
| `ApprovalRequested` / `ApprovalGranted` / `ApprovalDenied` | approval | Human-in-the-loop |
| `OutboundDispatched` / `OutboundFailed` | output | `http_post` / `file_append` / `stdout` results |
| `ResponseReady` | pipeline | Final pipeline output, the conventional "agent has answered" event |

`zymi events --kind <KindTag>` filters across streams.

## Gotchas

- **Don't edit `.zymi/events.db` by hand.** It breaks the hash chain and `zymi verify` will flag it.
- **`zymi resume` is non-destructive** — it always creates a new stream. The parent stream is preserved.
- **Frozen steps are NOT re-executed**, so config changes to the agents/tools that produced them are **not** picked up. If you need a different upstream output, fork from earlier in the DAG.
- **`correlation_id`** is the bridge for cross-stream causality (e.g. webhook stream → pipeline-run stream). It's optional but invaluable for tracing.
- **`EventKind` is `#[non_exhaustive]`** so a forward-compatible reader can ignore unknown variants. But if you snapshot event JSON and feed it back into an older zymi, that older zymi may reject unknown kinds.

## See also

- [CLI reference](cli.md) — `events`, `runs`, `verify`, `observe`, `resume` flags
- [Approvals](approvals.md) — how approval events flow
- [Store backends](store-backends.md) — where the log lives
- ADR-0009 (event sourcing), ADR-0018 (fork-resume), ADR-0022 (event-sourced approvals)
