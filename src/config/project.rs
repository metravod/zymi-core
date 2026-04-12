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
}
