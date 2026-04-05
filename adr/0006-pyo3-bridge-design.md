# PyO3 Bridge: Narrow Type Bridging, Not SDK Wrapper

Date: 2026-04-04

## Context

ADR-0005 established that Python components are event-driven participants on the Rust bus. PyO3 is used only for bridging core types — not wrapping orchestration logic. This ADR documents the bridge implementation decisions.

## Decision

The PyO3 bridge lives behind an optional `python` Cargo feature and exposes four classes:

| Python class   | Rust type          | Purpose                                   |
|----------------|--------------------|--------------------------------------------|
| `Event`        | `events::Event`    | Create, inspect, serialize/deserialize events |
| `EventStore`   | `SqliteEventStore` | Open/create SQLite store, read/write events   |
| `EventBus`     | `EventBus`         | Publish events, create subscriptions          |
| `Subscription` | `mpsc::Receiver`   | Blocking/non-blocking receive, Python iteration |

Key design choices:

1. **EventKind as JSON dict** — Rust enums with data don't map cleanly to Python. EventKind is serialized via serde to `{"type": "...", "data": {...}}` and converted to/from Python dicts using `json.dumps`/`json.loads`. This leverages the existing `#[serde(tag = "type", content = "data")]` format.

2. **Shared tokio runtime** — A single `OnceLock<Runtime>` is created lazily on first use. All async Rust operations (`append`, `read_stream`, `publish`, `recv`) call `runtime().block_on(...)` with the GIL released via `py.allow_threads()`.

3. **GIL release on all blocking calls** — Every method that touches the store or bus releases the GIL, allowing other Python threads to make progress.

4. **Subscription supports `__iter__`** — Python code can `for event in subscription:` to process events in a loop. `recv(timeout_secs=...)` provides timeout support.

5. **Build strategy** — `crate-type = ["rlib"]` in Cargo.toml; maturin adds `cdylib` automatically when building the Python wheel. The `extension-module` feature on PyO3 prevents linking against libpython, which is correct for extension modules but means `cargo test --features python` can't link. Regular tests run without the feature; Python integration tests run after `maturin develop`.

## Alternatives Considered

- **pyo3-asyncio for native async Python**: Adds dependency complexity and ties to a specific Python async framework. The blocking bridge with GIL release is simpler and works with any Python concurrency model (threads, asyncio via `run_in_executor`, etc.).
- **Exposing ContractEngine/Orchestrator to Python**: ADR-0005 explicitly scopes PyO3 to core types + bus. Orchestration stays in Rust; Python participates via events.
- **pythonize crate for dict conversion**: Would avoid the `json.dumps`/`json.loads` round-trip, but adds another dependency for marginal gain. JSON via Python stdlib is zero-dependency and debuggable.

## Consequences

- Python components can create events, publish to the bus, subscribe, and read from the store — everything needed for event-driven participation.
- No Python dependency for pure Rust consumers (`python` feature is optional).
- ContractEngine, Orchestrator, and Projections remain Rust-only — Python tools interact through the event bus, not direct FFI calls.
- Testing split: Rust tests via `cargo test`, Python integration tests via `maturin develop` + pytest.
