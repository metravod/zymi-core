# Services Layer: EventService Trait + LangFuse Integration

Date: 2026-04-05

## Context

zymi-core is an event-sourced agent engine. All state changes flow through the EventBus as events. External integrations (observability, notifications, analytics) need to react to these events without coupling to the core processing pipeline.

We need a generic service abstraction that subscribes to the event bus and performs side effects. LangFuse (LLM observability platform) is the first concrete integration.

## Decision

### EventService trait

A minimal async trait with three methods:

- `name()` — human-readable identifier for logging.
- `handle_event(&Event)` — process a single event; may buffer internally.
- `flush()` — drain internal buffers on shutdown.

Services do NOT filter events at the trait level. Each service decides internally which events it cares about (via pattern match returning `None` for irrelevant events).

### ServiceRunner

Manages service lifecycles:

- Each service gets its own EventBus subscription (per-service backpressure, no central routing bottleneck).
- Background tokio task per service with `select!` on event channel + shutdown signal.
- Graceful shutdown via `tokio::sync::watch<bool>` — drain remaining events, call `flush()`, await all tasks.

### LangFuse mapping

| Zymi Event | LangFuse Action |
|---|---|
| AgentProcessingStarted | trace-create |
| LlmCallStarted | generation-create |
| LlmCallCompleted | generation-update |
| ToolCallRequested | span-create |
| ToolCallCompleted | span-update |
| AgentProcessingCompleted | trace-update |

LLM call pairing uses `active_generation: HashMap<Uuid, String>` keyed on correlation_id (sequential within one agent cycle).

Batching: items accumulate in a buffer, flushed when `batch_size` reached or on shutdown. Sent to `POST /api/public/ingestion` with Basic auth.

### Configuration

`ServicesConfig` and `LangfuseConfig` structs live in `config/project.rs` (always compiled). The `services` module is behind `#[cfg(feature = "services")]`. YAML:

```yaml
services:
  langfuse:
    public_key: ${env.LANGFUSE_PUBLIC_KEY}
    secret_key: ${env.LANGFUSE_SECRET_KEY}
```

### Feature flag

`services = []` — no new dependencies (uses existing `reqwest`, `serde_json`, `async-trait`).

## Consequences

**Pros:**
- Clean separation: services are decoupled observers, not participants in the processing pipeline.
- Pattern is extensible: adding a new service = implement EventService + add config struct.
- No new dependencies; feature-gated so zero cost when unused.
- Per-service subscription avoids a central routing bottleneck.

**Cons:**
- Per-service subscription means N subscribers on the bus (N typically 1-3, negligible overhead).
- LLM call pairing relies on sequential assumption within a correlation — would need rework if parallel LLM calls per correlation are ever introduced.
- No retry logic on HTTP failures (acceptable for observability; events are in SQLite store for replay).
