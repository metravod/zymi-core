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
   rather than a human form. The `resume_token` is **opaque and integrity-
   protected** (signed/encrypted), carries the parked `request_id` + `stream_id`
   + an expiry, and is shaped to be a drop-in for SEP-2322 `requestState` (see
   *Alignment with SEP-2322*).
4. **A parked `ask:` has a bounded lifetime and fails closed on expiry.**
   Unlike an approval — whose answerer is a human who will come back — an `ask:`
   answerer is an automated caller, so a caller that never resumes (non-agentic,
   non-cooperative, or one that mistakes `needs_reasoning` for a final result)
   would otherwise hang the run forever. The park therefore inherits the
   approval timeout (`DEFAULT_APPROVAL_TIMEOUT`, `Runtime::approval_timeout`)
   and is reaped exactly like a dangling approval on restart
   (`replay_unfulfilled_approvals`, `src/cli/mcp.rs`): on expiry the step fails
   closed with a clear message. Abandonment is a first-class terminal state, not
   a leak.
5. **Outside `serve`, `ask:` uses a configured reasoning channel or fails
   closed.** Like approvals, an `ask:` step routes to a named `channel`. With no
   channel able to answer (e.g. a headless `zymi run` with none configured), the
   step fails closed with a clear message — the same posture as an approval with
   no channel (fail-closed, ADR-0022). No silent fallback to an HTTP provider:
   the whole point is *not* reaching for the API.
6. **Park scope is the whole run in v1; batching is deferred.** An `ask:` parks
   the run exactly as an approval does — the simplest correct semantics and a
   direct reuse of the approval machinery. Consequence: a fan-out pipeline with
   several independent `ask:` steps serializes into N sequential round-trips
   through the caller's loop (one park, one resume, each). SEP-2322's
   `inputRequests` is a *map*, i.e. it can batch several pending inputs into one
   round-trip; partial-park (park only the dependent sub-DAG) and surfacing
   multiple pending reasoning requests at once are the natural follow-ups, and
   they line up with the MRTR migration rather than fighting it. Named as a
   known limitation, not solved here.
7. **It requires no `llm:` block.** An `ask:`-only pipeline is model-free from
   the runtime's view — reasoning lives in whoever answers the channel. Composes
   with ADR-0041: `has_agent_step()` stays the trigger for a *required* provider;
   `ask:` steps do **not** set it. A pure `tool:` + `ask:` pipeline builds with
   no provider.

## Security — an `ask:` answer is untrusted tool output, not trusted step output

This is the sharpest hazard and the one to get right. An `ask:` answer is
**model-generated free text over context that is often attacker-influenced** —
in the worked example the prompt interpolates `${steps.diff.output}`, and a diff
can carry adversarial content. The answer then becomes a step output that can
flow into a sink: `${steps.summarize.output}` reaching a `shell` command, a
`ReadFile` path, an HTTP body. That is a prompt-injection → command-injection
chain, the same class ADR-0036 hardened for `ReadFile`.

Therefore:

- **An `ask:` answer carries the same taint as any tool output and MUST pass the
  same contract/guard discipline before reaching a sink** — it is emphatically
  **not** trusted step output. Concretely: the shell/HTTP template-escaping
  discipline (ADR-0039, the `command_template` resolution stages) applies to an
  interpolated `ask:` answer exactly as it does to any other untrusted
  interpolation, and file sinks apply the `BUILTIN_SECRET_DENY` / path-
  normalization boundary (ADR-0036).
- **Secure by default, not operator-dependent.** Following ADR-0036's principle
  (a human clicking "approve" on a secret read still leaks it), the guard sits
  on the data path, not on the author remembering to sanitize. Treat the answer
  as *data, never code*.
- The audit trail helps forensics (the exact injected answer is a recorded
  `ReasoningAnswered` event) but does **not** substitute for the guard — logging
  an exploit is not preventing it.

An earlier draft called the answer "step output that flows to shell like any
tool output" as if that were reassuring; it is the opposite, and this section
is the correction.

## Why park/ask/resume, not a sampling provider or a boundary hand-back

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

## Alignment with SEP-2322 (MRTR)

The `{ status: needs_reasoning, prompt, resume_token }` + separate `resume`
tool is, honestly, **SEP-2322 Multi-Round-Trip Requests hand-rolled over bare
tool calls**: `needs_reasoning` ≈ `InputRequiredResult`, `resume_token` ≈ the
opaque `requestState` the client must echo back unmodified, and the re-issued
`resume` call ≈ retrying the original request with `inputResponses`. Two
consequences, one good and one to state plainly:

- **Design for a drop-in migration (the good part).** MRTR is the stateless
  successor MCP is standardizing for server→client interaction, so shape the
  wire forms now to map 1:1: make `resume_token` opaque + integrity-protected
  (MRTR *SHOULD*s an encrypted/signed `requestState` — AES-GCM or a signed JWT),
  never let the caller inspect or mutate it, and key any future multi-request
  batch identically. Then adopting the standard envelope later is a form change,
  not a redesign.
- **The bespoke handshake is a real compliance risk until then (the honest
  part).** A conformant MRTR client auto-resolves the round-trip in its SDK; our
  hand-rolled version instead relies on the *model* correctly playing a custom
  multi-step protocol — recognize a non-final `needs_reasoning`, reason, call
  `resume` with the answer keyed correctly. Claude Code does this reliably;
  arbitrary hosts may not. The standard would remove that risk; we are trading
  it for working-today-with-no-RC-dependency. That is a deliberate, temporary
  trade, and it is named here rather than hidden.

One subtlety that actually *reinforces* the design: MRTR's built-in input types
are elicitation (human) and **sampling (deprecated by SEP-2577)**. So the
standard's own "caller's model answers" path rides the deprecated primitive.
Our reasoning content is deliberately **not** an MRTR `sampling` inputRequest —
it is plain tool-result data the caller's agent loop reasons over. We therefore
want MRTR as the *envelope* (stateless multi-turn, batching) while keeping the
*reasoning content* as agent-loop-over-data, which depends on nothing
deprecated. Envelope: standardize. Content: stay off sampling.

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
  - **Fully audited *and* deterministically replayable.** The prompt and answer
    are recorded as events (`ReasoningRequested` / `ReasoningAnswered`) in the
    stream and hash chain. This is not just forensics: on `zymi resume` / replay
    the answer is read back from the log and **never re-asked**, so a run that
    borrowed the caller's model still replays **byte-identical** (ADR-0018,
    ADR-0040). A live model callback (sampling) could not offer this — its
    non-deterministic I/O would bypass the log; here it is free.
  - **Answerable by more than an agent.** `ReasoningChannel` is a sibling of
    `ApprovalChannel`, so an `ask:` can also be answered by a **human** over a
    TUI/HTTP/Telegram-style channel, or by a configured service — a
    human-answerable reasoning gate that works headless, not only under an agent
    host. No `capabilities.sampling` requirement anywhere.
  - Reuses ADR-0022 approval parking, ADR-0018 resume, and ADR-0033 2a tasks —
    little genuinely new machinery.
- **Cons / limitations:**
  - **Needs an answering channel, and the caller-model channel is multi-turn.**
    An `ask:` is only as good as a channel that will answer it: an agent that
    loops and calls `resume` (Claude Code), a human on a manual channel, or a
    configured service. It is *not* limited to agentic callers (see the human
    channel above) — but the headline "borrow the caller's model" mode does
    require an agentic, cooperative caller, and a non-cooperative one is handled
    by the expiry in Decision §4, not left to hang.
  - **Determinism moves to the caller.** Which model answers, and how well, is
    the caller's business. Where reproducibility of the *reasoning itself*
    matters, use `agent:` + `llm:`. (Note this is orthogonal to replay
    determinism above: once answered, the recorded answer replays identically;
    it is the *first* answer that is the caller's non-deterministic call.)
  - **Untrusted-output hazard.** The answer is untrusted and can reach a sink —
    see the Security section; this is the single most important thing to
    implement correctly, not a footnote.
  - **New step kind + event pair** touch the deserializer, DAG validation, the
    orchestrator dispatch match, the resume path, and the observability/graph
    renderers. All are enumerated `match`es, so the compiler flags every site.
  - **Resume-token surface.** Exposing park/resume to the caller widens the MCP
    tool surface (a `resume`-style call) and its idempotency/security story
    (ADR-0018 covers idempotent resume; the token must also be integrity-
    protected per *Alignment with SEP-2322*).

## A note on vocabulary (`ask` vs `reasoning`)

The authoring surface is the verb **`ask:`** (what you write in a step); the
recorded/observable domain is the noun **reasoning** (`ReasoningRequested` /
`ReasoningAnswered` events, the `reasoning` channel). This split is deliberate
and mirrors the existing `tool:` step → `ToolCallCompleted` event pattern: an
imperative you author, a noun the system records. The one place the two would
collide for a *user* is the channel name in config; keep that aligned with the
event vocabulary (`reasoning`) since it is answer-source configuration, sibling
to `approvals:`. If review prefers a single word end-to-end, unify on `ask`
(`AskRequested` / `AskAnswered`, `ask` channel) — either is fine as long as it
is one conscious choice, not two drifting ones.

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
  record the answer as the step output. Park inherits `Runtime::approval_timeout`
  and expiry fails the step closed (Decision §4).
- **Untrusted-output guard (do this first):** the answer must enter the same
  taint/contract path as tool output before any sink. Route it through the
  existing shell/HTTP template-escaping (ADR-0039) and file deny-list/path-
  normalization (ADR-0036) rather than treating a step-output substitution as
  trusted. If needed, tag the produced output as untrusted at the source so
  downstream contracts can enforce without per-step opt-in.
- **Channel:** a `ReasoningChannel` trait sibling to `ApprovalChannel`. Under
  `serve`, its MCP implementation surfaces the parked request as a task outcome
  (`needs_reasoning` + resume token) and maps the caller's `resume` call to a
  `ReasoningAnswered` publish — mirroring `McpElicitationApprovalChannel` wiring
  in `src/cli/mcp.rs`. The `resume_token` is opaque + signed/encrypted (shaped
  as SEP-2322 `requestState`); validate integrity + expiry before accepting a
  resume.
- **Resume:** reuse `handlers/resume_pipeline.rs` (ADR-0018) for the answer path
  and its idempotency guarantees; the reasoning answer is the resume input.
- **Tests:** (1) `ask:` + `tool:` mutual exclusion rejected at parse;
  (2) an `ask:`-only workspace builds with no `llm:` provider;
  (3) a parked `ask:` step resumes with a supplied answer and exposes it as
  `${steps.X.output}` to a downstream step (mid-DAG);
  (4) prompt and answer appear in the event stream / hash chain, and a replay
  reads the recorded answer without re-asking (byte-identical);
  (5) an `ask:` step with no answering channel fails closed;
  (6) a parked `ask:` that is never answered expires and fails closed;
  (7) an adversarial `ask:` answer (e.g. `$(rm -rf …)` / `; curl …`) is escaped
  or denied at a shell/file sink — the untrusted-output guard holds;
  (8) a `resume` with a tampered/expired `resume_token` is rejected.

## History

- **2026-07-08 (v1, withdrawn):** first draft proposed `ask:` backed by MCP
  `sampling/createMessage`. Withdrawn on discovering SEP-2577 deprecates
  sampling with no replacement, and on the reframe that the requirement is
  park/ask/resume (which zymi already has for approvals), not a model callback.
  The sampling approach — including as a time-boxed experimental bridge — is
  rejected.
- **2026-07-08 (v2):** park/ask/resume design (this document).
- **2026-07-08 (v3, review response):** incorporated review. Added the
  **Security** section (untrusted `ask:` output must take the tool-output
  taint/guard path before any sink — the strongest gap); a bounded park lifetime
  + expiry so an abandoning caller cannot hang a run (Decision §4); the
  **Alignment with SEP-2322 (MRTR)** section naming the hand-rolled-handshake
  compliance risk and the drop-in-`requestState` migration target, plus the
  subtlety that MRTR's own model-input type is deprecated sampling; the
  park-scope / batching limitation (Decision §6); replay-determinism promoted to
  an explicit pro; the multi-turn con narrowed (human/service channels also
  answer); and a deliberate `ask` (verb) vs `reasoning` (recorded noun)
  vocabulary decision.
