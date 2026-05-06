# Connectors and outputs

Connectors are inbound event sources; outputs are outbound event sinks. Both are declarative — configured under `connectors:` / `outputs:` in `project.yml`. No code, no rebuild.

## Overview

A connector listens for some external trigger (HTTP webhook, scheduled tick, polled API, file content, stdin), turns it into a `UserMessageReceived` event, and optionally pairs it with a `PipelineRequested` event so `zymi serve <pipeline>` reacts.

An output watches the bus for events matching a filter (by kind, by stream prefix) and pushes them to an external sink (HTTP POST, file append, stdout).

The full set of types lives under `src/connectors/` (ADR-0021).

## Inbound connectors

### `http_inbound` — webhook receiver {#http-inbound}

```yaml
connectors:
  - type: http_inbound
    name: github_webhook
    host: 0.0.0.0                     # default 127.0.0.1
    port: 8080                        # required
    path: /events                     # default /
    auth:                             # optional. None | bearer | hmac
      type: hmac
      header: X-Hub-Signature-256
      secret: ${env.GITHUB_WEBHOOK_SECRET}
      prefix: "sha256="
    extract:
      stream_id: "$.repository.full_name"   # JSONPath into request body
      content:   "$.head_commit.message"
      user:      "$.pusher.name"            # optional
      correlation_id: "$.delivery"          # optional
    pipeline: handle_event              # optional. Triggers `zymi serve handle_event`.
    pipeline_input: message             # default "message"
```

Auth modes: `none` (default), `bearer { token }`, `hmac { header, secret, prefix? }`.

### `http_poll` — long-polling external API {#http-poll}

For APIs that don't push (e.g., Telegram `getUpdates`, IMAP, paged REST endpoints).

```yaml
connectors:
  - type: http_poll
    name: telegram
    url: "https://api.telegram.org/bot${env.TELEGRAM_BOT_TOKEN}/getUpdates"
    method: GET                        # default GET
    interval_secs: 2                   # default 5
    headers:
      Accept: "application/json"
    extract:
      items:     "$.result[*]"         # iterate this JSONPath as a list
      stream_id: "$.message.chat.id"
      content:   "$.message.text"
      user:      "$.message.from.username"
    cursor:                            # optional. Persists last-seen across restarts.
      param: offset                    # query string parameter to send on the next request
      from_item: "$.update_id"         # JSONPath on each item
      plus_one: true                   # add 1 before persisting (Telegram requires this)
      persist: true                    # default true
    filter:                            # optional. Drop items that don't match.
      "$.message.from.username":
        one_of: ["alice", "bob"]
    pipeline: chat
    pipeline_input: message
```

Cursors live in `.zymi/connectors.db` (sqlite default) or in the `connector_cursors` Postgres table (when `store: postgres://…`). Multi-process `zymi serve` against shared Postgres sees one cursor — no double-fire.

### `cron` — schedule {#cron}

```yaml
connectors:
  - type: cron
    name: morning_digest
    at: "09:00"                        # daily at HH:MM (local time). Or:
    # every_secs: 3600                 # ...fixed interval, mutually exclusive.
    content: "Generate today's digest" # required. The synthetic message text.
    pipeline: digest                   # optional. Pairs with PipelineRequested.
    pipeline_input: message
```

Restart-naive: no cursor. A missed tick during downtime is not replayed.

### `file_read` — read a file once at startup

```yaml
connectors:
  - type: file_read
    name: seed
    path: ./seed.txt
    mode: lines                        # one event per line. Or:
    # mode: whole                      # one event with the whole file content.
    pipeline: process
    pipeline_input: message
```

Reads once and exits. No tail / inotify (use a real connector if you need that).

### `stdin` — line-delimited stdin

```yaml
connectors:
  - type: stdin
    name: cli_input
    pipeline: chat
    pipeline_input: message
```

One event per non-empty line until EOF.

## Outbound outputs

### `http_post` — POST on event match {#http-post}

```yaml
outputs:
  - type: http_post
    name: telegram_reply
    on: [ResponseReady]                # event-kind filter (single or list)
    url: "https://api.telegram.org/bot${env.TELEGRAM_BOT_TOKEN}/sendMessage"
    method: POST                       # default POST
    headers:
      Content-Type: "application/json"
    body_template: |
      {
        "chat_id": "{{ event.stream_id }}",
        "text": {{ event.content | tojson }}
      }
    retry:                             # optional
      attempts: 3                      # default 3
      backoff_secs: [1, 5, 30]         # default [1, 5, 30]
```

`body_template` is **MiniJinja** (full Jinja2 syntax, `tojson` filter is provided). Each successful POST emits an `OutboundDispatched` event; failures emit `OutboundFailed`. Both show up in `zymi events`.

### `file_append` — append on event match

```yaml
outputs:
  - type: file_append
    name: log
    on: [WorkflowNodeCompleted]
    path: ./run.log
    template: "{{ event.kind_tag }} stream={{ event.stream_id }}"
    newline: true                      # default true
```

### `stdout` — print on event match

```yaml
outputs:
  - type: stdout
    name: console
    on: [ResponseReady]
    template: "{{ event.content }}"
    newline: true
```

## Gotchas

- **`stream_id`, `content`, `correlation_id` are JSONPath expressions** — written like `$.message.chat.id`, not `message.chat.id`.
- **Cursors persist by default** — but only with `cursor:` declared. Without it, every poll re-emits everything (bus dedup ring saves you for in-flight events but not on restart).
- **`pipeline:` is optional.** Without it, the connector still publishes `UserMessageReceived` to the bus — useful when the consumer is something other than a pipeline (a downstream output, a logger, etc.).
- **`http_post.body_template` uses MiniJinja `{{ … }}` syntax**, not the `${…}` form used elsewhere in zymi YAML. They're different template engines on purpose: `${…}` for parse-time YAML interpolation, `{{ … }}` for per-event runtime templating.
- **`file_append` requires the parent directory to exist.** Strict-by-design — auto-mkdir would mask bugs.

## See also

- [Project YAML](project-yaml.md) — `connectors:` / `outputs:` / `default_approval_channel:` placement
- [Approvals](approvals.md) — telegram channel reuses `http_inbound` + `http_post` under the hood
- [Store backends](store-backends.md) — cursor persistence behaviour with sqlite vs postgres
- ADR-0021 (connector and plugin protocol)
