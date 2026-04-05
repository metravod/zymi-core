# Extract Rust core as standalone crate

Date: 2026-04-02

## Context

The zymi monorepo contained tightly coupled Rust code mixing core engine logic (events, ESAA, policy) with transport-specific code (Telegram, CLI, TUI). This made it impossible to reuse the engine independently or expose it via PyO3 bindings.

## Decision

Extract the pure engine into a standalone crate `zymi-core` with 6 modules:

- `types` — Message, ToolDefinition, ToolCallInfo, StreamEvent, TokenUsage
- `events/` — Event, EventKind, EventStore (SQLite + hash-chain), EventBus (mpsc), Connector, StreamRegistry
- `esaa/` — Intention, IntentionVerdict, ContractEngine (FileWriteContract, RateLimitConfig), Orchestrator, Projections
- `approval` — ApprovalHandler trait, SharedApprovalHandler, RAII ApprovalSlotGuard
- `policy` — PolicyConfig, PolicyEngine (glob-based allow/deny/require_approval with shell safety checks)

No dependencies on Agent, Telegram, CLI, or TUI.

## Consequences

- **Pro**: Clean boundary — any frontend (CLI, bot, web) imports `zymi-core` and wires its own connectors.
- **Pro**: PyO3 bridge becomes feasible — only expose `zymi-core` types.
- **Pro**: 105 tests, 0 clippy warnings at extraction.
- **Con**: Monorepo consumers need to be updated to import from the new crate.
