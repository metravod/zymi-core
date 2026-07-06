use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::events::{Event, EventStoreError};

/// Version tag prefixing a v2 stored hash. v2 (0.8.0+, ADR-0040) covers the
/// DB-assigned `sequence`; v1 (0.7.x) did not and stored a bare hex digest;
/// an empty string is a legacy pre-hash-chain row (ADR-0035).
pub const HASH_V2_PREFIX: &str = "v2:";

/// v1 hash: `SHA-256(event_id || data || prev_hash)`, bare hex. Retained only
/// to verify rows written before ADR-0040.
pub fn compute_hash_v1(event_id: &str, data: &str, prev_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(event_id.as_bytes());
    hasher.update(data.as_bytes());
    hasher.update(prev_hash.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// v2 hash: `SHA-256("v2" || event_id || sequence_le || data || prev_hash)`,
/// stored as `v2:<hex>`. Covers the authoritative DB-assigned sequence so the
/// ordering is no longer an unhashed column (ADR-0040).
pub fn compute_hash_v2(event_id: &str, sequence: u64, data: &str, prev_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"v2");
    hasher.update(event_id.as_bytes());
    hasher.update(sequence.to_le_bytes());
    hasher.update(data.as_bytes());
    hasher.update(prev_hash.as_bytes());
    format!("{HASH_V2_PREFIX}{:x}", hasher.finalize())
}

/// Recompute the expected stored hash for a row, dispatching on the stored
/// hash's version tag. Returns `None` for a legacy (empty) row, which the
/// caller exempts rather than verifies.
pub fn recompute_hash(
    event_id: &str,
    sequence: u64,
    data: &str,
    prev_hash: &str,
    stored_hash: &str,
) -> Option<String> {
    if stored_hash.is_empty() {
        None
    } else if stored_hash.starts_with(HASH_V2_PREFIX) {
        Some(compute_hash_v2(event_id, sequence, data, prev_hash))
    } else {
        Some(compute_hash_v1(event_id, data, prev_hash))
    }
}

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

    /// Stream IDs that have a recorded head (ADR-0040). `zymi verify` unions
    /// these with the event streams so a whole-stream deletion — every row
    /// gone but the head remaining — is still detected. Backends without a
    /// head table return empty.
    async fn head_stream_ids(&self) -> Result<Vec<String>, EventStoreError> {
        Ok(Vec::new())
    }

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
