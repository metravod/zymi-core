use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::types::ToolDefinition;

/// Shared memory store for write_memory/read_memory tools within a pipeline run.
pub type MemoryStore = Arc<Mutex<HashMap<String, String>>>;

pub fn new_memory_store() -> MemoryStore {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Execute a built-in tool by name. Returns the tool output as a string.
pub async fn execute_builtin_tool(
    tool_name: &str,
    arguments_json: &str,
    project_root: &Path,
    memory: &MemoryStore,
) -> Result<String, String> {
    let args: serde_json::Value = serde_json::from_str(arguments_json)
        .map_err(|e| format!("invalid tool arguments JSON: {e}"))?;

    match tool_name {
        "execute_shell_command" => {
            let command = args
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or("missing 'command' argument")?;
            let timeout_secs = args
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(30);
            execute_shell_command(command, timeout_secs, project_root).await
        }
        "read_file" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("missing 'path' argument")?;
            read_file(path, project_root).await
        }
        "write_file" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("missing 'path' argument")?;
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or("missing 'content' argument")?;
            write_file(path, content, project_root).await
        }
        "web_search" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or("missing 'query' argument")?;
            Ok(format!(
                "[web_search not connected to a search provider] Query: {query}"
            ))
        }
        "web_scrape" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or("missing 'url' argument")?;
            web_scrape(url).await
        }
        "write_memory" => {
            let key = args
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or("missing 'key' argument")?;
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or("missing 'content' argument")?;
            memory
                .lock()
                .await
                .insert(key.to_string(), content.to_string());
            Ok(format!("Stored memory key '{key}'"))
        }
        other => Err(format!("unknown built-in tool: {other}")),
    }
}

async fn execute_shell_command(
    command: &str,
    timeout_secs: u64,
    cwd: &Path,
) -> Result<String, String> {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn command: {e}"))?;

    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await
        .map_err(|_| format!("command timed out after {timeout_secs}s"))?
        .map_err(|e| format!("command failed: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        Ok(if stdout.is_empty() {
            "(no output)".to_string()
        } else {
            truncate_output(&stdout, 4000)
        })
    } else {
        let code = output.status.code().unwrap_or(-1);
        Err(format!(
            "exit code {code}\nstdout: {}\nstderr: {}",
            truncate_output(&stdout, 2000),
            truncate_output(&stderr, 2000),
        ))
    }
}

async fn read_file(path: &str, project_root: &Path) -> Result<String, String> {
    let full_path = project_root.join(path);
    let content = tokio::fs::read_to_string(&full_path)
        .await
        .map_err(|e| format!("failed to read {}: {e}", full_path.display()))?;
    Ok(truncate_output(&content, 8000))
}

async fn write_file(path: &str, content: &str, project_root: &Path) -> Result<String, String> {
    let full_path = project_root.join(path);
    if let Some(parent) = full_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("failed to create directories: {e}"))?;
    }
    tokio::fs::write(&full_path, content)
        .await
        .map_err(|e| format!("failed to write {}: {e}", full_path.display()))?;
    Ok(format!("Written {} bytes to {path}", content.len()))
}

async fn web_scrape(url: &str) -> Result<String, String> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| format!("failed to fetch {url}: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed to read response body: {e}"))?;

    Ok(truncate_output(&body, 8000))
}

fn truncate_output(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max_chars);
        format!("{}...\n[truncated at {max_chars} chars]", &s[..end])
    }
}

/// Build ToolDefinition list for an agent's declared tools.
pub fn tool_definitions_for_agent(tools: &[String]) -> Vec<ToolDefinition> {
    tools.iter().filter_map(|name| builtin_tool_def(name)).collect()
}

fn builtin_tool_def(name: &str) -> Option<ToolDefinition> {
    let (desc, params) = match name {
        "execute_shell_command" => (
            "Execute a shell command on the host",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to run"},
                    "timeout_secs": {"type": "integer", "description": "Timeout in seconds (default 30)"}
                },
                "required": ["command"]
            }),
        ),
        "read_file" => (
            "Read a file's contents",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path from project root"}
                },
                "required": ["path"]
            }),
        ),
        "write_file" => (
            "Write content to a file",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path from project root"},
                    "content": {"type": "string", "description": "File content to write"}
                },
                "required": ["path", "content"]
            }),
        ),
        "web_search" => (
            "Search the web for information",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"}
                },
                "required": ["query"]
            }),
        ),
        "web_scrape" => (
            "Fetch the contents of a URL",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "URL to fetch"}
                },
                "required": ["url"]
            }),
        ),
        "write_memory" => (
            "Store a key-value pair in the agent's memory",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Memory key"},
                    "content": {"type": "string", "description": "Content to store"}
                },
                "required": ["key", "content"]
            }),
        ),
        _ => return None,
    };

    Some(ToolDefinition {
        name: name.to_string(),
        description: desc.to_string(),
        parameters: params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_known_tools() {
        let tools = vec![
            "execute_shell_command".into(),
            "read_file".into(),
            "write_file".into(),
            "web_search".into(),
            "web_scrape".into(),
            "write_memory".into(),
        ];
        let defs = tool_definitions_for_agent(&tools);
        assert_eq!(defs.len(), 6);
        assert_eq!(defs[0].name, "execute_shell_command");
    }

    #[test]
    fn tool_definitions_skip_unknown() {
        let tools = vec!["unknown_tool".into(), "read_file".into()];
        let defs = tool_definitions_for_agent(&tools);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "read_file");
    }

    #[test]
    fn truncate_output_short() {
        assert_eq!(truncate_output("hello", 100), "hello");
    }

    #[test]
    fn truncate_output_long() {
        let long = "a".repeat(100);
        let result = truncate_output(&long, 50);
        assert!(result.contains("[truncated"));
        assert!(result.len() < 100);
    }

    #[tokio::test]
    async fn memory_store_roundtrip() {
        let mem = new_memory_store();
        let result =
            execute_builtin_tool("write_memory", r#"{"key":"k","content":"v"}"#, Path::new("."), &mem)
                .await
                .unwrap();
        assert!(result.contains("k"));
        assert_eq!(mem.lock().await.get("k").unwrap(), "v");
    }

    #[tokio::test]
    async fn shell_echo() {
        let mem = new_memory_store();
        let result = execute_builtin_tool(
            "execute_shell_command",
            r#"{"command":"echo hello"}"#,
            Path::new("."),
            &mem,
        )
        .await
        .unwrap();
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn read_nonexistent_file() {
        let mem = new_memory_store();
        let result = execute_builtin_tool(
            "read_file",
            r#"{"path":"definitely_does_not_exist_xyz.txt"}"#,
            Path::new("."),
            &mem,
        )
        .await;
        assert!(result.is_err());
    }
}
