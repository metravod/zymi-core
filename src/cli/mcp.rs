//! `zymi mcp` subcommands. Only `probe` today — spawns one MCP server,
//! completes the handshake, prints its `tools/list`, and shuts down. Useful
//! for deciding what belongs in the `allow:` whitelist before wiring a
//! server into `project.yml`.

use std::collections::HashMap;
use std::time::Duration;

use crate::mcp::{McpServerConnection, McpServerSpec};

pub fn exec_probe(
    name: &str,
    command: &[String],
    env_pairs: &[String],
    init_timeout_secs: u64,
    call_timeout_secs: u64,
) -> Result<(), String> {
    if command.is_empty() {
        return Err("missing command (usage: zymi mcp probe <name> -- <cmd> [args...])".into());
    }

    let env = parse_env_pairs(env_pairs)?;

    let spec = McpServerSpec {
        name: name.to_string(),
        command: command.to_vec(),
        env,
        cwd: None,
    };

    let rt = super::runtime();
    rt.block_on(async move {
        eprintln!("probe: spawning {name} via {command:?}");
        let conn = McpServerConnection::connect(
            spec,
            Duration::from_secs(init_timeout_secs),
            Duration::from_secs(call_timeout_secs),
        )
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
        eprintln!("probe: handshake ok");

        let tools = conn
            .list_tools()
            .await
            .map_err(|e| format!("tools/list failed: {e}"))?;

        println!("server '{name}' advertises {} tool(s):", tools.len());
        for tool in &tools {
            let desc = tool.description.as_deref().unwrap_or("");
            let first_line = desc.lines().next().unwrap_or("").trim();
            println!("  - {} — {first_line}", tool.name);
        }

        conn.shutdown(Duration::from_secs(2)).await;
        Ok::<(), String>(())
    })
}

fn parse_env_pairs(pairs: &[String]) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for pair in pairs {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("--env expects KEY=VALUE, got '{pair}'"))?;
        if k.is_empty() {
            return Err(format!("--env key is empty in '{pair}'"));
        }
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_pairs_accepts_multiple() {
        let out = parse_env_pairs(&["A=1".into(), "B=two=equals".into()]).unwrap();
        assert_eq!(out.get("A").map(String::as_str), Some("1"));
        assert_eq!(out.get("B").map(String::as_str), Some("two=equals"));
    }

    #[test]
    fn parse_env_pairs_rejects_missing_eq() {
        assert!(parse_env_pairs(&["NOEQ".into()]).is_err());
    }

    #[test]
    fn parse_env_pairs_rejects_empty_key() {
        assert!(parse_env_pairs(&["=value".into()]).is_err());
    }
}
