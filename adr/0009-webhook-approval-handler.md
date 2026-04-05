# Webhook Approval Handler

Date: 2026-04-05

## Context

The ESAA orchestrator supports human approval via the `ApprovalHandler` trait. For automated pipelines (CI, Slack bots, admin dashboards), approvals need an HTTP interface rather than interactive terminal prompts.

## Decision

A `WebhookApprovalHandler` (`src/webhook.rs`) implements `ApprovalHandler` by serving a lightweight HTTP API via axum, gated behind an optional `webhook` Cargo feature.

### Flow

1. Orchestrator calls `request_approval(description, explanation)`.
2. Handler creates a pending entry with a oneshot channel and blocks.
3. If `callback_url` is configured, POSTs an `ApprovalInfo` notification to the external system.
4. External system calls `POST /approvals/:id` with `{"approved": true/false}`.
5. Handler resolves the oneshot, returning the decision to the orchestrator.
6. On timeout: pending entry is cleaned up, defaults to denied.

### HTTP API

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/health` | GET | Health check → `"ok"` |
| `/approvals` | GET | List pending approvals → `[{id, description, explanation, created_at}]` |
| `/approvals/:id` | POST | Decide: `{"approved": bool}` → `{id, approved}` or 404 |

### Starting the server

```rust
let handler = WebhookApprovalHandler::start(
    "0.0.0.0:9090".parse().unwrap(),
    Duration::from_secs(300),    // 5 min timeout per approval
    Some("https://slack-bot.example.com/zymi-approval".into()),
).await?;

// Use as ApprovalHandler with Orchestrator
orchestrator.process_intention(&intention, stream_id, corr_id, Some(handler.as_ref())).await;
```

### Feature flag

```toml
[features]
webhook = ["axum"]
```

Library consumers without webhook needs don't pull in axum.

## Alternatives Considered

- **gRPC approval service**: Over-engineered for a simple approve/deny decision. HTTP JSON is universally accessible.
- **WebSocket for push notifications**: Adds connection management complexity. The callback URL POST achieves push semantics with zero connection state.
- **Embed in CLI binary only**: Would prevent library consumers from using webhook approvals programmatically.

## Consequences

- External systems (Slack bots, dashboards, CI) can approve/deny agent actions via HTTP.
- Timeout defaults to denial — fail-safe for unattended approvals.
- Callback URL enables push notifications without requiring the external system to poll.
- The handler is testable without a real HTTP server (oneshot channel pattern).
- axum adds ~6 transitive crates but shares hyper/tower with reqwest.
