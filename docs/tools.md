# Tools

A tool is something an agent can call. zymi unifies four kinds under one catalogue: **declarative** (HTTP / shell, YAML), **Python** (`@tool` decorator), **MCP** (subprocess servers), and **builtin** (shipped with zymi-core).

## Overview

All four kinds share the same dispatch envelope at runtime: every call emits `ToolCallRequested` and `ToolCallCompleted` events, gets the same approval treatment, shows up identically in `zymi observe` and `zymi events`. The agent doesn't know — or care — which catalogue a tool came from.

Tool name is the catalogue key. Names must be unique across all four catalogues — collisions are rejected at startup.

## Declarative HTTP {#declarative-http}

YAML, no code. Lives in `tools/<name>.yml`.

```yaml
name: slack_post
description: "Post a message to a Slack channel"
parameters:                       # JSON Schema, sent verbatim to the LLM.
  type: object
  properties:
    channel: { type: string }
    text:    { type: string }
  required: [channel, text]

implementation:
  kind: http
  method: POST                    # GET, POST, PUT, PATCH, DELETE
  url: "https://slack.com/api/chat.postMessage"
  headers:
    Content-Type: "application/json"
    Authorization: "Bearer ${env.SLACK_BOT_TOKEN}"
  body_template: |
    {"channel": "${args.channel}", "text": "${args.text}"}
```

`${args.X}` is resolved at call time from the LLM's arguments. `${env.X}` is resolved at parse time from the environment. `body_template` is optional.

Default `requires_approval: false` (override per-tool with the `requires_approval:` key).

## Declarative shell {#declarative-shell}

```yaml
name: run_tests
description: "Run cargo test in a directory"
parameters:
  type: object
  properties:
    dir: { type: string }
  required: [dir]

implementation:
  kind: shell
  command_template: "cd ${args.dir} && cargo test"
  timeout_secs: 120                 # default 30
```

Runs through the persistent shell session pool (same pool as the builtin `execute_shell_command`). Subject to the project's `policy:` allow/deny.

**Default `requires_approval: true`** — shell tools are gated by default (ADR-0014 §4). Override with `requires_approval: false` for genuinely read-only commands.

## Python {#python}

`tools/<name>.py` containing one or more `@tool`-decorated functions. Auto-discovered at startup; sync and async both supported.

```python
"""Look up the weather for a city."""
from zymi import tool


@tool
def get_weather(city: str) -> str:
    """Return the current weather for a city."""
    # Replace stub with a real API call.
    return f"sunny in {city}"


@tool(intention="web_search", requires_approval=False)
async def deep_search(query: str, limit: int = 10) -> str:
    """Search the web with deep retrieval."""
    import httpx
    async with httpx.AsyncClient() as client:
        r = await client.get(f"https://search.example.com?q={query}&n={limit}")
        return r.text
```

The decorator is a marker — it sets attributes on the function and returns it unchanged, so the function is still callable in tests. Signature + docstring are introspected at startup to build the JSON Schema sent to the LLM.

**Decorator kwargs:**

- `name=` — override the registered name (defaults to `func.__name__`)
- `description=` — override the description (defaults to docstring's first line)
- `intention=` — ESAA intention tag (defaults to `CallCustomTool`)
- `requires_approval=` — force human-approval gating

**Discovery rules:**

- Files in `tools/*.py` are scanned alphabetically at startup.
- One broken file logs a warning and is skipped — it does NOT take down the rest.
- Name collisions with declarative / MCP / builtin tools are a hard error.
- Python tools require pip-installed zymi (`pip install zymi-core`) — the cargo CLI binary doesn't bundle the Python runtime.

**Don't `print()`.** Return a string from the function. Print goes to stderr and is not captured.

## MCP {#mcp}

[Model Context Protocol](https://modelcontextprotocol.io) servers — one entry in `mcp_servers:` (in `project.yml`) gives the agent N tools. Tool names auto-namespace as `mcp__<server>__<tool>`.

```yaml
# project.yml
mcp_servers:
  - name: fs
    command: [npx, -y, "@modelcontextprotocol/server-filesystem", ./sandbox]
    allow:                           # whitelist; mutually exclusive with deny:
      - read_text_file
      - write_file
      - list_directory
    init_timeout_secs: 15            # default 10
    call_timeout_secs: 30            # default 60
    restart:
      max_restarts: 2
      backoff_secs: [1, 5]
    requires_approval: false         # default. Set true to gate every tool.
```

Then in an agent:

```yaml
tools:
  - mcp__fs__list_directory
  - mcp__fs__read_text_file
  - mcp__fs__write_file
```

**Probe a server before wiring it** to find out what it advertises:

```bash
zymi mcp probe fs -- npx -y @modelcontextprotocol/server-filesystem /tmp
```

Security posture (PATH auto-forward, env-isolation, restart-on-crash) — ADR-0023.

## Builtin

Shipped with zymi-core; available without configuration:

| Name | Purpose | Approval |
|------|---------|----------|
| `read_file` | Read a file from disk | no |
| `write_file` | Write a file (subject to `contracts.file_write` allow-list) | no |
| `write_memory` | Append to the agent's memory store | no |
| `execute_shell_command` | Run a shell command (subject to `policy:` allow/deny) | yes |
| `spawn_sub_agent` | Delegate a task to a different agent | no |

Add by name to an agent's `tools:`. They behave like any other tool — same events, same approval flow.

## Approval

Set `requires_approval: true` on a tool (declarative, MCP, or via `@tool(requires_approval=True)`). The runtime publishes `ApprovalRequested` and waits for an `ApprovalGranted` / `ApprovalDenied` decision before executing. Channels (`terminal`, `http`, `telegram`) are configured in `project.yml::approvals:` — see [docs/approvals.md](approvals.md).

## Gotchas

- **Tool name collisions are a startup error.** Pick globally unique names.
- **`${args.X}` is call-time, `${env.X}` is parse-time.** Mixing them up means stale or empty values.
- **Shell tools default to `requires_approval: true`** — explicit `requires_approval: false` is required for read-only utilities you want to run without prompting.
- **Python tools won't load from a `cargo run` build of the CLI** — the pyo3 extension only links inside a Python runtime. Use the pip-installed `zymi`.
- **MCP tool names are flat under the `mcp__<server>__` prefix.** No nested namespaces.

## See also

- [Project YAML](project-yaml.md) — `mcp_servers:` schema
- [Agents](agents.md) — how tools are wired to an agent's `tools:`
- [Approvals](approvals.md) — the gating flow
- ADR-0014 (declarative tools), ADR-0023 (MCP), ADR-0024 (deterministic tool steps)
