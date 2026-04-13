//! Action execution port.
//!
//! Splits "what an agent wants to do" (an [`Intention`] approved by the
//! [`Orchestrator`](crate::esaa::orchestrator::Orchestrator)) from "how it
//! actually happens" (running the underlying tool against host resources).
//!
//! Slice 1 of the runtime unification (ADR-0013) lifts the inline
//! `OrchestratorResult::Approved → execute_builtin_tool(...)` branch out of
//! the engine loop and behind a trait so the engine no longer owns built-in
//! tool details. Custom executors (Python `@tool`, MCP, sandboxed runners)
//! plug in by implementing the same trait.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use crate::config::tool::{HttpMethod, ImplementationConfig};
use crate::engine::tools::{execute_builtin_tool, MemoryStore};
use super::shell_session::{ShellSessionPool};
use super::tool_catalog::ToolCatalog;

use log::warn;

/// Per-call context for an [`ActionExecutor`].
///
/// Lifetimes are tied to a single tool invocation: `project_root` is the
/// pipeline's working directory and `memory` is the per-run [`MemoryStore`]
/// (event-sourced via [`crate::engine::tools::MemoryBridge`], ADR-0016 §2).
/// Keeping memory in the context (rather than inside the executor) preserves
/// today's behaviour where each pipeline run starts with a fresh memory store,
/// even when many runs share the same `Runtime` (as in `zymi serve`).
pub struct ActionContext<'a> {
    pub project_root: &'a Path,
    pub memory: &'a MemoryStore,
    /// Stream identifier — used to look up the persistent shell session
    /// and to scope memory events.
    pub stream_id: &'a str,
    /// Correlation ID for the current pipeline run — used to link memory
    /// events back to the originating request.
    pub correlation_id: Uuid,
}

/// Executes an approved tool call. Implementors are expected to be
/// side-effect-only — policy/approval/event emission is the orchestrator's
/// job, not the executor's.
#[async_trait]
pub trait ActionExecutor: Send + Sync {
    /// Run a tool by name. `arguments_json` is the raw JSON arguments string
    /// produced by the LLM, identical to what the previous inline path
    /// passed to `execute_builtin_tool`.
    async fn execute(
        &self,
        tool_name: &str,
        arguments_json: &str,
        ctx: &ActionContext<'_>,
    ) -> Result<String, String>;
}

/// Default executor: dispatches to the built-in tool implementations in
/// [`crate::engine::tools`]. Stateless — all per-run state lives in
/// [`ActionContext`].
///
/// **Deprecated**: use [`CatalogActionExecutor`] instead. This type is
/// kept for backwards compatibility with callers that construct an executor
/// without a [`super::ToolCatalog`].
#[derive(Debug, Default, Clone, Copy)]
pub struct BuiltinActionExecutor;

impl BuiltinActionExecutor {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ActionExecutor for BuiltinActionExecutor {
    async fn execute(
        &self,
        tool_name: &str,
        arguments_json: &str,
        ctx: &ActionContext<'_>,
    ) -> Result<String, String> {
        execute_builtin_tool(
            tool_name,
            arguments_json,
            ctx.project_root,
            ctx.memory,
            ctx.stream_id,
            ctx.correlation_id,
        )
        .await
    }
}

/// Catalog-aware executor: dispatches approved tool calls through the
/// [`ToolCatalog`]. Built-in tools delegate to [`execute_builtin_tool`];
/// `execute_shell_command` is routed to the persistent [`ShellSessionPool`];
/// declarative `kind: http` tools are dispatched via reqwest.
#[derive(Clone)]
pub struct CatalogActionExecutor {
    catalog: Arc<ToolCatalog>,
    shell_pool: Arc<ShellSessionPool>,
}

impl CatalogActionExecutor {
    pub fn new(catalog: Arc<ToolCatalog>, shell_pool: Arc<ShellSessionPool>) -> Self {
        Self { catalog, shell_pool }
    }
}

#[async_trait]
impl ActionExecutor for CatalogActionExecutor {
    async fn execute(
        &self,
        tool_name: &str,
        arguments_json: &str,
        ctx: &ActionContext<'_>,
    ) -> Result<String, String> {
        if tool_name == "execute_shell_command" {
            return self.execute_shell(arguments_json, ctx).await;
        }
        if self.catalog.is_declarative(tool_name) {
            let config = self.catalog.declarative_config(tool_name)
                .ok_or_else(|| format!("declarative tool '{tool_name}' config missing"))?;
            match &config.implementation {
                ImplementationConfig::Http { .. } => {
                    execute_declarative_http(config, arguments_json).await
                }
                ImplementationConfig::Shell { .. } => {
                    self.execute_declarative_shell(config, arguments_json, ctx)
                        .await
                }
            }
        } else {
            execute_builtin_tool(
                tool_name,
                arguments_json,
                ctx.project_root,
                ctx.memory,
                ctx.stream_id,
                ctx.correlation_id,
            )
            .await
        }
    }
}

impl CatalogActionExecutor {
    /// Route a declarative `kind: shell` tool through the persistent pool.
    ///
    /// Resolves `${args.X}` placeholders in the command template, then
    /// dispatches into the same [`ShellSessionPool`] as the built-in
    /// `execute_shell_command`.
    async fn execute_declarative_shell(
        &self,
        config: &crate::config::tool::ToolConfig,
        arguments_json: &str,
        ctx: &ActionContext<'_>,
    ) -> Result<String, String> {
        let (command_template, timeout_secs) = match &config.implementation {
            ImplementationConfig::Shell {
                command_template,
                timeout_secs,
            } => (command_template.as_str(), timeout_secs.unwrap_or(30)),
            _ => return Err(format!("tool '{}' is not kind: shell", config.name)),
        };

        // Log a warning if requires_approval was explicitly set to false on a
        // shell tool (ADR-0014 §4 unsafe-shell warning).
        if config.requires_approval == Some(false) {
            warn!(
                "tool '{}' is kind=shell with requires_approval=false — \
                 bypasses the execute_shell_command policy gate; \
                 verify this is intentional",
                config.name
            );
        }

        let args = parse_args_for_interpolation(arguments_json);
        let resolved_command = resolve_args(command_template, &args);

        let output = self
            .shell_pool
            .run_command(
                ctx.stream_id,
                &resolved_command,
                Duration::from_secs(timeout_secs),
                ctx.project_root,
            )
            .await
            .map_err(|e| format!("shell session error: {e}"))?;

        if output.timed_out {
            Err(format!(
                "command timed out after {timeout_secs}s\nstdout: {}\nstderr: {}",
                truncate_output(&output.stdout, 2000),
                truncate_output(&output.stderr, 2000),
            ))
        } else if output.exit_code == 0 {
            Ok(if output.stdout.is_empty() {
                "(no output)".to_string()
            } else {
                truncate_output(&output.stdout, 4000)
            })
        } else {
            Err(format!(
                "exit code {}\nstdout: {}\nstderr: {}",
                output.exit_code,
                truncate_output(&output.stdout, 2000),
                truncate_output(&output.stderr, 2000),
            ))
        }
    }

    /// Route `execute_shell_command` through the persistent shell pool.
    async fn execute_shell(
        &self,
        arguments_json: &str,
        ctx: &ActionContext<'_>,
    ) -> Result<String, String> {
        let args: serde_json::Value = serde_json::from_str(arguments_json)
            .map_err(|e| format!("invalid tool arguments JSON: {e}"))?;

        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or("missing 'command' argument")?;
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(30);

        let output = self
            .shell_pool
            .run_command(
                ctx.stream_id,
                command,
                Duration::from_secs(timeout_secs),
                ctx.project_root,
            )
            .await
            .map_err(|e| format!("shell session error: {e}"))?;

        if output.timed_out {
            Err(format!(
                "command timed out after {timeout_secs}s\nstdout: {}\nstderr: {}",
                truncate_output(&output.stdout, 2000),
                truncate_output(&output.stderr, 2000),
            ))
        } else if output.exit_code == 0 {
            Ok(if output.stdout.is_empty() {
                "(no output)".to_string()
            } else {
                truncate_output(&output.stdout, 4000)
            })
        } else {
            Err(format!(
                "exit code {}\nstdout: {}\nstderr: {}",
                output.exit_code,
                truncate_output(&output.stdout, 2000),
                truncate_output(&output.stderr, 2000),
            ))
        }
    }
}

/// Execute a declarative `kind: http` tool.
async fn execute_declarative_http(
    config: &crate::config::tool::ToolConfig,
    arguments_json: &str,
) -> Result<String, String> {
    let args: HashMap<String, String> = parse_args_for_interpolation(arguments_json);

    match &config.implementation {
        ImplementationConfig::Http {
            method,
            url,
            headers,
            body_template,
        } => {
            let resolved_url = resolve_args(url, &args);

            let client = reqwest::Client::new();
            let mut builder = match method {
                HttpMethod::Get => client.get(&resolved_url),
                HttpMethod::Post => client.post(&resolved_url),
                HttpMethod::Put => client.put(&resolved_url),
                HttpMethod::Patch => client.patch(&resolved_url),
                HttpMethod::Delete => client.delete(&resolved_url),
            };

            for (key, value) in headers {
                builder = builder.header(key, resolve_args(value, &args));
            }

            if let Some(template) = body_template {
                let body = resolve_args(template, &args);
                builder = builder.body(body);
            }

            let resp = builder
                .send()
                .await
                .map_err(|e| format!("HTTP request failed: {e}"))?;

            let status = resp.status();
            let body = resp
                .text()
                .await
                .map_err(|e| format!("failed to read response body: {e}"))?;

            if status.is_success() {
                Ok(truncate_output(&body, 8000))
            } else {
                Err(format!("HTTP {status}: {}", truncate_output(&body, 2000)))
            }
        }
        // Shell tools are dispatched by CatalogActionExecutor before reaching
        // this function; if we get here it's a bug.
        ImplementationConfig::Shell { .. } => {
            Err("kind: shell tools should be dispatched via execute_declarative_shell".into())
        }
    }
}

/// Parse LLM arguments JSON into a flat string map for `${args.X}` substitution.
/// Nested objects/arrays are serialized as JSON strings.
pub(crate) fn parse_args_for_interpolation(arguments_json: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(serde_json::Value::Object(obj)) = serde_json::from_str(arguments_json) {
        for (key, value) in obj {
            let s = match &value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            map.insert(key, s);
        }
    }
    map
}

/// Resolve `${args.X}` placeholders in a template string.
pub(crate) fn resolve_args(template: &str, args: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in args {
        result = result.replace(&format!("${{args.{key}}}"), value);
    }
    result
}

fn truncate_output(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max_chars);
        format!("{}...\n[truncated at {max_chars} chars]", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tools::new_memory_store;
    use crate::events::bus::EventBus;
    use crate::events::store::{open_store, StoreBackend};
    use std::path::PathBuf;

    fn test_memory_store() -> crate::engine::tools::MemoryStore {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_events.db");
        let store = open_store(StoreBackend::Sqlite { path: db_path }).unwrap();
        let bus = Arc::new(EventBus::new(store));
        std::mem::forget(dir);
        new_memory_store(bus)
    }

    #[tokio::test]
    async fn builtin_executor_runs_write_memory() {
        let memory = test_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
            stream_id: "test",
            correlation_id: Uuid::new_v4(),
        };
        let executor = BuiltinActionExecutor::new();
        let result = executor
            .execute("write_memory", r#"{"key":"k","content":"v"}"#, &ctx)
            .await
            .unwrap();
        assert!(result.contains("k"));
        // Verify via read_memory tool instead of direct HashMap access
        let read_result = executor
            .execute("read_memory", r#"{"key":"k"}"#, &ctx)
            .await
            .unwrap();
        assert_eq!(read_result, "v");
    }

    #[tokio::test]
    async fn builtin_executor_unknown_tool_errors() {
        let memory = test_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
            stream_id: "test",
            correlation_id: Uuid::new_v4(),
        };
        let executor = BuiltinActionExecutor::new();
        let err = executor
            .execute("nope", "{}", &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("unknown built-in tool"));
    }

    #[tokio::test]
    async fn catalog_executor_dispatches_builtin() {
        let catalog = Arc::new(ToolCatalog::builtin_only());
        let pool = Arc::new(ShellSessionPool::new(Duration::from_secs(60)));
        let memory = test_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
            stream_id: "test",
            correlation_id: Uuid::new_v4(),
        };
        let executor = CatalogActionExecutor::new(catalog, pool);
        let result = executor
            .execute("write_memory", r#"{"key":"k2","content":"v2"}"#, &ctx)
            .await
            .unwrap();
        assert!(result.contains("k2"));
    }

    #[test]
    fn resolve_args_basic() {
        let mut args = HashMap::new();
        args.insert("channel".into(), "C123".into());
        args.insert("text".into(), "hello world".into());

        let template = r#"{"channel": "${args.channel}", "text": "${args.text}"}"#;
        let result = resolve_args(template, &args);
        let expected = r#"{"channel": "C123", "text": "hello world"}"#;
        assert_eq!(result, expected);
    }

    #[test]
    fn resolve_args_no_placeholders() {
        let args = HashMap::new();
        let template = "https://example.com/api";
        assert_eq!(resolve_args(template, &args), template);
    }

    #[test]
    fn parse_args_flat_object() {
        let args = parse_args_for_interpolation(r#"{"name":"test","count":42}"#);
        assert_eq!(args.get("name").unwrap(), "test");
        assert_eq!(args.get("count").unwrap(), "42");
    }

    #[test]
    fn parse_args_invalid_json() {
        let args = parse_args_for_interpolation("not json");
        assert!(args.is_empty());
    }
}
