# MCP Client Integration

Date: 2026-04-19

## Context

zymi-core supports two sources of tools today: agent-local declarative HTTP tools (ADR 0014) and Python-decorated tools (ADR 0007). Both require the tool author to either write Rust-templateable YAML or Python code. This is fine for bespoke project-specific tools but scales poorly against the realistic user ask: "I want my agent to interact with GitHub / Postgres / Notion / Slack / my file system / Playwright — please don't make me wire each one."

The Model Context Protocol (MCP, Anthropic-standardised) has accumulated a large ecosystem of ready-made servers for exactly these integrations. A zymi-core user who wants GitHub and Postgres tooling should not have to reimplement what `@modelcontextprotocol/server-github` and `@modelcontextprotocol/server-postgres` already provide.

Additionally, ADR 0021 chose to reuse MCP's stdio JSON-RPC transport as the wire format for our connector plugin protocol. Implementing an MCP client first gives us that transport implementation and validates it against a real third-party server corpus before we commit to running our own plugins on the same wire.

## Decision

### Scope of v1

Ship an MCP client covering the **tools** capability only, with **stdio** transport. Explicitly out of scope for v1: `resources`, `prompts`, sampling, HTTP+SSE transport. Each of these is a separate follow-up ADR once we see demand.

Rationale: tools are the overwhelming majority of the MCP ecosystem value today, stdio is the simpler transport to test and isolate, and narrowing v1 keeps the implementation bounded.

### YAML surface

A new top-level section registers MCP servers:

```yaml
mcp_servers:
  - name: github
    command: ["npx", "-y", "@modelcontextprotocol/server-github"]
    env:
      GITHUB_PERSONAL_ACCESS_TOKEN: ${env.GH_TOKEN}
    allow:
      - "create_issue"
      - "add_issue_comment"
    # deny: [...]           # mutually exclusive with allow
    restart:
      on_exit: true
      max_restarts: 3
      backoff_secs: [1, 5, 30]

  - name: postgres
    command: ["npx", "-y", "@modelcontextprotocol/server-postgres", "${env.DB_URL}"]
    allow: ["query"]        # read-only posture enforced by tool naming
```

Each server registers under `mcp_servers:` per ADR 0020's plugin-registry model. The registry entry produces an in-process proxy (`McpServerConnection`) that owns the subprocess, speaks JSON-RPC 2.0 over stdio, and exposes the server's tools to the existing `ToolCatalog`.

### Tool namespacing

Every tool imported from an MCP server is registered in `ToolCatalog` under a namespaced id: `mcp__<server_name>__<tool_name>`. Example: `mcp__github__create_issue`.

The original spec draft used `mcp://server/tool`, but the colon-and-slash form is rejected by both OpenAI and Anthropic tool-name validators (`^[a-zA-Z0-9_-]{1,64}$`). The double-underscore form is the smallest change that keeps a unique prefix for collision avoidance, policy matching, and observability while staying inside the LLM's name grammar.

Benefits:
- No collisions between our native tools and MCP tools, or between two MCP servers exposing `search`.
- Routing is trivial: catalog matches the `mcp__` prefix, splits on `__`, and dispatches `tools/call` against the owning `McpServerConnection`.
- Policy rules (ADR 0013) can match the `mcp__` prefix for blanket MCP-scoped rate limits / audit flags.
- `<server>` and `<tool>` from MCP must therefore not contain `__`. We validate this on registration; it has not appeared in any of the surveyed npm-distributed servers.

### Lifecycle

On runtime startup:

1. For each entry in `mcp_servers:`, spawn the subprocess with the configured `command` + `env`.
2. Send MCP `initialize` request with our protocol version and capability set (`tools` only).
3. On `initialize` response, call `tools/list` to enumerate available tools, honour `allow:` / `deny:` filters, and register each surviving tool in `ToolCatalog`.
4. Publish `McpServerConnected { server, tool_count }` on the event bus for observability (TUI / `zymi runs`).

During operation:
- Agent triggers a tool call with a namespaced id. Catalog routes to the owning `McpServerConnection`, which forwards `tools/call` over stdio, awaits the response (with per-call timeout), and returns the result to the caller.
- Errors from the MCP server are mapped to structured `ToolCallCompleted { is_error: true }` events. Stdout/stderr noise from the server process is logged at `debug`, never confused with protocol messages.

On shutdown:
- Send `shutdown` request, wait up to a small grace period, SIGTERM the subprocess, SIGKILL if it lingers.
- Publish `McpServerDisconnected { server, reason }`.

On server crash mid-operation:
- In-flight calls are failed with a structured error. Restart is attempted per `restart:` config; during the restart window, new calls against that server's tools fail fast rather than queueing.

### Security posture

MCP servers execute arbitrary third-party code (they are npm packages in the common case). Two defaults matter:

- **Explicit allow-list per server is recommended, not required.** We do not auto-register every tool an MCP server offers without the user opting-in via `allow:`. The first-run experience with `allow:` omitted is a startup warning: "server `github` exposes 32 tools; none registered. Specify `allow:` or `deny: []`".
- **Env isolation.** Only keys explicitly named under `env:` are forwarded. The parent process's environment is *not* inherited by default; this is the opposite of the usual subprocess convention but is the right default for a tool-loading surface.

These defaults are called out in the README and MCP section docs.

### Transport implementation

JSON-RPC 2.0 over stdio with **newline-delimited JSON** framing, per the MCP spec (one message per line; messages MUST NOT contain embedded newlines). This differs from the LSP convention (`Content-Length` headers) — MCP deliberately picked the simpler line-framed form.

- Outbound: serialize to a single JSON object (no embedded newlines), append `\n`, write to child stdin.
- Inbound: line-reader loop on child stdout, deserialize each complete line, dispatch to pending request by `id` or route as notification. Non-JSON lines (server logging noise) are logged at `debug` and skipped.

Implemented as a standalone module (`src/mcp/transport.rs`) reusable by the connector plugin protocol (ADR 0021). No third-party MCP SDK dependency in v1 — the spec surface we cover is small enough (handshake + `tools/list` + `tools/call` + `shutdown`) that a ~400 LOC hand-rolled client is the right call. A community SDK can be adopted later if its feature set (resources, prompts, SSE) becomes something we ship.

### Interaction with context-builder and policy

- Context builder (ADR 0016) treats MCP tools identically to native tools when composing system prompts — their `description` and JSON schema come from `tools/list` and are surfaced to the LLM unchanged.
- Policy engine (`src/policy.rs`) evaluates rules against the namespaced id. A rule with `tool_name_matches: "mcp://github/*"` works out of the box.
- Approvals (ADR 0022): an MCP tool marked as approval-required in policy triggers the same `ApprovalRequested` flow as any other tool. The user experience is uniform across native and MCP-provided tools.

## Alternatives Considered

- **Adopt an existing Rust MCP SDK as a dependency.** The community crates (`rmcp`, `mcp-rust-sdk`) are young, track the spec at varying pace, and pull in features we deliberately scope out of v1. The implementation cost to write what we need ourselves is low (~400 LOC) and avoids a volatile dependency on our critical path.
- **HTTP+SSE transport in v1.** Valuable for remote / multi-tenant scenarios but doubles the test matrix and introduces auth, TLS, and reconnection concerns that stdio sidesteps. Deferred to a follow-up.
- **Skip MCP, expand declarative HTTP tools (ADR 0014) to cover the same ground.** Would require users to hand-describe every tool's schema in YAML, re-implementing work MCP servers already do. Wastes the ecosystem's existing labour.
- **Expose MCP servers via the external-process plugin protocol (ADR 0021) rather than as a first-class `mcp_servers:` section.** Conceptually tempting — both run subprocesses — but MCP's shape (schemas come from `tools/list` at runtime, not from YAML) is different enough from our connector plugins that collapsing them would either weaken MCP support or bloat the connector protocol. Keeping them distinct at the YAML layer while sharing the stdio transport is the right tradeoff.

## Consequences

**Pros**

- Day-one access to the MCP server ecosystem: one `mcp_servers:` entry per integration, no Rust or Python code, no per-tool YAML schema authoring.
- Strongest amplifier of declarative-first positioning: "declare an MCP server, get N tools" is the cleanest demo of the "yaml is the product" philosophy for AI-engineer audiences.
- Uniform policy / approval / audit story across native and MCP tools — no second control plane.
- The stdio transport implementation is immediately reusable by the connector plugin protocol (ADR 0021), so the work pays off twice.

**Cons**

- Subprocess model has non-zero startup cost per server. Warm agents amortise this; cold starts (e.g. serverless-style invocations) pay the price per run. Acceptable for current design centre.
- MCP spec is still evolving. Committing to a specific protocol version in v1 means a version bump may require us to support two versions transiently. Mitigated by writing the transport to be version-aware and confining protocol-specific logic to a small module.
- npm-distributed MCP servers execute arbitrary JavaScript, and supply-chain risk is real. Our mitigations (explicit `allow:`, env isolation, audit logging) help but do not eliminate it. Users running untrusted servers should be running them behind sandboxing they control (containers, `nsjail`, etc.); we document this, we do not inline it.
- Failure modes of external processes (hung, leaking, logging to stderr in patterns that break framing) become zymi's support surface. Offset by bounded `restart:` config and clear observability events.

**Migration**

- No existing user config breaks. This ADR adds a new top-level section (`mcp_servers:`) and a new tool-id prefix (`mcp://`). Native declarative tools (ADR 0014) and Python-decorated tools (ADR 0007) continue to work unchanged and coexist with MCP tools in the same `ToolCatalog`.
- `src/mcp/` is a new module. No existing `src/` tree is disturbed beyond the `ToolCatalog` gaining an MCP-backed tool variant.
