# zymi-core

Event-sourced agent engine for auditable AI workflows in Rust, YAML, and Python.

`zymi-core` helps you build agent workflows you can inspect after the fact. Every run is recorded as an immutable event stream in SQLite, agent side effects are mediated through intentions and boundary contracts, and pipelines execute as DAGs with parallel steps when possible.

## Highlights

- **Auditable by default**: every state change is persisted as an event with hash-chain verification.
- **Safer side effects**: agents emit intentions first; contracts and approvals decide what is allowed to execute.
- **Practical workflows**: define agents and DAG pipelines in YAML, then run them from a small CLI.
- **Flexible integration points**: use the Rust crate, Python bindings, or both.
- **LLM-provider ready**: OpenAI-compatible providers, Anthropic support, Python tools, and LangFuse event services.

## Installation

| If you want to... | Install with... |
| --- | --- |
| CLI + Python bindings | `pip install zymi-core` |
| Rust crate only | `zymi-core = "0.1"` |

`pip install zymi-core` gives you both the `zymi` CLI command and the `zymi_core` Python module.

## Quick Start

```bash
# Install
pip install zymi-core

# Create a demo project
mkdir zymi-demo
cd zymi-demo
zymi init --example research

# Add your LLM provider config to project.yml, then run the pipeline
zymi run research -i topic="event sourcing"

# Inspect what happened
zymi events --limit 20
zymi verify
```

For example, this is enough to get started with OpenAI:

```yaml
llm:
  provider: openai
  model: gpt-4o
  api_key: ${env.OPENAI_API_KEY}
```

What this gives you:

- `project.yml` for provider config, policies, contracts, and defaults
- `agents/` for agent definitions
- `pipelines/` for DAG workflows
- `.zymi/events.db` for the append-only event log
- `output/` and `memory/` directories in the research example

### Common CLI commands

```bash
zymi init --name my-project
zymi init --example research

zymi run main -i task="Summarize the architecture"
zymi run research -i topic="Rust event sourcing"

zymi events
zymi events --stream conversation-1
zymi events --kind LlmCallCompleted --json

zymi replay conversation-1 --from 1
zymi verify
zymi verify --stream conversation-1
```

## Project Layout

A `zymi` project is just a directory with YAML files:

```text
my-project/
  project.yml
  agents/
    default.yml
  pipelines/
    main.yml
  .zymi/
    events.db
```

The default scaffold created by `zymi init` is intentionally small:

```yaml
# project.yml
name: my-project
version: "0.1"

defaults:
  timeout_secs: 30
  max_iterations: 10

policy:
  enabled: true
  allow: ["ls *", "cat *", "echo *"]
  deny: ["rm -rf *"]
```

```yaml
# agents/default.yml
name: default
description: "Default agent"
tools:
  - web_search
  - read_file
  - write_memory
max_iterations: 10
```

```yaml
# pipelines/main.yml
name: main

steps:
  - id: process
    agent: default
    task: "${inputs.task}"

input:
  type: text

output:
  step: process
```

## Python Bindings

The same `pip install zymi-core` that gives you the CLI also exposes `Event`, `EventBus`, `EventStore`, `Subscription`, and `ToolRegistry` for programmatic use.

```python
from zymi_core import ToolRegistry

registry = ToolRegistry()

@registry.tool
def search(query: str) -> str:
    return f"Results for: {query}"

result = registry.call("search", '{"query":"rust async"}')
intention_json = registry.to_intention("search", '{"query":"rust async"}')
definitions = registry.definitions()
```

For lower-level event primitives the same package gives you the event store and bus directly:

```python
from zymi_core import Event, EventBus, EventStore

store = EventStore("./events.db")
bus = EventBus(store)
subscription = bus.subscribe()

event = Event(
    stream_id="conversation-1",
    kind={"type": "UserMessageReceived", "data": {
        "content": {"User": "Hello"},
        "connector": "python",
    }},
    source="python",
)

bus.publish(event)
received = subscription.try_recv()
```

## Rust Crate

Add the crate to your `Cargo.toml`:

```toml
[dependencies]
zymi-core = "0.1"
```

Example:

```rust
use std::sync::Arc;
use zymi_core::{Event, EventBus, EventKind, Message, SqliteEventStore};

let store = Arc::new(SqliteEventStore::new("events.db")?);
let bus = EventBus::new(store.clone());

let mut rx = bus.subscribe().await;

let event = Event::new(
    "conversation-1".into(),
    EventKind::UserMessageReceived {
        content: Message::User("Hello".into()),
        connector: "cli".into(),
    },
    "cli".into(),
);

bus.publish(event).await?;
let received = rx.recv().await.unwrap();
assert_eq!(received.kind_tag(), "user_message_received");

let verified_count = store.verify_chain("conversation-1").await?;
```

## How It Works

`zymi-core` is built around a small set of ideas:

1. **Every meaningful state change becomes an event.** The SQLite event store is the source of truth.
2. **Agents express intentions, not side effects.** Intentions are evaluated against boundary contracts before execution.
3. **Pipelines are DAGs.** Independent steps can run in parallel, while dependencies remain explicit.
4. **Runs stay replayable.** You can inspect events, replay streams, and verify hash-chain integrity later.

Core intention types include `ExecuteShellCommand`, `WriteFile`, `ReadFile`, `WebSearch`, `WebScrape`, `WriteMemory`, `SpawnSubAgent`, and `CallCustomTool`.

## Feature Flags (Rust crate)

The pip wheel ships with `python` and `cli` enabled. These flags are relevant when depending on the Rust crate directly.

| Feature | Description |
| --- | --- |
| `python` | PyO3 bindings for the `_zymi_core` Python extension module |
| `cli` | The `zymi` CLI binary |
| `runtime` | Async runtime and HTTP dependencies used by runtime integrations |
| `webhook` | HTTP approval handler built on Axum |
| `services` | Event-bus services such as LangFuse |

## Development

```bash
cargo test
cargo test --features services,webhook

cargo clippy -- -D warnings
cargo clippy --features services -- -D warnings

maturin develop --features python,cli
```

## License

MIT
