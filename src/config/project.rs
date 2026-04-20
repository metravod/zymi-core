use std::collections::HashMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::esaa::contracts::{FileWriteContract, RateLimitConfig};
use crate::policy::PolicyConfig;

use super::error::{parse_error, ConfigError};
use super::template;

/// Top-level project configuration (`project.yml`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectConfig {
    pub name: String,
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
