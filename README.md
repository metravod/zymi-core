<p align="center">
  <img src="https://raw.githubusercontent.com/metravod/zymi-core/main/assets/zymi-badge.png" alt="zymi" width="220" />
</p>

<h1 align="center">zymi-core</h1>

<p align="center"><em>Compile what your agent figured out into pipelines that run without it — declarative, event-sourced, approval-gated, callable from any MCP host.</em></p>

<p align="center"><sub>Pronounced <em>zoomi</em> — like dog zoomies.</sub></p>

<p align="center">
  <a href="https://pypi.org/project/zymi-core/"><img src="https://img.shields.io/pypi/v/zymi-core.svg?logo=pypi&logoColor=white" alt="PyPI" /></a>
  <a href="https://pypi.org/project/zymi-core/"><img src="https://img.shields.io/pypi/pyversions/zymi-core.svg?logo=python&logoColor=white" alt="Python versions" /></a>
  <a href="https://github.com/metravod/zymi-core/actions/workflows/ci.yml"><img src="https://github.com/metravod/zymi-core/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT" /></a>
  <a href="llms.txt"><img src="https://img.shields.io/badge/llms.txt-%E2%9C%93-8A2BE2" alt="llms.txt" /></a>
</p>

---

## Why zymi-core?

You ask your coding agent to add a VPN user, deploy a service, or clean up a server. It figures it out — after some trial and error. A week later you ask again, and it figures it out again, slightly differently. Then the agent is rate-limited, offline, or simply not there, and nobody remembers the steps.

zymi is for the moment you notice that. The procedure the agent converged on gets **compiled** into a YAML pipeline — most steps are plain deterministic tool calls, an LLM appears only where judgment is actually needed — and from then on:

- **It runs without the agent.** `zymi run add_vpn_user` from any terminal. Tool-only pipelines need no model and no API key at all, and missing inputs are asked interactively.
- **Your agent still uses it** — as one MCP tool, via `zymi mcp serve`, instead of re-deriving the procedure every time.
- **Dangerous steps wait for a human.** Steps emit *intentions* (shell, file write, HTTP) that pass policy, contracts and optional approval before anything happens. Over MCP the approval is a native approve/deny form in the calling agent's UI.
- **Every run is on the record.** Each state change is an immutable, hash-chained event: replay it, fork-resume it from any step, browse it in a TUI, or let the agent read its own run trace to explain a failure.

zymi is deliberately *not* an autonomous agent, an IDE plugin or a chat UI. It is the governed, reproducible layer *underneath* them: the place where the behaviour you want to keep stops being emergent.

---

## Two minutes: a personal pipeline library

```bash
uv tool install zymi-core       # one-time; puts `zymi` on PATH globally
zymi init --home                # your library at ~/.zymi
zymi ls                         # from any directory
zymi run hello                  # asks for its input, runs, no LLM involved
```

> Don't have `uv`? `curl -LsSf https://astral.sh/uv/install.sh | sh` (macOS/Linux) or `irm https://astral.sh/uv/install.ps1 | iex` (Windows).

`~/.zymi` is an ordinary zymi project that every command falls back to when you're not inside another one ([ADR-0044](adr/0044-home-project-and-named-providers.md)). Add your own pipeline — say, a site check:

```yaml
# ~/.zymi/tools/http_status.yml
name: http_status
description: "HTTP status code of a URL"
parameters:
  type: object
  properties:
    url: { type: string }
  required: [url]
implementation:
  kind: shell
  command_template: "curl -sS -o /dev/null -w '%{http_code}' ${args.url}"
```

```yaml
# ~/.zymi/pipelines/site_check.yml
name: site_check
description: "Is the site up?"
inputs:
  - name: url
    required: true
    description: "Full URL, e.g. https://example.com"
steps:
  - id: status
    tool: http_status
    args: { url: "${inputs.url}" }
output:
  step: status
```

Allow the command in `~/.zymi/project.yml` (`policy.allow: ["curl *"]` — anything unlisted asks for approval first), then `zymi run site_check`. The run, its arguments and its output are now in `~/.zymi/.zymi/events.db`; `zymi observe` shows them.

**Hand it to your agent.** Point any MCP host at the library and every pipeline with an `expose.mcp:` block becomes a tool:

```json
{ "command": "zymi", "args": ["mcp", "serve", "--dir", "/Users/you/.zymi"] }
```

**Add a model only where it earns its place.** Declare endpoints once per machine in `~/.zymi/providers.yml` and reference them by name from any project — `llm: neuraldeep`, or `llm: { use: neuraldeep, model: other }`. Keys live in `~/.zymi/.env`. A provider that can't be resolved only disables agent steps; the tool-only pipelines keep working.

Full walkthrough in [docs/getting-started.md](docs/getting-started.md).

---

## What's in the box

### Pipelines — DAGs of agent, tool and ask steps

A pipeline is a list of steps with `depends_on:` edges; independent steps run in parallel. Each step is one of:

- a **deterministic tool step** ([ADR-0024](adr/0024-deterministic-tool-steps.md)) — direct dispatch with templated args, no LLM hop;
- an **agent step** — an LLM ReAct loop with a tool allowlist;
- an **ask step** ([ADR-0042](adr/0042-mcp-sampling-ask-step.md)) — the run parks and asks *whoever called it* (a human at the terminal under `zymi run`, the connected agent under `zymi mcp serve`), then resumes with the answer. No second model to configure.

```yaml
steps:
  - id: recon                            # deterministic — no LLM
    tool: disk_report
    args: { host: "${inputs.host}" }

  - id: analyse                          # LLM, only for the judgment call
    agent: sysadmin
    task: "What is eating the disk? ${steps.recon.output}"
    depends_on: [recon]

  - id: confirm                          # the caller answers
    ask: "Clean up as proposed?\n${steps.analyse.output}"
    depends_on: [analyse]
```

**Branches** ([ADR-0028](adr/0028-conditional-dag-edges.md)) — a step can carry `when:`; skipped branches cascade to descendants and land in the trace as `StepSkipped` events. One pipeline can serve several actions:

```yaml
- id: add_client
  tool: vpn_add_client
  args: { email: "${inputs.email}" }
  depends_on: [route]
  when: "${inputs.action} == 'add'"
```

Schema, examples, gotchas → [docs/pipelines.md](docs/pipelines.md).

### Tools — four kinds, one catalogue

- **Declarative shell / HTTP** in `tools/<name>.yml` — no code.
- **Python `@tool`** in `tools/<name>.py` — sync or async, signature → JSON Schema, auto-discovered; runs in the project's own `.venv` ([ADR-0032](adr/0032-install-ux-fetch.md)).
- **MCP servers** — one `mcp_servers:` entry gives N tools, namespaced `mcp__<server>__<tool>` ([ADR-0023](adr/0023-mcp-client-integration.md)).
- **Builtins** — `read_file`, `write_file`, `write_memory`, `execute_shell_command`, `spawn_sub_agent`.

All four emit the same `ToolCallRequested` / `ToolCallCompleted` events. → [docs/tools.md](docs/tools.md)

### zymi as an MCP server

`zymi mcp serve` exposes pipelines as MCP tools over stdio to Claude Code, Claude Desktop, Cursor, or any framework with an MCP adapter ([ADR-0033](adr/0033-mcp-server-pipelines-as-tools.md)). Exposure is opt-in per pipeline:

```yaml
expose:
  mcp:
    name: vpn_provision
    description: "Provision a VPN client: preflight, then an approval-gated add."
    mode: sync                 # or async — the caller task-augments (SEP-1686)
```

- **Approvals render in the caller's UI** — a gated step sends `elicitation/create` back through the live `tools/call`; in Claude Code that's an approve/deny form. Clients without elicitation fail closed.
- **`ask:` steps borrow the caller's brain** — on a task-augmented call the task goes `input_required` with `{ prompt, resume_token }`, the agent answers via `zymi/reasoning/resume`. The answer is recorded, so replay is byte-identical.
- **The agent can debug its own runs** — `--expose-observability` adds read-only `zymi.runs.list` / `.get` / `.events` / `.step_io` ([ADR-0034](adr/0034-mcp-observability-tools.md)).

Honest limits: approvals inside *async* tasks wait on host adoption and time out (sync calls are fully interactive); cancellation is best-effort; arguments cross the boundary as strings; `mcp serve` is Unix-only for now.

### Approvals — event-sourced, restart-safe

`requires_approval: true` on a tool publishes `ApprovalRequested`; a channel routes the human decision back. Channels: `terminal`, `http`, `telegram`, `mcp_elicitation` ([ADR-0022](adr/0022-event-sourced-approvals.md)). Resolution: pipeline override → project default → fail-closed. A crash mid-approval is repaired on restart. → [docs/approvals.md](docs/approvals.md)

### Replay, resume, observe

```bash
zymi runs                                   # pipeline runs
zymi events --stream <run-id>               # every event of one run
zymi verify                                 # hash-chain integrity, with its denominator
zymi observe                                # TUI: runs / DAG / events, live
zymi resume <run-id> --from-step <id>       # fork-resume; upstream steps stay frozen
```

→ [docs/events-and-replay.md](docs/events-and-replay.md)

### Long-running services

`zymi serve` reacts to events from declarative connectors — `http_inbound`, `http_poll`, `cron`, `file_read`, `stdin` — and answers through outputs (`http_post`, `file_append`, `stdout`). That's enough for a full chat bot in YAML: `zymi init --example telegram` scaffolds one with approvals over Telegram buttons. SQLite by default; one `store: postgres://…` line for multi-process serving against shared state. → [docs/connectors.md](docs/connectors.md) · [docs/store-backends.md](docs/store-backends.md)

### Context window management

An agent's context is reconstructed from the event log each iteration, not accumulated in a buffer: older observations are masked in place, and only when the budget is still tight does one summarisation call compact the oldest batch ([ADR-0016](adr/0016-context-window-management.md)). → [docs/context.md](docs/context.md)

---

## Python embedding

With `zymi-core` in a project's venv (`uv add zymi-core`), the same wheel exposes `Runtime`, `Event`, `EventBus`, `EventStore`, `ToolRegistry` and the `@tool` decorator:

```python
from zymi import Runtime

rt = Runtime.for_project(".", approval="terminal")
result = rt.run_pipeline("site_check", {"url": "https://example.com"})
print(result.success, result.final_output)
```

Cross-process patterns (a web app driving `zymi serve` over the shared store) → [docs/python-api.md](docs/python-api.md).

---

## CLI cheatsheet

```bash
zymi init [--home | --example telegram]     # scaffold a project, or the ~/.zymi library
zymi ls                                     # what can I run (falls back to ~/.zymi)
zymi run <pipeline> [-i key=value …]        # one-shot run; asks for missing inputs on a TTY
zymi fetch                                  # uv sync — build ./.venv for @tool deps
zymi serve <pipeline…> | --all              # long-running, event-driven

zymi pipelines                              # step-level view of every pipeline
zymi runs · zymi events · zymi verify · zymi observe · zymi resume

zymi mcp serve [--expose-observability]     # pipelines as MCP tools
zymi mcp probe <name> -- <cmd> [args …]     # smoke a third-party MCP server
zymi schema {project|agent|pipeline|tool|--all}
```

Commands use `--dir`, else the current directory if it is a project, else `~/.zymi`. Full reference → [docs/cli.md](docs/cli.md).

---

## Documentation

- [Getting started](docs/getting-started.md) · [CLI reference](docs/cli.md) · [Project YAML](docs/project-yaml.md)
- [Pipelines](docs/pipelines.md) · [Tools](docs/tools.md) · [Agents](docs/agents.md) · [Approvals](docs/approvals.md)
- [Connectors](docs/connectors.md) · [Store backends](docs/store-backends.md) · [Events and replay](docs/events-and-replay.md) · [Python API](docs/python-api.md)
- [zymi-skill](https://github.com/metravod/zymi-skill) — an Agent Skill that teaches your coding assistant to write zymi-native YAML
- [llms.txt](llms.txt) — flat index of these docs for LLMs and RAG tools
- [`adr/`](adr/) — one short record per architectural decision

---

## Contributing & License

zymi-core is built in Rust and shipped via PyPI. Bug reports, examples and PRs welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for the dev loop, test matrix and ADR workflow.

MIT — see [LICENSE](LICENSE).
