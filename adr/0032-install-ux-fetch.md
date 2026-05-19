# Install UX: thin global CLI + per-project venv via `zymi fetch`

Date: 2026-05-19

Status: Accepted.

## Context

`pip install zymi-core` into a system Python or a project venv produces a `zymi` binary that is only on PATH while that venv is activated. Users routinely end up either prefixing every command with `uv run` / `source .venv/bin/activate` or losing the CLI between sessions. The user has flagged this as the single biggest UX papercut blocking the v0.6 public unveil: they want literally `pip install zymi-core` (or the uv equivalent) followed by a bare `zymi serve` to work, on Mac/Linux/Windows, with no shell ceremony.

The structural constraint is that Python `@tool` code runs **in-process via PyO3** (`src/python/py_runtime.rs`). Whichever Python interpreter the `zymi` binary is linked against is the one that resolves user `import pandas` statements. This forces a trade-off between two obvious shapes:

- **Path A** — global tool install (`uv tool install zymi-core`). `zymi` is on PATH everywhere, but the embedded Python is the global tool env's Python. Any project-specific `@tool` dep must be re-injected with `uv tool install --with …` and there is no per-project lockfile. Conflicts the moment two agent projects on the same machine want different versions of the same dep — and that's the default case for any nontrivial use of zymi.
- **Path B** — per-project venv (`uv add zymi-core` + `uv run zymi serve`). Correct isolation, real lockfile (`uv.lock`), reproducible deploys. But every command carries a `uv run` prefix, which the user explicitly rejected as not the UX zymi should ship with.

Neither shape alone meets the bar. CrewAI — the closest peer in the declarative-Python-agent space — solves this with a hybrid: thin global CLI (`uv tool install crewai`) plus a `crewai install` subcommand that builds a per-project venv from the project's `pyproject.toml`, and `crewai run` that executes inside it. We adopt the same architecture and diverge only on naming.

## Decision

Three coordinated changes.

**1. Distribution.** The recommended install becomes `uv tool install zymi-core` (Mac/Linux/Windows). `pip install zymi-core` into a venv remains supported for contributor workflows but is no longer the user-facing default in docs. The wheel is unchanged; only the install instructions and `zymi init`'s "Next steps" output change.

**2. Scaffold.** `zymi init` (both `default` and `tg` templates, `src/cli/init.rs`) additionally generates:

- `pyproject.toml` with `dependencies = ["zymi-core>=<current-version>"]`, plus the Python-side deps the template's `@tool` files import (`tg` template currently uses none beyond stdlib; default template none).
- `.python-version` pinning a sensible minor (3.12 as of writing — same one the maturin wheel is built against in CI).

The final "Next steps" printout becomes exactly:

```
uv tool install zymi-core      # if not already installed
cd <project>
zymi fetch
zymi serve
```

**3. Runtime.** Two new behaviours in the CLI:

- New subcommand `zymi fetch` (new file `src/cli/fetch.rs`). Shells out to `uv sync` in the project root. Fails fast with a readable message if `uv` is not on PATH or no `pyproject.toml` is present. No flags in v1.
- Pipeline-execution commands (`zymi serve`, `zymi run`, anything that loads and runs user `@tool` code) gain a re-exec preamble: detect `./.venv/bin/python` (or `.venv\Scripts\python.exe` on Windows) at command entry; if present and the current process's `sys.executable` does not match, re-exec via the venv's interpreter using the same argv. The re-exec is **opt-out** via `--no-venv` for contributor workflows.

The re-exec uses the venv's own Python rather than injecting the venv's `site-packages` into the global tool env's `sys.path`. This is the load-bearing choice: re-exec is robust against C-extension ABI mismatch; path injection is not.

`zymi-core` is listed as a dep in the scaffolded `pyproject.toml`, so `uv sync` populates the project's `.venv` with its own copy of the zymi wheel. The re-exec then lands on a process whose embedded Python is in the project's venv, sees both `zymi-core` and the user's tool deps in one site-packages tree, and PyO3 imports resolve cleanly.

## Consequences

- Pro: the user-facing UX is the four-line sequence above. No `uv run` prefix, no activate, no PATH ceremony past the one-time `uv tool install`.
- Pro: per-project deps are real — `uv.lock` per project, no global conflicts when multiple agent projects coexist on the same machine.
- Pro: deploy is `git clone … && cd … && zymi fetch && zymi serve`. uv pulls the right Python interpreter on the deploy host; system Python is not required.
- Pro: rollback is cheap — `--no-venv` flag bypasses re-exec, and not running `zymi fetch` simply means the user falls back to whatever Python the global tool env supplies.
- Con: `zymi-core` is installed twice on disk (once globally in the uv tool env, once per project venv). Each project pays one wheel's worth of disk — acceptable; not a user complaint we've ever heard.
- Con: a new hard dependency on `uv` being on the user's PATH. Mitigated by it already being the recommended installer; the failure message in `zymi fetch` tells the user how to install uv if missing.
- Con: re-exec adds a tens-of-milliseconds startup cost on pipeline-run commands. Below the noise floor of pipeline startup (which already pays YAML load + agent init).
- Future: a `zymi fetch --upgrade` and a `zymi fetch --add <pkg>` proxying to `uv add` are obvious next steps. Out of scope here — ship the base.

## Rejected alternatives

- **Pure path A.** Forces `uv tool upgrade --with <pkg>` per dep with no lockfile and global conflicts across projects. Wrong default for an extensible-tool framework.
- **Pure path B (`uv run zymi serve`).** Correct isolation, wrong ergonomics. Explicitly rejected by the user.
- **`sys.path` injection** — global zymi binary detects `./.venv/site-packages` and prepends it to its own embedded Python's `sys.path`. Brittle on C-extensions whose ABI tag doesn't match the global tool env's Python (e.g. project on 3.13, global env on 3.12). Same hazard class as the bundled-binary path rejected for separate reasons.
- **Subprocess `@tool` execution.** Spawn user `@tool` code via the project's `.venv/bin/python` for each call instead of in-process PyO3. Fully decouples envs but pays per-call overhead, requires re-plumbing event/store/approvals across a process boundary, and is a much bigger change than the problem warrants.
- **Match CrewAI naming (`zymi install`).** Considered. Rejected for two reasons: avoids looking derivative when the architectural debt of overlap with CrewAI is already non-trivial, and `fetch` is independently the better verb (established dev semantics in `git fetch` / `go fetch` / `nix fetch`).
