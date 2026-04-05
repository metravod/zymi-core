# Python @tool Decorator: ToolRegistry via PyO3

Date: 2026-04-05

## Context

ADR-0005 established Python components as event-driven participants; ADR-0006 scoped PyO3 to bridging core types. The next step is enabling Python users to define custom tools that LLM agents can call. These tools need to:

1. Provide `ToolDefinition` (name, description, JSON Schema) for the LLM API.
2. Map to ESAA `Intention` variants for contract evaluation and audit.
3. Support both sync and async Python callables.
4. Integrate with the existing event bus for `ToolCallRequested` / `ToolCallCompleted` events.

## Decision

A `ToolRegistry` PyO3 class (`src/python/tool.rs`) bridges Python tool functions to the Rust type system.

### Registration

Two modes:
- **Explicit**: `registry.register(name, description, parameters_schema, callable, intention=None)`
- **Decorator**: `@registry.tool` or `@registry.tool(description="...", intention="...")`

The decorator introspects the Python function using `inspect.signature()` and `typing.get_type_hints()` to auto-generate:
- Tool name from `__name__`
- Description from first line of docstring
- JSON Schema from type annotations (`str` -> `string`, `int` -> `integer`, `float` -> `number`, `bool` -> `boolean`, `list[X]` -> `array`, `dict` -> `object`, `Optional[X]` -> non-required)

### Execution

`registry.call(name, arguments_json)` invokes the Python callable:
- **Sync functions**: called directly with kwargs unpacked from JSON.
- **Async functions**: detected via `inspect.iscoroutinefunction()`, executed with `asyncio.run()`.

### Intention Mapping

`registry.to_intention(name, arguments_json)` maps tool calls to ESAA `Intention` variants:
- Built-in tools with `intention="web_search"` etc. map to `Intention::WebSearch`, `Intention::ExecuteShellCommand`, etc.
- Custom tools (no intention tag or unknown tag) map to `Intention::CallCustomTool { tool_name, arguments }`.

A new `Intention::CallCustomTool` variant was added, auto-approved by the `ContractEngine` (user-defined code running in-process).

### Decorator Factory Pattern

`@registry.tool(description="...", intention="...")` returns a `_ToolDecorator` helper class that captures the options and applies them when called with the function. This avoids complex closure semantics in PyO3 while providing clean Python decorator syntax.

## Alternatives Considered

- **Pure Python decorator (no Rust)**: Would lose type-safe `ToolDefinition` construction and require a separate Python package. The registry needs to produce Rust `ToolDefinition` for LLM providers.
- **pydantic model for schema**: Adds a heavy dependency. `inspect` + `typing` from stdlib is sufficient for tool parameter schemas.
- **Register tools via YAML config only**: Loses the ability to define tools as Python functions. YAML config (`agents/*.yml`) already lists tool names; the registry maps those names to implementations.
- **Auto-register all decorated functions globally**: Global state is fragile. Explicit registry instances allow multiple registries (e.g., per-agent tool sets).

## Consequences

- Python users can write `@registry.tool` and immediately have their functions available as LLM tools.
- Tool definitions flow through existing `ChatRequest.tools` to LLM providers.
- Tool calls flow through ESAA intentions for contract evaluation and audit trail.
- Custom tools are auto-approved by default; built-in tools with intention tags go through full contract evaluation.
- No new dependencies added.
- PyO3 scope remains narrow per ADR-0006: ToolRegistry bridges Python callables to Rust types, orchestration stays in Rust.
