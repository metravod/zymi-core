use std::fs;
use std::path::Path;

const KNOWN_EXAMPLES: &[&str] = &["research"];

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

# Placeholder — returns a message instead of real results.
# Replace with one of the provider blocks above.
implementation:
  kind: shell
  command_template: "echo 'web_search is not configured. Edit tools/web_search.yml to connect a search provider.'"
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

    println!("Initialized zymi project '{project_name}'");
    println!();
    println!("  project.yml              — project configuration");
    println!("  agents/default.yml       — default agent");
    println!("  pipelines/main.yml       — main pipeline");
    println!("  tools/web_search.yml     — web search tool (configure your provider)");
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

    println!("Initialized zymi project '{project_name}' with research example");
    println!();
    println!("  project.yml                — project configuration");
    println!("  agents/researcher.yml      — research agent (web_search, web_scrape, write_memory)");
    println!("  agents/writer.yml          — writer agent (read_file, write_file)");
    println!("  pipelines/research.yml     — 4-step research pipeline with parallel search");
    println!("  tools/web_search.yml       — web search tool (configure your provider)");
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
