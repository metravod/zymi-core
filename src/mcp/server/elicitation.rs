//! MCP elicitation approval channel (ADR-0033 2b-sync).
//!
//! Bridges an event-sourced approval (ADR-0022) to the connected MCP client:
//! when a pipeline parked on an approval publishes
//! [`EventKind::ApprovalRequested`] routed to this channel, the channel issues
//! an `elicitation/create` request to the client (via [`McpClientLink`]),
//! renders the decision as a tiny approve/deny form, and publishes
//! [`EventKind::ApprovalGranted`] / [`EventKind::ApprovalDenied`] back onto the
//! bus to unblock the orchestrator.
//!
//! This is the *synchronous* slice: the elicitation is nested inside the live
//! `tools/call` request, correlated only by JSON-RPC id — no SEP-1686 task /
//! `related-task` layer. The async slice (`input_required` on a backgrounded
//! task) stays parked on host adoption (ADR-0033 addendum).
//!
//! Structurally mirrors [`crate::approval::TelegramApprovalChannel`]: subscribe
//! to the bus, talk to an external surface, publish the decision back.

use std::sync::Arc;

use async_trait::async_trait;

use crate::approval::{ApprovalChannel, ChannelHandle};
use crate::events::bus::EventBus;
use crate::events::{Event, EventKind};

use super::link::McpClientLink;

/// Channel name the orchestrator routes to. Auto-set as the default approval
/// channel under `zymi mcp serve` when the project doesn't declare one.
pub const CHANNEL_NAME: &str = "mcp_elicitation";

/// Approval channel that surfaces approvals as MCP `elicitation/create` forms.
pub struct McpElicitationApprovalChannel {
    name: String,
    link: Arc<McpClientLink>,
}

impl McpElicitationApprovalChannel {
    pub fn new(link: Arc<McpClientLink>) -> Self {
        Self {
            name: CHANNEL_NAME.to_string(),
            link,
        }
    }
}

/// Build the `elicitation/create` params for an approval prompt. The schema
/// is a single required `decision` enum, which MCP hosts render as a
/// radio/select. Kept flat per the elicitation schema restrictions.
fn elicitation_params(description: &str, explanation: Option<&str>) -> serde_json::Value {
    let message = match explanation {
        Some(reason) => format!("{description}\n\nReason: {reason}"),
        None => description.to_string(),
    };
    serde_json::json!({
        "message": message,
        "requestedSchema": {
            "type": "object",
            "properties": {
                "decision": {
                    "type": "string",
                    "enum": ["approve", "deny"],
                    "description": "Approve or deny the requested action"
                }
            },
            "required": ["decision"]
        }
    })
}

/// Map an `elicitation/create` result to an approve/deny verdict. Granted iff
/// the user submitted the form (`action == "accept"`) AND chose `approve`.
/// `decline` / `cancel` / `deny` all map to denial — fail-closed.
fn verdict_from_result(result: &serde_json::Value) -> bool {
    let accepted = result.get("action").and_then(|a| a.as_str()) == Some("accept");
    let approved = result
        .get("content")
        .and_then(|c| c.get("decision"))
        .and_then(|d| d.as_str())
        == Some("approve");
    accepted && approved
}

#[async_trait]
impl ApprovalChannel for McpElicitationApprovalChannel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn start(&self, bus: Arc<EventBus>) -> Result<ChannelHandle, String> {
        let mut rx = bus.subscribe().await;
        let channel_name = self.name.clone();
        let link = Arc::clone(&self.link);

        let join = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let EventKind::ApprovalRequested {
                    approval_id,
                    stream_id,
                    description,
                    explanation,
                    channel,
                } = &ev.kind
                else {
                    continue;
                };
                if channel != &channel_name {
                    continue;
                }

                let approval_id = approval_id.clone();
                let stream_id = stream_id.clone();
                let description = description.clone();
                let explanation = explanation.clone();
                let correlation_id = ev.correlation_id;
                let link = Arc::clone(&link);
                let bus = Arc::clone(&bus);
                let channel_name = channel_name.clone();

                // Per-request task so a slow human at the form can't pin the
                // bus subscriber (mirrors the terminal/telegram channels).
                tokio::spawn(async move {
                    let (approved, deny_reason) = if !link.supports_elicitation() {
                        // Client never advertised elicitation — fail-closed
                        // with an audit-visible reason (mirrors 2a's "no
                        // elicitation-capable client" behaviour).
                        (false, Some("client_no_elicitation".to_string()))
                    } else {
                        let params = elicitation_params(&description, explanation.as_deref());
                        match link.request("elicitation/create", params).await {
                            Ok(result) => {
                                if verdict_from_result(&result) {
                                    (true, None)
                                } else {
                                    (false, None)
                                }
                            }
                            Err(e) => (false, Some(format!("elicitation_failed: {e}"))),
                        }
                    };

                    let decided_by = format!("mcp_elicitation:{channel_name}");
                    let kind = if approved {
                        EventKind::ApprovalGranted {
                            approval_id,
                            stream_id: stream_id.clone(),
                            decided_by,
                            reason: None,
                        }
                    } else {
                        EventKind::ApprovalDenied {
                            approval_id,
                            stream_id: stream_id.clone(),
                            decided_by,
                            reason: deny_reason,
                        }
                    };
                    let mut decision = Event::new(stream_id, kind, "approval_channel".into());
                    if let Some(corr) = correlation_id {
                        decision = decision.with_correlation(corr);
                    }
                    if let Err(e) = bus.publish(decision).await {
                        log::warn!("mcp elicitation approval decision publish failed: {e}");
                    }
                });
            }
        });

        Ok(ChannelHandle::from_task(self.name.clone(), join))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::store::SqliteEventStore;
    use crate::mcp::server::link::Outbound;
    use crate::mcp::server::protocol::ClientCaps;
    use serde_json::json;
    use std::time::Duration;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn approval_request(channel: &str, correlation: Uuid) -> Event {
        Event::new(
            "stream-x".into(),
            EventKind::ApprovalRequested {
                approval_id: "req-1".into(),
                stream_id: "stream-x".into(),
                description: "rm -rf /tmp/cache".into(),
                explanation: Some("free disk".into()),
                channel: channel.into(),
            },
            "orchestrator".into(),
        )
        .with_correlation(correlation)
    }

    async fn setup() -> (TempDir, Arc<EventBus>) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(SqliteEventStore::new(&dir.path().join("elicit.db")).unwrap());
        (dir, Arc::new(EventBus::new(store)))
    }

    /// Accept + decision=approve → ApprovalGranted, correlation preserved.
    #[tokio::test]
    async fn accept_approve_grants() {
        let (_dir, bus) = setup().await;
        let (link, mut outbound_rx) = McpClientLink::new();
        link.set_caps(ClientCaps { elicitation: true });

        let channel = McpElicitationApprovalChannel::new(Arc::clone(&link));
        let _handle = channel.start(Arc::clone(&bus)).await.unwrap();

        let correlation = Uuid::new_v4();
        let mut decisions = bus.subscribe_correlation(correlation).await;
        bus.publish(approval_request(CHANNEL_NAME, correlation))
            .await
            .unwrap();

        // The channel issues elicitation/create; play the client and accept.
        let id = loop {
            match outbound_rx.recv().await.unwrap() {
                Outbound::Request { id, method, params } => {
                    assert_eq!(method, "elicitation/create");
                    assert_eq!(params["requestedSchema"]["required"][0], "decision");
                    break id;
                }
                _ => continue,
            }
        };
        link.deliver_response(&id, Ok(json!({ "action": "accept", "content": { "decision": "approve" } })))
            .await;

        let decision = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let ev = decisions.recv().await.unwrap();
                if matches!(
                    ev.kind,
                    EventKind::ApprovalGranted { .. } | EventKind::ApprovalDenied { .. }
                ) {
                    return ev;
                }
            }
        })
        .await
        .expect("no decision");
        match &decision.kind {
            EventKind::ApprovalGranted { approval_id, .. } => {
                assert_eq!(approval_id.as_str(), "req-1")
            }
            other => panic!("expected granted, got {other:?}"),
        }
        assert_eq!(decision.correlation_id, Some(correlation));
    }

    /// Decline → ApprovalDenied.
    #[tokio::test]
    async fn decline_denies() {
        let (_dir, bus) = setup().await;
        let (link, mut outbound_rx) = McpClientLink::new();
        link.set_caps(ClientCaps { elicitation: true });
        let channel = McpElicitationApprovalChannel::new(Arc::clone(&link));
        let _handle = channel.start(Arc::clone(&bus)).await.unwrap();

        let correlation = Uuid::new_v4();
        let mut decisions = bus.subscribe_correlation(correlation).await;
        bus.publish(approval_request(CHANNEL_NAME, correlation))
            .await
            .unwrap();

        let id = match outbound_rx.recv().await.unwrap() {
            Outbound::Request { id, .. } => id,
            _ => panic!("expected request"),
        };
        link.deliver_response(&id, Ok(json!({ "action": "decline" })))
            .await;

        let decision = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let ev = decisions.recv().await.unwrap();
                if matches!(
                    ev.kind,
                    EventKind::ApprovalGranted { .. } | EventKind::ApprovalDenied { .. }
                ) {
                    return ev;
                }
            }
        })
        .await
        .expect("no decision");
        assert!(matches!(decision.kind, EventKind::ApprovalDenied { .. }));
    }

    /// Client without elicitation capability → fail-closed denial, and no
    /// elicitation/create is ever sent.
    #[tokio::test]
    async fn no_capability_fails_closed() {
        let (_dir, bus) = setup().await;
        let (link, mut outbound_rx) = McpClientLink::new();
        // No set_caps → supports_elicitation() == false.
        let channel = McpElicitationApprovalChannel::new(Arc::clone(&link));
        let _handle = channel.start(Arc::clone(&bus)).await.unwrap();

        let correlation = Uuid::new_v4();
        let mut decisions = bus.subscribe_correlation(correlation).await;
        bus.publish(approval_request(CHANNEL_NAME, correlation))
            .await
            .unwrap();

        let decision = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let ev = decisions.recv().await.unwrap();
                if matches!(
                    ev.kind,
                    EventKind::ApprovalGranted { .. } | EventKind::ApprovalDenied { .. }
                ) {
                    return ev;
                }
            }
        })
        .await
        .expect("no decision");
        match &decision.kind {
            EventKind::ApprovalDenied { reason, .. } => {
                assert_eq!(reason.as_deref(), Some("client_no_elicitation"));
            }
            other => panic!("expected denied, got {other:?}"),
        }
        // Nothing should have been written to the wire.
        assert!(outbound_rx.try_recv().is_err());
    }

    /// An approval routed to a *different* channel is ignored.
    #[tokio::test]
    async fn ignores_other_channels() {
        let (_dir, bus) = setup().await;
        let (link, mut outbound_rx) = McpClientLink::new();
        link.set_caps(ClientCaps { elicitation: true });
        let channel = McpElicitationApprovalChannel::new(Arc::clone(&link));
        let _handle = channel.start(Arc::clone(&bus)).await.unwrap();

        bus.publish(approval_request("telegram", Uuid::new_v4()))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(outbound_rx.try_recv().is_err());
    }
}
