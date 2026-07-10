# Getting started

Build your first zymi project in five minutes: install, scaffold, run.

## Overview

zymi-core is shipped as a single Python package with an embedded Rust runtime. You install the `zymi` CLI globally, scaffold a project with `zymi init`, build the project's venv with `zymi fetch`, point it at an LLM provider, and run a pipeline.

## Install

Install the CLI globally with [uv](https://docs.astral.sh/uv/) — this is the recommended path ([ADR-0032](../adr/0032-install-ux-fetch.md)):

```bash
uv tool install zymi-core
```

This puts the `zymi` CLI on your `$PATH` in its own isolated environment, so it doesn't collide with any project's dependencies. Don't have `uv`? `curl -LsSf https://astral.sh/uv/install.sh | sh` (macOS/Linux) or `irm https://astral.sh/uv/install.ps1 | iex` (Windows).

> **Embedding zymi in your own Python?** Use `uv add zymi-core` (or `pip install zymi-core`) *inside that project's venv* instead — the same wheel exposes the `zymi` Python module (`Runtime`, `@tool`, …). See [python-api.md](python-api.md). For running pipelines from the CLI, prefer the global `uv tool install` above.

Verify the install:

```bash
zymi --version
```

## Scaffold a project

In an empty directory, run one of the two scaffolds:

```bash
# Minimal — bare project.yml + one default agent + two declarative tool stubs.
zymi init

# Full demo — Telegram bot with approvals, declarative + Python tools, commented MCP block.
zymi init --example telegram
```

Both scaffolds drop an `AGENTS.md` into the project — your AI coding assistant (Claude Code, Cursor, …) will read it automatically and understand how zymi projects are laid out.

## Configure a provider

Edit `project.yml` and uncomment the `llm:` block. The minimal config wants a provider, a model, and an API key:

```yaml
llm:
  provider: openai
  model: gpt-4o-mini
  api_key: ${env.OPENAI_API_KEY}
```

`${env.NAME}` reads the env var at startup. zymi auto-loads `.env` from the project root, so write keys there:

```bash
# .env
OPENAI_API_KEY=sk-...
```

`.env` is in `.gitignore` by default — do not commit it.

Supported providers (out of the box): `openai` and any OpenAI-compatible endpoint (Anthropic via proxy, OpenRouter, local Ollama, …) by setting `base_url:` alongside `provider: openai`.

## Build the project venv

`zymi init` writes a `pyproject.toml` alongside your project. Build its venv once with `zymi fetch` (a thin wrapper over `uv sync`); pipeline-run commands transparently re-exec inside `./.venv` ([ADR-0032](../adr/0032-install-ux-fetch.md)):

```bash
zymi fetch
```

Re-run it whenever you add a Python `@tool` that imports a third-party library (append the dep to `pyproject.toml` first).

## Run a pipeline

For the minimal scaffold:

```bash
zymi run main -i task="Summarize the latest research on quantum error correction"
```

For the telegram scaffold, follow the printed checklist (BotFather token, `.env` setup, then `zymi serve chat`). No public URL or ngrok needed — the Telegram scaffold uses the `http_poll` connector (long-polls `getUpdates`) and the `telegram` approval channel (DMs admins inline ✅/❌ buttons), so nothing has to be reachable from the internet. Full Telegram setup is in [docs/connectors.md#http-poll](connectors.md#http-poll) and [docs/approvals.md](approvals.md).

## Inspect what happened

Every step zymi runs is recorded as an event in `.zymi/events.db`. Browse:

```bash
zymi runs                                # all pipeline runs
zymi events --stream <stream-id>         # event timeline for one run
zymi observe                             # 3-panel TUI: runs / DAG / events live
zymi verify --stream <stream-id>         # hash-chain integrity check
```

See [docs/events-and-replay.md](events-and-replay.md) for the event-sourcing model and fork-resume.

## Next steps

- [Project YAML reference](project-yaml.md) — every key in `project.yml`.
- [Pipelines](pipelines.md) — DAGs, agent steps, deterministic tool steps, ask steps.
- [Tools](tools.md) — declarative HTTP/shell, Python `@tool`, MCP servers.
- [Approvals](approvals.md) — gate sensitive tools behind a human decision; reasoning delegation (`ask:`) is the sibling mechanism.
- [CLI reference](cli.md) — every `zymi` subcommand with flags.

## See also

- [llms.txt](../llms.txt) — index for AI agents and scrapers.
- [README](../README.md) — pitch and 60-second tour.
