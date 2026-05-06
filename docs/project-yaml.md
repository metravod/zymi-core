# Project YAML

`project.yml` is the top-level config of a zymi project. It declares the LLM provider, defaults, security policy, file-write contracts, MCP servers, inbound connectors, outbound outputs, approval channels, and the event store backend.

## Overview

One file per project at the root. zymi-core reads it at startup of every command (`run`, `serve`, `events`, `runs`, `observe`, `verify`). It is **declarative**: no code, no rebuild — edit YAML, restart `zymi serve`, the new behaviour takes effect.

## Schema

```yaml
name: my-project              # required. Logical project name.
schema_version: "1"           # current YAML schema version (ADR-0020).
version: "0.1"                # optional. Free-form project version string.

# --- LLM provider ----------------------------------------------------------
llm:
  provider: openai            # "openai" or any OpenAI-compatible endpoint.
  base_url: https://...       # optional. Override for proxies / local servers.
  model: gpt-4o-mini
  api_key: ${env.OPENAI_API_KEY}

# --- Reusable variables ----------------------------------------------------
variables:
  default_model: gpt-4o-mini  # referenced as ${default_model} elsewhere.

# --- Defaults inherited by agents -----------------------------------------
defaults:
  timeout_secs: 30            # default 30
  max_iterations: 10          # default 10

# --- Shell + tool-call policy ---------------------------------------------
policy:
  enabled: true
  allow: ["echo *", "ls *"]   # glob patterns. `*` matches one path segment.
  deny: ["rm -rf *"]

# --- Boundary contracts (enforced at tool execution) ----------------------
contracts:
  file_write:
    allowed_dirs: ["./output", "./memory"]
    deny_patterns: ["*.env", "*.key", "*.pem"]
  rate_limit:                 # optional. Per-tool rate limiting.
    requests_per_minute: 60

# --- Runtime tuning (optional) --------------------------------------------
runtime:
  shell:                      # persistent shell session, ADR-0015
    timeout_secs: 30
  context:                    # context-window management, ADR-0016
    observation_window: 5
    soft_cap_chars: 80000

# --- MCP servers (subprocess tools, ADR-0023) ------------------------------
mcp_servers:
  - name: fs
    command: [npx, -y, "@modelcontextprotocol/server-filesystem", ./sandbox]
    allow: [read_text_file, write_file, list_directory]
    init_timeout_secs: 15
    call_timeout_secs: 30
    restart:
      max_restarts: 2
      backoff_secs: [1, 5]

# --- Inbound connectors (event sources) -----------------------------------
connectors:
  - type: http_poll           # see docs/connectors.md
    name: telegram
    # ... type-specific fields

# --- Outbound outputs (event sinks) ---------------------------------------
outputs:
  - type: http_post
    name: telegram_reply
    on: [ResponseReady]
    # ... type-specific fields

# --- Approval channels (ADR-0022) -----------------------------------------
default_approval_channel: ops_tg
approvals:
  - type: telegram
    name: ops_tg
    # ... type-specific fields

# --- Event store backend (ADR-0012) ---------------------------------------
store: sqlite                 # default. Or:
# store: sqlite:./custom/path.db
# store: postgres://user:pass@host/db
# store: ${env.DATABASE_URL}
```

## Field reference

| Field | Required | Purpose |
|-------|----------|---------|
| `name` | yes | Logical project name |
| `schema_version` | no | YAML contract version (currently `"1"`) |
| `version` | no | Free-form project version |
| `llm` | no\* | LLM provider config — required for any agent step |
| `variables` | no | Named values reusable as `${var_name}` |
| `defaults` | no | `timeout_secs`, `max_iterations` defaults for agents |
| `policy` | no | Shell command allow/deny lists |
| `contracts` | no | `file_write` allow/deny, `rate_limit` |
| `runtime` | no | Persistent shell + context-window tuning |
| `mcp_servers` | no | Subprocess tool servers ([docs/tools.md#mcp](tools.md#mcp)) |
| `connectors` | no | Inbound event sources ([docs/connectors.md](connectors.md)) |
| `outputs` | no | Outbound event sinks |
| `approvals` | no | Human-in-the-loop channels ([docs/approvals.md](approvals.md)) |
| `default_approval_channel` | no | Channel name for tools without per-pipeline override |
| `store` | no | Event store backend ([docs/store-backends.md](store-backends.md)) |

\* `llm` is optional at parse time but mandatory at runtime for any pipeline that contains an agent step.

## Examples

**Minimal:**

```yaml
name: my-project
schema_version: "1"
llm:
  provider: openai
  model: gpt-4o-mini
  api_key: ${env.OPENAI_API_KEY}
```

**Production-ish (Postgres + approvals):**

```yaml
name: ops-bot
schema_version: "1"
llm:
  provider: openai
  model: gpt-4o-mini
  api_key: ${env.OPENAI_API_KEY}

defaults:
  timeout_secs: 60
  max_iterations: 8

policy:
  enabled: true
  allow: ["echo *"]
  deny: ["*"]

contracts:
  file_write:
    allowed_dirs: ["./output"]
    deny_patterns: ["*.env", "*.key"]

connectors:
  - type: http_inbound
    name: webhook
    bind: "0.0.0.0:8080"
    path: /events
    pipeline: handle_event

default_approval_channel: ops_tg
approvals:
  - type: telegram
    name: ops_tg
    bot_token: ${env.TELEGRAM_BOT_TOKEN}
    chat_id: ${env.OPS_CHAT_ID}
    bind: "127.0.0.1:8088"
    callback_path: /telegram/approval
    secret_token: ${env.TELEGRAM_WEBHOOK_SECRET}

store: ${env.DATABASE_URL}    # postgres://...
```

## Gotchas

- **`${env.NAME}` is parsed lazily.** Missing env var doesn't crash at YAML parse — it surfaces when the field is consumed. Keep secrets in `.env` (auto-loaded).
- **`policy.deny` wins ties.** A command matching both `allow:` and `deny:` is denied.
- **`contracts.file_write.allowed_dirs` is the only path tools can write to** — outside, the file_write tool errors out. `deny_patterns` is checked first regardless of dir.
- **Postgres requires the `postgres` Cargo feature** when building from source. The pip-installed wheel ships with it enabled.
- **`schema_version` is enforced loosely today** — bumping to `"2"` will surface a warning in CLI validation. ADR-0020 has the full compatibility contract.

## See also

- [Agents](agents.md), [Pipelines](pipelines.md), [Tools](tools.md), [Connectors](connectors.md), [Approvals](approvals.md), [Store backends](store-backends.md)
- ADR-0020 (schema versioning), ADR-0012 (store backends), ADR-0022 (approvals), ADR-0023 (MCP)
