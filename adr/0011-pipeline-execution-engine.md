# Pipeline Execution Engine

Date: 2026-04-05

## Context

The `zymi run` CLI command existed only as a dry-run that printed the execution plan. To actually run pipelines — the core value proposition of zymi — we needed a real execution engine that orchestrates agent steps with LLM calls, tool execution, and event sourcing.

## Decision

Added `src/engine/` module with two files:

- `mod.rs` — pipeline execution orchestration:
  - Builds infrastructure (EventStore, EventBus, ContractEngine, Orchestrator, LLM provider) from workspace config
  - Executes DAG levels sequentially, steps within a level in parallel via `tokio::spawn`
  - Each step runs an agent loop: system prompt → task → LLM call → tool calls → repeat until text response
  - Passes outputs between dependent steps via template resolution (`${steps.X.output}`)
  - All actions emit events through the EventBus for full auditability

- `tools.rs` — built-in tool implementations:
  - `execute_shell_command` — `tokio::process::Command` with timeout
  - `read_file` / `write_file` — async filesystem operations relative to project root
  - `web_scrape` — reqwest GET with response truncation
  - `write_memory` — shared in-memory HashMap across pipeline steps
  - `web_search` — stub (no search provider integrated yet)

All tool calls go through the Orchestrator/ContractEngine for policy evaluation before execution.

## Consequences

- **Pros**: Pipelines are now executable end-to-end. Full event trail for every LLM call and tool execution. Policy enforcement on all side effects. Parallel execution of independent steps.
- **Cons**: No streaming output yet. No approval handler in CLI mode (denied-by-policy tools just return an error message to the LLM). web_search is a stub. No pipeline input CLI args yet.
- **Future**: Add `--input key=value` CLI flag, streaming token output, CLI-based approval handler (stdin y/n), and pluggable search provider.
