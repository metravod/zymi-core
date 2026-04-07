# Cross-Process Event Delivery via SQLite Tail Polling

Date: 2026-04-07

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
