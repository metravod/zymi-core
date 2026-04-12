# Persistent shell session

Date: 2026-04-12

## Context

ADR-0011 shipped `execute_shell_command` as a built-in tool, and ADR-0014 (Slice 1) moved its dispatch into `CatalogActionExecutor`. The implementation in `src/engine/tools.rs:92-128` is the simplest thing that could possibly work: every call spawns a fresh `tokio::process::Command::new("sh").arg("-c").arg(command)`, waits with a timeout, returns stdout/stderr, and drops the child.

That design has a single, structural consequence: **no shell state survives between calls**. The agent that runs

```
execute_shell_command("cd /tmp/work")
execute_shell_command("ls")
```

does not list `/tmp/work` — it lists the project root, because the second `sh -c` spawn has no idea the first one ever ran. The same is true for environment variables (`export FOO=bar`), shell functions, `source venv/bin/activate`, and any command-prefix state.

This is not a corner case. It is the *common* case for any agent that does work more complex than a single one-shot command:

1. **Coding agents.** The natural workflow is "cd into a subdir → run tests → cd into another subdir → run a different test". Today the agent has to either prefix every command with `cd /full/path && ...` or accept that it cannot keep working state.
2. **DevOps agents.** Activating a Python venv, exporting credentials, switching kube contexts — all of these are stateful operations that the current tool throws away on the next call.
3. **Research agents with bash access.** Even something as simple as `pip install -e . && python script.py` is fragile, because if the agent decides to split it into two tool calls (which the LLM frequently does for explainability), the second call has no install.

The workaround "tell the LLM to write one big `&&`-chained command" is the wrong layer to fix this. It pushes runtime-state management onto prompt engineering, makes failures harder to attribute (is the failure in step 3 of a 6-step chain?), and still does not solve the env-export case (`export FOO=bar` in one chain does not survive into the next chain).

There is also an event-store angle that matters more than it looks. zymi-core's invariant is that the event log is the source of truth: anything that matters for replay, audit, or recovery has to be in events. Persistent shell state is *deliberately not* one of those things — a long-lived bash process holds runtime state (current directory, exported variables, open file descriptors) that cannot be reconstructed by replaying events into a fresh shell. This ADR has to draw the line explicitly: the shell session is **ephemeral runtime state**, not part of the event-sourced history. That choice has consequences for replay, recovery, and parallel sessions, all of which the design has to address.

## Decision

Introduce a **`ShellSession`** as a first-class runtime concept, owned by the `Runtime`, keyed by `stream_id`, and consumed by `CatalogActionExecutor` when dispatching the built-in `execute_shell_command` tool (and, later, by ADR-0014's `kind: shell` declarative tools).

### 1. New module: `src/runtime/shell_session.rs`

```rust
pub struct ShellSession {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    stderr: tokio::io::BufReader<tokio::process::ChildStderr>,
    stream_id: String,
    last_used: std::time::Instant,
    cwd_at_creation: std::path::PathBuf,
}

pub struct ShellSessionPool {
    sessions: tokio::sync::Mutex<HashMap<String, ShellSession>>,
    idle_timeout: Duration,
}

impl ShellSessionPool {
    pub async fn run_command(
        &self,
        stream_id: &str,
        command: &str,
        timeout: Duration,
        project_root: &Path,
    ) -> Result<ShellOutput, ShellError> { ... }

    pub async fn close_session(&self, stream_id: &str);
    pub async fn reap_idle(&self);
}
```

The pool is created once per `Runtime` and held in `Arc<ShellSessionPool>`. `CatalogActionExecutor` gets a clone of the `Arc` at construction time.

### 2. Sentinel-based command boundaries

The hardest design problem is "where does this command's output end and the next one begin", because we are reading a long-lived stream, not a process exit. The standard pattern, and the one this ADR adopts:

For each command, the pool wraps it as:

```bash
{ <user_command> ; } ; printf '\n__ZYMI_END_%s__%d\n' "$RUN_ID" "$?" >&1
{ true ; } 2>&1; printf '__ZYMI_END_%s__\n' "$RUN_ID" >&2
```

where `RUN_ID` is a fresh UUID per call (not per session — per *call*) and is only known to the pool. Two boundary markers, one on stdout (carrying the exit code) and one on stderr. The pool reads stdout until it sees `__ZYMI_END_<run_id>__<code>`, parses the code, then drains stderr until it sees the matching marker, and returns.

Why per-call UUID, not a fixed string: a malicious or buggy command that prints `__ZYMI_END_FOO__0` to stdout would otherwise be able to truncate the output of any future call. Per-call UUID means an attacker would have to predict the next UUID to forge a boundary, which is the same threat model as a CSRF token.

Why two markers (stdout + stderr): without a stderr marker we cannot tell when stderr is "done" without picking an arbitrary delay. The dual-marker design lets the read loop terminate deterministically.

Why `printf` and not `echo`: `echo` interpretation differs across `dash`/`bash`/`sh`, `printf` is POSIX-uniform.

### 3. Lifecycle

- **Creation.** Lazy. The first `execute_shell_command` for a `stream_id` that has no session in the pool spawns a fresh `bash -i` (interactive flag so PS1 / shell init runs) with stdin/stdout/stderr piped, sets `cwd = project_root`, and inserts it into the pool. The session emits `EventKind::ShellSessionStarted { stream_id, pid, shell_path }` so the event log records that a session existed (without recording its state).
- **Reuse.** Subsequent calls on the same `stream_id` look up the existing session. `last_used` is bumped on every call.
- **Idle reaping.** A background task on the runtime calls `reap_idle()` every 60 seconds and kills sessions whose `last_used` is older than `idle_timeout` (default 10 minutes, configurable in `project.yml`). Reaping emits `EventKind::ShellSessionClosed { stream_id, reason: "idle" }`.
- **Workflow end.** When `WorkflowCompleted` is emitted on a stream, the runtime calls `close_session(stream_id)` and emits `ShellSessionClosed { reason: "workflow_end" }`.
- **Hard kill on Drop.** `ShellSessionPool::Drop` sends SIGKILL to all live children. The runtime never relies on Drop alone; explicit close is the contract. Drop is the safety net for panics.

### 4. Event types added to `EventKind`

Two new variants in `src/events/mod.rs`:

```rust
ShellSessionStarted {
    stream_id: String,
    pid: u32,
    shell_path: String,
},
ShellSessionClosed {
    stream_id: String,
    reason: String, // "idle" | "workflow_end" | "killed" | "exit"
},
```

These are **history events**, not state events: replay does not recreate the shell. Their only purpose is audit ("did the agent in this run have a persistent shell available, and when did it go away") and observability (Langfuse projection in `services/`).

The existing `ToolCallCompleted { call_id, result_preview, is_error, duration_ms }` covers the per-command result. No new event for "command ran in session" is needed — the session is implementation detail of how the tool executed, not a separate thing to record.

### 5. Replay semantics

This is the subtle part and has to be locked down explicitly because it is the place this ADR most easily gets misunderstood.

**On replay, the engine does NOT recreate the shell session.** A `ShellSessionStarted` event during replay is informational only — replay rebuilds projections (memory, conversation context), not runtime processes. If a replayed run wants to *resume* execution (not just rebuild state), the resume path starts a fresh shell session for the same `stream_id`, and the agent must treat that shell as starting from `cwd = project_root` with no state, even though earlier `execute_shell_command` results in the event log show different cwds.

This is a deliberate trade-off:

- **For:** event log stays a clean source of truth. No "ghost state" hidden in non-replayable shell processes. Replay is deterministic.
- **Against:** an agent that does `cd /work && ...` then crashes mid-workflow cannot resume "from where it left off" with cwd preserved. Resume is best-effort: same memory, same conversation, fresh shell.

ADR-0016 (context window management) consumes this guarantee: the `ContextBuilder` will surface `ShellSessionStarted` / `ShellSessionClosed` events in the model context so the LLM knows whether its previous-turn shell state is still alive or has been reset.

### 6. Configuration

`project.yml` gains an optional block:

```yaml
runtime:
  shell:
    idle_timeout_seconds: 600    # default 600 (10 min)
    max_sessions: 32             # default 32; LRU-evict beyond this
    shell_path: "/bin/bash"      # default: bash, fallback to sh
    interactive: true            # default true; passes -i to the shell
```

Loaded via the existing `src/config/project.rs` and exposed on `Runtime` through `RuntimeConfig`.

### 7. Security boundaries that do NOT change

This ADR is about *state*, not *sandboxing*. All of these stay exactly as they are today:

- `requires_approval` for `execute_shell_command` continues to gate via `ContractEngine` (`src/esaa/`). A persistent session does not bypass approval; every `execute_shell_command` call still goes through `Intention::ExecuteShellCommand` and the same gate.
- The per-command `timeout` knob still applies. A long-running command in a persistent session is killed after the timeout the same way it is today — but the *session* survives the killed command, only the command dies. The ADR tests this case explicitly.
- `cwd` clamping (the project root) is the *initial* cwd. Once the agent runs `cd ..`, it can leave the project root — same behaviour as today, because `bash -c "cd .. && pwd"` already does the same thing. If FS-level sandboxing is wanted, that is a separate ADR.

## Alternatives Considered

- **Stay stateless, paper over with prompt engineering.** "Tell the LLM to chain commands with `&&`." Rejected: pushes runtime concerns into prompt design, makes failures opaque, does not solve env-export across calls, and is unfixable for any agent that operates at the granularity of one tool call per logical action.

- **Per-command shell with serialised state file.** Spawn a fresh shell per call but persist `pwd` and exported env to a state file between calls; `source` the state file as the first thing in each new shell. Tempting because it preserves the "no long-lived process" property and keeps replay trivial. Rejected: serialising shell state is a tar pit — exported functions, aliases, shell options, open file descriptors, terminal state, signal handlers, history, and `set -e` mode all have to be captured and replayed. The "simple" version captures cwd and env, which is enough to mislead the agent into thinking the session is persistent right up until it relies on a shell function it defined two calls ago.

- **One persistent shell per `Runtime`, not per `stream_id`.** Simpler pool, no `HashMap<stream_id, Session>`. Rejected: breaks parallel sessions (multiple `PipelineRequested` running concurrently would race on the shared shell). The whole point of session isolation in Phase A1 of the roadmap is that streams do not see each other's state; sharing a shell would undo that.

- **Use `script(1)` or `expect(1)` instead of raw stdin/stdout pipes.** These tools already solve the boundary-detection problem by giving you a PTY. Rejected: PTY behaviour varies meaningfully across macOS (BSD `script`) and Linux (util-linux `script`), drags in a system dependency that we currently do not require, and the sentinel-marker approach is well-understood and portable.

- **Run each command in a fresh container instead of a fresh process.** Strongest isolation, full reproducibility. Rejected for v1: massive ops surface (Docker / Podman / runc dependency, image management, layer caching, network namespaces), overkill for the "I want `cd` to persist" problem this ADR is actually solving. If sandboxing becomes the goal, a separate ADR-00xx introducing `kind: container` for both the built-in tool and ADR-0014 declarative tools is the right vehicle.

- **Treat the shell session as event-sourced state.** Record every `cd`, every `export`, every shell action as an event, replay to reconstruct cwd and env. Rejected: see "serialising shell state is a tar pit" above. The set of state that *can* be event-sourced is a strict subset of what bash actually carries between commands, and the gap is the dangerous part — the agent thinks it has a persistent shell when actually it has a partial reconstruction of one.

## Consequences

- **Pro.** Coding, DevOps, and research agents finally get a shell that behaves like a shell. The "tell the LLM to `&&`-chain everything" workaround can be retired.
- **Pro.** Replay semantics stay clean: the event log is still the source of truth, and the shell is explicitly out-of-band runtime state. No invisible "but actually there's also this hashmap of state" coupling.
- **Pro.** The same `ShellSessionPool` is the natural backend for ADR-0014 `kind: shell` declarative tools — they look up the session by `stream_id` instead of spawning their own subprocess. Locks in consistency between the two shell paths.
- **Pro.** Idle reaping + per-stream sessions means a `zymi serve` process running many concurrent pipelines does not accumulate orphan shells the way a "one shell per Runtime" design would.
- **Con.** New runtime resource that has to be explicitly cleaned up. Drop is the safety net, not the contract; bugs in the close path will leak processes. Mitigation: explicit `close_session` test on `WorkflowCompleted`, plus an idle reaper as a second line of defence.
- **Con.** Sentinel-marker output parsing is one more thing to get exactly right. Tests have to cover: command that prints binary to stdout, command that hangs forever (timeout path must kill the command without killing the session), command that exits the shell itself (`exit` from the agent — session is gone, next call must respawn), command whose output contains a string that *looks* like the sentinel but uses the wrong UUID.
- **Con.** `pid: u32` in `ShellSessionStarted` is OS-dependent and not particularly useful for replay. Kept anyway because it is invaluable for debugging "why is there an orphan bash process" in production.
- **Con.** Replay-resume becomes "fresh shell, same memory" — an explicit limitation in the resume story. The Recovery and projections goal in drift will need to document this clearly: "resumed runs do not preserve shell state".
- **Con.** Approval flow is unchanged, which means a session that has been approved once still re-prompts on every command. This is the *correct* behaviour (each command is its own decision) but operators may expect "I approved the session, why is it asking again". Documentation, not code.

## Implementation slices

To keep this from landing as one big PR, three slices, each independently shippable.

1. **Slice 1 — `ShellSessionPool` + sentinel reader, no Runtime wiring.** Standalone module with its own tests. Spawns a real `bash`, runs a sequence of commands, verifies cwd persists, env persists, exit codes are captured, timeouts kill the command without killing the session, sentinel-spoofing attacks are defeated. No changes to `engine/tools.rs` yet. This is the "prove the primitive works" slice.

2. **Slice 2 — Wire into `CatalogActionExecutor` for `execute_shell_command`.** Replace `src/engine/tools.rs:92` shell spawn with a call into the pool. `CatalogActionExecutor` constructor accepts `Arc<ShellSessionPool>`. `Runtime` constructs the pool. Add `ShellSessionStarted` / `ShellSessionClosed` to `EventKind`, emit them, render them in `cli/events.rs`. Wire `WorkflowCompleted` → `close_session`. Idle reaper task spawned by `RuntimeBuilder::build`. This is the "production wiring" slice.

3. **Slice 3 — Configuration + ADR-0014 `kind: shell` adoption.** Add the `runtime.shell` block to `project.yml`, plumb it through `RuntimeConfig`. When ADR-0014 Slice 3 (`kind: shell` declarative tools) finally lands, it dispatches into the same pool instead of building its own subprocess machinery. This slice may land out-of-order with ADR-0014 Slice 3 — whichever is ready first wires the pool into the other.

Each slice updates this ADR with a "Slice N — what landed" section in the same way ADR-0013 and ADR-0014 do.

## Slice 3 — what landed (2026-04-12)

Added `runtime.shell` configuration block to `project.yml` and shipped ADR-0014 `kind: shell` declarative tool adoption. The pool is now fully configurable and declarative shell tools dispatch through the same `ShellSessionPool` as the built-in `execute_shell_command`.

Key changes:

- `src/config/project.rs` — added `RuntimeConfig` and `ShellConfig` structs with schemars derives. `ProjectConfig` gained `runtime: Option<RuntimeConfig>`. `ShellConfig` fields: `idle_timeout_seconds` (default 600), `max_sessions` (default 32), `shell_path` (default `/bin/bash`), `interactive` (default false). All fields have serde defaults.
- `src/runtime/shell_session.rs` — `ShellSessionPool::from_config(&ShellConfig)` constructor. `ShellSession::spawn` accepts `shell_path` and `interactive` parameters instead of hardcoded `bash --norc --noprofile`. LRU eviction: when `max_sessions` is reached, the least-recently-used session is killed before spawning a new one (emits `ShellSessionClosed { reason: "lru_evict" }`). `emit_started` uses the configured `shell_path` instead of hardcoded `"bash"`.
- `src/runtime/mod.rs` — `RuntimeBuilder::build` reads `workspace.project.runtime.shell` (falls back to `ShellConfig::default()`) and passes it to `ShellSessionPool::from_config`.
- `src/config/tool.rs` — `ImplementationConfig` gained `Shell { command_template, timeout_secs }` variant. `effective_requires_approval()` returns `true` for `kind: shell` (ADR-0014 §4). Added `is_shell()` helper.
- `src/runtime/action_executor.rs` — `CatalogActionExecutor::execute` dispatches declarative `kind: shell` tools to `execute_declarative_shell`, which resolves `${args.X}` in the command template and runs through the persistent pool. Logs `warn!` for `kind: shell` tools with `requires_approval: false`. `parse_args_for_interpolation` and `resolve_args` promoted to `pub(crate)` for reuse by `ToolCatalog`.
- `src/runtime/tool_catalog.rs` — `intention()` maps declarative shell tools to `Intention::ExecuteShellCommand` (same ESAA gate as the built-in), resolving `${args.X}` in the command template. HTTP and unknown tools still map to `CallCustomTool`.

New tests: config parsing (full + partial `runtime.shell` block, defaults), `from_config` pool construction, LRU eviction at `max_sessions`, custom `shell_path`, `kind: shell` YAML parsing (full + minimal + approval override), shell tool intention mapping to `ExecuteShellCommand`, HTTP tool intention stays `CallCustomTool`. 254 tests pass with `--features runtime`, clippy clean.
