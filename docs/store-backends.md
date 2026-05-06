# Store backends

The event store is where every `Event` zymi produces is persisted, hash-chained, and made available for replay. Two backends ship: embedded **SQLite** (default, zero-config) and networked **Postgres** (for multi-process `zymi serve` against shared state).

## Overview

Configured via the top-level `store:` field in `project.yml`. Both backends implement the same `EventStore` trait (per-stream sequencing + hash chain + global cursor for tail-watching) — the choice is operational, not semantic. ADR-0012 has the full design rationale.

## Schema

```yaml
# project.yml

# Default — embedded SQLite at <project_root>/.zymi/events.db
# (omit the field, or:)
store: sqlite

# SQLite at a custom path:
store: sqlite:./var/zymi/events.db

# Networked Postgres:
store: postgres://user:pass@host:5432/dbname

# Templated, recommended for production:
store: ${env.DATABASE_URL}
```

Recognised forms:

| `store:` value | Backend | Where the events live |
|----------------|---------|----------------------|
| absent / empty / `sqlite` | SQLite | `<root>/.zymi/events.db` |
| `sqlite:<path>` | SQLite | `<path>` (absolute or relative to project root) |
| `postgres://…` or `postgresql://…` | Postgres | networked DB at the URL |

A typo in the URL surfaces as an error at startup, not after several green-path operations.

## SQLite (default)

**When:** single process, dev/laptop, demo, CI.

- Zero config.
- File at `.zymi/events.db`. Cursor state in `.zymi/connectors.db`.
- Per-stream sequence + hash chain enforced via `BEGIN IMMEDIATE` transactions.
- Other processes opening the same file will block; this is the right behaviour for a single-writer setup.

## Postgres

**When:** multiple `zymi serve` processes against a shared store, production deployment, infra ops want an external DB.

- Requires the `postgres` Cargo feature. The pip-installed wheel ships with it; if you build from source, rebuild with `--features postgres`.
- Connection pool: `tokio-postgres` + `deadpool`.
- Schema mirrors the SQLite events table (`BIGSERIAL id`, per-stream `sequence` and hash-chained `prev_hash`/`hash`).
- Per-stream `pg_advisory_xact_lock(hashtextextended(stream_id, 0))` serialises concurrent appends to the same stream — the hash chain stays intact even with two writers.
- Cursor table: `connector_cursors` on the same DB. Multi-process `zymi serve` sees one cursor — no double-fire on `update_id`.
- `zymi serve` startup logs the DB URL with the password redacted.

```yaml
# project.yml
store: postgres://zymi:${env.DB_PASSWORD}@db.internal:5432/zymi
```

```bash
# .env
DB_PASSWORD=...
```

## Migrating from SQLite to Postgres

The events table schema is identical, but events are not auto-migrated. Two approaches:

1. **Cold start:** new project, point `store:` at Postgres from day one.
2. **Replay:** export SQLite events as JSON (`zymi events --raw > events.jsonl`), bring up the Postgres-backed project on an empty DB, and re-publish events into the bus from the JSON. This rebuilds the hash chain on the new backend. Manual; no helper command yet.

## Operational notes

- **Hash chain integrity** can be verified at any time, on either backend, with `zymi verify [--stream <id>]`.
- **`zymi events`, `zymi runs`, `zymi observe`, `zymi resume`** all work identically on both backends — the runtime resolves the URL once at startup and threads it through every read path.
- **Connector cursors** are paired with the event store backend (sqlite events ↔ sqlite cursors, postgres events ↔ postgres cursors). You can't mix.
- **`zymi run` (one-shot)** uses the sync `open_store` path. Postgres requires the async path; sync `open_store` rejects it with a clear error. Use `zymi serve` or the Python SDK for Postgres-backed projects.

## Gotchas

- **Postgres test gating:** the Rust test suite includes Postgres tests gated on `ZYMI_POSTGRES_TEST_URL`. Without that env var, they're skipped — you don't need a DB to run the suite.
- **Schema migrations are forward-compatible only** today. Don't downgrade zymi-core after writing events with a newer schema.
- **`store: sqlite:<absolute path>`** writes outside the project tree. Useful for shared volumes; risky for project portability.

## See also

- [Project YAML](project-yaml.md) — `store:` placement
- [Events and replay](events-and-replay.md) — what the store records and how to inspect it
- [Connectors](connectors.md) — cursor persistence behaviour
- ADR-0012 (store backends)
