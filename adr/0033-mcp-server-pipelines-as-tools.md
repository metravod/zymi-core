# MCP server: pipelines as tools (with SEP-1686 Tasks for long pipelines)

Date: 2026-05-22

Status: Proposed.

## Context

ADR-0023 shipped MCP **client** support — zymi consumes third-party MCP servers (github, postgres, playwright, …) as tool sources. The mirror direction was deliberately out of scope: there is no way for an external MCP client (Claude Desktop, Cursor, Windsurf, OpenHands, LangGraph via `langchain-mcp-adapters`, ChatGPT Apps SDK, Gemini API) to **invoke a zymi pipeline as a tool**. ADR-0023 §"Scope of v1" called this out as deferred. This ADR revisits the deferral.

Two facts forced the revisit (research 2026-05-22):

1. **MCP is the universal agent-tool protocol in 2026.** ~97M monthly SDK downloads, ~81K stars, supported natively by Anthropic / OpenAI / Google / Microsoft / AWS and embedded in Claude Code, Cursor, Windsurf, Zed, VS Code, JetBrains AI, OpenAI Agents SDK, OpenHands, the Vercel AI SDK, and LangGraph (via `langchain-mcp-adapters`). OpenHands and LangGraph both consume MCP servers via short config — no per-runtime adapter code on either side. One MCP server reaches every major agent runtime simultaneously.

2. **The `zori` sibling project (autonomous ReAct agent runtime over zymi pipelines, parked in `~/Documents/pet_projects/zori/.drift`) loses most of its rationale against this surface.** ~90% of zori's value — "let an LLM agent pick + invoke zymi pipelines as tools" — collapses into shipping a server-side MCP implementation, without building a second runtime. Per-runtime native wrappers (LangChain `BaseTool`, CrewAI tool subclass, OpenAI Agents SDK function-tool, AutoGen function tool) are also dead surfaces: every major runtime already speaks MCP. They would add zero ergonomic gain over the MCP server and N parallel contracts to maintain.

The long-pipeline question (tool calls that take minutes to hours, breaking client-side timeouts) is independently solved by **SEP-1686 (MCP Tasks primitive)**, accepted into spec `2025-11-25` and being implemented across TypeScript / Python / Kotlin SDKs. SEP-1686 augments any request (including `tools/call`) with a `task: { ttl }` field, returns `CreateTaskResult` instead of the normal result, and exposes `tasks/get` / `tasks/result` / `tasks/cancel` / `tasks/list` for polling. Its `input_required` lifecycle state is a near-zero-cost mapping for ADR-0022 (event-sourced approvals): pipeline pauses awaiting approval → task transitions to `input_required` → human approves → task resumes.

## Decision

Ship `zymi mcp serve` — a server-side MCP implementation exposing zymi pipelines as MCP tools — co-existing with the existing client (ADR-0023). Five coordinated parts.

**1. Transport and wire.** stdio + newline-delimited JSON-RPC 2.0, same framing as the client (ADR-0023). The transport code already lives in `crate::plugin::transport` and is reused; the server is a new handler layer over the same wire. HTTP+SSE deferred to a follow-up ADR.

**2. Per-pipeline exposure (opt-in).** Pipelines declare exposure in YAML; default is not exposed:

```yaml
# pipelines/my_pipeline.yml
expose:
  mcp:
    name: my_pipeline          # optional; defaults to file stem
    mode: sync | async         # default: sync
    description: "..."         # default: pipeline description
```

Opt-in because `tools/list` cardinality matters: every exposed pipeline adds a tool descriptor to every connected agent's system prompt. Many pipelines (internal cron jobs, batch workflows) should never appear in agent tool catalogs.

**3. Sync mode (short pipelines).** One MCP tool per pipeline, named `<expose.mcp.name>`. Tool input schema auto-generated from pipeline `inputs:` (the existing backlog item *Pipeline-as-tool JSON Schema — auto-gen from pipelines/\*.yml* becomes a hard prerequisite for this ADR; it ships first). `tools/call` blocks until pipeline terminates. If the client included `_meta.progressToken`, the server emits `notifications/progress` at each step boundary with `progress`, `total = total_steps`, and a human-readable step name as `message`. Final response is the pipeline output payload; pipeline failure surfaces as MCP tool error with the structured failure.

**4. Async mode (long pipelines, SEP-1686 native).** When pipeline has `expose.mcp.mode: async`, declare `execution.taskSupport: required` in the tool definition. `tools/call` returns `CreateTaskResult` immediately with the zymi run_id as `taskId`. Lifecycle maps directly:

- `working` ← pipeline executing
- `input_required` ← pipeline halted on event-sourced approval (ADR-0022)
- `completed` ← `PipelineCompleted{success: true}`
- `failed` ← `PipelineCompleted{success: false}` or hard error
- `cancelled` ← client invoked `tasks/cancel`

`tasks/get` returns current state. `tasks/result` blocks until terminal and returns pipeline outputs (or the pending-approval payload as a structured prompt). `tasks/cancel` routes to the existing zymi cancellation path. Task TTL is server-configurable (default 1h) and bounded by the event store's retention.

**5. Capability negotiation and namespacing.**

- Server always declares `capabilities.tools`. Declares `capabilities.tasks.requests.tools.call` only if at least one exposed pipeline is async. If the connected client does not advertise tasks support, an async pipeline's `tools/call` refuses with a clear error pointing at the client's MCP spec version. **No fallback to tool-splitting** (`zymi_X_start` / `_status` / `_result` as three separate tools) — SEP-1686 explicitly classifies that pattern as anti-pattern and we accept its reasoning.
- CLI flags `--include <glob>` and `--exclude <glob>` filter the exposed pipeline set at boot. Default with no flags: all pipelines that have `expose: { mcp: … }` set.

This ADR partially supersedes ADR-0023 §"Scope of v1" — the "server-side out of scope" clause is lifted; the rest of ADR-0023 (transport, error handling, the tools-capability framing) carries over unchanged for the server direction.

## Consequences

- Pro: one piece of code (server) makes zymi pipelines callable from every major MCP client today. Distribution is a docs page of config snippets (Claude Desktop, Cursor, Windsurf, OpenHands `config.toml`, LangGraph `MultiServerMCPClient`), not a per-runtime adapter library.
- Pro: SEP-1686 alignment means the long-pipeline question is solved by the protocol, not by custom polling tools. Future client adoption of SEP-1686 (Python/TS/Kotlin SDKs shipping as of May 2026) lifts our async pipelines for free.
- Pro: `input_required` ↔ ADR-0022 approvals is a near-zero-cost mapping. Approvals become a first-class agent-loop primitive rather than a zymi-internal concept.
- Pro: zori-as-runtime ([[project_zori_parked]]) loses most of its rationale. The remaining differentiator (event-sourced agent control loop fully owned by zymi) can be re-evaluated against a concrete consumer pull later. Meanwhile the bridge between zymi and existing agent runtimes ships via this work.
- Con: opt-in exposure means existing pipelines do nothing until annotated. Acceptable trade-off — `tools/list` bloat is the worse failure mode.
- Con: SEP-1686 client adoption is still rolling out (SDKs implementing as of May 2026; Claude Desktop / Cursor / Windsurf shipped-support unconfirmed in published docs). Async-mode pipelines may not run in current clients until they ship the spec version `2025-11-25`. Documented as a client requirement in the README async section.
- Con: per-pipeline `inputs:` schema completeness becomes user-visible — LLM agents read tool descriptions to choose tools, so loose `inputs:` (untyped, no description) yield poor agent behavior. Some existing pipelines will need tightening when their authors decide to expose them. Acceptable; same pressure applies to any LLM-driven tool surface.
- Con: serving pipelines outside the project boundary changes the threat model. ADR-0022 approvals cover write paths; read-path exposure is new. Mitigated by default-deny exposure (opt-in per pipeline) plus `--include`/`--exclude` filters. Authentication of the MCP client itself is out of scope for stdio transport (the client is a child process the user trusts); HTTP+SSE follow-up will need to address this.
- Con: long-lived `zymi mcp serve` becomes a startup-latency liability — slow MCP servers are exactly what we complain about for third-party MCP servers we consume ([[project_mcp_eager_init_no_recovery]]). Startup-time discipline matters more once we are on the other side of that complaint.
- Future: `notifications/progress` could carry partial step outputs (intermediate observations) in async mode once a consumer asks for it. v1 sticks with terminal-only result.
- Future: `mcp.resources` capability (exposing event-store reads as MCP resources) is a separate ADR if remote observability of zymi runs is ever pulled by a consumer.
- Future: `zymi fetch --add` over the project's `pyproject.toml` could grow a parallel "register this pipeline as an MCP tool for Claude Desktop" convenience command. Out of scope here.

## Rejected alternatives

- **Per-runtime native wrappers (LangChain `BaseTool`, CrewAI tool subclass, OpenAI Agents SDK function-tool, AutoGen function tool).** `langchain-mcp-adapters` and equivalent adapters in every major runtime collapse this to zero code on the user's side; writing wrappers would maintain N parallel contracts of the same thing. See [[feedback_langchain_wrapper_dead]].
- **Tool-splitting for long pipelines (`zymi_<name>_start` / `_status` / `_result` as three tools).** SEP-1686 spec explicitly calls this out as anti-pattern: LLM sees three unrelated tools, doesn't reliably chain them, no spec-level cancellation. Replaced by SEP-1686 native.
- **HTTP+SSE transport in v1.** Adds TLS / port / auth dimensions that stdio doesn't have. The four largest clients (Claude Desktop, Cursor, Windsurf, OpenHands) all support stdio MCP servers first-class. HTTP can come later if a remote-deployed zymi consumer surfaces.
- **Default-exposing all pipelines.** `tools/list` cardinality blows up agent context; many pipelines (internal cron jobs, background workflows) should never appear in agent tool catalogs. Opt-in is the cheap way to keep this honest.
- **Wait for SEP-1686 client adoption to stabilize before shipping.** Sync mode (the 80% case) does not depend on SEP-1686 and works on every MCP client today. Adding async behind capability negotiation now is strictly better than waiting — SEP-1686 servers ahead of clients are the SDK ecosystem's stated rollout shape.
- **Build zori first, expose pipelines through zori only.** Inverts the value proposition. zymi pipelines belong in the agent catalog of *every* runtime, not just one we control.

## Addendum 2026-05-24 — Slice 2 wire correction (SEP-1686 *Final*)

Slice 2 implementation pinned the wire against the `2025-11-25` schema (`schema/2025-11-25/schema.ts`, SEP-1686 now **Final**, not "accepted/implementing" as §Context assumed). Several §4/§5 assumptions were drafted from an earlier shape and are **wrong**; this addendum supersedes them. The schema is authoritative wherever it disagrees with the SEP prose or this ADR's body.

1. **`taskId` is client-generated, not server-generated.** The requestor augments a request with `_meta["modelcontextprotocol.io/task"] = { taskId, ttl? }`; the server maps that client `taskId` → internal zymi `run_id` (it does **not** hand back its own run_id as the taskId). The §4 line "returns `CreateTaskResult` immediately with the zymi run_id as `taskId`" is corrected: the immediate response is `CreateTaskResult { task: Task }` where `Task.taskId` echoes the client's id.

2. **`Task` shape (verbatim from schema):** `{ taskId, status, statusMessage?, createdAt, lastUpdatedAt, ttl: number|null, pollInterval? }`. `TaskStatus = working | input_required | completed | failed | cancelled` (no `submitted`/`unknown` in the enum). Field names are `ttl`/`pollInterval`, **not** `keepAlive`/`pollFrequency` from the SEP prose.

3. **Capability negotiation** is `capabilities.tasks` structured by request category, **not** an `execution.taskSupport: required` field on the tool definition (that was an earlier draft). Server declares `tasks: { list: {}, cancel: {}, requests: { tools: { call: {} } } }` iff ≥1 exposed pipeline is async. The client's `tasks.requests.{sampling,elicitation}` advertises whether the approval bridge (below) is usable.

4. **Methods:** `tasks/get` → `GetTaskResult (= Result & Task)`; `tasks/result` → original `CallToolResult`, **MUST** error `-32602` unless status is `completed`; `tasks/list` → paginated `{ tasks: Task[], nextCursor? }`. After creating a task the server **MUST** emit `notifications/tasks/created` (related-task meta) to close the poll-before-exists race. All task-associated messages carry `_meta["modelcontextprotocol.io/related-task"] = { taskId }`.

5. **Cancellation** is the universal `notifications/cancelled` (referencing the original request id), with an optional explicit `tasks/cancel` gated by the `tasks.cancel` capability. There is **no** per-run cancel path in the engine today (`run_pipeline::handle` is fire-and-forget), so Slice 2 adds one: a `CancellationToken` per task, aborting the spawned pipeline future best-effort and recording the terminal `cancelled` transition.

6. **`input_required` is far more than the "near-zero-cost mapping" §61 claimed.** Per §4.4, `input_required` is entered only when the *server* sends a request *back to the client* (`elicitation/create` or `sampling/createMessage`) sharing the related-task id. So bridging an ADR-0022 approval to `input_required` requires the MCP server to become **bidirectional**: when a parked pipeline publishes `ApprovalRequested` on the bus, the server issues `elicitation/create` to the client, flips the task to `input_required`, and on the elicitation response publishes `ApprovalGranted`/`ApprovalDenied` back onto the bus to unblock the orchestrator. This only functions if the connected host advertises `capabilities.tasks.requests.elicitation.create` — adoption thinner than async itself. **Decision:** split Slice 2 into **2a** (async execution + task store + get/result/list + `notifications/tasks/created` + cancellation + capability negotiation; an async pipeline that hits an approval with no elicitation-capable client transitions to `failed` with a clear `statusMessage`) and **2b** (the elicitation approval bridge), gating 2b on a concrete elicitation-capable consumer rather than building it speculatively. 2a is the shippable, client-reachable surface today.
