use std::collections::HashMap;

use tokio::sync::{mpsc, RwLock};

use crate::types::StreamEvent;

/// Shared registry mapping correlation_id → StreamEvent sender.
///
/// Used to bridge the EventDrivenConnector (which registers a sender before
/// publishing) and the AgentWorker (which takes the sender to forward
/// streaming tokens from `process_stream`).
///
/// The `take` semantic is one-shot: once the AgentWorker picks up the sender,
/// it is removed from the registry to prevent double-consumption.
pub struct StreamRegistry {
    senders: RwLock<HashMap<String, mpsc::UnboundedSender<StreamEvent>>>,
}

impl StreamRegistry {
    pub fn new() -> Self {
        Self {
            senders: RwLock::new(HashMap::new()),
        }
    }

    /// Register a stream sender for a given correlation_id.
    /// Must be called before publishing the event to avoid a race condition.
    pub async fn register(&self, correlation_id: &str, tx: mpsc::UnboundedSender<StreamEvent>) {
        self.senders
            .write()
            .await
            .insert(correlation_id.to_string(), tx);
    }

    /// Take the registered sender for a correlation_id (one-shot removal).
    /// Returns `None` if no sender was registered (e.g. Telegram path).
    pub async fn take(&self, correlation_id: &str) -> Option<mpsc::UnboundedSender<StreamEvent>> {
        self.senders.write().await.remove(correlation_id)
    }

    /// Remove a registered sender without returning it (cleanup on error/timeout).
    pub async fn remove(&self, correlation_id: &str) {
        self.senders.write().await.remove(correlation_id);
    }
}

impl Default for StreamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_take() {
        let registry = StreamRegistry::new();
        let (tx, mut rx) = mpsc::unbounded_channel();

        registry.register("corr-1", tx).await;

        let taken = registry.take("corr-1").await;
        assert!(taken.is_some());

        taken.unwrap().send(StreamEvent::Done("ok".into())).unwrap();
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, StreamEvent::Done(s) if s == "ok"));
    }

    #[tokio::test]
    async fn take_returns_none_after_first_take() {
        let registry = StreamRegistry::new();
        let (tx, _rx) = mpsc::unbounded_channel();

        registry.register("corr-1", tx).await;
        let _ = registry.take("corr-1").await;

        assert!(registry.take("corr-1").await.is_none());
    }

    #[tokio::test]
    async fn take_returns_none_when_not_registered() {
        let registry = StreamRegistry::new();
        assert!(registry.take("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn remove_cleans_up() {
        let registry = StreamRegistry::new();
        let (tx, _rx) = mpsc::unbounded_channel();

        registry.register("corr-1", tx).await;
        registry.remove("corr-1").await;

        assert!(registry.take("corr-1").await.is_none());
    }
}
