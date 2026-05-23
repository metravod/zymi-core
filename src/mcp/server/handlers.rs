//! MCP method handlers for `zymi mcp serve` (ADR-0033 Slice 1, sync only).
//!
//! Three methods today:
//! - `initialize` — protocol handshake, server info, tools capability.
//! - `tools/list` — enumerate exposed pipelines as MCP tools.
//! - `tools/call` — invoke an exposed pipeline and block on completion.
//!
//! Async-mode pipelines are silently skipped from `tools/list` here;
//! Slice 2 (SEP-1686) will flip the capability bit and add task surface.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::commands::RunPipeline;
use crate::config::pipeline::{McpExposeMode, PipelineConfig};
use crate::handlers::run_pipeline;
use crate::runtime::Runtime;

use super::protocol::{
    RpcError, ERR_INTERNAL, ERR_INVALID_PARAMS, ERR_METHOD_NOT_FOUND,
};

/// Server-side protocol version advertised in `initialize`. Tracks the
/// 2025-11-25 MCP spec (the version with SEP-1686 accepted); the v1
/// surface only uses methods/capabilities that are stable across
/// 2025-06-18 → 2025-11-25, so older clients still negotiate cleanly.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Server identity surfaced to MCP clients via `initialize`.
pub const SERVER_NAME: &str = "zymi";

/// `initialize` result: capabilities + server info + protocol version.
pub fn handle_initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// Enumerate exposed pipelines (`expose.mcp:` opt-in, ADR-0033 §2) as
/// MCP tool descriptors. Async-mode pipelines are skipped in Slice 1.
///
/// `filter` decides which pipeline names are exposed at boot time
/// (`--include` / `--exclude` glob filters); it's applied *before* the
/// `expose:` check so that names which never pass the filter cannot
/// leak through.
pub fn handle_tools_list<F>(runtime: &Runtime, filter: F) -> Value
where
    F: Fn(&str) -> bool,
{
    let mut tools: Vec<Value> = Vec::new();
    let mut names: Vec<&String> = runtime.workspace().pipelines.keys().collect();
    names.sort();
    for name in names {
        if !filter(name) {
            continue;
        }
        let config = &runtime.workspace().pipelines[name];
        let Some(exposure) = config.mcp_exposure() else {
            continue;
        };
        // Slice 1 is sync-only. Async pipelines are declared but not
        // served until Slice 2 (SEP-1686) ships.
        if exposure.mode == McpExposeMode::Async {
            continue;
        }
        let mut tool = Map::new();
        let tool_name = config
            .mcp_tool_name()
            .map(str::to_string)
            .unwrap_or_else(|| config.name.clone());
        tool.insert("name".into(), Value::String(tool_name));
        if let Some(desc) = config.mcp_tool_description() {
            tool.insert("description".into(), Value::String(desc.into()));
        }
        tool.insert("inputSchema".into(), config.inputs_json_schema());
        tools.push(Value::Object(tool));
    }
    json!({ "tools": tools })
}

/// Invoke an exposed pipeline. Blocks until the pipeline terminates.
///
/// Per MCP convention, pipeline failures surface inside the tool result
/// (`isError: true` with text content) rather than as a JSON-RPC error.
/// JSON-RPC errors are reserved for protocol-level problems: unknown
/// tool name, malformed arguments, not-exposed pipeline.
pub async fn handle_tools_call<F>(
    runtime: &Arc<Runtime>,
    params: &Value,
    filter: F,
) -> Result<Value, RpcError>
where
    F: Fn(&str) -> bool,
{
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::new(ERR_INVALID_PARAMS, "missing tool `name`"))?;

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));

    let (pipeline_name, config) = resolve_exposed_pipeline(runtime, name, filter)
        .ok_or_else(|| RpcError::new(ERR_METHOD_NOT_FOUND, format!("unknown tool `{name}`")))?;

    if config.mcp_exposure().map(|m| m.mode) == Some(McpExposeMode::Async) {
        // Defensive: filter excludes async in `tools/list`, but a
        // pre-fetched stale name could still hit `tools/call`.
        return Err(RpcError::new(
            ERR_METHOD_NOT_FOUND,
            format!("tool `{name}` is declared async; Slice 2 (SEP-1686) not yet shipped"),
        ));
    }

    let inputs = coerce_arguments(&arguments)
        .map_err(|e| RpcError::new(ERR_INVALID_PARAMS, e))?;

    let cmd = RunPipeline::new(pipeline_name.clone(), inputs);
    match run_pipeline::handle(runtime, cmd).await {
        Ok(pr) => Ok(tool_call_result(pr.success, pr.final_output.as_deref(), None)),
        Err(err) => Ok(tool_call_result(false, None, Some(err.as_str()))),
    }
}

fn resolve_exposed_pipeline<'r, F>(
    runtime: &'r Runtime,
    tool_name: &str,
    filter: F,
) -> Option<(String, &'r PipelineConfig)>
where
    F: Fn(&str) -> bool,
{
    runtime
        .workspace()
        .pipelines
        .iter()
        .find(|(name, config)| {
            filter(name)
                && config.mcp_exposure().is_some()
                && config.mcp_tool_name() == Some(tool_name)
        })
        .map(|(name, config)| (name.clone(), config))
}

/// Coerce MCP `arguments` (`Map<String, Value>`) into the
/// `HashMap<String, String>` that the pipeline runtime ingests today.
/// Decision recorded in [[project_install_ux_fetch]] adjacent: stringify
/// at the MCP boundary, leave runtime types untouched (Slice 1 scope).
fn coerce_arguments(arguments: &Value) -> Result<HashMap<String, String>, String> {
    let obj = match arguments {
        Value::Object(o) => o,
        Value::Null => return Ok(HashMap::new()),
        _ => return Err("`arguments` must be an object".into()),
    };
    let mut out = HashMap::with_capacity(obj.len());
    for (k, v) in obj {
        let s = match v {
            Value::Null => continue,
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::Array(_) | Value::Object(_) => serde_json::to_string(v)
                .map_err(|e| format!("failed to serialize `{k}`: {e}"))?,
        };
        out.insert(k.clone(), s);
    }
    Ok(out)
}

fn tool_call_result(success: bool, output: Option<&str>, error: Option<&str>) -> Value {
    let text = match (output, error) {
        (Some(o), _) => o.to_string(),
        (None, Some(e)) => format!("pipeline failed: {e}"),
        (None, None) => {
            if success {
                "(no output)".to_string()
            } else {
                "pipeline failed".to_string()
            }
        }
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": !success,
    })
}

pub fn unknown_method(method: &str) -> RpcError {
    RpcError::new(ERR_METHOD_NOT_FOUND, format!("unknown method `{method}`"))
}

pub fn internal_error(err: impl std::fmt::Display) -> RpcError {
    RpcError::new(ERR_INTERNAL, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_advertises_tools_capability() {
        let result = handle_initialize();
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[test]
    fn coerce_arguments_handles_scalar_types() {
        let v = json!({
            "str": "hello",
            "num": 42,
            "flag": true,
            "null_field": null,
        });
        let out = coerce_arguments(&v).unwrap();
        assert_eq!(out.get("str").map(String::as_str), Some("hello"));
        assert_eq!(out.get("num").map(String::as_str), Some("42"));
        assert_eq!(out.get("flag").map(String::as_str), Some("true"));
        assert!(!out.contains_key("null_field"));
    }

    #[test]
    fn coerce_arguments_serialises_compound_values() {
        let v = json!({
            "items": ["a", "b"],
            "obj": { "k": 1 },
        });
        let out = coerce_arguments(&v).unwrap();
        assert_eq!(out.get("items").unwrap(), r#"["a","b"]"#);
        assert_eq!(out.get("obj").unwrap(), r#"{"k":1}"#);
    }

    #[test]
    fn coerce_arguments_rejects_non_object() {
        let err = coerce_arguments(&json!("not an object")).unwrap_err();
        assert!(err.contains("must be an object"), "got: {err}");
    }

    #[test]
    fn coerce_arguments_accepts_null_and_missing() {
        assert!(coerce_arguments(&Value::Null).unwrap().is_empty());
        assert!(coerce_arguments(&json!({})).unwrap().is_empty());
    }

    #[test]
    fn tool_call_result_packs_success_payload() {
        let v = tool_call_result(true, Some("done"), None);
        assert_eq!(v["isError"], false);
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "done");
    }

    #[test]
    fn tool_call_result_packs_error_payload() {
        let v = tool_call_result(false, None, Some("boom"));
        assert_eq!(v["isError"], true);
        assert!(v["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("boom"));
    }
}
