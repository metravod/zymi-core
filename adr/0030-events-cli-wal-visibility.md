# `zymi events` CLI vs SQLite WAL visibility

Date: 2026-05-15

Status: Draft — observation only, fix deferred until more bugs land in this area.

## Context

The SQLite event store opens with `PRAGMA journal_mode=WAL` (`src/events/store/sqlite.rs:25`). Writer and reader processes share the same path via `open_store_async` (`src/cli/events.rs:29`); the CLI opens its own `Connection` against the same file.

Bug observed in 0.6.9 on metrahost: terminal events written near the end of a pipeline run (`pipeline_completed`, sometimes the last few `step_*`) were not visible via `zymi events` until a manual checkpoint:

```
python3 -c "import sqlite3; sqlite3.connect('.zymi/events.db').execute('PRAGMA wal_checkpoint(FULL);')"
```

After the checkpoint the events appeared. The `sqlite3` CLI is not installed on metrahost — only the Python workaround is available there.

In WAL mode committed frames are normally visible to other readers of the same file without an explicit checkpoint, so something in the writer/reader interaction is keeping the tail of the log from being seen. Unconfirmed hypotheses:

1. The writer process exits without finishing the WAL fsync, and the reader connection (a fresh process) reads stale state because the WAL/-shm files have not been synced to disk.
2. The reader uses `read_stream` / `read_all` queries that hit a connection-local cache or a stale `wal-index` snapshot when the writer was a separate process whose `-shm` file was not normally torn down.
3. The pipeline writer drops the connection without an explicit `wal_checkpoint`; if the pipeline ran inside a long-lived host (e.g. `zymi serve`), no SQLite "last connection closes" auto-checkpoint fires either.

We do not yet have a reliable repro outside metrahost, and only one user has hit it once. Treating as a single data point, not a pattern.

## Decision

Do not change runtime or CLI code yet. Park this with three concrete actions:

- This ADR exists so the next time the symptom shows up we already have the workaround and the hypotheses written down — no re-discovery cost.
- Memory entry [[project-events-cli-wal-bug]] carries the user-facing workaround for in-session help.
- Wait for a second hit (different host, or a clean repro) before picking a fix. Acting now risks fixing the wrong layer — if it is hypothesis (1) the right place is the writer's shutdown path, if (2) it is the reader's connection setup, if (3) it is an `wal_autocheckpoint` tuning or an explicit checkpoint on `PipelineCompleted`.

When a fix is warranted, the cheapest first move is to checkpoint on `PipelineCompleted` in the writer (one extra `PRAGMA wal_checkpoint(PASSIVE)` per run), which covers (1) and (3) without changing the reader. The reader-side fix (re-opening the connection or calling `wal_checkpoint(PASSIVE)` before the first read in the CLI) is a fallback if the writer-side fix doesn't fully resolve it.

## Consequences

- Users hitting the symptom get a documented workaround instead of a "ghost events" mystery.
- 0.6.9 ships with the bug; not blocking the unveil polish (no ADR-0030-versioned release).
- If the bug recurs and we go with the writer-side checkpoint, expect a small per-pipeline cost — `wal_checkpoint(PASSIVE)` is non-blocking and runs against the writer's own connection, so it shouldn't show up under load. Worth measuring on a `zymi serve` host before merging.
- This ADR will get a Status update or a successor when a fix lands.
