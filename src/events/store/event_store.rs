use async_trait::async_trait;

use crate::events::{Event, EventStoreError};

/// An event paired with a backend-assigned monotonic global cursor.
///
/// `global_seq` is **opaque**: only the originating store interprets its value.
/// For [`SqliteEventStore`](super::sqlite::SqliteEventStore) it is the autoincrement `id`,
/// for a future Postgres backend it could be a sequence value, etc.
///
/// Used by [`StoreTailWatcher`](super::watcher::StoreTailWatcher) to deliver new
/// events from the store to in-process subscribers.
#[derive(Debug, Clone)]
pub struct TailedEvent {
    pub global_seq: u64,
    pub event: Event,
}

/// Outcome of verifying one stream's hash chain.
///
/// `verified` counts events whose hash was recomputed and matched. `legacy`
/// counts events written before the hash-chain feature existed (empty stored
/// `hash` column): these cannot be verified and are *exempted* rather than
/// flagged as broken. Backfilling their hashes was rejected on purpose — it
/// would sign history that was never actually chained, turning an unverifiable
/// prefix into a falsely-trusted one. See ADR-0035.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChainVerification {
    /// Events whose hash was recomputed and matched.
    pub verified: u64,
    /// Legacy (pre-hash-chain) events, exempted from verification.
    pub legacy: u64,
}

impl ChainVerification {
    /// Total events walked (verified + legacy).
    pub fn total(&self) -> u64 {
        self.verified + self.legacy
    }
}

/// Append-only event store. Events are immutable once written.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Append an event to its stream. Assigns the next sequence number.
    /// Returns the assigned sequence number.
    async fn append(&self, event: &mut Event) -> Result<u64, EventStoreError>;

    /// Read all events in a stream starting from `from_seq` (inclusive).
    async fn read_stream(
        &self,
        stream_id: &str,
        from_seq: u64,
    ) -> Result<Vec<Event>, EventStoreError>;

    /// Read events across all streams, ordered by insertion (global sequence).
    /// `from_global_seq` is the lower bound (exclusive).
    async fn read_all(
        &self,
        from_global_seq: u64,
        limit: usize,
    ) -> Result<Vec<Event>, EventStoreError>;

    /// Read events with `global_seq` strictly greater than `after_global_seq`,
    /// in monotonic global order, up to `limit`.
    ///
    /// Used by [`StoreTailWatcher`](super::watcher::StoreTailWatcher) to deliver
    /// cross-process events to in-process subscribers without re-persisting them.
    async fn tail(
        &self,
        after_global_seq: u64,
        limit: usize,
    ) -> Result<Vec<TailedEvent>, EventStoreError>;

    /// Current high-water mark of `global_seq`. Returns 0 for an empty store.
    /// The watcher uses this on startup to skip already-existing events.
    async fn current_global_seq(&self) -> Result<u64, EventStoreError>;

    /// Get the last sequence number for a stream, or 0 if empty.
    async fn last_sequence(&self, stream_id: &str) -> Result<u64, EventStoreError>;

    /// Count events in a stream, optionally filtered by kind tag.
    async fn count(
        &self,
        stream_id: &str,
        kind_tag: Option<&str>,
    ) -> Result<u64, EventStoreError>;

    /// Verify the hash chain for a stream. Returns a [`ChainVerification`]
    /// split into verified and exempted-legacy counts, or an error describing
    /// the first broken link.
    async fn verify_chain(&self, stream_id: &str) -> Result<ChainVerification, EventStoreError>;

    /// List all distinct stream IDs with their event counts.
    async fn list_streams(&self) -> Result<Vec<(String, u64)>, EventStoreError>;

    /// Find "orphaned" inbound events: events of `inbound_tag` whose correlation_id
    /// has no corresponding event of `completion_tag` in the store.
    /// Used for recovery after restart.
    async fn find_unmatched(
        &self,
        inbound_tag: &str,
        completion_tag: &str,
    ) -> Result<Vec<Event>, EventStoreError>;

    /// Flush durably-committed events so a *separate* reader process (e.g. a
    /// fresh `zymi events` invocation) sees them. On SQLite in WAL mode the
    /// tail of a run could otherwise stay in the `-wal` file, invisible to a
    /// reader that opened its own connection, until an auto-checkpoint fired
    /// (ADR-0030). The writer calls this once a run reaches a terminal event.
    /// No-op for backends without a write-ahead log (e.g. Postgres).
    async fn checkpoint(&self) -> Result<(), EventStoreError> {
        Ok(())
    }
}
