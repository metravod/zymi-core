# MCP observability tools (agent self-introspection of zymi runs)

Date: 2026-05-22

Status: Proposed.

## Context

ADR-0033 proposed `zymi mcp serve` exposing pipelines as MCP tools. Its "Future" section noted that an `mcp.resources` capability over the event store was a separate ADR pending a concrete consumer pull. This ADR converts that hint into a concrete tool surface — and lays out the staged path that ends in agents editing their own pipelines.

The asymmetry today: when a pipeline invoked via MCP fails, the agent sees only an opaque "tool error" payload. The full execution trace (events, step inputs/outputs, masked-context reconstruction) lives in zymi's event store, but the agent has no way to reach it. The human must switch from the host (Claude Desktop, Cursor, OpenHands) into a terminal and run `zymi observe` / `zymi events`. That context switch is the UX gap.

Two reinforcing observations:

1. **zymi's event granularity is unique among major agent frameworks.** Masking (ADR-0016, lossless) + deterministic tool steps as first-class (ADR-0024) + idempotent fork/resume (ADR-0018) produce a per-step trace that OH's summarising condenser, LangGraph's checkpointers, and CrewAI's logs cannot match. Exposing it through MCP is something none of those frameworks can match, because they don't have the underlying store. This is the concrete reinforcement of the "MCP backend for agents" positioning ([[project_mcp_server_strategy]]).

2. **The agent already has competence in zymi YAML via the bundled `zymi-skill`.** What it lacks is *capability* — runtime access to its own execution traces, and (in the future) the ability to edit pipelines. The skill answers "how do I write a pipeline?"; this ADR answers "what just happened in this run?", and lays groundwork for "let me fix the pipeline that just broke."

## Decision

Ship MCP observability tools as a peer surface to ADR-0033 pipeline tools. Read-only in v1; editing is a staged follow-up with explicit safety gating (see Future).

**1. Opt-in via CLI flag.** `zymi mcp serve --expose-observability`, off by default. Mirrors the per-pipeline `expose:` opt-in in ADR-0033 — tool catalogs stay lean unless the operator explicitly wants introspection.

**2. Four tools.**

- `zymi.runs.list(pipeline?, status?, limit=20)` → compact run summaries `{ run_id, pipeline, status, started_at, duration_ms, error? }`
- `zymi.runs.get(run_id)` → current state `{ status, current_step, total_steps, started_at, finished_at?, error? }`
- `zymi.runs.events(run_id, since_seq?, types?, limit=50)` → flat event projection `{ seq, ts, type, step_id?, summary }`. Compact-by-default, no nested payloads.
- `zymi.runs.step_io(run_id, step_id)` → full prompt-in / output-out for one step, with assembled prompt reconstructed via ContextBuilder on read (matches [[project_observe_ui_full_io]]). This is the expensive call; agents should reach for `events()` first.

**3. Scope.** Default: runs initiated by the same MCP session, tracked via `parent_run_id`. Cross-session global access requires `--observability-scope all` and is intended for single-user dev setups, not shared agent runtimes. The point is mitigating the case where one tenant's agent reads another tenant's prompts/outputs through a multiplexed MCP server.

**4. Token discipline.** `events()` returns flat summaries (no nested LLM payloads); full inputs/outputs require explicit `step_io`. Server-side `since_seq`, `types`, `limit` are mandatory parameters in the schema (no "give me everything"). The expected agent flow is *narrow before deepen*: `list → get → events(filtered) → step_io(one step)`.

**5. No write surface in v1.** Fork, cancel, edit are explicitly *not* in this ADR. Cancel is already covered by SEP-1686 `tasks/cancel` (ADR-0033). Fork and edit ship later, behind separate gates (see Future).

## Consequences

- Pro: agent can investigate its own pipeline failures inside the host conversation, without forcing the human to switch to a terminal. Closes the UX gap left by ADR-0033.
- Pro: the capability itself is unique to zymi in 2026 among the MCP-tool-server ecosystem. OH/LangGraph/CrewAI cannot expose comparable traces because they don't have the underlying event granularity. This is a concrete "wow" demo for the MCP-backend positioning.
- Pro: compounds with the bundled `zymi-skill`. Skill provides YAML competence; observability tools provide runtime data. Together they unlock the Stage 3 editing surface below without further skill investment.
- Pro: scoped + opt-in + compact-by-default = small attack surface relative to the capability gained.
- Con: cross-session scope correctness depends on new `parent_run_id` tracking plumbing. Needs explicit isolation tests before this is safe to enable in any shared deployment.
- Con: tool catalog grows by 4 entries in agent system prompts when enabled. Acceptable given the opt-in default; agents not given the flag see the same surface as ADR-0033 alone.
- Con: recursion risk — agent reads its own events, retries, reads again. Not catastrophic but the host's `max_steps` / `max_tool_calls` is the only natural ceiling. Document as known limitation; a server-side circuit breaker (per-`run_id` introspection budget) is a candidate follow-up if it bites in practice.
- Con: `step_io` is large (full prompts + outputs). Misuse blows agent context. Mitigated by description framing ("call only for a specific step you've already narrowed down via `events()`") and by reserving full payloads to this one tool rather than smearing them across the surface.

## Future

The point of writing observability and editing in the same ADR (rather than two unrelated ones) is that the architecture must support the editing endpoint cleanly, not bolt it on later. Stages 2–5 below are not committed but the v1 surface above does not preclude them.

**Stage 2 — pipeline read tools.** `zymi.pipelines.list()` and `zymi.pipelines.get(name)` returning YAML source. Read-only, but exposes pipeline internals (prompts, tool wiring) to the agent. Behind a separate `--expose-pipelines` flag because the threat model differs from observability (pipeline source may contain credentials-by-reference, prompts considered IP, etc.). Useful in combination with zymi-skill: agent sees the YAML, knows the schema, can reason about the failure.

**Stage 3 — pipeline editing tools.** The high-leverage move. Two tools:

- `zymi.pipelines.validate(content)` — runs ADR-0025 load-time validation, returns errors/warnings. No side effect. Safe surface.
- `zymi.pipelines.write(name, content)` — writes the file. **Mandatory ADR-0022 approval** gates every call. SEP-1686 `input_required` lifecycle maps directly (ADR-0033 §4): write request → task transitions to `input_required` → human reviews diff in the host → approves → file written; otherwise rejected.

Intended flow with the bundled `zymi-skill`:

1. Pipeline fails. Agent reads `runs.events(run_id)` → `step_io(run_id, failing_step)`.
2. Agent uses zymi-skill knowledge to diagnose + draft a fix.
3. Agent calls `pipelines.validate(patched)` (no side effect) to confirm the patch parses + type-checks.
4. Agent calls `pipelines.write(name, patched)` → approval round-trip → file lands.
5. Next pipeline invocation uses the fixed version.

Open questions for Stage 3:

- **Active-run isolation.** Tasks bound to YAML snapshot at start; edits affect *future* runs only. Worth verifying once the pipeline loader is in scope.
- **Editable allowlist.** `--editable <glob>` so the agent can only touch a declared subset. Off by default. The production-critical pipeline should never be in the LLM's write path without operator intent.
- **Git policy.** Write the file dirty; let the user commit. Alternative: auto-commit with a `zymi-agent:` trailer. Defer to consumer pull — don't pre-decide.
- **Atomic multi-file edits.** A bug that requires changing a pipeline + adding a tool file needs `pipelines.patch_set([{path, content}, ...])` rather than two separate approvals. Probably the right shape once Stage 3 sees real use.
- **Loop hazard.** Agent edits pipeline, pipeline fails differently, agent edits again, … Reuses the same recursion concern as v1 but with write blast radius. Approvals are the primary control; editing budget per session is a candidate secondary control.

**Stage 4 — fork / replay.** `zymi.runs.fork(run_id, from_step)` using ADR-0018 idempotent fork. Write surface (creates a new run). Behind approval. Closes the "retry from step N with different inputs" loop without re-running the entire pipeline.

**Stage 5 — resources, eventually.** When MCP client support for resources matures (subscriptions surfacing in host UIs), migrate event streams to `zymi://runs/{run_id}/events` resources with live subscriptions. Tools remain the explicit-call surface; resources become the streaming surface. Not v1 because Claude Desktop / Cursor resource UX is weak as of 2026-05.

## Rejected alternatives

- **Resources in v1.** Semantically the right MCP primitive for read-only event streams with subscriptions, but agent reliability for resources is worse than for tools across current hosts. Defer until host clients ship better resource handling (Stage 5).
- **Default-on observability.** Tool catalog bloat is real; many `zymi mcp serve` deployments will be agent-tool-only and don't need introspection. Same opt-in discipline as `expose:` in ADR-0033.
- **Subscriptions / live tail in v1.** Interesting but adds notification plumbing. SEP-1686 `notifications/progress` (ADR-0033 sync mode) and `tasks/get` (async mode) already cover the "is it still running" question at the protocol level for the active-run case. Historical inspection (the actual gap) is fine over plain request/response.
- **Editing in v1.** High-leverage but premature without Stage 2 read tools to make the editing context useful, and without explicit ADR-0022 approval integration on the MCP write path. Phased release with hard gates is the correct shape, not a single big-bang ADR. See [[feedback_narrow_speculative_scope]].
- **Combine with ADR-0033 instead of a new ADR.** Observability is conceptually adjacent but the consequence surface (security scope, token budget, recursion, eventual write path) is distinct enough to warrant its own decision record. Keeps ADR-0033 stable as the "expose pipelines" decision.
- **Expose the full event union schema in `events()`.** Honest but unusable — agents trip on union variants. Flat projection with summary string is the pragmatic call; deep payloads live in `step_io`.
