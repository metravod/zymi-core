# Target Runtime Architecture (Draft v1.1)

Date: 2026-04-07

Status: Partially implemented — slices 1 and 2 landed on 2026-04-07 (Runtime/AppContext + RunPipeline command/handler + ActionExecutor split; PyO3 `Runtime` bridge). See "Slice 1 — what landed" and "Slice 2 — what landed" below. Slice 3 (`EventCommandRouter` extraction) is still open and tracked under "Runtime unification" in `.drift/project.json`. Once slice 3 lands or is retired, this draft will be promoted to a binding ADR.

## Context

After ADR-0012 (cross-process event delivery) the system has three live entrypoints — `zymi` CLI, the Python bridge, and `zymi serve <pipeline>` — each wiring its own copy of `EventStore`, `EventBus`, providers, and contracts. The drift goal "Runtime unification" calls for one canonical execution path with a `Runtime/AppContext` builder and a command/handler shape (`RunPipeline`, `ProcessConversation`, `DecideApproval`, `ReplayStream`). Several adjacent goals (action executor split, event-sourced memory, projections/recovery) are downstream of the same architectural shape.

A first sketch of the target was produced as a layered diagram. This ADR records that sketch (v1), the critique of it, and a corrected v1.1 that we will use as the working reference until implementation invalidates it.

## Original sketch (v1)

```mermaid
flowchart TB
    subgraph Adapters["Adapters / Entrypoints"]
        CLI["CLI"]
        PY["Python API"]
        WEB["Webhook / Bots / Schedulers"]
    end

    subgraph App["Application Layer"]
        RT["Runtime / AppContext"]
        CMD["Commands"]
        HND["Command Handlers"]
    end

    subgraph Domain["Domain Layer"]
        WF["Workflow Domain"]
        ESAA["Intentions / Contracts / Approvals"]
        EVT["Domain Events"]
    end

    subgraph Infra["Infrastructure Layer"]
        STORE["EventStore"]
        BUS["Live Event Stream"]
        PROJ["Projections / Read Models"]
        EXEC["ActionExecutor"]
        LLM["LlmProvider"]
        APPROVAL["ApprovalBroker"]
        OBS["Services / Langfuse"]
    end

    CLI --> CMD
    PY --> CMD
    WEB --> CMD

    CMD --> HND
    RT --> HND

    HND --> WF
    HND --> ESAA
    WF --> EVT
    ESAA --> EVT

    EVT --> STORE
    STORE --> BUS
    STORE --> PROJ
    BUS --> OBS

    HND --> EXEC
    EXEC --> LLM
    EXEC --> APPROVAL
    EXEC --> EVT
```

## What v1 gets right (and v1.1 keeps)

- **CMD → HND with a separate `Runtime/AppContext`** node — matches the command/handler target in the runtime-unification goal.
- **STORE → BUS direction**, not the reverse. This is the answer ADR-0012 already implies: store is the single source of truth, bus is in-process fan-out over persisted events.
- **`ApprovalBroker` as a standalone component**, not embedded in the engine.
- **Projections as first-class consumers**, not bolted on later.
- **Adapters cleanly separated**, with webhooks/schedulers sitting next to CLI and Python now that `zymi serve` exists.

## What v1 gets wrong or leaves out

1. **`ActionExecutor` is placed in Infrastructure.** Executing an Intention is an application/domain concern; only the *ports* it uses (LLM client, HTTP, FS) belong in infra. As drawn, infra emits domain events (`EXEC → EVT`), which inverts the dependency direction.
2. **No read path.** All arrows go handlers → events → store → projections. Nothing flows back from `Projections` into `Handlers`, so the diagram describes a write-only system. Half of CQRS is missing — the half that matters for the event-sourced memory work.
3. **No cross-process command path.** ADR-0012 introduced "command arrives as an event on the bus" (`PipelineRequested` → `zymi serve` → `PipelineCompleted`), but v1 only shows synchronous adapter → command. There is no `BUS → CMD`.
4. **`Workflow Domain` is a flat box.** In an event-sourced system pipelines/agents are aggregates: handler loads them by replaying the store, mutates them, emits new events, persists. The diagram shows only the emit step.
5. **`Runtime/AppContext` has no incoming arrows.** Its whole job is to be *constructed* from config + infra ports, but on v1 it appears as a magic singleton.
6. **The approval loop is half drawn.** `EXEC → APPROVAL` is shown, but the decision returning as a `DecideApproval` command is not.
7. **`Intentions / Contracts / Approvals` are conflated** into one box despite having different lifecycles (Intentions = "what an agent wants", Contracts = policy constraints over intentions, Approvals = decisions about specific intentions).

## Target sketch (v1.1)

```mermaid
flowchart TB
    subgraph Adapters["Adapters / Entrypoints"]
        CLI["CLI"]
        PY["Python API"]
        WEB["Webhooks / Schedulers"]
    end

    subgraph App["Application Layer"]
        RT["Runtime / AppContext<br/>(built from config + infra ports)"]
        CMD["Commands<br/>RunPipeline · ProcessConversation<br/>DecideApproval · ReplayStream"]
        HND["Command Handlers"]
        EXEC["ActionExecutor"]
    end

    subgraph Domain["Domain Layer"]
        AGG["Pipeline / Agent Aggregates"]
        ESAA["Intentions & Contracts"]
        APPR["Approval State"]
        EVT["Domain Events"]
    end

    subgraph Infra["Infrastructure (Ports & Adapters)"]
        STORE["EventStore<br/>SQLite / libSQL / Postgres"]
        WATCH["StoreTailWatcher"]
        BUS["EventBus<br/>in-process fan-out"]
        PROJ["Projections / Read Models<br/>Memory · Conversation · Approvals"]
        LLM["LlmProvider"]
        APPROVAL["ApprovalBroker<br/>terminal / webhook"]
        OBS["Observability Sinks<br/>Langfuse, ..."]
    end

    %% sync command intake
    CLI --> CMD
    PY  --> CMD
    WEB --> CMD

    %% async command intake (cross-process, ADR-0012)
    BUS -- "async cmd events" --> CMD

    %% dispatch
    RT --> HND
    CMD --> HND

    %% CQRS read path
    PROJ -- "query state" --> HND

    %% handler -> domain
    HND --> AGG
    HND --> ESAA
    HND --> APPR
    HND --> EXEC

    %% aggregate rehydration loop
    STORE -- "rehydrate" --> AGG
    AGG --> EVT
    ESAA --> EVT
    APPR --> EVT
    EXEC --> EVT

    %% executor uses infra ports
    EXEC --> LLM
    EXEC --> APPROVAL

    %% persistence and fan-out (ADR-0012)
    EVT --> STORE
    STORE --> WATCH
    WATCH --> BUS
    BUS --> PROJ
    STORE -- "rebuild" --> PROJ
    BUS --> OBS

    %% approval round-trip closes back as a command
    APPROVAL -- "DecideApproval" --> CMD
```

### Deltas from v1

1. `ActionExecutor` moved from Infrastructure into Application, next to `Command Handlers`. Infra now contains only ports (`LlmProvider`, `ApprovalBroker`, `EventStore`, etc.).
2. Added `Projections → Handlers` as the CQRS query edge — the read path that v1 was missing.
3. Added `BUS → CMD` ("async cmd events") for the cross-process command path that ADR-0012 already enables via `zymi serve`.
4. `Workflow Domain` renamed to `Pipeline / Agent Aggregates`, with the rehydration loop made explicit: `STORE → AGG → EVT → STORE`.
5. `Intentions & Contracts` and `Approval State` split into two domain boxes.
6. `Runtime / AppContext` annotated as "built from config + infra ports", making the construction direction explicit instead of leaving RT as a free-floating node.
7. Approval loop closed: `APPROVAL → CMD ("DecideApproval")` shows that approval decisions re-enter through the same command path as everything else.
8. Added `StoreTailWatcher` as the named bridge between `STORE` and `BUS` so the diagram matches what is in the code today.
9. `Projections` are fed by both `BUS` (live updates) and `STORE` (rebuild path), instead of only one.

## Open questions (to resolve before this becomes a binding ADR)

- **Aggregates: rehydrate-from-store or read-model-backed?** v1.1 shows handlers loading aggregates by replay from the store. For long-lived pipelines that may be too expensive — a snapshot/projection-backed aggregate is the natural follow-up, but it conflates the "Recovery and projections" goal with this one. We will pick a side once event-sourced memory lands.
- **Where does the async-command router live?** v1.1 draws `BUS → CMD` as one edge, but in code that needs a small subscriber that maps event types to commands. Whether it sits in Application as `EventCommandRouter` or inside each handler is undecided.
- **Crate boundaries vs module boundaries.** The four layers can be enforced as separate crates (`zymi-domain`, `zymi-app`, `zymi-infra`, `zymi-adapters`) or as modules inside `zymi-core` with discipline. Splitting crates pays for itself only if we expect external consumers of the domain layer.
- **`StoreTailWatcher` poll/lag policy** is currently a hidden default (~100ms). It belongs in the runtime contract, not in `events::store::watcher.rs` as a constant. Will be promoted as part of runtime unification.
- **Backpressure on projections.** If a projection lags the bus, do we drop, block, or buffer? Undecided; depends on whether projections become load-bearing (memory read model) or stay read-mostly (audit).

## Slice 1 — what landed (2026-04-07)

The smallest viable slice from the "Next steps" section is now in `main`:

- **`src/runtime/`** — `Runtime` + `RuntimeBuilder` own the per-project wiring (workspace, project root, store, bus, LLM provider, contracts, orchestrator, approval handler, action executor, tail poll interval). Defaults match the inline construction the old engine entrypoints did, so behaviour is preserved.
- **`src/commands.rs`** — only [`RunPipeline`] is shipped; the other three commands from v1.1 are deferred until they have a real caller.
- **`src/handlers/run_pipeline.rs`** — canonical pipeline execution path. Body is the former `engine::run_pipeline_for_request` loop, but it pulls every dependency from the runtime instead of constructing it locally, and routes approved tool calls through `ActionExecutor` instead of calling `execute_builtin_tool` directly.
- **`src/runtime/action_executor.rs`** — `ActionExecutor` trait + `BuiltinActionExecutor` default implementation. Per-call `ActionContext` carries the per-run `MemoryStore` so memory stays isolated between runs even when many runs share one runtime (preserves today's `zymi serve` behaviour).
- **`src/engine/mod.rs`** — `engine::run_pipeline` and `engine::run_pipeline_for_request` survive as `#[deprecated]` thin wrappers that build a `Runtime` and dispatch the new handler. Existing tests and external callers keep working.
- **`src/cli/run.rs`** — builds a `Runtime` once with a `TerminalApprovalHandler`, dispatches `RunPipeline::new(...)`. No more direct `engine::run_pipeline` call.
- **`src/cli/serve.rs`** — builds a `Runtime` once at startup, spawns the `StoreTailWatcher` against `runtime.store()/bus()` with `runtime.tail_poll_interval()`, and on each matching `PipelineRequested` event dispatches `RunPipeline::from_request(...)`. The poll interval is now a runtime field, not a hidden 100 ms constant.

What this **does not** include (still under "Runtime unification" in `.drift/project.json`):

- **Python bridge port (slice 2).** Landed on 2026-04-07; see next section.
- **`EventCommandRouter` extraction (slice 3).** The `PipelineRequested → RunPipeline` translation is still inlined in `cli/serve.rs`. Extraction is deferred until a second async command type appears, since premature extraction would be a one-caller abstraction.
- Aggregate rehydration vs read-model-backed handlers, projection-backed memory, recovery — all still downstream of the event-sourced workflow memory goal.

## Slice 2 — what landed (2026-04-07)

Python stops shelling out to `cli_main`: it builds a [`Runtime`] and dispatches [`RunPipeline`] commands through the same handler `zymi run` and `zymi serve` use. The "Adapters → Application Layer" arrow in v1.1 now has three concrete callers (CLI, Python, `zymi serve`), not two.

- **`src/python/py_runtime.rs`** — new PyO3 module behind `#[cfg(feature = "runtime")]` exposing `Runtime`, `RunPipelineResult`, `StepResult` Python classes. `Runtime.for_project(path, approval="terminal")` loads a project from disk and builds the runtime; `approval` accepts `"terminal"` (default, matches `zymi run`) or `"none"`. `run_pipeline(name, inputs)` blocks on the shared tokio runtime and returns a `RunPipelineResult`. `bus()` / `store()` return `PyEventBus` / `PyEventStore` wrapping the runtime's own `Arc`s, so Python subscribers see the events the handler publishes — no second bus over the same SQLite file.
- **`src/python/bus.rs`, `src/python/store.rs`** — added `pub(crate) fn from_arc(...)` constructors so the new `Runtime` class can hand out Python wrappers over its shared infrastructure instead of constructing fresh ones.
- **`src/approval.rs`** — `TerminalApprovalHandler` moved out of `src/cli/approval.rs` into the root approval module. It never had a real dependency on the `cli` feature, and keeping it behind the `cli` gate would have forced the new Python `Runtime.for_project(..., approval="terminal")` path to pull in clap. `src/cli/run.rs` / `src/cli/serve.rs` updated to import it from `crate::approval`; `src/cli/approval.rs` deleted.
- **`zymi_core/__init__.py`** — re-exports `Runtime`, `RunPipelineResult`, `StepResult` alongside the existing classes, so `from zymi_core import Runtime` works in user scripts.

Pluggable Python approval handlers (where a Python callable is invoked from inside the tokio runtime under the GIL) and asyncio integration are **not** in slice 2 — they are follow-ups. Python users today get the same fail-closed terminal prompt the CLI uses, or `approval="none"`.

## Consequences

- **Pros**: Gives the runtime unification work a concrete target instead of "command/handler shape, somehow". Makes the read path explicit, so the event-sourced memory goal has a place to land. Surfaces the cross-process command path as a first-class concept rather than an `zymi serve` quirk. Aligns the diagram with what ADR-0012 already shipped.
- **Cons**: Adds one more layer of indirection (Runtime, Commands, Handlers, Executor) over today's direct `engine::run_pipeline`. Migration is non-trivial: every entrypoint and the Python bridge will need to move to the new wiring at the same time, otherwise we end up with three implementations instead of one.
- **Next steps**: (1) land the safety-bug fix (`requires_approval` downgrade) on top of the current shape — it does not depend on this rewrite. (2) Start runtime unification with the smallest viable slice: introduce `Runtime/AppContext` + `RunPipeline` command/handler, port `zymi run` to it, and only then port Python and `zymi serve`. (3) Resolve the open questions above as that slice forces them.
