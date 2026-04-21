use std::fs;
use std::path::Path;

const KNOWN_EXAMPLES: &[&str] = &["research", "mcp"];

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

# Placeholder — returns a message instead of real results.
# Replace with one of the provider blocks above.
implementation:
  kind: shell
  command_template: "echo 'web_search is not configured. Edit tools/web_search.yml to connect a search provider.'"
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

# Placeholder — returns a message instead of real results.
# Replace with one of the provider blocks above.
implementation:
  kind: shell
  command_template: "echo 'web_scrape is not configured. Edit tools/web_scrape.yml to connect a scraping provider.'"
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
