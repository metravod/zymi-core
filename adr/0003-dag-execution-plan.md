# DAG Execution Plan — Acyclic, Level-Based Parallelism

Date: 2026-04-04

## Context

Pipeline steps declare `depends_on` edges forming a directed graph. We need a runtime execution plan that determines step ordering and identifies parallelism opportunities. Key questions: allow cycles (state-machine semantics) or enforce DAG? Which algorithm for topological sort?

## Decision

- Pipelines are **strictly acyclic** (DAG). Cycles are rejected at validation time (DFS in `validate.rs`) and at plan-build time (Kahn's algorithm in `dag.rs`).
- **Kahn's algorithm** (BFS topological sort) chosen over DFS-based topo sort because it naturally produces **execution levels** — groups of steps with no inter-dependencies that can run in parallel.
- `ExecutionPlan` returns `Vec<Vec<String>>` where each inner vec is one level. Levels execute sequentially; steps within a level execute concurrently.
- Step ordering within a level is **lexicographically sorted** for deterministic plans.

## Alternatives Considered

- **petgraph crate**: Too heavy for our use case — we only need topo sort, not general graph algorithms. Kahn's is ~60 lines.
- **DFS topo sort**: Produces a valid ordering but doesn't group parallel steps naturally. Would need a second pass to compute levels.
- **Allowing cycles**: Would enable retry/loop semantics but breaks event-sourcing replay guarantees (non-termination risk, unbounded event streams). Loops should be modeled as pipeline re-invocation, not in-graph cycles.

## Consequences

- Replay is deterministic — same inputs always produce the same execution order.
- Parallel execution of independent steps is a first-class concept, not an optimization.
- Retry/loop patterns must be implemented externally (re-run pipeline, not cycle in graph).
- `ExecutionPlan` is the runtime contract between config layer and future executor.
