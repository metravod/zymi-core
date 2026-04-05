pub mod langfuse;

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::watch;

use crate::config::ServicesConfig;
use crate::events::bus::EventBus;
use crate::events::Event;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("service error: {0}")]
    Other(String),
}

/// A service that subscribes to the event bus and performs side effects.
///
/// Implementations receive events one at a time via `handle_event`.
/// The `flush` method is called on graceful shutdown to drain any internal buffers.
#[async_trait]
pub trait EventService: Send + Sync {
    /// Human-readable name for logging.
    fn name(&self) -> &str;

    /// Process a single event. May buffer internally (e.g. for batching HTTP calls).
    async fn handle_event(&self, event: &Event) -> Result<(), ServiceError>;

    /// Flush internal buffers. Called once on shutdown.
    async fn flush(&self) -> Result<(), ServiceError>;
}

/// Manages background tasks that drive event services.
pub struct ServiceRunner {
    handles: Vec<tokio::task::JoinHandle<()>>,
    shutdown_tx: watch::Sender<bool>,
}

impl Default for ServiceRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceRunner {
    pub fn new() -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            handles: Vec::new(),
            shutdown_tx,
        }
    }

    /// Subscribe to the bus and spawn a background task that feeds events to the service.
    pub async fn start_service(
        &mut self,
        bus: &EventBus,
        service: Arc<dyn EventService>,
    ) {
        let mut rx = bus.subscribe().await;
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let name = service.name().to_owned();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    maybe_event = rx.recv() => {
                        match maybe_event {
                            Some(ref event) => {
                                if let Err(e) = service.handle_event(event).await {
                                    log::warn!("Service '{name}' handle error: {e}");
                                }
                            }
                            None => break, // bus dropped
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        // Drain remaining buffered events
                        while let Ok(ref event) = rx.try_recv() {
                            let _ = service.handle_event(event).await;
                        }
                        if let Err(e) = service.flush().await {
                            log::warn!("Service '{name}' flush error: {e}");
                        }
                        break;
                    }
                }
            }
        });

        self.handles.push(handle);
    }

    /// Signal all services to stop, flush, and await completion.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        for handle in self.handles {
            let _ = handle.await;
        }
    }
}

/// Create a `ServiceRunner` and start all services enabled in the config.
pub async fn start_configured_services(
    bus: &EventBus,
    config: &ServicesConfig,
) -> ServiceRunner {
    let mut runner = ServiceRunner::new();

    if let Some(ref langfuse_config) = config.langfuse {
        let service = Arc::new(langfuse::LangfuseService::new(langfuse_config));
        log::info!("Starting service: {}", service.name());
        runner.start_service(bus, service).await;
    }

    runner
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::store::SqliteEventStore;
    use crate::events::EventKind;
    use crate::types::Message;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// A mock service that counts events and records flush calls.
    struct MockService {
        name: String,
        event_count: AtomicUsize,
        flush_count: AtomicUsize,
    }

    impl MockService {
        fn new(name: &str) -> Self {
            Self {
                name: name.into(),
                event_count: AtomicUsize::new(0),
                flush_count: AtomicUsize::new(0),
            }
        }

        fn events(&self) -> usize {
            self.event_count.load(Ordering::SeqCst)
        }

        fn flushes(&self) -> usize {
            self.flush_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl EventService for MockService {
        fn name(&self) -> &str {
            &self.name
        }

        async fn handle_event(&self, _event: &Event) -> Result<(), ServiceError> {
            self.event_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn flush(&self) -> Result<(), ServiceError> {
            self.flush_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    async fn setup() -> (TempDir, Arc<EventBus>) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_services.db");
        let store = Arc::new(SqliteEventStore::new(&db_path).unwrap());
        let bus = Arc::new(EventBus::new(store));
        (dir, bus)
    }

    #[tokio::test]
    async fn service_receives_published_events() {
        let (_dir, bus) = setup().await;
        let service = Arc::new(MockService::new("test"));
        let service_ref = Arc::clone(&service);

        let mut runner = ServiceRunner::new();
        runner.start_service(&bus, service).await;

        for _ in 0..3 {
            let event = Event::new(
                "s1".into(),
                EventKind::UserMessageReceived {
                    content: Message::User("hello".into()),
                    connector: "test".into(),
                },
                "test".into(),
            );
            bus.publish(event).await.unwrap();
        }

        // Give the background task time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(service_ref.events(), 3);

        runner.shutdown().await;
        assert_eq!(service_ref.flushes(), 1);
    }

    #[tokio::test]
    async fn multiple_services_all_receive() {
        let (_dir, bus) = setup().await;
        let svc1 = Arc::new(MockService::new("svc1"));
        let svc2 = Arc::new(MockService::new("svc2"));
        let svc1_ref = Arc::clone(&svc1);
        let svc2_ref = Arc::clone(&svc2);

        let mut runner = ServiceRunner::new();
        runner.start_service(&bus, svc1).await;
        runner.start_service(&bus, svc2).await;

        let event = Event::new(
            "s1".into(),
            EventKind::AgentProcessingStarted {
                conversation_id: "s1".into(),
            },
            "agent".into(),
        );
        bus.publish(event).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(svc1_ref.events(), 1);
        assert_eq!(svc2_ref.events(), 1);

        runner.shutdown().await;
        assert_eq!(svc1_ref.flushes(), 1);
        assert_eq!(svc2_ref.flushes(), 1);
    }

    #[tokio::test]
    async fn shutdown_flushes_services() {
        let (_dir, bus) = setup().await;
        let service = Arc::new(MockService::new("flush-test"));
        let service_ref = Arc::clone(&service);

        let mut runner = ServiceRunner::new();
        runner.start_service(&bus, service).await;

        runner.shutdown().await;
        assert_eq!(service_ref.flushes(), 1);
    }

    #[tokio::test]
    async fn service_error_does_not_crash_runner() {
        struct FailingService;

        #[async_trait]
        impl EventService for FailingService {
            fn name(&self) -> &str {
                "failing"
            }
            async fn handle_event(&self, _event: &Event) -> Result<(), ServiceError> {
                Err(ServiceError::Other("boom".into()))
            }
            async fn flush(&self) -> Result<(), ServiceError> {
                Ok(())
            }
        }

        let (_dir, bus) = setup().await;
        let mut runner = ServiceRunner::new();
        runner.start_service(&bus, Arc::new(FailingService)).await;

        let event = Event::new(
            "s1".into(),
            EventKind::ResponseReady {
                conversation_id: "s1".into(),
                content: "done".into(),
            },
            "agent".into(),
        );
        bus.publish(event).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        runner.shutdown().await;
        // No panic — runner survived the error
    }
}
