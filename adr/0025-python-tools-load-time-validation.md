# Validate Python tools at config load time

Date: 2026-05-07

## Context

Python tools (`tools/*.py`, `@tool`-decorated) are auto-discovered by the runtime at `Runtime::start` (`src/runtime/mod.rs`). The config-load path (`config::load_project_dir` → `validate_workspace`) ran *before* that and only knew about three name sources: builtin (`agent::KNOWN_TOOLS`), declarative (`tools/*.yml`), and MCP server prefixes from `project.mcp_servers`.

Result: any pipeline tool step or `agent.tools` entry referencing a Python tool was rejected as "unknown tool" by the validator, even though the same name would resolve correctly once the runtime started. `zymi pipelines` happened to be green because it doesn't call `validate_workspace`; `zymi serve` (full `load_project_dir`) failed.

## Decision

Add a fourth source to `ConfigToolNameResolver`: `python_names: Vec<String>` with a `.with_python_names(...)` builder. Populate it in `load_project_dir` by calling `crate::python::auto_discover::discover_python_tools(root)` and collecting `tools[].name`, gated behind `#[cfg(feature = "python")]` (no-op stub when the feature is off, so the config crate still builds without pyo3).

Discovery now runs twice — once here, once in the runtime. Accepted as a tradeoff: the patch stays self-contained (no need to thread `DiscoveryOutcome` through `WorkspaceConfig`) and the cost is a couple of `PyModule::from_code_bound` calls at startup.

Per-file discovery errors are deliberately swallowed at validation time. The runtime pass surfaces them via `log::warn!`; if a `tools/foo.py` fails to import, any pipeline referencing `foo` falls back to the existing "unknown tool" error path, which is the right user-facing behaviour.

## Consequences

- Pipelines and agents can reference Python tools without workarounds (no need to wrap `tools/foo.py` as a shell tool with a CLI shim).
- `validate_workspace` now reflects the same tool universe the runtime will see, closing the load-time / runtime drift that produced the original bug.
- Python discovery cost is paid twice per project load; negligible at current tool counts. If it becomes a problem, share the `DiscoveryOutcome` via `WorkspaceConfig`.
- Validator unit tests (`config::validate::tests`) cover the resolver path. End-to-end coverage via `load_project_dir` with a real `tools/foo.py` is not added — `cargo test --features python` doesn't link locally because pyo3 is in `extension-module` mode (Python tests run through maturin in CI).
