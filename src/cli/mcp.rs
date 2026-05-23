//! `zymi mcp` subcommands. Two today:
//! - `probe` — spawn an MCP server, handshake, list its tools, shut down.
//! - `serve` — run zymi *as* an MCP server, exposing pipelines that
//!   opt in via `expose.mcp:` (ADR-0033 Slice 1, sync only).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::BufReader;

use crate::config::load_project_dir;
use crate::mcp::server::{serve as serve_mcp, ServerConfig};
use crate::mcp::{McpServerConnection, McpServerSpec};
use crate::runtime::Runtime;

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

/// Run zymi as an MCP server over stdio (ADR-0033 Slice 1).
///
/// Pipelines that declare `expose.mcp:` are exposed as MCP tools. `--include`
/// and `--exclude` further filter the exposed set by name glob.
///
/// Stdio discipline (the critical bit): `run_pipeline::handle` emits
/// operator-facing `println!` for the `zymi run`/`zymi serve` UX, which
/// would corrupt the JSON-RPC wire. Before any pipeline runs, we duplicate
/// fd 1 into a private handle, redirect process stdout to stderr, and
/// write the wire through the dup'd fd. Everything else (logs, pipeline
/// progress prints) lands on stderr where MCP hosts surface it as a log.
pub fn exec_serve(
    include: &[String],
    exclude: &[String],
    root: impl AsRef<Path>,
) -> Result<(), String> {
    let root = root.as_ref().to_path_buf();
    if !root.join("project.yml").exists() {
        return Err(format!(
            "no project.yml found in {}. Run `zymi init` first.",
            root.display()
        ));
    }
    let workspace =
        load_project_dir(&root).map_err(|e| format!("failed to load project: {e}"))?;

    let config = ServerConfig::new(include.to_vec(), exclude.to_vec());

    let rt = super::runtime();
    rt.block_on(async move {
        let runtime = Arc::new(
            Runtime::builder(workspace, root.clone())
                .build_async()
                .await?,
        );

        let real_stdout = redirect_stdout_for_jsonrpc()
            .map_err(|e| format!("could not isolate stdout for MCP wire: {e}"))?;
        eprintln!("zymi mcp serve: ready (stdio, pipelines filtered by expose.mcp)");

        let stdin = BufReader::new(tokio::io::stdin());
        serve_mcp(runtime, config, stdin, real_stdout).await
    })
}

/// Duplicate fd 1 (stdout) into a private handle and point process fd 1
/// at fd 2 (stderr). Returns the dup'd fd wrapped as a tokio writer.
///
/// After this call, *any* `println!` / `print!` anywhere in the process
/// goes to stderr — only writes through the returned handle land on the
/// MCP client.
#[cfg(unix)]
fn redirect_stdout_for_jsonrpc() -> std::io::Result<tokio::fs::File> {
    use std::os::fd::FromRawFd;

    // Self-contained ffi — avoids a `libc` dep entry for two calls.
    extern "C" {
        fn dup(oldfd: i32) -> i32;
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }

    // Make sure any buffered stdout content is flushed before we
    // re-point the fd — otherwise it would land on the MCP wire later.
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // SAFETY: we hand ownership of the dup'd fd straight into File via
    // from_raw_fd, satisfying the single-owner invariant. dup2 is a
    // pure file-descriptor remap with no aliasing concerns.
    let dup_fd = unsafe { dup(1) };
    if dup_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { dup2(2, 1) } < 0 {
        let err = std::io::Error::last_os_error();
        // Best-effort: close the dup before bailing.
        unsafe {
            extern "C" {
                fn close(fd: i32) -> i32;
            }
            close(dup_fd);
        }
        return Err(err);
    }
    let std_file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
    Ok(tokio::fs::File::from_std(std_file))
}

#[cfg(not(unix))]
fn redirect_stdout_for_jsonrpc() -> std::io::Result<tokio::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "stdio MCP server requires a Unix-like platform; HTTP transport is post-0.7",
    ))
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
