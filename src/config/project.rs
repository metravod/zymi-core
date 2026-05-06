use std::collections::HashMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::esaa::contracts::{FileWriteContract, RateLimitConfig};
use crate::policy::PolicyConfig;

use super::error::{parse_error, ConfigError};
use super::template;

/// Current YAML schema version. Bumped per the semver contract in ADR-0020:
/// adding a new `type:` or optional field bumps the minor segment; removing
/// or renaming a `type:` or required field bumps the major segment. `"1"` is
/// the initial stable contract cut at P2 (v0.3).
pub const SCHEMA_VERSION: &str = "1";

/// Top-level project configuration (`project.yml`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectConfig {
    pub name: String,
    /// YAML-schema version this project targets. See [`SCHEMA_VERSION`] and
    /// ADR-0020 for the compatibility contract. Omitting the field is
    /// equivalent to "pinned to whatever the running zymi-core supports" —
    /// tolerated for backwards compatibility but warned about in CLI
    /// validation (future slice). New projects scaffolded by `zymi init`
    /// set this field explicitly.
    #[serde(default, rename = "schema_version")]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub variables: HashMap<String, String>,
    #[serde(default)]
    pub llm: Option<LlmConfig>,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub contracts: ContractsConfig,
    #[serde(default)]
    pub runtime: Option<RuntimeConfig>,
    #[serde(default)]
    pub services: Option<ServicesConfig>,
    /// MCP servers to spawn at startup (ADR-0023). Each entry produces
    /// one subprocess whose advertised tools are registered in the
    /// `ToolCatalog` under `mcp__<name>__<tool>` ids.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Inbound connectors — event sources (http_inbound, http_poll, ...).
    /// Dispatched at runtime by the `InboundConnector` plugin registry
    /// (ADR-0020 / ADR-0021). Stored as raw YAML so this crate can parse
    /// projects without pulling in connector dependencies; the JSON
    /// schema therefore exposes them as a permissive array of objects
    /// (each entry's `type:` selects a plugin whose own shape is
    /// validated at runtime).
    #[serde(default)]
    #[schemars(with = "Vec<serde_json::Value>")]
    pub connectors: Vec<serde_yml::Value>,
    /// Outbound sinks — event consumers (http_post, ...). Dispatched via
    /// the `OutboundSink` plugin registry. Stored as raw YAML for the
    /// same reason as `connectors`; permissive JSON schema for the same
    /// reason.
    #[serde(default)]
    #[schemars(with = "Vec<serde_json::Value>")]
    pub outputs: Vec<serde_yml::Value>,
    /// Approval channels (ADR-0022). Each entry is dispatched at runtime
    /// by the `ApprovalChannel` plugin registry: terminal prompt, HTTP
    /// endpoint, Telegram bot, etc. Stored as raw YAML so this crate
    /// can parse projects without pulling in webhook/connectors deps;
    /// permissive JSON schema for the same reason.
    #[serde(default)]
    #[schemars(with = "Vec<serde_json::Value>")]
    pub approvals: Vec<serde_yml::Value>,
    /// Project-wide default approval channel name (ADR-0022 §"Resolution
    /// order"). Used when a pipeline does not declare its own
    /// `approval_channel:`. `None` means: if `--approval=` is also
    /// unset and no `approvals:` section is configured, the runtime
    /// auto-spawns a terminal channel when stdin is attached
    /// (zero-config UX); otherwise the orchestrator fail-closes.
    #[serde(default)]
    pub default_approval_channel: Option<String>,
    /// Override the event store backend (ADR-0012). Accepts:
    ///
    /// * `sqlite` or omitted → embedded SQLite at `<root>/.zymi/events.db`
    ///   (the zero-config default).
    /// * `postgres://user:pass@host/db` → networked Postgres backend.
    ///   Requires the `postgres` Cargo feature; rebuild the CLI with
    ///   `--features postgres` if you set this and see a "not compiled in"
    ///   error.
    ///
    /// Templated like every other secret-bearing field — write
    /// `store: ${env.DATABASE_URL}` and keep the URL out of the YAML.
    #[serde(default)]
    pub store: Option<String>,
}

/// One entry in the top-level `mcp_servers:` list (ADR-0023).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpServerConfig {
    /// Logical server name. Becomes the `<server>` segment of every
    /// catalog id produced for this server. Must not contain `__`.
    pub name: String,
    /// argv for the server subprocess. First element is the program.
    pub command: Vec<String>,
    /// Explicit environment variables. Parent env is **not** inherited
    /// by default (ADR-0023 §security posture).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Allow-list of tool names to import. Mutually exclusive with
    /// `deny`. Omitting both loads zero tools from the server with a
    /// startup warning (opt-in UX).
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    /// Deny-list of tool names to skip. Mutually exclusive with `allow`.
    #[serde(default)]
    pub deny: Option<Vec<String>>,
    /// If `true`, every tool imported from this server requires human
    /// approval by default (per-tool overrides still go through the
    /// policy engine). Defaults to `false`.
    #[serde(default)]
    pub requires_approval: Option<bool>,
    /// Timeout for the `initialize` handshake. Defaults to 10s.
    #[serde(default)]
    pub init_timeout_secs: Option<u64>,
    /// Timeout for individual `tools/list` / `tools/call` requests.
    /// Defaults to 60s.
    #[serde(default)]
    pub call_timeout_secs: Option<u64>,
    /// Restart policy on subprocess crash. Absent = no auto-restart:
    /// a transport failure surfaces to the caller and the server stays
    /// down until the runtime is restarted.
    #[serde(default)]
    pub restart: Option<McpRestartConfig>,
}

/// Per-server restart policy (ADR-0023 §Lifecycle). Reactive: a crash is
/// detected on the first `tools/call` after the subprocess exits, and the
/// registry re-spawns + re-handshakes before retrying the call.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpRestartConfig {
    /// Maximum number of restart attempts over the lifetime of the
    /// runtime. `0` or unset is equivalent to omitting `restart:`.
    #[serde(default)]
    pub max_restarts: Option<u32>,
    /// Per-attempt backoff in seconds. `[1, 5, 30]` means attempt 1
    /// waits 1s, attempt 2 waits 5s, attempt 3 (and later, if any) waits
    /// 30s. Empty or unset defaults to `[1]`.
    #[serde(default)]
    pub backoff_secs: Option<Vec<u64>>,
}

impl ProjectConfig {
    /// Resolve the configured `store:` URL into a [`StoreBackend`],
    /// falling back to the embedded SQLite default at `<root>/.zymi/events.db`.
    ///
    /// Recognised forms:
    /// * absent / empty / `sqlite` → `Sqlite { path: <root>/.zymi/events.db }`
    /// * `sqlite:</absolute/path>` or `sqlite:<relative/path>` →
    ///   `Sqlite { path: <resolved> }`
    /// * `postgres://…` or `postgresql://…` → `Postgres { url }`
    ///
    /// Returns `Err` for an unrecognised scheme rather than silently
    /// falling through to the SQLite default — typos in the URL should
    /// surface at startup, not after several green-path operations.
    pub fn resolve_store_backend(
        &self,
        project_root: &Path,
    ) -> Result<crate::events::store::StoreBackend, ConfigError> {
        use crate::events::store::StoreBackend;

        let default_sqlite = || StoreBackend::Sqlite {
            path: project_root.join(".zymi").join("events.db"),
        };

        let raw = match self.store.as_deref().map(str::trim) {
            None | Some("") | Some("sqlite") => return Ok(default_sqlite()),
            Some(s) => s,
        };

        if let Some(rest) = raw
            .strip_prefix("postgres://")
            .or_else(|| raw.strip_prefix("postgresql://"))
        {
            let _ = rest; // shape check only — full parsing happens in tokio-postgres.
            return Ok(StoreBackend::Postgres { url: raw.to_string() });
        }

        if let Some(path_str) = raw.strip_prefix("sqlite:") {
            let path = std::path::PathBuf::from(path_str);
            let resolved = if path.is_absolute() {
                path
            } else {
                project_root.join(path)
            };
            return Ok(StoreBackend::Sqlite { path: resolved });
        }

        Err(ConfigError::UnsupportedStoreUrl {
            url: raw.to_string(),
        })
    }
}

impl McpServerConfig {
    /// Effective init-handshake timeout after applying the default.
    pub fn effective_init_timeout_secs(&self) -> u64 {
        self.init_timeout_secs.unwrap_or(10)
    }

    /// Effective per-call timeout after applying the default.
    pub fn effective_call_timeout_secs(&self) -> u64 {
        self.call_timeout_secs.unwrap_or(60)
    }
}

impl McpRestartConfig {
    /// Resolved max-restarts value. `None` or `Some(0)` → 0.
    pub fn effective_max_restarts(&self) -> u32 {
        self.max_restarts.unwrap_or(0)
    }

    /// Backoff duration for the given 1-based attempt. Past the end of
    /// the list, reuses the last element. Empty list defaults to 1s.
    pub fn backoff_for_attempt(&self, attempt: u32) -> u64 {
        let list = self.backoff_secs.as_deref().unwrap_or(&[]);
        if list.is_empty() {
            return 1;
        }
        let idx = (attempt.saturating_sub(1) as usize).min(list.len() - 1);
        list[idx]
    }
}

/// LLM provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LlmConfig {
    pub provider: String,
    #[serde(default)]
    pub base_url: Option<String>,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Default values inherited by agents unless overridden.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DefaultsConfig {
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_timeout(),
            max_iterations: default_max_iterations(),
        }
    }
}

fn default_timeout() -> u64 {
    30
}
fn default_max_iterations() -> usize {
    10
}

/// Boundary contract configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ContractsConfig {
    #[serde(default)]
    pub file_write: FileWriteContract,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
}

/// Runtime configuration block (`runtime:` in `project.yml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeConfig {
    /// Persistent shell session settings (ADR-0015).
    #[serde(default)]
    pub shell: ShellConfig,
    /// Context window management settings (ADR-0016).
    #[serde(default)]
    pub context: ContextWindowConfig,
}

/// Context window management configuration (ADR-0016 §5).
///
/// Controls observation masking, soft/hard caps, and compaction
/// behaviour. Values here map 1:1 to [`crate::runtime::context_builder::ContextConfig`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContextWindowConfig {
    /// Number of recent turns whose tool results are kept verbatim.
    /// Older observations are replaced with a compact placeholder.
    #[serde(default = "default_observation_window")]
    pub observation_window: usize,

    /// Character count that triggers hybrid compaction (LLM summarization
    /// of the oldest masked batch). Only fires when a provider is wired.
    #[serde(default = "default_soft_cap_chars")]
    pub soft_cap_chars: usize,

    /// Absolute character limit. Exceeding this after compaction is a
    /// fatal `HardCapExceeded` error that stops the agent step.
    #[serde(default = "default_hard_cap_chars")]
    pub hard_cap_chars: usize,

    /// Minimum recent turns preserved even under compaction pressure.
    #[serde(default = "default_min_tail_turns")]
    pub min_tail_turns: usize,
}

impl Default for ContextWindowConfig {
    fn default() -> Self {
        Self {
            observation_window: default_observation_window(),
            soft_cap_chars: default_soft_cap_chars(),
            hard_cap_chars: default_hard_cap_chars(),
            min_tail_turns: default_min_tail_turns(),
        }
    }
}

fn default_observation_window() -> usize {
    10
}
fn default_soft_cap_chars() -> usize {
    400_000
}
fn default_hard_cap_chars() -> usize {
    600_000
}
fn default_min_tail_turns() -> usize {
    4
}

/// Configuration for persistent shell sessions (ADR-0015 §6).
///
/// Controls the `ShellSessionPool` that backs `execute_shell_command`
/// and declarative `kind: shell` tools.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ShellConfig {
    /// Seconds a session can be idle before the reaper kills it.
    #[serde(default = "default_shell_idle_timeout")]
    pub idle_timeout_seconds: u64,

    /// Maximum number of concurrent shell sessions. When exceeded the
    /// least-recently-used session is evicted.
    #[serde(default = "default_shell_max_sessions")]
    pub max_sessions: usize,

    /// Path to the shell binary. Falls back to `sh` if the configured
    /// path is not executable.
    #[serde(default = "default_shell_path")]
    pub shell_path: String,

    /// Pass `-i` (interactive) to the shell on spawn. When `true`,
    /// the user's shell init (PS1, aliases) runs — useful for coding
    /// agents that need a realistic shell, but slower to start.
    #[serde(default = "default_shell_interactive")]
    pub interactive: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            idle_timeout_seconds: default_shell_idle_timeout(),
            max_sessions: default_shell_max_sessions(),
            shell_path: default_shell_path(),
            interactive: default_shell_interactive(),
        }
    }
}

fn default_shell_idle_timeout() -> u64 {
    600
}
fn default_shell_max_sessions() -> usize {
    32
}
fn default_shell_path() -> String {
    "/bin/bash".to_string()
}
fn default_shell_interactive() -> bool {
    false
}

/// Services configuration — external integrations that subscribe to the event bus.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ServicesConfig {
    #[serde(default)]
    pub langfuse: Option<LangfuseConfig>,
}

/// LangFuse observability service configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LangfuseConfig {
    /// LangFuse API base URL.
    #[serde(default = "default_langfuse_base_url")]
    pub base_url: String,
    /// Public key for Basic auth (supports `${env.*}` templates).
    pub public_key: String,
    /// Secret key for Basic auth (supports `${env.*}` templates).
    pub secret_key: String,
    /// Max events to buffer before flushing to LangFuse.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    /// Max seconds between flushes.
    #[serde(default = "default_flush_interval_secs")]
    pub flush_interval_secs: u64,
}

fn default_langfuse_base_url() -> String {
    "https://cloud.langfuse.com".into()
}

fn default_batch_size() -> usize {
    10
}

fn default_flush_interval_secs() -> u64 {
    5
}

/// Load and parse `project.yml` from the given path.
///
/// Template variables in the file are resolved in two passes:
/// 1. `${env.*}` is resolved immediately from the environment.
/// 2. After parsing, `${project.*}` and plain `${var}` are resolved from
///    the project's own `variables` map (for use by agent/pipeline files).
pub fn load_project(path: &Path) -> Result<ProjectConfig, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    // First pass: resolve ${env.*} only (variables map not yet available).
    let env_vars = HashMap::new();
    let resolved = template::resolve_templates(&raw, &env_vars, path)
        // If there are unresolved plain vars, that's expected — they'll be in the variables map.
        // So we fall back to the raw string for non-env vars.
        .unwrap_or_else(|_| raw.clone());

    let config: ProjectConfig =
        serde_yml::from_str(&resolved).map_err(|e| parse_error(path, &resolved, e))?;

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn minimal_project() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "project.yml", "name: test-project\n");
        let config = load_project(&path).unwrap();
        assert_eq!(config.name, "test-project");
        assert_eq!(config.defaults.timeout_secs, 30);
        assert_eq!(config.defaults.max_iterations, 10);
    }

    #[test]
    fn full_project() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: my-project
version: "0.2"
variables:
  default_model: gpt-4o
llm:
  provider: openai
  model: gpt-4o
defaults:
  timeout_secs: 60
  max_iterations: 5
policy:
  enabled: true
  allow: ["ls *"]
  deny: ["rm -rf *"]
contracts:
  file_write:
    allowed_dirs: ["./output"]
    deny_patterns: ["*.env"]
  rate_limit:
    shell_commands_per_minute: 30
"#;
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        assert_eq!(config.name, "my-project");
        assert_eq!(config.version.as_deref(), Some("0.2"));
        assert_eq!(config.defaults.timeout_secs, 60);
        assert!(config.policy.enabled);
        assert_eq!(config.contracts.file_write.allowed_dirs, vec!["./output"]);
        assert_eq!(config.contracts.rate_limit.shell_commands_per_minute, 30);
    }

    #[test]
    fn missing_file_returns_io_error() {
        let err = load_project(Path::new("/nonexistent/project.yml")).unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn invalid_yaml_returns_parse_error() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "project.yml", "name: [invalid yaml");
        let err = load_project(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn project_with_services() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: test-project
services:
  langfuse:
    public_key: pk-test
    secret_key: sk-test
    batch_size: 20
"#;
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        let langfuse = config.services.unwrap().langfuse.unwrap();
        assert_eq!(langfuse.public_key, "pk-test");
        assert_eq!(langfuse.secret_key, "sk-test");
        assert_eq!(langfuse.base_url, "https://cloud.langfuse.com");
        assert_eq!(langfuse.batch_size, 20);
        assert_eq!(langfuse.flush_interval_secs, 5);
    }

    #[test]
    fn project_without_services_is_none() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "project.yml", "name: test\n");
        let config = load_project(&path).unwrap();
        assert!(config.services.is_none());
    }

    #[test]
    fn env_var_resolution() {
        unsafe { std::env::set_var("ZYMI_TEST_KEY", "sk-test") };
        let dir = TempDir::new().unwrap();
        let yaml = "name: test\nllm:\n  provider: openai\n  model: gpt-4o\n  api_key: ${env.ZYMI_TEST_KEY}\n";
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        assert_eq!(config.llm.unwrap().api_key.as_deref(), Some("sk-test"));
        unsafe { std::env::remove_var("ZYMI_TEST_KEY") };
    }

    #[test]
    fn project_without_runtime_uses_defaults() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "project.yml", "name: test\n");
        let config = load_project(&path).unwrap();
        assert!(config.runtime.is_none());

        // ShellConfig::default() should match ADR-0015 §6 values.
        let shell = ShellConfig::default();
        assert_eq!(shell.idle_timeout_seconds, 600);
        assert_eq!(shell.max_sessions, 32);
        assert_eq!(shell.shell_path, "/bin/bash");
        assert!(!shell.interactive);
    }

    #[test]
    fn project_with_runtime_shell_config() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: test
runtime:
  shell:
    idle_timeout_seconds: 120
    max_sessions: 8
    shell_path: /bin/sh
    interactive: true
"#;
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        let rt = config.runtime.unwrap();
        assert_eq!(rt.shell.idle_timeout_seconds, 120);
        assert_eq!(rt.shell.max_sessions, 8);
        assert_eq!(rt.shell.shell_path, "/bin/sh");
        assert!(rt.shell.interactive);
    }

    #[test]
    fn project_with_partial_runtime_shell_config() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: test
runtime:
  shell:
    idle_timeout_seconds: 300
"#;
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        let rt = config.runtime.unwrap();
        assert_eq!(rt.shell.idle_timeout_seconds, 300);
        // Rest should be defaults.
        assert_eq!(rt.shell.max_sessions, 32);
        assert_eq!(rt.shell.shell_path, "/bin/bash");
        assert!(!rt.shell.interactive);
    }

    #[test]
    fn project_with_runtime_context_config() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: test
runtime:
  context:
    observation_window: 5
    soft_cap_chars: 200000
    hard_cap_chars: 500000
    min_tail_turns: 2
"#;
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        let rt = config.runtime.unwrap();
        assert_eq!(rt.context.observation_window, 5);
        assert_eq!(rt.context.soft_cap_chars, 200_000);
        assert_eq!(rt.context.hard_cap_chars, 500_000);
        assert_eq!(rt.context.min_tail_turns, 2);
    }

    #[test]
    fn project_with_partial_runtime_context_config() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: test
runtime:
  context:
    observation_window: 20
"#;
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        let rt = config.runtime.unwrap();
        assert_eq!(rt.context.observation_window, 20);
        // Rest should be defaults.
        assert_eq!(rt.context.soft_cap_chars, 400_000);
        assert_eq!(rt.context.hard_cap_chars, 600_000);
        assert_eq!(rt.context.min_tail_turns, 4);
    }

    #[test]
    fn project_with_mcp_servers_parses_full_entry() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: test
mcp_servers:
  - name: github
    command: ["npx", "-y", "@modelcontextprotocol/server-github"]
    env:
      GITHUB_PERSONAL_ACCESS_TOKEN: xxx
    allow: ["create_issue", "add_issue_comment"]
  - name: postgres
    command: ["npx", "-y", "@modelcontextprotocol/server-postgres", "postgres://localhost/db"]
    deny: ["drop_table"]
    requires_approval: true
"#;
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        assert_eq!(config.mcp_servers.len(), 2);

        let gh = &config.mcp_servers[0];
        assert_eq!(gh.name, "github");
        assert_eq!(gh.command[0], "npx");
        assert_eq!(gh.env.get("GITHUB_PERSONAL_ACCESS_TOKEN").unwrap(), "xxx");
        assert_eq!(gh.allow.as_deref().unwrap(), &["create_issue", "add_issue_comment"]);
        assert!(gh.deny.is_none());
        assert!(gh.requires_approval.is_none());

        let pg = &config.mcp_servers[1];
        assert_eq!(pg.name, "postgres");
        assert_eq!(pg.deny.as_deref().unwrap(), &["drop_table"]);
        assert_eq!(pg.requires_approval, Some(true));
    }

    #[test]
    fn project_without_mcp_servers_is_empty_vec() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "project.yml", "name: test\n");
        let config = load_project(&path).unwrap();
        assert!(config.mcp_servers.is_empty());
    }

    #[test]
    fn mcp_server_timeouts_and_restart_parse() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: test
mcp_servers:
  - name: slow
    command: ["./slow-server"]
    init_timeout_secs: 30
    call_timeout_secs: 120
    restart:
      max_restarts: 3
      backoff_secs: [1, 5, 30]
"#;
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        let srv = &config.mcp_servers[0];
        assert_eq!(srv.init_timeout_secs, Some(30));
        assert_eq!(srv.call_timeout_secs, Some(120));
        assert_eq!(srv.effective_init_timeout_secs(), 30);
        assert_eq!(srv.effective_call_timeout_secs(), 120);
        let restart = srv.restart.as_ref().unwrap();
        assert_eq!(restart.effective_max_restarts(), 3);
        assert_eq!(restart.backoff_for_attempt(1), 1);
        assert_eq!(restart.backoff_for_attempt(2), 5);
        assert_eq!(restart.backoff_for_attempt(3), 30);
        // Past the list end → stays at the tail value.
        assert_eq!(restart.backoff_for_attempt(7), 30);
    }

    #[test]
    fn mcp_server_timeout_defaults_when_unset() {
        let cfg = McpServerConfig {
            name: "x".into(),
            command: vec!["true".into()],
            env: HashMap::new(),
            allow: None,
            deny: None,
            requires_approval: None,
            init_timeout_secs: None,
            call_timeout_secs: None,
            restart: None,
        };
        assert_eq!(cfg.effective_init_timeout_secs(), 10);
        assert_eq!(cfg.effective_call_timeout_secs(), 60);
    }

    #[test]
    fn mcp_restart_empty_backoff_defaults_to_one_second() {
        let r = McpRestartConfig {
            max_restarts: Some(2),
            backoff_secs: Some(Vec::new()),
        };
        assert_eq!(r.backoff_for_attempt(1), 1);
        assert_eq!(r.backoff_for_attempt(99), 1);
    }

    #[test]
    fn mcp_restart_missing_backoff_defaults_to_one_second() {
        let r = McpRestartConfig {
            max_restarts: Some(1),
            backoff_secs: None,
        };
        assert_eq!(r.backoff_for_attempt(1), 1);
    }

    #[test]
    fn mcp_restart_unset_max_is_zero() {
        let r = McpRestartConfig {
            max_restarts: None,
            backoff_secs: None,
        };
        assert_eq!(r.effective_max_restarts(), 0);
    }

    #[test]
    fn project_with_connectors_and_outputs_parses_raw() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: telegram-bot
connectors:
  - type: http_poll
    name: telegram_updates
    url: "https://api.telegram.org/bot${env.TELEGRAM_BOT_TOKEN}/getUpdates"
    interval_secs: 2
    extract:
      items:    "$.result[*]"
      stream_id: "$.message.chat.id"
      content:   "$.message.text"
    publishes: UserMessageReceived
outputs:
  - type: http_post
    name: telegram_reply
    on: [ResponseReady]
    url: "https://api.telegram.org/bot${env.TELEGRAM_BOT_TOKEN}/sendMessage"
    body_template: '{"chat_id":"{{ event.stream_id }}","text":"{{ event.content }}"}'
"#;
        // Stub the env var so template resolution doesn't fail the first pass.
        unsafe { std::env::set_var("TELEGRAM_BOT_TOKEN", "123:abc") };
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        unsafe { std::env::remove_var("TELEGRAM_BOT_TOKEN") };

        assert_eq!(config.connectors.len(), 1);
        assert_eq!(config.outputs.len(), 1);

        let inbound = &config.connectors[0];
        let as_map = inbound.as_mapping().expect("connector entry is mapping");
        assert_eq!(
            as_map
                .get(serde_yml::Value::String("type".into()))
                .and_then(|v| v.as_str()),
            Some("http_poll")
        );

        let outbound = &config.outputs[0];
        let as_map = outbound.as_mapping().expect("output entry is mapping");
        assert_eq!(
            as_map
                .get(serde_yml::Value::String("name".into()))
                .and_then(|v| v.as_str()),
            Some("telegram_reply")
        );
    }

    #[test]
    fn project_without_connectors_is_empty_vec() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "project.yml", "name: test\n");
        let config = load_project(&path).unwrap();
        assert!(config.connectors.is_empty());
        assert!(config.outputs.is_empty());
    }

    #[test]
    fn project_with_approvals_section_parses_raw() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: ops
default_approval_channel: ops_slack
approvals:
  - type: terminal
    name: local
  - type: http
    name: ops_api
    bind: "127.0.0.1:8081"
    bearer_token: "${env.APPROVAL_TOKEN}"
"#;
        unsafe { std::env::set_var("APPROVAL_TOKEN", "tok-1") };
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        unsafe { std::env::remove_var("APPROVAL_TOKEN") };

        assert_eq!(config.default_approval_channel.as_deref(), Some("ops_slack"));
        assert_eq!(config.approvals.len(), 2);
        let first = config.approvals[0].as_mapping().unwrap();
        assert_eq!(
            first.get(serde_yml::Value::String("name".into())).and_then(|v| v.as_str()),
            Some("local")
        );
    }

    #[test]
    fn project_without_approvals_defaults_empty_and_no_default_channel() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "project.yml", "name: t\n");
        let config = load_project(&path).unwrap();
        assert!(config.approvals.is_empty());
        assert!(config.default_approval_channel.is_none());
    }

    #[test]
    fn project_without_runtime_context_uses_defaults() {
        let dir = TempDir::new().unwrap();
        let yaml = "name: test\nruntime:\n  shell:\n    max_sessions: 16\n";
        let path = write_file(&dir, "project.yml", yaml);
        let config = load_project(&path).unwrap();
        let rt = config.runtime.unwrap();
        let ctx = rt.context;
        assert_eq!(ctx.observation_window, 10);
        assert_eq!(ctx.soft_cap_chars, 400_000);
        assert_eq!(ctx.hard_cap_chars, 600_000);
        assert_eq!(ctx.min_tail_turns, 4);
    }
}
