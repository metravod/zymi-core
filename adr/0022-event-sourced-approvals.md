# Event-Sourced Approvals

Date: 2026-04-19

## Context

Human-in-the-loop approvals today go through the `ApprovalHandler` trait (ADR 0009), with two concrete implementations: a terminal prompt (default for interactive CLI) and `WebhookApprovalHandler` (an axum server keyed by the `webhook` feature flag).

Two problems with the current shape:

1. **Not reachable from YAML.** The approval mode is selected via `--approval=terminal|webhook` CLI flag, with `--approval-bind` / `--approval-callback` arguments. Users cannot declare "this pipeline requires Slack approval" in `project.yml`. With ADR 0020 committing to declarative-first, this gap becomes a primary correctness issue, not a nice-to-have.
2. **Ad-hoc in-memory state.** `WebhookApprovalHandler::pending` is a `HashMap<String, PendingApproval>` held in a `Mutex` inside the handler struct (`src/webhook.rs:26-28`). Pending approvals do not survive process restart, are not visible to the TUI (ADR 0017), and are not part of the hash-chain audit trail (ADR 0016). For a framework whose selling point is "event-sourced, auditable agents", hiding approvals outside the event store is inconsistent.

At the same time, approvals are exactly the kind of plugin category ADR 0020 describes: several concrete channels (terminal, HTTP, Slack, future Telegram / Discord / email), each selected by `type:` in YAML, each implementing the same small trait.

This ADR combines both fixes: move approvals onto the event bus (fixing #2 and unlocking cross-process / TUI / audit visibility for free) and expose them through a `approvals:` YAML section with a plugin registry (fixing #1).

## Decision

### New event kinds

`EventKind` gains three variants, all marked non-exhaustive-compatible:

- `ApprovalRequested { approval_id, stream_id, description, explanation, channel }`
- `ApprovalGranted   { approval_id, stream_id, decided_by, reason }`
- `ApprovalDenied    { approval_id, stream_id, decided_by, reason }`

`approval_id` is a UUID generated at request time. `channel` names the approval plugin that is expected to pick the request up (by the `name:` field of a `approvals:` entry). `decided_by` is a free-form string the channel fills in (e.g. `"slack:@alice"`, `"terminal"`, `"http:api-key-fp-9c2f"`).

These events land in the same sqlite store as every other event. They are covered by the hash-chain audit (ADR 0016) and visible in the TUI (ADR 0017) without additional plumbing.

### Runtime flow

When the orchestrator needs an approval:

1. Publish `ApprovalRequested` on the bus with a generated `approval_id` and the configured `channel` (derived from YAML — per-action policy, pipeline default, or global default, in that order).
2. Subscribe filtered by `approval_id` to the bus, awaiting `ApprovalGranted` / `ApprovalDenied`.
3. Apply the configured per-request timeout (default: denied on timeout).

The `ApprovalHandler` trait collapses into a thin shim that does exactly the publish-and-wait; the orchestrator never talks to a plugin directly.

### Approval plugins

An `approvals:` YAML section registers channels:

```yaml
approvals:
  - type: terminal
    name: local
    # default when running interactively

  - type: http
    name: ops_api
    bind: "0.0.0.0:8081"
    auth:
      bearer: ${env.APPROVAL_TOKEN}
    notify:
      callback_url: ${env.APPROVAL_CALLBACK}    # optional push

  - type: slack
    name: ops_channel
    webhook_url: ${env.SLACK_APPROVALS_WEBHOOK}
    channel: "#agent-approvals"
    timeout_secs: 600
```

Each `type:` implements `ApprovalChannel`:

```rust
trait ApprovalChannel: Send + Sync {
    async fn start(&self, bus: Arc<EventBus>) -> Result<()>;
    async fn shutdown(&self) -> Result<()>;
}
```

Start-up subscribes the plugin to the bus filtered by `channel == self.name`. When an `ApprovalRequested` arrives for this plugin, it reaches out to its human surface (render prompt, POST to Slack, light up HTTP endpoint). When the human replies, the plugin publishes `ApprovalGranted` / `ApprovalDenied` with the same `approval_id`.

### Which channel handles which request

Resolution order (first match wins):

1. Per-action override in the agent / policy config (future — not in v1).
2. Pipeline-level default: `pipeline.approval_channel: slack.ops_channel`.
3. Project-level default: top-level `default_approval_channel: terminal.local`.

Until per-action overrides ship, v1 supports levels 2 and 3. If no match: fail-closed — the orchestrator treats the intention as denied and logs a `policy.approval.misconfigured` error event.

### Restart safety

On runtime startup:

1. Replay `events` where no matching `ApprovalGranted` / `ApprovalDenied` exists.
2. For each such `ApprovalRequested`, if configured timeout has elapsed, synthesise an `ApprovalDenied { reason: "restart_timeout" }` and publish it.
3. Otherwise, re-subscribe (plugin is already active from startup) so late decisions still land correctly.

No in-memory state required on the plugin side, and no "lost approvals" on crash mid-flight.

### Terminal channel

The terminal channel remains the default in the interactive CLI. Its implementation is a plugin that renders a prompt on stdin/stderr and publishes the decision. It is wired unconditionally when `approvals:` is absent from YAML and a terminal is attached. This preserves the current zero-config experience.

### HTTP channel

`type: http` replaces what `WebhookApprovalHandler` used to be. It reuses `src/webhook.rs` but rewires:

- No `pending: HashMap` — the source of truth is the event store.
- `GET /approvals` queries the store for unfulfilled `ApprovalRequested` events filtered to this channel, not an in-memory map.
- `POST /approvals/:id` validates auth, then publishes `ApprovalGranted` / `ApprovalDenied`. The orchestrator picks it up via the normal bus subscription.
- Notification `callback_url` is a thin side-effect on publish, unchanged.

The existing axum routes are preserved for backward compatibility with any consumer that already points at them.

### Slack channel (v1 target)

Posts an Interactive Message with `approve` / `deny` buttons to the configured Slack webhook when `ApprovalRequested` arrives. Button actions POST back to a small receiver endpoint that publishes `ApprovalGranted` / `ApprovalDenied`. `decided_by` records the Slack user id.

Implementation ships behind feature flag `approval-slack`. Estimated ~300 LOC.

## Alternatives Considered

- **Keep `ApprovalHandler` as a direct in-process trait, just add YAML dispatch.** Cheaper to implement, but leaves the restart-unsafe in-memory state, the TUI blind spot, and the audit gap in place — all of which are the actual complaints.
- **Store approvals in a separate table, not as events.** Avoids growing `EventKind`, but breaks the "event store is the single source of truth" principle (ADR 0016). Diverges audit-trail guarantees between "agent actions" and "human decisions", which is the opposite of what an auditable framework should do.
- **Separate approval bus.** A second bus for human decisions, to keep `EventKind` focused on agent events. Rejected: doubles infrastructure (second sqlite table, second TUI pane, second replay path) for a marginal conceptual cleanliness gain. Approvals are causally interleaved with agent events anyway.
- **Synchronous RPC to plugins (no bus).** The current design. Rejected: every problem listed in Context traces back to this choice.

## Consequences

**Pros**

- Approvals become first-class citizens of the event store: visible in the TUI, in `zymi runs`, in exported trace data (ADR 0017, 0020), in the hash-chain audit (ADR 0016).
- Restart safety is structural, not a bolt-on.
- Adding a new approval channel = one impl of `ApprovalChannel` + one `type:` entry in the registry. Slack, Discord, Telegram, email are now incremental work, not architecture work.
- Users can observe approval SLAs (time between request and decision) using the same analytics pipeline as other event latency metrics.
- Non-interactive use cases (CI, scheduled runs) are supported out of the box by pointing the project at `type: http` or `type: slack` in YAML; no CLI-flag gymnastics.

**Cons**

- `EventKind` grows three variants. Any exhaustive match over `EventKind` in the codebase needs a new arm. Mitigated by the `#[non_exhaustive]` direction from ADR 0020 — downstream consumers are not broken, internal sites are handled once.
- The v1 Slack channel relies on an outbound webhook + a small receiver endpoint bound to a port. Users behind NAT without a public endpoint cannot use `type: slack` directly; they are steered to `type: http` with a reverse-tunnel or to a future event-only Slack integration.
- Replay-based restart recovery means startup cost scales with the number of unfinished approvals. In practice this is bounded by per-approval timeouts; a runaway is caught by the `policy.approval.misconfigured` fail-closed path.

**Supersession**

This ADR supersedes ADR 0009 (`WebhookApprovalHandler` as a standalone code-level component). The struct remains in the tree, but reframed as "the `type: http` approval plugin under the registry introduced here". The documentation in ADR 0009 is historical.
