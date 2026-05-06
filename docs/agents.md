# Agents

An agent is an LLM with a system prompt and a list of tools. zymi runs it as a bounded ReAct loop: the model decides what tool to call, zymi executes it, the result feeds back into the next turn — up to `max_iterations`.

## Overview

One agent per file under `agents/<name>.yml`. The filename stem and the `name:` field must match. Agents are referenced by name from pipeline steps (`agent: <name>` in `pipelines/<name>.yml`).

## Schema

```yaml
name: my_agent              # required. Must match the filename stem.
description: "..."          # optional. Free-form summary.
model: gpt-4o-mini          # optional. Falls back to project.yml `llm.model`.
system_prompt: |            # optional. Multi-line string. Sets the agent's role.
  You are a helpful assistant.
tools:                      # optional. List of tool names callable by this agent.
  - web_search
  - get_weather
  - mcp__fs__read_text_file
max_iterations: 10          # optional. Defaults to project.yml `defaults.max_iterations` (10).
timeout_secs: 60            # optional. Defaults to project.yml `defaults.timeout_secs` (30).
policy:                     # optional. Per-agent shell allow/deny override.
  enabled: true
  allow: ["echo *"]
  deny: ["*"]
```

## Field reference

| Field | Required | Default | Notes |
|-------|----------|---------|-------|
| `name` | yes | — | Filename stem must match |
| `description` | no | none | For your own reference; not seen by the LLM |
| `model` | no | `llm.model` from project.yml | Override per-agent if useful |
| `system_prompt` | no | empty | Strongly recommended — sets behaviour |
| `tools` | no | `[]` | Tool names from any of the four catalogues (declarative, Python, MCP, builtin) |
| `max_iterations` | no | `defaults.max_iterations` (10) | Caps the ReAct loop |
| `timeout_secs` | no | `defaults.timeout_secs` (30) | Per-call LLM timeout |
| `policy` | no | inherits project | Tightens shell allow/deny only |

## Tool reference

Tool names come from four catalogues (resolved at startup, hard error on collision):

- **Declarative** — filename stem of any `tools/<name>.yml` file
- **Python** — function name (or `@tool(name=…)` override) in any `tools/<name>.py` file
- **MCP** — `mcp__<server>__<tool>` (the server name is `mcp_servers[].name` in `project.yml`)
- **Builtin** — fixed names: `read_file`, `write_file`, `write_memory`, `execute_shell_command`, `spawn_sub_agent`

If a tool returns a string like `"NOT_CONFIGURED: …"` (placeholder convention used by `zymi init` scaffolds), the agent reads it as instructions to itself and falls back gracefully — no crash.

## Examples

**Minimal:**

```yaml
name: helper
system_prompt: "You answer questions concisely."
```

**Multi-tool agent:**

```yaml
name: researcher
description: "Searches the web and stores findings in memory."
model: gpt-4o
system_prompt: |
  You are a thorough research assistant. Cite primary sources.
tools:
  - web_search
  - web_scrape
  - write_memory
max_iterations: 15
```

**Agent with template variables:**

```yaml
# project.yml has:  variables: { default_model: gpt-4o }
name: writer
model: ${default_model}
system_prompt: "Turn raw notes into a structured report."
tools:
  - read_file
  - write_file
```

## Gotchas

- **Filename and `name:` must match.** A mismatch is rejected at startup.
- **`tools:` is an allow-list, not an alias list.** The agent can only call tools listed here; missing tools are not "available but unused".
- **`max_iterations` bounds the ReAct loop, not LLM calls per turn.** A single iteration = one LLM call + zero or more tool calls in parallel.
- **`policy:` only tightens, never widens.** A per-agent `allow:` cannot grant something `project.yml`'s `policy.deny:` blocks.
- **`system_prompt:` is interpolated** with `${var}` references at parse time. `${env.NAME}` works here too.

## See also

- [Pipelines](pipelines.md) — how agents are sequenced
- [Tools](tools.md) — what to put in `tools:`
- [Project YAML](project-yaml.md) — `defaults:`, `policy:`, `llm:` inherit chain
