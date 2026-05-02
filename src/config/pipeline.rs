use std::collections::HashMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::error::{parse_error, ConfigError};
use super::template;

/// Pipeline configuration (`pipelines/*.yml`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PipelineConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub inputs: Vec<PipelineInput>,
    pub steps: Vec<PipelineStep>,
    #[serde(default)]
    pub output: Option<PipelineOutput>,
    /// Per-pipeline approval channel override (ADR-0022 §"Resolution
    /// order"). When set, takes precedence over the project's
    /// `default_approval_channel:`. The value must match the `name:` of
    /// an entry under `project.yml::approvals:`.
    #[serde(default)]
    pub approval_channel: Option<String>,
}

/// A declared input parameter for the pipeline.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PipelineInput {
    pub name: String,
    #[serde(default = "default_input_type")]
    pub r#type: String,
    #[serde(default)]
    pub required: bool,
}

fn default_input_type() -> String {
    "string".into()
}

/// A single step in the pipeline DAG.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PipelineStep {
    pub id: String,
    pub agent: String,
    pub task: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// Declares which step produces the final pipeline output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PipelineOutput {
    pub step: String,
}

/// Load and parse a single pipeline YAML file.
///
/// `${inputs.*}` variables are left unresolved — they are filled at runtime.
pub fn load_pipeline(
    path: &Path,
    vars: &HashMap<String, String>,
) -> Result<PipelineConfig, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    let resolved = template::resolve_templates(&raw, vars, path)?;

    let config: PipelineConfig =
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
    fn minimal_pipeline() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: simple
steps:
  - id: step1
    agent: helper
    task: "do something"
"#;
        let path = write_file(&dir, "pipeline.yml", yaml);
        let config = load_pipeline(&path, &HashMap::new()).unwrap();
        assert_eq!(config.name, "simple");
        assert_eq!(config.steps.len(), 1);
        assert!(config.steps[0].depends_on.is_empty());
    }

    #[test]
    fn pipeline_with_dag() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: research
inputs:
  - name: query
    required: true
steps:
  - id: search
    agent: researcher
    task: "Search for: ${inputs.query}"
  - id: analyze
    agent: researcher
    task: "Analyze results"
    depends_on: [search]
  - id: summarize
    agent: researcher
    task: "Summarize"
    depends_on: [analyze]
  - id: review
    agent: researcher
    task: "Review"
    depends_on: [analyze]
output:
  step: summarize
"#;
        let path = write_file(&dir, "pipeline.yml", yaml);
        let config = load_pipeline(&path, &HashMap::new()).unwrap();
        assert_eq!(config.steps.len(), 4);
        assert_eq!(config.steps[1].depends_on, vec!["search"]);
        // review and summarize both depend on analyze — can run in parallel.
        assert_eq!(config.steps[2].depends_on, vec!["analyze"]);
        assert_eq!(config.steps[3].depends_on, vec!["analyze"]);
        assert_eq!(config.output.unwrap().step, "summarize");
    }

    #[test]
    fn pipeline_inputs_left_unresolved() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: test
steps:
  - id: s1
    agent: a
    task: "Query: ${inputs.q}"
"#;
        let path = write_file(&dir, "pipeline.yml", yaml);
        let config = load_pipeline(&path, &HashMap::new()).unwrap();
        assert!(config.steps[0].task.contains("${inputs.q}"));
    }

    #[test]
    fn pipeline_approval_channel_parses() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: deploy
approval_channel: ops_slack
steps:
  - id: ship
    agent: deployer
    task: "release"
"#;
        let path = write_file(&dir, "pipeline.yml", yaml);
        let config = load_pipeline(&path, &HashMap::new()).unwrap();
        assert_eq!(config.approval_channel.as_deref(), Some("ops_slack"));
    }

    #[test]
    fn pipeline_without_approval_channel_is_none() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: noop
steps:
  - id: s
    agent: a
    task: t
"#;
        let path = write_file(&dir, "pipeline.yml", yaml);
        let config = load_pipeline(&path, &HashMap::new()).unwrap();
        assert!(config.approval_channel.is_none());
    }

    #[test]
    fn pipeline_default_input_type() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: test
inputs:
  - name: query
steps:
  - id: s1
    agent: a
    task: "go"
"#;
        let path = write_file(&dir, "pipeline.yml", yaml);
        let config = load_pipeline(&path, &HashMap::new()).unwrap();
        assert_eq!(config.inputs[0].r#type, "string");
        assert!(!config.inputs[0].required);
    }
}
