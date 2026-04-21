pub mod bus;
pub mod connector;
pub mod store;
pub mod stream_registry;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::types::{Message, TokenUsage};

/// Universal event envelope. Every state change in the system is recorded as an Event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Unique identifier for this event.
    pub id: Uuid,
    /// Stream identifier (typically conversation_id).
    pub stream_id: String,
    /// Monotonically increasing sequence number within the stream.
    pub sequence: u64,
    /// When the event occurred.
    pub timestamp: DateTime<Utc>,
    /// What happened.
    pub kind: EventKind,
    /// Links related events across a single user request lifecycle.
    pub correlation_id: Option<Uuid>,
    /// Which event directly caused this one.
    pub causation_id: Option<Uuid>,
    /// Origin of the event: "telegram", "cli", "scheduler", "agent", "orchestrator".
    pub source: String,
}

impl Event {
    /// Create a new event with auto-generated id and current timestamp.
    pub fn new(stream_id: String, kind: EventKind, source: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            stream_id,
            sequence: 0, // assigned by EventStore on append
            timestamp: Utc::now(),
            kind,
            correlation_id: None,
            causation_id: None,
            source,
        }
    }

    pub fn with_correlation(mut self, id: Uuid) -> Self {
        self.correlation_id = Some(id);
        self
    }

    pub fn with_causation(mut self, id: Uuid) -> Self {
        self.causation_id = Some(id);
        self
    }
}

/// The kind discriminant extracted as a string for indexing/filtering.
impl Event {
    pub fn kind_tag(&self) -> &'static str {
        self.kind.tag()
    }
}

/// Domain events covering the full lifecycle of agent processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum EventKind {
    // -- Inbound --
    UserMessageReceived {
        content: Message,
        connector: String,
    },
    ScheduledTaskTriggered {
        entry_id: String,
        task: String,
    },

    // -- Agent lifecycle --
    AgentProcessingStarted {
        conversation_id: String,
    },
    AgentProcessingCompleted {
        conversation_id: String,
        success: bool,
    },
    LlmCallStarted {
        iteration: usize,
        message_count: usize,
        approx_context_chars: usize,
    },
    LlmCallCompleted {
        /// Full assistant message including content and tool_calls.
        /// `None` for pre-enrichment events (ADR-0016 §1a).
        #[serde(default)]
        response_message: Option<Message>,
        has_tool_calls: bool,
        usage: Option<TokenUsage>,
        content_preview: Option<String>,
    },

    // -- Tool lifecycle --
    ToolCallRequested {
        tool_name: String,
        arguments: String,
        call_id: String,
    },
    ApprovalRequested {
        description: String,
        approval_id: String,
    },
    ApprovalDecided {
        approval_id: String,
        approved: bool,
    },
    ToolCallCompleted {
        call_id: String,
        /// Full tool output, untruncated (ADR-0016 §1a).
        /// Empty string for pre-enrichment events — fall back to `result_preview`.
        #[serde(default)]
        result: String,
        result_preview: String,
        is_error: bool,
        duration_ms: u64,
    },

    // -- ESAA intention lifecycle --
    IntentionEmitted {
        intention_tag: String,
        intention_data: String,
    },
    IntentionEvaluated {
        intention_tag: String,
        verdict: String,
    },

    // -- Outbound --
    ResponseReady {
        conversation_id: String,
        content: String,
    },

    // -- Workflow --
    WorkflowStarted {
        user_message: String,
        node_count: usize,
    },
    WorkflowNodeStarted {
        node_id: String,
        description: String,
    },
    WorkflowNodeCompleted {
        node_id: String,
        success: bool,
    },
    WorkflowCompleted {
        success: bool,
    },

    // -- Memory lifecycle (ADR-0016 §1/§2) --
    /// Agent wrote a key-value pair to workflow memory.
    /// State event: replay rebuilds MemoryProjection from these.
    MemoryWritten {
        key: String,
        value: String,
        /// Set when overwriting a previous value; None on first write.
        previous_value_seq: Option<u64>,
    },
    /// Agent deleted a key from workflow memory.
    MemoryDeleted {
        key: String,
        previous_value_seq: u64,
    },

    // -- Context compaction (ADR-0016 §5) --
    /// The context builder compacted a range of events into a summary.
    /// State event: replay honours this to reconstruct the correct working
    /// context (events in the replaced range are suppressed).
    ContextCompacted {
        /// Inclusive event sequence range this summary replaces.
        replaces_seq_range: (u64, u64),
        /// LLM-generated summary of the compacted span.
        summary: String,
        /// Bytes saved (original − replacement). Negative means bad compaction.
        bytes_saved: i64,
    },

    // -- Shell session lifecycle (ADR-0015) --
    /// A persistent shell session was created for a stream.
    /// History-only: replay does not recreate the shell.
    ShellSessionStarted {
        stream_id: String,
        pid: u32,
        shell_path: String,
    },
    /// A persistent shell session was closed.
    ShellSessionClosed {
        stream_id: String,
        /// `"idle"` | `"workflow_end"` | `"killed"` | `"exit"`
        reason: String,
    },

    // -- Pipeline service contract (cross-process trigger/result) --
    /// External request to run a named pipeline. Published by clients
    /// (e.g. Django) and consumed by `zymi serve <pipeline>`.
    /// `correlation_id` on the envelope is used to match the response.
    PipelineRequested {
        pipeline: String,
        inputs: HashMap<String, String>,
    },
    /// Result of a pipeline run, published by `zymi serve` with the same
    /// `correlation_id` as the originating [`PipelineRequested`].
    PipelineCompleted {
        pipeline: String,
        success: bool,
        final_output: Option<String>,
        error: Option<String>,
    },

    // -- MCP server lifecycle (ADR-0023) --
    /// An MCP server subprocess finished its `initialize` handshake and
    /// its tools have been filtered + registered in the [`ToolCatalog`].
    /// History-only: projections do not rebuild the subprocess on replay.
    McpServerConnected {
        /// Logical server name from `mcp_servers: - name:` in project.yml.
        server: String,
        /// Number of tools actually registered after applying allow/deny.
        tool_count: usize,
    },
    /// An MCP server subprocess exited or was shut down.
    /// History-only.
    McpServerDisconnected {
        server: String,
        /// Free-form cause: `"shutdown"` on orderly stop, `"spawn_failed"`,
        /// `"init_failed"`, `"crash"`, etc.
        reason: String,
    },

    /// Marker emitted at the start of a forked resume run (ADR-0018).
    /// History-only: projections ignore it. The new stream copies the parent's
    /// frozen step events as if they happened on the new stream; this marker
    /// preserves the audit trail back to the originating run.
    ResumeForked {
        parent_stream_id: String,
        parent_correlation_id: Uuid,
        fork_at_step: String,
    },
}

impl EventKind {
    /// Short string tag for the event kind, used for DB indexing.
    pub fn tag(&self) -> &'static str {
        match self {
            EventKind::UserMessageReceived { .. } => "user_message_received",
            EventKind::ScheduledTaskTriggered { .. } => "scheduled_task_triggered",
            EventKind::AgentProcessingStarted { .. } => "agent_processing_started",
            EventKind::AgentProcessingCompleted { .. } => "agent_processing_completed",
            EventKind::LlmCallStarted { .. } => "llm_call_started",
            EventKind::LlmCallCompleted { .. } => "llm_call_completed",
            EventKind::ToolCallRequested { .. } => "tool_call_requested",
            EventKind::ApprovalRequested { .. } => "approval_requested",
            EventKind::ApprovalDecided { .. } => "approval_decided",
            EventKind::ToolCallCompleted { .. } => "tool_call_completed",
            EventKind::IntentionEmitted { .. } => "intention_emitted",
            EventKind::IntentionEvaluated { .. } => "intention_evaluated",
            EventKind::ResponseReady { .. } => "response_ready",
            EventKind::WorkflowStarted { .. } => "workflow_started",
            EventKind::WorkflowNodeStarted { .. } => "workflow_node_started",
            EventKind::WorkflowNodeCompleted { .. } => "workflow_node_completed",
            EventKind::WorkflowCompleted { .. } => "workflow_completed",
            EventKind::MemoryWritten { .. } => "memory_written",
            EventKind::MemoryDeleted { .. } => "memory_deleted",
            EventKind::ContextCompacted { .. } => "context_compacted",
            EventKind::ShellSessionStarted { .. } => "shell_session_started",
            EventKind::ShellSessionClosed { .. } => "shell_session_closed",
            EventKind::PipelineRequested { .. } => "pipeline_requested",
            EventKind::PipelineCompleted { .. } => "pipeline_completed",
            EventKind::McpServerConnected { .. } => "mcp_server_connected",
            EventKind::McpServerDisconnected { .. } => "mcp_server_disconnected",
            EventKind::ResumeForked { .. } => "resume_forked",
        }
    }
}

#[derive(Debug, Error)]
pub enum EventStoreError {
    #[error("connection error: {0}")]
    Connection(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolCallInfo;

    #[test]
    fn event_kind_serialization_roundtrip() {
        let kind = EventKind::ToolCallCompleted {
            call_id: "tc-1".into(),
            result: "ok".into(),
            result_preview: "ok".into(),
            is_error: false,
            duration_ms: 42,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let deserialized: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.tag(), "tool_call_completed");
    }

    #[test]
    fn event_builder() {
        let corr = Uuid::new_v4();
        let event = Event::new(
            "conv-1".into(),
            EventKind::AgentProcessingStarted {
                conversation_id: "conv-1".into(),
            },
            "agent".into(),
        )
        .with_correlation(corr);

        assert_eq!(event.stream_id, "conv-1");
        assert_eq!(event.correlation_id, Some(corr));
        assert_eq!(event.kind_tag(), "agent_processing_started");
    }

    #[test]
    fn pipeline_requested_serialization_roundtrip() {
        let mut inputs = HashMap::new();
        inputs.insert("topic".into(), "rust event sourcing".into());
        let kind = EventKind::PipelineRequested {
            pipeline: "research".into(),
            inputs,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag(), "pipeline_requested");
        if let EventKind::PipelineRequested { pipeline, inputs } = back {
            assert_eq!(pipeline, "research");
            assert_eq!(inputs.get("topic").unwrap(), "rust event sourcing");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn pipeline_completed_serialization_roundtrip() {
        let kind = EventKind::PipelineCompleted {
            pipeline: "research".into(),
            success: true,
            final_output: Some("done".into()),
            error: None,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag(), "pipeline_completed");
    }

    #[test]
    fn memory_written_serialization_roundtrip() {
        let kind = EventKind::MemoryWritten {
            key: "findings".into(),
            value: "rust is fast".into(),
            previous_value_seq: None,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag(), "memory_written");
        if let EventKind::MemoryWritten { key, value, previous_value_seq } = back {
            assert_eq!(key, "findings");
            assert_eq!(value, "rust is fast");
            assert!(previous_value_seq.is_none());
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn memory_written_overwrite_roundtrip() {
        let kind = EventKind::MemoryWritten {
            key: "findings".into(),
            value: "updated".into(),
            previous_value_seq: Some(42),
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        if let EventKind::MemoryWritten { previous_value_seq, .. } = back {
            assert_eq!(previous_value_seq, Some(42));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn memory_deleted_serialization_roundtrip() {
        let kind = EventKind::MemoryDeleted {
            key: "old_key".into(),
            previous_value_seq: 7,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag(), "memory_deleted");
        if let EventKind::MemoryDeleted { key, previous_value_seq } = back {
            assert_eq!(key, "old_key");
            assert_eq!(previous_value_seq, 7);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn context_compacted_serialization_roundtrip() {
        let kind = EventKind::ContextCompacted {
            replaces_seq_range: (5, 20),
            summary: "Agent read 3 files and fixed a bug.".into(),
            bytes_saved: 12345,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag(), "context_compacted");
        if let EventKind::ContextCompacted { replaces_seq_range, summary, bytes_saved } = back {
            assert_eq!(replaces_seq_range, (5, 20));
            assert_eq!(summary, "Agent read 3 files and fixed a bug.");
            assert_eq!(bytes_saved, 12345);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn context_compacted_negative_bytes_saved() {
        let kind = EventKind::ContextCompacted {
            replaces_seq_range: (1, 3),
            summary: "A very verbose summary that is longer than the original.".into(),
            bytes_saved: -500,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        if let EventKind::ContextCompacted { bytes_saved, .. } = back {
            assert_eq!(bytes_saved, -500);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn shell_session_events_serialization_roundtrip() {
        let started = EventKind::ShellSessionStarted {
            stream_id: "s-1".into(),
            pid: 12345,
            shell_path: "bash".into(),
        };
        let json = serde_json::to_string(&started).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag(), "shell_session_started");

        let closed = EventKind::ShellSessionClosed {
            stream_id: "s-1".into(),
            reason: "workflow_end".into(),
        };
        let json = serde_json::to_string(&closed).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag(), "shell_session_closed");
    }

    #[test]
    fn enriched_tool_call_completed_roundtrip() {
        let kind = EventKind::ToolCallCompleted {
            call_id: "tc-1".into(),
            result: "full output here".into(),
            result_preview: "full output h…".into(),
            is_error: false,
            duration_ms: 42,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        if let EventKind::ToolCallCompleted { result, result_preview, .. } = back {
            assert_eq!(result, "full output here");
            assert_eq!(result_preview, "full output h…");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn enriched_llm_call_completed_roundtrip() {
        let msg = Message::Assistant {
            content: Some("I'll help.".into()),
            tool_calls: vec![ToolCallInfo {
                id: "tc-1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            }],
        };
        let kind = EventKind::LlmCallCompleted {
            response_message: Some(msg),
            has_tool_calls: true,
            usage: None,
            content_preview: Some("I'll help.".into()),
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        if let EventKind::LlmCallCompleted { response_message, has_tool_calls, .. } = back {
            assert!(has_tool_calls);
            let rm = response_message.unwrap();
            match rm {
                Message::Assistant { content, tool_calls } => {
                    assert_eq!(content.as_deref(), Some("I'll help."));
                    assert_eq!(tool_calls.len(), 1);
                    assert_eq!(tool_calls[0].name, "read_file");
                }
                _ => panic!("expected Assistant"),
            }
        } else {
            panic!("wrong variant");
        }
    }

    /// Pre-enrichment events (stored before ADR-0016 §1a) deserialize
    /// with default values for the new fields.
    #[test]
    fn pre_enrichment_backward_compat() {
        // Simulate a pre-enrichment ToolCallCompleted (no `result` field).
        // EventKind uses adjacently-tagged serde: {"type":"..","data":{..}}
        let json = r#"{"type":"ToolCallCompleted","data":{"call_id":"tc-old","result_preview":"ok","is_error":false,"duration_ms":10}}"#;
        let kind: EventKind = serde_json::from_str(json).unwrap();
        if let EventKind::ToolCallCompleted { call_id, result, result_preview, .. } = kind {
            assert_eq!(call_id, "tc-old");
            assert!(result.is_empty(), "result should default to empty string");
            assert_eq!(result_preview, "ok");
        } else {
            panic!("wrong variant");
        }

        // Simulate a pre-enrichment LlmCallCompleted (no `response_message` field).
        let json = r#"{"type":"LlmCallCompleted","data":{"has_tool_calls":true,"usage":null,"content_preview":"hi"}}"#;
        let kind: EventKind = serde_json::from_str(json).unwrap();
        if let EventKind::LlmCallCompleted { response_message, has_tool_calls, .. } = kind {
            assert!(response_message.is_none(), "response_message should default to None");
            assert!(has_tool_calls);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn mcp_server_events_serialization_roundtrip() {
        let kind = EventKind::McpServerConnected {
            server: "github".into(),
            tool_count: 3,
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag(), "mcp_server_connected");
        if let EventKind::McpServerConnected { server, tool_count } = back {
            assert_eq!(server, "github");
            assert_eq!(tool_count, 3);
        } else {
            panic!("wrong variant");
        }

        let kind = EventKind::McpServerDisconnected {
            server: "github".into(),
            reason: "shutdown".into(),
        };
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tag(), "mcp_server_disconnected");
        if let EventKind::McpServerDisconnected { server, reason } = back {
            assert_eq!(server, "github");
            assert_eq!(reason, "shutdown");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn event_full_serialization_roundtrip() {
        let event = Event::new(
            "stream-1".into(),
            EventKind::UserMessageReceived {
                content: Message::User("hello".into()),
                connector: "telegram".into(),
            },
            "telegram".into(),
        );
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, event.id);
        assert_eq!(deserialized.stream_id, "stream-1");
        assert_eq!(deserialized.kind_tag(), "user_message_received");
    }
}
