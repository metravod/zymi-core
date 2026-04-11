pub mod agent;
pub mod dag;
pub mod error;
pub mod pipeline;
pub mod project;
pub mod template;
pub mod tool;
pub mod validate;

use std::collections::HashMap;
use std::path::Path;

pub use agent::AgentConfig;
pub use error::ConfigError;
pub use dag::{build_execution_plan, ExecutionPlan};
pub use pipeline::{PipelineConfig, PipelineInput, PipelineOutput, PipelineStep};
pub use project::{ContractsConfig, DefaultsConfig, LangfuseConfig, LlmConfig, ProjectConfig, ServicesConfig};
pub use tool::{ToolConfig, ImplementationConfig, HttpMethod};

/// A fully loaded and validated workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub project: ProjectConfig,
    pub agents: HashMap<String, AgentConfig>,
    pub pipelines: HashMap<String, PipelineConfig>,
    pub tools: HashMap<String, tool::ToolConfig>,
}

/// Load and validate an entire project directory.
///
/// Expected layout:
/// ```text
/// root/
///   project.yml
///   agents/
///     *.yml
///   pipelines/
///     *.yml
/// ```
pub fn load_project_dir(root: &Path) -> Result<WorkspaceConfig, ConfigError> {
    // 1. Load project.yml
    let project_path = root.join("project.yml");
    let project = project::load_project(&project_path)?;

    // 2. Build template variables from project
    let vars = template::build_project_vars(
        &project.name,
        project.version.as_deref(),
        &project.variables,
    );

    // 3. Load all agent YAMLs
    let agents_dir = root.join("agents");
    let mut agents = HashMap::new();
    let mut agent_sources: HashMap<String, std::path::PathBuf> = HashMap::new();
    if agents_dir.is_dir() {
        for entry in std::fs::read_dir(&agents_dir).map_err(|e| ConfigError::Io {
            path: agents_dir.clone(),
            source: e,
        })? {
            let entry = entry.map_err(|e| ConfigError::Io {
                path: agents_dir.clone(),
                source: e,
            })?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "yml" || ext == "yaml") {
                let cfg = agent::load_agent(&path, &vars)?;
                if let Some(first_path) = agent_sources.get(&cfg.name) {
                    return Err(ConfigError::DuplicateName {
                        kind: "agent".into(),
                        name: cfg.name,
                        first: first_path.clone(),
                        second: path,
                    });
                }
                agent_sources.insert(cfg.name.clone(), path);
                agents.insert(cfg.name.clone(), cfg);
            }
        }
    }

    // 4. Load all pipeline YAMLs
    let pipelines_dir = root.join("pipelines");
    let mut pipelines = HashMap::new();
    let mut pipeline_sources: HashMap<String, std::path::PathBuf> = HashMap::new();
    if pipelines_dir.is_dir() {
        for entry in std::fs::read_dir(&pipelines_dir).map_err(|e| ConfigError::Io {
            path: pipelines_dir.clone(),
            source: e,
        })? {
            let entry = entry.map_err(|e| ConfigError::Io {
                path: pipelines_dir.clone(),
                source: e,
            })?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "yml" || ext == "yaml") {
                let cfg = pipeline::load_pipeline(&path, &vars)?;
                if let Some(first_path) = pipeline_sources.get(&cfg.name) {
                    return Err(ConfigError::DuplicateName {
                        kind: "pipeline".into(),
                        name: cfg.name,
                        first: first_path.clone(),
                        second: path,
                    });
                }
                pipeline_sources.insert(cfg.name.clone(), path);
                pipelines.insert(cfg.name.clone(), cfg);
            }
        }
    }

    // 5. Load all tool YAMLs
    let tools_dir = root.join("tools");
    let mut tools = HashMap::new();
    let mut tool_sources: HashMap<String, std::path::PathBuf> = HashMap::new();
    if tools_dir.is_dir() {
        for entry in std::fs::read_dir(&tools_dir).map_err(|e| ConfigError::Io {
            path: tools_dir.clone(),
            source: e,
        })? {
            let entry = entry.map_err(|e| ConfigError::Io {
                path: tools_dir.clone(),
                source: e,
            })?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "yml" || ext == "yaml") {
                let cfg = tool::load_tool(&path, &vars)?;
                if let Some(first_path) = tool_sources.get(&cfg.name) {
                    return Err(ConfigError::DuplicateName {
                        kind: "tool".into(),
                        name: cfg.name,
                        first: first_path.clone(),
                        second: path,
                    });
                }
                tool_sources.insert(cfg.name.clone(), path);
                tools.insert(cfg.name.clone(), cfg);
            }
        }
    }

    // 6. Cross-validate (uses the static builtin tool list; the Runtime
    //    re-validates against the full ToolCatalog at build time if needed).
    validate::validate_workspace(
        &agents,
        &pipelines,
        &validate::BuiltinToolNameResolver,
    )?;

    Ok(WorkspaceConfig {
        project,
        agents,
        pipelines,
        tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_workspace(dir: &TempDir) {
        let root = dir.path();
        fs::create_dir_all(root.join("agents")).unwrap();
        fs::create_dir_all(root.join("pipelines")).unwrap();

        fs::write(
            root.join("project.yml"),
            r#"
name: test-project
version: "0.1"
variables:
  default_model: gpt-4o
"#,
        )
        .unwrap();

        fs::write(
            root.join("agents/researcher.yml"),
            r#"
name: researcher
model: ${default_model}
tools:
  - web_search
  - write_memory
"#,
        )
        .unwrap();

        fs::write(
            root.join("pipelines/research.yml"),
            r#"
name: research
steps:
  - id: search
    agent: researcher
    task: "search the web"
  - id: summarize
    agent: researcher
    task: "summarize results"
    depends_on: [search]
output:
  step: summarize
"#,
        )
        .unwrap();
    }

    #[test]
    fn load_full_workspace() {
        let dir = TempDir::new().unwrap();
        setup_workspace(&dir);

        let ws = load_project_dir(dir.path()).unwrap();
        assert_eq!(ws.project.name, "test-project");
        assert_eq!(ws.agents.len(), 1);
        assert_eq!(ws.agents["researcher"].model.as_deref(), Some("gpt-4o"));
        assert_eq!(ws.pipelines.len(), 1);
        assert_eq!(ws.pipelines["research"].steps.len(), 2);
    }

    #[test]
    fn workspace_without_agents_dir() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("project.yml"), "name: bare\n").unwrap();

        let ws = load_project_dir(dir.path()).unwrap();
        assert!(ws.agents.is_empty());
        assert!(ws.pipelines.is_empty());
    }

    #[test]
    fn workspace_rejects_duplicate_agent_names() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("agents")).unwrap();
        fs::write(root.join("project.yml"), "name: test\n").unwrap();
        fs::write(root.join("agents/a.yml"), "name: dup\ntools: []\n").unwrap();
        fs::write(root.join("agents/b.yml"), "name: dup\ntools: []\n").unwrap();

        let err = load_project_dir(root).unwrap_err();
        assert!(matches!(err, ConfigError::DuplicateName { .. }));
    }

    #[test]
    fn workspace_rejects_duplicate_pipeline_names() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("agents")).unwrap();
        fs::create_dir_all(root.join("pipelines")).unwrap();
        fs::write(root.join("project.yml"), "name: test\n").unwrap();
        fs::write(root.join("agents/a.yml"), "name: agent_a\ntools: []\n").unwrap();
        fs::write(
            root.join("pipelines/a.yml"),
            "name: dup\nsteps:\n  - id: s1\n    agent: agent_a\n    task: go\n",
        ).unwrap();
        fs::write(
            root.join("pipelines/b.yml"),
            "name: dup\nsteps:\n  - id: s1\n    agent: agent_a\n    task: go\n",
        ).unwrap();

        let err = load_project_dir(root).unwrap_err();
        assert!(matches!(err, ConfigError::DuplicateName { .. }));
    }

    #[test]
    fn workspace_validation_catches_bad_agent_ref() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("pipelines")).unwrap();
        fs::write(dir.path().join("project.yml"), "name: test\n").unwrap();
        fs::write(
            dir.path().join("pipelines/bad.yml"),
            "name: bad\nsteps:\n  - id: s1\n    agent: ghost\n    task: go\n",
        )
        .unwrap();

        let err = load_project_dir(dir.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Validation { .. }));
    }
}
