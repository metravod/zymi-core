# Cross-Process Event Delivery via SQLite Tail Polling

Date: 2026-04-07

## Backend roadmap (2026-05-02)

- **SQLite (rusqlite, default)**: shipped in v0.3. Embedded, file-based, the zero-config path.
- **Postgres**: shipped v0.4 sprint 3 (P10). Behind the `postgres` Cargo feature. Tokio-postgres + deadpool pool, schema mirrors the SQLite events table (events + per-stream sequence + hash chain + global `BIGSERIAL id`). Per-stream sequence assignment + hash-chain reads run inside a transaction guarded by `pg_advisory_xact_lock(hashtextextended(stream_id, 0))`, so two concurrent appenders to the same stream serialise without external coordination. `StoreTailWatcher` polls `id > ?` exactly as it does on SQLite — no protocol change required. Project config opts in via `store: postgres://…` (templated, so `store: ${env.DATABASE_URL}` is the canonical form). Cross-process delivery validated by an integration test gated on `ZYMI_POSTGRES_TEST_URL` (skipped when unset, so plain `cargo test` doesn't require a database).
- **Sync vs async open**: `factory::open_store` (sync) accepts only SQLite; Postgres requires `factory::open_store_async`. `RuntimeBuilder::build_async` and CLI subcommands (`events`, `runs`, `observe`, `resume`, `serve`) thread through the async path automatically. The sync `RuntimeBuilder::build` returns a clear error if `project.store` resolves to Postgres rather than panicking via a nested-runtime trick.
- **Cursor store pairing**: `http_poll`'s per-connector cursor (Telegram `update_id`, Gmail `pageToken`, …) lives in a separate small store, but its backend is **paired with the event-store backend** (sqlite events ↔ sqlite cursors at `.zymi/connectors.db`; postgres events ↔ postgres cursors in the same DB's `connector_cursors` table). Pairing happens automatically in `RuntimeBuilder::build_async` via `connectors::cursor_store::open_cursor_store(&backend, &project_root)`; the user picks one backend through `store:`. Without this pairing, two `zymi serve` processes against shared Postgres would both keep local `.zymi/connectors.db` files and double-fire on every `update_id`. Cursors are still **deliberately separate from the event log** — they're control-plane state, not part of the audit chain.

- **libSQL (deferred → v0.6 P12)**: not a like-for-like backend swap — pulled out of v0.4 because it gives nothing over SQLite on the v0.4 goal. The real value is libSQL's embedded vector index (`F32_BLOB` + `vector_top_k`), which we'll pick up alongside a concrete consumer: a semantic memory layer for inter-agent context (new `memory:` YAML section + `query:` step type, separate ADR). Switching the embedded backend without that consumer = moving the foundation without picking the fruit.

## Context

ADR-0005 declared Python components as **cross-process participants on the
event bus** rather than an SDK wrapping the Rust core. The implementation,
however, only delivered events inside a single process: `EventBus` is built
on tokio mpsc channels, so a Django/Celery/script process appending events
directly to the SQLite store stayed invisible to a long-running Rust process
subscribed to the same store.

This blocked the canonical multi-process integration we want — Django
publishes a `PipelineRequested` event, a long-lived `zymi serve <pipeline>`
process picks it up, runs the pipeline, and publishes
`PipelineCompleted` for the client to await.

We also wanted backend isolation: future migration to libSQL or Postgres
should be a single-file change, not a cross-crate refactor.

## Decision

1. **SQLite remains the single source of truth.** No second IPC channel,
   no notifier process.

2. **Polling tail watcher.** A new `StoreTailWatcher` task polls the store
   on a configurable interval (~100ms by default), reads new rows by their
   global cursor, and fans them out via a new `EventBus::redeliver` method
   that **only** does in-process fanout — it never re-appends, so the
   per-stream hash chain stays intact and the global cursor remains
   monotonic.

3. **`EventStore` trait gains two methods**: `tail(after_global_seq, limit)`
   and `current_global_seq()`. `read_all` is now a thin wrapper over
   `tail`. The autoincrement `id` column already gave us a stable global
   cursor; the new methods just make the contract explicit and reusable
   for the watcher.

4. **`zymi serve <pipeline>` subcommand** spawns the watcher, subscribes
   to `PipelineRequested` events targeted at the named pipeline, runs the
   pipeline via the new `engine::run_pipeline_for_request` (which accepts
   a shared `bus`/`store`/`correlation_id`), and publishes
   `PipelineCompleted` with the same correlation_id.

5. **Backend isolation via factory.** A new `events::store::factory::open_store`
   returns `Arc<dyn EventStore>` for a `StoreBackend` enum. All call
   sites (CLI commands, engine, PyO3 store wrapper) now depend on
   `dyn EventStore`. Adding a libSQL or Postgres backend is a matter of
   creating a sibling file under `events/store/` and adding a variant to
   `StoreBackend` — no churn elsewhere.

## Alternatives Considered

- **`sqlite3_update_hook`** — fires only on the connection that wrote the
  row. Useless for cross-process delivery.
- **Postgres LISTEN/NOTIFY** — requires Postgres. Out of scope; will be
  revisited when adding a Postgres backend.
- **Unix domain socket nudge** — adds a second channel and breaks down on
  multi-host deployments.
- **gRPC sidecar / dedicated broker** — over-engineered for the LLM
  workload latency budget.

## Consequences

- **Latency**: cross-process delivery is bounded by the poll interval
  (~100ms by default). Acceptable for LLM-bound workloads where each
  step already costs hundreds of ms to seconds.
- **Operational simplicity**: single SQLite file is still the only thing
  to back up, lock, or replicate.
- **Hash chain safety**: `redeliver` is a fan-out-only path; the chain
  invariants are unchanged.
- **Backend-ready**: future libSQL / Postgres migration is a contained
  change. Tracked separately as ADR-0013.
- **Out of scope for this iteration**: vector search (waits on libSQL),
  approval handler integration in serve mode, streaming responses through
  the service contract, and authn/authz for cross-process writers.
