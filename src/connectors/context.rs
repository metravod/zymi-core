//! Flat template / extract contexts (ADR-0021 §Event-side constraints).
//!
//! The YAML contract exposes a small, stable dictionary shape to both
//! JSONPath extractors (inbound) and MiniJinja templates (outbound). Keeping
//! the dict flat and decoupled from the internal `Event` struct means
//! renaming a Rust field never breaks a user's `project.yml`.

use serde::Serialize;
use serde_json::json;
use serde_json::Value as JsonValue;

use crate::events::{Event, EventKind};
use crate::types::Message;

/// Template context shape handed to outbound `body_template` / `url` renders.
///
/// Access in MiniJinja templates as `{{ event.<field> }}`.
#[derive(Debug, Clone, Serialize)]
pub struct EventContext {
    pub kind: &'static str,
    pub stream_id: String,
    pub correlation_id: Option<String>,
    pub source: String,
    pub timestamp: String,
    /// Flattened payload view. Which fields exist depends on `kind`:
    /// `user_message_received` → `content`, `connector`.
    /// `response_ready` → `conversation_id`, `content`.
    /// Other kinds fall back to the serde-serialised `EventKind` data as
    /// a JSON object under `payload`.
    #[serde(flatten)]
    pub payload: JsonValue,
}

impl EventContext {
    pub fn from_event(event: &Event) -> Self {
        let payload = flatten_payload(&event.kind);
        Self {
            kind: event.kind.tag(),
            stream_id: event.stream_id.clone(),
            correlation_id: event.correlation_id.map(|c| c.to_string()),
            source: event.source.clone(),
            timestamp: event.timestamp.to_rfc3339(),
            payload,
        }
    }
}

fn flatten_payload(kind: &EventKind) -> JsonValue {
    match kind {
        EventKind::UserMessageReceived { content, connector } => json!({
            "content": message_text(content),
            "connector": connector,
        }),
        EventKind::ResponseReady {
            conversation_id,
            content,
        } => json!({
            "conversation_id": conversation_id,
            "content": content,
        }),
        other => {
            // Fall back: serialise the typed enum variant and expose its
            // `data` body as the flat payload. Unknown variants are still
            // usable from templates via `{{ event.payload.<field> }}`.
            let raw = serde_json::to_value(other).unwrap_or(JsonValue::Null);
            if let JsonValue::Object(mut map) = raw {
                if let Some(data) = map.remove("data") {
                    data
                } else {
                    JsonValue::Object(map)
                }
            } else {
                json!({"payload": raw})
            }
        }
    }
}

fn message_text(m: &Message) -> String {
    use crate::types::ContentPart;
    match m {
        Message::User(t) => t.clone(),
        Message::UserMultimodal { parts } => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        Message::System(t) => t.clone(),
        Message::Assistant { content, .. } => content.clone().unwrap_or_default(),
        Message::ToolResult { content, .. } => content.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;

    #[test]
    fn response_ready_flattens_expected_fields() {
        let ev = Event::new(
            "s1".into(),
            EventKind::ResponseReady {
                conversation_id: "s1".into(),
                content: "hi there".into(),
            },
            "agent".into(),
        );
        let ctx = EventContext::from_event(&ev);
        assert_eq!(ctx.kind, "response_ready");
        assert_eq!(ctx.stream_id, "s1");
        let v = serde_json::to_value(&ctx).unwrap();
        assert_eq!(v["content"], "hi there");
        assert_eq!(v["conversation_id"], "s1");
    }

    #[test]
    fn user_message_received_flattens_content() {
        let ev = Event::new(
            "s2".into(),
            EventKind::UserMessageReceived {
                content: Message::User("hello".into()),
                connector: "telegram".into(),
            },
            "telegram".into(),
        );
        let ctx = EventContext::from_event(&ev);
        let v = serde_json::to_value(&ctx).unwrap();
        assert_eq!(v["content"], "hello");
        assert_eq!(v["connector"], "telegram");
    }
}
