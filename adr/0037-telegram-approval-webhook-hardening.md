# Telegram approval webhook hardening

Date: 2026-07-06

Context:

arikusi's review found three issues in the Telegram approval callback handler:

1. **Not idempotent.** The handler did not check whether the approval was
   already decided. A double-click published two decisions; a deny followed by
   an approve recorded both. The orchestrator honors whichever it sees first,
   but the audit trail then shows contradictory decisions for one approval.
2. **`secret_token` defaults to `None`** — an unauthenticated endpoint that
   approves any pending action for anyone who can reach it. The doc comment
   said "only safe for local development", but nothing enforced or warned.
3. Each callback scanned the store from global sequence 0 to find the
   originating request.

Decision:

- **Idempotency: first decision wins.** The callback now scans for an existing
  `ApprovalGranted`/`ApprovalDenied` for the same `approval_id` in the same pass
  that locates the originating `ApprovalRequested`. If a decision already
  exists, the callback returns `200 OK` and publishes nothing. This folds into
  the existing scan (no extra pass) — and because decisions are appended after
  the request, the scan no longer early-breaks at the request.

- **Fail closed on unauthenticated public binds.** At `start()`, if
  `secret_token` is `None`: a **loopback** bind logs a loud warning (fine for
  local dev / behind a tunnel), and a **non-loopback** bind is a hard startup
  error. The recommended production topology binds loopback behind a tunnel, so
  this refuses only a bare public bind — exactly the dangerous case.

Consequences:

- Pros: the audit trail can no longer record contradictory decisions for one
  approval; an unauthenticated approver endpoint can't be exposed by accident.
  No config schema change.
- Cons / limitations:
  - The idempotency check is **not atomic**: two truly-simultaneous callbacks
    could both observe "not decided" and both publish. This is acceptable — the
    requester already takes the first decision, so the residual harm is a
    duplicate audit row in a nanosecond race, not a wrong outcome. True
    single-decision enforcement needs a store-level uniqueness constraint on
    (approval_id, decision), deferred.
  - The originating-request lookup still scans the store (no `approval_id`
    index; approval events live on the pipeline stream, not a per-approval
    stream). The scan is now single-pass; an index is a separate optimization,
    not correctness.
  - **Breaking:** an existing deployment that bound a non-loopback address
    without a `secret_token` will now refuse to start until a token is set.
    Intended — that configuration was the vulnerability.
