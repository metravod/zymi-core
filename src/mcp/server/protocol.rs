//! JSON-RPC 2.0 wire types for the MCP server side.
//!
//! Mirrors `crate::plugin::transport`'s framing (newline-delimited JSON,
//! `jsonrpc: "2.0"`) but inverts the role: here we *receive* requests and
//! produce responses keyed by the inbound `id`, rather than issuing
//! requests and demultiplexing responses.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const JSONRPC_VERSION: &str = "2.0";

/// SEP-1686 `_meta` key a requestor uses to augment a request with a task
/// (`{ taskId, ttl? }`). The `taskId` is **client-generated** — the server
/// maps it onto an internal run.
pub const TASK_META_KEY: &str = "modelcontextprotocol.io/task";

/// SEP-1686 `_meta` key that ties every task-associated message back to its
/// task (§4.7). Present on `tasks/*` responses and `notifications/tasks/*`.
pub const RELATED_TASK_META_KEY: &str = "modelcontextprotocol.io/related-task";

/// Task lifecycle state (SEP-1686 `TaskStatus`). `submitted`/`unknown` from
/// the SEP prose are absent from the `2025-11-25` schema enum, so they are
/// absent here too — a freshly spawned pipeline starts `Working`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Working,
    InputRequired,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled)
    }
}

/// SEP-1686 `Task` (schema `2025-11-25`). Field names follow the schema
/// (`ttl`/`pollInterval`), not the SEP prose (`keepAlive`/`pollFrequency`).
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    #[serde(rename = "taskId")]
    pub task_id: String,
    pub status: TaskStatus,
    #[serde(rename = "statusMessage", skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "lastUpdatedAt")]
    pub last_updated_at: String,
    /// `null` ⇒ retained for the process lifetime (we do not evict in 2a).
    pub ttl: Option<i64>,
    #[serde(rename = "pollInterval", skip_serializing_if = "Option::is_none")]
    pub poll_interval: Option<i64>,
}

/// `{ "modelcontextprotocol.io/related-task": { "taskId": … } }` — the
/// `_meta` block every task-associated response must carry (§4.7).
pub fn related_task_meta(task_id: &str) -> Value {
    json!({ RELATED_TASK_META_KEY: { "taskId": task_id } })
}

/// A server→client JSON-RPC notification (no `id`).
#[derive(Debug)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

/// What the connected client advertised in its `initialize` `capabilities`.
/// We only track what the server side acts on; today that's elicitation
/// (the 2b-sync approval bridge). Absent ⇒ treated as unsupported.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientCaps {
    /// Client declared `capabilities.elicitation` — it can render
    /// `elicitation/create` forms. Gates the MCP elicitation approval
    /// channel (ADR-0033 2b-sync).
    pub elicitation: bool,
}

/// Parse the client's advertised capabilities from `initialize` params.
/// Presence of the `capabilities.elicitation` object is the signal; its
/// inner shape is not inspected (the 2025-11-25 schema leaves it open).
pub fn parse_client_caps(params: &Value) -> ClientCaps {
    let elicitation = params
        .get("capabilities")
        .and_then(|c| c.get("elicitation"))
        .is_some();
    ClientCaps { elicitation }
}

/// Outcome of handling a request: the JSON-RPC `result` plus any
/// notifications to emit alongside it (e.g. `notifications/tasks/created`,
/// which SEP-1686 requires right after task creation).
#[derive(Debug)]
pub struct MethodOutcome {
    pub result: Value,
    pub notifications: Vec<Notification>,
}

impl MethodOutcome {
    pub fn just(result: Value) -> Self {
        Self {
            result,
            notifications: Vec::new(),
        }
    }
}

/// Task augmentation a requestor attached to a request, if any. Pulled from
/// `params._meta["modelcontextprotocol.io/task"]`.
#[derive(Debug, Clone)]
pub struct TaskAugmentation {
    pub task_id: String,
    pub ttl: Option<i64>,
}

/// Extract a [`TaskAugmentation`] from a request's `params`. Returns `None`
/// when the request is not task-augmented (the common sync path).
pub fn task_augmentation(params: &Value) -> Option<TaskAugmentation> {
    let task = params.get("_meta")?.get(TASK_META_KEY)?;
    let task_id = task.get("taskId")?.as_str()?.to_string();
    let ttl = task.get("ttl").and_then(Value::as_i64);
    Some(TaskAugmentation { task_id, ttl })
}

// Standard JSON-RPC 2.0 error codes.
pub const ERR_PARSE: i64 = -32700;
pub const ERR_INVALID_REQUEST: i64 = -32600;
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERR_INVALID_PARAMS: i64 = -32602;
pub const ERR_INTERNAL: i64 = -32603;

/// Inbound JSON-RPC frame. `id` is optional so we can also accept
/// notifications (e.g. `notifications/initialized`).
///
/// The spec allows `id` to be a number, string, or null. We preserve the
/// original `Value` and echo it back on the response.
#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct RpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl RpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(error),
        }
    }
}
