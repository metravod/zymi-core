# MCP filesystem demo

Minimal example of zymi-core consuming an off-the-shelf MCP server. One
`mcp_servers:` entry in `project.yml` pulls 14 filesystem tools from
[`@modelcontextprotocol/server-filesystem`][mcp-fs]; the `allow:` filter
narrows them to the four this agent actually needs.

No Rust, no Python, no per-tool schema authoring — the tool definitions come
from the server's `tools/list` response at startup.

## Prerequisites

- `node` + `npx` on `PATH`. `npx -y @modelcontextprotocol/server-filesystem`
  on first run will prompt to download the package.
- `OPENAI_API_KEY` exported, or edit `project.yml` to point at a different
  provider.

## Run

```bash
cd examples/mcp/filesystem
export OPENAI_API_KEY=sk-...
zymi run main task="List everything in the sandbox, then write a file called notes.md that summarises what you found."
```

Watch the run with the TUI:

```bash
zymi observe
```

You should see an `mcp_server_connected { server: "fs", tool_count: 4 }`
event at startup, followed by the agent's `mcp__fs__*` tool calls. On
shutdown the server publishes `mcp_server_disconnected { reason: "shutdown" }`.

## Env isolation

Only keys named under `env:` reach the server subprocess — `OPENAI_API_KEY`,
`GITHUB_PERSONAL_ACCESS_TOKEN` and everything else the parent process can see
stay out of the child (ADR-0023 §security posture).

`PATH` is the one exception: it's auto-forwarded so interpreters resolve
(every npm/uvx/pip-distributed server needs it). If you want a custom `PATH`
for the server, set `PATH: /your/path` under `env:` and yours wins.

## Probing a new server

Before wiring a server into `project.yml`, list its tools without running a
full pipeline:

```bash
cargo run --example mcp_probe -- \
  fs npx -y @modelcontextprotocol/server-filesystem ./sandbox
```

Output is one line per tool with its one-line description — use this to
decide what belongs in `allow:`.

[mcp-fs]: https://www.npmjs.com/package/@modelcontextprotocol/server-filesystem
