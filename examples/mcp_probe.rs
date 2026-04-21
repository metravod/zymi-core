//! Smoke-test one MCP server: spawn it, handshake, list its tools, shut down.
//!
//! Unlike `zymi run`, this does not require an LLM or a project directory — it
//! exercises `McpServerConnection` directly so you can verify a server boots
//! and see exactly which tool names / descriptions it advertises before you
//! write an `allow:` list in `project.yml`.
//!
//! Usage:
//!   cargo run --example mcp_probe -- <name> <command> [args...]
//!
//! Example:
//!   cargo run --example mcp_probe -- fs npx -y \
//!     @modelcontextprotocol/server-filesystem /tmp
//!
//! Extra environment variables for the server are pulled from pairs named
//! `MCP_ENV_<KEY>=<VALUE>` in the current process env. `PATH` is already
//! auto-forwarded (see `resolve_child_env` in `src/mcp/connection.rs`); use
//! `MCP_ENV_*` only for secrets the server needs, e.g. API tokens.

use std::collections::HashMap;
use std::time::Duration;

use zymi_core::mcp::{McpServerConnection, McpServerSpec};

#[tokio::main]
async fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let name = args
        .next()
        .ok_or_else(|| "usage: mcp_probe <name> <command> [args...]".to_string())?;
    let command: Vec<String> = args.collect();
    if command.is_empty() {
        return Err("missing command".into());
    }

    let env: HashMap<String, String> = std::env::vars()
        .filter_map(|(k, v)| k.strip_prefix("MCP_ENV_").map(|stripped| (stripped.to_string(), v)))
        .collect();

    let spec = McpServerSpec {
        name: name.clone(),
        command: command.clone(),
        env,
        cwd: None,
    };

    eprintln!("probe: spawning {} via {:?}", name, command);
    let conn = McpServerConnection::connect(spec, Duration::from_secs(15), Duration::from_secs(30))
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
    eprintln!("probe: handshake ok");

    let tools = conn
        .list_tools()
        .await
        .map_err(|e| format!("tools/list failed: {e}"))?;

    println!("server '{}' advertises {} tool(s):", name, tools.len());
    for tool in &tools {
        let desc = tool.description.as_deref().unwrap_or("");
        let first_line = desc.lines().next().unwrap_or("").trim();
        println!("  - {} — {}", tool.name, first_line);
    }

    conn.shutdown(Duration::from_secs(2)).await;
    Ok(())
}
