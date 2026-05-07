# Generic HTTP Connectors and External Plugin Protocol

Date: 2026-04-19

## Implementation status (2026-05-02)

- **Slice 1 — `http_inbound`**: shipped. axum-based webhook receiver with bearer / HMAC-SHA256 auth (header + optional prefix, constant-time compare), `serde_json_path`-based JSONPath extract into a flat context, synthesises `UserMessageReceived`, and (when `pipeline:` is set) additionally publishes `PipelineRequested { inputs: {pipeline_input: content} }` so `zymi serve <pipeline>` picks it up without extra glue.
- **Slice 2 — `http_post`**: shipped. Subscribes to the bus, filters on `on: [EventKind]` (accepts both `ResponseReady` and `response_ready`), renders URL / headers / `body_template` with `minijinja` over a flat `event.*` context, retries with a `backoff_secs: [...]` list, gives up without retry on 4xx.
- **Slice 3 — `http_poll`**: shipped. Tokio-timer loop with per-connector cursor persisted to `.zymi/connectors.db` (sibling SQLite file, not the event store — cursors aren't part of the event stream). Optional `filter:` block with JSONPath → `one_of` / `equals` rules (gates the Telegram bot to a known username list). Same `pipeline:` / `pipeline_input` semantics as `http_inbound`.
- **Slice 4 — `cron`**: shipped (v0.4 sprint 2), reworked during v0.6 polish (2026-05-07). Now takes a standard cron expression (`cron: "*/15 * * * *"`, 5 or 6 fields, local timezone, `croner` crate); `every_secs:` and `at:` were dropped. New `inputs:` map lets a single tick fan out into a parameterised pipeline (`PipelineRequested { inputs }` with multiple keys), so the same pipeline runs both manually with `zymi run <name> -i k=v -i k=v` and on schedule. Legacy `content:` + `pipeline_input:` form kept as a back-compat shim and routes through the same `inputs` map under the hood. Synthesises `UserMessageReceived` per tick. No network, ~350 LOC including tests.
- **Slice 5 — `file_read` / `file_append`**: shipped (v0.4 sprint 2). `file_read` reads `path:` once at startup, `mode: lines` (default, one event per non-empty line) or `mode: whole` (single event with full body) — useful as a batch driver for evals-lite. `file_append` is a bus-subscriber sink: `on:` filter, MiniJinja `template:`, append-write to `path:` (parent dir must exist; we don't auto-create). No tail/cursor — compose `cron` + `file_read` if you need re-reads.
- **Slice 6 — `stdin` / `stdout`**: shipped (v0.4 sprint 2). `stdin` reads lines from process STDIN until EOF, one event per non-empty line; `stdout` is the symmetric outbound — render a MiniJinja template per matching event and write to STDOUT (newline-appended by default). Enables `cat prompts.txt | zymi run agent | downstream_tool` and similar pipe compositions. Deliberately tiny — no buffering / formatting flags.
- **Slice 7 — external-process plugin protocol**: still deferred. Moved to backlog *without a release anchor* per 2026-05-02 reprioritisation: the "community wants language-agnostic plugins" hypothesis is unconfirmed and we're not building speculatively. Pulled back in only when a concrete user request arrives.
- **CLI `.env` auto-load**: shipped (v0.4 sprint 2). `dispatch` calls `dotenvy::from_path(<dir>/.env)` for the resolved subcommand's `--dir` (when present) and CWD before any subcommand runs. Existing process env vars always win — `.env` only fills holes — and a malformed file logs a warning rather than aborting. Closes the 0.3 QoL gap where users had to remember `set -a; source .env; set +a` before every invocation.
- **Contract bridge**: `run_pipeline::handle` now publishes `ResponseReady { conversation_id, content }` on every successful pipeline with a `final_output`, so declarative `outputs:` see a stable public event regardless of internal step shape.
- **Wiring**: `RuntimeBuilder::build_async` spawns every `connectors:` / `outputs:` entry alongside MCP; handles live on `Runtime.plugin_handles` and are drained by `Runtime::shutdown_connectors()`, called from both `zymi run` and `zymi serve` on exit. Shutdown cancels each task's token, then awaits its `JoinHandle` with a 5 s per-handle deadline — inside that budget axum (`http_inbound`) drains accepted requests, `http_post` finishes the current attempt, `http_poll` completes the current tick and persists its cursor. A handle that blows past the deadline is logged and left detached rather than wedging process exit.
- **Rate-limit handling**: `http_post` now retries 429 separately from other 4xx, honours `Retry-After` (header and Telegram's `parameters.retry_after` body field), and clamps server hints to 300 s so a pathological `Retry-After: 86400` can't park a sink for a day.
- **Scaffold**: `zymi init --example telegram` produces a ready-to-run Telegram chat agent (long-polling, username filter, reply via http_post) — primary demo for the declarative-extension narrative.

## Context

ADR 0020 introduces `connectors:` and `outputs:` as top-level YAML sections and commits the project to declarative-first, "batteries with plugin escape hatches" philosophy. This ADR decides *how* to ship those batteries without hand-writing a Rust implementation for every conceivable integration (Telegram, Slack, Discord, Twilio, GitHub webhooks, MQTT, IMAP, Kafka, WebSocket…).

Two observations:

1. **Most modern SaaS integrations are HTTP-shaped.** Webhook inbound, REST outbound, occasionally long-polling for services without webhook support. One well-designed generic HTTP connector, driven entirely by YAML, covers ~60-70% of realistic user asks without any per-integration Rust code.
2. **The remaining ~30%** (MQTT, Kafka, IMAP/SMTP, WebSocket, gRPC, persistent-connection brokers) do not cleanly fit "HTTP + JSONPath". For them we need an extension mechanism that does not require core to grow a new Rust feature-crate per protocol and — critically — does not force our Python / AI-engineer audience to write Rust at all.

Goal: maximise the "works via YAML alone" surface, and for the rest provide a plugin protocol in which community/users can write connectors in any language.

## Decision

### Layer 1 — Generic HTTP connectors (ship in core)

Three `type:`s dispatch to one generic implementation driven by YAML:

#### `type: http_inbound` — webhook receiver

```yaml
connectors:
  - type: http_inbound
    name: telegram_hook
    port: 8080
    path: /telegram
    auth:
      hmac_header: "X-Telegram-Bot-Api-Secret-Token"
      secret: ${env.TG_SECRET}
    extract:
      stream_id:  "$.message.chat.id"
      user:       "$.message.from.username"
      content:    "$.message.text"
      # optional: correlation_id, attachments, etc.
    publishes: UserMessageReceived    # event kind emitted on every valid POST
```

One axum server binds `port`/`path`, validates `auth` (bearer / HMAC / none), runs JSONPath extraction on the body, synthesises a typed event, publishes to the bus. Invalid bodies → 400 with a structured error, no event published. No Rust written by the user.

#### `type: http_poll` — long-polling for services without webhooks

```yaml
connectors:
  - type: http_poll
    name: gmail_inbox
    url: "https://gmail.googleapis.com/.../messages"
    method: GET
    interval_secs: 10
    headers: { Authorization: "Bearer ${env.GMAIL_TOKEN}" }
    cursor:
      param: "pageToken"
      from_response: "$.nextPageToken"
      persist: true     # cursor survives restarts in sqlite
    extract:
      items:      "$.messages[*]"
      stream_id:  "$.id"
      content:    "$.snippet"
    publishes: UserMessageReceived
```

A Tokio timer loop hits the endpoint, applies the extractor per item, publishes each. Cursor state is persisted in the event store (same sqlite file as events) keyed by connector `name`, so restart safety is free.

#### `type: http_post` — outbound sink on bus events

```yaml
outputs:
  - type: http_post
    name: telegram_reply
    on: [ResponseReady]               # subscribe filter over EventKind
    url: "https://api.telegram.org/bot${env.TG_TOKEN}/sendMessage"
    method: POST
    headers: { "Content-Type": "application/json" }
    body_template: |
      {
        "chat_id": "{{ event.stream_id }}",
        "text": "{{ event.content }}"
      }
    retry:
      attempts: 3
      backoff_secs: [1, 5, 30]
```

A bus subscriber filtered by `on:`, templating via Handlebars / MiniJinja over the event payload, reqwest POST. Retries on 5xx / network errors, DLQ-style log on final failure (we already have sqlite to persist "outbound failed" events if needed later).

#### Shared primitives

All three share:
- JSONPath extractor (library: `jsonpath-rust` or equivalent).
- Template engine for outbound body/URL (one choice: `minijinja` — small, Python-familiar syntax).
- Auth adapters (bearer, HMAC-SHA256, basic, none). Extensible by constant-time string match on `auth.type`.
- Per-connector named health metric exposed via the observability TUI (ADR 0017).

Estimated implementation budget: ~1000-1200 LOC Rust + YAML schema + integration tests against httpbin / local axum fixture. One engineer-week.

### Layer 2 — Narrowly native connectors (ship in core, small set)

Integrations that do not fit "HTTP + JSONPath" but are universally useful and not worth pushing onto users:

- `type: cron` — timer-driven synthetic `UserMessageReceived` events with a fixed `content`. No network involved. ~100 LOC.
- `type: file_append` / `type: file_read` — local file I/O sinks / sources. ~150 LOC each.
- `type: stdin` / `type: stdout` — pipe-mode for scripting / `zymi run | downstream_tool`. Trivial.

We deliberately resist the urge to ship `type: telegram`, `type: slack`, `type: discord` as distinct types. Webhook-capable SaaS flows through `http_inbound`/`http_post`; listing every service name in core invites an unbounded maintenance surface with no architectural benefit.

### Layer 3 — External-process plugin protocol (the escape hatch)

For integrations that need persistent connections, binary protocols, or libraries we do not want to vendor (MQTT, IMAP, Kafka, WebSocket, gRPC, …), users register a subprocess plugin:

```yaml
connectors:
  - type: external
    name: my_mqtt
    command: ["python", "-m", "my_mqtt_connector"]
    env: { MQTT_BROKER: "tcp://broker:1883" }
    transport: stdio         # or http+sse for remote plugins
```

The plugin process speaks a small JSON-RPC protocol over the chosen transport. Shape (informal):

| RPC method             | Direction       | Purpose                                           |
|------------------------|-----------------|---------------------------------------------------|
| `initialize`           | core → plugin   | capability handshake, config                      |
| `shutdown`             | core → plugin   | graceful termination                              |
| `publish_event`        | plugin → core   | plugin emits an event to the bus                  |
| `subscribe`            | plugin → core   | plugin requests a filtered event stream           |
| `event` (notification) | core → plugin   | delivers one event matching an active subscription|

**Transport choice: reuse MCP's stdio transport.** MCP already defines JSON-RPC 2.0 over stdio with the framing and error conventions we need, and ADR 0023 will bring an MCP client into the project anyway. Adding our own `publish_event` / `subscribe` methods on top of the same wire format means:

- One transport implementation serves both tools (ADR 0023) and connectors (this ADR).
- Community authors who already know MCP have a near-zero learning curve.
- Python plugin authors can use the existing `mcp` PyPI SDK, extend it with our methods, and ship.

We do not claim these methods are "MCP-standard". We reuse the *transport*, not the spec. Where methods overlap with MCP (e.g. `initialize`), we stay wire-compatible so an MCP-only process can be introspected but will simply report no capability for our extension methods.

Reference Python plugin (ships in `examples/`): ~50 LOC skeleton showing "subscribe to `ResponseReady`, format, send over MQTT". Primary documentation vector for users.

### Registry and lifecycle

Per ADR 0020:

- `connectors:` items register in a `Registry<dyn InboundConnector>` keyed by `name`, dispatched by `type`.
- `outputs:` items register in a `Registry<dyn OutboundSink>` likewise.
- External plugins are normal registry entries whose implementation is an in-process proxy that forwards to the subprocess over JSON-RPC. From the runtime's POV, they are indistinguishable from native `type:` implementations.
- Startup wires the registries via the runtime builder; no `if let Some(...)` hardcoding. Shutdown calls `stop()` on each in reverse registration order.

### Event-side constraints

To keep outputs stable across minor versions:

- Every event kind that outputs and connectors can filter on (`on: [...]`) is listed in docs as "stable" and covered by schema tests.
- Template context exposed to `body_template` / extraction is an explicit, flat dictionary (`event.kind`, `event.stream_id`, `event.timestamp`, …), not "serialize the whole internal Event struct". This decouples YAML contract from Rust struct layout.

## Alternatives Considered

- **Hand-writing `type: telegram`, `type: slack`, …** — rejected. Unbounded maintenance, each API has its own auth/rate-limit quirks, no architectural leverage. Every one we ship is a crate we are on the hook for.
- **Lua / WASM embedded scripting** — a legitimate alternative to JSONPath + templating for more complex transformations. Rejected for v1: higher implementation cost, unfamiliar to Python-first audience, and the 80% case is well-served by JSONPath + MiniJinja. Revisit if/when users hit the ceiling.
- **Fully synchronous in-process plugins (dylib loading)** — rejected. Forces all plugin authors to match our Rust ABI, blows up cross-platform packaging, and precludes Python-native plugins. External process is slower but vastly simpler and language-agnostic.
- **Separate transport from MCP's** — rejected. We would end up shipping two nearly-identical JSON-RPC-over-stdio implementations. Consolidating around MCP's transport is a free win.

## Consequences

**Pros**

- Ships a genuinely useful day-1 feature set (`http_inbound` / `http_poll` / `http_post` + `cron` + file I/O) with a small, finite implementation budget (~1500 LOC total).
- Coverage of the typical integration long-tail (MQTT, IMAP, …) is delegated to users/community in their native language — zero core maintenance burden per integration.
- Reusing MCP transport means the same muscle invested in ADR 0023 (MCP client) pays for the plugin protocol too.
- YAML templates + extractors give a clean debugging story: users can see and test the shape of their inbound/outbound without recompilation.

**Cons**

- External-process plugins carry IPC overhead and per-process resource cost. For high-throughput use cases (thousands of events/sec) users may need to batch via a single plugin process. Acceptable for current design centre (human-paced agents).
- JSONPath + MiniJinja is expressive but not unboundedly so. Edge cases (signed request verification schemes that require reconstructing a canonical string, OAuth2 flows with token refresh) may exceed what declarative config can cleanly express; such cases fall through to the plugin protocol.
- The "one HTTP connector, three `type:`s" design ties those three types together — a bug in the shared extractor affects all three. Offset by: shared code is exercised by all three connector's tests.
- Growth of `EventKind` affects the `on:` filter surface. Mitigation: documented stable subset (per ADR 0020, `#[non_exhaustive]` on `EventKind`).

**Migration / wiring**

- `src/events/connector.rs` (`EventDrivenConnector`) remains as the low-level Rust helper; the new YAML `connectors:` layer sits above it.
- `src/services/start_configured_services` pattern is replaced by generic `start_configured_connectors` / `start_configured_outputs` / … dispatchers driven by the plugin registry. Old code in `src/services/` either migrates into the new registry as `services:` entries (ADR 0022 finalises this) or is removed.
- `src/webhook.rs` (approval-specific) is untouched by this ADR; ADR 0022 handles it.
