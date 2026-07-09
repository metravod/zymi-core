# Pipelines

A pipeline is a DAG of steps. Each step is an **agent step** (LLM ReAct loop), a **deterministic tool step** (direct tool dispatch with templated args, no LLM), or an **ask step** (delegate a reasoning question to whoever called the pipeline — ADR-0042). Steps wait for their `depends_on` to complete; independent steps run in parallel.

## Overview

One pipeline per file under `pipelines/<name>.yml`. Run with `zymi run <name>` or react to inbound events with `zymi serve <name>`.

## Schema

```yaml
name: my_pipeline           # required. Must match filename stem.
description: "..."          # optional.

inputs:                     # optional. Declares pipeline parameters.
  - name: topic
    type: string
    required: true

steps:                      # required. One or more steps.
  - id: research            # required. Unique within the pipeline.
    agent: researcher       # agent step: invokes agents/researcher.yml
    task: "Investigate ${inputs.topic}"
    depends_on: []          # optional. Default: empty (root step).
    when: "..."             # optional. Conditional edge (ADR-0028).

  - id: enrich              # tool step (ADR-0024): direct dispatch, no LLM.
    tool: get_weather       # any tool defined in tools/ (declarative or Python @tool)
    args:
      city: "${inputs.topic}"   # ${…} substitution only — no Jinja filters here
    depends_on: [research]

  - id: sanity              # ask step (ADR-0042): the caller answers this.
    ask: "Does this look right?\n${steps.research.output}"
    channel: mcp_reasoning  # optional; defaults to the connected caller / terminal
    depends_on: [research]

  - id: write_up
    agent: writer
    task: |
      Topic: ${inputs.topic}
      Findings: ${steps.research.output}
      Weather: ${steps.enrich.output}
    depends_on: [research, enrich]

output:                     # optional. Declares which step is "the answer".
  step: write_up            # or `any_of: [a, b]` — see "Output resolution"

approval_channel: ops_tg    # optional. Per-pipeline override of project's
                            # `default_approval_channel:`.
```

## Step kinds

A step is **exactly one** of an agent step, a tool step, or an ask step — declaring more than one of `agent:` / `tool:` / `ask:` is rejected at parse time, as is declaring none.

### Agent step

```yaml
- id: respond
  agent: assistant          # references agents/assistant.yml
  task: "${inputs.message}" # the user-message-equivalent for this turn
  depends_on: [...]
```

The agent runs its ReAct loop bounded by its `max_iterations`. The step's `output` is the agent's final response text.

### Tool step (ADR-0024)

```yaml
- id: fetch
  tool: get_user            # any tool in the catalog: builtin
                            # (read_file, write_file, execute_shell_command,
                            # write_memory, spawn_sub_agent), a declarative
                            # tools/*.yml, a Python @tool, or an MCP tool
                            # (mcp__<server>__<name>). There is no implicit
                            # http_get — define a kind: http tool in
                            # tools/get_user.yml or write a Python @tool.
  args:
    id: "${inputs.id}"
  depends_on: []
```

Tool steps dispatch via the same catalogue agents use (declarative / Python / MCP). They emit the same `WorkflowNodeStarted/Completed` + `ToolCallRequested/Completed` events, so they show up in `zymi observe` and `zymi events` indistinguishably from agent-driven calls.

Templated args resolve `${inputs.*}`, `${steps.<id>.output}`, and `${env.*}` on string leaves; the resolved value is then converted to JSON for catalog dispatch.

### Ask step ([ADR-0042](../adr/0042-mcp-sampling-ask-step.md))

```yaml
- id: summarize
  ask: "Summarize this deploy diff in two sentences:\n${steps.diff.output}"
  channel: mcp_reasoning    # optional. Which channel answers; defaults to the
                            # connected caller under serve, else "terminal".
  depends_on: [diff]
```

An **ask step delegates a reasoning question to whoever called the pipeline** instead of reaching for an `llm:` provider. The run **parks**, surfaces the (templated) prompt as a *reasoning request*, and resumes with the answer as the step's `output` — the approval mechanism (ADR-0022) generalized from a *decision* to a *value*. Use it for "the caller can just answer this" work (*summarize this*, *does this look right?*) when the thing that invoked the pipeline is already a capable model.

- **No `llm:` required.** An `ask:` step does **not** count toward the "needs a provider" check (ADR-0041), so a pure tool + ask pipeline builds with no model configured.
- **Who answers, by context:**
  - Under **`zymi mcp serve`** — the connected agent. The parked step surfaces on the task as `input_required` with `{ status: needs_reasoning, prompt, resume_token }`; the caller reasons and calls `zymi/reasoning/resume { resume_token, answer }`. See [ADR-0033](../adr/0033-mcp-server-pipelines-as-tools.md) and [ADR-0042](../adr/0042-mcp-sampling-ask-step.md).
  - Under **`zymi run` / `zymi resume`** — a human at the terminal (the zero-config reasoning channel), or any configured reasoning channel.
- **Fails closed.** No channel able to answer, or the caller never answers within the timeout → the step fails closed with a clear message (never a silent fallback to an HTTP model). The prompt and answer are recorded as `ReasoningRequested` / `ReasoningAnswered` events, so a replay reads the recorded answer back and **never re-asks** (byte-identical, ADR-0018/0040).
- **Security — the answer is untrusted.** An ask answer is model-generated free text over often attacker-influenced context. It becomes an ordinary step output and flows into downstream `${steps.<id>.output}` through the *same* path as any tool output — so the same sink guards apply (shell/HTTP template-escaping, file deny-list). Treat it as data, never trusted step output: don't splice it into a raw `execute_shell_command` any more than you would a tool output.

## Interpolation

Inside string fields (`task`, `args` values):

- `${inputs.<key>}` — pipeline input. Set via `zymi run … -i key=value` or by a connector's `pipeline_input:` field.
- `${steps.<id>.output>}` — string output of an upstream step. **The referenced step MUST be in this step's `depends_on`** — otherwise the template fails at runtime.
- `${env.<NAME>}` — environment variable.
- `${var_name}` — entry from project-level `variables:` (resolved at parse time, not runtime).

## Output resolution

`output:` selects the step whose `output` is treated as "the answer" — the value emitted as `ResponseReady.content` and surfaced as `PipelineCompleted.final_output`. Two shapes (ADR-0029):

```yaml
output:
  step: write_up            # single declared terminal (legacy form)
```

```yaml
output:
  any_of: [smalltalk, knowledge]   # routed terminal (ADR-0029)
```

`any_of:` walks the list **in declared order** and picks the first step that was not skipped. It is the form to use with `when:` (ADR-0028): the router agent decides which branch fires, exactly one survives, and `any_of:` picks the survivor without you having to repeat the routing logic on the output side.

Failure modes:

- `step:` form, declared step was skipped → run fails (`output step 'X' was skipped`). Silently picking another branch would defeat the trace.
- `any_of:` form, every candidate was skipped → run fails (`all any_of outputs were skipped: [...]`).

Successful resolution emits an `OutputResolved { chosen_step, via }` event between `WorkflowCompleted` and `ResponseReady`, so `zymi observe` / `zymi events` show which branch became the answer.

## DAG semantics

- Steps with empty `depends_on:` (or absent) are roots.
- Steps run as soon as all `depends_on` complete.
- Independent branches run in parallel.
- Cycles are rejected at validation time.
- A step that fails halts its descendants but lets parallel branches finish.

`zymi observe` renders the DAG in real time. The hash-chained event stream lets you fork-resume from any step (see [docs/events-and-replay.md](events-and-replay.md)).

## Conditional branching ([ADR-0028](../adr/0028-conditional-dag-edges.md))

A step can carry an optional `when:` predicate. When it evaluates to false, the step is **skipped** — its body never runs, no `StepResult` is recorded, and a `StepSkipped { step_id, reason: "when=false" }` event is appended. The DAG topology (and therefore parallelism) is unchanged; `when:` is a runtime filter, not an edge.

**Cascade-skip:** any step whose `depends_on` contains a skipped step is itself skipped without evaluating its own `when:` (reason `"ancestor_skipped"`). This is what makes routing through an agent decision express cleanly without join semantics: every branch is its own terminal arm.

### Concierge pattern — agent picks one of N branches

The agent routes by calling a tool whose return value is the branch label. Downstream steps gate on `${steps.<router>.output}`:

```yaml
name: concierge
inputs:
  - { name: query, type: string, required: true }

steps:
  - id: router
    agent: concierge        # system prompt: "Decide and call route('short' | 'rag')."
    task: "Pick a route for: ${inputs.query}"

  - id: short_answer
    agent: helper
    task: "Answer briefly: ${inputs.query}"
    depends_on: [router]
    when: "${steps.router.output} == 'short'"

  - id: rag_lookup
    tool: pinecone_query
    args: { query: "${inputs.query}" }
    depends_on: [router]
    when: "${steps.router.output} == 'rag'"

  - id: rag_answer
    agent: writer
    task: "Answer using: ${steps.rag_lookup.output}"
    depends_on: [rag_lookup]
    # No when: needed — cascade-skip handles it when rag_lookup was skipped.

output:
  step: rag_answer            # or short_answer, declared via two `output` arms
                              # is not yet supported; see "Gotchas" below.
```

### Expression syntax

Templates resolve first; the parser then sees fully-substituted strings.

```
EXPR := COMP (LOGIC COMP)*
COMP := VALUE OP VALUE
OP    := '==' | '!='
LOGIC := '&&' | '||'
VALUE := single-quoted-string | unquoted-token
```

`&&` and `||` have **equal precedence** and evaluate left-to-right. Comparison is byte-exact — no case-folding, no trimming. No parentheses, no `in [...]`, no regex — keep predicates flat.

```yaml
when: "${steps.router.output} == 'rag'"
when: "${steps.router.output} != 'short' && ${inputs.mode} == 'fast'"
```

### Validation

The config validator rejects, at load time:

- `when:` on a step with empty `depends_on:` — there is nothing to branch on.
- Syntactically invalid `when:` expressions.
- `${steps.X.output}` references in `when:` that aren't in this step's `depends_on:` (would race against an unrelated branch).

### Output-step skipped

If `output.step` resolves to a skipped step, the pipeline fails hard: `PipelineCompleted { success: false, error: "output step '<id>' was skipped" }` and `zymi run` exits non-zero. This is intentional — silently returning empty output would defeat the point of making routing decisions traceable.

### Resume

Skipped steps don't enter `frozen_outputs`. On `zymi resume`, their `when:` re-evaluates against frozen ancestor outputs and yields the same answer by construction (string compare is deterministic). No special skip-state persistence is needed.

### What's out of scope (today)

- Multi-field routing like `${steps.router.route}` — requires structured agent output, separate ADR.
- `depends_on_any` / join-after-branch — cascade-skip is the only join mode.
- Numeric / regex / `in [...]` comparisons in `when:`.
- `on_error: continue` for tool steps — see [ADR-0027](../adr/0027-deterministic-tool-step-fail-fast.md) for the fail-fast posture.

## Examples

**Single-step pipeline:**

```yaml
name: main
steps:
  - id: respond
    agent: default
    task: "${inputs.task}"
output:
  step: respond
```

**Parallel search → analyse → report:**

```yaml
name: research
inputs:
  - { name: topic, type: string, required: true }

steps:
  - id: search_web
    agent: researcher
    task: "Search the web for: ${inputs.topic}"

  - id: search_deep
    agent: researcher
    task: "Find in-depth articles about: ${inputs.topic}"

  - id: analyse
    agent: researcher
    task: |
      Cross-reference findings. Identify themes and contradictions.
    depends_on: [search_web, search_deep]

  - id: write_report
    agent: writer
    task: "Write a structured report based on: ${steps.analyse.output}"
    depends_on: [analyse]

output:
  step: write_report
```

**Mixed pipeline (deterministic + agent):**

Assumes you have defined `tools/fetch_ticket.yml` (kind: http) and
`tools/slack_post.yml` (kind: http) — there are no built-in `http_get`
or `slack_post` tools, the tool step dispatches whatever is in the
catalog.

```yaml
name: triage
inputs:
  - { name: ticket_id, type: string, required: true }

steps:
  - id: fetch_ticket           # deterministic — no LLM
    tool: fetch_ticket         # → tools/fetch_ticket.yml (kind: http)
    args:
      ticket_id: "${inputs.ticket_id}"

  - id: classify               # LLM
    agent: classifier
    task: "${steps.fetch_ticket.output}"
    depends_on: [fetch_ticket]

  - id: notify                 # deterministic
    tool: slack_post           # → tools/slack_post.yml (kind: http)
    args:
      channel: "#triage"
      text: "Ticket ${inputs.ticket_id} → ${steps.classify.output}"
    depends_on: [classify]
```

## Gotchas

- **`depends_on` is checked.** Referencing `${steps.<id>.output}` for a step not in `depends_on` fails at runtime even if the referenced step happens to have completed.
- **Tool step args go through MiniJinja-light templating, not full Jinja.** Only `${…}` substitution; no `{{ … | filter }}`. (MiniJinja templates exist in `body_template` / `command_template` of declarative tools — see [docs/tools.md](tools.md).)
- **`name:` must match the filename stem.** Otherwise validation rejects it.
- **The runtime executes steps as soon as their dependencies resolve** — there's no sequential fallback when a parallel branch is "expected first".

## See also

- [Agents](agents.md), [Tools](tools.md), [Approvals](approvals.md), [Events and replay](events-and-replay.md)
- ADR-0018 (fork-resume), ADR-0024 (deterministic tool steps), ADR-0028 (conditional DAG edges), ADR-0042 (ask steps / reasoning delegation)
