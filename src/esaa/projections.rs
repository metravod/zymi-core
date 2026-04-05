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
                result_preview: "ok".into(),
                is_error: false,
                duration_ms: 42,
            },
        ));

        proj.apply(&make_event(
            "s1",
            EventKind::ToolCallCompleted {
                call_id: "tc-2".into(),
                result_preview: "Tool error: fail".into(),
                is_error: true,
                duration_ms: 10,
            },
        ));

        proj.apply(&make_event(
            "s1",
            EventKind::LlmCallCompleted {
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
