# Python Layer as Event-Driven Components, Not SDK Wrapper

Date: 2026-04-04

## Context

zymi-core is a Rust crate. The product vision includes a Python layer for user-facing tools, extensions, and `zymi run` CLI. The question is how Python and Rust communicate: direct FFI calls (PyO3) or event-driven integration over the bus?

Decision prompted by Codex architecture review session (2026-04-04).

## Decision

Python components are **external event-driven participants** on the Rust event bus, not an SDK wrapper around Rust internals.

Communication pattern:
1. Rust orchestrator publishes an **intention/request event** to the bus.
2. Python component subscribes, reads the event, performs work.
3. Python publishes a **result event** back to the bus.
4. Rust continues orchestration by `correlation_id`.

Two classes of services:
- **Reactive** (fire-and-forget): memory indexing, summarization, observability, LangFuse projection. Subscribe and process asynchronously.
- **Request/response over bus**: token budget checks, approval-like control-plane decisions. Orchestrator waits for a correlated response event with a timeout.

PyO3 is still used, but only for **bridging core types** (`Event`, `EventStore`, `EventBus`) into Python — not for wrapping orchestration logic.

## Alternatives Considered

- **Pure PyO3 SDK**: Python calls Rust functions directly. Simpler initial implementation, but breaks the event-sourcing contract — Python-side actions bypass the event bus, losing audit trail and replay capability.
- **gRPC/HTTP between Rust and Python**: Over-engineered for a single-process tool. Adds networking complexity, serialization overhead, and deployment burden.
- **Pure Python reimplementation**: Loses the performance and safety guarantees of the Rust core. Event store, policy engine, and contract evaluation are better in Rust.

## Consequences

- **Unified integration contract**: tools, services, and user extensions all communicate the same way (events).
- **Full replayability**: every Python-side action is recorded as an event, enabling `zymi replay`.
- **Audit trail**: no hidden side channels between Rust and Python.
- **Extensibility**: users can write custom event-driven components in Python without touching Rust.
- **Latency trade-off**: request/response over bus adds overhead vs direct FFI. Acceptable for LLM-bound workloads (network latency >> bus overhead).
- **PyO3 scope is narrow**: bridge types and bus subscription, not business logic.
