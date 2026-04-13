use std::collections::HashMap;

use crate::types::Message;
use crate::events::{Event, EventKind};

/// A projection rebuilds state from an event stream.
/// Projections are pure functions of the event history — calling apply()
/// with the same events in the same order always produces the same state.
pub trait Projection: Send + Sync {
    /// Apply a single event to update the projection's state.
    fn apply(&mut self, event: &Event);
}

/// Rebuilds a conversation's message list from events.
/// This projection can be compared against direct ConversationStorage reads
/// to verify that the event log and message store are consistent.
pub struct ConversationProjection {
    pub conversation_id: String,
    pub messages: Vec<Message>,
}

impl ConversationProjection {
    pub fn new(conversation_id: &str) -> Self {
        Self {
            conversation_id: conversation_id.to_string(),
            messages: Vec::new(),
        }
    }
}

impl Projection for ConversationProjection {
    fn apply(&mut self, event: &Event) {
        if event.stream_id != self.conversation_id {
            return;
        }

        match &event.kind {
            EventKind::UserMessageReceived { content, .. } => {
                self.messages.push(content.clone());
            }
            EventKind::ResponseReady { content, .. } => {
                self.messages.push(Message::Assistant {
                    content: Some(content.clone()),
                    tool_calls: vec![],
                });
            }
            _ => {}
        }
    }
}

/// Tracks aggregate metrics from events: LLM call count, tool call count,
/// total token usage, error count.
pub struct MetricsProjection {
    pub llm_calls: u64,
    pub tool_calls: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub errors: u64,
}

impl MetricsProjection {
    pub fn new() -> Self {
        Self {
            llm_calls: 0,
            tool_calls: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            errors: 0,
        }
    }
}

impl Default for MetricsProjection {
    fn default() -> Self {
        Self::new()
    }
}

impl Projection for MetricsProjection {
    fn apply(&mut self, event: &Event) {
        match &event.kind {
            EventKind::LlmCallCompleted { usage, .. } => {
                self.llm_calls += 1;
                if let Some(u) = usage {
                    self.total_input_tokens += u.input_tokens as u64;
                    self.total_output_tokens += u.output_tokens as u64;
                }
            }
            EventKind::ToolCallCompleted { is_error, .. } => {
                self.tool_calls += 1;
                if *is_error {
                    self.errors += 1;
                }
            }
            _ => {}
        }
    }
}

/// A single entry in the agent's workflow memory.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub value: String,
    pub written_at_seq: u64,
}

/// Rebuilds workflow memory state from `MemoryWritten` / `MemoryDeleted` events.
///
/// This is the **only** readable surface for workflow memory (ADR-0016 §2).
/// The legacy `Arc<Mutex<HashMap<String, String>>>` is replaced by this projection.
pub struct MemoryProjection {
    state: HashMap<String, MemoryEntry>,
}

impl MemoryProjection {
    pub fn new() -> Self {
        Self {
            state: HashMap::new(),
        }
    }

    /// Read a single memory key. Returns `None` if the key was never written
    /// or has been deleted.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.state.get(key).map(|e| e.value.as_str())
    }

    /// Snapshot the full memory state as a simple key-value map.
    /// Used by the future ContextBuilder (ADR-0016 §4, Layer B) to render
    /// the memory snapshot into the agent's context.
    pub fn snapshot(&self) -> HashMap<String, String> {
        self.state
            .iter()
            .map(|(k, e)| (k.clone(), e.value.clone()))
            .collect()
    }

    /// Returns the sequence number at which a key was last written,
    /// or `None` if the key doesn't exist. Used to populate
    /// `MemoryWritten.previous_value_seq` on overwrites.
    pub fn seq_for(&self, key: &str) -> Option<u64> {
        self.state.get(key).map(|e| e.written_at_seq)
    }

    /// Returns `true` if the projection contains no entries.
    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }
}

impl Default for MemoryProjection {
    fn default() -> Self {
        Self::new()
    }
}

impl Projection for MemoryProjection {
    fn apply(&mut self, event: &Event) {
        match &event.kind {
            EventKind::MemoryWritten { key, value, .. } => {
                self.state.insert(
                    key.clone(),
                    MemoryEntry {
                        value: value.clone(),
                        written_at_seq: event.sequence,
                    },
                );
            }
            EventKind::MemoryDeleted { key, .. } => {
                self.state.remove(key);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TokenUsage;

    fn make_event(stream: &str, kind: EventKind) -> Event {
        Event::new(stream.into(), kind, "test".into())
    }

    #[test]
    fn conversation_projection_collects_messages() {
        let mut proj = ConversationProjection::new("conv-1");

        proj.apply(&make_event(
            "conv-1",
            EventKind::UserMessageReceived {
                content: Message::User("hello".into()),
                connector: "test".into(),
            },
        ));

        proj.apply(&make_event(
            "conv-1",
            EventKind::LlmCallCompleted {
                response_message: None,
                has_tool_calls: false,
                usage: None,
                content_preview: None,
            },
        ));

        proj.apply(&make_event(
            "conv-1",
            EventKind::ResponseReady {
                conversation_id: "conv-1".into(),
                content: "hi there".into(),
            },
        ));

        // Different stream — should be ignored
        proj.apply(&make_event(
            "conv-2",
            EventKind::UserMessageReceived {
                content: Message::User("wrong stream".into()),
                connector: "test".into(),
            },
        ));

        assert_eq!(proj.messages.len(), 2);
        match &proj.messages[0] {
            Message::User(text) => assert_eq!(text, "hello"),
            _ => panic!("Expected User message"),
        }
        match &proj.messages[1] {
            Message::Assistant { content, .. } => {
                assert_eq!(content.as_deref(), Some("hi there"));
            }
            _ => panic!("Expected Assistant message"),
        }
    }

    #[test]
    fn metrics_projection_aggregates() {
        let mut proj = MetricsProjection::new();

        proj.apply(&make_event(
            "s1",
            EventKind::LlmCallCompleted {
                response_message: None,
                has_tool_calls: true,
                usage: Some(TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                }),
                content_preview: None,
            },
        ));

        proj.apply(&make_event(
            "s1",
            EventKind::ToolCallCompleted {
                call_id: "tc-1".into(),
                result: "ok".into(),
                result_preview: "ok".into(),
                is_error: false,
                duration_ms: 42,
            },
        ));

        proj.apply(&make_event(
            "s1",
            EventKind::ToolCallCompleted {
                call_id: "tc-2".into(),
                result: "Tool error: fail".into(),
                result_preview: "Tool error: fail".into(),
                is_error: true,
                duration_ms: 10,
            },
        ));

        proj.apply(&make_event(
            "s1",
            EventKind::LlmCallCompleted {
                response_message: None,
                has_tool_calls: false,
                usage: Some(TokenUsage {
                    input_tokens: 200,
                    output_tokens: 100,
                }),
                content_preview: Some("hello world".into()),
            },
        ));

        assert_eq!(proj.llm_calls, 2);
        assert_eq!(proj.tool_calls, 2);
        assert_eq!(proj.total_input_tokens, 300);
        assert_eq!(proj.total_output_tokens, 150);
        assert_eq!(proj.errors, 1);
    }

    #[test]
    fn memory_projection_write_and_read() {
        let mut proj = MemoryProjection::new();
        assert!(proj.is_empty());

        let mut event = make_event(
            "s1",
            EventKind::MemoryWritten {
                key: "findings".into(),
                value: "rust is fast".into(),
                previous_value_seq: None,
            },
        );
        event.sequence = 1;
        proj.apply(&event);

        assert!(!proj.is_empty());
        assert_eq!(proj.get("findings"), Some("rust is fast"));
        assert_eq!(proj.seq_for("findings"), Some(1));
        assert_eq!(proj.get("nonexistent"), None);
    }

    #[test]
    fn memory_projection_overwrite() {
        let mut proj = MemoryProjection::new();

        let mut e1 = make_event(
            "s1",
            EventKind::MemoryWritten {
                key: "k".into(),
                value: "v1".into(),
                previous_value_seq: None,
            },
        );
        e1.sequence = 1;
        proj.apply(&e1);

        let mut e2 = make_event(
            "s1",
            EventKind::MemoryWritten {
                key: "k".into(),
                value: "v2".into(),
                previous_value_seq: Some(1),
            },
        );
        e2.sequence = 5;
        proj.apply(&e2);

        assert_eq!(proj.get("k"), Some("v2"));
        assert_eq!(proj.seq_for("k"), Some(5));
    }

    #[test]
    fn memory_projection_delete() {
        let mut proj = MemoryProjection::new();

        let mut e1 = make_event(
            "s1",
            EventKind::MemoryWritten {
                key: "k".into(),
                value: "v".into(),
                previous_value_seq: None,
            },
        );
        e1.sequence = 1;
        proj.apply(&e1);

        let mut e2 = make_event(
            "s1",
            EventKind::MemoryDeleted {
                key: "k".into(),
                previous_value_seq: 1,
            },
        );
        e2.sequence = 2;
        proj.apply(&e2);

        assert_eq!(proj.get("k"), None);
        assert!(proj.is_empty());
    }

    #[test]
    fn memory_projection_snapshot() {
        let mut proj = MemoryProjection::new();

        for (i, (k, v)) in [("a", "1"), ("b", "2"), ("c", "3")].iter().enumerate() {
            let mut e = make_event(
                "s1",
                EventKind::MemoryWritten {
                    key: k.to_string(),
                    value: v.to_string(),
                    previous_value_seq: None,
                },
            );
            e.sequence = i as u64 + 1;
            proj.apply(&e);
        }

        let snap = proj.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap.get("a").unwrap(), "1");
        assert_eq!(snap.get("b").unwrap(), "2");
        assert_eq!(snap.get("c").unwrap(), "3");
    }

    #[test]
    fn memory_projection_ignores_unrelated_events() {
        let mut proj = MemoryProjection::new();
        proj.apply(&make_event(
            "s1",
            EventKind::LlmCallCompleted {
                response_message: None,
                has_tool_calls: false,
                usage: None,
                content_preview: None,
            },
        ));
        assert!(proj.is_empty());
    }

    #[test]
    fn replay_from_events() {
        let events = vec![
            make_event(
                "conv-1",
                EventKind::UserMessageReceived {
                    content: Message::User("What is 2+2?".into()),
                    connector: "telegram".into(),
                },
            ),
            make_event(
                "conv-1",
                EventKind::AgentProcessingStarted {
                    conversation_id: "conv-1".into(),
                },
            ),
            make_event(
                "conv-1",
                EventKind::LlmCallCompleted {
                    response_message: None,
                    has_tool_calls: false,
                    usage: Some(TokenUsage {
                        input_tokens: 50,
                        output_tokens: 10,
                    }),
                    content_preview: None,
                },
            ),
            make_event(
                "conv-1",
                EventKind::ResponseReady {
                    conversation_id: "conv-1".into(),
                    content: "2+2 = 4".into(),
                },
            ),
        ];

        let mut conv_proj = ConversationProjection::new("conv-1");
        let mut metrics_proj = MetricsProjection::new();

        for event in &events {
            conv_proj.apply(event);
            metrics_proj.apply(event);
        }

        assert_eq!(conv_proj.messages.len(), 2);
        assert_eq!(metrics_proj.llm_calls, 1);
        assert_eq!(metrics_proj.total_input_tokens, 50);
    }
}
