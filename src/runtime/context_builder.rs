//! Layer-based context construction from the event store (ADR-0016 §4).
//!
//! [`ContextBuilder`] materialises the agent's working context as a *view*
//! over the event store, with three layers:
//!
//! - **Layer A (prefix):** system prompt + user task — deterministic, never
//!   masked, never compacted.
//! - **Layer B (memory snapshot):** current state from [`MemoryBridge`],
//!   formatted as a compact system message.
//! - **Layer C (observation tail):** conversation turns reconstructed from
//!   enriched events, with observation masking applied.

use std::sync::Arc;

use crate::engine::tools::MemoryStore;
use crate::events::store::EventStore;
use crate::events::EventKind;
use crate::runtime::context_window::{mask_observations, DEFAULT_OBSERVATION_WINDOW};
use crate::types::Message;

/// Configuration for context budget management.
#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// Number of recent turns kept in full (observation window).
    pub observation_window: usize,
    // soft_cap_chars, hard_cap_chars, min_tail_turns — slice 4.
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            observation_window: DEFAULT_OBSERVATION_WINDOW,
        }
    }
}

/// Error from context construction.
#[derive(Debug)]
pub enum ContextError {
    StoreError(crate::events::EventStoreError),
    // HardCapExceeded — slice 4.
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContextError::StoreError(e) => write!(f, "event store error: {e}"),
        }
    }
}

/// Builds the agent's working context from the event store.
///
/// Call [`build()`](ContextBuilder::build) at the top of each agent iteration
/// to get a fresh `Vec<Message>` that reflects the latest events.
pub struct ContextBuilder {
    store: Arc<dyn EventStore>,
    memory: MemoryStore,
    stream_id: String,
    system_prompt: String,
    task: String,
    config: ContextConfig,
}

impl ContextBuilder {
    pub fn new(
        store: Arc<dyn EventStore>,
        memory: MemoryStore,
        stream_id: String,
        system_prompt: String,
        task: String,
        config: ContextConfig,
    ) -> Self {
        Self {
            store,
            memory,
            stream_id,
            system_prompt,
            task,
            config,
        }
    }

    /// Materialise the agent's working context.
    ///
    /// Reads events from the store, converts significant ones to messages,
    /// applies observation masking, and prepends the stable prefix and
    /// memory snapshot.
    pub async fn build(&self) -> Result<Vec<Message>, ContextError> {
        let mut messages = Vec::new();

        // --- Layer A: stable prefix ---
        messages.push(Message::System(self.system_prompt.clone()));
        if !self.task.is_empty() {
            messages.push(Message::User(self.task.clone()));
        }

        // --- Layer B: memory snapshot ---
        if !self.memory.is_empty().await {
            let snapshot = self.memory.snapshot().await;
            let mut mem_text = String::from("[memory]\n");
            for (key, value) in &snapshot {
                let preview = truncate_str(value, 200);
                mem_text.push_str(&format!("{key}: {preview}\n"));
            }
            messages.push(Message::System(mem_text));
        }

        // --- Layer C: observation tail ---
        let events = self
            .store
            .read_stream(&self.stream_id, 0)
            .await
            .map_err(ContextError::StoreError)?;

        let tail = events_to_messages(&events);

        // Apply observation masking to the tail only.
        // We wrap with dummy prefix markers so mask_observations()
        // doesn't treat tail messages as prefix.
        let masked_tail = mask_observations(&tail, self.config.observation_window);
        messages.extend(masked_tail);

        Ok(messages)
    }
}

/// Convert a sequence of events into conversation messages.
///
/// Significant event kinds (per ADR-0016 §4):
/// - `LlmCallCompleted` → `Message::Assistant` (from `response_message`, or
///   fallback to `content_preview` for pre-enrichment events)
/// - `ToolCallCompleted` → `Message::ToolResult` (from full `result`, or
///   fallback to `result_preview` for pre-enrichment events)
/// - `UserMessageReceived` → `Message::User`
///
/// All other event kinds are skipped (noise for the model).
fn events_to_messages(events: &[crate::events::Event]) -> Vec<Message> {
    let mut messages = Vec::new();

    for event in events {
        match &event.kind {
            EventKind::LlmCallCompleted {
                response_message,
                content_preview,
                ..
            } => {
                if let Some(msg) = response_message {
                    // Enriched event: use full response message.
                    messages.push(msg.clone());
                } else {
                    // Pre-enrichment fallback: reconstruct a minimal Assistant.
                    messages.push(Message::Assistant {
                        content: content_preview.clone(),
                        tool_calls: vec![],
                    });
                }
            }
            EventKind::ToolCallCompleted {
                call_id,
                result,
                result_preview,
                ..
            } => {
                // Use full result if available, fall back to preview.
                let content = if result.is_empty() {
                    result_preview.clone()
                } else {
                    result.clone()
                };
                messages.push(Message::ToolResult {
                    tool_call_id: call_id.clone(),
                    content,
                });
            }
            EventKind::UserMessageReceived { content, .. } => {
                messages.push(content.clone());
            }
            _ => {} // Skip non-significant events.
        }
    }

    messages
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let end = s.floor_char_boundary(max);
        &s[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::bus::EventBus;
    use crate::events::store::{open_store, StoreBackend};
    use crate::events::{Event, EventKind};
    use crate::engine::tools::new_memory_store;
    use crate::types::{TokenUsage, ToolCallInfo};

    async fn setup() -> (Arc<dyn EventStore>, Arc<EventBus>, MemoryStore) {
        let store: Arc<dyn EventStore> =
            open_store(StoreBackend::Sqlite { path: ":memory:".into() }).unwrap();
        let bus = Arc::new(EventBus::new(Arc::clone(&store)));
        let memory = new_memory_store(Arc::clone(&bus));
        (store, bus, memory)
    }

    async fn emit(bus: &EventBus, stream_id: &str, kind: EventKind) {
        let event = Event::new(stream_id.into(), kind, "test".into());
        bus.publish(event).await.unwrap();
    }

    fn assistant_msg(content: Option<&str>, tool_calls: Vec<ToolCallInfo>) -> Message {
        Message::Assistant {
            content: content.map(|s| s.to_string()),
            tool_calls,
        }
    }

    fn tc(id: &str, name: &str, args: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn build_prefix_only() {
        let (store, _bus, memory) = setup().await;
        let builder = ContextBuilder::new(
            store,
            memory,
            "test-stream".into(),
            "You are an agent.".into(),
            "Do the thing.".into(),
            ContextConfig::default(),
        );

        let messages = builder.build().await.unwrap();
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::System(s) if s == "You are an agent."));
        assert!(matches!(&messages[1], Message::User(s) if s == "Do the thing."));
    }

    #[tokio::test]
    async fn build_includes_memory_snapshot() {
        let (store, bus, memory) = setup().await;
        memory
            .write("key1", "value1", "test-stream", uuid::Uuid::new_v4())
            .await
            .unwrap();

        let builder = ContextBuilder::new(
            store,
            memory,
            "test-stream".into(),
            "sys".into(),
            "task".into(),
            ContextConfig::default(),
        );

        let messages = builder.build().await.unwrap();
        // System + User + Memory snapshot + (MemoryWritten events are skipped)
        assert_eq!(messages.len(), 3);
        match &messages[2] {
            Message::System(s) => {
                assert!(s.starts_with("[memory]"));
                assert!(s.contains("key1: value1"));
            }
            _ => panic!("expected System message for memory snapshot"),
        }

        // Memory events should NOT appear in the tail.
        let _ = bus; // keep bus alive for the test
    }

    #[tokio::test]
    async fn build_reconstructs_conversation_from_events() {
        let (store, bus, memory) = setup().await;
        let stream = "agent-step-1";

        // Simulate one LLM iteration with a tool call.
        let assistant = assistant_msg(
            Some("I'll read the file."),
            vec![tc("tc1", "read_file", r#"{"path":"a.rs"}"#)],
        );
        emit(
            &bus,
            stream,
            EventKind::LlmCallCompleted {
                response_message: Some(assistant),
                has_tool_calls: true,
                usage: Some(TokenUsage { input_tokens: 100, output_tokens: 50 }),
                content_preview: Some("I'll read the file.".into()),
            },
        )
        .await;

        emit(
            &bus,
            stream,
            EventKind::ToolCallCompleted {
                call_id: "tc1".into(),
                result: "fn main() { println!(\"hello\"); }".into(),
                result_preview: "fn main() { println…".into(),
                is_error: false,
                duration_ms: 10,
            },
        )
        .await;

        let builder = ContextBuilder::new(
            store,
            memory,
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig::default(),
        );

        let messages = builder.build().await.unwrap();
        // System + User + Assistant + ToolResult = 4
        assert_eq!(messages.len(), 4);

        // Assistant message from enriched event.
        match &messages[2] {
            Message::Assistant { content, tool_calls } => {
                assert_eq!(content.as_deref(), Some("I'll read the file."));
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "read_file");
            }
            _ => panic!("expected Assistant"),
        }

        // ToolResult uses full result (not preview).
        match &messages[3] {
            Message::ToolResult { tool_call_id, content } => {
                assert_eq!(tool_call_id, "tc1");
                assert!(content.contains("fn main()"));
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[tokio::test]
    async fn build_falls_back_for_pre_enrichment_events() {
        let (store, bus, memory) = setup().await;
        let stream = "old-stream";

        // Pre-enrichment LlmCallCompleted (no response_message).
        emit(
            &bus,
            stream,
            EventKind::LlmCallCompleted {
                response_message: None,
                has_tool_calls: true,
                usage: None,
                content_preview: Some("Let me check.".into()),
            },
        )
        .await;

        // Pre-enrichment ToolCallCompleted (empty result).
        emit(
            &bus,
            stream,
            EventKind::ToolCallCompleted {
                call_id: "tc-old".into(),
                result: String::new(),
                result_preview: "ok".into(),
                is_error: false,
                duration_ms: 5,
            },
        )
        .await;

        let builder = ContextBuilder::new(
            store,
            memory,
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig::default(),
        );

        let messages = builder.build().await.unwrap();
        assert_eq!(messages.len(), 4); // sys + task + assistant + tool_result

        // Fallback assistant: content from content_preview, no tool_calls.
        match &messages[2] {
            Message::Assistant { content, tool_calls } => {
                assert_eq!(content.as_deref(), Some("Let me check."));
                assert!(tool_calls.is_empty());
            }
            _ => panic!("expected Assistant"),
        }

        // Fallback tool result: uses result_preview.
        match &messages[3] {
            Message::ToolResult { content, .. } => {
                assert_eq!(content, "ok");
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[tokio::test]
    async fn build_applies_observation_masking() {
        let (store, bus, memory) = setup().await;
        let stream = "masking-test";

        // Emit 3 turns so we can mask with window=1.
        for i in 0..3 {
            let tc_id = format!("tc{i}");
            let assistant = assistant_msg(
                Some(&format!("step {i}")),
                vec![tc(&tc_id, "read_file", r#"{"path":"big.rs"}"#)],
            );
            emit(
                &bus,
                stream,
                EventKind::LlmCallCompleted {
                    response_message: Some(assistant),
                    has_tool_calls: true,
                    usage: None,
                    content_preview: None,
                },
            )
            .await;

            emit(
                &bus,
                stream,
                EventKind::ToolCallCompleted {
                    call_id: tc_id,
                    result: "x".repeat(5000),
                    result_preview: "x".repeat(200),
                    is_error: false,
                    duration_ms: 10,
                },
            )
            .await;
        }

        let builder = ContextBuilder::new(
            store,
            memory,
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig {
                observation_window: 1,
            },
        );

        let messages = builder.build().await.unwrap();

        // Prefix (2) + 3 turns × 2 messages = 8 total.
        assert_eq!(messages.len(), 8);

        // First two turns should be masked (placeholders).
        match &messages[3] {
            // First turn's ToolResult at index 3 (sys + user + assistant + here).
            Message::ToolResult { content, .. } => {
                assert!(
                    content.starts_with("[masked:"),
                    "expected masked placeholder, got: {content}"
                );
            }
            _ => panic!("expected ToolResult"),
        }

        // Last turn should be in full.
        match &messages[7] {
            Message::ToolResult { content, .. } => {
                assert_eq!(content.len(), 5000, "last turn should be unmasked");
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[tokio::test]
    async fn tool_use_tool_result_pairs_preserved() {
        let (store, bus, memory) = setup().await;
        let stream = "pair-test";

        // Multi-tool turn.
        let assistant = assistant_msg(
            None,
            vec![
                tc("a", "read_file", r#"{"path":"x.rs"}"#),
                tc("b", "read_file", r#"{"path":"y.rs"}"#),
            ],
        );
        emit(
            &bus,
            stream,
            EventKind::LlmCallCompleted {
                response_message: Some(assistant),
                has_tool_calls: true,
                usage: None,
                content_preview: None,
            },
        )
        .await;

        for id in ["a", "b"] {
            emit(
                &bus,
                stream,
                EventKind::ToolCallCompleted {
                    call_id: id.into(),
                    result: format!("content of {id}"),
                    result_preview: format!("content of {id}"),
                    is_error: false,
                    duration_ms: 5,
                },
            )
            .await;
        }

        let builder = ContextBuilder::new(
            store,
            memory,
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig { observation_window: 0 }, // mask everything
        );

        let messages = builder.build().await.unwrap();

        // Find the Assistant message and verify its ToolResults follow.
        for (i, msg) in messages.iter().enumerate() {
            if let Message::Assistant { tool_calls, .. } = msg {
                for (j, tc) in tool_calls.iter().enumerate() {
                    let next = &messages[i + 1 + j];
                    match next {
                        Message::ToolResult { tool_call_id, .. } => {
                            assert_eq!(tool_call_id, &tc.id);
                        }
                        _ => panic!("expected ToolResult after Assistant, got {next:?}"),
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn noise_events_excluded_from_tail() {
        let (store, bus, memory) = setup().await;
        let stream = "noise-test";

        // Emit noise events that should be skipped.
        emit(
            &bus,
            stream,
            EventKind::LlmCallStarted {
                iteration: 1,
                message_count: 2,
                approx_context_chars: 100,
            },
        )
        .await;

        emit(
            &bus,
            stream,
            EventKind::IntentionEmitted {
                intention_tag: "test".into(),
                intention_data: "{}".into(),
            },
        )
        .await;

        emit(
            &bus,
            stream,
            EventKind::WorkflowNodeStarted {
                node_id: "step1".into(),
                description: "running".into(),
            },
        )
        .await;

        let builder = ContextBuilder::new(
            store,
            memory,
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig::default(),
        );

        let messages = builder.build().await.unwrap();
        // Only prefix: sys + task. All noise events excluded.
        assert_eq!(messages.len(), 2);
    }
}
