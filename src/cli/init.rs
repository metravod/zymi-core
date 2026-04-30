use std::fs;
use std::path::Path;

const KNOWN_EXAMPLES: &[&str] = &["research", "mcp", "telegram"];

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
        Some("research") => scaffold_research(&cwd, &project_name)?,
        Some("mcp") => scaffold_mcp(&cwd, &project_name)?,
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

steps:
  - id: process
    agent: default
    task: "${inputs.task}"

input:
  type: text

output:
  step: process
"#,
    )?;

    write_file(&root.join("tools/web_search.yml"), WEB_SEARCH_TOOL)?;
    write_file(&root.join("tools/web_scrape.yml"), WEB_SCRAPE_TOOL)?;

    println!("Initialized zymi project '{project_name}'");
    println!();
    println!("  project.yml              — project configuration");
    println!("  agents/default.yml       — default agent");
    println!("  pipelines/main.yml       — main pipeline");
    println!("  tools/web_search.yml     — web search tool (configure your provider)");
    println!("  tools/web_scrape.yml     — web scrape tool (configure your provider)");
    println!("  .zymi/                   — runtime data (events.db)");
    println!();
    println!("Next: configure your LLM provider in project.yml, then run:");
    println!("  zymi run main");

    Ok(())
}

// ---------------------------------------------------------------------------
// Research example scaffold
// ---------------------------------------------------------------------------

fn scaffold_research(root: &Path, project_name: &str) -> Result<(), String> {
    create_dir(root, "output")?;
    create_dir(root, "memory")?;

    // -- project.yml --
    write_file(
        &root.join("project.yml"),
        &format!(
            r#"name: {project_name}
version: "0.1"

# Uncomment and set your LLM provider:
# llm:
#   provider: openai
#   model: gpt-4o
#   api_key: ${{env.OPENAI_API_KEY}}

variables:
  default_model: gpt-4o

defaults:
  timeout_secs: 60
  max_iterations: 15

policy:
  enabled: true
  allow: ["cat *", "ls *", "echo *"]
  deny: ["rm -rf *"]

contracts:
  file_write:
    allowed_dirs: ["./output", "./memory"]
    deny_patterns: ["*.env", "*.key", "*.pem"]
"#
        ),
    )?;

    // -- agents/researcher.yml --
    write_file(
        &root.join("agents/researcher.yml"),
        r#"name: researcher
description: "Research agent — searches the web, scrapes pages, and stores findings in memory"
model: ${default_model}
system_prompt: |
  You are a thorough research assistant. Your job is to find accurate,
  up-to-date information on the given topic.

  Strategy:
  1. Start with broad web searches to identify key sources.
  2. Scrape the most promising pages for detailed content.
  3. Store important findings in memory with clear keys.
  4. Cite your sources.

  Always prefer primary sources over secondary ones.
  If information conflicts, note the discrepancy.
tools:
  - web_search
  - web_scrape
  - write_memory
max_iterations: 15
"#,
    )?;

    // -- agents/writer.yml --
    write_file(
        &root.join("agents/writer.yml"),
        r#"name: writer
description: "Writer agent — reads research findings and produces a structured report"
model: ${default_model}
system_prompt: |
  You are a skilled technical writer. Your job is to transform raw research
  findings into a clear, well-structured report.

  Guidelines:
  1. Read all available memory entries to understand the research.
  2. Organize findings into logical sections.
  3. Include a summary at the top.
  4. Cite sources where available.
  5. Write the final report to the output directory.

  Format: Markdown. Be concise but thorough.
tools:
  - read_file
  - write_file
max_iterations: 10
"#,
    )?;

    // -- pipelines/research.yml --
    write_file(
        &root.join("pipelines/research.yml"),
        r#"name: research
description: "Multi-step research pipeline: parallel search → analysis → report"

steps:
  - id: search_web
    agent: researcher
    task: "Search the web for information about: ${inputs.topic}"

  - id: search_deep
    agent: researcher
    task: "Find in-depth articles and technical details about: ${inputs.topic}"

  - id: analyze
    agent: researcher
    task: >
      Analyze and cross-reference all findings from the web search and deep
      search. Identify key themes, contradictions, and gaps. Store a structured
      summary in memory under the key 'analysis'.
    depends_on:
      - search_web
      - search_deep

  - id: write_report
    agent: writer
    task: >
      Read the analysis from memory and write a comprehensive research report
      to ./output/report.md. Include an executive summary, main findings,
      and a sources section.
    depends_on:
      - analyze

input:
  type: text

output:
  step: write_report
"#,
    )?;

    write_file(&root.join("tools/web_search.yml"), WEB_SEARCH_TOOL)?;
    write_file(&root.join("tools/web_scrape.yml"), WEB_SCRAPE_TOOL)?;

    println!("Initialized zymi project '{project_name}' with research example");
    println!();
    println!("  project.yml                — project configuration");
    println!("  agents/researcher.yml      — research agent (web_search, web_scrape, write_memory)");
    println!("  agents/writer.yml          — writer agent (read_file, write_file)");
    println!("  pipelines/research.yml     — 4-step research pipeline with parallel search");
    println!("  tools/web_search.yml       — web search tool (configure your provider)");
    println!("  tools/web_scrape.yml       — web scrape tool (configure your provider)");
    println!("  output/                    — report output directory");
    println!("  memory/                    — agent memory store");
    println!("  .zymi/                     — runtime data (events.db)");
    println!();
    println!("Pipeline DAG:");
    println!("  search_web ──┐");
    println!("               ├─→ analyze ──→ write_report");
    println!("  search_deep ─┘");
    println!();
    println!("Next:");
    println!("  1. Configure your LLM provider in project.yml");
    println!("  2. Run: zymi run research");

    Ok(())
}

// ---------------------------------------------------------------------------
// MCP example scaffold
// ---------------------------------------------------------------------------

fn scaffold_mcp(root: &Path, project_name: &str) -> Result<(), String> {
    create_dir(root, "sandbox")?;

    write_file(
        &root.join("project.yml"),
        &format!(
            r#"name: {project_name}
version: "0.1"

# The whole point of this example: one `mcp_servers:` entry = N tools from an
# off-the-shelf MCP server, no per-tool YAML schemas to write. See
# https://modelcontextprotocol.io for the protocol and a server catalogue.

llm:
  provider: openai
  model: gpt-4o-mini
  api_key: ${{env.OPENAI_API_KEY}}

mcp_servers:
  - name: fs
    command:
      - npx
      - "-y"
      - "@modelcontextprotocol/server-filesystem"
      - ./sandbox
    # `PATH` is auto-forwarded so `npx` resolves. Every other variable the
    # parent process has (OPENAI_API_KEY, credentials, …) stays out of the
    # child unless you name it here — ADR-0023 §security posture.
    # The server advertises 14 tools. Narrow to what this demo actually
    # uses — everything else stays unreachable to the agent.
    allow:
      - read_text_file
      - write_file
      - list_directory
      - search_files
    init_timeout_secs: 15
    call_timeout_secs: 30
    restart:
      max_restarts: 2
      backoff_secs: [1, 5]

defaults:
  timeout_secs: 60
  max_iterations: 10

policy:
  enabled: true
  # No shell access in this example — the agent only talks to the MCP server.
  allow: []
  deny: ["*"]

contracts:
  file_write:
    allowed_dirs: ["./sandbox", "./output"]
    deny_patterns: ["*.env", "*.key"]
"#
        ),
    )?;

    write_file(
        &root.join("agents/default.yml"),
        r#"name: default
description: "Agent that reads/writes files via the MCP filesystem server."
system_prompt: |
  You are a filesystem assistant operating inside a sandboxed directory.

  All file operations go through the MCP filesystem server, exposed as tools
  prefixed with `mcp__fs__`. Paths are resolved relative to the server's
  configured root, so `./notes.md` and `notes.md` both mean the same file
  inside the sandbox.

  Workflow:
  1. Use `mcp__fs__list_directory` to see what is already there.
  2. Use `mcp__fs__read_text_file` to read existing files before overwriting.
  3. Use `mcp__fs__write_file` to produce your output.

  Be concise. Do the work, then stop — do not narrate every step.
tools:
  - mcp__fs__list_directory
  - mcp__fs__read_text_file
  - mcp__fs__write_file
  - mcp__fs__search_files
max_iterations: 8
"#,
    )?;

    write_file(
        &root.join("pipelines/main.yml"),
        r#"name: main
description: "Single-step demo: agent uses MCP filesystem tools to fulfil a task."

steps:
  - id: do_it
    agent: default
    task: "${inputs.task}"

input:
  type: text

output:
  step: do_it
"#,
    )?;

    write_file(
        &root.join("sandbox/README.md"),
        r#"This directory is the sandbox the MCP filesystem server has access to.

Everything the agent reads and writes lives here. Feel free to drop files in
before running the demo — the agent can list, read, search, and overwrite
them.
"#,
    )?;

    println!("Initialized zymi project '{project_name}' with MCP example");
    println!();
    println!("  project.yml              — one `mcp_servers:` entry pulls the filesystem server");
    println!("  agents/default.yml       — agent wired to `mcp__fs__*` tools");
    println!("  pipelines/main.yml       — single-step demo pipeline");
    println!("  sandbox/                  — the only dir the MCP server can touch");
    println!("  .zymi/                   — runtime data (events.db)");
    println!();
    println!("Prerequisites:");
    println!("  - `node` + `npx` on PATH (`npx -y @modelcontextprotocol/server-filesystem` on first run");
    println!("     may prompt to download the package)");
    println!("  - `OPENAI_API_KEY` exported, or swap the provider in project.yml");
    println!();
    println!("Next:");
    println!("  1. export OPENAI_API_KEY=sk-...");
    println!("  2. zymi run main -i task=\"List everything in the sandbox, then write notes.md summarising it.\"");
    println!();
    println!("Tip: before wiring any new MCP server into project.yml, probe it:");
    println!("  zymi mcp probe <name> -- <command> [args...]");

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
  # echoes "not configured" when no provider is wired. We allow `echo` so
  # the placeholder can run; once you uncomment a real HTTP provider in
  # `tools/*.yml`, neither tool touches the shell at all.
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
  - If a tool returns "not configured", proceed using your own knowledge —
    don't mention the tool's failure to the user.
tools:
  - web_search
  - web_scrape
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

input:
  type: text

output:
  step: polish
"#,
    )?;

    // -- tools/web_search.yml + tools/web_scrape.yml --
    write_file(&root.join("tools/web_search.yml"), WEB_SEARCH_TOOL)?;
    write_file(&root.join("tools/web_scrape.yml"), WEB_SCRAPE_TOOL)?;

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
    println!("  project.yml              — Telegram http_poll + http_post wiring");
    println!("  agents/assistant.yml     — drafts the reply (uses web_search / web_scrape)");
    println!("  agents/reviewer.yml      — verifies the draft, polishes if needed");
    println!("  pipelines/chat.yml       — DAG: respond → polish");
    println!("  tools/web_search.yml     — declarative search tool (configure a provider)");
    println!("  tools/web_scrape.yml     — declarative scrape tool (configure a provider)");
    println!("  .env.example             — token placeholders");
    println!("  .zymi/                   — runtime data (events.db, connectors.db)");
    println!();
    println!("Next:");
    println!("  1. Message @BotFather in Telegram and create a bot; copy the token.");
    println!("  2. cp .env.example .env  &&  fill TELEGRAM_BOT_TOKEN + OPENAI_API_KEY");
    println!("  3. Edit project.yml → connectors[0].filter: replace 'your_username_here'");
    println!("     with your actual Telegram username (no @). This is what keeps");
    println!("     strangers who somehow found your token out of the bot.");
    println!("  4. (optional) Open tools/web_search.yml and uncomment a provider block");
    println!("     (Brave / Tavily / SerpAPI / Google) + set its API key in .env.");
    println!("     Without this the bot still works — it just answers from its own");
    println!("     knowledge instead of searching the web.");
    println!("  5. set -a; source .env; set +a   # export vars to child processes");
    println!("     ~/.../target/release/zymi serve chat   # or your installed `zymi`");
    println!();
    println!("Tip: once running, message the bot on Telegram — you should see an agent");
    println!("     reply within a couple of seconds. The DAG (respond → polish) is");
    println!("     visible live in `zymi observe`; outbound POSTs to Telegram show as");
    println!("     `outbound_dispatched` events in `zymi events`.");

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
