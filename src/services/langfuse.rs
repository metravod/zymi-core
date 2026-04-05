use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::LangfuseConfig;
use crate::events::{Event, EventKind};

use super::{EventService, ServiceError};

/// LangFuse observability service.
///
/// Subscribes to the event bus and maps agent lifecycle events to LangFuse
/// traces, generations, and spans. Buffers items and flushes in batches
/// via the LangFuse ingestion API.
pub struct LangfuseService {
    config: LangfuseConfig,
    client: reqwest::Client,
    state: Mutex<LangfuseState>,
}

struct LangfuseState {
    /// Pending batch items.
    batch: Vec<LangfuseBatchItem>,
    /// correlation_id → LangFuse trace_id.
    trace_map: HashMap<Uuid, String>,
    /// correlation_id → current active generation_id (LLM calls are sequential per correlation).
    active_generation: HashMap<Uuid, String>,
    /// tool call_id → LangFuse span_id.
    span_map: HashMap<String, String>,
    /// Last flush timestamp.
    last_flush: Instant,
}

impl LangfuseState {
    fn new() -> Self {
        Self {
            batch: Vec::new(),
            trace_map: HashMap::new(),
            active_generation: HashMap::new(),
            span_map: HashMap::new(),
            last_flush: Instant::now(),
        }
    }
}

// -- LangFuse wire types (private) --

#[derive(Serialize)]
struct IngestionRequest {
    batch: Vec<LangfuseBatchItem>,
}

#[derive(Debug, Clone, Serialize)]
struct LangfuseBatchItem {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    body: serde_json::Value,
}

impl LangfuseService {
    pub fn new(config: &LangfuseConfig) -> Self {
        Self {
            config: config.clone(),
            client: reqwest::Client::new(),
            state: Mutex::new(LangfuseState::new()),
        }
    }

    /// Map a zymi event to 0 or 1 LangFuse batch items, updating internal state.
    fn map_event(state: &mut LangfuseState, event: &Event) -> Option<LangfuseBatchItem> {
        match &event.kind {
            EventKind::AgentProcessingStarted { conversation_id } => {
                let trace_id = event
                    .correlation_id
                    .unwrap_or(event.id)
                    .to_string();

                if let Some(corr) = event.correlation_id {
                    state.trace_map.insert(corr, trace_id.clone());
                }

                Some(LangfuseBatchItem {
                    id: Uuid::new_v4().to_string(),
                    event_type: "trace-create".into(),
                    body: serde_json::json!({
                        "id": trace_id,
                        "name": format!("agent:{conversation_id}"),
                        "timestamp": event.timestamp.to_rfc3339(),
                    }),
                })
            }

            EventKind::LlmCallStarted {
                iteration,
                message_count,
                approx_context_chars,
            } => {
                let trace_id = event
                    .correlation_id
                    .and_then(|c| state.trace_map.get(&c).cloned())?;

                let gen_id = event.id.to_string();
                if let Some(corr) = event.correlation_id {
                    state.active_generation.insert(corr, gen_id.clone());
                }

                Some(LangfuseBatchItem {
                    id: Uuid::new_v4().to_string(),
                    event_type: "generation-create".into(),
                    body: serde_json::json!({
                        "id": gen_id,
                        "traceId": trace_id,
                        "name": format!("llm-call-{iteration}"),
                        "startTime": event.timestamp.to_rfc3339(),
                        "metadata": {
                            "iteration": iteration,
                            "message_count": message_count,
                            "approx_context_chars": approx_context_chars,
                        },
                    }),
                })
            }

            EventKind::LlmCallCompleted {
                has_tool_calls,
                usage,
                content_preview,
            } => {
                let gen_id = event
                    .correlation_id
                    .and_then(|c| state.active_generation.remove(&c))?;

                let trace_id = event
                    .correlation_id
                    .and_then(|c| state.trace_map.get(&c).cloned())?;

                let mut body = serde_json::json!({
                    "id": gen_id,
                    "traceId": trace_id,
                    "endTime": event.timestamp.to_rfc3339(),
                    "metadata": {
                        "has_tool_calls": has_tool_calls,
                    },
                });

                if let Some(u) = usage {
                    body["usage"] = serde_json::json!({
                        "input": u.input_tokens,
                        "output": u.output_tokens,
                    });
                }
                if let Some(preview) = content_preview {
                    body["output"] = serde_json::json!(preview);
                }

                Some(LangfuseBatchItem {
                    id: Uuid::new_v4().to_string(),
                    event_type: "generation-update".into(),
                    body,
                })
            }

            EventKind::ToolCallRequested {
                tool_name,
                arguments,
                call_id,
            } => {
                let trace_id = event
                    .correlation_id
                    .and_then(|c| state.trace_map.get(&c).cloned())?;

                let span_id = event.id.to_string();
                state.span_map.insert(call_id.clone(), span_id.clone());

                Some(LangfuseBatchItem {
                    id: Uuid::new_v4().to_string(),
                    event_type: "span-create".into(),
                    body: serde_json::json!({
                        "id": span_id,
                        "traceId": trace_id,
                        "name": format!("tool:{tool_name}"),
                        "startTime": event.timestamp.to_rfc3339(),
                        "input": {
                            "arguments": arguments,
                        },
                    }),
                })
            }

            EventKind::ToolCallCompleted {
                call_id,
                result_preview,
                is_error,
                duration_ms,
            } => {
                let span_id = state.span_map.remove(call_id)?;

                let trace_id = event
                    .correlation_id
                    .and_then(|c| state.trace_map.get(&c).cloned())?;

                Some(LangfuseBatchItem {
                    id: Uuid::new_v4().to_string(),
                    event_type: "span-update".into(),
                    body: serde_json::json!({
                        "id": span_id,
                        "traceId": trace_id,
                        "endTime": event.timestamp.to_rfc3339(),
                        "output": {
                            "result_preview": result_preview,
                            "is_error": is_error,
                        },
                        "metadata": {
                            "duration_ms": duration_ms,
                        },
                    }),
                })
            }

            EventKind::AgentProcessingCompleted {
                conversation_id,
                success,
            } => {
                let trace_id = event
                    .correlation_id
                    .and_then(|c| state.trace_map.remove(&c))?;

                // Cleanup related maps
                if let Some(corr) = event.correlation_id {
                    state.active_generation.remove(&corr);
                }

                Some(LangfuseBatchItem {
                    id: Uuid::new_v4().to_string(),
                    event_type: "trace-update".into(),
                    body: serde_json::json!({
                        "id": trace_id,
                        "output": {
                            "conversation_id": conversation_id,
                            "success": success,
                        },
                    }),
                })
            }

            // Other event kinds are not mapped to LangFuse.
            _ => None,
        }
    }

    /// Send the current batch to LangFuse. Clears the buffer on success or failure.
    async fn flush_batch(&self, items: Vec<LangfuseBatchItem>) -> Result<(), ServiceError> {
        if items.is_empty() {
            return Ok(());
        }

        let url = format!("{}/api/public/ingestion", self.config.base_url);
        let request = IngestionRequest { batch: items };

        let resp = self
            .client
            .post(&url)
            .basic_auth(&self.config.public_key, Some(&self.config.secret_key))
            .json(&request)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            log::warn!("LangFuse ingestion failed (HTTP {status}): {body}");
        }

        Ok(())
    }
}

#[async_trait]
impl EventService for LangfuseService {
    fn name(&self) -> &str {
        "langfuse"
    }

    async fn handle_event(&self, event: &Event) -> Result<(), ServiceError> {
        let should_flush;
        let items_to_flush;

        {
            let mut state = self.state.lock().await;
            if let Some(item) = Self::map_event(&mut state, event) {
                state.batch.push(item);
            }

            should_flush = state.batch.len() >= self.config.batch_size
                || state.last_flush.elapsed().as_secs() >= self.config.flush_interval_secs;

            if should_flush {
                items_to_flush = std::mem::take(&mut state.batch);
                state.last_flush = Instant::now();
            } else {
                items_to_flush = Vec::new();
            }
        }

        if should_flush {
            self.flush_batch(items_to_flush).await?;
        }

        Ok(())
    }

    async fn flush(&self) -> Result<(), ServiceError> {
        let items = {
            let mut state = self.state.lock().await;
            std::mem::take(&mut state.batch)
        };
        self.flush_batch(items).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, TokenUsage};

    fn test_config() -> LangfuseConfig {
        LangfuseConfig {
            base_url: "https://test.langfuse.com".into(),
            public_key: "pk-test".into(),
            secret_key: "sk-test".into(),
            batch_size: 10,
            flush_interval_secs: 60,
        }
    }

    fn make_event(kind: EventKind, correlation_id: Option<Uuid>) -> Event {
        let mut event = Event::new("stream-1".into(), kind, "test".into());
        event.correlation_id = correlation_id;
        event
    }

    #[test]
    fn maps_agent_processing_started_to_trace_create() {
        let mut state = LangfuseState::new();
        let corr = Uuid::new_v4();
        let event = make_event(
            EventKind::AgentProcessingStarted {
                conversation_id: "conv-1".into(),
            },
            Some(corr),
        );

        let item = LangfuseService::map_event(&mut state, &event).unwrap();
        assert_eq!(item.event_type, "trace-create");
        assert_eq!(item.body["name"], "agent:conv-1");
        assert!(state.trace_map.contains_key(&corr));
    }

    #[test]
    fn maps_llm_call_started_to_generation_create() {
        let mut state = LangfuseState::new();
        let corr = Uuid::new_v4();

        // First create a trace
        let start = make_event(
            EventKind::AgentProcessingStarted {
                conversation_id: "conv-1".into(),
            },
            Some(corr),
        );
        LangfuseService::map_event(&mut state, &start);

        // Then LLM call
        let event = make_event(
            EventKind::LlmCallStarted {
                iteration: 0,
                message_count: 3,
                approx_context_chars: 1500,
            },
            Some(corr),
        );

        let item = LangfuseService::map_event(&mut state, &event).unwrap();
        assert_eq!(item.event_type, "generation-create");
        assert_eq!(item.body["name"], "llm-call-0");
        assert!(state.active_generation.contains_key(&corr));
    }

    #[test]
    fn maps_llm_call_completed_to_generation_update() {
        let mut state = LangfuseState::new();
        let corr = Uuid::new_v4();

        // Setup: trace + llm start
        let start = make_event(
            EventKind::AgentProcessingStarted {
                conversation_id: "conv-1".into(),
            },
            Some(corr),
        );
        LangfuseService::map_event(&mut state, &start);

        let llm_start = make_event(
            EventKind::LlmCallStarted {
                iteration: 0,
                message_count: 3,
                approx_context_chars: 1500,
            },
            Some(corr),
        );
        LangfuseService::map_event(&mut state, &llm_start);

        // Complete
        let event = make_event(
            EventKind::LlmCallCompleted {
                has_tool_calls: true,
                usage: Some(TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                }),
                content_preview: Some("hello world".into()),
            },
            Some(corr),
        );

        let item = LangfuseService::map_event(&mut state, &event).unwrap();
        assert_eq!(item.event_type, "generation-update");
        assert_eq!(item.body["usage"]["input"], 100);
        assert_eq!(item.body["usage"]["output"], 50);
        assert_eq!(item.body["output"], "hello world");
        // Active generation removed
        assert!(!state.active_generation.contains_key(&corr));
    }

    #[test]
    fn maps_tool_call_lifecycle() {
        let mut state = LangfuseState::new();
        let corr = Uuid::new_v4();

        // Setup trace
        let start = make_event(
            EventKind::AgentProcessingStarted {
                conversation_id: "conv-1".into(),
            },
            Some(corr),
        );
        LangfuseService::map_event(&mut state, &start);

        // Tool requested
        let requested = make_event(
            EventKind::ToolCallRequested {
                tool_name: "web_search".into(),
                arguments: r#"{"query":"test"}"#.into(),
                call_id: "tc-1".into(),
            },
            Some(corr),
        );
        let item = LangfuseService::map_event(&mut state, &requested).unwrap();
        assert_eq!(item.event_type, "span-create");
        assert_eq!(item.body["name"], "tool:web_search");
        assert!(state.span_map.contains_key("tc-1"));

        // Tool completed
        let completed = make_event(
            EventKind::ToolCallCompleted {
                call_id: "tc-1".into(),
                result_preview: "search results".into(),
                is_error: false,
                duration_ms: 250,
            },
            Some(corr),
        );
        let item = LangfuseService::map_event(&mut state, &completed).unwrap();
        assert_eq!(item.event_type, "span-update");
        assert_eq!(item.body["metadata"]["duration_ms"], 250);
        assert!(!state.span_map.contains_key("tc-1"));
    }

    #[test]
    fn maps_agent_processing_completed_to_trace_update() {
        let mut state = LangfuseState::new();
        let corr = Uuid::new_v4();

        // Setup
        let start = make_event(
            EventKind::AgentProcessingStarted {
                conversation_id: "conv-1".into(),
            },
            Some(corr),
        );
        LangfuseService::map_event(&mut state, &start);

        // Complete
        let event = make_event(
            EventKind::AgentProcessingCompleted {
                conversation_id: "conv-1".into(),
                success: true,
            },
            Some(corr),
        );

        let item = LangfuseService::map_event(&mut state, &event).unwrap();
        assert_eq!(item.event_type, "trace-update");
        assert_eq!(item.body["output"]["success"], true);
        // Trace map cleaned up
        assert!(!state.trace_map.contains_key(&corr));
    }

    #[test]
    fn unmapped_events_return_none() {
        let mut state = LangfuseState::new();
        let event = make_event(
            EventKind::UserMessageReceived {
                content: Message::User("hello".into()),
                connector: "test".into(),
            },
            None,
        );

        assert!(LangfuseService::map_event(&mut state, &event).is_none());
    }

    #[test]
    fn llm_call_without_trace_returns_none() {
        let mut state = LangfuseState::new();
        let corr = Uuid::new_v4();

        // LLM call without a preceding AgentProcessingStarted
        let event = make_event(
            EventKind::LlmCallStarted {
                iteration: 0,
                message_count: 1,
                approx_context_chars: 500,
            },
            Some(corr),
        );

        assert!(LangfuseService::map_event(&mut state, &event).is_none());
    }

    #[test]
    fn full_agent_lifecycle_mapping() {
        let mut state = LangfuseState::new();
        let corr = Uuid::new_v4();

        // 1. Agent starts
        let e1 = make_event(
            EventKind::AgentProcessingStarted {
                conversation_id: "conv-1".into(),
            },
            Some(corr),
        );
        assert_eq!(
            LangfuseService::map_event(&mut state, &e1)
                .unwrap()
                .event_type,
            "trace-create"
        );

        // 2. LLM call starts
        let e2 = make_event(
            EventKind::LlmCallStarted {
                iteration: 0,
                message_count: 2,
                approx_context_chars: 1000,
            },
            Some(corr),
        );
        assert_eq!(
            LangfuseService::map_event(&mut state, &e2)
                .unwrap()
                .event_type,
            "generation-create"
        );

        // 3. LLM call completes with tool calls
        let e3 = make_event(
            EventKind::LlmCallCompleted {
                has_tool_calls: true,
                usage: Some(TokenUsage {
                    input_tokens: 200,
                    output_tokens: 80,
                }),
                content_preview: None,
            },
            Some(corr),
        );
        assert_eq!(
            LangfuseService::map_event(&mut state, &e3)
                .unwrap()
                .event_type,
            "generation-update"
        );

        // 4. Tool requested
        let e4 = make_event(
            EventKind::ToolCallRequested {
                tool_name: "search".into(),
                arguments: "{}".into(),
                call_id: "tc-42".into(),
            },
            Some(corr),
        );
        assert_eq!(
            LangfuseService::map_event(&mut state, &e4)
                .unwrap()
                .event_type,
            "span-create"
        );

        // 5. Tool completed
        let e5 = make_event(
            EventKind::ToolCallCompleted {
                call_id: "tc-42".into(),
                result_preview: "ok".into(),
                is_error: false,
                duration_ms: 120,
            },
            Some(corr),
        );
        assert_eq!(
            LangfuseService::map_event(&mut state, &e5)
                .unwrap()
                .event_type,
            "span-update"
        );

        // 6. Second LLM call (final response)
        let e6 = make_event(
            EventKind::LlmCallStarted {
                iteration: 1,
                message_count: 4,
                approx_context_chars: 2000,
            },
            Some(corr),
        );
        assert_eq!(
            LangfuseService::map_event(&mut state, &e6)
                .unwrap()
                .event_type,
            "generation-create"
        );

        let e7 = make_event(
            EventKind::LlmCallCompleted {
                has_tool_calls: false,
                usage: Some(TokenUsage {
                    input_tokens: 300,
                    output_tokens: 150,
                }),
                content_preview: Some("Final answer".into()),
            },
            Some(corr),
        );
        assert_eq!(
            LangfuseService::map_event(&mut state, &e7)
                .unwrap()
                .event_type,
            "generation-update"
        );

        // 7. Agent completes
        let e8 = make_event(
            EventKind::AgentProcessingCompleted {
                conversation_id: "conv-1".into(),
                success: true,
            },
            Some(corr),
        );
        assert_eq!(
            LangfuseService::map_event(&mut state, &e8)
                .unwrap()
                .event_type,
            "trace-update"
        );

        // All state cleaned up
        assert!(state.trace_map.is_empty());
        assert!(state.active_generation.is_empty());
        assert!(state.span_map.is_empty());
    }

    #[tokio::test]
    async fn handle_event_buffers_and_respects_batch_size() {
        let mut config = test_config();
        config.batch_size = 3;
        let service = LangfuseService::new(&config);

        let corr = Uuid::new_v4();

        // Event 1: trace create
        let e1 = make_event(
            EventKind::AgentProcessingStarted {
                conversation_id: "conv-1".into(),
            },
            Some(corr),
        );
        // This won't trigger flush (1 < 3)
        service.handle_event(&e1).await.unwrap();

        {
            let state = service.state.lock().await;
            assert_eq!(state.batch.len(), 1);
        }

        // Event 2: generation create
        let e2 = make_event(
            EventKind::LlmCallStarted {
                iteration: 0,
                message_count: 1,
                approx_context_chars: 500,
            },
            Some(corr),
        );
        service.handle_event(&e2).await.unwrap();

        {
            let state = service.state.lock().await;
            assert_eq!(state.batch.len(), 2);
        }
    }
}
