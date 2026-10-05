# CLI reference

Every `zymi` subcommand. Run `zymi <command> --help` for the live flag list.

## Global

```
zymi --version
zymi --help
```

Most commands accept `-d, --dir <PATH>` to point at a project root other than the current directory.

### Which project a command uses

Project-scoped commands (`run`, `serve`, `resume`, `ls`, `pipelines`, `events`, `runs`, `observe`, `verify`, `fetch`, `mcp serve`) resolve their project as (ADR-0044):

1. `--dir`, if given;
2. the current directory, if it has a `project.yml`;
3. the **home project** — `$ZYMI_HOME`, default `~/.zymi` — if it has a `project.yml`. Announced on stderr: `zymi: no project.yml here — using home project …`;
4. otherwise the current directory (and the usual "no project.yml" error).

The home project is your personal pipeline library: create it once with `zymi init --home`, then `zymi ls` / `zymi run <name>` work from any directory that is not itself a project.

`.env` files are loaded from the resolved project, then the current directory, then `$ZYMI_HOME/.env` — earlier wins, and real environment variables always win over all of them. Machine-wide keys (e.g. for [named providers](project-yaml.md#named-providers)) belong in `~/.zymi/.env`.

## `zymi init`

Initialize a new zymi project in the current directory.

```bash
zymi init [-n NAME]
zymi init --example telegram [-n NAME]
zymi init --home                             # personal library at ~/.zymi
```

| Flag | Description |
|------|-------------|
| `-n, --name NAME` | Project name. Defaults to the directory name (`home` with `--home`). |
| `--example NAME` | Scaffold from a built-in example. Currently only `telegram`. |
| `--home` | Scaffold the home project at `$ZYMI_HOME` (default `~/.zymi`) instead of the current directory. |

`--home` drops a lean library: `project.yml` with no `llm:` and an enabled shell policy, `providers.yml` (commented), `.env.example`, a tool-only `pipelines/hello.yml` + `tools/say.yml`, `AGENTS.md` and `pyproject.toml`. Worth keeping in a private git repo; `.env` is gitignored.

Drops `project.yml`, `agents/`, `pipelines/`, `tools/`, `.zymi/`, and `AGENTS.md`. The `--example telegram` scaffold also drops `agents/{assistant,reviewer}.yml`, `pipelines/chat.yml`, two declarative tool stubs, two Python tool stubs, an approval-gated `broadcast` tool, and `.env.example`.

## `zymi run`

Run a pipeline once with explicit inputs.

```bash
zymi run <pipeline> -i key=value [-i key=value …]
```

| Flag | Description |
|------|-------------|
| `<pipeline>` | Pipeline name (filename stem under `pipelines/`). |
| `-i, --input KEY=VALUE` | Pipeline input. Repeatable. |
| `--approval terminal\|webhook` | Override approval routing. Default: use `approvals:` from `project.yml`. |
| `--callback-url URL` | Notification URL for `--approval=webhook`. |
| `-d, --dir PATH` | Project root. |

When stdin and stdout are both a terminal, `zymi run` asks for every declared input you didn't pass with `-i`, showing its description. Required inputs re-ask on an empty answer; optional ones are skipped. Scripts, CI and agents (no TTY) get the old non-interactive behaviour.

Whether an LLM is required is judged by the pipeline being run: a tool-only pipeline runs even if other pipelines in the project have agent steps and there is no `llm:`.

## `zymi serve`

Run a pipeline as a long-lived service that reacts to `PipelineRequested` events from connectors.

```bash
zymi serve <pipeline>
```

| Flag | Description |
|------|-------------|
| `<pipeline>` | Pipeline name to handle. |
| `--poll-interval-ms N` | Cross-process store-watcher poll interval (default 100ms). |
| `--approval terminal\|webhook` | Override approval routing. |
| `--callback-url URL` | Notification URL for `--approval=webhook`. |
| `-d, --dir PATH` | Project root. |

Multiple `zymi serve` processes can run against a shared Postgres store — each picks up its share of pending `PipelineRequested` events.

## `zymi events`

List and inspect events from the store.

```bash
zymi events                                  # last 50 events across all streams
zymi events --stream <stream-id>             # one run, incl. its per-step tool/LLM events
zymi events --kind WorkflowNodeCompleted
zymi events --raw                            # one JSON event per line
```

| Flag | Description |
|------|-------------|
| `-s, --stream ID` | Filter by stream id. |
| `-k, --kind TAG` | Filter by event-kind tag. |
| `-l, --limit N` | Max events shown (default 50). |
| `--raw` | Emit raw JSON, one event per line. |
| `-v, --verbose` | Extended detail per event. |
| `-d, --dir PATH` | Project root. |

`--stream` merges in the run's per-step sub-streams (`<run>:step:<id>`, where tool calls and LLM calls are recorded) in time order, tagging those events with `[step]`. A failed model call shows up as `llm_call_failed` with the error and elapsed time.

## `zymi runs`

List recorded pipeline runs.

```bash
zymi runs
zymi runs --pipeline chat
zymi runs --raw                              # one JSON record per line
```

| Flag | Description |
|------|-------------|
| `-p, --pipeline NAME` | Filter by pipeline name. |
| `-l, --limit N` | Max runs shown (default 50). |
| `--raw` | Emit raw JSON, one run per line. |
| `-d, --dir PATH` | Project root. |

## `zymi ls`

Compact listing — what can I run: name, first line of the description, inputs (`*` = required). Outside a project it lists the home project.

```bash
zymi ls
```

## `zymi pipelines`

List pipelines defined in the project, with their steps, dependencies and outputs.

```bash
zymi pipelines
```

## `zymi observe`

Interactive 3-panel TUI: run list, pipeline DAG, event timeline. Live updates from the store-watcher.

```bash
zymi observe
zymi observe --run pipeline-chat-abc         # pre-select a run
```

| Flag | Description |
|------|-------------|
| `-r, --run STREAM_ID` | Pre-select a run. |
| `-d, --dir PATH` | Project root. |

## `zymi verify`

Hash-chain integrity check. Detects tampering in `.zymi/events.db` (or in a Postgres-backed store).

```bash
zymi verify                                  # all streams
zymi verify --stream pipeline-chat-abc       # one stream
```

| Flag | Description |
|------|-------------|
| `-s, --stream ID` | Verify a single stream. |
| `-d, --dir PATH` | Project root. |

## `zymi resume`

Fork-resume a previous pipeline run from a chosen step (ADR-0018). Steps upstream of the fork are frozen — copied to a new stream verbatim. The fork step and its descendants run from scratch using the current configs on disk.

```bash
zymi resume <stream-id> --from-step <step-id>
zymi resume <stream-id> --from-step <step-id> --dry-run
```

| Flag | Description |
|------|-------------|
| `<stream-id>` | Parent run id (find via `zymi runs` / `zymi observe`). |
| `--from-step ID` | Step to fork at. |
| `--dry-run` | Print the resume plan and exit without writing events. |
| `--approval terminal\|webhook` | Override approval routing. |
| `--callback-url URL` | Notification URL for `--approval=webhook`. |
| `-d, --dir PATH` | Project root. |

## `zymi mcp probe`

Spawn an MCP server, handshake, list its advertised tools, shut it down. No project required — use this to discover what a server offers before writing an `allow:` whitelist.

```bash
zymi mcp probe fs -- npx -y @modelcontextprotocol/server-filesystem /tmp
zymi mcp probe gh --env GITHUB_PERSONAL_ACCESS_TOKEN=ghp_... -- npx -y @modelcontextprotocol/server-github
```

| Flag | Description |
|------|-------------|
| `<name>` | Cosmetic handshake name (doesn't have to match `project.yml`). |
| `--env KEY=VALUE` | Forward an extra env var. Repeatable. Only `PATH` is auto-forwarded. |
| `--init-timeout-secs N` | Handshake timeout (default 15). |
| `--call-timeout-secs N` | Per-call timeout (default 30). |
| `-- <CMD> [ARGS …]` | Server command. Use `--` to separate from `zymi` flags. |

## `zymi schema`

Emit JSON Schema for project-config types — useful for IDE-assisted editing of YAML.

```bash
zymi schema project                          # ProjectConfig
zymi schema agent
zymi schema pipeline
zymi schema tool
zymi schema --all > schemas.json             # everything in one document
```

## See also

- [Getting started](getting-started.md)
- [Events and replay](events-and-replay.md) — what `events`/`runs`/`verify`/`observe`/`resume` see
- [Approvals](approvals.md) — `--approval` flag semantics
