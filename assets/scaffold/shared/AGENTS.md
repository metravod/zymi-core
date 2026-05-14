# AGENTS.md — guide for AI assistants editing this project

This is a [zymi-core](https://github.com/metravod/zymi-core) project: an
event-sourced agent engine where everything is configured in YAML and
optionally Python. You (the AI assistant) edit YAML and Python files;
zymi runs them.

## Vocabulary

- **Project** — this directory. Has a `project.yml` at the root. Contains
  `agents/`, `pipelines/`, `tools/`, `.zymi/`.
- **Agent** — an LLM with a system_prompt and a list of tools. Lives in
  `agents/<name>.yml`.
- **Pipeline** — a DAG of steps. Each step is either an agent invocation
  OR a deterministic tool call. Lives in `pipelines/<name>.yml`.
- **Step** — one node of a pipeline DAG. `agent: <name>` runs an agent
  loop; `tool: <name>` calls a tool directly with templated args, no LLM.
- **Tool** — something callable from an agent. Four kinds:
  - **Declarative** (`tools/<name>.yml`, `kind: http` or `kind: shell`)
  - **Python** (`tools/<name>.py` with `@tool` from `zymi`, sync or async)
  - **MCP** (`mcp_servers:` in `project.yml`, namespaced
    `mcp__<server>__<tool>`)
  - **Builtin** (shipped by zymi-core: `read_file`, `write_file`,
    `write_memory`, …)
- **Connector** — inbound source of events. Types: `http_inbound`
  (webhook), `http_poll` (long-poll, e.g. Telegram getUpdates), `cron`
  (schedule), `file_read`, `stdin`.
- **Output** — outbound target on event match. Types: `http_post`,
  `file_append`, `stdout`.
- **Approval** — a gated tool call. Tools with `requires_approval: true`
  publish `ApprovalRequested`; an approval channel routes a human
  decision back. Channels: `terminal`, `http`, `telegram`.
- **Stream** — append-only ordered sequence of events. Each pipeline run
  is a stream. Hash-chained per stream for tamper-evidence.
- **Event** — every state change. Hits the bus, gets persisted in
  `.zymi/events.db`, replayable at any time.

## Project layout

```
project.yml                 # top-level config: llm, defaults, policy, contracts,
                            # connectors, outputs, approvals, mcp_servers, store
agents/<name>.yml           # one file per agent
pipelines/<name>.yml        # one file per pipeline
tools/<name>.yml            # declarative tools (HTTP / shell)
tools/<name>.py             # Python tools with @tool decorator
.zymi/                      # runtime data — events.db, connectors.db. DO NOT edit.
.env                        # secrets — gitignored. Auto-loaded by zymi.
```

## Task → file routing

| When you want to … | Edit / run | See |
|--------------------|-----------|-----|
| Add a new agent | `agents/<name>.yml` | docs/agents.md |
| Add a pipeline | `pipelines/<name>.yml` | docs/pipelines.md |
| Add a non-LLM (deterministic) step | In a pipeline step use `tool: <name>` instead of `agent: <name>` | docs/pipelines.md#tool-steps |
| Make a step run conditionally (branching) | Add `when: "${steps.<router>.output} == 'label'"` on the step; needs `depends_on` | docs/pipelines.md#conditional-branching |
| Add an HTTP tool | `tools/<name>.yml` with `kind: http` | docs/tools.md#declarative-http |
| Add a shell tool | `tools/<name>.yml` with `kind: shell` | docs/tools.md#declarative-shell |
| Add a Python tool | `tools/<name>.py` with `@tool` from `zymi` | docs/tools.md#python |
| Add an MCP server | `mcp_servers:` block in `project.yml` | docs/tools.md#mcp |
| Set up an inbound webhook | `connectors:` block, `type: http_inbound` | docs/connectors.md#http-inbound |
| Long-poll an external API (Telegram, …) | `connectors:` block, `type: http_poll` | docs/connectors.md#http-poll |
| Run something on a schedule | `connectors:` block, `type: cron` | docs/connectors.md#cron |
| Send HTTP on agent reply | `outputs:` block, `type: http_post`, `on: [ResponseReady]` | docs/connectors.md#http-post |
| Gate a tool behind human approval | Set `requires_approval: true` on the tool, configure `approvals:` channel in `project.yml` | docs/approvals.md |
| Run multiple `zymi serve` against shared store | `store: postgres://…` in `project.yml` | docs/store-backends.md |
| Replay or fork a failed run | `zymi resume <run-id> --from-step <id>` | docs/events-and-replay.md#fork-resume |
| Inspect what happened | `zymi events` / `zymi runs` / `zymi observe` (TUI) | docs/cli.md |

## Conventions

**Interpolation in YAML strings:**

- `${env.NAME}` — environment variable (loaded from `.env` automatically)
- `${inputs.<key>}` — pipeline input (passed via `-i key=value` or
  connector `pipeline_input`)
- `${steps.<id>.output}` — output of an upstream step (the step MUST be
  in `depends_on`, otherwise resolution fails)
- `${args.<key>}` — tool argument (used inside a tool's `implementation`)

**Templates** (in `body_template`, `command_template`): MiniJinja, e.g.
`{{ event.content }}` or `{{ event.content | tojson }}`. Standard Jinja2
syntax; `tojson` is provided.

**Conditional steps** (`when:` on a pipeline step, ADR-0028): a step with
`when:` runs only if the predicate evaluates true after dependencies
complete. If skipped, the step's body never runs, no `StepResult` is
recorded, and a `StepSkipped` event is appended. Skips cascade — any
descendant with the skipped step in its `depends_on` is also skipped
without re-evaluating its own `when:`. Use this for agent-driven
routing: have the router agent call a tool whose return value is the
branch label, then gate downstream steps on
`${steps.<router>.output} == 'label'`. Grammar: `VALUE (==|!=) VALUE`
joined by `&&` / `||` (equal precedence, left-to-right); values are
single-quoted strings or bare tokens; no parens, no regex, no `in [...]`.
`when:` requires non-empty `depends_on`, and every `${steps.X.output}`
it reads must be a declared ancestor.

**Naming:**

- Tool / agent / pipeline names are lowercase `snake_case`.
- The `name:` field inside `agents/<name>.yml` MUST match the filename.
- MCP tools auto-prefix as `mcp__<server>__<tool>`.

## Don'ts

- **Don't `print()` from Python tools.** Return a string from the
  function. `print` goes to stderr and is not captured.
- **Don't bypass approvals** by deleting `requires_approval`. The bus
  event still emits and downstream consumers will see the unapproved call.
- **Don't reference `${steps.<id>.output}` without `depends_on`.** The
  template fails to resolve at runtime.
- **Don't mutate `.zymi/`.** It's the source of truth for replay; manual
  edits break the hash chain.
- **Don't commit `.env`.** It's gitignored for a reason.
- **Don't write tools that eval/exec untrusted input.** Use the `policy`
  block in `project.yml` to constrain shell access.

## Quick command reference

```bash
# One-shot run with explicit inputs
zymi run <pipeline> -i key=value [-i key=value …]

# Long-running: react to PipelineRequested events from connectors
zymi serve <pipeline>

# Inspection
zymi runs                                # list all pipeline runs
zymi events --stream <stream-id>         # all events for a stream
zymi events --kind <KindTag> [--raw]     # filter by event kind
zymi verify --stream <stream-id>         # hash-chain integrity check
zymi observe                             # 3-panel TUI: runs / DAG / events live

# Replay
zymi resume <run-id> --from-step <id> [--dry-run]

# MCP
zymi mcp probe <name> -- <cmd> [args …]  # smoke a server before wiring it
```

## Reference docs

Online reference for every surface, versioned with the zymi-core repo:

- https://github.com/metravod/zymi-core/blob/main/docs/getting-started.md
- https://github.com/metravod/zymi-core/blob/main/docs/project-yaml.md
- https://github.com/metravod/zymi-core/blob/main/docs/agents.md
- https://github.com/metravod/zymi-core/blob/main/docs/pipelines.md
- https://github.com/metravod/zymi-core/blob/main/docs/tools.md
- https://github.com/metravod/zymi-core/blob/main/docs/connectors.md
- https://github.com/metravod/zymi-core/blob/main/docs/approvals.md
- https://github.com/metravod/zymi-core/blob/main/docs/store-backends.md
- https://github.com/metravod/zymi-core/blob/main/docs/events-and-replay.md
- https://github.com/metravod/zymi-core/blob/main/docs/cli.md
- https://github.com/metravod/zymi-core/blob/main/docs/python-api.md
