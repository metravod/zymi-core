# Python API

`pip install zymi-core` ships the `zymi` CLI plus the `zymi_core` Python module for two use cases: writing `@tool`-decorated Python functions auto-discovered from `tools/*.py`, and embedding zymi inside your own Python application.

## Overview

The Python module is a thin wrapper over the Rust runtime, built with pyo3 + maturin. The exposed types share `Arc`-handles with the Rust side — `Runtime.bus()` and `Runtime.store()` give you the *same* bus and store the pipeline writes to, so you can subscribe in Python and see Rust-emitted events in real time.

## Top-level imports

```python
from zymi_core import (
    Event,
    EventBus,
    EventStore,
    RunPipelineResult,
    Runtime,
    StepResult,
    Subscription,
    ToolRegistry,
)

from zymi import tool          # @tool decorator for tools/*.py
```

## `@tool` — Python tool decorator

Auto-discovered from any `tools/<name>.py` in a zymi project. Sync and async both supported.

```python
from zymi import tool


@tool
def get_weather(city: str) -> str:
    """Return the current weather for a city."""
    return f"sunny in {city}"


@tool(name="search_v2", description="Newer search.", intention="web_search", requires_approval=False)
async def search(query: str, limit: int = 10) -> str:
    """..."""
    ...
```

The decorator is a marker: it sets `_zymi_tool = True` (plus optional `_zymi_tool_name`, `_zymi_tool_description`, `_zymi_tool_intention`, `_zymi_tool_requires_approval`) and returns the function unchanged. The function is still callable directly in tests.

The Rust loader walks every `tools/*.py` at runtime startup, picks up callables with `_zymi_tool` truthy, and uses Python's `inspect` module to introspect signature + docstring into a JSON Schema for the LLM.

**Decorator kwargs:**

| Kwarg | Purpose |
|-------|---------|
| `name=` | Override the registered name (default: `func.__name__`) |
| `description=` | Override the description (default: docstring's first line) |
| `intention=` | ESAA intention tag (default: `CallCustomTool`) |
| `requires_approval=` | Force human-approval gating |

## `Runtime`

Embed a zymi project in your Python app.

```python
from zymi_core import Runtime

rt = Runtime.for_project("./my-zymi-project", approval="terminal")
result = rt.run_pipeline("chat", inputs={"message": "hello"})

print(result.success, result.final_output)
for step_id, sr in result.step_results().items():
    print(step_id, sr.success, sr.iterations, sr.output)
```

**`Runtime.for_project(path, approval="terminal")`** — load `project.yml` and build a runtime.

- `path` — project root containing `project.yml`.
- `approval` — `"terminal"` (default, prompts on stdin for tools requiring approval) or `"none"` (such tool calls resolve to a denial). A pluggable Python callback is a future addition.

**`Runtime.run_pipeline(name, inputs=None) -> RunPipelineResult`** — one-shot pipeline run. `inputs` is a `dict[str, str]`.

**`Runtime.bus() -> EventBus`** — shared `Arc<EventBus>` with the runtime. Subscribe to see live events.

**`Runtime.store() -> EventStore`** — shared `Arc<EventStore>` with the runtime. Read events directly.

**`Runtime.project_root() -> str`** — absolute path of the project root.

The runtime cleans up its approval channels on `__del__`.

## `RunPipelineResult` and `StepResult`

```python
result.pipeline_name        # str
result.success              # bool
result.final_output         # Optional[str] — the `output:` step's text
result.step_results()       # dict[str, StepResult]
result.step("respond")      # Optional[StepResult]
```

```python
sr.step_id                  # str
sr.agent_name               # str — the agent that ran (None for tool steps)
sr.output                   # str
sr.iterations               # int — ReAct loop count
sr.success                  # bool
```

## `EventBus`, `EventStore`, `Event`, `Subscription`

For embedding scenarios where you want to inject your own events or observe live.

```python
from zymi_core import EventStore, EventBus, Event

store = EventStore("./my-project/.zymi/events.db")
bus = EventBus(store)

# Publish.
event = Event(
    stream_id="my-stream",
    kind={"type": "UserMessageReceived", "data": {
        "content": {"User": "hello"},
        "connector": "embedded",
    }},
    source="my-app",
)
bus.publish(event)

# Subscribe.
sub = bus.subscribe()
ev = sub.try_recv()           # Optional[Event] — non-blocking
if ev is not None:
    print(ev.kind_tag, ev.stream_id)

# Subscribe to one correlation.
sub = bus.subscribe_correlation("approval-abc")
```

**`Event(stream_id, kind, source)`** — construct in Python. `kind` is a `dict` matching the `EventKind` JSON shape (`{"type": "VariantName", "data": {...}}`).

**`Event.kind_tag`** — string variant tag (`"user_message_received"` etc).

**`Subscription.try_recv() -> Optional[Event]`** — non-blocking poll.

**`EventStore(path)`** — opens or creates a SQLite event store at `path`. The Postgres backend is currently driven via the YAML `store: postgres://…` field; a dedicated Python constructor is a follow-up.

## `ToolRegistry`

Programmatic tool registration — useful when you want to define tools in Python WITHOUT relying on the `tools/*.py` auto-discovery (e.g. dynamically generated tools, tools loaded from a non-standard directory).

```python
from zymi_core import ToolRegistry

reg = ToolRegistry()
reg.register_from_callable(my_function)        # picks up `@tool` markers if present
print(reg.names())                              # list[str]
```

The auto-discovery path uses an internal registry equivalent — most users never need to touch `ToolRegistry` directly.

## CLI entry point

The pip wheel installs `zymi` as a console script defined by `[project.scripts]`:

```toml
[project.scripts]
zymi = "zymi_core._cli:main"
```

`zymi_core._cli.main(argv)` is a thin shim that dispatches to the embedded Rust CLI. You can call it from Python if you want the CLI's behaviour without shelling out:

```python
from zymi_core._cli import main
main(["run", "chat", "-i", "message=hello"])
```

## Building from source

```bash
maturin develop --features python,cli         # editable install into the active venv
# or
maturin build --features python,cli           # produce a wheel
pip install dist/zymi_core-*.whl
```

The `python` feature pulls in pyo3; `cli` pulls in `clap` and the full CLI surface. macOS local `cargo test --features python` doesn't link cleanly because pyo3's extension-module setting is intended for runtime loading — use maturin + Python for local Python testing.

## Gotchas

- **`@tool`'d functions are still callable directly** — useful in tests but means "did the decorator apply" can't be observed at the call site. Check `func._zymi_tool` to verify.
- **Don't `print()` from `@tool` functions.** Return a string. `print` goes to stderr and is not captured by the runtime.
- **`Runtime.run_pipeline` is blocking.** It runs the async pipeline on a shared Tokio runtime via `block_on`. If you call it from inside an existing event loop you'll deadlock — use `asyncio.to_thread` or run it from a worker.
- **The pip wheel and the cargo build are different artefacts.** Cargo `cargo build --features cli` (no `python`) produces a `zymi` binary that won't load `tools/*.py`. Use the pip-installed `zymi` for any project that uses Python tools.

## See also

- [Tools](tools.md) — declarative + Python + MCP + builtin overview
- [Events and replay](events-and-replay.md) — what events look like
- ADR-0014 (declarative tools), ADR-0009 (event sourcing)
