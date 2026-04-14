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
use crate::events::bus::EventBus;
use crate::events::store::EventStore;
use crate::events::{Event, EventKind};
use crate::llm::{ChatRequest, LlmProvider};
use crate::runtime::context_window::{
    approx_chars, identify_segments, mask_observations, Segment, DEFAULT_OBSERVATION_WINDOW,
};
use crate::types::Message;

/// Configuration for context budget management.
#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// Number of recent turns kept in full (observation window).
    pub observation_window: usize,
    /// Soft cap in chars. Triggers hybrid compaction when exceeded after masking.
    pub soft_cap_chars: usize,
    /// Hard cap in chars. Returns `HardCapExceeded` if exceeded after compaction.
    pub hard_cap_chars: usize,
    /// Minimum recent turns preserved even under compaction pressure.
    pub min_tail_turns: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            observation_window: DEFAULT_OBSERVATION_WINDOW,
            soft_cap_chars: 400_000,
            hard_cap_chars: 600_000,
            min_tail_turns: 4,
        }
    }
}

impl From<crate::config::project::ContextWindowConfig> for ContextConfig {
    fn from(c: crate::config::project::ContextWindowConfig) -> Self {
        Self {
            observation_window: c.observation_window,
            soft_cap_chars: c.soft_cap_chars,
            hard_cap_chars: c.hard_cap_chars,
            min_tail_turns: c.min_tail_turns,
        }
    }
}

/// Error from context construction.
#[derive(Debug)]
pub enum ContextError {
    StoreError(crate::events::EventStoreError),
    /// Context exceeds the hard cap even after compaction.
    HardCapExceeded { total_chars: usize, hard_cap: usize },
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContextError::StoreError(e) => write!(f, "event store error: {e}"),
            ContextError::HardCapExceeded { total_chars, hard_cap } => write!(
                f,
                "context exceeds hard cap: {total_chars} chars > {hard_cap} hard cap"
            ),
        }
    }
}

/// Fixed compaction prompt (ADR-0016 §5).
const COMPACTION_PROMPT: &str = "\
You are compressing a sequence of masked observations from an autonomous \
agent's history. Each entry shows what tool was called and whether it \
succeeded. Produce a single paragraph summarizing: (a) what the agent \
accomplished in this span, (b) any errors or dead ends, (c) any decisions \
the agent committed to. Do NOT preserve individual tool call details — \
the summary replaces them entirely.";

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
    /// Optional: required for hybrid compaction. When `None`, compaction is
    /// skipped and only the hard-cap check fires.
    provider: Option<Arc<dyn LlmProvider>>,
    /// Optional: required for emitting compaction events.
    bus: Option<Arc<EventBus>>,
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
            provider: None,
            bus: None,
        }
    }

    /// Enable hybrid compaction by providing an LLM provider and event bus.
    pub fn with_compaction(
        mut self,
        provider: Arc<dyn LlmProvider>,
        bus: Arc<EventBus>,
    ) -> Self {
        self.provider = Some(provider);
        self.bus = Some(bus);
        self
    }

    /// Materialise the agent's working context.
    ///
    /// Reads events from the store, converts significant ones to messages,
    /// applies observation masking, and enforces budget caps. If the masked
    /// context exceeds `soft_cap_chars` and a provider is available, triggers
    /// hybrid compaction (LLM summarization of the oldest masked batch).
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

        let prefix_len = messages.len();

        // --- Layer C: observation tail ---
        let events = self
            .store
            .read_stream(&self.stream_id, 0)
            .await
            .map_err(ContextError::StoreError)?;

        let tail = events_to_messages(&events);
        let masked_tail = mask_observations(&tail, self.config.observation_window);
        messages.extend(masked_tail);

        // --- Soft cap: trigger compaction if needed ---
        let total_chars = approx_chars(&messages);
        if total_chars > self.config.soft_cap_chars {
            if let (Some(provider), Some(bus)) = (&self.provider, &self.bus) {
                self.try_compact(&events, &mut messages, prefix_len, provider, bus)
                    .await?;
            }
        }

        // --- Hard cap enforcement ---
        let final_chars = approx_chars(&messages);
        if final_chars > self.config.hard_cap_chars {
            return Err(ContextError::HardCapExceeded {
                total_chars: final_chars,
                hard_cap: self.config.hard_cap_chars,
            });
        }

        Ok(messages)
    }

    /// Attempt hybrid compaction: summarize the oldest masked turns via LLM.
    ///
    /// Non-fatal: if the LLM call fails, logs a warning and returns `Ok(())`
    /// so the hard-cap check in `build()` acts as the safety net.
    async fn try_compact(
        &self,
        events: &[Event],
        messages: &mut Vec<Message>,
        prefix_len: usize,
        provider: &Arc<dyn LlmProvider>,
        bus: &Arc<EventBus>,
    ) -> Result<(), ContextError> {
        let body = &messages[prefix_len..];
        if body.is_empty() {
            return Ok(());
        }

        // Count turns, preserve min_tail_turns.
        let segments = identify_segments(body);
        let turn_count = segments.iter().filter(|s| s.is_turn()).count();
        let compactable_turns = turn_count.saturating_sub(self.config.min_tail_turns);
        if compactable_turns == 0 {
            return Ok(());
        }

        // Collect messages from the oldest compactable turns.
        let mut compactable_end_idx = 0; // relative to body
        let mut turns_seen = 0;
        for seg in &segments {
            if turns_seen >= compactable_turns {
                break;
            }
            match seg {
                Segment::Turn { end, .. } => {
                    compactable_end_idx = *end;
                    turns_seen += 1;
                }
                Segment::PassThrough(idx) => {
                    compactable_end_idx = *idx + 1;
                }
            }
        }

        let compactable_messages = &body[..compactable_end_idx];
        if compactable_messages.is_empty() {
            return Ok(());
        }

        // Determine event sequence range for the compacted span.
        let tagged = events_to_messages_with_seqs(events);
        // Map: we need the seqs for the first `compactable_end_idx` messages
        // in the tail. The tail messages correspond 1:1 with tagged (before
        // masking), since masking doesn't add/remove messages.
        if tagged.len() < compactable_end_idx {
            return Ok(());
        }
        let seq_lo = tagged[0].1;
        let seq_hi = tagged[compactable_end_idx - 1].1;

        let original_chars: usize = compactable_messages.iter().map(msg_chars).sum();

        // Build the compaction input from the masked messages.
        let compaction_input = format_turns_for_compaction(compactable_messages);
        let compaction_messages = vec![
            Message::System(COMPACTION_PROMPT.to_string()),
            Message::User(compaction_input),
        ];

        let correlation_id = uuid::Uuid::new_v4();

        // Emit LlmCallStarted for the compaction call.
        emit_compaction_event(
            bus,
            &self.stream_id,
            correlation_id,
            EventKind::LlmCallStarted {
                iteration: 0,
                message_count: compaction_messages.len(),
                approx_context_chars: approx_chars(&compaction_messages),
            },
        )
        .await;

        // Call the LLM.
        let request = ChatRequest {
            messages: compaction_messages,
            tools: vec![],
            temperature: Some(0.3),
            max_tokens: Some(1024),
        };

        let response = match provider.chat_completion(&request).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("compaction LLM call failed: {e}; skipping compaction");
                return Ok(());
            }
        };

        let summary = match &response.message {
            Message::Assistant { content, .. } => content.clone().unwrap_or_default(),
            _ => String::new(),
        };

        // Emit LlmCallCompleted for the compaction call.
        emit_compaction_event(
            bus,
            &self.stream_id,
            correlation_id,
            EventKind::LlmCallCompleted {
                response_message: Some(response.message.clone()),
                has_tool_calls: false,
                usage: Some(response.usage),
                content_preview: Some(truncate_str(&summary, 100).to_string()),
            },
        )
        .await;

        let bytes_saved = original_chars as i64 - summary.len() as i64;

        // Emit ContextCompacted event.
        emit_compaction_event(
            bus,
            &self.stream_id,
            correlation_id,
            EventKind::ContextCompacted {
                replaces_seq_range: (seq_lo, seq_hi),
                summary,
                bytes_saved,
            },
        )
        .await;

        // Re-read events (now includes ContextCompacted) and rebuild the
        // tail. This is the cleanest path — events_to_messages already
        // knows how to honour ContextCompacted.
        let updated_events = self
            .store
            .read_stream(&self.stream_id, 0)
            .await
            .map_err(ContextError::StoreError)?;

        let tail = events_to_messages(&updated_events);
        let masked_tail = mask_observations(&tail, self.config.observation_window);

        messages.truncate(prefix_len);
        messages.extend(masked_tail);

        Ok(())
    }
}

/// Convert a sequence of events into conversation messages.
///
/// Honours [`EventKind::ContextCompacted`]: events whose sequence falls
/// within a compacted range are suppressed, replaced by a single
/// `Message::System` carrying the compaction summary.
///
/// Significant event kinds (per ADR-0016 §4):
/// - `LlmCallCompleted` → `Message::Assistant`
/// - `ToolCallCompleted` → `Message::ToolResult`
/// - `UserMessageReceived` → `Message::User`
/// - `ContextCompacted` → `Message::System("[compacted history]\n…")`
fn events_to_messages(events: &[crate::events::Event]) -> Vec<Message> {
    events_to_messages_with_seqs(events)
        .into_iter()
        .map(|(msg, _seq)| msg)
        .collect()
}

/// Like [`events_to_messages`] but also returns the originating event
/// sequence for each produced message. Used by compaction to determine
/// `replaces_seq_range`.
fn events_to_messages_with_seqs(events: &[crate::events::Event]) -> Vec<(Message, u64)> {
    // First pass: collect compacted ranges and their summaries.
    let mut compacted_ranges: Vec<(u64, u64)> = Vec::new();
    // (range_start, summary) — sorted by range_start for in-order emission.
    let mut compaction_summaries: Vec<(u64, String)> = Vec::new();

    for event in events {
        if let EventKind::ContextCompacted {
            replaces_seq_range,
            summary,
            ..
        } = &event.kind
        {
            compacted_ranges.push(*replaces_seq_range);
            compaction_summaries.push((replaces_seq_range.0, summary.clone()));
        }
    }
    compaction_summaries.sort_by_key(|(start, _)| *start);

    let mut result = Vec::new();
    let mut next_summary_idx = 0;

    for event in events {
        let seq = event.sequence;

        // Check if this event falls within any compacted range.
        let in_compacted = compacted_ranges
            .iter()
            .any(|&(lo, hi)| seq >= lo && seq <= hi);

        if in_compacted {
            // Emit the summary message at the start of each compacted range.
            if next_summary_idx < compaction_summaries.len()
                && compaction_summaries[next_summary_idx].0 == seq
            {
                let summary = &compaction_summaries[next_summary_idx].1;
                result.push((
                    Message::System(format!("[compacted history]\n{summary}")),
                    seq,
                ));
                next_summary_idx += 1;
            }
            continue;
        }

        // Normal event → message conversion.
        match &event.kind {
            EventKind::LlmCallCompleted {
                response_message,
                content_preview,
                ..
            } => {
                if let Some(msg) = response_message {
                    result.push((msg.clone(), seq));
                } else {
                    result.push((
                        Message::Assistant {
                            content: content_preview.clone(),
                            tool_calls: vec![],
                        },
                        seq,
                    ));
                }
            }
            EventKind::ToolCallCompleted {
                call_id,
                result: full_result,
                result_preview,
                ..
            } => {
                let content = if full_result.is_empty() {
                    result_preview.clone()
                } else {
                    full_result.clone()
                };
                result.push((
                    Message::ToolResult {
                        tool_call_id: call_id.clone(),
                        content,
                    },
                    seq,
                ));
            }
            EventKind::UserMessageReceived { content, .. } => {
                result.push((content.clone(), seq));
            }
            EventKind::ContextCompacted { .. } => {
                // Already handled in the compacted-range logic above.
                // If the ContextCompacted event itself is not inside another
                // compacted range, skip it (it's metadata, not a message).
            }
            _ => {} // Skip non-significant events.
        }
    }

    result
}

/// Character count for a single message.
fn msg_chars(m: &Message) -> usize {
    match m {
        Message::System(s) | Message::User(s) => s.len(),
        Message::Assistant { content, .. } => content.as_ref().map_or(0, |c| c.len()),
        Message::ToolResult { content, .. } => content.len(),
        Message::UserMultimodal { parts } => parts
            .iter()
            .map(|p| match p {
                crate::types::ContentPart::Text(t) => t.len(),
                _ => 0,
            })
            .sum(),
    }
}

/// Format turn messages into a text block for the compaction prompt.
fn format_turns_for_compaction(messages: &[Message]) -> String {
    let mut buf = String::new();
    for m in messages {
        match m {
            Message::Assistant { content: Some(c), .. } => {
                buf.push_str("Assistant: ");
                buf.push_str(c);
                buf.push('\n');
            }
            Message::Assistant { content: None, .. } => {}
            Message::ToolResult { content, .. } => {
                buf.push_str("Tool result: ");
                buf.push_str(content);
                buf.push('\n');
            }
            Message::System(s) => {
                buf.push_str(s);
                buf.push('\n');
            }
            Message::User(s) => {
                buf.push_str("User: ");
                buf.push_str(s);
                buf.push('\n');
            }
            _ => {}
        }
    }
    buf
}

/// Emit an event on the bus for compaction telemetry.
async fn emit_compaction_event(
    bus: &EventBus,
    stream_id: &str,
    correlation_id: uuid::Uuid,
    kind: EventKind,
) {
    let event = Event::new(stream_id.to_string(), kind, "compaction".into())
        .with_correlation(correlation_id);
    if let Err(e) = bus.publish(event).await {
        log::warn!("failed to emit compaction event: {e}");
    }
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
                ..Default::default()
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
            ContextConfig { observation_window: 0, ..Default::default() }, // mask everything
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

    // ── ContextCompacted tests ──────────────────────────────────────────

    #[tokio::test]
    async fn events_to_messages_honours_compaction() {
        let (store, bus, _memory) = setup().await;
        let stream = "compact-test";

        // Emit 3 turns.
        for i in 0..3 {
            let tc_id = format!("tc{i}");
            let a = assistant_msg(Some(&format!("step {i}")), vec![tc(&tc_id, "read", "{}")]);
            emit(
                &bus,
                stream,
                EventKind::LlmCallCompleted {
                    response_message: Some(a),
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
                    result: format!("result-{i}"),
                    result_preview: format!("result-{i}"),
                    is_error: false,
                    duration_ms: 5,
                },
            )
            .await;
        }

        // Read events to find seq range for first 2 turns (seqs 1-4).
        let events = store.read_stream(stream, 0).await.unwrap();
        let seq_lo = events[0].sequence;
        let seq_hi = events[3].sequence; // 4 events for 2 turns

        // Emit a ContextCompacted replacing the first 2 turns.
        emit(
            &bus,
            stream,
            EventKind::ContextCompacted {
                replaces_seq_range: (seq_lo, seq_hi),
                summary: "Did steps 0 and 1.".into(),
                bytes_saved: 500,
            },
        )
        .await;

        // Re-read and convert.
        let events = store.read_stream(stream, 0).await.unwrap();
        let messages = events_to_messages(&events);

        // Expected: 1 compacted summary + 1 assistant + 1 tool_result = 3
        assert_eq!(messages.len(), 3);
        match &messages[0] {
            Message::System(s) => {
                assert!(s.contains("[compacted history]"));
                assert!(s.contains("Did steps 0 and 1."));
            }
            other => panic!("expected System, got {other:?}"),
        }
        // Turn 2 survives.
        match &messages[1] {
            Message::Assistant { content, .. } => {
                assert_eq!(content.as_deref(), Some("step 2"));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multiple_compactions_in_sequence() {
        let (store, bus, _memory) = setup().await;
        let stream = "multi-compact";

        // Emit 4 turns.
        for i in 0..4 {
            let tc_id = format!("tc{i}");
            let a = assistant_msg(Some(&format!("step {i}")), vec![tc(&tc_id, "tool", "{}")]);
            emit(
                &bus,
                stream,
                EventKind::LlmCallCompleted {
                    response_message: Some(a),
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
                    result: format!("r{i}"),
                    result_preview: format!("r{i}"),
                    is_error: false,
                    duration_ms: 1,
                },
            )
            .await;
        }

        let events = store.read_stream(stream, 0).await.unwrap();

        // Compact turns 0-1 (seqs 1-4) and turn 2-3 (seqs 5-8).
        emit(
            &bus,
            stream,
            EventKind::ContextCompacted {
                replaces_seq_range: (events[0].sequence, events[3].sequence),
                summary: "First batch.".into(),
                bytes_saved: 100,
            },
        )
        .await;
        emit(
            &bus,
            stream,
            EventKind::ContextCompacted {
                replaces_seq_range: (events[4].sequence, events[7].sequence),
                summary: "Second batch.".into(),
                bytes_saved: 200,
            },
        )
        .await;

        let events = store.read_stream(stream, 0).await.unwrap();
        let messages = events_to_messages(&events);

        // 2 summary messages, no turn messages.
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::System(s) if s.contains("First batch.")));
        assert!(matches!(&messages[1], Message::System(s) if s.contains("Second batch.")));
    }

    #[tokio::test]
    async fn build_below_soft_cap_no_compaction() {
        let (store, bus, memory) = setup().await;
        let stream = "no-compact";

        // Single turn — well below any cap.
        let a = assistant_msg(Some("hello"), vec![tc("t1", "echo", "{}")]);
        emit(
            &bus,
            stream,
            EventKind::LlmCallCompleted {
                response_message: Some(a),
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
                call_id: "t1".into(),
                result: "ok".into(),
                result_preview: "ok".into(),
                is_error: false,
                duration_ms: 1,
            },
        )
        .await;

        let builder = ContextBuilder::new(
            store.clone(),
            memory,
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig::default(),
        );

        let messages = builder.build().await.unwrap();
        // sys + task + assistant + tool_result = 4
        assert_eq!(messages.len(), 4);

        // No ContextCompacted event should exist.
        let events = store.read_stream(stream, 0).await.unwrap();
        assert!(
            !events.iter().any(|e| e.kind_tag() == "context_compacted"),
            "no compaction should have been triggered"
        );
    }

    #[tokio::test]
    async fn build_hard_cap_exceeded_without_provider() {
        let (store, bus, memory) = setup().await;
        let stream = "hardcap";

        // Emit a single turn with a huge result.
        let a = assistant_msg(Some("x"), vec![tc("t1", "big", "{}")]);
        emit(
            &bus,
            stream,
            EventKind::LlmCallCompleted {
                response_message: Some(a),
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
                call_id: "t1".into(),
                result: "x".repeat(1000),
                result_preview: "x".repeat(100),
                is_error: false,
                duration_ms: 1,
            },
        )
        .await;

        // Set hard_cap very low (100 chars) so it triggers.
        let builder = ContextBuilder::new(
            store,
            memory,
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig {
                observation_window: 10,
                soft_cap_chars: 50,
                hard_cap_chars: 100,
                min_tail_turns: 0,
            },
        );
        // No with_compaction() → compaction skipped, hard cap fires.

        let result = builder.build().await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ContextError::HardCapExceeded { .. }),
            "expected HardCapExceeded, got: {err}"
        );
    }

    // ── Mock LlmProvider for compaction tests ───────────────────────────

    use crate::llm::{ChatResponse, LlmError};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct MockProvider {
        summary: String,
        call_count: AtomicUsize,
    }

    impl MockProvider {
        fn new(summary: &str) -> Self {
            Self {
                summary: summary.to_string(),
                call_count: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn chat_completion(
            &self,
            _request: &ChatRequest,
        ) -> Result<ChatResponse, LlmError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                message: Message::Assistant {
                    content: Some(self.summary.clone()),
                    tool_calls: vec![],
                },
                usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 20,
                },
                model: "mock".into(),
            })
        }
    }

    #[derive(Debug)]
    struct FailingProvider;

    #[async_trait]
    impl LlmProvider for FailingProvider {
        async fn chat_completion(
            &self,
            _request: &ChatRequest,
        ) -> Result<ChatResponse, LlmError> {
            Err(LlmError::Api { status: 500, message: "mock failure".into() })
        }
    }

    /// Helper: emit N turns of a fixed size to a stream.
    async fn emit_turns(bus: &EventBus, stream: &str, count: usize, result_size: usize) {
        for i in 0..count {
            let tc_id = format!("tc{i}");
            let a = assistant_msg(
                Some(&format!("step {i}")),
                vec![tc(&tc_id, "read_file", r#"{"path":"f.rs"}"#)],
            );
            emit(
                bus,
                stream,
                EventKind::LlmCallCompleted {
                    response_message: Some(a),
                    has_tool_calls: true,
                    usage: None,
                    content_preview: None,
                },
            )
            .await;
            emit(
                bus,
                stream,
                EventKind::ToolCallCompleted {
                    call_id: tc_id,
                    result: "x".repeat(result_size),
                    result_preview: "x".repeat(result_size.min(200)),
                    is_error: false,
                    duration_ms: 5,
                },
            )
            .await;
        }
    }

    #[tokio::test]
    async fn build_above_soft_cap_triggers_compaction() {
        let (store, bus, memory) = setup().await;
        let stream = "compact-trigger";

        // 5 turns, each with 200-char results ≈ 1000+ chars in total.
        emit_turns(&bus, stream, 5, 200).await;

        let mock = Arc::new(MockProvider::new("Compacted summary."));

        let builder = ContextBuilder::new(
            store.clone(),
            memory,
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig {
                observation_window: 2,
                soft_cap_chars: 100, // very low → forces compaction
                hard_cap_chars: 100_000,
                min_tail_turns: 2,
            },
        )
        .with_compaction(Arc::clone(&mock) as Arc<dyn LlmProvider>, bus.clone());

        let messages = builder.build().await.unwrap();

        // The mock provider should have been called.
        assert!(mock.calls() > 0, "provider should have been called");

        // The result should contain a compacted summary message.
        let has_compacted = messages
            .iter()
            .any(|m| matches!(m, Message::System(s) if s.contains("[compacted history]")));
        assert!(has_compacted, "should contain compacted summary");

        // A ContextCompacted event should be in the store.
        let events = store.read_stream(stream, 0).await.unwrap();
        let compaction_events: Vec<_> = events
            .iter()
            .filter(|e| e.kind_tag() == "context_compacted")
            .collect();
        assert_eq!(compaction_events.len(), 1);
    }

    #[tokio::test]
    async fn build_min_tail_preserved() {
        let (store, bus, memory) = setup().await;
        let stream = "min-tail";

        // 5 turns.
        emit_turns(&bus, stream, 5, 100).await;

        let mock = Arc::new(MockProvider::new("Summary."));

        let builder = ContextBuilder::new(
            store.clone(),
            memory,
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig {
                observation_window: 10,
                soft_cap_chars: 100, // forces compaction
                hard_cap_chars: 100_000,
                min_tail_turns: 3,   // preserve 3 most recent
            },
        )
        .with_compaction(Arc::clone(&mock) as Arc<dyn LlmProvider>, bus.clone());

        let messages = builder.build().await.unwrap();

        // Count surviving Assistant messages (turns).
        let assistant_count = messages
            .iter()
            .filter(|m| matches!(m, Message::Assistant { .. }))
            .count();

        // At least min_tail_turns should survive.
        assert!(
            assistant_count >= 3,
            "expected at least 3 preserved turns, got {assistant_count}"
        );
    }

    #[tokio::test]
    async fn build_compaction_failure_non_fatal() {
        let (store, bus, memory) = setup().await;
        let stream = "fail-compact";

        emit_turns(&bus, stream, 5, 200).await;

        let failing = Arc::new(FailingProvider);

        let builder = ContextBuilder::new(
            store,
            memory,
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig {
                observation_window: 2,
                soft_cap_chars: 100, // would trigger compaction
                hard_cap_chars: 100_000, // but high enough to not fail
                min_tail_turns: 2,
            },
        )
        .with_compaction(failing as Arc<dyn LlmProvider>, bus.clone());

        // Should succeed (compaction failure is non-fatal).
        let result = builder.build().await;
        assert!(result.is_ok(), "compaction failure should be non-fatal");
    }

    #[tokio::test]
    async fn replay_after_compaction() {
        let (store, bus, memory) = setup().await;
        let stream = "replay";

        emit_turns(&bus, stream, 5, 200).await;

        let mock = Arc::new(MockProvider::new("Summary of old work."));

        // First build: triggers compaction.
        let builder = ContextBuilder::new(
            store.clone(),
            memory.clone(),
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig {
                observation_window: 2,
                soft_cap_chars: 100,
                hard_cap_chars: 100_000,
                min_tail_turns: 2,
            },
        )
        .with_compaction(Arc::clone(&mock) as Arc<dyn LlmProvider>, bus.clone());

        let messages1 = builder.build().await.unwrap();
        let calls_after_first = mock.calls();
        assert!(calls_after_first > 0);

        // Second build: ContextCompacted already in the store, context
        // should be within soft_cap now → no additional compaction call.
        let builder2 = ContextBuilder::new(
            store,
            memory,
            stream.into(),
            "sys".into(),
            "task".into(),
            ContextConfig {
                observation_window: 2,
                soft_cap_chars: 100_000, // raise soft cap so it won't trigger
                hard_cap_chars: 200_000,
                min_tail_turns: 2,
            },
        )
        .with_compaction(Arc::clone(&mock) as Arc<dyn LlmProvider>, bus.clone());

        let messages2 = builder2.build().await.unwrap();

        // Both builds should produce a compacted summary.
        let has_summary = |msgs: &[Message]| {
            msgs.iter()
                .any(|m| matches!(m, Message::System(s) if s.contains("[compacted history]")))
        };
        assert!(has_summary(&messages1));
        assert!(has_summary(&messages2));

        // No additional LLM call on second build.
        assert_eq!(mock.calls(), calls_after_first);
    }
}
