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

use async_trait::async_trait;

use crate::config::tool::{HttpMethod, ImplementationConfig};
use crate::engine::tools::{execute_builtin_tool, MemoryStore};
use super::tool_catalog::ToolCatalog;

/// Per-call context for an [`ActionExecutor`].
///
/// Lifetimes are tied to a single tool invocation: `project_root` is the
/// pipeline's working directory and `memory` is the per-run [`MemoryStore`]
/// created by the [`super::Runtime`] command handler. Keeping memory in the
/// context (rather than inside the executor) preserves today's behaviour
/// where each pipeline run starts with a fresh memory store, even when many
/// runs share the same `Runtime` (as in `zymi serve`).
pub struct ActionContext<'a> {
    pub project_root: &'a Path,
    pub memory: &'a MemoryStore,
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
        execute_builtin_tool(tool_name, arguments_json, ctx.project_root, ctx.memory).await
    }
}

/// Catalog-aware executor: dispatches approved tool calls through the
/// [`ToolCatalog`]. Built-in tools delegate to [`execute_builtin_tool`];
/// declarative `kind: http` tools are dispatched via reqwest.
#[derive(Clone)]
pub struct CatalogActionExecutor {
    catalog: Arc<ToolCatalog>,
}

impl CatalogActionExecutor {
    pub fn new(catalog: Arc<ToolCatalog>) -> Self {
        Self { catalog }
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
        if self.catalog.is_declarative(tool_name) {
            let config = self.catalog.declarative_config(tool_name)
                .ok_or_else(|| format!("declarative tool '{tool_name}' config missing"))?;
            execute_declarative_http(config, arguments_json).await
        } else {
            execute_builtin_tool(tool_name, arguments_json, ctx.project_root, ctx.memory).await
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
    }
}

/// Parse LLM arguments JSON into a flat string map for `${args.X}` substitution.
/// Nested objects/arrays are serialized as JSON strings.
fn parse_args_for_interpolation(arguments_json: &str) -> HashMap<String, String> {
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
fn resolve_args(template: &str, args: &HashMap<String, String>) -> String {
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
    use std::path::PathBuf;

    #[tokio::test]
    async fn builtin_executor_runs_write_memory() {
        let memory = new_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
        };
        let executor = BuiltinActionExecutor::new();
        let result = executor
            .execute("write_memory", r#"{"key":"k","content":"v"}"#, &ctx)
            .await
            .unwrap();
        assert!(result.contains("k"));
        assert_eq!(memory.lock().await.get("k").unwrap(), "v");
    }

    #[tokio::test]
    async fn builtin_executor_unknown_tool_errors() {
        let memory = new_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
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
        let memory = new_memory_store();
        let root = PathBuf::from(".");
        let ctx = ActionContext {
            project_root: &root,
            memory: &memory,
        };
        let executor = CatalogActionExecutor::new(catalog);
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
