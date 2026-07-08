# Delegate a reasoning step to the calling agent via park/ask/resume

Date: 2026-07-08

Status: Proposed (supersedes the sampling-based draft below — see History)

## Context

A zymi pipeline can be exposed as an MCP tool (`zymi mcp serve`, ADR-0033) and
driven by a host that is *itself* an LLM agent — Claude Code, Claude Desktop,
etc. When such a pipeline hits an `agent:` step, the runtime builds an
`LlmProvider` (`src/llm/mod.rs`) and makes an **outbound HTTP call to a model
provider** — a second, independently-keyed-and-billed model, even though the
host that invoked the tool is already a capable model one JSON-RPC frame away.

For genuinely deterministic reasoning (a pinned model with known parameters,
independent of the caller) that is correct. But for the common "the caller
could just answer this" case — *summarize this diff*, *does this output look
right?* — configuring, keying, and paying for a whole `llm:` provider is
redundant when the thing that called us could answer directly. That is the
odd shape to remove: *why reach out over the API when there's already a neural
net right here?*

### The dead end this ADR walked out of

The obvious mechanism was MCP's `sampling/createMessage` (server asks the
client to run a completion on its behalf). Investigation killed it:

- **SEP-2577 deprecates sampling** in the `2026-07-28` spec RC (advisory-only,
  functional ≥1 year, but **no replacement** offered). Rationale: low adoption
  vs. implementation complexity. Building a new, permanent, user-facing step on
  a just-deprecated primitive with no successor is a bad bet.
- **Elicitation survives but is not a substitute.** It returns *user-entered
  form data*, not a model completion — a host answers it by prompting a human.
- The broader signal: **MCP is going stateless, away from server→client model
  invocation.** "A model-less server borrows the caller's brain mid-call" is a
  pattern the whole ecosystem is retreating from; each participant is expected
  to bring its own model access. Switching to another protocol (A2A, etc.)
  would hit the same wall — those assume every agent has its own brain too.

### The reframe

The need was mis-stated as "invoke the caller's model." The real need is:
**the pipeline must pause, ask the caller a question, and resume with the
caller's answer.** zymi *already does exactly this* for approvals (ADR-0022):
the orchestrator publishes `ApprovalRequested` on the bus, parks, and awaits
`ApprovalGranted` / `ApprovalDenied` (`src/events/mod.rs:137-163`); a run can
be resumed from its event stream (ADR-0018, `handlers/resume_pipeline.rs`).

An approval today carries a *decision* (approve/deny). Generalize it to carry
a *value*: a prompt in, free text out. Route that to the connected agent, and
the agent's own loop supplies the answer — no model callback, no deprecated
primitive, no new transport. This is pure request/response tools, the most
durable part of MCP.

### One mechanism, not two

An earlier framing split "reasoning mid-DAG" from "reasoning at the pipeline
boundary" and asked whether the boundary case deserved a simpler design. It
does not. The real discriminator is **whether the answer re-enters zymi**:

- If the answer is recorded in the event store / hash chain, feeds a later
  step, or drives a branch → the run **must** resume to receive it. A terminal
  reasoning gate still needs resume purely to *record* the answer. This is the
  general park/ask/resume mechanism regardless of DAG position.
- If the answer never re-enters zymi (pure hand-back to the caller) → no resume
  is needed, but then zymi *does nothing* with the reasoning and the step is
  outside the audit trail — antithetical to zymi's reason to exist (ADR-0035,
  ADR-0040). Where that's acceptable, the work arguably shouldn't route through
  zymi at all.

So there is one feature to build. "Boundary" is just the terminal special case
of it, not a separate, simpler thing.

## Decision

Add an **`ask:` step**: a pipeline step that parks the run, surfaces a prompt
to the connected caller as a *reasoning request*, and resumes with the caller's
text answer as the step output. It is the approval mechanism generalized from
`decision` to `value`.

```yaml
steps:
  - id: summarize
    ask: "Summarize this deploy diff in two sentences:\n${steps.diff.output}"
    # optional: which channel answers this (defaults to the caller under serve)
    channel: mcp_reasoning
  - id: gate
    tool: shell
    args: { command: "./ship.sh" }
    depends_on: [summarize]        # answer re-enters zymi and drives later steps
```

Semantics:

1. **`ask:` is a third `PipelineStepKind`** alongside `Agent` and `Tool`,
   mutually exclusive with them at the YAML layer (same detection pattern as
   `agent:` vs `tool:` in `PipelineStep::deserialize`, `src/config/pipeline.rs`).
   Its body is a templated prompt (`${inputs.*}` / `${steps.*.output}`
   substituted first, like every step) plus an optional answering `channel`.
2. **On dispatch it publishes a reasoning request on the bus and parks**,
   structurally identical to `ApprovalRequested`. A new event pair —
   `ReasoningRequested { request_id, stream_id, prompt, channel }` and
   `ReasoningAnswered { request_id, stream_id, answer, answered_by }` —
   sits beside the approval events. The orchestrator awaits the answer, exactly
   as it awaits `ApprovalGranted`. The `answer` becomes the step output and
   flows into downstream `${steps.summarize.output}` like any tool output. It
   is recorded in the event stream, so it is inside the audit trail and the
   hash chain by construction.
3. **Under `zymi mcp serve` the answering channel is the connected agent.**
   The pipeline is invoked as a task; when it parks on a reasoning request, the
   `tools/call` returns an outcome the calling agent can act on
   (`{ status: needs_reasoning, prompt, resume_token }`), and the agent — mid
   its own loop — reasons and calls a `resume` tool with the answer. This reuses
   the async task + resume surface (ADR-0033 2a, ADR-0018); it is plain
   stateless request/response, no `sampling`, no `elicitation`. A `ReasoningChannel`
   bridges the bus request to this task/resume flow, mirroring how
   `McpElicitationApprovalChannel` bridges an approval to `elicitation/create`
   (`src/mcp/server/elicitation.rs`), but answering from the caller's *model*
   rather than a human form.
4. **Outside `serve`, `ask:` uses a configured reasoning channel or fails
   closed.** Like approvals, an `ask:` step routes to a named `channel`. With no
   channel able to answer (e.g. a headless `zymi run` with none configured), the
   step fails closed with a clear message — the same posture as an approval with
   no channel (fail-closed, ADR-0022). No silent fallback to an HTTP provider:
   the whole point is *not* reaching for the API.
5. **It requires no `llm:` block.** An `ask:`-only pipeline is model-free from
   the runtime's view — reasoning lives in whoever answers the channel. Composes
   with ADR-0041: `has_agent_step()` stays the trigger for a *required* provider;
   `ask:` steps do **not** set it. A pure `tool:` + `ask:` pipeline builds with
   no provider.

### Why park/ask/resume, not a sampling provider or a boundary hand-back

- **vs. sampling (`provider: mcp_sampling` or a sampling-backed `ask:`):**
  deprecated by SEP-2577 with no successor; against MCP's stateless direction;
  requires a sampling-capable host; and MCP sampling has no tool-use anyway, so
  it could never back a tool-using agent step. Rejected outright — not even as a
  time-boxed bridge, because it would need self-deprecation within a year.
- **vs. a separate "boundary hand-back" feature:** not a distinct design. Either
  the answer re-enters zymi (then it needs resume — this mechanism) or it does
  not (then it is outside the audit trail and arguably not zymi's job). Building
  a second, "simpler" path would duplicate the parking machinery for a case that
  either collapses into this one or shouldn't exist.
- **vs. keeping only `agent:` + HTTP `llm:`:** that stays available and is the
  right tool when a pinned, reproducible model matters. `ask:` is the complement
  for "let whoever called me reason," not a replacement.

## Consequences

- **Pros:**
  - Removes the redundant second model hop and its `llm:` block / API key for
    the "the caller can just answer this" case, using the model already on the
    other end of the connection.
  - **No deprecated primitives.** Pure stateless tools + park/resume — aligned
    with where MCP is investing, not what it is sunsetting. Survives a protocol
    change because it stands on request/response tools, not a special channel.
  - **Works mid-DAG.** A real pause/resume, so a reasoning answer can feed later
    deterministic steps and branches — not just the pipeline boundary.
  - **Fully audited.** The prompt and answer are events in the stream and the
    hash chain, unlike a transparent model callback whose I/O would bypass the
    log.
  - **Host-agnostic.** Any caller that runs a tool loop can answer; no
    `capabilities.sampling` requirement.
  - Reuses ADR-0022 approval parking, ADR-0018 resume, and ADR-0033 2a tasks —
    little genuinely new machinery.
- **Cons / limitations:**
  - **Multi-turn.** The caller must be an agent that loops and can call a resume
    tool (Claude Code can). A non-agentic MCP client can't answer an `ask:` step
    — but neither would sampling have helped there.
  - **Determinism moves to the caller.** Which model answers, and how well, is
    the caller's business. Where reproducibility matters, use `agent:` + `llm:`.
    Documented at the step, not buried.
  - **New step kind + event pair** touch the deserializer, DAG validation, the
    orchestrator dispatch match, the resume path, and the observability/graph
    renderers. All are enumerated `match`es, so the compiler flags every site.
  - **Resume-token surface.** Exposing park/resume to the caller widens the MCP
    tool surface (a `resume`-style call) and its idempotency/security story
    (ADR-0018 already covers idempotent resume; reuse it).

## Implementation notes

- **Events:** add `ReasoningRequested` / `ReasoningAnswered` beside the approval
  variants in `EventKind` (`src/events/mod.rs`); wire their `kind_str` arms and
  any store/observability matches the compiler surfaces.
- **Config:** add `PipelineStepKind::Ask { prompt, channel }` and matching raw
  fields; extend the `(has_agent, has_tool, has_ask)` mutual-exclusion check in
  `PipelineStep::deserialize`. `ask:` must **not** count toward
  `WorkspaceConfig::has_agent_step()` (ADR-0041).
- **Orchestrator:** on an `ask:` step, publish `ReasoningRequested`, park, and
  await `ReasoningAnswered` — the same await pattern the approval path uses;
  record the answer as the step output.
- **Channel:** a `ReasoningChannel` trait sibling to `ApprovalChannel`. Under
  `serve`, its MCP implementation surfaces the parked request as a task outcome
  (`needs_reasoning` + resume token) and maps the caller's `resume` call to a
  `ReasoningAnswered` publish — mirroring `McpElicitationApprovalChannel` wiring
  in `src/cli/mcp.rs`.
- **Resume:** reuse `handlers/resume_pipeline.rs` (ADR-0018) for the answer path
  and its idempotency guarantees; the reasoning answer is the resume input.
- **Tests:** (1) `ask:` + `tool:` mutual exclusion rejected at parse;
  (2) an `ask:`-only workspace builds with no `llm:` provider;
  (3) a parked `ask:` step resumes with a supplied answer and exposes it as
  `${steps.X.output}` to a downstream step (mid-DAG);
  (4) prompt and answer appear in the event stream / hash chain;
  (5) an `ask:` step with no answering channel fails closed.

## History

- **2026-07-08 (v1, withdrawn):** first draft proposed `ask:` backed by MCP
  `sampling/createMessage`. Withdrawn on discovering SEP-2577 deprecates
  sampling with no replacement, and on the reframe that the requirement is
  park/ask/resume (which zymi already has for approvals), not a model callback.
  The sampling approach — including as a time-boxed experimental bridge — is
  rejected. This document is the replacement design.
