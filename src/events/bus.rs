use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex, RwLock};
use uuid::Uuid;

use super::store::EventStore;
use super::{Event, EventStoreError};

/// Default capacity for subscriber channels. Provides backpressure:
/// if a subscriber falls behind by this many events, new sends will fail
/// (the event is still persisted in the store — subscriber can replay later).
const DEFAULT_CHANNEL_CAPACITY: usize = 256;

/// Capacity of the bounded ring tracking event ids fanned out by this
/// process via [`EventBus::publish`]. The cross-process
/// [`StoreTailWatcher`](super::store::StoreTailWatcher) consults the ring
/// before [`EventBus::redeliver`] to drop redeliveries of our own writes —
/// otherwise every locally-published event arrives twice (once from
/// `publish`'s direct fan-out, once from the watcher polling its own row).
///
/// The ring is large enough to cover a full burst window
/// (`max_catchup_batches × batch_size = 16 × 256 = 4096` by default) plus
/// a comfortable cushion: by the time an id falls off, the watcher has
/// already advanced its cursor past it and won't ask again. If it does
/// (because of a misconfigured very-aggressive policy), the worst case is
/// a single duplicate per affected event — the same failure mode we had
/// before the ring existed, not a regression.
const RECENT_LOCAL_CAPACITY: usize = 8192;

/// A subscriber with an optional server-side filter.
/// When `correlation_filter` is set, only events with a matching correlation_id
/// are delivered — avoiding the clone+send+discard cycle for irrelevant events.
struct Subscriber {
    tx: mpsc::Sender<Arc<Event>>,
    correlation_filter: Option<Uuid>,
}

impl Subscriber {
    fn accepts(&self, event: &Event) -> bool {
        match self.correlation_filter {
            None => true,
            Some(corr) => event.correlation_id == Some(corr),
        }
    }

    fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// In-process event bus. Persists events to the store, then fans out to subscribers.
///
/// Backpressure: subscriber channels are bounded. If a subscriber's buffer is full,
/// the event is dropped for that subscriber (but always persisted in the store).
/// Subscribers that fall behind can recover by replaying from the store.
pub struct EventBus {
    store: Arc<dyn EventStore>,
    subscribers: RwLock<Vec<Subscriber>>,
    /// Bounded ring of event ids this process has fanned out via
    /// [`Self::publish`]. Consulted by [`Self::redeliver`] to skip
    /// redeliveries of our own writes (see [`RECENT_LOCAL_CAPACITY`]).
    recent_local: Mutex<RecentLocal>,
}

#[derive(Default)]
struct RecentLocal {
    set: HashSet<Uuid>,
    queue: VecDeque<Uuid>,
}

impl RecentLocal {
    fn record(&mut self, id: Uuid) {
        if self.set.insert(id) {
            self.queue.push_back(id);
            if self.queue.len() > RECENT_LOCAL_CAPACITY {
                if let Some(evicted) = self.queue.pop_front() {
                    self.set.remove(&evicted);
                }
            }
        }
    }

    fn contains(&self, id: &Uuid) -> bool {
        self.set.contains(id)
    }
}

impl EventBus {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self {
            store,
            subscribers: RwLock::new(Vec::new()),
            recent_local: Mutex::new(RecentLocal::default()),
        }
    }

    /// Persist the event to the store (source of truth), then deliver to all subscribers.
    /// The event's sequence number is assigned by the store.
    ///
    /// Events are always persisted even if no subscriber is available or all buffers are full.
    /// Subscribers receive `Arc<Event>` — one allocation regardless of subscriber count.
    pub async fn publish(&self, mut event: Event) -> Result<(), EventStoreError> {
        let tag = event.kind_tag();
        self.store.append(&mut event).await?;

        // Record the id *before* fan-out so a near-instantaneous watcher
        // poll can never observe the row before we've marked it as ours.
        self.recent_local.lock().await.record(event.id);

        let subs = self.subscribers.read().await;
        if subs.is_empty() {
            return Ok(());
        }

        let shared = Arc::new(event);
        let mut delivered = 0usize;
        let mut dropped = 0usize;
        for sub in subs.iter() {
            if !sub.accepts(&shared) {
                continue;
            }
            match sub.tx.try_send(Arc::clone(&shared)) {
                Ok(()) => delivered += 1,
                Err(_) => dropped += 1,
            }
        }
        if dropped > 0 {
            log::warn!(
                "EventBus: {tag} delivered to {delivered}/{} subscribers ({dropped} dropped)",
                subs.len()
            );
        }
        Ok(())
    }

    /// Deliver an already-persisted event to local subscribers **without** touching
    /// the store. Used by [`StoreTailWatcher`](super::store::StoreTailWatcher) to
    /// fan out events produced by other processes.
    ///
    /// Calling `publish` here would re-append the event, breaking the hash chain
    /// and producing a duplicate global cursor entry — `redeliver` is the safe path.
    ///
    /// **Same-process dedup.** The watcher polls *all* events past its
    /// cursor, including the ones we just published locally via
    /// [`Self::publish`]. Without dedup, every same-process event would
    /// be delivered to subscribers twice (once direct, once via the
    /// watcher's redelivery). [`Self::publish`] records every fanned-out
    /// id in a bounded ring; `redeliver` checks the ring and drops local
    /// echoes silently.
    pub async fn redeliver(&self, event: Arc<Event>) {
        if self.recent_local.lock().await.contains(&event.id) {
            return;
        }
        let subs = self.subscribers.read().await;
        if subs.is_empty() {
            return;
        }
        let tag = event.kind_tag();
        let mut delivered = 0usize;
        let mut dropped = 0usize;
        for sub in subs.iter() {
            if !sub.accepts(&event) {
                continue;
            }
            match sub.tx.try_send(Arc::clone(&event)) {
                Ok(()) => delivered += 1,
                Err(_) => dropped += 1,
            }
        }
        if dropped > 0 {
            log::warn!(
                "EventBus: {tag} redelivered to {delivered}/{} subscribers ({dropped} dropped)",
                subs.len()
            );
        }
    }

    /// Subscribe to all events. Returns a bounded receiver.
    ///
    /// If the subscriber falls behind by more than `DEFAULT_CHANNEL_CAPACITY` events,
    /// newer events will be dropped for this subscriber. The subscriber can recover
    /// by reading from the store directly.
    pub async fn subscribe(&self) -> mpsc::Receiver<Arc<Event>> {
        self.subscribe_with_capacity(DEFAULT_CHANNEL_CAPACITY).await
    }

    /// Subscribe with a custom channel capacity.
    pub async fn subscribe_with_capacity(&self, capacity: usize) -> mpsc::Receiver<Arc<Event>> {
        let (tx, rx) = mpsc::channel(capacity);
        let mut subs = self.subscribers.write().await;
        subs.retain(|s| !s.is_closed());
        subs.push(Subscriber { tx, correlation_filter: None });
        rx
    }

    /// Subscribe to events matching a specific correlation_id only.
    /// The bus filters server-side — the subscriber never sees irrelevant events.
    /// Uses a small channel (4) since only a handful of events match a single correlation.
    pub async fn subscribe_correlation(&self, correlation_id: Uuid) -> mpsc::Receiver<Arc<Event>> {
        let (tx, rx) = mpsc::channel(4);
        let mut subs = self.subscribers.write().await;
        subs.retain(|s| !s.is_closed());
        subs.push(Subscriber { tx, correlation_filter: Some(correlation_id) });
        rx
    }

    /// Access the underlying store for replay/queries.
    pub fn store(&self) -> &Arc<dyn EventStore> {
        &self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use crate::events::store::SqliteEventStore;
    use crate::events::EventKind;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, Arc<EventBus>) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_bus.db");
        let store = Arc::new(SqliteEventStore::new(&db_path).unwrap());
        let bus = Arc::new(EventBus::new(store));
        (dir, bus)
    }

    #[tokio::test]
    async fn publish_persists_and_delivers() {
        let (_dir, bus) = setup().await;
        let mut rx = bus.subscribe().await;

        let event = Event::new(
            "s1".into(),
            EventKind::UserMessageReceived {
                content: Message::User("hello".into()),
                connector: "test".into(),
            },
            "test".into(),
        );

        bus.publish(event).await.unwrap();

        let received = rx.try_recv().unwrap();
        assert_eq!(received.kind_tag(), "user_message_received");
        assert_eq!(received.sequence, 1);

        let stored = bus.store().read_stream("s1", 1).await.unwrap();
        assert_eq!(stored.len(), 1);
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive() {
        let (_dir, bus) = setup().await;
        let mut rx1 = bus.subscribe().await;
        let mut rx2 = bus.subscribe().await;
        let mut rx3 = bus.subscribe().await;

        let event = Event::new(
            "s1".into(),
            EventKind::AgentProcessingStarted {
                conversation_id: "s1".into(),
            },
            "agent".into(),
        );

        bus.publish(event).await.unwrap();

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
        assert!(rx3.try_recv().is_ok());
    }

    #[tokio::test]
    async fn dropped_subscriber_does_not_block() {
        let (_dir, bus) = setup().await;
        let rx1 = bus.subscribe().await;
        let mut rx2 = bus.subscribe().await;

        drop(rx1);

        let event = Event::new(
            "s1".into(),
            EventKind::ResponseReady {
                conversation_id: "s1".into(),
                content: "done".into(),
            },
            "agent".into(),
        );

        bus.publish(event).await.unwrap();
        assert!(rx2.try_recv().is_ok());
    }

    #[tokio::test]
    async fn dead_subscribers_cleaned_on_next_subscribe() {
        let (_dir, bus) = setup().await;
        let rx1 = bus.subscribe().await;
        let _rx2 = bus.subscribe().await;

        drop(rx1);

        let _rx3 = bus.subscribe().await;

        let subs = bus.subscribers.read().await;
        assert_eq!(subs.len(), 2);
    }

    #[tokio::test]
    async fn events_arrive_in_order() {
        let (_dir, bus) = setup().await;
        let mut rx = bus.subscribe().await;

        for i in 0..5 {
            let event = Event::new(
                "s1".into(),
                EventKind::LlmCallStarted { iteration: i, message_count: 0, approx_context_chars: 0 },
                "agent".into(),
            );
            bus.publish(event).await.unwrap();
        }

        for i in 0..5 {
            let received = rx.try_recv().unwrap();
            if let EventKind::LlmCallStarted { iteration, .. } = &received.kind {
                assert_eq!(*iteration, i);
            } else {
                panic!("unexpected event kind");
            }
            assert_eq!(received.sequence, (i + 1) as u64);
        }
    }

    #[tokio::test]
    async fn redeliver_skips_locally_published_events() {
        // Regression for `zymi serve` double-delivery: the in-process tail
        // watcher polls the store and calls `redeliver` on rows it sees,
        // including the ones this process just appended via `publish`.
        // Subscribers must not see those events twice.
        let (_dir, bus) = setup().await;
        let mut rx = bus.subscribe().await;

        let event = Event::new(
            "s1".into(),
            EventKind::ResponseReady {
                conversation_id: "s1".into(),
                content: "hi".into(),
            },
            "agent".into(),
        );
        bus.publish(event.clone()).await.unwrap();
        let first = rx.try_recv().expect("publish delivers locally");

        // Now imitate the watcher seeing the row it just polled. With the
        // dedup ring this must be a no-op.
        bus.redeliver(first.clone()).await;
        assert!(
            rx.try_recv().is_err(),
            "redeliver of a locally-published event must be silently dropped"
        );
    }

    #[tokio::test]
    async fn redeliver_still_fires_for_external_events() {
        // Counterpart: events that did NOT originate from this process's
        // `publish` (i.e. were appended by a different process) must still
        // be redelivered — the dedup ring is keyed by id, and a foreign
        // id is not in the ring.
        let (_dir, bus) = setup().await;
        let mut rx = bus.subscribe().await;

        let foreign = Event::new(
            "s1".into(),
            EventKind::ResponseReady {
                conversation_id: "s1".into(),
                content: "from-elsewhere".into(),
            },
            "external".into(),
        );
        // Note: not `bus.publish`, so the ring stays empty for this id.
        bus.redeliver(Arc::new(foreign)).await;
        assert!(
            rx.try_recv().is_ok(),
            "redeliver of a foreign event must reach subscribers"
        );
    }

    #[tokio::test]
    async fn backpressure_drops_events_for_slow_subscriber() {
        let (_dir, bus) = setup().await;
        let mut rx = bus.subscribe_with_capacity(2).await;

        for i in 0..5 {
            let event = Event::new(
                "s1".into(),
                EventKind::LlmCallStarted { iteration: i, message_count: 0, approx_context_chars: 0 },
                "agent".into(),
            );
            bus.publish(event).await.unwrap();
        }

        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());

        let stored = bus.store().read_stream("s1", 1).await.unwrap();
        assert_eq!(stored.len(), 5);
    }
}
