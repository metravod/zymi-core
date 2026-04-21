# Declarative-First Framework: YAML Extension Surface and Plugin Registry

Date: 2026-04-18

## Context

The positioning of zymi-core has sharpened: it is a declarative framework in the spirit of dbt, where **YAML is the product** and the primary audience is Python / AI engineers who do not (and should not have to) write Rust. The value proposition is "describe your multi-agent system in YAML, run it". Rust is an implementation detail; Python bindings are an escape hatch for the ~5% of users who want programmatic embedding.

Today the codebase mixes two extension philosophies:

1. **Declarative** — `agents/`, `pipelines/`, `tools/` (ADR 0002, 0014) are already configured in YAML via `type:`-dispatched handlers.
2. **Code-level** — `EventService` trait (ADR 0010) assumes callers build a Rust binary, `WebhookApprovalHandler` (ADR 0009) is a concrete Rust struct wired in CLI code, Langfuse is a single hardcoded `if let Some(langfuse_config)` branch in `services::start_configured_services`. None of these are reachable from YAML alone, and `start_configured_services` is never actually invoked — the services layer as shipped is dead code.

At the same time, the set of things users want to declare keeps growing: inbound connectors (Telegram, Slack, HTTP webhooks, cron), outbound sinks (reply-to-user, notifications, file write), observability exporters (Langfuse, OTLP), human approval channels (terminal, Slack, HTTP), and tool sources (in-repo declarative HTTP tools, MCP servers). Each of these needs the same thing: "declare the component in YAML, point to an implementation by `type:`, pass config, run".

Rather than ship a bespoke abstraction per category (services-trait, approval-trait, connector-trait, tool-registry, MCP-registry), we codify a single **plugin registry pattern** and use it uniformly.

## Decision

### Top-level YAML surface

`project.yml` exposes these top-level sections. Each holds a list of typed plugin instances:

```yaml
agents:          [...]   # existing (ADR 0002)
pipelines:       [...]   # existing (ADR 0002, 0011)
tools:           [...]   # existing (ADR 0014) — declarative HTTP tools
mcp_servers:     [...]   # new (ADR 0023) — external tool providers via MCP
connectors:      [...]   # new (ADR 0021) — inbound event sources
outputs:         [...]   # new (ADR 0021) — outbound sinks on bus events
services:        [...]   # revised — observability/analytics/cost-accounting exporters
approvals:       [...]   # revised (ADR 0022) — human-in-the-loop channels
evals:           [...]   # existing (ADR 0019)
```

Every item carries a `type:` discriminator and component-specific config:

```yaml
services:
  - type: langfuse
    name: prod_traces
    public_key: ${env.LANGFUSE_PUBLIC_KEY}
    secret_key: ${env.LANGFUSE_SECRET_KEY}

connectors:
  - type: http_inbound
    name: telegram_hook
    port: 8080
    path: /telegram
    extract: { stream_id: "$.message.chat.id", content: "$.message.text" }

approvals:
  - type: slack
    name: ops_channel
    channel: "#agent-approvals"
    webhook_url: ${env.SLACK_WEBHOOK}
```

### Plugin registry pattern

One generic mechanism serves every pluggable section:

1. **Trait per category** — `EventService`, `InboundConnector`, `OutboundSink`, `ApprovalChannel`, `ToolProvider`. Each is a narrow async trait with a small method set (typically `start`, `handle_event`/`publish`, `shutdown`).
2. **Registry keyed by `type:`** — each category has a `Registry<dyn CategoryTrait>` built at startup by walking the YAML and dispatching on `type`. Adding an implementation = register one constructor.
3. **Feature-gated implementations** — non-trivial impls live behind feature flags (e.g. `service-langfuse`, `connector-http`) so ring-0 builds stay small. Core ships a curated default set; long-tail integrations come via the plugin protocol (ADR 0021).
4. **YAML is the stable public contract** — `type:` names, config field names, and semantics are versioned via `schema_version:` in `project.yml`. Internal Rust traits are free to evolve without breaking users.

### What stability means here

The framework's semver contract is about **YAML shape**, not Rust API:

- Adding a new `type:` or optional field → minor bump.
- Removing/renaming a `type:` or required field → major bump.
- Rust trait changes (adding methods, changing signatures) → internal, non-breaking to YAML users.

This intentionally inverts the usual library contract, because our users are not Rust callers. Rust consumers are a minority; for them we mark `EventKind` and other cross-plugin types `#[non_exhaustive]` so internal refactors don't cascade, but the primary contract we guard is the YAML schema.

### Two-layer escape hatch

Not every integration fits declarative config. Two hatches:

1. **Generic HTTP plugin** (ADR 0021) — covers most REST / webhook-based integrations with a single implementation driven entirely by YAML. The first line of defence against "we need a connector for X".
2. **External-process plugin protocol** (ADR 0021) — when HTTP isn't enough (MQTT, IMAP, Kafka, WebSocket), users run a subprocess plugin in any language and register it via `type: external` with `command: [...]`. Core doesn't need to ship these integrations.

Python bindings remain a third, lower-priority hatch for users who want to embed zymi programmatically — relevant for a minority of use cases, not the main path.

## Consequences

**Pros**

- One mental model for users: "declare in YAML, pick `type:`, configure". Same shape across services, connectors, approvals, tools, MCP.
- One mental model for contributors: add a category implementation = write an impl + register a constructor keyed by `type:`. No new frameworks per category.
- Feature-gating per implementation keeps the default binary small; long-tail integrations don't pollute core.
- Inverting the semver axis (YAML schema stable, Rust internals fluid) matches the actual audience.
- Failure mode of ADR 0010 (abstraction shipped but never wired, no reachable-from-YAML path) is ruled out structurally: a new category is not "done" until there is a registry wired into runtime startup and a YAML section that drives it.

**Cons**

- Existing code diverges from this pattern (services, approvals, webhook) and needs retrofitting. Mechanical, but non-trivial: ADRs 0021 / 0022 describe the concrete migrations.
- A stable YAML contract is a maintenance commitment. Once `type: langfuse` is public, we cannot silently rename config fields. Requires a visible deprecation policy in docs.
- Per-category traits will grow subtle differences (start/stop lifecycles are not identical across services vs connectors vs approvals). The uniformity is at the YAML / registry layer, not at the trait level — pitched honestly in docs to avoid over-promising.

**Supersession**

- ADR 0010 (services layer) is revised by this ADR: the `EventService` trait survives, but the single hardcoded `if let Some(langfuse_config)` dispatch is replaced by a typed registry, wired into runtime startup, and Langfuse moves behind a `service-langfuse` feature.
- ADR 0009 (webhook approval) is superseded by ADR 0022, which reframes approvals as an event-sourced plugin category under this registry model.

**Immediate follow-ups**

- ADR 0021: connectors (`connectors:`, `outputs:`) and the external-process plugin protocol.
- ADR 0022: event-sourced approvals under `approvals:`.
- ADR 0023: MCP client as the first non-trivial tool-provider plugin (`mcp_servers:`).

## Implementation progress

**2026-04-21 (v0.3 groundwork, P2):**

- Stdio JSON-RPC 2.0 transport extracted from `src/mcp/transport.rs` into `src/plugin/transport.rs`. `src/mcp` re-exports `Transport`/`TransportError` so existing downstream modules (`src/runtime/mod.rs`) keep compiling. The external-process plugin protocol (ADR 0021) will share this module.
- Generic `PluginRegistry<T>` + `PluginBuilder<T>` trait landed in `src/plugin/registry.rs`. Construction-side only: `type:`-keyed dispatch, duplicate-name detection, typed error surface. No category traits or concrete builders yet — P3 (`InboundConnector`, `http_inbound`) is the first real consumer.
- `mcp_servers:` **not migrated** onto the generic registry. Every entry today is an MCP stdio server — there is no `type:` discriminator to dispatch on, and introducing `type: stdio` with a default would be pattern-theatre. The live-connection pool (`McpRegistry`) remains category-specific; only the construction-side gets uniformed, and `mcp_servers:` will plug in once it actually gains a second `type:` (e.g. HTTP+SSE in a follow-up ADR).
- Service layer (ADR 0010, Langfuse dispatch): **intentionally deferred** and explicitly off-roadmap for new work. Existing code stays as-is behind the `services` feature; observability is owned by the native event store / TUI, not third-party exporters. `services:` remains a valid plugin category slot for residual integrations that don't fit `tools:` / `connectors:` / `outputs:` / `approvals:` / `mcp_servers:`, but Langfuse is not the template for future work.
- `#[non_exhaustive]` on `EventKind`, `EventStoreError`, `StoreBackend`, and `TransportError`. Protects external Rust consumers when P3/P5/P10 add variants. Stability contract is on the YAML schema, not the Rust API.
- `schema_version` field added to `project.yml` (`SCHEMA_VERSION = "1"`). Tolerates absence for backwards compat; new projects scaffolded by `zymi init` should set it explicitly once P3 scaffolding lands.
