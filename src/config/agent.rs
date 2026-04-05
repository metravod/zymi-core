use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::policy::PolicyConfig;

use super::error::{parse_error, ConfigError};
use super::template;

/// Agent configuration (`agents/*.yml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub max_iterations: Option<usize>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub policy: Option<PolicyConfig>,
}

/// Valid tool names that map to [`crate::esaa::Intention`] variants.
pub const KNOWN_TOOLS: &[&str] = &[
    "execute_shell_command",
    "write_file",
    "read_file",
    "web_search",
    "web_scrape",
    "write_memory",
    "spawn_sub_agent",
];

/// Load and parse a single agent YAML file.
pub fn load_agent(
    path: &Path,
    vars: &HashMap<String, String>,
) -> Result<AgentConfig, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    let resolved = template::resolve_templates(&raw, vars, path)?;

    let config: AgentConfig =
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
    fn minimal_agent() {
        let dir = TempDir::new().unwrap();
        let path = write_file(&dir, "agent.yml", "name: helper\n");
        let config = load_agent(&path, &HashMap::new()).unwrap();
        assert_eq!(config.name, "helper");
        assert!(config.tools.is_empty());
    }

    #[test]
    fn full_agent() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: researcher
description: "Research agent"
model: gpt-4o
system_prompt: "You are a researcher."
tools:
  - web_search
  - web_scrape
  - write_memory
max_iterations: 5
timeout_secs: 60
policy:
  enabled: true
  deny: ["rm *"]
"#;
        let path = write_file(&dir, "agent.yml", yaml);
        let config = load_agent(&path, &HashMap::new()).unwrap();
        assert_eq!(config.name, "researcher");
        assert_eq!(config.tools, vec!["web_search", "web_scrape", "write_memory"]);
        assert_eq!(config.max_iterations, Some(5));
        assert!(config.policy.unwrap().enabled);
    }

    #[test]
    fn agent_with_template_vars() {
        let dir = TempDir::new().unwrap();
        let yaml = "name: bot\nmodel: ${default_model}\n";
        let mut vars = HashMap::new();
        vars.insert("default_model".into(), "gpt-4o".into());
        let path = write_file(&dir, "agent.yml", yaml);
        let config = load_agent(&path, &vars).unwrap();
        assert_eq!(config.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn agent_unresolved_var_errors() {
        let dir = TempDir::new().unwrap();
        let yaml = "name: bot\nmodel: ${nonexistent}\n";
        let path = write_file(&dir, "agent.yml", yaml);
        let err = load_agent(&path, &HashMap::new()).unwrap_err();
        assert!(matches!(err, ConfigError::UnresolvedVariable { .. }));
    }
}
