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

use std::path::Path;

use async_trait::async_trait;

use crate::engine::tools::{execute_builtin_tool, MemoryStore};

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
}
