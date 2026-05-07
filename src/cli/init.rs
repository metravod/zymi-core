use std::fs;
use std::path::Path;

const KNOWN_EXAMPLES: &[&str] = &["telegram"];

const PROJECT_NAME_PLACEHOLDER: &str = "__PROJECT_NAME__";

// ---------------------------------------------------------------------------
// Scaffold templates — kept as plain .yml/.md/.py files under
// assets/scaffold/ so they round-trip cleanly through editors / PR diffs
// without Rust string-escaping noise. Substitution is a single
// `__PROJECT_NAME__` sentinel; everything else is verbatim.
// ---------------------------------------------------------------------------

// Shared between scaffolds.
const WEB_SEARCH_TOOL: &str = include_str!("../../assets/scaffold/shared/web_search.yml");
const WEB_SCRAPE_TOOL: &str = include_str!("../../assets/scaffold/shared/web_scrape.yml");
const AGENTS_DOC: &str = include_str!("../../assets/scaffold/shared/AGENTS.md");

// Default scaffold.
const DEFAULT_PROJECT_YML: &str = include_str!("../../assets/scaffold/default/project.yml");
const DEFAULT_AGENT_YML: &str = include_str!("../../assets/scaffold/default/agents/default.yml");
const DEFAULT_PIPELINE_YML: &str = include_str!("../../assets/scaffold/default/pipelines/main.yml");

// Telegram example scaffold.
const TG_PROJECT_YML: &str = include_str!("../../assets/scaffold/telegram/project.yml");
const TG_ASSISTANT_YML: &str = include_str!("../../assets/scaffold/telegram/agents/assistant.yml");
const TG_REVIEWER_YML: &str = include_str!("../../assets/scaffold/telegram/agents/reviewer.yml");
const TG_CHAT_PIPELINE_YML: &str = include_str!("../../assets/scaffold/telegram/pipelines/chat.yml");
const TG_BROADCAST_TOOL: &str = include_str!("../../assets/scaffold/telegram/tools/broadcast.yml");
const TG_GET_WEATHER_PY: &str = include_str!("../../assets/scaffold/telegram/tools/get_weather.py");
const TG_TRANSLATE_PY: &str = include_str!("../../assets/scaffold/telegram/tools/translate.py");
const TG_ENV_EXAMPLE: &str = include_str!("../../assets/scaffold/telegram/.env.example");

fn render(template: &str, project_name: &str) -> String {
    template.replace(PROJECT_NAME_PLACEHOLDER, project_name)
}

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
    write_file(&root.join("project.yml"), &render(DEFAULT_PROJECT_YML, project_name))?;
    write_file(&root.join("agents/default.yml"), &render(DEFAULT_AGENT_YML, project_name))?;
    write_file(&root.join("pipelines/main.yml"), DEFAULT_PIPELINE_YML)?;
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
    write_file(&root.join("project.yml"), &render(TG_PROJECT_YML, project_name))?;
    write_file(&root.join("agents/assistant.yml"), TG_ASSISTANT_YML)?;
    write_file(&root.join("agents/reviewer.yml"), TG_REVIEWER_YML)?;
    write_file(&root.join("pipelines/chat.yml"), TG_CHAT_PIPELINE_YML)?;
    write_file(&root.join("tools/web_search.yml"), WEB_SEARCH_TOOL)?;
    write_file(&root.join("tools/web_scrape.yml"), WEB_SCRAPE_TOOL)?;
    write_file(&root.join("tools/broadcast.yml"), TG_BROADCAST_TOOL)?;
    write_file(&root.join("tools/get_weather.py"), TG_GET_WEATHER_PY)?;
    write_file(&root.join("tools/translate.py"), TG_TRANSLATE_PY)?;
    write_file(&root.join("AGENTS.md"), AGENTS_DOC)?;
    write_file(&root.join(".env.example"), TG_ENV_EXAMPLE)?;

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
