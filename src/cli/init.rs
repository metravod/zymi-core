use std::fs;
use std::path::Path;

const KNOWN_EXAMPLES: &[&str] = &["telegram"];

/// Declarative web_search tool template shipped with `zymi init`.
///
/// Ships as a working shell placeholder that returns a helpful message.
/// Users uncomment the provider block they want and set the API key.
const WEB_SEARCH_TOOL: &str = r#"# ============================================================================
# web_search — declarative tool example
#
# This is a working example of a declarative tool. Out of the box it returns
# a placeholder message. To connect it to a real search API:
#
#   1. Pick a provider below and uncomment its `implementation:` block.
#   2. Delete or comment out the placeholder `implementation:` at the bottom.
#   3. Set the API key in your environment.
#
# If you don't need web search, just delete this file — nothing will break.
# ============================================================================

name: web_search
description: "Search the web for information on a given query"
parameters:
  type: object
  properties:
    query:
      type: string
      description: "Search query"
  required: [query]

# --- Provider: Brave Search (https://brave.com/search/api/) ---
#     Free tier: 2000 queries/month
#
# implementation:
#   kind: http
#   method: GET
#   url: "https://api.search.brave.com/res/v1/web/search?q=${args.query}&count=5"
#   headers:
#     Accept: "application/json"
#     X-Subscription-Token: "${env.BRAVE_SEARCH_API_KEY}"

# --- Provider: SerpAPI (https://serpapi.com/) ---
#     Free tier: 100 queries/month
#
# implementation:
#   kind: http
#   method: GET
#   url: "https://serpapi.com/search.json?q=${args.query}&num=5&api_key=${env.SERPAPI_KEY}"

# --- Provider: Google Custom Search (https://developers.google.com/custom-search/) ---
#     Free tier: 100 queries/day
#
# implementation:
#   kind: http
#   method: GET
#   url: "https://www.googleapis.com/customsearch/v1?q=${args.query}&key=${env.GOOGLE_API_KEY}&cx=${env.GOOGLE_SEARCH_CX}&num=5"

# --- Provider: Tavily (https://tavily.com/) ---
#     AI-optimised search — returns clean, LLM-ready content.
#     Free tier: 1000 queries/month
#
# implementation:
#   kind: http
#   method: POST
#   url: "https://api.tavily.com/search"
#   headers:
#     Content-Type: "application/json"
#     Authorization: "Bearer ${env.TAVILY_API_KEY}"
#   body_template: '{"query": "${args.query}", "max_results": 5}'

# Placeholder — when no provider is wired, the tool's *output* is read by
# the agent, not the user. So the message is written *to the model* as
# instructions, not as an admin error. Without this, models read
# "not configured" as a generic failure and refuse to answer at all.
# Once you uncomment a real provider above, this whole block goes away.
implementation:
  kind: shell
  command_template: "echo 'WEB_SEARCH_DISABLED: this project has no search provider wired. Answer the user using your own training knowledge. If the question is time-sensitive (current events, weather, prices, news), give your best general or seasonal answer based on training data and add ONE short line acknowledging that you could not check live sources. Do not refuse to answer; do not mention this tool by name.'"
"#;

/// Declarative web_scrape tool template shipped with `zymi init`.
///
/// Scrapes a URL and returns clean markdown/text content.
/// Users uncomment the provider block they want and set the API key.
const WEB_SCRAPE_TOOL: &str = r#"# ============================================================================
# web_scrape — declarative tool example
#
# Fetches a URL and returns its content as clean text/markdown.
# Out of the box it returns a placeholder message. To connect a provider:
#
#   1. Pick a provider below and uncomment its `implementation:` block.
#   2. Delete or comment out the placeholder `implementation:` at the bottom.
#   3. Set the API key in your environment.
#
# If you don't need web scraping, just delete this file — nothing will break.
# ============================================================================

name: web_scrape
description: "Fetch a web page and return its content as clean text"
parameters:
  type: object
  properties:
    url:
      type: string
      description: "URL to scrape"
  required: [url]

# --- Provider: Firecrawl (https://firecrawl.dev/) ---
#     Turns any URL into clean markdown. AI-optimised.
#     Free tier: 500 pages/month
#
# implementation:
#   kind: http
#   method: POST
#   url: "https://api.firecrawl.dev/v1/scrape"
#   headers:
#     Content-Type: "application/json"
#     Authorization: "Bearer ${env.FIRECRAWL_API_KEY}"
#   body_template: '{"url": "${args.url}", "formats": ["markdown"]}'

# --- Provider: Jina Reader (https://jina.ai/reader/) ---
#     Free, no API key needed. Prepend r.jina.ai/ to any URL.
#
# implementation:
#   kind: http
#   method: GET
#   url: "https://r.jina.ai/${args.url}"
#   headers:
#     Accept: "text/plain"

# Placeholder — see the note in tools/web_search.yml. The tool's output is
# read by the agent, so the message is written *to the model* as
# instructions, not as an admin error. Once you uncomment a real
# provider above, this block goes away.
implementation:
  kind: shell
  command_template: "echo 'WEB_SCRAPE_DISABLED: this project has no scraping provider wired. Tell the user briefly that you cannot fetch the page contents directly, then offer to discuss what you already know about the URL or the topic. Do not refuse to engage; do not mention this tool by name.'"
"#;

/// `AGENTS.md` shipped with every `zymi init` scaffold.
///
/// Aimed at AI coding assistants (Claude Code, Cursor, Aider, …) that will
/// help the user extend their zymi project. Covers vocabulary, file map,
/// task → file routing, conventions, and don'ts. Stable headings so agents
/// can deep-link.
const AGENTS_DOC: &str = r#"# AGENTS.md — guide for AI assistants editing this project

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
"#;

pub fn exec(name: Option<String>, example: Option<&str>) -> Result<(), String> {
    if let Some(ex) = example {
        if !KNOWN_EXAMPLES.contains(&ex) {
            return Err(format!(
                "unknown example '{ex}'. Available: {}",
                KNOWN_EXAMPLES.join(", ")
            ));
        }
    }

    let cwd = std::env::current_dir().map_err(|e| format!("cannot determine cwd: {e}"))?;

    let project_name = name.unwrap_or_else(|| {
        cwd.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("my-project")
            .to_string()
    });

    if cwd.join("project.yml").exists() {
        return Err("project.yml already exists — this directory is already a zymi project".into());
    }

    create_dir(&cwd, "agents")?;
    create_dir(&cwd, "pipelines")?;
    create_dir(&cwd, "tools")?;
    create_dir(&cwd, ".zymi")?;

    match example {
        Some("telegram") => scaffold_telegram(&cwd, &project_name)?,
        _ => scaffold_default(&cwd, &project_name)?,
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Default scaffold
// ---------------------------------------------------------------------------

fn scaffold_default(root: &Path, project_name: &str) -> Result<(), String> {
    write_file(
        &root.join("project.yml"),
        &format!(
            r#"name: {project_name}
version: "0.1"

# LLM provider configuration
# llm:
#   provider: openai
#   model: gpt-4o
#   api_key: ${{env.OPENAI_API_KEY}}

defaults:
  timeout_secs: 30
  max_iterations: 10

policy:
  enabled: true
  allow: ["ls *", "cat *", "echo *"]
  deny: ["rm -rf *"]

contracts:
  file_write:
    allowed_dirs: ["./output", "./memory"]
    deny_patterns: ["*.env", "*.key"]
"#
        ),
    )?;

    write_file(
        &root.join("agents/default.yml"),
        &format!(
            r#"name: default
description: "Default agent for {project_name}"
# model: gpt-4o
tools:
  - web_search
  - read_file
  - write_memory
max_iterations: 10
"#
        ),
    )?;

    write_file(
        &root.join("pipelines/main.yml"),
        r#"name: main
description: "Main pipeline"

inputs:
  - name: task
    required: true

steps:
  - id: process
    agent: default
    task: "${inputs.task}"

output:
  step: process
"#,
    )?;

    write_file(&root.join("tools/web_search.yml"), WEB_SEARCH_TOOL)?;
    write_file(&root.join("tools/web_scrape.yml"), WEB_SCRAPE_TOOL)?;
    write_file(&root.join("AGENTS.md"), AGENTS_DOC)?;

    println!("Initialized zymi project '{project_name}'");
    println!();
    println!("  project.yml              — project configuration");
    println!("  agents/default.yml       — default agent");
    println!("  pipelines/main.yml       — main pipeline");
    println!("  tools/web_search.yml     — web search tool (configure your provider)");
    println!("  tools/web_scrape.yml     — web scrape tool (configure your provider)");
    println!("  AGENTS.md                — guide for AI assistants editing this project");
    println!("  .zymi/                   — runtime data (events.db)");
    println!();
    println!("Next: configure your LLM provider in project.yml, then run:");
    println!("  zymi run main");

    Ok(())
}

// ---------------------------------------------------------------------------
// Telegram example scaffold
// ---------------------------------------------------------------------------

fn scaffold_telegram(root: &Path, project_name: &str) -> Result<(), String> {
    // -- project.yml --
    // `http_poll` calls Telegram's `getUpdates` over long-polling, so this
    // works from a laptop without ngrok / HTTPS. `http_post` sends replies
    // on every `ResponseReady` event the agent produces. `filter:` pins the
    // bot to a set of usernames — critical, since a leaked bot token lets
    // anyone spam the agent otherwise.
    write_file(
        &root.join("project.yml"),
        &format!(
            r#"name: {project_name}
version: "0.1"

llm:
  provider: openai
  model: gpt-4o-mini
  api_key: ${{env.OPENAI_API_KEY}}

defaults:
  timeout_secs: 60
  max_iterations: 8

policy:
  enabled: true
  # `web_search` / `web_scrape` ship with a `kind: shell` placeholder that
  # echoes "not configured" when no provider is wired. `broadcast` is a
  # `kind: shell` tool too; we allow `echo` so all three placeholders run.
  allow: ["echo *"]
  deny: ["*"]

# ── Telegram I/O (declarative, no Rust required) ────────────────────────────
#
# Edit the `filter:` list below to include your Telegram username (case-
# sensitive, without the @). Anyone else messaging the bot is silently
# dropped. Remove the filter block to accept everyone — only do that for a
# throwaway bot.

connectors:
  - type: http_poll
    name: telegram
    url: "https://api.telegram.org/bot${{env.TELEGRAM_BOT_TOKEN}}/getUpdates"
    method: GET
    interval_secs: 2
    extract:
      items:     "$.result[*]"
      stream_id: "$.message.chat.id"
      content:   "$.message.text"
      user:      "$.message.from.username"
    cursor:
      param: offset
      from_item: "$.update_id"
      plus_one: true
      persist: true
    filter:
      "$.message.from.username":
        one_of: ["your_username_here"]
    # Every accepted update fires `chat` pipeline with `inputs.message`
    # set to the message text. `zymi serve chat` picks it up.
    pipeline: chat
    pipeline_input: message

outputs:
  - type: http_post
    name: telegram_reply
    on: [ResponseReady]
    url: "https://api.telegram.org/bot${{env.TELEGRAM_BOT_TOKEN}}/sendMessage"
    method: POST
    headers:
      Content-Type: "application/json"
    body_template: |
      {{
        "chat_id": "{{{{ event.stream_id }}}}",
        "text": {{{{ event.content | tojson }}}}
      }}
    retry:
      attempts: 3
      backoff_secs: [1, 5, 30]

# ── Human approvals (ADR-0022) ──────────────────────────────────────────────
#
# When `tools/broadcast.yml` is invoked the agent publishes
# `ApprovalRequested`. The `ops_tg` channel below picks it up, DMs the
# admin chat with an inline ✅ / ❌ keyboard, and the click flows back as
# `ApprovalGranted` / `ApprovalDenied` on the bus.
#
# Telegram needs a public URL to reach `bind:` for the callback. Two
# common ways to get one on a laptop:
#   ngrok http 8088            →  https://abc123.ngrok-free.app
#   cloudflared tunnel --url http://localhost:8088
# Then point the bot at it once:
#   curl "https://api.telegram.org/bot${{TELEGRAM_BOT_TOKEN}}/setWebhook" \
#        -d "url=<public-url>/telegram/approval" \
#        -d "secret_token=${{TELEGRAM_WEBHOOK_SECRET}}"

default_approval_channel: ops_tg

approvals:
  - type: telegram
    name: ops_tg
    bot_token: "${{env.TELEGRAM_BOT_TOKEN}}"
    # The admin chat that receives approval prompts. Often the same chat
    # the user is in, so they see "approve?" land in their DMs as the
    # agent thinks. Set in .env (see TELEGRAM_ADMIN_CHAT_ID).
    chat_id: ${{env.TELEGRAM_ADMIN_CHAT_ID}}
    bind: "127.0.0.1:8088"
    callback_path: /telegram/approval
    # `secret_token` is enforced via Telegram's
    # `X-Telegram-Bot-Api-Secret-Token` header. Without it, anyone who
    # guesses your public URL can forge approvals.
    secret_token: "${{env.TELEGRAM_WEBHOOK_SECRET}}"

# ── (optional) MCP server — N tools from one block, no YAML stubs ───────────
#
# Uncomment to give the agent read/write access to the local filesystem via
# the official MCP filesystem server. Tools land in the catalogue prefixed
# with `mcp__fs__*` (list_directory, read_text_file, write_file, …) — wire
# them into agents/assistant.yml `tools:` to make them callable.
#
# Requires `node` + `npx` on PATH. See ADR-0023 for the full security posture
# (PATH auto-forward, env-isolation, restart policy).
#
# mcp_servers:
#   - name: fs
#     command:
#       - npx
#       - "-y"
#       - "@modelcontextprotocol/server-filesystem"
#       - ./sandbox
#     allow: [read_text_file, write_file, list_directory]
#     init_timeout_secs: 15
#     call_timeout_secs: 30
"#
        ),
    )?;

    // -- agents/assistant.yml --
    // The "draft" agent in our two-step chat pipeline. Has tools; runs a
    // ReAct loop bounded by max_iterations. Web tools default to a shell
    // placeholder that says "not configured" — the agent reads this and
    // falls back to its own knowledge without crashing the run.
    write_file(
        &root.join("agents/assistant.yml"),
        r#"name: assistant
description: "Friendly Telegram chat assistant — drafts the first reply."
system_prompt: |
  You are a helpful assistant answering questions in a Telegram chat.

  Style:
  - Keep replies concise and friendly.
  - Do not use markdown — Telegram's default view renders plain text cleanest.
  - If you cannot help with something, say so briefly.

  Tools:
  - `web_search` for questions that need current or specific external information.
  - `web_scrape` to read a specific URL the user mentions.
  - `get_weather` for current weather in a named city (Python tool, sync).
  - `translate` for translating text into a target language (Python tool, async).
  - `broadcast` sends an announcement to a public channel. Always
    requires human approval — the operator gets a Telegram DM with
    ✅ / ❌ buttons before the message goes out. Use this only when the
    user explicitly asks you to "announce", "broadcast", or "post" a
    message; otherwise stay in normal chat.
  - If a tool returns "not configured", proceed using your own knowledge —
    don't mention the tool's failure to the user.
tools:
  - web_search
  - web_scrape
  - get_weather
  - translate
  - broadcast
max_iterations: 4
"#,
    )?;

    // -- agents/reviewer.yml --
    // Lightweight verifier on top of the draft. No tools, short prompt:
    // either let the draft through verbatim or rewrite it. This is the
    // "verifier" half of a draft-then-verify pipeline (Anthropic's
    // recommended pattern for chat over multi-agent orchestration).
    write_file(
        &root.join("agents/reviewer.yml"),
        r#"name: reviewer
description: "Brutally lazy reviewer — only intervenes when the draft is bad."
system_prompt: |
  You are reviewing a draft answer that will be sent verbatim to a user
  in a Telegram chat. Your only job is to decide between two options:

    1. The draft already answers the question well → return it verbatim,
       byte-for-byte. No prefix, no commentary, no formatting changes.
    2. The draft is wrong, hallucinating, off-topic, or confusingly long →
       write a better short version yourself.

  Be brutally lazy: option 1 is the right call most of the time. Only
  rewrite when the draft is actually broken.

  Output rules:
  - Output ONLY the final answer text, nothing else.
  - Never say "Reviewed:" or "Here is the corrected answer:" or anything
    similar — the user must not see that this step exists.
  - Plain text only. No markdown.
tools: []
max_iterations: 1
"#,
    )?;

    // -- pipelines/chat.yml --
    // Two-step DAG: assistant drafts, reviewer checks. One stream per
    // Telegram `chat_id`; every inbound message starts one pipeline run,
    // and the reviewer's output flows back through `http_post`.
    write_file(
        &root.join("pipelines/chat.yml"),
        r#"name: chat
description: "Two-step chat loop: assistant drafts, reviewer polishes."

inputs:
  - name: message
    required: true

steps:
  - id: respond
    agent: assistant
    task: "${inputs.message}"

  - id: polish
    agent: reviewer
    task: |
      User asked:
      ${inputs.message}

      Draft answer from the assistant:
      ${steps.respond.output}

      Decide whether to keep the draft verbatim or write a better version.
      Output ONLY the final answer the user will see.
    depends_on: [respond]

output:
  step: polish
"#,
    )?;

    // -- tools/web_search.yml + tools/web_scrape.yml + tools/broadcast.yml --
    write_file(&root.join("tools/web_search.yml"), WEB_SEARCH_TOOL)?;
    write_file(&root.join("tools/web_scrape.yml"), WEB_SCRAPE_TOOL)?;

    // -- tools/get_weather.py + tools/translate.py --
    // Auto-discovered by zymi at startup (ADR-0014 slice 3): any file under
    // `tools/*.py` containing one or more `@tool`-decorated functions gets
    // registered in the catalogue. Drop more files in to add tools.
    //
    // Python tools require pip-installed zymi (`pip install zymi-core`) —
    // the cargo binary doesn't bundle the Python runtime.
    write_file(
        &root.join("tools/get_weather.py"),
        r#""""Sync Python tool — current weather for a city.

This is the simplest possible @tool: a regular Python function whose
arguments and docstring are introspected to build the LLM-facing schema.
Replace the stub body with a real API call (OpenWeatherMap, weatherapi.com,
…) when you want live data.
"""
from zymi import tool


@tool
def get_weather(city: str) -> str:
    """Return the current weather for a city."""
    return f"It's mild and partly cloudy in {city} today (stub — wire a real API)."
"#,
    )?;

    write_file(
        &root.join("tools/translate.py"),
        r#""""Async Python tool — `async def` is supported as-is.

The runtime awaits the coroutine on a worker thread, so you can use
`httpx`, `aiohttp`, or any other async I/O library here without blocking
the agent loop. The stub below just sleeps to prove async dispatch works.
"""
import asyncio

from zymi import tool


@tool
async def translate(text: str, target: str = "en") -> str:
    """Translate text into the target language (ISO 639-1 code)."""
    await asyncio.sleep(0)  # placeholder for a real async HTTP call
    return f"[{target}] {text}"
"#,
    )?;

    // -- AGENTS.md (guide for AI assistants editing this project) --
    write_file(&root.join("AGENTS.md"), AGENTS_DOC)?;
    write_file(
        &root.join("tools/broadcast.yml"),
        r#"# broadcast — gated tool that triggers the ADR-0022 approval flow.
#
# When the agent calls this tool, the orchestrator publishes
# `ApprovalRequested`. The `ops_tg` approval channel in project.yml
# DMs the admin chat with ✅ / ❌ buttons and waits for the click.
# On approve, the shell command below runs and the result is fed back
# to the agent. On deny (or timeout), the tool reports a refusal to the
# agent and no shell ever executes.
name: broadcast
description: |
  Send an announcement to the team channel. Sensitive — the human
  operator must approve every call before it goes out.
parameters:
  type: object
  properties:
    message:
      type: string
      description: "The announcement text. Plain text, ≤ 280 chars."
  required: [message]

# Default for `kind: shell` is already `requires_approval: true`, but
# spell it out so a future reader sees the gate without grepping ADRs.
requires_approval: true

implementation:
  kind: shell
  command_template: "echo 'BROADCAST_DISABLED: would have sent → ${args.message}'"
"#,
    )?;

    // -- .env.example --
    write_file(
        &root.join(".env.example"),
        r#"# Rename this file to `.env` and fill in the values. `.env` is gitignored
# by default.

# Create a bot via @BotFather in Telegram; it will hand you a token.
TELEGRAM_BOT_TOKEN=

# Any Chat-Completions-compatible OpenAI account works. Swap the provider
# in project.yml if you prefer Anthropic / Gemini / a local server.
OPENAI_API_KEY=

# --- Approval flow (ADR-0022) ----------------------------------------------
# Chat id that receives ✅ / ❌ approval prompts when the agent calls a
# `requires_approval: true` tool (e.g. `broadcast`). To find your own
# numeric id, message @userinfobot in Telegram. For a group, send any
# message in the group, then GET
# https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/getUpdates and copy
# `result[*].message.chat.id` (groups are negative numbers).
TELEGRAM_ADMIN_CHAT_ID=

# Random shared secret enforced via the `X-Telegram-Bot-Api-Secret-Token`
# header. Anything unguessable works:  openssl rand -hex 16
TELEGRAM_WEBHOOK_SECRET=
"#,
    )?;

    // Ensure `.env` never gets committed by accident.
    let gitignore_path = root.join(".gitignore");
    let current = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
    let mut updated = current.clone();
    for line in [".env", ".zymi/"] {
        if !updated.lines().any(|l| l.trim() == line) {
            if !updated.is_empty() && !updated.ends_with('\n') {
                updated.push('\n');
            }
            updated.push_str(line);
            updated.push('\n');
        }
    }
    if updated != current {
        write_file(&gitignore_path, &updated)?;
    }

    println!("Initialized zymi project '{project_name}' with telegram example");
    println!();
    println!("  project.yml              — Telegram chat I/O + ops_tg approval channel + commented mcp_servers: block");
    println!("  agents/assistant.yml     — drafts the reply (declarative + Python tools, broadcast w/ approval)");
    println!("  agents/reviewer.yml      — verifies the draft, polishes if needed");
    println!("  pipelines/chat.yml       — DAG: respond → polish");
    println!("  tools/web_search.yml     — declarative search tool (configure a provider)");
    println!("  tools/web_scrape.yml     — declarative scrape tool (configure a provider)");
    println!("  tools/broadcast.yml      — gated `requires_approval: true` tool (ADR-0022)");
    println!("  tools/get_weather.py     — sync @tool (auto-discovered, requires pip-installed zymi)");
    println!("  tools/translate.py       — async @tool (auto-discovered, requires pip-installed zymi)");
    println!("  AGENTS.md                — guide for AI assistants editing this project");
    println!("  .env.example             — token placeholders");
    println!("  .zymi/                   — runtime data (events.db, connectors.db)");
    println!();
    println!("Next:");
    println!("  1. Message @BotFather in Telegram and create a bot; copy the token.");
    println!("  2. cp .env.example .env  &&  fill TELEGRAM_BOT_TOKEN + OPENAI_API_KEY");
    println!("     + TELEGRAM_ADMIN_CHAT_ID + TELEGRAM_WEBHOOK_SECRET (see comments).");
    println!("  3. Edit project.yml → connectors[0].filter: replace 'your_username_here'");
    println!("     with your actual Telegram username (no @). This is what keeps");
    println!("     strangers who somehow found your token out of the bot.");
    println!("  4. Expose the approval callback so Telegram can reach it:");
    println!("       ngrok http 8088    # or `cloudflared tunnel --url http://localhost:8088`");
    println!("     Then point the bot at it (one-shot setup, persists on Telegram's side):");
    println!("       curl \"https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/setWebhook\" \\");
    println!("            -d \"url=https://<your-tunnel>/telegram/approval\" \\");
    println!("            -d \"secret_token=$TELEGRAM_WEBHOOK_SECRET\"");
    println!("  5. (optional) Open tools/web_search.yml and uncomment a provider block");
    println!("     (Brave / Tavily / SerpAPI / Google) + set its API key in .env.");
    println!("     Without this the bot still works — it just answers from its own");
    println!("     knowledge instead of searching the web.");
    println!("  6. set -a; source .env; set +a   # export vars to child processes");
    println!("     ~/.../target/release/zymi serve chat   # or your installed `zymi`");
    println!();
    println!("Tip: once running, message the bot on Telegram — you should see an agent");
    println!("     reply within a couple of seconds. The DAG (respond → polish) is");
    println!("     visible live in `zymi observe`; outbound POSTs to Telegram show as");
    println!("     `outbound_dispatched` events in `zymi events`.");
    println!();
    println!("Try the approval flow: ask the bot to \"announce that we're closing at 5pm\".");
    println!("     The agent calls `broadcast`, which DMs the admin chat with ✅ / ❌");
    println!("     buttons. Click one and watch `Approval`/`Approved`/`Denied` events");
    println!("     stream into `zymi observe` and `zymi events`.");

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn create_dir(root: &Path, name: &str) -> Result<(), String> {
    let path = root.join(name);
    if !path.exists() {
        fs::create_dir_all(&path).map_err(|e| format!("cannot create {name}/: {e}"))?;
    }
    Ok(())
}

fn write_file(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|e| format!("cannot write {}: {e}", path.display()))
}
