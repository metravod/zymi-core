use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use uuid::Uuid;

use crate::esaa::projections::{MemoryProjection, Projection};
use crate::events::bus::EventBus;
use crate::events::{Event, EventKind};
use crate::types::ToolDefinition;

/// Event-sourced workflow memory (ADR-0016 §2).
///
/// Replaces the legacy `Arc<Mutex<HashMap<String, String>>>`. Writes go through
/// the [`EventBus`] as `MemoryWritten` events; reads come from the
/// [`MemoryProjection`]. The projection is updated **synchronously** after each
/// publish so that a `write_memory` followed by `read_memory` in the same
/// iteration always sees the write.
pub struct MemoryBridge {
    projection: RwLock<MemoryProjection>,
    bus: Arc<EventBus>,
}

impl MemoryBridge {
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self {
            projection: RwLock::new(MemoryProjection::new()),
            bus,
        }
    }

    /// Write a key-value pair. Emits `MemoryWritten` on the bus and applies
    /// to the local projection before returning — the next `get()` call in
    /// the same iteration will see the value.
    pub async fn write(
        &self,
        key: &str,
        value: &str,
        stream_id: &str,
        correlation_id: Uuid,
    ) -> Result<(), String> {
        let previous_value_seq = self.projection.read().await.seq_for(key);

        let mut event = Event::new(
            stream_id.to_string(),
            EventKind::MemoryWritten {
                key: key.to_string(),
                value: value.to_string(),
                previous_value_seq,
            },
            "agent".to_string(),
        )
        .with_correlation(correlation_id);

        self.bus
            .publish(event.clone())
            .await
            .map_err(|e| format!("failed to publish MemoryWritten: {e}"))?;

        // Synchronous local update — the bus.publish() call above assigned
        // event.sequence via the store, but we cloned before publish mutated
        // the sequence. Re-read isn't needed: apply() only uses sequence for
        // written_at_seq tracking, and the projection will get the real event
        // via its bus subscription for future replays. For the local
        // synchronous guarantee, we assign a synthetic high sequence so that
        // seq_for() returns a value > 0 (the exact number doesn't matter for
        // correctness within a single run — it only matters for
        // previous_value_seq which we already captured above).
        event.sequence = u64::MAX; // placeholder — overwritten by next real apply from bus
        self.projection.write().await.apply(&event);

        Ok(())
    }

    /// Delete a key. Emits `MemoryDeleted` on the bus.
    pub async fn delete(
        &self,
        key: &str,
        stream_id: &str,
        correlation_id: Uuid,
    ) -> Result<(), String> {
        let previous_value_seq = self.projection.read().await.seq_for(key);
        let Some(prev_seq) = previous_value_seq else {
            return Err(format!("memory key '{key}' not found"));
        };

        let mut event = Event::new(
            stream_id.to_string(),
            EventKind::MemoryDeleted {
                key: key.to_string(),
                previous_value_seq: prev_seq,
            },
            "agent".to_string(),
        )
        .with_correlation(correlation_id);

        self.bus
            .publish(event.clone())
            .await
            .map_err(|e| format!("failed to publish MemoryDeleted: {e}"))?;

        event.sequence = u64::MAX;
        self.projection.write().await.apply(&event);

        Ok(())
    }

    /// Read a key from the projection. Returns `None` if never written or deleted.
    pub async fn get(&self, key: &str) -> Option<String> {
        self.projection.read().await.get(key).map(|s| s.to_string())
    }

    /// Snapshot the full memory state.
    pub async fn snapshot(&self) -> std::collections::HashMap<String, String> {
        self.projection.read().await.snapshot()
    }

    /// Check whether memory is empty.
    pub async fn is_empty(&self) -> bool {
        self.projection.read().await.is_empty()
    }
}

/// Shared handle to the event-sourced memory bridge.
pub type MemoryStore = Arc<MemoryBridge>;

pub fn new_memory_store(bus: Arc<EventBus>) -> MemoryStore {
    Arc::new(MemoryBridge::new(bus))
}

/// Execute a built-in tool by name. Returns the tool output as a string.
pub async fn execute_builtin_tool(
    tool_name: &str,
    arguments_json: &str,
    project_root: &Path,
    memory: &MemoryStore,
    stream_id: &str,
    correlation_id: Uuid,
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
                .write(key, content, stream_id, correlation_id)
                .await?;
            Ok(format!("Stored memory key '{key}'"))
        }
        "read_memory" => {
            let key = args
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or("missing 'key' argument")?;
            match memory.get(key).await {
                Some(value) => Ok(value),
                None => Ok(format!("[memory key '{key}' not found]")),
            }
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
        "read_memory" => (
            "Read a value from the agent's memory by key",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Memory key to read"}
                },
                "required": ["key"]
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
    use crate::events::store::{open_store, StoreBackend};

    fn test_memory_store() -> MemoryStore {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_events.db");
        let store = open_store(StoreBackend::Sqlite { path: db_path }).unwrap();
        let bus = Arc::new(EventBus::new(store));
        // Leak the tempdir so it lives long enough for the test.
        std::mem::forget(dir);
        new_memory_store(bus)
    }

    fn test_correlation_id() -> Uuid {
        Uuid::new_v4()
    }

    #[test]
    fn tool_definitions_known_tools() {
        let tools = vec![
            "execute_shell_command".into(),
            "read_file".into(),
            "write_file".into(),
            "web_scrape".into(),
            "write_memory".into(),
            "read_memory".into(),
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
    async fn memory_write_read_roundtrip() {
        let mem = test_memory_store();
        let corr = test_correlation_id();
        let result = execute_builtin_tool(
            "write_memory",
            r#"{"key":"k","content":"v"}"#,
            Path::new("."),
            &mem,
            "test-stream",
            corr,
        )
        .await
        .unwrap();
        assert!(result.contains("k"));

        // Read back via read_memory tool
        let read_result = execute_builtin_tool(
            "read_memory",
            r#"{"key":"k"}"#,
            Path::new("."),
            &mem,
            "test-stream",
            corr,
        )
        .await
        .unwrap();
        assert_eq!(read_result, "v");
    }

    #[tokio::test]
    async fn memory_read_nonexistent_key() {
        let mem = test_memory_store();
        let corr = test_correlation_id();
        let result = execute_builtin_tool(
            "read_memory",
            r#"{"key":"nope"}"#,
            Path::new("."),
            &mem,
            "test-stream",
            corr,
        )
        .await
        .unwrap();
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn memory_overwrite_preserves_latest() {
        let mem = test_memory_store();
        let corr = test_correlation_id();
        execute_builtin_tool(
            "write_memory",
            r#"{"key":"k","content":"v1"}"#,
            Path::new("."),
            &mem,
            "test-stream",
            corr,
        )
        .await
        .unwrap();

        execute_builtin_tool(
            "write_memory",
            r#"{"key":"k","content":"v2"}"#,
            Path::new("."),
            &mem,
            "test-stream",
            corr,
        )
        .await
        .unwrap();

        let read_result = execute_builtin_tool(
            "read_memory",
            r#"{"key":"k"}"#,
            Path::new("."),
            &mem,
            "test-stream",
            corr,
        )
        .await
        .unwrap();
        assert_eq!(read_result, "v2");
    }

    #[tokio::test]
    async fn memory_write_emits_event() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_events.db");
        let store = open_store(StoreBackend::Sqlite { path: db_path }).unwrap();
        let bus = Arc::new(EventBus::new(Arc::clone(&store)));
        let mem = new_memory_store(Arc::clone(&bus));
        let corr = test_correlation_id();

        execute_builtin_tool(
            "write_memory",
            r#"{"key":"k","content":"v"}"#,
            Path::new("."),
            &mem,
            "test-stream",
            corr,
        )
        .await
        .unwrap();

        // Verify event was persisted in the store
        let events = store.read_stream("test-stream", 0).await.unwrap();
        let memory_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind.tag() == "memory_written")
            .collect();
        assert_eq!(memory_events.len(), 1);
        if let EventKind::MemoryWritten { key, value, previous_value_seq } = &memory_events[0].kind {
            assert_eq!(key, "k");
            assert_eq!(value, "v");
            assert!(previous_value_seq.is_none());
        } else {
            panic!("expected MemoryWritten");
        }
    }

    #[tokio::test]
    async fn memory_overwrite_records_previous_seq() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_events.db");
        let store = open_store(StoreBackend::Sqlite { path: db_path }).unwrap();
        let bus = Arc::new(EventBus::new(Arc::clone(&store)));
        let mem = new_memory_store(Arc::clone(&bus));
        let corr = test_correlation_id();

        // First write
        execute_builtin_tool(
            "write_memory",
            r#"{"key":"k","content":"v1"}"#,
            Path::new("."),
            &mem,
            "test-stream",
            corr,
        )
        .await
        .unwrap();

        // Overwrite
        execute_builtin_tool(
            "write_memory",
            r#"{"key":"k","content":"v2"}"#,
            Path::new("."),
            &mem,
            "test-stream",
            corr,
        )
        .await
        .unwrap();

        let events = store.read_stream("test-stream", 0).await.unwrap();
        let memory_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind.tag() == "memory_written")
            .collect();
        assert_eq!(memory_events.len(), 2);

        // Second write should have previous_value_seq set
        if let EventKind::MemoryWritten { previous_value_seq, .. } = &memory_events[1].kind {
            assert!(previous_value_seq.is_some(), "overwrite should record previous seq");
        } else {
            panic!("expected MemoryWritten");
        }
    }

    #[tokio::test]
    async fn shell_echo() {
        let mem = test_memory_store();
        let corr = test_correlation_id();
        let result = execute_builtin_tool(
            "execute_shell_command",
            r#"{"command":"echo hello"}"#,
            Path::new("."),
            &mem,
            "test-stream",
            corr,
        )
        .await
        .unwrap();
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn read_nonexistent_file() {
        let mem = test_memory_store();
        let corr = test_correlation_id();
        let result = execute_builtin_tool(
            "read_file",
            r#"{"path":"definitely_does_not_exist_xyz.txt"}"#,
            Path::new("."),
            &mem,
            "test-stream",
            corr,
        )
        .await;
        assert!(result.is_err());
    }
}
