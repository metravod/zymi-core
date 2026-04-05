use std::sync::Arc;

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::types::StreamEvent;

use super::bus::EventBus;
use super::stream_registry::StreamRegistry;
use super::{Event, EventKind};

/// Adapter for connectors (Telegram, CLI, scheduler) to publish events and await responses.
///
/// Usage: the connector sets up its approval handler (ApprovalSlotGuard) as usual,
/// then calls `submit_and_wait()` or `submit_and_wait_streaming()`.
/// The approval handler remains active in the connector's scope while the
/// AgentWorker processes the event asynchronously.
pub struct EventDrivenConnector {
    bus: Arc<EventBus>,
    stream_registry: Arc<StreamRegistry>,
}

impl EventDrivenConnector {
    pub fn new(bus: Arc<EventBus>, stream_registry: Arc<StreamRegistry>) -> Self {
        Self { bus, stream_registry }
    }

    /// Publish a UserMessageReceived event and wait for the matching ResponseReady.
    ///
    /// Returns the response content, or an error if the timeout elapses.
    /// The caller should hold an ApprovalSlotGuard while this is in progress.
    pub async fn submit_and_wait(
        &self,
        conversation_id: &str,
        message: crate::types::Message,
        source: &str,
        timeout: std::time::Duration,
    ) -> Result<String, ConnectorError> {
        let correlation_id = Uuid::new_v4();

        // Filtered subscribe: only events with our correlation_id are delivered
        let mut rx = self.bus.subscribe_correlation(correlation_id).await;

        let event = Event::new(
            conversation_id.into(),
            EventKind::UserMessageReceived {
                content: message,
                connector: source.into(),
            },
            source.into(),
        )
        .with_correlation(correlation_id);

        self.bus
            .publish(event)
            .await
            .map_err(|e| ConnectorError::PublishFailed(e.to_string()))?;

        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                maybe_event = rx.recv() => {
                    match maybe_event {
                        Some(ref event) => {
                            if let EventKind::ResponseReady { ref content, .. } = event.kind {
                                return Ok(content.clone());
                            }
                        }
                        None => return Err(ConnectorError::BusClosed),
                    }
                }
                _ = &mut deadline => {
                    return Err(ConnectorError::Timeout);
                }
            }
        }
    }

    /// Fire-and-forget: publish an event without waiting for a response.
    /// Useful for scheduled tasks where the result goes to a notification channel.
    pub async fn submit(
        &self,
        conversation_id: &str,
        message: crate::types::Message,
        source: &str,
    ) -> Result<Uuid, ConnectorError> {
        let correlation_id = Uuid::new_v4();

        let event = Event::new(
            conversation_id.into(),
            EventKind::UserMessageReceived {
                content: message,
                connector: source.into(),
            },
            source.into(),
        )
        .with_correlation(correlation_id);

        self.bus
            .publish(event)
            .await
            .map_err(|e| ConnectorError::PublishFailed(e.to_string()))?;

        Ok(correlation_id)
    }

    /// Publish a UserMessageReceived event, register a StreamEvent sender for
    /// real-time streaming, and wait for the matching ResponseReady.
    ///
    /// The `stream_tx` is registered in the StreamRegistry so that the
    /// AgentWorker can forward StreamEvents (tokens, tool calls, etc.)
    /// directly to the caller while processing.
    ///
    /// Returns the final response content, or an error on timeout.
    pub async fn submit_and_wait_streaming(
        &self,
        conversation_id: &str,
        message: crate::types::Message,
        source: &str,
        timeout: std::time::Duration,
        stream_tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<String, ConnectorError> {
        let correlation_id = Uuid::new_v4();
        let corr_str = correlation_id.to_string();

        // Register stream sender BEFORE publishing to avoid race condition
        self.stream_registry.register(&corr_str, stream_tx).await;

        // Filtered subscribe: only events with our correlation_id
        let mut rx = self.bus.subscribe_correlation(correlation_id).await;

        let event = Event::new(
            conversation_id.into(),
            EventKind::UserMessageReceived {
                content: message,
                connector: source.into(),
            },
            source.into(),
        )
        .with_correlation(correlation_id);

        if let Err(e) = self.bus.publish(event).await {
            self.stream_registry.remove(&corr_str).await;
            return Err(ConnectorError::PublishFailed(e.to_string()));
        }

        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);

        let result = loop {
            tokio::select! {
                maybe_event = rx.recv() => {
                    match maybe_event {
                        Some(ref event) => {
                            if let EventKind::ResponseReady { ref content, .. } = event.kind {
                                break Ok(content.clone());
                            }
                        }
                        None => break Err(ConnectorError::BusClosed),
                    }
                }
                _ = &mut deadline => {
                    break Err(ConnectorError::Timeout);
                }
            }
        };

        // Cleanup on error
        if result.is_err() {
            self.stream_registry.remove(&corr_str).await;
        }

        result
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectorError {
    #[error("failed to publish event: {0}")]
    PublishFailed(String),
    #[error("response timeout")]
    Timeout,
    #[error("event bus closed")]
    BusClosed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use crate::events::store::SqliteEventStore;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, Arc<EventBus>, Arc<StreamRegistry>) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_connector.db");
        let store = Arc::new(SqliteEventStore::new(&db_path).unwrap());
        let bus = Arc::new(EventBus::new(store));
        let registry = Arc::new(StreamRegistry::new());
        (dir, bus, registry)
    }

    #[tokio::test]
    async fn submit_and_wait_receives_response() {
        let (_dir, bus, registry) = setup().await;
        let connector = EventDrivenConnector::new(bus.clone(), registry);

        let bus_clone = bus.clone();
        tokio::spawn(async move {
            let mut rx = bus_clone.subscribe().await;
            while let Some(event) = rx.recv().await {
                if let EventKind::UserMessageReceived { .. } = &event.kind {
                    let response = Event::new(
                        event.stream_id.clone(),
                        EventKind::ResponseReady {
                            conversation_id: event.stream_id.clone(),
                            content: "echo response".into(),
                        },
                        "mock_worker".into(),
                    )
                    .with_correlation(event.correlation_id.unwrap());

                    bus_clone.publish(response).await.unwrap();
                }
            }
        });

        let result = connector
            .submit_and_wait(
                "test-conv",
                Message::User("hello".into()),
                "test",
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(result, "echo response");
    }

    #[tokio::test]
    async fn submit_and_wait_timeout() {
        let (_dir, bus, registry) = setup().await;
        let connector = EventDrivenConnector::new(bus, registry);

        let result = connector
            .submit_and_wait(
                "test-conv",
                Message::User("hello".into()),
                "test",
                std::time::Duration::from_millis(50),
            )
            .await;

        assert!(matches!(result, Err(ConnectorError::Timeout)));
    }

    #[tokio::test]
    async fn submit_fire_and_forget() {
        let (_dir, bus, registry) = setup().await;
        let connector = EventDrivenConnector::new(bus.clone(), registry);

        let corr = connector
            .submit("test-conv", Message::User("hello".into()), "scheduler")
            .await
            .unwrap();

        let events = bus.store().read_stream("test-conv", 1).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].correlation_id, Some(corr));
    }
}
