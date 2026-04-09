//! Cross-process event delivery via store tail polling.
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
//! write). Polling at ~100 ms is more than enough for LLM-bound workloads
//! and avoids dragging in a separate IPC channel just to nudge the watcher.
//! Future Postgres backends will likely use `LISTEN/NOTIFY` instead — that
//! will be a sibling watcher type, not a change to this one. The
//! [`TailWatcherPolicy`] contract (poll interval, batch size, catch-up
//! cap, lag warning) is store-agnostic and applies to any backend that
//! implements [`EventStore::tail`] / [`EventStore::current_global_seq`].
//!
//! ## Hash chain safety
//!
//! The watcher **never** calls `store.append`. Events are read by their
//! global cursor and republished via [`EventBus::redeliver`], which is a
//! pure fan-out — no second persistence, no duplicated rows, no broken
//! per-stream hash chain.
//!
//! ## Backpressure decision (ADR-0012 + ADR-0013 slice 4)
//!
//! Two distinct lag concerns live here, and they have different policies:
//!
//! 1. **Watcher → bus → subscribers.** Subscribers can be slower than the
//!    watcher. We **do not block** the watcher on a slow subscriber: the
//!    underlying [`EventBus::redeliver`] uses `try_send` and drops the
//!    event for that subscriber, logging a warning. The store is the
//!    source of truth (ADR-0012), so a lagging subscriber can recover by
//!    replaying from the store directly. Blocking the watcher on one slow
//!    subscriber would (a) starve every other subscriber on the same bus,
//!    and (b) prevent the cancel signal from being observed in any
//!    reasonable time. The drop policy is the right choice and we
//!    deliberately keep it.
//!
//! 2. **Writer → store → watcher.** The writer can append events faster
//!    than the watcher drains them. The catch-up loop will keep pulling
//!    full batches without sleeping, but **only up to
//!    [`TailWatcherPolicy::max_catchup_batches`] times per cycle**. After
//!    that the watcher yields back to the outer interval — this gives the
//!    cancel signal a chance to fire and prevents the watcher from
//!    monopolising the tokio worker under a write storm. On exit from the
//!    catch-up burst we sample `current_global_seq` once and, if the lag
//!    exceeds [`TailWatcherPolicy::lag_warn_threshold`], emit a single
//!    warn-level log. There is intentionally no "drop oldest" or "buffer"
//!    here — the store already buffers everything, and missed events are
//!    impossible (the cursor is monotonic).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::events::bus::EventBus;

use super::event_store::EventStore;

/// Runtime contract for [`StoreTailWatcher`]: how aggressively to poll, how
/// big a single read batch is, and how the watcher behaves when it falls
/// behind the writer.
///
/// Lives next to the watcher (not in `runtime::`) because every field is a
/// property of *this* polling watcher; future `LISTEN/NOTIFY`-based
/// watchers for Postgres etc. will define their own policy type with
/// different knobs.
#[derive(Debug, Clone)]
pub struct TailWatcherPolicy {
    /// Sleep between catch-up cycles when the watcher is fully drained.
    /// Lower values cut delivery latency at the cost of CPU and store
    /// contention. Default: 100 ms.
    pub poll_interval: Duration,
    /// Max events per [`EventStore::tail`] call. The watcher keeps pulling
    /// without sleeping while batches come back full, up to
    /// `max_catchup_batches` times in a row. Default: 256.
    pub batch_size: usize,
    /// Hard cap on consecutive full batches drained in one cycle before
    /// yielding back to the outer interval. Prevents the watcher from
    /// monopolising the tokio worker — and starving the cancel signal —
    /// under a sustained write storm. Default: 16, i.e. up to
    /// `16 * 256 = 4096` events per burst before a forced yield. For
    /// LLM-bound workloads (10–30 events per pipeline step) this cap is
    /// effectively infinite and never fires; it exists as a safety net,
    /// not as a steady-state limit.
    pub max_catchup_batches: usize,
    /// When `Some(n)`, log a warn on cycle exit if
    /// `current_global_seq - last_seen > n`. `None` disables the check
    /// entirely. Default: `Some(4096)`, deliberately aligned with the
    /// burst cap so a warn means "the cap actually fired and we are still
    /// behind" rather than a transient burst the watcher already absorbed.
    pub lag_warn_threshold: Option<u64>,
}

impl Default for TailWatcherPolicy {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            batch_size: 256,
            max_catchup_batches: 16,
            lag_warn_threshold: Some(4096),
        }
    }
}

/// Polls an [`EventStore`] for events written by other processes and
/// fans them out into a local [`EventBus`].
pub struct StoreTailWatcher {
    store: Arc<dyn EventStore>,
    bus: Arc<EventBus>,
    policy: TailWatcherPolicy,
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
            policy: TailWatcherPolicy::default(),
        }
    }

    /// Replace the entire policy. Prefer this over the per-knob setters
    /// when wiring from a [`crate::runtime::Runtime`] — the runtime owns
    /// one canonical [`TailWatcherPolicy`] and this is the path that
    /// keeps every knob (`poll_interval`, `batch_size`,
    /// `max_catchup_batches`, `lag_warn_threshold`) consistent.
    pub fn with_policy(mut self, policy: TailWatcherPolicy) -> Self {
        self.policy = policy;
        self.policy.batch_size = self.policy.batch_size.max(1);
        self.policy.max_catchup_batches = self.policy.max_catchup_batches.max(1);
        self
    }

    /// Override the polling interval only. Convenience for tests and
    /// embedded callers; production wiring should go through
    /// [`Self::with_policy`].
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.policy.poll_interval = interval;
        self
    }

    /// Override the batch size only. Convenience for tests and embedded
    /// callers; production wiring should go through [`Self::with_policy`].
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.policy.batch_size = batch_size.max(1);
        self
    }

    /// Spawn the polling task. Returns a [`WatcherHandle`] that stops the
    /// watcher when dropped or when [`WatcherHandle::stop`] is called.
    ///
    /// The watcher reads `current_global_seq` once on startup so that
    /// already-existing events are skipped — only events written *after*
    /// this point are republished.
    pub fn spawn(self) -> WatcherHandle {
        let StoreTailWatcher { store, bus, policy } = self;
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
                // Drain up to `max_catchup_batches` consecutive full
                // batches before yielding. The cap prevents the watcher
                // from monopolising the tokio worker (and starving the
                // cancel signal) when the writer is producing faster than
                // we drain. See module-level "Backpressure decision".
                let mut burst_batches: usize = 0;
                let burst_capped = loop {
                    if cancel_rx.try_recv().is_ok() {
                        return;
                    }
                    match store.tail(last_seen, policy.batch_size).await {
                        Ok(batch) => {
                            if batch.is_empty() {
                                break false;
                            }
                            let received = batch.len();
                            for tailed in batch {
                                last_seen = tailed.global_seq;
                                bus.redeliver(Arc::new(tailed.event)).await;
                            }
                            if received < policy.batch_size {
                                break false;
                            }
                            burst_batches += 1;
                            if burst_batches >= policy.max_catchup_batches {
                                break true;
                            }
                            // Full batch under the cap — keep pulling.
                        }
                        Err(e) => {
                            log::warn!(
                                "StoreTailWatcher: tail() failed at cursor {last_seen}: {e}"
                            );
                            break false;
                        }
                    }
                };

                // If the burst hit the cap, the writer is keeping up with
                // (or ahead of) us. Sample the head once and warn if the
                // residual lag exceeds the threshold. Skipped on natural
                // drain so we don't pay for an extra round-trip every
                // idle cycle.
                if burst_capped {
                    if let Some(threshold) = policy.lag_warn_threshold {
                        match store.current_global_seq().await {
                            Ok(head) => {
                                let lag = head.saturating_sub(last_seen);
                                if lag > threshold {
                                    log::warn!(
                                        "StoreTailWatcher: lag={lag} (cursor={last_seen}, head={head}) — \
                                         catch-up cap of {} batches × {} reached, yielding",
                                        policy.max_catchup_batches,
                                        policy.batch_size,
                                    );
                                }
                            }
                            Err(e) => {
                                log::debug!(
                                    "StoreTailWatcher: lag probe failed at cursor {last_seen}: {e}"
                                );
                            }
                        }
                    }
                }

                tokio::select! {
                    _ = &mut cancel_rx => return,
                    _ = tokio::time::sleep(policy.poll_interval) => {}
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
    async fn burst_cap_yields_but_delivers_all_events_in_order() {
        // With max_catchup_batches=2 and batch_size=2, the watcher can
        // drain at most 4 events per cycle before yielding. We pre-write
        // 10 events; the watcher must still deliver all 10 in the right
        // order across multiple cycles, not just the first burst.
        let (_dir, store, bus) = build().await;
        let mut rx = bus.subscribe().await;

        let policy = TailWatcherPolicy {
            poll_interval: Duration::from_millis(20),
            batch_size: 2,
            max_catchup_batches: 2,
            // Disable lag warn for the test so the noise threshold is not
            // tied to the burst cap value (the math would otherwise warn
            // on every cycle while we are catching up).
            lag_warn_threshold: None,
        };

        let watcher = StoreTailWatcher::new(Arc::clone(&store), Arc::clone(&bus))
            .with_policy(policy)
            .spawn();

        // Let the watcher record its starting cursor.
        sleep(Duration::from_millis(20)).await;

        for i in 0..10 {
            let mut ev = make_event(&format!("burst-{i}"));
            store.append(&mut ev).await.unwrap();
        }

        let mut received = Vec::with_capacity(10);
        for _ in 0..10 {
            let ev = timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("watcher should drain across multiple cycles")
                .expect("channel open");
            received.push(ev.stream_id.clone());
        }

        let expected: Vec<String> = (0..10).map(|i| format!("burst-{i}")).collect();
        assert_eq!(
            received, expected,
            "all 10 events must arrive in order across burst-capped cycles"
        );

        watcher.stop().await;
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
