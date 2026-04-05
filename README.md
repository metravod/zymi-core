# zymi-core

Event-sourced agent engine written in Rust with Python bindings. Declarative YAML agents and pipelines, boundary contracts, hash-chain audit trail, and pluggable LLM providers.

## Why zymi-core?

Most agent frameworks treat execution as a black box. zymi-core records **every state change as an immutable event** in a SQLite store with hash-chain integrity. This means you can replay, audit, and verify any agent run after the fact.

Agents don't execute side effects directly. Instead, they emit **intentions** that are evaluated against **boundary contracts** before execution. This gives you fine-grained control over what agents can and cannot do.

## Features

- **Event sourcing** — append-only SQLite store with hash-chain verification, in-process pub/sub bus
- **ESAA (Event-Sourced Agent Architecture)** — intentions, boundary contracts, orchestrator, projections
- **Declarative YAML** — define agents, pipelines, and project config in YAML with template variables
- **DAG pipelines** — topological sort, parallel step execution, `depends_on` for step ordering
- **LLM providers** — OpenAI-compatible (OpenAI, vLLM, Ollama, Together) and Anthropic
- **Python tools** — `@tool` decorator with auto JSON Schema from type hints, sync and async support
- **Boundary contracts** — file write restrictions, shell command policies, rate limits
- **Human-in-the-loop** — approval handler with webhook support (POST /approve)
- **Services** — pluggable event bus subscribers for external integrations (LangFuse included)
- **CLI** — `zymi init`, `run`, `events`, `verify`, `replay`

## Installation

### Python bindings (from PyPI)

```bash
pip install zymi-core
```

Provides `zymi_core` package with `Event`, `EventStore`, `EventBus`, `ToolRegistry`.

### Rust crate (from crates.io)

```toml
[dependencies]
zymi-core = "0.1"
```

### CLI (from source)

```bash
cargo install zymi-core --features cli
```

## Quick Start

### Python

```python
from zymi_core import EventStore, EventBus, Event, ToolRegistry

# 1. Create an event store and bus
store = EventStore("./events.db")
bus = EventBus(store)

# 2. Subscribe to events
sub = bus.subscribe()

# 3. Publish events
event = Event(
    stream_id="conversation-1",
    kind={"type": "UserMessageReceived", "data": {
        "content": {"User": "What is the weather?"},
        "connector": "python",
    }},
    source="python",
)
bus.publish(event)

# 4. Receive events
received = sub.try_recv()
print(received)  # Event(stream_id='conversation-1', kind='user_message_received', ...)
```

### Register Python tools

```python
registry = ToolRegistry()

@registry.tool
def search(query: str) -> str:
    """Search the web for information."""
    return f"Results for: {query}"

@registry.tool(intention="web_search")
async def deep_search(query: str, max_results: int = 10) -> str:
    """Deep search with pagination."""
    ...

# Get tool definitions for LLM API
definitions = registry.definitions()

# Call a tool by name
result = registry.call("search", '{"query": "rust async"}')

# Map to ESAA intention
intention_json = registry.to_intention("search", '{"query": "test"}')
```

### CLI

The CLI is a Rust binary (not included in the Python wheel):

```bash
# Install from source
cargo install zymi-core --features cli

# Initialize a new project
zymi init --name my-project
zymi init --example research    # with a working research pipeline

# Run a pipeline with inputs
zymi run research -i topic="event sourcing"

# Inspect events
zymi events
zymi events --stream conversation-1
zymi events --kind LlmCallCompleted --json

# Verify hash-chain integrity
zymi verify

# Replay a stream
zymi replay conversation-1 --from 1
```

## Project Structure

A zymi project is a directory with YAML configuration:

```
my-project/
  project.yml              # project config, LLM provider, policies, contracts
  agents/
    researcher.yml         # agent definition: model, tools, system prompt
    writer.yml
  pipelines/
    research.yml           # pipeline: steps, dependencies, inputs/outputs
  .zymi/
    events.db              # SQLite event store (auto-created)
```

### project.yml

```yaml
name: my-project
version: "0.1"

llm:
  provider: openai           # openai | anthropic | ollama | vllm | together
  model: gpt-4o
  api_key: ${env.OPENAI_API_KEY}

variables:
  default_model: gpt-4o

defaults:
  timeout_secs: 60
  max_iterations: 15

policy:
  enabled: true
  allow: ["ls *", "cat *"]
  deny: ["rm -rf *"]

contracts:
  file_write:
    allowed_dirs: ["./output", "./memory"]
    deny_patterns: ["*.env", "*.key"]

services:
  langfuse:
    public_key: ${env.LANGFUSE_PUBLIC_KEY}
    secret_key: ${env.LANGFUSE_SECRET_KEY}
```

### agents/*.yml

```yaml
name: researcher
description: "Research agent"
model: ${default_model}
system_prompt: |
  You are a research assistant...
tools:
  - web_search
  - web_scrape
  - write_memory
max_iterations: 15
```

### pipelines/*.yml

```yaml
name: research
steps:
  - id: search_web
    agent: researcher
    task: "Search for: ${inputs.topic}"

  - id: search_deep
    agent: researcher
    task: "Find details about: ${inputs.topic}"

  - id: analyze
    agent: researcher
    task: "Cross-reference findings"
    depends_on: [search_web, search_deep]    # runs after both complete

  - id: write_report
    agent: writer
    task: "Write report to ./output/report.md"
    depends_on: [analyze]

input:
  type: text
output:
  step: write_report
```

Pipeline DAG (parallel where possible):

```
search_web  --+
              +--> analyze --> write_report
search_deep --+
```

## Architecture

### Event Sourcing

Every state change is an `Event` with:
- Unique ID, stream ID, sequence number, timestamp
- `EventKind` — discriminated union (17 variants covering the full agent lifecycle)
- Correlation ID — links related events across a request lifecycle
- Causation ID — tracks which event caused this one
- Hash chain — each event's hash includes the previous event's hash

The `EventStore` (SQLite) is the source of truth. The `EventBus` persists first, then fans out to subscribers.

### ESAA (Event-Sourced Agent Architecture)

Agents emit **intentions** (what they want to do), not actions. Each intention is evaluated:

1. **IntentionEmitted** event recorded
2. **ContractEngine** evaluates against boundary contracts
3. **IntentionEvaluated** event recorded with verdict
4. If `RequiresHumanApproval` — approval requested via handler
5. Caller executes only if approved

Intention types: `ExecuteShellCommand`, `WriteFile`, `ReadFile`, `WebSearch`, `WebScrape`, `WriteMemory`, `SpawnSubAgent`, `CallCustomTool`.

### Services

Services subscribe to the EventBus and react to events for external integrations:

```rust
#[async_trait]
pub trait EventService: Send + Sync {
    fn name(&self) -> &str;
    async fn handle_event(&self, event: &Event) -> Result<(), ServiceError>;
    async fn flush(&self) -> Result<(), ServiceError>;
}
```

Built-in: **LangFuse** — maps agent lifecycle events to traces, generations, and spans. Batches HTTP calls to the LangFuse ingestion API.

## Rust API

### Feature Flags

| Feature | Dependencies | Description |
|---------|-------------|-------------|
| `python` | `pyo3` | PyO3 bindings (`_zymi_core` Python module) |
| `cli` | `clap` | CLI binary (`zymi`) |
| `webhook` | `axum` | HTTP approval handler |
| `services` | — | Event bus services (LangFuse) |

### Core Types

```rust
use zymi_core::{
    // Events
    Event, EventKind, EventBus, EventStore, SqliteEventStore, StreamRegistry,
    // ESAA
    Intention, IntentionVerdict, Orchestrator, ContractEngine,
    // Config
    ProjectConfig, AgentConfig, PipelineConfig, WorkspaceConfig,
    load_project_dir, build_execution_plan,
    // LLM
    LlmProvider, ChatRequest, ChatResponse, create_provider,
    // Policy
    PolicyEngine, PolicyConfig, PolicyDecision,
};
```

### Example: Event Store

```rust
use zymi_core::{SqliteEventStore, EventBus, Event, EventKind};
use std::sync::Arc;

let store = Arc::new(SqliteEventStore::new("events.db")?);
let bus = EventBus::new(store.clone());

// Subscribe before publishing
let mut rx = bus.subscribe().await;

// Publish an event
let event = Event::new(
    "conversation-1".into(),
    EventKind::UserMessageReceived {
        content: Message::User("Hello".into()),
        connector: "cli".into(),
    },
    "cli".into(),
);
bus.publish(event).await?;

// Receive
let received = rx.recv().await.unwrap();
assert_eq!(received.kind_tag(), "user_message_received");

// Verify hash chain integrity
let verified_count = store.verify_chain("conversation-1").await?;
```

## Development

```bash
# Run all tests
cargo test

# Run tests with all features
cargo test --features services,webhook

# Clippy (CI treats warnings as errors)
cargo clippy -- -D warnings
cargo clippy --features services -- -D warnings

# Build Python wheel (requires maturin)
maturin develop --features python
```

## License

MIT
