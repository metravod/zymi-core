# Deterministic tool steps fail-fast

Date: 2026-05-07

## Context

ADR-0024 introduced `kind: tool` pipeline steps — a deterministic catalog dispatch with no LLM hop. Until now both `kind: agent` and `kind: tool` shared the same failure handling in `src/handlers/run_pipeline.rs`:

- `ActionExecutor::execute` returning `Err(e)` is folded into the step output as the literal string `"[tool error] {e}"`.
- `run_tool_step` returns `Ok(StepResult { success: false, output })`.
- The level loop sets `overall_success = false` but **does not stop** — subsequent levels run with `${steps.<failed>.output}` = `"[tool error] …"` substituted as if it were a normal value.

That shape is correct for `kind: agent`: the LLM is meant to see the error and decide what to do next. It is wrong for `kind: tool`: a deterministic step has no decider, so feeding the traceback string forward as data is a silent failure. A user who wires `git_pull → chunk → vector_upsert` and watches `git_pull` raise expects the run to stop, not for `chunk` to receive the traceback as `paths:` and continue.

The Python-tool path (`execute_python` in `runtime/action_executor.rs`) makes this especially visible — exception text including stack frames lands inside the next step's templated args.

## Decision

Deterministic tool steps fail-fast at the pipeline level. An `Err` from `ActionExecutor::execute`, an approval denial, a human rejection, or "no approval handler configured" on a `kind: tool` step halts the pipeline.

Mechanics:

- The level loop in `handle()` collects results as before, awaiting all in-flight handles in the current level so siblings finish cleanly and emit their `ToolCallCompleted` / `WorkflowNodeCompleted` events.
- When a `StepResult` from a `PipelineStepKind::Tool` step has `success: false`, the loop captures the first such failure as `halt: Option<String>` and breaks before starting the next level.
- The terminal events still fire: `WorkflowCompleted { success: false }`, `PipelineCompleted { success: false, error: Some(reason), … }`. The shell-pool session is closed in the same finalisation block as a normal run.
- `handle()` returns `Err(reason)` so the CLI exits non-zero and the gRPC/HTTP path surfaces a hard failure rather than a `success: false` envelope.

Agent steps are untouched. `kind: agent` keeps the existing soft-fail behaviour (the LLM observes the tool error and the loop continues) — flipping that would change ReAct semantics, which is out of scope here.

`ResponseReady` was already gated on `overall_success`, so external connector replies (Telegram, Slack, http_post outputs) keep the same not-emitted-on-failure behaviour.

## Consequences

- Pipelines with a `kind: tool` step whose tool raises now stop at that level instead of cascading the error string into downstream args. The audit log gains a `PipelineCompleted` with a populated `error:` field, and CLI exit code is non-zero.
- Parallel siblings within the failing level still complete; their results are recorded in `step_results`. Levels after the failed one do not start.
- Pre-existing pipelines that relied on the old "soft failure" of tool steps to keep going will start halting. None ship in-tree; the change is breaking only for external pipelines authored against the old behaviour. Pre-1.0, acceptable.
- `kind: tool` no longer offers a way to express "best-effort, continue on failure". If that is needed later, it should be an explicit `on_error: continue|fail` knob on the step — not the default. Not adding it now: no concrete consumer.
- Approval denial / human rejection on tool steps now halts too, matching the principle that a deterministic step has no recovery path.
