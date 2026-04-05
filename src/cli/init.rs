use std::fs;
use std::path::Path;

const KNOWN_EXAMPLES: &[&str] = &["research"];

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

    println!("Initialized zymi project '{project_name}'");
    println!();
    println!("  project.yml          — project configuration");
    println!("  agents/default.yml   — default agent");
    println!("  pipelines/main.yml   — main pipeline");
    println!("  .zymi/               — runtime data (events.db)");
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

    println!("Initialized zymi project '{project_name}' with research example");
    println!();
    println!("  project.yml                — project configuration");
    println!("  agents/researcher.yml      — research agent (web_search, web_scrape, write_memory)");
    println!("  agents/writer.yml          — writer agent (read_file, write_file)");
    println!("  pipelines/research.yml     — 4-step research pipeline with parallel search");
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
