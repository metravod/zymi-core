# Declarative YAML configuration layer

Date: 2026-04-04

## Context

zymi-core had no way to declaratively define projects, agents, or pipelines. All configuration was programmatic (Rust structs). For the CLI (`zymi init/run`) and user-facing experience, we need human-readable config files with good error messages.

## Decision

Add `src/config/` module with:

- **3 YAML schemas**: `project.yml` (meta, LLM, policy, contracts, variables), `agents/*.yml` (agent definition), `pipelines/*.yml` (step DAG with depends_on)
- **serde_yml** for YAML parsing (maintained serde_yaml fork)
- **miette** for rich error diagnostics with source spans, error codes, and help text
- **Template variables**: `${env.X}` (env vars), `${project.name}` (project fields), `${var}` (project variables map), `${inputs.X}` (pipeline runtime — left unresolved at parse time)
- **Semantic validation**: agent tool names checked against known Intention tags, pipeline cross-references (agent exists, step IDs valid), DAG cycle detection via iterative DFS
- **Re-use** existing types: `PolicyConfig`, `FileWriteContract`, `RateLimitConfig` embedded directly in YAML structs

Module structure: `error.rs`, `template.rs`, `project.rs`, `agent.rs`, `pipeline.rs`, `validate.rs`, `mod.rs`.

Public API: `load_project_dir(root) -> Result<WorkspaceConfig, ConfigError>` loads and cross-validates the entire workspace.

## Consequences

- **Pro**: Foundation for CLI, DAG builder, and `zymi init --example`.
- **Pro**: miette gives users actionable error messages with source context.
- **Pro**: Template variables enable env-based config and pipeline parameterization.
- **Pro**: 32 new tests (137 total), 0 clippy warnings.
- **Con**: Adds serde_yml + miette dependencies (~50 transitive crates).
- **Con**: Template resolution happens on raw YAML string before parsing — edge cases possible with YAML special chars in variable values.
