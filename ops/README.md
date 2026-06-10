# ops/ — zymi-core releases itself

A zymi project that cuts zymi-core releases. Dogfood: the release runs
as a [pipeline-as-MCP-tool](../README.md#zymi-as-an-mcp-server--pipelines-as-tools-for-any-agent)
with the point-of-no-return step behind a human approval form.

## Release via an MCP host (Claude Code)

```bash
cargo build --features cli        # build the binary you are about to release
cd ops && claude                  # .mcp.json wires ../target/debug/zymi automatically
```

Release from `main` (the `ship` step pushes `HEAD`, so whatever branch
you're on is what goes to origin). Then ask the agent: *"release 0.7.0"*.
The `release` tool runs:

1. `guard` — fail if the working tree is dirty
2. `bump` — set the version in **both** manifests (Cargo.toml + pyproject.toml)
3. `check` — clippy `-D warnings` (cli+python features) + lib tests; refreshes Cargo.lock
4. `commit` — `release: vX.Y.Z` with the two manifests + Cargo.lock
5. `ship` — **approve/deny form pops here** → tag `vX.Y.Z` + push branch and tag

Deny the form and the pipeline halts after `commit` — nothing leaves the
machine; `git reset --hard HEAD~1` undoes the bump. Every step lands
hash-chained in `ops/.zymi/events.db` (`zymi observe` to browse, or ask
the agent — the server exposes `zymi.runs.*` introspection tools).

## Manual fallback

The pipeline is sugar over the documented procedure (CLAUDE.md "Release
versioning" + pre-push checklist). If the pipeline itself is what's
broken, release by hand — same five commands in the same order.

## Why `../target/debug/zymi` and not the global `zymi`?

Two reasons:

- **No feature lag.** The release flow may rely on engine features the
  last *published* version doesn't have (this pipeline already does:
  interactive MCP approvals shipped after 0.6.13).
- **Self-test.** The code you are about to tag is the code running your
  release. If the MCP server, the approval bridge, or the event store
  regressed, the release refuses to happen — that's a feature.

## Notes

- No agent steps, no API key needed: every step is a deterministic tool
  step (the `llm:` block in `project.yml` is a loader-required stub).
- Long `check` step: if your MCP host enforces a per-tool-call timeout,
  raise it (Claude Code: `MCP_TOOL_TIMEOUT`).
- `ops/.zymi/` is gitignored — run history is local.
