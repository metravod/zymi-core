# Approvals

Event-sourced human-in-the-loop. A tool with `requires_approval: true` publishes an `ApprovalRequested` event; an approval channel routes the prompt to a human; the human's decision lands as `ApprovalGranted` or `ApprovalDenied` on the bus. Restart-safe by construction.

## Overview

Approvals live entirely on the event bus (ADR-0022). No in-memory pending map, no opaque handler state — the event store is the single source of truth for "who is waiting on what". This means:

- Restart safety: a `zymi serve` restart re-subscribes to in-flight approvals from the store (within timeout) or seals them as `ApprovalDenied{reason: restart_timeout}`.
- TUI / `zymi events` see every approval decision the same way they see any other event.
- Hash-chained: decisions are tamper-evident.

Three channel types ship in the box: `terminal`, `http`, `telegram`. New channels plug in via the same registry as connectors.

## Resolution order

When a tool with `requires_approval: true` fires, zymi picks a channel by:

1. Pipeline-level `approval_channel:` (in `pipelines/<name>.yml`), if set.
2. Project-level `default_approval_channel:` (in `project.yml`), if set.
3. Auto-spawn a terminal channel — only when no `approvals:` is configured AND stdin is attached (zero-config UX for `zymi run`).
4. Fail closed (`ApprovalDenied{reason: no_channel}`) — when none of the above apply.

## Schema

```yaml
# project.yml
default_approval_channel: ops_tg     # optional. Channel name to use by default.

approvals:                           # one or more channels.
  - type: terminal | http | telegram
    name: <channel-name>             # referenced by ApprovalRequested.channel
    # ... type-specific fields
```

## Channels

### `terminal` — local prompt

Best for `zymi run` (one-shot CLI). Reads from stdin.

```yaml
approvals:
  - type: terminal
    name: local                       # optional. Defaults to "terminal".
```

Only one terminal prompt is active at a time (serialised lock) so concurrent approvals don't tangle on a shared TTY.

### `http` — REST endpoint {#http}

Best for ops dashboards, Slack bots, anything that wants to drive approvals over HTTP.

```yaml
approvals:
  - type: http
    name: ops_http
    bind: "0.0.0.0:8088"
    bearer_token: ${env.APPROVAL_TOKEN}     # optional. Gates GET/POST.
    callback_url: "https://hooks.slack.com/..."  # optional. POSTed on each ApprovalRequested.
```

Routes:

- `GET /health` — `"ok"` plaintext (always public).
- `GET /approvals` — JSON list of currently-pending requests for this channel.
- `POST /approvals/{id}` — body: `{"approved": bool, "reason"?: "string"}`.

Bearer auth gates `GET /approvals` and `POST /approvals/{id}` when `bearer_token:` is set. `GET /health` is always open.

### `telegram` — bot DM with inline buttons {#telegram-channel}

Best for personal / small-team operations: every `ApprovalRequested` shows up as a DM with ✅ / ❌ inline keyboard buttons.

```yaml
approvals:
  - type: telegram
    name: ops_tg
    bot_token: ${env.TELEGRAM_BOT_TOKEN}
    chat_id: ${env.TELEGRAM_ADMIN_CHAT_ID}    # numeric (use @userinfobot to find yours)
    bind: "127.0.0.1:8088"
    callback_path: /telegram/callback         # default. Must match setWebhook URL.
    secret_token: ${env.TELEGRAM_WEBHOOK_SECRET}  # optional but strongly recommended
```

Telegram needs a public URL for the callback. Two simple ways on a laptop:

```bash
# Option 1: ngrok
ngrok http 8088

# Option 2: cloudflared
cloudflared tunnel --url http://localhost:8088

# Then point the bot at the public URL once (persists on Telegram's side):
curl "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/setWebhook" \
     -d "url=https://<your-tunnel>/telegram/callback" \
     -d "secret_token=${TELEGRAM_WEBHOOK_SECRET}"
```

`secret_token:` is enforced via Telegram's `X-Telegram-Bot-Api-Secret-Token` header. Without it, anyone who guesses the public URL can forge approvals — only safe for local development.

`decided_by:` on the resulting `ApprovalGranted` / `ApprovalDenied` event records `telegram:<from.username>` so the audit trail names the human who clicked.

## Per-pipeline override

```yaml
# pipelines/sensitive.yml
name: sensitive
approval_channel: ops_tg              # overrides project's default for this pipeline
steps:
  - id: act
    agent: operator
    task: "${inputs.task}"
```

## Restart-safe replay

On `zymi serve` startup (and `zymi run` when an approval channel is configured), the runtime walks the event store for `ApprovalRequested` events without a matching `ApprovalGranted`/`ApprovalDenied`:

- **Within timeout:** re-subscribe via `bus.redeliver` so the channel can re-prompt and the orchestrator picks up the eventual decision.
- **Past timeout:** publish `ApprovalDenied{reason: restart_timeout}` and let downstream consumers resolve the stuck call.

This means: a `zymi serve` crash mid-approval leaves no orphaned state. Restart, the approval re-prompts (or expires) cleanly.

## Examples

**Auto-terminal when running interactively:**

```yaml
# project.yml has no `approvals:` section
# Tool requires approval:
# tools/danger.yml:
#   ...
#   requires_approval: true
#   ...
```

`zymi run pipeline_with_danger` opens a terminal prompt automatically.

**Telegram bot for production:**

See [the canonical telegram scaffold](../src/cli/init.rs) (`zymi init --example telegram`) — it ships with a working `approvals:` block plus a gated `tools/broadcast.yml`.

## Gotchas

- **`chat_id` is numeric, not `@username`.** Use `@userinfobot` to find yours; for groups, send a message and inspect `getUpdates`.
- **`callback_path:` must match the URL passed to `setWebhook`.** Mismatched paths silently fail (Telegram POSTs to a 404).
- **`bearer_token:` on the http channel is optional** but strongly recommended for any non-localhost bind.
- **Terminal channels lock stdin globally.** Don't run two `zymi run` processes against terminal in parallel — the prompts will interleave.
- **Approvals are emitted to the bus regardless of channel availability.** Removing `requires_approval: true` doesn't mute the trail; downstream observers still see the unapproved call.

## See also

- [Tools](tools.md) — `requires_approval:` per-tool override and per-tool defaults
- [Project YAML](project-yaml.md) — `approvals:` placement
- [Events and replay](events-and-replay.md) — observing approval decisions
- ADR-0022 (event-sourced approvals)
