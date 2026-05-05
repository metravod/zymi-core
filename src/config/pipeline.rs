use std::collections::HashMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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
///
/// A step is either an *agent* step (LLM ReAct loop, ADR pre-0024) or a
/// *deterministic tool* step (ADR-0024). Discriminated by which key is
/// present (`agent:` vs `tool:`); ambiguity / emptiness fail at parse time.
#[derive(Debug, Clone)]
pub struct PipelineStep {
    pub id: String,
    pub kind: PipelineStepKind,
    pub depends_on: Vec<String>,
}

/// Step body. The two variants are mutually exclusive at the YAML level.
#[derive(Debug, Clone)]
pub enum PipelineStepKind {
    /// LLM ReAct step: the agent plans tool calls itself.
    Agent { agent: String, task: String },
    /// Deterministic tool step (ADR-0024): orchestrator dispatches the
    /// named tool with templated args, no LLM hop.
    Tool {
        tool: String,
        args: serde_yml::Value,
    },
}

impl PipelineStep {
    /// Convenience for legacy callers / tests that only care about agent
    /// steps. `None` for tool steps.
    pub fn agent_name(&self) -> Option<&str> {
        match &self.kind {
            PipelineStepKind::Agent { agent, .. } => Some(agent.as_str()),
            PipelineStepKind::Tool { .. } => None,
        }
    }
}

/// Wire format. Both arms' fields are optional so we can detect ambiguous
/// (`agent:` + `tool:`) and empty configs ourselves with a clear error
/// message instead of relying on serde's "did not match any variant" output.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
struct PipelineStepRaw {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub args: Option<serde_yml::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

impl<'de> Deserialize<'de> for PipelineStep {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = PipelineStepRaw::deserialize(d)?;
        let has_agent = raw.agent.is_some() || raw.task.is_some();
        let has_tool = raw.tool.is_some() || raw.args.is_some();
        let kind = match (has_agent, has_tool) {
            (true, true) => {
                return Err(D::Error::custom(format!(
                    "step `{}`: cannot specify both `agent:` and `tool:` (ADR-0024)",
                    raw.id
                )));
            }
            (false, false) => {
                return Err(D::Error::custom(format!(
                    "step `{}`: must specify either `agent:` + `task:` or `tool:` (ADR-0024)",
                    raw.id
                )));
            }
            (true, false) => {
                let agent = raw.agent.ok_or_else(|| {
                    D::Error::custom(format!("step `{}`: `agent:` is required", raw.id))
                })?;
                let task = raw.task.ok_or_else(|| {
                    D::Error::custom(format!(
                        "step `{}`: `task:` is required for agent steps",
                        raw.id
                    ))
                })?;
                PipelineStepKind::Agent { agent, task }
            }
            (false, true) => {
                let tool = raw.tool.ok_or_else(|| {
                    D::Error::custom(format!(
                        "step `{}`: `tool:` is required for tool steps",
                        raw.id
                    ))
                })?;
                let args = raw.args.unwrap_or(serde_yml::Value::Null);
                PipelineStepKind::Tool { tool, args }
            }
        };
        Ok(PipelineStep {
            id: raw.id,
            kind,
            depends_on: raw.depends_on,
        })
    }
}

impl Serialize for PipelineStep {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let raw = match &self.kind {
            PipelineStepKind::Agent { agent, task } => PipelineStepRaw {
                id: self.id.clone(),
                agent: Some(agent.clone()),
                task: Some(task.clone()),
                tool: None,
                args: None,
                depends_on: self.depends_on.clone(),
            },
            PipelineStepKind::Tool { tool, args } => PipelineStepRaw {
                id: self.id.clone(),
                agent: None,
                task: None,
                tool: Some(tool.clone()),
                args: Some(args.clone()),
                depends_on: self.depends_on.clone(),
            },
        };
        raw.serialize(s)
    }
}

impl JsonSchema for PipelineStep {
    fn schema_name() -> String {
        "PipelineStep".into()
    }
    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        // The user-visible YAML is fully described by the Raw struct; we
        // accept that the schema does not encode the agent/tool oneOf
        // constraint (ADR-0024 "tech implications": JsonSchema isn't a
        // stable artefact yet).
        PipelineStepRaw::json_schema(gen)
    }
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
        assert_eq!(config.steps[0].agent_name(), Some("helper"));
    }

    #[test]
    fn tool_step_parses() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: ingest
steps:
  - id: fetch
    tool: git_pull
    args:
      repo: "https://example.com/r"
      ref: main
"#;
        let path = write_file(&dir, "pipeline.yml", yaml);
        let config = load_pipeline(&path, &HashMap::new()).unwrap();
        match &config.steps[0].kind {
            PipelineStepKind::Tool { tool, args } => {
                assert_eq!(tool, "git_pull");
                assert!(args.get("repo").is_some());
            }
            _ => panic!("expected tool step"),
        }
    }

    #[test]
    fn tool_step_without_args_defaults_to_null() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: p
steps:
  - id: s
    tool: noop
"#;
        let path = write_file(&dir, "pipeline.yml", yaml);
        let config = load_pipeline(&path, &HashMap::new()).unwrap();
        match &config.steps[0].kind {
            PipelineStepKind::Tool { args, .. } => {
                assert!(args.is_null());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn step_with_both_agent_and_tool_rejected() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: bad
steps:
  - id: s
    agent: a
    task: t
    tool: x
"#;
        let path = write_file(&dir, "pipeline.yml", yaml);
        let err = load_pipeline(&path, &HashMap::new()).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { detail, .. } if detail.contains("both")));
    }

    #[test]
    fn step_with_neither_agent_nor_tool_rejected() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: bad
steps:
  - id: s
    depends_on: []
"#;
        let path = write_file(&dir, "pipeline.yml", yaml);
        let err = load_pipeline(&path, &HashMap::new()).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { detail, .. } if detail.contains("either")));
    }

    #[test]
    fn agent_step_without_task_rejected() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
name: bad
steps:
  - id: s
    agent: a
"#;
        let path = write_file(&dir, "pipeline.yml", yaml);
        let err = load_pipeline(&path, &HashMap::new()).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { detail, .. } if detail.contains("task")));
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
        match &config.steps[0].kind {
            PipelineStepKind::Agent { task, .. } => assert!(task.contains("${inputs.q}")),
            _ => panic!(),
        }
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
