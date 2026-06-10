<p align="center">
  <img src="https://raw.githubusercontent.com/metravod/zymi-core/main/assets/zymi-badge.png" alt="zymi" width="220" />
</p>

<h1 align="center">zymi-core</h1>

<p align="center"><em>The auditable MCP backend for agents — tools as declarative YAML pipelines: event-sourced, replayable, approval-gated.</em></p>

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

Agent frameworks compete for the *front* of the stack — the loop, the planner, the IDE. zymi owns the **back**: the tools your agent calls.

[`zymi mcp serve`](#zymi-as-an-mcp-server--pipelines-as-tools-for-any-agent) exposes declarative YAML pipelines as MCP tools to any host — Claude Code, Claude Desktop, Cursor, or any framework with an MCP adapter (LangGraph, CrewAI, OpenAI Agents SDK). Unlike a script behind an endpoint, a zymi tool is:

- **Declarative, like dbt.** Agents, pipelines, tools, connectors, approvals — all YAML. The engine validates and runs them as a DAG.
- **Event-sourced.** Every state change is an immutable, hash-chained event. Runs are replayable, resumable, and auditable without extra logging.
- **Boundary-safe — interactively.** Steps emit *intentions* (run shell, write file, call HTTP) that pass through policy + contracts + optional human approval before execution. Over MCP the approval renders as an approve/deny form right in the calling agent's UI; the risky thing doesn't happen until someone says yes.
- **Self-debuggable.** Serve with `--expose-observability` and the agent can introspect its own runs — list them, pull the event trace, read any step's exact I/O — and explain a failure without you opening a log file.

zymi is deliberately *not* an autonomous coding agent, an IDE plugin, or a chat UI — it's the governed tool layer underneath those. It also runs standalone: [bring a Telegram agent online in two minutes](#run-a-telegram-agent-in-two-minutes), no MCP involved. Either way, a year later you can still answer *exactly what this agent did* on any past run.

📚 **AI-assistant friendly out of the box.** Every `zymi init` scaffold drops an `AGENTS.md` into the user's project — vocabulary, file map, task→file routing. Claude Code / Cursor / Aider read it automatically; the YAML they help you write gets noticeably more correct. For agents that *build* zymi projects (rather than work inside one), install [zymi-skill](https://github.com/metravod/zymi-skill) into your assistant — opinionated Agent Skill with activation rules + progressive disclosure references, so the assistant produces zymi-native YAML instead of generic agent advice.

---

## Run a Telegram agent in two minutes

The canonical standalone demo (no MCP host needed) — a real chat bot, wired declaratively.

```bash
uv tool install zymi-core    # one-time; puts `zymi` on PATH globally

mkdir telegram-agent && cd telegram-agent
zymi init --example telegram

# 1. Create a bot via @BotFather in Telegram; copy the token.
# 2. Fill .env:
cp .env.example .env         # edit TELEGRAM_BOT_TOKEN + OPENAI_API_KEY
# 3. Open project.yml, replace "your_username_here" with your actual
#    Telegram username (no @). Keeps strangers out of the bot.

zymi fetch                   # uv sync — builds ./.venv from pyproject.toml
zymi serve chat              # .env is auto-loaded; pipeline runs in ./.venv
```

> **Why `uv tool install` and `zymi fetch`?** `zymi` is a global CLI; your
> project keeps its own `pyproject.toml` + `.venv` for any Python deps your
> `@tool` files import. `zymi fetch` wraps `uv sync` to build that venv, and
> pipeline-run commands transparently re-exec inside it ([ADR-0032](adr/0032-install-ux-fetch.md)).
> Don't have `uv` yet? `curl -LsSf https://astral.sh/uv/install.sh | sh`
> (macOS/Linux) or `irm https://astral.sh/uv/install.ps1 | iex` (Windows).

Message the bot. It replies in seconds. Every inbound message, LLM call, approval decision, and outbound reply is in `.zymi/events.db`; watch live with `zymi observe`.

The whole wiring — Telegram I/O, two-step DAG (`assistant` drafts, `reviewer` polishes), declarative + Python tools, approval channel — lives in YAML. The scaffold also drops `AGENTS.md` so an AI coding assistant can extend the project safely. Concrete demo of:

- **`http_poll` connector** — long-polls Telegram's `getUpdates`, no HTTPS / ngrok needed
- **`http_post` output** — sends each `ResponseReady` back to the user
- **Telegram approval channel** — DMs admins with ✅ / ❌ buttons when the agent calls `broadcast` (`requires_approval: true`)
- **Python `@tool` auto-discovery** — drop `tools/get_weather.py` (sync) or `tools/translate.py` (async) and the agent picks them up

Ask the bot to "announce that we're closing at 5pm" — the agent calls `broadcast`, you get a DM with approve/deny buttons, nothing goes out until you click. End-to-end audit trail in `zymi events`.

Full setup in [docs/getting-started.md](docs/getting-started.md). Connector deep-dive in [docs/connectors.md](docs/connectors.md). Approvals in [docs/approvals.md](docs/approvals.md).

---

## What's in the box

### Pipelines — DAGs, agent steps, deterministic tool steps

A pipeline is a list of steps with `depends_on:` edges. Independent steps run in parallel. Each step is either an **agent step** (LLM ReAct loop) or a **deterministic tool step** ([ADR-0024](adr/0024-deterministic-tool-steps.md)) — direct dispatch with templated args, no LLM hop, but the same event envelope.

Mix them freely:

```yaml
steps:
  - id: fetch                            # deterministic — no LLM
    tool: http_get
    args: { url: "https://api.example.com/${inputs.id}" }

  - id: classify                         # LLM
    agent: classifier
    task: "${steps.fetch.output}"
    depends_on: [fetch]
```

**Conditional branches** ([ADR-0028](adr/0028-conditional-dag-edges.md)) — a step can gate on an upstream output. Skipped branches cascade to descendants and emit `StepSkipped` events, so routing decisions land in the trace, not in the LLM's head:

```yaml
- id: router
  agent: concierge
  task: "Pick: ${inputs.q}"   # calls route('short' | 'rag')

- id: rag_lookup
  tool: pinecone_query
  args: { query: "${inputs.q}" }
  depends_on: [router]
  when: "${steps.router.output} == 'rag'"
```

Schema, examples, gotchas → [docs/pipelines.md](docs/pipelines.md).

### Tools — four kinds, one catalogue

All four kinds emit identical `ToolCallRequested` / `ToolCallCompleted` events; the agent doesn't know which catalogue a tool came from.

- **Declarative HTTP / shell** in `tools/<name>.yml` — no code.
- **Python `@tool`** in `tools/<name>.py` — sync or async, signature → JSON Schema, auto-discovered.
- **MCP servers** — one `mcp_servers:` entry gives N tools, namespaced `mcp__<server>__<tool>` ([ADR-0023](adr/0023-mcp-client-integration.md)).
- **Builtins** — `read_file`, `write_file`, `write_memory`, `execute_shell_command`, `spawn_sub_agent`.

```python
# tools/get_weather.py — auto-discovered at runtime startup.
from zymi import tool

@tool
def get_weather(city: str) -> str:
    """Return the current weather for a city."""
    return f"sunny in {city}"
```

Schema and the four kinds in detail → [docs/tools.md](docs/tools.md).

### zymi as an MCP server — pipelines as tools for any agent

The mirror of the MCP *client* above: `zymi mcp serve` exposes your pipelines as MCP tools over stdio, so **any** MCP host (Claude Code, Claude Desktop, Cursor, the OpenAI Agents / LangGraph / OpenHands runtimes via their MCP adapters) can call a zymi pipeline as a single tool — no per-runtime glue ([ADR-0033](adr/0033-mcp-server-pipelines-as-tools.md)).

This is the **priority direction for zymi**: own the auditable, event-sourced *back* of the agent stack rather than competing on the front. A pipeline is a tool whose every step is hash-chained, replayable, and resumable — which is exactly what an agent's tool catalogue is missing.

Exposure is opt-in per pipeline (so internal/cron pipelines never leak into agent tool catalogues):

```yaml
# pipelines/research.yml
expose:
  mcp:
    name: research            # tool name (defaults to file stem)
    mode: sync | async        # async hints the caller to task-augment (SEP-1686)
    description: "Deep-research a topic and return a brief."
```

```bash
zymi mcp serve                              # serve all expose:-d pipelines over stdio
zymi mcp serve --include 'research_*' --exclude '*_internal'
```

- **Sync** — `tools/call` blocks until the pipeline finishes; works on every MCP client today. Tool input schema is auto-generated from the pipeline's `inputs:`.
- **Async** — a client that augments the call with a [SEP-1686](https://modelcontextprotocol.io/seps/1686-tasks) task gets a `CreateTaskResult` immediately and polls `tasks/get` / `tasks/result` / `tasks/list`; `tasks/cancel` and `notifications/cancelled` cancel it. The pipeline runs in the background and stays fully observable in the event store.

**Human approvals render in the calling agent's UI.** A pipeline step that trips an [approval](#approvals--event-sourced-restart-safe) sends a server-initiated `elicitation/create` back through the live `tools/call` — in Claude Code that's a native approve/deny form. Approve and the pipeline continues; deny and it halts with the decision in the audit trail; a client without elicitation support fail-closes (`ApprovalDenied{reason: client_no_elicitation}`). Verified live against Claude Code.

**The agent can debug its own runs.** `zymi mcp serve --expose-observability` adds four read-only tools — `zymi.runs.list` / `.get` / `.events` / `.step_io` ([ADR-0034](adr/0034-mcp-observability-tools.md)). Ask the agent *"why did the last run fail?"* and it pulls the event trace and answers with the exact policy verdict and approval decision — introspection other stacks can't expose because the per-step event granularity isn't there. Scoped to the serve session by default; `--observability-scope all` opens the whole store for single-user dev.

**Current limitations (honest list):**

- **Async tasks don't pause for approvals.** The interactive approval bridge above is sync-mode; `input_required` + related-task elicitation on a *task-augmented* call waits on host adoption, so an approval inside an async task times out (auto-deny). Sync calls are fully interactive.
- **Cancellation is best-effort:** the task is aborted, but pipeline steps already in flight (and their side effects) may run to completion.
- **Arguments cross the boundary as strings** — pipelines expecting string `inputs:` are fine; richly typed inputs are stringified.
- Async mode needs a SEP-1686-capable client; `zymi mcp serve` is Unix-only for now (stdio); tasks live for the server process lifetime (no TTL eviction). Hosts may normalise dotted tool names — Claude Code shows `zymi.runs.list` as `zymi_runs_list`.

Design, wire shapes, and the approval bridge → [ADR-0033](adr/0033-mcp-server-pipelines-as-tools.md).

### Connectors and outputs

Inbound: `http_inbound` (webhook), `http_poll` (long-poll), `cron`, `file_read`, `stdin`.
Outbound: `http_post`, `file_append`, `stdout`.

All declarative, all emit events. Filter recipes ([docs/connectors.md](docs/connectors.md#http-poll)):

```yaml
# GitHub — only react to PR opens
filter:
  "$.action":              { equals: "opened" }
  "$.pull_request.draft":  { equals: false }
```

429 + `Retry-After` handled automatically. Cursors persist across restarts. Multi-process `zymi serve` against shared Postgres sees one cursor table, no double-fire.

### Approvals — event-sourced, restart-safe

Tools with `requires_approval: true` publish `ApprovalRequested` on the bus; an approval channel routes a human decision back. Four channels in the box: `terminal`, `http`, `telegram`, and `mcp_elicitation` — the default under `zymi mcp serve`, rendering the approve/deny form in the calling MCP host ([ADR-0022](adr/0022-event-sourced-approvals.md)).

Resolution order: **pipeline override → project default → fail-closed**. A `zymi serve` crash mid-approval is repaired on next start: in-flight requests are redelivered to live channels; expired ones are sealed with `ApprovalDenied{reason: restart_timeout}`.

Full schemas + telegram setup → [docs/approvals.md](docs/approvals.md).

### Replay, resume, observe

```bash
zymi runs                                   # all pipeline runs
zymi events --stream pipeline-chat-abc      # every event in one run
zymi verify --stream pipeline-chat-abc      # hash-chain integrity check
zymi observe                                # 3-panel TUI: runs / DAG / events live

# Fork-resume from a chosen step. Upstream steps are frozen; the fork
# step + DAG-descendants re-run against current configs on disk.
zymi resume pipeline-chat-abc --from-step polish
zymi resume pipeline-chat-abc --from-step polish --dry-run
```

Useful when you're iterating on a prompt: don't re-burn the expensive early steps every time you tweak the later ones. → [docs/events-and-replay.md](docs/events-and-replay.md).

### Store backends

SQLite (default, zero-config) for single-process / dev. Postgres for multi-process `zymi serve` against shared state — one `store: postgres://…` line in `project.yml` ([ADR-0012](adr/0012-cross-process-event-delivery.md)). Same hash-chain semantics either way. → [docs/store-backends.md](docs/store-backends.md).

### Context window management

The agent's working context is reconstructed from the event log each iteration, not accumulated in a buffer. Older tool observations are masked in-place (~2× cost reduction, no extra LLM calls). When the budget still gets tight, hybrid compaction summarises the oldest masked batch with one fast LLM call. Tunable in `runtime.context:` — see [docs/context.md](docs/context.md) for recommended chat / coding / evals profiles ([ADR-0016](adr/0016-context-window-management.md)).

### JSON Schemas for configs

IDE autocomplete and LLM-assisted YAML come free:

```bash
zymi schema project          # draft-07 JSON Schema for project.yml
zymi schema --all
```

---

## Python embedding

When `zymi-core` is in your project's venv (`uv add zymi-core` in a uv
project, or `pip install zymi-core` in a traditional venv), the same wheel
exposes a Python API: `Runtime`, `Event`, `EventBus`, `EventStore`,
`Subscription`, `ToolRegistry`, plus the `@tool` decorator.

```python
from zymi import Runtime

rt = Runtime.for_project(".", approval="terminal")
result = rt.run_pipeline("chat", {"message": "hello"})
print(result.success, result.final_output)
```

`rt.bus()` and `rt.store()` share `Arc`-handles with the runtime — Python subscribers see exactly what the handler publishes.

**Cross-process pattern** (Django view / Celery task drives `zymi serve` over the shared store):

```python
import uuid
from zymi import Event, EventBus, EventStore

store = EventStore(".zymi/events.db")
bus = EventBus(store)

corr = str(uuid.uuid4())
sub = bus.subscribe_correlation(corr)

ev = Event(
    stream_id=f"web-{corr}",
    kind={"type": "PipelineRequested",
          "data": {"pipeline": "research", "inputs": {"topic": "rust event sourcing"}}},
    source="django",
)
ev.with_correlation(corr)
bus.publish(ev)

result = sub.recv(timeout_secs=300)
```

Full surface → [docs/python-api.md](docs/python-api.md).

---

## CLI cheatsheet

```bash
zymi init [--example telegram]              # scaffold a project (writes pyproject.toml too)
zymi fetch                                  # uv sync — build ./.venv from pyproject.toml
zymi run <pipeline> -i key=value …          # one-shot run (re-execs in ./.venv if present)
zymi serve <pipeline>                       # long-running: react to PipelineRequested

zymi runs                                   # list pipeline runs
zymi events [--stream ID] [--kind TAG]      # query event log
zymi verify [--stream ID]                   # hash-chain integrity check
zymi observe [--run ID]                     # interactive TUI
zymi resume <run-id> --from-step <id>       # fork-resume

zymi mcp probe <name> -- <cmd> [args …]     # smoke a third-party MCP server
zymi mcp serve [--expose-observability]     # serve expose:-d pipelines as MCP tools
              [--include G] [--exclude G]   #   + zymi.runs.* introspection tools
zymi schema {project|agent|pipeline|tool|--all}
```

Full reference → [docs/cli.md](docs/cli.md).

---

## Documentation

- [Getting started](docs/getting-started.md) — install → init → first run
- [Project YAML](docs/project-yaml.md) — `project.yml` schema
- [Agents](docs/agents.md) · [Pipelines](docs/pipelines.md) · [Tools](docs/tools.md)
- [Connectors](docs/connectors.md) · [Approvals](docs/approvals.md) · [Store backends](docs/store-backends.md)
- [Events and replay](docs/events-and-replay.md) · [CLI reference](docs/cli.md) · [Python API](docs/python-api.md)
- [llms.txt](llms.txt) — flat index of this documentation tree for LLM scrapers and RAG tools
- `AGENTS.md` — agent-onboarding doc generated by `zymi init` into each new project (vocabulary, file map, task→file routing)
- [`adr/`](adr/) — architectural decision records, one short markdown file per decision

---

## Contributing & License

zymi-core is built in Rust and shipped via PyPI. Bug reports, examples, PRs welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for the dev loop, test matrix, ADR workflow, and how to build from source.

MIT — see [LICENSE](LICENSE).

[mcp]: https://modelcontextprotocol.io
