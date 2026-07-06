# Hash-chain verification: legacy exemption and honest scope

Date: 2026-07-06

Context:

An external reviewer (arikusi, from HN) read the event store in depth and
flagged three things about the per-stream hash chain:

1. `docs/events-and-replay.md` claimed the chain used **BLAKE3**, but the
   implementation is **SHA-256** (`compute_hash` in `sqlite.rs` / `postgres.rs`).
   In the one document whose whole point is auditability, a wrong crypto claim
   is the first thing a skeptical reader finds.
2. `zymi verify` reports a **mismatch on every stream created before the hash
   feature existed**. The migration added `prev_hash`/`hash` with `DEFAULT ''`
   and never backfilled, so legacy rows have an empty stored `hash`; verify
   recomputes a non-empty hash for them and declares the stream broken. Legacy
   stores are permanently "broken" as far as verify is concerned.
3. The chain **does not detect truncation**. Deleting the tail of a stream, or
   a whole stream, still passes `verify_chain`, because verification walks
   whatever rows remain with no externally-anchored head to compare against.
   (Related, deferred to a later slice: the `sequence` column is not covered by
   the hash — events are serialized before the DB assigns the sequence, so the
   authoritative ordering lives in an unhashed column.)

This ADR covers the decisions taken in the 0.7.3 hardening slice for (1) and
(2), and records the direction for (3).

Decision:

- **Doc fix (1):** state the real construction —
  `hash = SHA-256(event_id || payload || prev_hash)`, chained per stream — and
  correct the stale "ADR-0009 (event sourcing)" reference (0009 is the webhook
  approval handler; there is no single event-sourcing ADR).

- **Legacy rows are exempted, not backfilled (2):** `verify_chain` now returns
  a `ChainVerification { verified, legacy }` instead of a bare count. A row with
  an empty stored `hash` is counted as `legacy` and skipped; it leaves the
  running `expected_prev_hash` unchanged (empty), so the first genuinely-hashed
  event chains from a clean root. `zymi verify` reports legacy events as
  "exempt" rather than failing the stream.

  Backfilling legacy hashes was explicitly **rejected**: computing hashes over
  rows now and writing them back would sign history that was never actually
  chained, converting an *unverifiable* prefix into a *falsely-trusted* one.
  Exemption is the honest state — "we cannot vouch for these" beats "we vouch
  for these" when we can't.

- **Honest scope in the docs (3):** `verify` is documented as
  *modification*-evidence (in-place edits and reordering of hashed events are
  caught), **not** *completeness*-evidence (truncation is not yet caught). The
  Python `verify_chain` binding keeps returning an `int` (the verified count)
  so the pip API is unchanged.

Consequences:

- Pros: legacy stores stop false-alarming; the auditability docs match the
  code; the trust claim is now precise rather than aspirational. The
  `ChainVerification` return type gives callers the verified/legacy split
  without another query.
- Cons: an empty stored `hash` is treated as "legacy" unconditionally, so an
  attacker who can already write the DB could blank a hash to get a row
  exempted. This is acceptable for 0.7.3 — an attacker with write access to the
  append-only log defeats a bare hash chain anyway — and is exactly what the
  next slice closes.
- Next (0.8.0): cover the `sequence` in the hash and add an **anchored head**
  (a per-stream head commitment checked on verify) so truncation and
  whole-stream deletion become detectable. That is a hash-format change and
  will land as a breaking minor release, tracked as a follow-up to this ADR.
