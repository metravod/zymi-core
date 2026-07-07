# Make `llm:` optional when no pipeline has an agent step

Date: 2026-07-07

Status: Accepted

## Context

`Runtime` construction requires an LLM provider unconditionally
(`src/runtime/mod.rs:588-599`):

```rust
let llm_config = workspace.project.llm.as_ref().ok_or(
    "no 'llm' section in project.yml — configure a provider to run pipelines",
)?;
```

The reasoning in the comment is "a Runtime that cannot run any pipeline is not
a useful Runtime, so fail at build time rather than at first command dispatch."
That reasoning is wrong for a genuinely LLM-free workload. A pipeline built
entirely from `tool:` steps (shell/http/python) and control-flow runs to
completion without ever calling a model.

This surfaced in real use: a deployment pipeline with **zero** `agent:` steps
still refused to start, forcing a dummy `llm: { provider: openai, model:
gpt-4o }` block with no key. The engine advertises "declarative deterministic
pipelines with a free audit trail" as a first-class use case (the deploy /
ops story), yet the start contract contradicts it. `project.llm` is already
`Option<LlmConfig>` at parse time (`src/config/project.rs:36`) — only this
build-time check makes it effectively mandatory.

## Decision

Require an LLM provider **only when at least one loaded pipeline contains an
`agent:` step.** At `Runtime::build`:

1. Scan `workspace.pipelines` for any `PipelineStepKind::Agent` (the step
   kinds are already introspectable — see `pipeline.rs:233`).
2. If an agent step exists and no provider override was injected: keep today's
   behavior — `project.llm` is required, and a missing/invalid provider is a
   hard build-time error (unchanged fail-fast).
3. If no agent step exists: `project.llm` is optional. When absent, the
   Runtime is built with no provider. If a provider *is* configured anyway,
   still construct it (so a misconfiguration is caught early and the config
   isn't silently ignored).
4. The no-provider Runtime holds `Option<Arc<dyn LlmProvider>>`. Any agent
   dispatch on such a Runtime is a defensive internal error — but step 1
   guarantees that path is unreachable for a validly-loaded workspace, since
   an agent step implies a provider was required.

### Rejected alternative: lazy provider construction

Build the provider on first agent-step dispatch instead of scanning up front.
Rejected because it **weakens fail-fast for the common mixed pipeline.** Today
a bad `api_key` blows up at startup; under lazy construction it would blow up
mid-run, at step N of a deploy — exactly the "safe to run up to the dangerous
step" property that makes the current design good. Static detection (this
decision) keeps startup validation for anyone who uses an agent and relaxes
the requirement only for workloads that provably never touch a model.

## Consequences

- **Pros:**
  - Deterministic-only projects (deploy/ops pipelines, pure tool DAGs) start
    with no `llm:` block and no dummy provider — the contract now matches the
    advertised use case.
  - Fail-fast is preserved exactly where it matters: any pipeline with an
    agent step still validates the provider at build time.
  - No schema change. `llm:` was already `Option`; this only relaxes a
    runtime check.
- **Cons / limitations:**
  - The check is **static over the loaded workspace.** If a workspace with no
    agent steps later gains one at runtime (not a path that exists today —
    pipelines are loaded at start), the guarantee in step 4 would need
    revisiting. Acceptable given the current load model.
  - A project that *intended* to configure an LLM but has no agent step yet
    (mid-build scaffolding) will no longer be warned that its `llm:` block is
    unused. Minor; step 3 still constructs and validates a present provider.
  - Introduces an `Option<Arc<dyn LlmProvider>>` on the Runtime, so the agent
    dispatch path gains one defensive `ok_or(internal_error)`. Small, local.

## Implementation notes

- Add a helper on `WorkspaceConfig` (or inline in `Runtime::build`) that
  returns `true` if any pipeline step is `PipelineStepKind::Agent`.
- Gate the provider block at `src/runtime/mod.rs:588` on that predicate.
- Change the stored provider field to `Option<...>`; thread the option through
  agent dispatch with a clear internal-error message.
- Test: a workspace with one shell-tool pipeline and no `llm:` builds and runs
  to `PipelineCompleted`; a workspace with an agent step and no `llm:` still
  fails at build with the existing message.
