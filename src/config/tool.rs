use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::error::{parse_error, ConfigError};
use super::template;

/// Custom tool configuration (`tools/*.yml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolConfig {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's parameters (sent to the LLM).
    pub parameters: serde_json::Value,
    /// Whether this tool requires human approval before execution.
    /// Defaults depend on implementation kind (see [`ImplementationConfig`]).
    #[serde(default)]
    pub requires_approval: Option<bool>,
    /// The implementation backend for this tool.
    pub implementation: ImplementationConfig,
}

/// Implementation backend for a declarative tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ImplementationConfig {
    /// HTTP request. `${args.X}` placeholders in url, headers, and
    /// body_template are resolved at call time from the LLM's arguments.
    #[serde(rename = "http")]
    Http {
        method: HttpMethod,
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        body_template: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl ToolConfig {
    /// Effective `requires_approval` value, considering the implementation kind default.
    pub fn effective_requires_approval(&self) -> bool {
        self.requires_approval.unwrap_or(match &self.implementation {
            ImplementationConfig::Http { .. } => false,
            // Future: kind: shell defaults to true
        })
    }
}

/// Load and parse a single tool YAML file.
pub fn load_tool(
    path: &Path,
    vars: &HashMap<String, String>,
) -> Result<ToolConfig, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    // Resolve ${env.X}, ${project.X}, ${var} at parse time.
    // ${args.X} must be left unresolved for call-time substitution.
    let resolved = template::resolve_templates(&raw, vars, path)?;

    let config: ToolConfig =
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
    fn parse_http_tool() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: slack_post
description: "Post a message to Slack"
parameters:
  type: object
  properties:
    channel:
      type: string
    text:
      type: string
  required: [channel, text]
implementation:
  kind: http
  method: POST
  url: "https://slack.com/api/chat.postMessage"
  headers:
    Content-Type: "application/json"
  body_template: '{"channel": "${args.channel}", "text": "${args.text}"}'
"#;
        let path = write_file(&dir, "slack_post.yml", yaml);
        let config = load_tool(&path, &HashMap::new()).unwrap();
        assert_eq!(config.name, "slack_post");
        assert_eq!(config.description, "Post a message to Slack");
        assert!(!config.effective_requires_approval());
        match &config.implementation {
            ImplementationConfig::Http { method, url, headers, body_template } => {
                assert!(matches!(method, HttpMethod::Post));
                assert_eq!(url, "https://slack.com/api/chat.postMessage");
                assert_eq!(headers.get("Content-Type").unwrap(), "application/json");
                assert!(body_template.as_ref().unwrap().contains("${args.channel}"));
            }
        }
    }

    #[test]
    fn parse_minimal_http_tool() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: ping
description: "Ping an endpoint"
parameters:
  type: object
  properties:
    url: { type: string }
  required: [url]
implementation:
  kind: http
  method: GET
  url: "${args.url}"
"#;
        let path = write_file(&dir, "ping.yml", yaml);
        let config = load_tool(&path, &HashMap::new()).unwrap();
        assert_eq!(config.name, "ping");
        match &config.implementation {
            ImplementationConfig::Http { method, body_template, .. } => {
                assert!(matches!(method, HttpMethod::Get));
                assert!(body_template.is_none());
            }
        }
    }

    #[test]
    fn requires_approval_override() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: sensitive_api
description: "A sensitive API call"
parameters:
  type: object
  properties: {}
requires_approval: true
implementation:
  kind: http
  method: POST
  url: "https://example.com/sensitive"
"#;
        let path = write_file(&dir, "sensitive.yml", yaml);
        let config = load_tool(&path, &HashMap::new()).unwrap();
        assert!(config.effective_requires_approval());
    }

    #[test]
    fn env_var_resolution_in_tool() {
        let dir = TempDir::new().unwrap();
        unsafe { std::env::set_var("ZYMI_TEST_TOKEN", "secret123") };
        let yaml = r#"
name: auth_api
description: "API with auth"
parameters:
  type: object
  properties: {}
implementation:
  kind: http
  method: GET
  url: "https://api.example.com"
  headers:
    Authorization: "Bearer ${env.ZYMI_TEST_TOKEN}"
"#;
        let path = write_file(&dir, "auth.yml", yaml);
        let config = load_tool(&path, &HashMap::new()).unwrap();
        match &config.implementation {
            ImplementationConfig::Http { headers, .. } => {
                assert_eq!(headers.get("Authorization").unwrap(), "Bearer secret123");
            }
        }
        unsafe { std::env::remove_var("ZYMI_TEST_TOKEN") };
    }
}
