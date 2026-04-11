# Declarative custom tools

Date: 2026-04-09

## Context

ADR-0002 shipped the YAML config layer and ADR-0007 shipped the Python `@tool` decorator, but the two were never bridged. Today the declarative path (`agents/*.yml` → `Runtime` → `BuiltinActionExecutor`) is hard-wired to seven built-in tools in three different places:

1. **`src/config/agent.rs:32` — `KNOWN_TOOLS`.** A static slice of seven names. `validate_agent_tools` (`src/config/validate.rs:33`) rejects anything else with `ConfigError::Validation` at workspace load time.
2. **`src/engine/tools.rs:178` — `tool_definitions_for_agent`.** Builds the `Vec<ToolDefinition>` shipped to the LLM in `ChatRequest.tools`. Filters silently against the same seven names via `builtin_tool_def`. A name that survived validation but is not built-in would simply disappear from the LLM tool list.
3. **`src/runtime/action_executor.rs` — `BuiltinActionExecutor`.** Dispatches approved tool calls to `execute_builtin_tool`, returning `"unknown built-in tool: …"` for anything else.

`RuntimeBuilder::with_action_executor` (`src/runtime/mod.rs:181`) already accepts a custom `Arc<dyn ActionExecutor>`, so the *execution port* exists. What is missing is everything *upstream* of execution: declaring a custom tool, validating that the agent is allowed to reference it, and producing a `ToolDefinition` for the LLM. ADR-0007's Python `ToolRegistry` solves the same three problems but only for code that constructs the registry programmatically — there is no way to point a YAML-declared agent at it, and no way to declare a tool *without* writing Python at all.

This is a real gap, not a stylistic one. A user setting up a project from `project.yml` + `agents/*.yml` + `pipelines/*.yml` — i.e. the path the CLI optimises for — currently has zero options for adding a custom tool. They must either fork `engine/tools.rs` or abandon the declarative path entirely and drive the runtime from Python.

A second gap is sharper than it looks: ADR-0007 maps custom tools to `Intention::CallCustomTool`, which `ContractEngine` auto-approves. That is a defensible default for "I wrote this Python function myself", but it would be a footgun for a declarative `kind: shell` tool whose argument is interpolated into a command string — the operator would expect the same policy/approval surface that protects the built-in `execute_shell_command`. The new design has to give declarative tools an opinionated default that is at least as safe as the built-in they most resemble.

## Decision

Introduce a **`ToolCatalog`** as a first-class runtime contract, owned by `Runtime`, consulted by both the config validator and the pipeline handler, and extended by *both* declarative YAML tool files *and* programmatic registries (Python `ToolRegistry`, future MCP, etc.). Each tool in the catalog carries its own ESAA mapping and approval requirement, so the safety story is per-tool, not per-source.

### 1. New file shape: `tools/*.yml`

A new directory under the project root, sibling to `agents/` and `pipelines/`. Each file declares one custom tool:

```yaml
name: jira_create_issue
description: "Create a Jira issue in the configured project"
parameters:
  type: object
  properties:
    summary: { type: string }
    body:    { type: string }
  required: [summary]

# Approval / policy story (see §4)
requires_approval: false        # default for kind: http / kind: python
intention: call_custom_tool     # default; explicit values map to existing Intention variants

# One implementation kind per tool. Exactly one of these must be present.
implementation:
  kind: http
  method: POST
  url: "${env.JIRA_BASE}/rest/api/2/issue"
  headers:
    Authorization: "Bearer ${env.JIRA_TOKEN}"
  body_template: |
    {"fields": {"summary": "${args.summary}", "description": "${args.body}"}}
```

Three `implementation.kind` values ship in v1:

- **`shell`** — command template, arguments interpolated as `${args.X}`. Runs through the same sandbox/timeout knobs as the built-in `execute_shell_command`. **Default `requires_approval: true`** (see §4).
- **`http`** — method, URL, headers, optional JSON body template. No shell. Default `requires_approval: false`.
- **`python`** — `module: pkg.mod` + `function: name`. Only loadable when the `python` feature is enabled; the catalog logs and skips python tools on a python-less build instead of failing the whole workspace. Default `requires_approval: false` (the function is project code, like ADR-0007's `@tool`).

`tools/*.yml` is parsed by a new `src/config/tool.rs` (mirroring `agent.rs` / `pipeline.rs`), goes through `template::resolve_templates` like every other config file, and is loaded by `load_project_dir`. `WorkspaceConfig` grows a `tools: HashMap<String, ToolConfig>` field next to `agents` / `pipelines`.

### 2. `ToolCatalog` runtime contract

A new `src/runtime/tool_catalog.rs`:

```rust
pub struct ToolCatalog {
    builtin: BuiltinToolSet,                    // the existing seven
    declarative: HashMap<String, DeclarativeTool>,
    programmatic: HashMap<String, Arc<dyn ProgrammaticTool>>,
}

pub trait ProgrammaticTool: Send + Sync {
    fn definition(&self) -> &ToolDefinition;
    fn intention(&self, args_json: &str) -> Intention;
    fn requires_approval(&self) -> bool;
}
```

The catalog answers three questions for any tool name, regardless of source:

1. **`fn knows(&self, name: &str) -> bool`** — used by the config validator instead of `KNOWN_TOOLS.contains(...)`.
2. **`fn definition(&self, name: &str) -> Option<&ToolDefinition>`** — used by the handler in place of `tool_definitions_for_agent` to build `ChatRequest.tools`.
3. **`fn intention(&self, name: &str, args_json: &str) -> Option<Intention>`** + **`fn requires_approval(&self, name: &str) -> bool`** — consumed by the orchestrator/contract path so a declarative tool can opt into the same gates as a built-in.

Lookup precedence: **builtin → declarative → programmatic**. Collisions are a hard error at catalog construction time, not silent override; an operator who declares a `tools/web_search.yml` next to the built-in `web_search` should get a config error, not a surprise.

### 3. `Runtime` wiring

- `Runtime` gains `tool_catalog: Arc<ToolCatalog>`. `RuntimeBuilder::build` constructs it from `workspace.tools` plus the always-present built-in set.
- `RuntimeBuilder::with_tool_catalog(catalog)` lets advanced callers (Python, tests, future MCP host) supply their own catalog. By default, `with_tool_catalog` *replaces* the builder's auto-constructed catalog; a `with_programmatic_tool(name, tool)` convenience adds one entry on top of the YAML-derived catalog without forcing the caller to rebuild it.
- The Python bridge (`src/python/py_runtime.rs`) exposes a new `Runtime.register_tool(...)` that delegates to a `ProgrammaticTool` impl wrapping a `ToolRegistry` entry. Existing `ToolRegistry` users keep working — the registry becomes one source of `ProgrammaticTool`s, not the only path to a tool.
- `BuiltinActionExecutor` is renamed to `CatalogActionExecutor` and takes `Arc<ToolCatalog>` in its constructor. Its `execute` method dispatches by lookup:
  - **builtin** → existing `execute_builtin_tool`.
  - **declarative shell/http** → new dispatchers in `runtime::tool_dispatch::{shell,http}`.
  - **declarative python** → goes through the same PyO3 path the programmatic registry uses.
  - **programmatic** → calls `ProgrammaticTool::execute` (new method on the trait, returning `Result<String, String>`).

The existing `ActionExecutor` trait does not change. Operators who already injected a custom executor via `with_action_executor` keep working unchanged; they simply opt out of the catalog. The default executor in the builder is now `CatalogActionExecutor` instead of `BuiltinActionExecutor`.

### 4. ESAA / approval defaults per kind

The footgun in the current `Intention::CallCustomTool` auto-approval is real, so the new tool config carries explicit `intention` and `requires_approval` fields with kind-aware defaults:

| `implementation.kind` | Default `intention`                                  | Default `requires_approval` | Rationale                                                                                  |
|-----------------------|------------------------------------------------------|-----------------------------|--------------------------------------------------------------------------------------------|
| `shell`               | `Intention::ExecuteShellCommand { command }` (synthesised from the resolved template) | **`true`**                  | Same surface as the built-in. Refuses to silently downgrade.                                |
| `http`                | `Intention::CallCustomTool { tool_name, arguments }` | `false`                     | Network calls without local FS/process side effects; project code.                          |
| `python`              | `Intention::CallCustomTool { tool_name, arguments }` | `false`                     | Matches ADR-0007 behaviour for `@tool` functions — they are first-party project code.      |

The operator can override either field per tool. Setting `requires_approval: false` on a `kind: shell` tool is allowed but is **logged as a warning by the validator** (`tool '{name}' is kind=shell with requires_approval=false — declarative shell tools bypass the execute_shell_command policy gate; double-check this is intentional`). This makes the unsafe path explicit instead of accidental.

The `intention` field accepts the same set of tags `KNOWN_TOOLS` already maps to, plus `call_custom_tool` (default). Anything else is a config error.

### 5. Validator changes

`src/config/validate.rs::validate_agent_tools` no longer compares against `KNOWN_TOOLS`. It receives the workspace's catalog (built once, before the existing cross-validation pass) and asks `catalog.knows(tool)`. The error message now lists *all* available tools (builtin + declarative), not just the seven. `KNOWN_TOOLS` itself stays in `agent.rs` as the canonical *built-in* list — it just stops being the only acceptable answer.

Two new validation rules ride along:
- **Name collision** between a `tools/*.yml` and a built-in is a hard error (see §2).
- **Unsafe-shell warning** as described in §4. This is a warning, not an error, so it goes through `miette`'s `Diagnostic::severity = Warning` path; the workspace still loads.

### 6. What the user writes for the simplest case

For someone setting up a test project right now (the trigger for this ADR), the smallest end-to-end declarative custom tool is:

```yaml
# tools/echo.yml
name: echo
description: "Echo a string back to the agent"
parameters:
  type: object
  properties:
    text: { type: string }
  required: [text]
implementation:
  kind: http
  method: POST
  url: "https://postman-echo.com/post"
  body_template: '{"text": "${args.text}"}'
```

```yaml
# agents/helper.yml
name: helper
tools:
  - read_file        # built-in
  - echo             # custom, resolved from tools/echo.yml
```

No Rust, no Python, no rebuild.

## Alternatives Considered

- **Inline `custom_tools:` block on each agent.** Tempting because it skips the new directory, but it forces every agent that wants a tool to redeclare its full schema, and it makes name-collision validation across agents awkward. Rejected: tools are project-level resources, not agent-level ones, the same way pipelines are.
- **Programmatic-only via Python `ToolRegistry`.** This is what we have today, modulo the missing wiring. It would close the gap for users willing to write Python, but it explicitly does *not* solve the declarative case the user actually asked about, and it leaves `KNOWN_TOOLS` as a load-bearing hardcoded list.
- **MCP as the only extension point.** Letting operators register an MCP server in `project.yml` and route all custom tools through it is the natural long-term shape, but it pushes the gap behind a much larger dependency (an MCP client, transport selection, schema translation) and forces a one-line `kind: http` tool to be a spawned subprocess. We will almost certainly add `kind: mcp` later as a fourth implementation kind, but it should not be the *first* one.
- **Lift `KNOWN_TOOLS` to a runtime-mutable static.** A 30-line fix to the validator that lets a startup hook push names into the global list. Rejected because it does nothing about `tool_definitions_for_agent` (the LLM still sees no schema) and nothing about execution dispatch — it would silently produce broken pipelines instead of a clean validation error.
- **Re-use `Intention::CallCustomTool` auto-approval for everything declarative.** Smallest diff, but it is exactly the footgun §4 closes: a `kind: shell` tool with arguments interpolated from LLM output is *more* dangerous than the built-in, not less. Rejected.

## Consequences

- **Pro**: Closes a real declarative-mode gap. The user can build a complete project — agents, pipelines, custom tools — without writing Rust or Python.
- **Pro**: Removes `KNOWN_TOOLS` as a load-bearing constant. Built-ins become "the catalog entries the runtime ships with", not "the only valid names in the universe".
- **Pro**: Per-tool ESAA and approval defaults give operators a single place to reason about safety, instead of "built-ins are gated, custom tools are auto-approved" as an invisible rule. The unsafe-shell warning makes the dangerous path opt-in *and* visible.
- **Pro**: `ToolCatalog` is the integration point a future MCP host (or other tool source) plugs into. Adding `kind: mcp` later is a sibling implementation, not a redesign.
- **Pro**: Existing `ToolRegistry` and `with_action_executor` callers keep working — the catalog is additive.
- **Con**: New surface area in three modules: `src/config/tool.rs`, `src/runtime/tool_catalog.rs`, `src/runtime/tool_dispatch/{shell,http,python}.rs`. ~600–900 LoC plus tests.
- **Con**: The template variable resolver gains a new context (`${args.X}` for tool argument interpolation). Today `${env.X}`, `${project.X}`, `${var}`, `${inputs.X}` are resolved at parse time; `${args.X}` has to be resolved at call time, against per-call JSON. This is a real change to `template.rs`, not a one-liner — escaping for shell vs JSON body matters and will need its own tests.
- **Con**: The validator's miette warning path is currently unused (we only emit errors). The unsafe-shell warning forces us to wire warning-level diagnostics through `load_project_dir`. Worth doing once and reusing for future warnings.
- **Con**: Three implementation kinds is three things to maintain. The temptation to add `kind: grpc`, `kind: graphql`, `kind: sql`, … should be resisted until each one has a real caller — keep the kind set small and force exotic cases through `kind: python` or a future `kind: mcp`.

## Implementation slices

To avoid landing this as one ~900-LoC PR, the work splits cleanly into four slices, each individually shippable:

1. **Slice 1 — `ToolCatalog` + builtin migration.** Introduce `ToolCatalog`, populate it with the existing seven built-ins, replace `tool_definitions_for_agent` and `validate_agent_tools` with catalog lookups, rename `BuiltinActionExecutor` → `CatalogActionExecutor`. Behaviour identical to today; no new YAML, no new tools. This is the refactor that unblocks everything else.
2. **Slice 2 — `tools/*.yml` declarative loading + `kind: http`.** Add `src/config/tool.rs`, extend `WorkspaceConfig`, wire `${args.X}` template resolution, ship `kind: http` as the first declarative implementation. Smallest and safest of the three kinds — no shell, no Python — so it is the right one to validate the end-to-end shape.
3. **Slice 3 — `kind: shell`.** Adds the shell dispatcher *and* the unsafe-shell warning path through miette. Gated behind a real test that proves `${args.X}` is escaped, not just substituted.
4. **Slice 4 — `kind: python` + `ProgrammaticTool` bridge for `ToolRegistry`.** Connects ADR-0007's existing registry to the catalog and adds the `python` implementation kind. Only this slice touches the `python` feature build.

Each slice updates this ADR with a "Slice N — what landed" section the same way ADR-0013 does.
