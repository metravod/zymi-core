<p align="center">
  <img src="https://raw.githubusercontent.com/metravod/zymi-core/main/assets/zymi-badge.png" alt="zymi" width="220" />
</p>

<h1 align="center">zymi-core</h1>

<p align="center"><em>Declarative agent engine — dbt for AI workflows, with event sourcing built in.</em></p>

<p align="center"><sub>Pronounced <em>zoomi</em> — like dog zoomies.</sub></p>

<p align="center">
  <a href="https://pypi.org/project/zymi-core/"><img src="https://img.shields.io/pypi/v/zymi-core.svg?logo=pypi&logoColor=white" alt="PyPI" /></a>
</p>

---

## Why zymi-core?

Most agent frameworks are imperative Python: you write a script that makes LLM calls, maybe persists some messages, and leaves you guessing about the rest. Debugging a bad run means reading logs, hoping you logged enough.

`zymi-core` inverts that:

- **Declarative, like dbt.** Describe agents, tools, pipelines, and integrations in YAML. The engine loads them, validates them, runs them as a DAG. No orchestration code to write.
- **Event-sourced.** Every state change — inbound message, LLM call, tool result, approval, outbound reply — is an immutable event persisted to SQLite with a per-stream hash chain. Runs are replayable, resumable, and auditable without extra logging.
- **Boundary-safe.** Agents emit *intentions* ("I want to write this file", "I want to run this shell command") that pass through contracts and optional human approval before execution. The unsafe thing never happens until someone said yes.

The ergonomic target: you can bring a useful agent online in minutes without writing code, and a year later still answer *exactly what this agent did* on any past run.

---

## Run a Telegram agent in two minutes

This is the primary demo — a real chat bot you talk to on your phone, wired declaratively.

```bash
pip install zymi-core

mkdir telegram-agent && cd telegram-agent
zymi init --example telegram

# 1. Create a bot via @BotFather in Telegram; copy the token.
# 2. Fill .env:
cp .env.example .env       # edit TELEGRAM_BOT_TOKEN + OPENAI_API_KEY
# 3. Open project.yml, replace "your_username_here" with your actual
#    Telegram username (no @). This keeps strangers out of the bot.

source .env
zymi serve chat
```

Message the bot — it replies within a couple of seconds. Every inbound message, LLM call, and outbound reply is persisted to `.zymi/events.db`; watch it live with `zymi observe`.

The entire wiring lives in `project.yml`:

```yaml
llm:
  provider: openai
  model: gpt-4o-mini
  api_key: ${env.OPENAI_API_KEY}

connectors:
  - type: http_poll                   # long-polls getUpdates (no HTTPS needed)
    name: telegram
    url: "https://api.telegram.org/bot${env.TELEGRAM_BOT_TOKEN}/getUpdates"
    interval_secs: 2
    extract:
      items:     "$.result[*]"
      stream_id: "$.message.chat.id"
      content:   "$.message.text"
    cursor: { param: offset, from_item: "$.update_id", plus_one: true, persist: true }
    filter:
      "$.message.from.username":
        one_of: ["your_username_here"]
    pipeline: chat                    # every accepted message → `zymi serve chat`

outputs:
  - type: http_post                   # reply on every pipeline completion
    name: telegram_reply
    on: [ResponseReady]
    url: "https://api.telegram.org/bot${env.TELEGRAM_BOT_TOKEN}/sendMessage"
    headers: { Content-Type: "application/json" }
    body_template: '{"chat_id":"{{ event.stream_id }}","text":{{ event.content | tojson }}}'
    retry: { attempts: 3, backoff_secs: [1, 5, 30] }
```

That's it. No Rust. No Python. The scaffold ships with a small two-step DAG — `respond` (assistant with `web_search` / `web_scrape` tools) → `polish` (a brutally lazy reviewer that keeps the draft verbatim unless it's actually broken). Both steps are visible live in `zymi observe`.

Ask the bot to "announce that we're closing at 5pm" and the agent reaches for `tools/broadcast.yml` (`requires_approval: true`). The `approvals:` section in `project.yml` DMs you with ✅ / ❌ buttons; nothing goes out until you click. See [Approvals on the bus](#approvals-on-the-bus) below.

Want a real search backend? Open `tools/web_search.yml`, uncomment a provider block (Brave, Tavily, SerpAPI, Google), set its key in `.env`. Out of the box the bot still works — the assistant just answers from its own knowledge.

### Same primitives elsewhere

`http_inbound` / `http_poll` / `http_post` cover webhooks and REST for most SaaS. Pick `http_inbound` when the service can POST to you over HTTPS, `http_poll` when you're behind NAT or the service has no webhooks. Filter recipes for the common cases:

```yaml
# GitHub — only react to PR opens, not pushes or issues
filter:
  "$.action":              { equals: "opened" }
  "$.pull_request.draft":  { equals: false }

# Slack Events API — one channel, skip bot echoes
filter:
  "$.event.channel":  { equals: "C0123456789" }
  "$.event.subtype":  { equals: null }
```

Out of the box `filter:` supports `one_of` / `equals`. 429 rate-limiting is handled automatically (`Retry-After` header + Telegram's body-level hint are both honoured on retries).

---

## Highlights

### Pipelines are DAGs

A pipeline is a list of steps with `depends_on:` edges. Independent steps run in parallel, dependencies stay explicit.

```yaml
# pipelines/research.yml
steps:
  - id: search_web
    agent: researcher
    task: "Find articles on: ${inputs.topic}"

  - id: search_deep
    agent: researcher
    task: "Find technical details on: ${inputs.topic}"

  - id: analyze
    agent: researcher
    task: "Cross-reference findings, store a structured summary in memory."
    depends_on: [search_web, search_deep]    # runs after both finish

  - id: write_report
    agent: writer
    task: "Read memory, write ./output/report.md."
    depends_on: [analyze]
```

Try it: `zymi init --example research`. The scaffold pre-configures a two-agent pipeline with parallel search.

### Replay, resume, observe

Because everything is an event, you don't just log runs — you *keep* them.

```bash
zymi runs                                   # all pipeline runs
zymi events --stream pipeline-research-abc  # every event in one run
zymi verify --stream pipeline-research-abc  # hash-chain integrity check
zymi observe                                # 3-panel TUI: runs / DAG / events live

# Fork-resume an earlier run from a chosen step. Upstream steps are frozen;
# the fork step + DAG-descendants re-run against current configs on disk.
zymi resume pipeline-research-abc --from-step write_report
zymi resume pipeline-research-abc --from-step write_report --dry-run
```

Fork-resume is useful when you're iterating on a prompt or tool config: you don't have to re-burn the expensive early steps every time you tweak the later ones. See [ADR-0018](adr/0018-idempotent-fork-resume.md).

### MCP servers — N tools per YAML entry

Declarative tools cover HTTP and shell. For anything heavier — filesystem sandbox, git client, search index, proprietary protocol — drop a [Model Context Protocol][mcp] server into `project.yml` and `zymi-core` handles the subprocess, handshake, restarts, and tool-catalog wiring.

```yaml
mcp_servers:
  - name: fs
    command: [npx, -y, "@modelcontextprotocol/server-filesystem", ./sandbox]
    allow: [read_text_file, write_file, list_directory]
    restart: { max_restarts: 2, backoff_secs: [1, 5] }
```

Then in the agent:

```yaml
tools:
  - mcp__fs__read_text_file
  - mcp__fs__write_file
  - mcp__fs__list_directory
```

No per-tool schemas to author — they come from the server's `tools/list` at startup. Every call is audited as a normal tool event. Probe new servers before wiring:

```bash
zymi mcp probe fs -- npx -y @modelcontextprotocol/server-filesystem /tmp
zymi mcp probe gh --env GITHUB_PERSONAL_ACCESS_TOKEN=ghp_... \
                  -- npx -y @modelcontextprotocol/server-github
```

End-to-end demo: `zymi init --example mcp`. Full posture (PATH forwarding, env-isolation, restart policy) in [ADR-0023](adr/0023-mcp-client-integration.md).

### Declarative custom tools

HTTP and shell tools live in `tools/*.yml`. No rebuild, no Rust.

```yaml
# tools/slack_post.yml
name: slack_post
description: "Post a message to a Slack channel"
parameters:
  type: object
  properties:
    channel: { type: string }
    text:    { type: string }
  required: [channel, text]
implementation:
  kind: http
  method: POST
  url: "https://slack.com/api/chat.postMessage"
  headers:
    Authorization: "Bearer ${env.SLACK_TOKEN}"
    Content-Type:  "application/json"
  body_template: '{"channel": "${args.channel}", "text": "${args.text}"}'
```

`${env.*}` resolves at parse time; `${args.*}` resolves at call time from LLM arguments. Collisions with built-in tools are a hard error.

### Automatic context management

The agent's working context is reconstructed from the event log each iteration, not accumulated in a growing buffer. Older tool observations are masked in-place (~2× cost reduction, no extra LLM calls). When the token budget still gets tight, hybrid compaction summarises the oldest masked batch with a fast LLM call. Both caps are tunable:

```yaml
runtime:
  context:
    observation_window: 10         # recent turns kept verbatim
    soft_cap_chars: 400000         # triggers LLM summarisation
    hard_cap_chars: 600000         # fatal if exceeded after compaction
    min_tail_turns: 4
```

Design and trade-offs in [ADR-0016](adr/0016-context-window-management.md).

### Safer side effects

Agents don't take actions directly — they emit intentions (`ExecuteShellCommand`, `WriteFile`, `ReadFile`, `WebSearch`, `SpawnSubAgent`, `CallCustomTool`, …). Intentions pass through:

1. **Policy engine** — shell command allow/deny patterns.
2. **Contracts** — file-write boundaries, rate limits, tool-specific rules.
3. **Approval** — declarative `approvals:` channels (terminal / http / telegram), or fail-closed when no channel is configured.

Nothing with side effects runs until the intention is approved.

### Approvals on the bus

Approvals are declarative in `project.yml` ([ADR-0022](adr/0022-event-sourced-approvals.md)). Pick a channel — terminal prompt, HTTP endpoint, or Telegram DM with inline ✅/❌ keyboard — and any tool flagged `requires_approval: true` routes through it.

```yaml
default_approval_channel: ops_tg

approvals:
  - type: telegram
    name: ops_tg
    bot_token:    "${env.TELEGRAM_BOT_TOKEN}"
    chat_id:      "${env.TELEGRAM_ADMIN_CHAT_ID}"
    bind:         "127.0.0.1:8088"
    callback_path: /telegram/approval
    secret_token: "${env.TELEGRAM_WEBHOOK_SECRET}"
```

```yaml
# tools/broadcast.yml — gated tool
name: broadcast
description: "Send an announcement to the team channel."
parameters: { type: object, properties: { message: { type: string } }, required: [message] }
requires_approval: true
implementation:
  kind: shell
  command_template: "post-to-slack ${args.message}"
```

When the agent calls `broadcast`, the bot DMs the admin chat with approve/deny buttons; the click flows back as `ApprovalGranted` / `ApprovalDenied` events. Resolution order is **pipeline override → project default → fail-closed**. Every step is on the event bus, so the audit trail is uniform with the rest of the run, and a hard crash mid-approval is repaired on next start: in-flight requests are redelivered to live channels, expired ones are sealed with `ApprovalDenied{reason: restart_timeout}`. End-to-end demo: `zymi init --example telegram`.

### JSON Schemas for configs

IDE autocomplete and LLM-assisted config generation come free:

```bash
zymi schema project       # draft-07 JSON Schema for project.yml
zymi schema --all
```

---

## Python bindings

The same `pip install zymi-core` that gives you the CLI also exposes `Runtime`, `Event`, `EventBus`, `EventStore`, `Subscription`, `ToolRegistry`.

```python
from zymi_core import Runtime

rt = Runtime.for_project(".", approval="terminal")
result = rt.run_pipeline("research", {"topic": "rust event sourcing"})
print(result.success, result.final_output)
```

`rt.bus()` and `rt.store()` hand out wrappers over the same `Arc`s the Rust runtime uses — a Python subscriber sees exactly what the handler publishes, no second bus over the same SQLite file.

### Cross-process events (Django, Celery, scripts)

The SQLite store is the source of truth: events written from one process are visible to every other process that opens the same file, and `zymi serve` picks them up via a polling tail watcher ([ADR-0012](adr/0012-cross-process-event-delivery.md)).

Canonical pattern — a web app publishes `PipelineRequested`, the long-running `zymi serve` runs the pipeline, the result comes back as `PipelineCompleted` with the same `correlation_id`.

```python
# Terminal A: zymi serve research
# Terminal B: any Python process — Django view, Celery task, script
import uuid
from zymi_core import Event, EventBus, EventStore

store = EventStore(".zymi/events.db")
bus   = EventBus(store)

correlation_id = str(uuid.uuid4())
sub = bus.subscribe_correlation(correlation_id)

event = Event(
    stream_id=f"web-{correlation_id}",
    kind={"type": "PipelineRequested", "data": {
        "pipeline": "research",
        "inputs":   {"topic": "rust event sourcing"},
    }},
    source="django",
)
event.with_correlation(correlation_id)
bus.publish(event)

result = sub.recv(timeout_secs=300)
print(result.kind)
```

Inside `zymi serve` the `PipelineRequested → RunPipeline` translation is done by `EventCommandRouter` — re-exported from `zymi_core::runtime`, so your own scheduler or bot adapter can drive a `Runtime` without copying `cli/serve.rs`.

---

## Rust crate

```toml
[dependencies]
zymi-core = "0.4"
```

```rust
use std::sync::Arc;
use zymi_core::{open_store, Event, EventBus, EventKind, Message, StoreBackend};

let store = open_store(StoreBackend::Sqlite { path: "events.db".into() })?;
let bus   = EventBus::new(store.clone());
let mut rx = bus.subscribe().await;

let event = Event::new(
    "conversation-1".into(),
    EventKind::UserMessageReceived {
        content:   Message::User("Hello".into()),
        connector: "cli".into(),
    },
    "cli".into(),
);
bus.publish(event).await?;

let received   = rx.recv().await.unwrap();
let verified   = store.verify_chain("conversation-1").await?;
```

Feature flags:

| Feature | Description |
| --- | --- |
| `python`     | PyO3 bindings for the `_zymi_core` extension module |
| `cli`        | The `zymi` CLI binary (implies `connectors`) |
| `runtime`    | Async runtime and HTTP dependencies |
| `webhook`    | HTTP approval handler (axum) |
| `services`   | Event-bus services (LangFuse) |
| `connectors` | Declarative `connectors:` / `outputs:` (`http_inbound` / `http_poll` / `http_post`, [ADR-0021](adr/0021-generic-http-connectors-and-plugin-protocol.md)) |

---

## CLI reference

```bash
zymi init                              # minimal scaffold
zymi init --example telegram           # declarative chat bot
zymi init --example research           # parallel-search + writer pipeline
zymi init --example mcp                # agent wired to an MCP server

zymi run <pipeline> -i key=value ...   # one-shot run
zymi serve <pipeline>                  # long-running: react to PipelineRequested

zymi pipelines                         # list pipelines in the project
zymi runs [--pipeline NAME] [--limit N]
zymi events [--stream ID] [--kind TAG] [--raw]
zymi verify [--stream ID]              # hash-chain integrity check
zymi observe [--run ID]                # 3-panel TUI
zymi resume <run-id> --from-step <id> [--dry-run]

zymi mcp probe <name> -- <cmd> [args...]
zymi schema project|agent|pipeline|tool
zymi schema --all
```

---

## How it works

1. **Every state change becomes an event.** The SQLite event store with per-stream hash chain is the source of truth.
2. **Agents emit intentions, not side effects.** Intentions are evaluated against policy + contracts + approvals before execution.
3. **Pipelines are DAGs.** Independent steps run in parallel; dependencies stay explicit.
4. **Context is event-sourced.** The working context is reassembled from the log each iteration — older observations masked, hybrid compaction when the budget gets tight.
5. **Runs stay replayable.** Inspect with `zymi events`, verify with `zymi verify`, fork-resume from any step.

More detail lives in [`adr/`](adr/) — each architectural decision is a short markdown file.

---

## Contributing

Bug reports, examples, and PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the dev-loop, test matrix, and ADR workflow.

## License

MIT — see [LICENSE](LICENSE).

[mcp]: https://modelcontextprotocol.io
