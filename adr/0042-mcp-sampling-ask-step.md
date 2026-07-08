# Ask the calling MCP client's model instead of a second API round-trip

Date: 2026-07-08

Status: Proposed

## Context

A zymi pipeline can be exposed as an MCP tool (`zymi mcp serve`, ADR-0033) and
driven by a host that is *itself* an LLM agent — Claude Code, Claude Desktop,
etc. When such a pipeline hits an `agent:` step, the runtime builds an
`LlmProvider` (`src/llm/mod.rs`) and makes an **outbound HTTP call to a model
provider** — a second, independent model, billed and keyed separately from the
host that invoked the tool.

That is often the right thing: the pipeline author wants a *specific*,
deterministic model with known parameters, independent of whoever calls the
tool. But sometimes it is plainly redundant — the caller is already a capable
model sitting one JSON-RPC frame away. Paying for, keying, and configuring a
whole `llm:` provider just to ask "summarize this diff" or "does this output
look right?" — when the thing that called us could answer directly — is the
odd shape the user flagged: *why reach out over the API when there's already a
neural net right here?*

MCP has a purpose-built primitive for exactly this inversion:
**`sampling/createMessage`** — the server asks the *client* to run a
completion on the server's behalf. It is the mirror image of the
`elicitation/create` bridge we already ship (ADR-0033 2b-sync): server→client
request, correlated by JSON-RPC id, awaited inline. ADR-0023 explicitly parked
sampling ("out of scope for v1 ... a separate follow-up ADR once we see
demand"). This is that follow-up.

### What we already have

Everything the transport needs is built:

- `McpClientLink` (`src/mcp/server/link.rs`) issues server→client requests and
  awaits the correlated response while the read loop keeps pumping. It already
  carries a `OnceLock<ClientCaps>` and a `supports_elicitation()` accessor.
- `ClientCaps` / `parse_client_caps` (`src/mcp/server/protocol.rs`) read the
  client's advertised capabilities at `initialize`. Adding `sampling` is one
  field and one `.get("sampling")` probe.
- `LlmProvider` is an object-safe trait behind `Option<Arc<dyn LlmProvider>>`
  on the Runtime (ADR-0041), created by a single factory
  (`llm::create_provider`).

### The constraint that shapes the whole decision

**MCP sampling has no tool-use.** A `sampling/createMessage` message carries
text / image / audio content only; there is no way for the model to emit a
`tool_use` block back through the client, and no way for us to feed
`tool_result` in. Our `agent:` step is a **ReAct loop** — `ChatRequest` ships
`tools: Vec<ToolDefinition>` and the whole point is that the agent plans tool
calls (`PipelineStepKind::Agent`, `src/config/pipeline.rs:218`). Those two
models do not compose. A sampling call can serve a **single-shot,
tool-less completion**, and nothing more.

This is the fork in the road, and it is why the naive framing ("make sampling
just another provider so agent steps use it transparently") is wrong: an agent
step with a non-empty tool list handed to a sampling provider would have to
either silently drop the tools (breaking the step's contract) or error at
first call. Sampling is not a drop-in `LlmProvider`; it is a *different step
shape*.

## Decision

Add a dedicated single-shot pipeline step that poses a prompt to the calling
client's model over `sampling/createMessage`, and captures the text answer as
the step's output. Call it an **`ask:` step**.

```yaml
steps:
  - id: summarize
    ask: "Summarize this deploy diff in two sentences:\n${steps.diff.output}"
    # optional knobs, all with safe defaults:
    system: "You are a terse release engineer."
    max_tokens: 512
```

Semantics:

1. **`ask:` is a third `PipelineStepKind`** alongside `Agent` and `Tool`,
   mutually exclusive with them at the YAML layer (same detection pattern as
   `agent:` vs `tool:` in `PipelineStep::deserialize`). It takes a templated
   prompt string (`${inputs.*}` / `${steps.*.output}` substituted first, like
   every other step) and optional `system` / `max_tokens` / `temperature`.
2. **It resolves through the MCP client**, not an HTTP provider. When a step
   dispatches, the orchestrator issues `sampling/createMessage` via
   `McpClientLink` and blocks on the response, exactly as the elicitation
   bridge blocks on `elicitation/create`. The text of the returned message
   becomes the step output; it flows into downstream `${steps.summarize.output}`
   like any tool output.
3. **It is gated on `capabilities.sampling`.** `ClientCaps` gains a `sampling`
   bool; `McpClientLink` gains `supports_sampling()`. If a pipeline with an
   `ask:` step is loaded and the connected client did **not** advertise
   sampling, the step fails with a clear `statusMessage` — same fail-closed
   posture as an async pipeline hitting an approval with no elicitation-capable
   client (ADR-0033 §6).
4. **It requires no `llm:` block.** An `ask:`-only pipeline is model-free from
   the runtime's point of view — the model lives in the host. This composes
   cleanly with ADR-0041: `has_agent_step()` stays the trigger for a *required*
   provider; `ask:` steps do **not** set it. A pure `tool:` + `ask:` pipeline
   builds with no provider and runs entirely off the caller's model.
5. **`ask:` only works under `zymi mcp serve`.** There is no client to sample
   from under `zymi run` / `zymi serve` / cron. A workspace with an `ask:` step
   loaded outside the MCP-server context fails fast at build with a message
   pointing the author at `zymi mcp serve` (or at using an `agent:` step with
   an `llm:` provider instead). We do **not** silently fall back to an HTTP
   provider — the whole value is *not* reaching for the API, and a hidden
   fallback would reintroduce the config/keying burden this removes.

### Why a new step kind, not a `provider: mcp_sampling`

Rejected: expressing this as `llm: { provider: mcp_sampling }` so existing
`agent:` steps route to the client transparently.

- **Tool-use mismatch (the killer).** As above, sampling can't serve a ReAct
  loop. A sampling-backed `LlmProvider` would have to reject any `ChatRequest`
  with a non-empty `tools` list — i.e. reject the exact thing `agent:` steps
  are for. The provider abstraction would be a lie.
- **Wrong lifetime.** `LlmProvider` is built at `Runtime::build`, but the
  `McpClientLink` is created *after* `build_async()` returns
  (`src/cli/mcp.rs:120-128`) and its `ClientCaps` are only known after the
  client's `initialize`. A provider that depends on a not-yet-existing link is
  an ordering hazard. A step kind resolved at *dispatch* time sidesteps it —
  the link is live by the time any `tools/call` runs.
- **Honest surface.** `ask:` names what it is — "pose a question to whoever
  called you" — and reads differently from `agent:` (a tool-using worker) on
  purpose. Authors should see the two as distinct, because they are: one is
  deterministic-and-tool-capable against a pinned model, the other is
  cheap-and-tool-less against the caller's model.

### Deferred: sampling as a fallback provider for tool-less agent steps

An `agent:` step whose agent happens to have an empty tool list is,
functionally, a single-shot completion and *could* be served by sampling. We
deliberately do **not** special-case that here — it couples two independent
concepts (agent identity/context policy vs. transport) and invites the
"silently drop tools" trap the moment someone adds a tool to that agent. If
demand appears, it is a clean follow-up: detect tool-less agent steps and,
under `serve` with a sampling-capable client, offer sampling as an opt-in
resolution. Out of scope now.

## Consequences

- **Pros:**
  - Removes the redundant second model hop and its `llm:` block / API key for
    the common "the caller can just answer this" case. Matches the deploy/ops
    and MCP-tool story: a pipeline exposed to Claude Code can lean on Claude
    Code's model with zero provider config.
  - Reuses the entire 2b-sync side-channel (`McpClientLink`, capability
    gating, inline await). No new transport, no new bidirectional machinery.
  - Composes with ADR-0041: `ask:`-only pipelines stay model-free; the
    `has_agent_step()` fail-fast contract is untouched.
  - Capability-gated and fail-closed — an `ask:` step against a
    non-sampling client fails loudly, never silently degrades.
- **Cons / limitations:**
  - **Determinism drops.** The pipeline no longer controls which model answers
    or how it's parameterized — MCP sampling lets the server only *hint*
    (`modelPreferences`, `systemPrompt`, `temperature`), and the host may
    ignore hints, substitute a model, or gate on human approval. `ask:` steps
    are therefore unsuitable where a pinned, reproducible model matters; those
    keep using `agent:` + `llm:`. This trade-off must be documented at the
    step, not buried.
  - **No tool-use, by construction.** `ask:` is single-shot text-in/text-out.
    Anything needing the agent to *act* stays an `agent:` step.
  - **`serve`-only.** An `ask:` step is meaningless without a connected client;
    the build-time guard prevents accidental use in headless runs but does mean
    a pipeline mixing `ask:` and headless execution can't exist. Acceptable —
    they're genuinely different execution contexts.
  - **New surface area in the hot path.** A third step kind touches the
    deserializer, DAG validation, the orchestrator dispatch match, and the
    observability/graph renderers that pattern-match on step kind. All are
    finite, enumerated `match`es today, so the compiler flags every site.

## Implementation notes

- **Protocol:** add `sampling: bool` to `ClientCaps`; probe
  `capabilities.sampling` in `parse_client_caps`
  (`src/mcp/server/protocol.rs`). Add `supports_sampling()` to `McpClientLink`.
- **Config:** add `PipelineStepKind::Ask { prompt, system, max_tokens,
  temperature }` and the matching optional fields to `PipelineStepRaw`; extend
  the `(has_agent, has_tool, has_ask)` mutual-exclusion check in
  `PipelineStep::deserialize` (`src/config/pipeline.rs`). `ask:` must **not**
  count toward `WorkspaceConfig::has_agent_step()` (ADR-0041).
- **Sampling call:** a small helper that builds `sampling/createMessage`
  params (`messages: [{ role: "user", content: { type: "text", text } }]`,
  optional `systemPrompt`, `maxTokens`, `temperature`) and maps the result's
  `content.text` to a step output string. Errors (`link closed`, client
  `error`, non-text content) surface as a failed step with a clear message.
- **Dispatch wiring:** the orchestrator needs the `McpClientLink` to resolve an
  `ask:` step, but the link is `serve`-specific. Thread an optional
  sampling resolver into the runtime (mirroring how the elicitation approval
  channel is handed the link in `src/cli/mcp.rs`), `None` outside `serve`; an
  `ask:` dispatch with `None` resolver is the build-guarded-unreachable
  internal error.
- **Build guard:** at `Runtime::build`, if the workspace has an `ask:` step and
  no sampling resolver was injected, fail with a message pointing at
  `zymi mcp serve`. Parallels the ADR-0041 agent-step provider guard.
- **Tests:** (1) `ask:` + `tool:` mutual exclusion rejected at parse;
  (2) an `ask:`-only workspace builds with no `llm:` provider;
  (3) under a mock link advertising sampling, an `ask:` step round-trips a
  `sampling/createMessage` and captures the reply as output;
  (4) a sampling-incapable client fails the step closed;
  (5) an `ask:` workspace built without a resolver fails at build.
