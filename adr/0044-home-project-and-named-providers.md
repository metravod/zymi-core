# A personal home project at `~/.zymi`, found automatically, with named LLM providers

Date: 2026-10-04

## Context

zymi's most valuable artefact for its own author turned out to be boring ops
pipelines: provision a VPN user, add a client in the x-ui panel, read traffic
stats. Most are pure tool DAGs (ADR-0041), so they run **without any LLM** —
which means they keep working when the coding agent that wrote them is down,
rate-limited or unreachable. That is the point: the human must be able to run
`zymi run add_vpn_user` on their own.

Today that promise breaks on three papercuts, all observed on the author's
machine on 2026-10-04:

1. **No home for personal pipelines.** The working `vpn_provision` pipeline
   lives inside the `hotpath-miner` research repo, and Claude Code's MCP
   config points `zymi mcp serve --dir .../hotpath-miner`. Every zymi command
   resolves its project as `--dir` or cwd, so a pipeline is only reachable by
   remembering where it was born.
2. **The library is not browsable for a human.** `zymi pipelines` exists but
   prints a debug view (every step, every edge). And `zymi run` requires all
   inputs as `-i k=v` up front — without the agent around, nobody remembers
   that `add_vpn_user` wants `name`, `inbound_id`, `expiry_days`.
3. **LLM endpoints are copy-pasted per project.** A user with several
   OpenAI-compatible endpoints (e.g. a cheap Qwen at `api.neuraldeep.ru/v1`)
   re-types `provider`/`base_url`/`api_key` in every `project.yml`. The same
   endpoint is also the natural fallback model when the primary one is down.

Rejected alternatives:

- **Explicit `-g/--global` flag instead of automatic fallback.** Safer against
  "ran the wrong project", but adds ceremony to the exact command a stressed
  human types from an arbitrary directory. The author chose automatic. The
  wrong-project risk is mitigated by announcing the fallback (Decision 2).
- **Walk up parent directories like git.** More magic, and a stray
  `project.yml` in `~` or a monorepo root would silently win. Only cwd is
  inspected; walking up can be added later if a consumer asks.
- **Merge home + local pipelines into one namespace.** Name collisions, two
  event stores, two `.venv`s, two policy blocks in one runtime. A run belongs
  to exactly one project.
- **XDG split (`~/.config/zymi` + `~/.local/share/zymi`).** Correct on Linux,
  but the home project is a *project* (yml + tools + `.venv` + store), and
  splitting it across two roots breaks every invariant that a project is a
  single directory. One dir, overridable via `ZYMI_HOME`, mirrors `~/.claude`.
- **Implicit global default LLM** applied to any project without `llm:`.
  Silent, and it would make ADR-0041 deterministic projects build a provider
  they never use. Providers are only used when a project names one.
- **Providers declared in `project.yml`.** Does not solve sharing across
  projects, which is the whole ask.
- **OS keychain for API keys.** Nice, but `.env` already works everywhere and
  is what users know. Revisit on demand.

## Decision

1. **Home project.** `$ZYMI_HOME`, defaulting to `~/.zymi` (`HOME`, or
   `USERPROFILE` on Windows), is an ordinary zymi project: `project.yml`,
   `pipelines/`, `tools/`, optional `pyproject.toml` + `.venv`, its own event
   store (which lands at `~/.zymi/.zymi/events.db` — accepted wart, consistent
   with every other project). `zymi init --home` scaffolds it. The author is
   encouraged to keep it in a private git repo (`.env` stays ignored).

   The home scaffold is lean on purpose: no agent pipeline (so a fresh home
   runs without any LLM), a tool-only `hello` example, a commented
   `providers.yml`, `.env.example`, and `policy.enabled: true` with a small
   `allow:` list — a disabled policy makes *every* shell command ask for
   approval, which is the wrong default for a library of unattended ops.

2. **Project resolution** for every project-scoped command (`run`, `serve`,
   `resume`, `pipelines`, `ls`, `events`, `runs`, `observe`, `verify`,
   `fetch`, `mcp serve`):
   1. `--dir`, if given;
   2. cwd, if it contains `project.yml`;
   3. `$ZYMI_HOME`, if it contains `project.yml` — announced with one stderr
      line (`zymi: no project.yml here — using home project <path>`); stderr
      so it never corrupts `mcp serve`'s stdout protocol;
   4. otherwise cwd, so existing "no project.yml found" errors are unchanged.

   The `.venv` re-exec (ADR-0032) uses the same resolved root, so a home
   project with Python tools runs inside `~/.zymi/.venv`.

3. **`.env` layering.** Loaded in order, earlier wins (dotenv only fills
   holes, real env always wins): resolved root's `.env` → cwd's `.env` →
   `$ZYMI_HOME/.env`. The home `.env` is where provider keys live, so they
   are available from any project.

4. **Library UX.**
   - `zymi ls`: compact listing — name, first line of description, inputs
     (`*` marks required). `zymi pipelines` stays as the verbose debug view.
   - `zymi run` prompts for every declared input not passed via `-i`, but
     only when stdin and stdout are both a TTY. The prompt shows the input's
     description; required inputs re-ask on empty, optional ones are skipped
     on empty. Non-interactive invocations (scripts, CI, agents) behave
     exactly as before.

   `zymi run` builds its runtime for the requested pipeline only, so the
   ADR-0041 "is an LLM required" check is judged per pipeline: a tool-only
   pipeline runs from a library that also holds agent pipelines and has no
   `llm:`. `serve` and `mcp serve` keep the workspace-wide check.

5. **Named providers.** `$ZYMI_HOME/providers.yml` maps a name to the fields
   of `LlmConfig`:
   ```yaml
   neuraldeep:
     provider: openai
     base_url: https://api.neuraldeep.ru/v1
     api_key: ${env.NEURALDEEP_API_KEY}
     model: qwen3.8-27b
   ```
   In `project.yml`, `llm:` accepts, besides the existing inline mapping:
   - a string — `llm: neuraldeep`;
   - a mapping with `use:` plus overrides — `llm: { use: neuraldeep, model: qwen3-coder }`.

   The reference is resolved at project load time into a plain `LlmConfig`,
   so the runtime is unchanged. `${env.*}` inside `providers.yml` is resolved
   only for the selected entry — an unset key for an unused provider is not
   an error.

   A reference that **fails** to resolve (unknown name, missing
   `providers.yml`, unset key) does **not** fail the project load: it is
   recorded and the project loads with no LLM. Only when the runtime needs a
   model — a pipeline with an agent step — does it fail, with the original
   message (file path + list of known providers). Otherwise the whole library
   would die with its LLM key, including the tool-only pipelines whose point
   is to work without one. Malformed *inline* `llm:` still fails the load as
   before.

## Consequences

- Personal ops pipelines get one stable address; MCP hosts can point at
  `--dir ~/.zymi` (or omit `--dir` when launched outside any project).
- The "agent is down" scenario is covered end-to-end: deterministic pipelines
  run without a model, interactive prompts remove the need to remember
  inputs, and agent-step pipelines can be pointed at a fallback provider by
  changing one word.
- **Minus: wrong-project risk.** Typing `zymi run deploy` in a directory you
  *thought* was a project runs the home one instead. Mitigated only by the
  stderr announcement; destructive pipelines should keep approval gates.
- **Minus: portability.** A `project.yml` with `llm: neuraldeep` only loads
  on a machine whose `providers.yml` defines `neuraldeep`. Inline `llm:`
  remains the portable form; named refs are machine-local by design. The
  error (raised when an agent step needs the model) says exactly what is
  missing.
- **Minus: action at a distance via `.env`.** A key set in `~/.zymi/.env`
  is visible to every project's `${env.*}`. Project and real env still win.
- **Minus: `mcp serve` on the home project is still all-or-nothing.** If the
  library gains an agent pipeline but `llm:` is absent (or its provider's
  key is unset), the whole MCP server fails to start, taking the tool-only
  pipelines with it. Narrowing that needs per-pipeline provider build in
  the runtime — deferred until it actually bites.
- `ConfigError::Parse`'s message now includes its detail: the CLI prints
  errors via Display, which dropped miette's `help` and left a bare
  "invalid YAML in …" for every config error, provider lookups included.
- `ProjectConfig.llm` keeps its type (`Option<LlmConfig>`); only the loader
  and the JSON schema (`zymi schema`) learn the string / `use:` forms.
- Следствие: agent-level `model:` in `agents/*.yml` is parsed but never read
  by the runtime today; a per-agent `model: neuraldeep/qwen3.8` reference
  would be the natural next step, but needs that field wired first.
