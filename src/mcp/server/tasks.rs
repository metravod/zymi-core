//! In-memory task registry for `zymi mcp serve` async tools/call
//! (ADR-0033 Slice 2a, SEP-1686 Tasks).
//!
//! A task is a backgrounded pipeline run, keyed by the **client-generated**
//! `taskId` (SEP-1686: the requestor owns the id, the server maps it onto an
//! internal run). The spawned wrapper writes the terminal outcome into the
//! per-task [`TaskState`] behind a `Mutex`; `tasks/get` / `tasks/result` /
//! `tasks/list` read snapshots of it. Cancellation aborts the wrapper's
//! `JoinHandle` best-effort (§4.8.2 — inner step tasks already spawned by
//! `run_pipeline` are detached and may run to completion; threading a real
//! `CancellationToken` through the engine is deferred).
//!
//! 2a does not evict on `ttl`: tasks live for the process lifetime. The
//! server is a short-lived stdio child, so unbounded growth is bounded by
//! the session.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::task::AbortHandle;

use super::protocol::{Task, TaskStatus};

/// Mutable per-task state, updated by the spawned pipeline wrapper.
#[derive(Debug)]
pub struct TaskState {
    pub status: TaskStatus,
    pub status_message: Option<String>,
    /// The `CallToolResult` payload, populated once the run terminates.
    pub result: Option<Value>,
    pub last_updated_at: DateTime<Utc>,
}

/// One registered task: immutable identity + an abort handle + shared state.
pub struct TaskHandle {
    pub task_id: String,
    pub pipeline: String,
    /// The event-store stream the backgrounded run records under
    /// (`mcp-task-<task_id>`). Lets the reasoning bridge (ADR-0042) map a
    /// parked `ReasoningRequested` back to its task.
    pub stream_id: String,
    /// JSON-RPC id of the originating `tools/call`, so a later
    /// `notifications/cancelled { requestId }` can find this task (§4.8.1).
    pub request_id: Value,
    pub ttl: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub abort: AbortHandle,
    pub state: Arc<Mutex<TaskState>>,
}

impl TaskHandle {
    /// Build a wire [`Task`] snapshot from the current state.
    pub async fn snapshot(&self) -> Task {
        let s = self.state.lock().await;
        Task {
            task_id: self.task_id.clone(),
            status: s.status,
            status_message: s.status_message.clone(),
            created_at: self.created_at.to_rfc3339(),
            last_updated_at: s.last_updated_at.to_rfc3339(),
            ttl: self.ttl,
            // A modest default poll cadence; clients SHOULD respect it.
            poll_interval: Some(1000),
        }
    }
}

/// Process-lifetime registry of backgrounded pipeline runs.
#[derive(Default)]
pub struct TaskStore {
    inner: Mutex<HashMap<String, Arc<TaskHandle>>>,
}

impl TaskStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a task with this client-generated id already exists. Used to
    /// reject duplicate `taskId`s (§4.2.3, surfaced as `-32602`).
    pub async fn contains(&self, task_id: &str) -> bool {
        self.inner.lock().await.contains_key(task_id)
    }

    pub async fn register(&self, handle: Arc<TaskHandle>) {
        self.inner
            .lock()
            .await
            .insert(handle.task_id.clone(), handle);
    }

    pub async fn get(&self, task_id: &str) -> Option<Arc<TaskHandle>> {
        self.inner.lock().await.get(task_id).cloned()
    }

    /// Find a task by the JSON-RPC id of its originating request.
    pub async fn find_by_request_id(&self, request_id: &Value) -> Option<Arc<TaskHandle>> {
        self.inner
            .lock()
            .await
            .values()
            .find(|h| &h.request_id == request_id)
            .cloned()
    }

    /// Find a task by the event-store stream its run records under. Used by
    /// the reasoning bridge (ADR-0042) to route a parked `ask:` to its task.
    pub async fn find_by_stream_id(&self, stream_id: &str) -> Option<Arc<TaskHandle>> {
        self.inner
            .lock()
            .await
            .values()
            .find(|h| h.stream_id == stream_id)
            .cloned()
    }

    /// Snapshot every task (used by `tasks/list`). Sorted by creation time
    /// for a stable order; 2a does not paginate (no `nextCursor`).
    pub async fn list(&self) -> Vec<Task> {
        let handles: Vec<Arc<TaskHandle>> = {
            let guard = self.inner.lock().await;
            let mut v: Vec<Arc<TaskHandle>> = guard.values().cloned().collect();
            v.sort_by_key(|h| h.created_at);
            v
        };
        let mut tasks = Vec::with_capacity(handles.len());
        for h in handles {
            tasks.push(h.snapshot().await);
        }
        tasks
    }
}
