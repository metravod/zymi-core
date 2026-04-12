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
            EventKind::ShellSessionStarted { .. } => "shell_session_started",
            EventKind::ShellSessionClosed { .. } => "shell_session_closed",
            EventKind::PipelineRequested { .. } => "pipeline_requested",
            EventKind::PipelineCompleted { .. } => "pipeline_completed",
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

    #[test]
    fn event_kind_serialization_roundtrip() {
        let kind = EventKind::ToolCallCompleted {
            call_id: "tc-1".into(),
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
