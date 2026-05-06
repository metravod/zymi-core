<p align="center">
  <img src="https://raw.githubusercontent.com/metravod/zymi-core/main/assets/zymi-badge.png" alt="zymi" width="220" />
</p>

<h1 align="center">zymi-core</h1>

<p align="center"><em>dbt for AI workflows — declarative agents, deterministic replay, human-in-the-loop, all from YAML.</em></p>

<p align="center"><sub>Pronounced <em>zoomi</em> — like dog zoomies.</sub></p>

<p align="center">
  <a href="https://pypi.org/project/zymi-core/"><img src="https://img.shields.io/pypi/v/zymi-core.svg?logo=pypi&logoColor=white" alt="PyPI" /></a>
</p>

---

## Why zymi-core?

Most agent frameworks are imperative Python: write a script that makes LLM calls, persist some messages, hope you logged enough to debug a bad run later.

`zymi-core` inverts that:

- **Declarative, like dbt.** Agents, pipelines, tools, connectors, approvals — all YAML. The engine validates and runs them as a DAG.
- **Event-sourced.** Every state change is an immutable, hash-chained event. Runs are replayable, resumable, and auditable without extra logging.
- **Boundary-safe.** Agents emit *intentions* (run shell, write file, call HTTP) that pass through policy + contracts + optional human approval before execution. The risky thing doesn't happen until someone says yes.

Bring a useful agent online in minutes without writing code. A year later, still answer *exactly what this agent did* on any past run.

📚 **AI-assistant friendly out of the box.** Every `zymi init` scaffold drops an `AGENTS.md` into the user's project — vocabulary, file map, task→file routing. Claude Code / Cursor / Aider read it automatically; the YAML they help you write gets noticeably more correct.

---

## Run a Telegram agent in two minutes

This is the canonical demo — a real chat bot, wired declaratively.

```bash
pip install zymi-core

mkdir telegram-agent && cd telegram-agent
zymi init --example telegram

# 1. Create a bot via @BotFather in Telegram; copy the token.
# 2. Fill .env:
cp .env.example .env       # edit TELEGRAM_BOT_TOKEN + OPENAI_API_KEY
# 3. Open project.yml, replace "your_username_here" with your actual
#    Telegram username (no @). Keeps strangers out of the bot.

set -a; source .env; set +a
zymi serve chat
```

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

Tools with `requires_approval: true` publish `ApprovalRequested` on the bus; an approval channel routes a human decision back. Three channels in the box: `terminal`, `http`, `telegram` ([ADR-0022](adr/0022-event-sourced-approvals.md)).

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

The agent's working context is reconstructed from the event log each iteration, not accumulated in a buffer. Older tool observations are masked in-place (~2× cost reduction, no extra LLM calls). When the budget still gets tight, hybrid compaction summarises the oldest masked batch with one fast LLM call. Tunable in `runtime.context:` ([ADR-0016](adr/0016-context-window-management.md)).

### JSON Schemas for configs

IDE autocomplete and LLM-assisted YAML come free:

```bash
zymi schema project          # draft-07 JSON Schema for project.yml
zymi schema --all
```

---

## Python embedding

The same `pip install zymi-core` exposes a Python API: `Runtime`, `Event`, `EventBus`, `EventStore`, `Subscription`, `ToolRegistry`, plus the `@tool` decorator.

```python
from zymi_core import Runtime

rt = Runtime.for_project(".", approval="terminal")
result = rt.run_pipeline("chat", {"message": "hello"})
print(result.success, result.final_output)
```

`rt.bus()` and `rt.store()` share `Arc`-handles with the runtime — Python subscribers see exactly what the handler publishes.

**Cross-process pattern** (Django view / Celery task drives `zymi serve` over the shared store):

```python
import uuid
from zymi_core import Event, EventBus, EventStore

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
zymi init [--example telegram]              # scaffold a project
zymi run <pipeline> -i key=value …          # one-shot run
zymi serve <pipeline>                       # long-running: react to PipelineRequested

zymi runs                                   # list pipeline runs
zymi events [--stream ID] [--kind TAG]      # query event log
zymi verify [--stream ID]                   # hash-chain integrity check
zymi observe [--run ID]                     # interactive TUI
zymi resume <run-id> --from-step <id>       # fork-resume

zymi mcp probe <name> -- <cmd> [args …]     # smoke an MCP server
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
