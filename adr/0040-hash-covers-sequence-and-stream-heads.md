# Hash covers sequence; stream heads detect truncation

Date: 2026-07-06

Context:

ADR-0035 fixed the docs and the legacy-row false-failure, but left two deeper
issues arikusi raised open, flagged there as follow-ups:

1. **The `sequence` was not covered by the hash.** Events are serialized before
   the DB assigns the sequence, so every stored JSON blob carries `sequence: 0`
   and the authoritative ordering lived in an unhashed column. Re-ordering rows
   by editing the `sequence` column alone would not break the chain.
2. **Truncation was undetectable.** With no anchored head, deleting a stream's
   tail — or a whole stream — still passed `verify`, which just walked whatever
   rows remained.

Decision:

- **Versioned hash that covers the sequence.** `hash` is now
  `SHA-256("v2" || event_id || sequence_le || data || prev_hash)`, stored as
  `v2:<hex>`. The version tag lets `verify` pick the formula per row:
  - `v2:` prefix → recompute with the sequence-covering formula;
  - bare hex (no prefix) → 0.7.x **v1** row, recompute with the old
    `SHA-256(event_id || data || prev_hash)` — so rows written by 0.7.x still
    verify (the ADR-0035 legacy exemption only covered empty hashes);
  - empty → legacy, exempt (ADR-0035).
  The hash functions live once in `event_store.rs` and are shared by both
  backends, so sqlite and postgres cannot drift.

- **Per-stream head table.** A `stream_heads` table (`stream_id`,
  `last_sequence`, `last_hash`) is upserted in the *same* transaction as each
  append. `verify_chain` compares the walked tail against the head: a head
  ahead of the last row means the tail was truncated; a head with zero
  surviving rows means the whole stream was deleted. `zymi verify` (all
  streams) unions the event streams with `head_stream_ids()` so a fully-deleted
  stream is still checked.

Consequences:

- Pros: ordering is now authenticated; accidental truncation, careless
  `DELETE`, and whole-stream deletion are caught; 0.7.x stores keep verifying;
  no config change; one shared hash implementation.
- Cons / limitations (honest scope — this is variant 1, not full anchoring):
  - **Not tamper-evidence against a DB-write attacker.** The head lives in the
    same database, so an attacker who can write `events` can also update
    `stream_heads` and pass verify. This raises the bar (two places to edit)
    and closes the accidental/careless case, but real adversarial
    tamper-evidence needs an **external, unreachable** anchor (signed heads, or
    heads pushed to an append-only external sink). Deferred until a concrete
    consumer needs it — for local single-host deployments the external
    anchor's guarantee is largely illusory anyway (key and sink sit on the same
    box). Tracked as the "variant 3" follow-up.
  - Truncation detection only applies to streams that have a head, i.e. those
    written by 0.8.0+. Pre-0.8 streams have no head and are checked for
    in-place tampering only, as before.
- Supersedes the "anchored heads are planned" note in ADR-0035 for the
  in-database step; the external-anchor step remains future work.
