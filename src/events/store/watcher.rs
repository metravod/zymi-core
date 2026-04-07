//! Cross-process event delivery via SQLite tail polling.
//!
//! [`StoreTailWatcher`] periodically reads new events from the underlying
//! [`EventStore`] and fans them out to local subscribers through
//! [`EventBus::redeliver`]. This closes the gap between
//! [`adr/0005`](../../../../adr/0005-python-as-event-driven-components.md) — which
//! declares Python components as cross-process participants — and the
//! in-memory [`EventBus`] implementation, which by itself only sees events
//! produced inside the same process.
//!
//! ## Why polling
//!
//! SQLite has no built-in cross-process notification primitive
//! (`sqlite3_update_hook` only fires inside the connection that did the
//! write). Polling at ~100ms is more than enough for LLM-bound workloads
//! and avoids dragging in a separate IPC channel just to nudge the watcher.
//!
//! ## Hash chain safety
//!
//! The watcher **never** calls `store.append`. Events are read by their
//! global cursor and republished via [`EventBus::redeliver`], which is a
//! pure fan-out — no second persistence, no duplicated rows, no broken
//! per-stream hash chain.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::events::bus::EventBus;

use super::event_store::EventStore;

/// Default interval between poll cycles.
const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);
/// Default batch size per `tail` call. The watcher will keep pulling
/// without sleeping while the store keeps returning full batches.
const DEFAULT_BATCH_SIZE: usize = 256;

/// Polls an [`EventStore`] for events written by other processes and
/// fans them out into a local [`EventBus`].
pub struct StoreTailWatcher {
    store: Arc<dyn EventStore>,
    bus: Arc<EventBus>,
    interval: Duration,
    batch_size: usize,
}

/// Handle to a running watcher task. Dropping the handle stops the watcher.
pub struct WatcherHandle {
    cancel: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl WatcherHandle {
    /// Stop the watcher and wait for the polling task to exit.
    pub async fn stop(mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.join.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
        // Don't await JoinHandle in Drop — it will be cancelled when the
        // runtime shuts down. Callers wanting graceful shutdown should
        // call `stop().await` explicitly.
    }
}

impl StoreTailWatcher {
    pub fn new(store: Arc<dyn EventStore>, bus: Arc<EventBus>) -> Self {
        Self {
            store,
            bus,
            interval: DEFAULT_INTERVAL,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    /// Override the polling interval. Lower values reduce delivery latency
    /// at the cost of CPU and SQLite contention.
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Override the batch size. The watcher pulls up to this many events
    /// per `tail` call before checking back in with the cancellation signal.
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    /// Spawn the polling task. Returns a [`WatcherHandle`] that stops the
    /// watcher when dropped or when [`WatcherHandle::stop`] is called.
    ///
    /// The watcher reads `current_global_seq` once on startup so that
    /// already-existing events are skipped — only events written *after*
    /// this point are republished.
    pub fn spawn(self) -> WatcherHandle {
        let StoreTailWatcher {
            store,
            bus,
            interval,
            batch_size,
        } = self;
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();

        let join = tokio::spawn(async move {
            let mut last_seen = match store.current_global_seq().await {
                Ok(seq) => seq,
                Err(e) => {
                    log::error!("StoreTailWatcher: failed to read initial cursor: {e}");
                    0
                }
            };

            loop {
                // Drain a batch. If we get a full batch, immediately try
                // again — this lets the watcher catch up after a stall
                // without waiting for the next interval tick.
                loop {
                    if cancel_rx.try_recv().is_ok() {
                        return;
                    }
                    match store.tail(last_seen, batch_size).await {
                        Ok(batch) => {
                            if batch.is_empty() {
                                break;
                            }
                            let received = batch.len();
                            for tailed in batch {
                                last_seen = tailed.global_seq;
                                bus.redeliver(Arc::new(tailed.event)).await;
                            }
                            if received < batch_size {
                                break;
                            }
                            // Full batch — keep pulling.
                        }
                        Err(e) => {
                            log::warn!(
                                "StoreTailWatcher: tail() failed at cursor {last_seen}: {e}"
                            );
                            break;
                        }
                    }
                }

                tokio::select! {
                    _ = &mut cancel_rx => return,
                    _ = tokio::time::sleep(interval) => {}
                }
            }
        });

        WatcherHandle {
            cancel: Some(cancel_tx),
            join: Some(join),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::bus::EventBus;
    use crate::events::store::SqliteEventStore;
    use crate::events::{Event, EventKind};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::time::{sleep, timeout};

    fn make_event(stream: &str) -> Event {
        Event::new(
            stream.into(),
            EventKind::AgentProcessingStarted {
                conversation_id: stream.into(),
            },
            "test".into(),
        )
    }

    async fn build() -> (TempDir, Arc<dyn EventStore>, Arc<EventBus>) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("watcher.db");
        let store: Arc<dyn EventStore> = Arc::new(SqliteEventStore::new(&path).unwrap());
        let bus = Arc::new(EventBus::new(Arc::clone(&store)));
        (dir, store, bus)
    }

    #[tokio::test]
    async fn delivers_external_appends() {
        let (_dir, store, bus) = build().await;
        let mut rx = bus.subscribe().await;

        let watcher = StoreTailWatcher::new(Arc::clone(&store), Arc::clone(&bus))
            .with_interval(Duration::from_millis(20))
            .spawn();

        // Give the watcher a chance to record its starting cursor before
        // we append — otherwise the test races against the initial
        // `current_global_seq()` call.
        sleep(Duration::from_millis(50)).await;

        // Imitate a different process by appending directly to the store,
        // bypassing the local bus entirely.
        let mut ev = make_event("external");
        store.append(&mut ev).await.unwrap();

        let received = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("watcher should deliver event")
            .expect("subscriber channel open");
        assert_eq!(received.stream_id, "external");

        watcher.stop().await;
    }

    #[tokio::test]
    async fn skips_existing_events_on_start() {
        let (_dir, store, bus) = build().await;

        // Pre-existing event before watcher starts.
        let mut pre = make_event("pre");
        store.append(&mut pre).await.unwrap();

        let mut rx = bus.subscribe().await;
        let watcher = StoreTailWatcher::new(Arc::clone(&store), Arc::clone(&bus))
            .with_interval(Duration::from_millis(20))
            .spawn();

        // Give the watcher time to do at least one poll cycle.
        sleep(Duration::from_millis(100)).await;
        assert!(
            rx.try_recv().is_err(),
            "pre-existing events must not be redelivered"
        );

        // New event after watcher started.
        let mut after = make_event("after");
        store.append(&mut after).await.unwrap();

        let received = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("watcher should deliver new event")
            .expect("channel open");
        assert_eq!(received.stream_id, "after");

        watcher.stop().await;
    }

    #[tokio::test]
    async fn redeliver_does_not_persist_again() {
        let (_dir, store, bus) = build().await;

        let mut ev = make_event("once");
        store.append(&mut ev).await.unwrap();
        assert_eq!(store.current_global_seq().await.unwrap(), 1);

        // Manually redeliver to simulate watcher behavior.
        bus.redeliver(Arc::new(ev)).await;
        assert_eq!(
            store.current_global_seq().await.unwrap(),
            1,
            "redeliver must not append a second copy"
        );
    }

    #[tokio::test]
    async fn stops_on_handle_stop() {
        let (_dir, store, bus) = build().await;
        let watcher = StoreTailWatcher::new(Arc::clone(&store), Arc::clone(&bus))
            .with_interval(Duration::from_millis(20))
            .spawn();
        watcher.stop().await;
        // After stop returns, no new events should be delivered.
        let mut rx = bus.subscribe().await;
        let mut ev = make_event("after-stop");
        store.append(&mut ev).await.unwrap();
        sleep(Duration::from_millis(100)).await;
        assert!(rx.try_recv().is_err());
    }
}
