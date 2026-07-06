# File contracts: secret deny-list for reads, and path normalization

Date: 2026-07-06

Context:

arikusi's review flagged the highest-severity leak in the codebase:

- `Intention::ReadFile` was **unconditionally approved** (`contracts.rs`). The
  write side denied `*.env`/`*.key`, but the read side denied nothing. The
  quickstart itself puts `TELEGRAM_BOT_TOKEN` and `OPENAI_API_KEY` in a `.env`
  in the project root. An agent could read that file, its full content is
  persisted forever in `ToolCallCompleted.result`, and it is fed to the LLM —
  a silent, permanent secrets leak with no redaction anywhere.
- `FileWriteContract` checked `path.starts_with(dir)` with **no
  canonicalization**, so `./memory/../../home/user/.ssh/config` was
  auto-approved as being "inside ./memory/".

Decision:

- **Secret deny-list applies to reads and writes, secure by default.** A
  built-in `BUILTIN_SECRET_DENY` list (`.env` variants, `*.key`/`*.pem`/PKCS
  bundles, SSH private keys, `.netrc`/`.pypirc`/`.npmrc`, `credentials`,
  `*.secret`) is always compiled in, *additive* to any configured
  `deny_patterns`. It holds even when the user configures nothing — security
  must not depend on the operator remembering to deny secrets. `ReadFile` now
  runs through this deny check instead of auto-approving; a match is `Denied`,
  not merely approval-gated, because a human clicking "approve" on a secret
  read still lands the secret in the log.

- **Reads stay otherwise permissive.** Reads are *not* gated on `allowed_dirs`
  — agents legitimately read across the tree, and forcing approval on every
  read would be unusable. The deny-list is the read boundary; `allowed_dirs`
  remains a write-only boundary.

- **Lexical path normalization.** Paths are normalized (resolve `.`/`..`
  component-wise) before matching, and `allowed_dirs` containment is checked on
  normalized components (`is_within`), not string `starts_with`. Normalization
  is *lexical* — it does not hit the filesystem, because write targets need not
  exist yet and we must not follow symlinks during a policy check. Deny
  patterns match against both the normalized full path and its basename, so
  name-style (`.env`) and path-style (`secrets/*`) patterns both work.

- **No config schema change.** `FileWriteContract` keeps its name and fields
  (`allowed_dirs`, `deny_patterns`); the config key `contracts.file_write` is
  unchanged. Behavior is extended, not the surface.

Consequences:

- Pros: the marquee leak is closed by default; `..` traversal can no longer
  escape an allowed dir; existing configs keep working with strictly more
  protection.
- Cons / limitations:
  - **Behavior change (breaking):** pipelines that previously read `.env` (or
    other listed files) through the agent's ReadFile now get a hard `Denied`.
    This is why it ships in **0.8.0**, not a patch. Operators who genuinely
    need such a read must load it outside the agent (env vars, a redaction
    step) — reading a secret into the event log is exactly what we're stopping.
  - **Symlinks are not resolved.** A symlink inside an allowed dir that points
    outside it defeats the lexical `is_within` check. Resolving symlinks
    safely (without TOCTOU) is a filesystem operation deferred to a later
    slice; the lexical fix closes the reported string-traversal bypass.
  - The built-in list is deliberately conservative; it is not a complete
    secret taxonomy. Operators layer project-specific patterns via
    `deny_patterns`.
