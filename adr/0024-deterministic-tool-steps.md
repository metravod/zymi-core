# Deterministic Tool Steps

Date: 2026-05-04

## Context

Today every entry in `pipelines/*.yml::steps[]` routes through `agent: <name>` (`src/config/pipeline.rs:45-51`). The orchestrator builds an agent context, sends a `task:` prompt to the LLM, runs the ReAct loop, and only then dispatches whatever tool the agent decided to call. This is correct for reasoning steps but is the wrong shape for the cases that motivated this ADR:

- **Cron ingestion** for the planned RAG reference app (`git pull → chunk → vector_upsert`) — no decision space, the model would just be a templated argument router with extra latency, cost, and hallucination surface.
- **Outbound dispatch after agent reasoning** (`agent decides what to send → tool-step actually sends it`) — splitting "decide" and "do" lets the audit log show both, and lets the second part be approved (ADR-0022) and replayed deterministically (ADR-0018).
- **Glue between connectors and agents** (`http_inbound → tool-step transform → agent`) — today this would force a no-op LLM call to massage data.

All three paths exist today only as Python/declarative tools called *from inside* an agent loop. That works for one-off transforms inside a reasoning step but is wrong as a pipeline-level node — costs an LLM hop, blurs the audit trail, and hides deterministic work behind agent semantics.

The piece that's already in place: `ToolCatalog` (`src/runtime/tool_catalog.rs:86`) already unifies builtin / declarative / MCP / Python tool dispatch and exposes `intention(name, args_json) → Intention` and `requires_approval(name) → bool`. ESAA approvals (ADR-0022) gate on `Intention`, so the same gate works for any caller — agent or step.

## Decision

Add a second pipeline step kind: **deterministic tool step**. Same DAG, same `id` / `depends_on` / templating, same event-sourcing — but no agent loop. The step calls a catalog tool with templated arguments and captures the result as the step's output.

### YAML shape

`PipelineStep` becomes an untagged union discriminated by which key is present (`agent:` or `tool:`). No `type:` discriminator — the schema already has only two variants and presence is unambiguous.

```yaml
steps:
  - id: fetch_docs
    tool: git_pull              # any name in the ToolCatalog
    args:
      repo: "${env.DOCS_REPO}"
      ref: main
    # no `task:` — args are explicit, no NL prompt

  - id: chunk
    tool: chunk_markdown
    args:
      paths: "${steps.fetch_docs.output}"
      max_tokens: 800
    depends_on: [fetch_docs]

  - id: index
    tool: vector_upsert
    args:
      chunks: "${steps.chunk.output}"
    depends_on: [chunk]
```

A step **must** specify exactly one of `agent:` or `tool:`. Both is a config error. Neither is a config error. `task:` is required iff `agent:` is set; `args:` is required iff `tool:` is set.

`args:` is a free-form YAML map. Values pass through the existing template resolver (`${inputs.*}`, `${steps.<id>.output}`, `${env.*}` — same allowlist as v0.3 §"\${steps.*}" fix). After resolution, the map is serialized to JSON and handed to the catalog dispatcher. Validation that `args` matches the tool's input schema is **not** done at parse time — the catalog already validates at dispatch and we don't want to duplicate JSON Schema work here. Type errors surface as `ToolFailed` events at runtime.

### Rust shape

```rust
pub struct PipelineStep {
    pub id: String,
    #[serde(flatten)]
    pub kind: PipelineStepKind,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PipelineStepKind {
    Agent { agent: String, task: String },
    Tool  { tool: String, #[serde(default)] args: serde_yml::Value },
}
```

`load_pipeline` keeps its current signature. Validation (`src/config/validate.rs`) gains:

- both keys present → `ConfigError::AmbiguousStep`
- `tool:` references a name unknown to the catalog → `ConfigError::UnknownTool` (catalog handle threaded into validator the same way agent registry already is)
- `args:` contains an `${steps.<other>.output}` ref where `<other>` isn't in `depends_on` → existing DAG-coverage check, extended to scan `args` recursively

Dropping the existing flat `PipelineStep` struct is a breaking config change at the Rust API level, but YAML files written for v0.5 (which only used `agent:` + `task:`) remain valid because they fall into the `Agent` arm via untagged-deserialize.

### Runtime flow

The orchestrator dispatches a tool step in the same async task slot as an agent step. For each step it now branches on `kind`:

- `Agent` arm: unchanged path through `RunPipeline` → context build → ReAct loop.
- `Tool` arm:
  1. `PipelineStepStarted { step_id, kind: "tool" }` published.
  2. Resolve `args` templates against pipeline inputs + upstream step outputs.
  3. `intention = catalog.intention(tool, args_json)`. If `catalog.requires_approval(tool)` is true OR the step has an explicit `requires_approval: true` (future — out of this ADR), publish `ApprovalRequested` and await on the bus. Same code path ESAA already runs for agent-issued tool calls. No new approval surface.
  4. Dispatch via the same `CatalogActionExecutor::execute_*` that the agent path uses today — `ToolCalled` / `ToolReturned` events fire identically, so the TUI and audit log don't need to learn a new event kind.
  5. Step output = stringified return value (tools that already return JSON keep returning JSON; downstream `${steps.<id>.output}` interpolates verbatim — no new shape).
  6. `PipelineStepCompleted { step_id, output }` (or `PipelineStepFailed`).

No new `EventKind` variants. The existing `PipelineStepStarted/Completed/Failed` and `ToolCalled/Returned/Failed` cover everything, and adding a `kind: "tool" | "agent"` discriminator inside `PipelineStepStarted` payload is the only schema delta — backwards-compatible because consumers ignore unknown fields.

### Approvals composition (ADR-0022)

Tool steps participate in approvals through the same `Intention` gate as agent-issued tool calls. The flow:

```
PipelineStepStarted
  → (if requires_approval) ApprovalRequested → … → ApprovalGranted/Denied
  → ToolCalled → ToolReturned
  → PipelineStepCompleted
```

The approval `stream_id` is the pipeline run stream (not the step). This matches how agent-issued approvals already attach. No changes to `approvals:` YAML or to the bus.

### Fork-resume composition (ADR-0018)

Tool steps are **trivially deterministic on replay**: input is the resolved `args` JSON (computed from prior events), output is captured in `PipelineStepCompleted`. Resume from a downstream step replays the already-emitted output without re-dispatching the tool. Resume from the tool step itself re-dispatches — same semantics as resuming an agent step at the tool-call boundary.

### Connectors / outputs composition (ADR-0021)

No overlap. Connectors emit events into the bus; outputs react to events. Tool steps live inside a pipeline DAG and are scheduled by the orchestrator. A connector firing `PipelineRequested` still triggers a pipeline; whether that pipeline's first step is `agent:` or `tool:` is irrelevant to the connector. Tool steps are **not** a replacement for connectors and we won't unify the surfaces.

### MCP dispatch parity

`ToolCatalog` already routes MCP tools through the same `intention()` / dispatch path. A tool step that names an MCP tool works without any MCP-specific code in the step layer. This is load-bearing for the planned RAG reference app where `vector_upsert` / `vector_query` come from a Pinecone MCP server.

### Out of scope

- **`when:` conditions on steps.** Conditional execution is a separate ADR; this one is purely about a new step kind.
- **`each:` / map-over-array** — same.
- **Step-level `requires_approval: true` override.** Catalog-level `requires_approval` covers the demo cases. If we ever need step-level override (e.g. "this tool is normally fire-and-forget but in this pipeline it needs sign-off"), it's a follow-up.
- **Pipeline-call step kind** (calling another pipeline as a step). Different problem; will need its own ADR if/when it comes up.
- **Streaming tool output.** Today step outputs are captured as a single string at completion. No change.

## Consequences

**Pros:**

- Cron ingestion, edit-flow, and glue steps stop paying for unnecessary LLM hops. Cost ↓, latency ↓, determinism ↑.
- The audit log gets a clean separation between "agent decided X" and "system did X" — a recurring confusion point in the v0.3/v0.5 demos.
- Approvals (ADR-0022) and fork-resume (ADR-0018) are inherited for free — no new bus events, no new replay logic.
- Reference app (RAG telegram bot) becomes implementable without folding RAG-specific machinery into core. Pinecone-via-MCP plus Python tools cover the vector path; tool steps wire them into a pipeline.

**Cons:**

- Two step kinds = two error shapes for users. Mitigation: validator rejects ambiguous configs early with a clear message ("steps[2]: cannot specify both `agent:` and `tool:`").
- Schema breakage at the Rust API level for anything that pattern-matches on `PipelineStep` fields directly. Pre-1.0, acceptable; CHANGELOG flags it.
- `args:` is a free-form `serde_yml::Value`, so type errors only surface at dispatch. Acceptable for v0.6; tightening to per-tool input schemas is a future polish (and would require structured input schemas from MCP / Python sides, which we don't always have today).

**Tech implications:**

- `PipelineStep` becomes a struct-with-flattened-enum. `JsonSchema` derive needs verification — `schemars` handles flattened untagged enums but the rendered schema isn't always pretty. Acceptable; we don't publish the JSON Schema as a stable artifact yet.
- `ToolCatalog` must be available to the validator (currently it's built later in `RuntimeBuilder`). Either lift catalog construction earlier, or run the unknown-tool check at runtime-build time rather than parse time. Implementation will pick whichever is less invasive — the choice doesn't affect the user-visible surface.
- ~400 LOC estimate holds: schema split (~60), validator (~80), orchestrator branch (~120), tests (~140).
- No new feature flags. Tool steps work in default / cli / python / postgres builds identically.
