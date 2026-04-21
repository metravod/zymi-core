# MCP demos

Examples of zymi-core consuming off-the-shelf [Model Context Protocol][mcp]
servers. One `mcp_servers:` entry in `project.yml` pulls every tool the
server advertises at startup — no per-tool Rust or YAML schemas to write.

[mcp]: https://modelcontextprotocol.io

## Index

| Demo | Server | Highlights |
| --- | --- | --- |
| [`filesystem/`](./filesystem) | [`@modelcontextprotocol/server-filesystem`][mcp-fs] | Narrow 14 tools to 4 with `allow:`; sandboxed file I/O; env isolation |

More demos will land as we exercise additional servers (fetch, git, sqlite, …).

[mcp-fs]: https://www.npmjs.com/package/@modelcontextprotocol/server-filesystem

## Probing a server before wiring it in

Before editing `project.yml`, list what a server advertises:

```bash
cargo run --example mcp_probe -- <name> <command> [args...]
```

No LLM, no project directory — the probe spawns the server, completes the
MCP handshake, prints `tools/list`, then shuts down cleanly. Use the output
to decide what belongs in `allow:`.

### Keyless example: the official `fetch` server

```bash
cargo run --example mcp_probe -- fetch uvx mcp-server-fetch
```

```text
probe: spawning fetch via ["uvx", "mcp-server-fetch"]
probe: handshake ok
server 'fetch' advertises 1 tool(s):
  - fetch — Fetches a URL from the internet and optionally extracts its contents as markdown.
```

Nothing secret flows to the child: the probe only forwards `PATH` (so
`uvx` / `npx` / `python` resolve) and whatever you explicitly pass via
`MCP_ENV_<KEY>=<VALUE>` in the parent process. See ADR-0023 §security
posture for the full rule.

## Shape of an `mcp_servers:` entry

```yaml
mcp_servers:
  - name: fs                # becomes the `mcp__fs__*` tool-name prefix
    command: [npx, -y, "@modelcontextprotocol/server-filesystem", ./sandbox]
    env:                    # only keys named here reach the child (PATH auto)
      FOO: bar
    allow: [read_text_file, write_file]  # optional whitelist
    init_timeout_secs: 15   # handshake deadline
    call_timeout_secs: 30   # per tools/call deadline
    restart:
      max_restarts: 2
      backoff_secs: [1, 5]
```

Agents then reference the prefixed names in their `tools:` list:

```yaml
tools:
  - mcp__fs__read_text_file
  - mcp__fs__write_file
```

At startup you get one `mcp_server_connected { server, tool_count }` event
per server; on shutdown, `mcp_server_disconnected { reason }`. Every
`tools/call` is bracketed by the usual `tool_started` / `tool_finished`
events with the `mcp__<server>__<tool>` name, so the audit trail is
identical to any other tool.
