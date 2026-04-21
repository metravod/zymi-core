use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::agent::AgentConfig;
use super::error::ConfigError;
use super::pipeline::PipelineConfig;

/// Validate cross-references and structural invariants across the workspace.
///
/// Checks performed:
/// 1. Pipeline steps reference agents that exist.
/// 2. Pipeline `depends_on` references valid step IDs within the same pipeline.
/// 3. Pipeline `output.step` references a valid step ID.
/// 4. No cycles in pipeline DAGs.
/// 5. Agent tool names are registered in the tool catalog.
pub fn validate_workspace(
    agents: &HashMap<String, AgentConfig>,
    pipelines: &HashMap<String, PipelineConfig>,
    known_tools: &dyn ToolNameResolver,
) -> Result<(), ConfigError> {
    for agent in agents.values() {
        validate_agent_tools(agent, known_tools)?;
    }

    for pipeline in pipelines.values() {
        validate_pipeline_refs(pipeline, agents)?;
        validate_pipeline_dag(pipeline)?;
    }

    Ok(())
}

/// Trait for resolving tool names during validation.
///
/// Implemented by [`crate::runtime::ToolCatalog`] in runtime builds, and by
/// a simple static-set fallback for non-runtime (config-only) validation.
pub trait ToolNameResolver {
    fn knows(&self, name: &str) -> bool;
    fn all_tool_names(&self) -> Vec<&str>;
}

/// Check that all tool names in an agent config are registered in the catalog.
fn validate_agent_tools(
    agent: &AgentConfig,
    known_tools: &dyn ToolNameResolver,
) -> Result<(), ConfigError> {
    for tool in &agent.tools {
        if !known_tools.knows(tool) {
            return Err(ConfigError::Validation {
                message: format!(
                    "agent `{}` references unknown tool `{}`",
                    agent.name, tool
                ),
                help: format!("known tools: {}", known_tools.all_tool_names().join(", ")),
                path: PathBuf::from(format!("agents/{}.yml", agent.name)),
            });
        }
    }
    Ok(())
}

/// Resolver used by [`super::load_project_dir`] at config-load time, before a
/// [`Runtime`](crate::runtime::Runtime) (and therefore a
/// [`ToolCatalog`](crate::runtime::ToolCatalog)) exists.
///
/// Accepts three sources of tool names:
/// 1. Built-ins from [`super::agent::KNOWN_TOOLS`].
/// 2. Declarative tool names loaded from `tools/*.yml`.
/// 3. Any `mcp__<server>__<tool>` where `<server>` is declared in
///    `project.mcp_servers`. Per-tool name existence is deferred to the
///    runtime — the server only advertises its `tools/list` at startup, so
///    load-time validation would require spawning subprocesses. Typos in the
///    server name are still caught here.
pub struct ConfigToolNameResolver {
    declarative_names: Vec<String>,
    mcp_server_names: Vec<String>,
}

impl ConfigToolNameResolver {
    pub fn new(declarative_names: Vec<String>) -> Self {
        Self {
            declarative_names,
            mcp_server_names: Vec::new(),
        }
    }

    /// Attach the list of MCP server names from `project.mcp_servers`. Any
    /// agent tool name of the form `mcp__<server>__<tool>` is accepted as
    /// long as `<server>` appears here; the `<tool>` segment is not checked
    /// until runtime.
    pub fn with_mcp_servers(mut self, names: Vec<String>) -> Self {
        self.mcp_server_names = names;
        self
    }
}

impl ToolNameResolver for ConfigToolNameResolver {
    fn knows(&self, name: &str) -> bool {
        if super::agent::KNOWN_TOOLS.contains(&name)
            || self.declarative_names.iter().any(|n| n == name)
        {
            return true;
        }
        if let Some(rest) = name.strip_prefix(crate::mcp::MCP_PREFIX) {
            if let Some((server, tool)) = rest.split_once("__") {
                return !tool.is_empty() && self.mcp_server_names.iter().any(|s| s == server);
            }
        }
        false
    }

    fn all_tool_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = super::agent::KNOWN_TOOLS.to_vec();
        names.extend(self.declarative_names.iter().map(|s| s.as_str()));
        // MCP tool names aren't known at load time; surface the server names
        // so the help text hints at the right prefix.
        names.extend(self.mcp_server_names.iter().map(|s| s.as_str()));
        names
    }
}

/// Check that pipeline steps reference existing agents and valid step IDs.
fn validate_pipeline_refs(
    pipeline: &PipelineConfig,
    agents: &HashMap<String, AgentConfig>,
) -> Result<(), ConfigError> {
    let step_ids: HashSet<&str> = pipeline.steps.iter().map(|s| s.id.as_str()).collect();
    let path = PathBuf::from(format!("pipelines/{}.yml", pipeline.name));

    for step in &pipeline.steps {
        // Check agent exists.
        if !agents.contains_key(&step.agent) {
            return Err(ConfigError::Validation {
                message: format!(
                    "step `{}` references unknown agent `{}`",
                    step.id, step.agent
                ),
                help: format!(
                    "available agents: {}",
                    agents.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
                path: path.clone(),
            });
        }

        // Check depends_on references valid step IDs.
        for dep in &step.depends_on {
            if !step_ids.contains(dep.as_str()) {
                return Err(ConfigError::Validation {
                    message: format!(
                        "step `{}` depends on unknown step `{}`",
                        step.id, dep
                    ),
                    help: format!(
                        "available steps: {}",
                        step_ids.iter().copied().collect::<Vec<_>>().join(", ")
                    ),
                    path: path.clone(),
                });
            }
        }
    }

    // Check output step exists.
    if let Some(output) = &pipeline.output {
        if !step_ids.contains(output.step.as_str()) {
            return Err(ConfigError::Validation {
                message: format!("output references unknown step `{}`", output.step),
                help: format!(
                    "available steps: {}",
                    step_ids.iter().copied().collect::<Vec<_>>().join(", ")
                ),
                path,
            });
        }
    }

    Ok(())
}

/// Detect cycles in the pipeline step DAG using iterative DFS.
fn validate_pipeline_dag(pipeline: &PipelineConfig) -> Result<(), ConfigError> {
    let step_ids: Vec<&str> = pipeline.steps.iter().map(|s| s.id.as_str()).collect();
    let adj: HashMap<&str, Vec<&str>> = pipeline
        .steps
        .iter()
        .map(|s| {
            (
                s.id.as_str(),
                s.depends_on.iter().map(|d| d.as_str()).collect(),
            )
        })
        .collect();

    // 0 = unvisited, 1 = in-progress, 2 = done
    let mut state: HashMap<&str, u8> = step_ids.iter().map(|&id| (id, 0u8)).collect();

    for &start in &step_ids {
        if state[start] == 2 {
            continue;
        }

        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        state.insert(start, 1);

        while let Some((node, idx)) = stack.last_mut() {
            let deps = adj.get(*node).map(|v| v.as_slice()).unwrap_or(&[]);
            if *idx < deps.len() {
                let dep = deps[*idx];
                *idx += 1;

                match state.get(dep).copied().unwrap_or(0) {
                    0 => {
                        state.insert(dep, 1);
                        stack.push((dep, 0));
                    }
                    1 => {
                        // Found a cycle — collect the cycle path.
                        let cycle: Vec<String> = stack
                            .iter()
                            .map(|(id, _)| id.to_string())
                            .skip_while(|id| id != dep)
                            .collect();
                        return Err(ConfigError::CyclicDependency {
                            pipeline: pipeline.name.clone(),
                            cycle,
                        });
                    }
                    _ => {} // already fully visited
                }
            } else {
                let (finished, _) = stack.pop().unwrap();
                state.insert(finished, 2);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str, tools: Vec<&str>) -> AgentConfig {
        AgentConfig {
            name: name.into(),
            description: None,
            model: None,
            system_prompt: None,
            tools: tools.into_iter().map(String::from).collect(),
            max_iterations: None,
            timeout_secs: None,
            policy: None,
        }
    }

    fn pipeline_from_steps(name: &str, steps: Vec<(&str, &str, Vec<&str>)>) -> PipelineConfig {
        PipelineConfig {
            name: name.into(),
            description: None,
            inputs: vec![],
            steps: steps
                .into_iter()
                .map(|(id, ag, deps)| super::super::pipeline::PipelineStep {
                    id: id.into(),
                    agent: ag.into(),
                    task: "task".into(),
                    depends_on: deps.into_iter().map(String::from).collect(),
                })
                .collect(),
            output: None,
        }
    }

    #[test]
    fn valid_workspace() {
        let mut agents = HashMap::new();
        agents.insert("researcher".into(), agent("researcher", vec!["web_search"]));

        let mut pipelines = HashMap::new();
        pipelines.insert(
            "research".into(),
            pipeline_from_steps(
                "research",
                vec![
                    ("search", "researcher", vec![]),
                    ("analyze", "researcher", vec!["search"]),
                ],
            ),
        );

        // web_search is declarative (from tools/*.yml), not builtin.
        let resolver = ConfigToolNameResolver::new(vec!["web_search".into()]);
        assert!(validate_workspace(&agents, &pipelines, &resolver).is_ok());
    }

    #[test]
    fn unknown_tool_rejected() {
        let mut agents = HashMap::new();
        agents.insert("a".into(), agent("a", vec!["nonexistent_tool"]));

        let err = validate_workspace(&agents, &HashMap::new(), &ConfigToolNameResolver::new(vec![])).unwrap_err();
        assert!(matches!(err, ConfigError::Validation { .. }));
    }

    #[test]
    fn unknown_agent_in_pipeline() {
        let agents = HashMap::new(); // no agents

        let mut pipelines = HashMap::new();
        pipelines.insert(
            "p".into(),
            pipeline_from_steps("p", vec![("s1", "missing_agent", vec![])]),
        );

        let err = validate_workspace(&agents, &pipelines, &ConfigToolNameResolver::new(vec![])).unwrap_err();
        assert!(matches!(err, ConfigError::Validation { message, .. } if message.contains("missing_agent")));
    }

    #[test]
    fn unknown_depends_on() {
        let mut agents = HashMap::new();
        agents.insert("a".into(), agent("a", vec![]));

        let mut pipelines = HashMap::new();
        pipelines.insert(
            "p".into(),
            pipeline_from_steps("p", vec![("s1", "a", vec!["nonexistent"])]),
        );

        let err = validate_workspace(&agents, &pipelines, &ConfigToolNameResolver::new(vec![])).unwrap_err();
        assert!(matches!(err, ConfigError::Validation { message, .. } if message.contains("nonexistent")));
    }

    #[test]
    fn self_cycle_detected() {
        let mut agents = HashMap::new();
        agents.insert("a".into(), agent("a", vec![]));

        let mut pipelines = HashMap::new();
        pipelines.insert(
            "p".into(),
            pipeline_from_steps("p", vec![("s1", "a", vec!["s1"])]),
        );

        let err = validate_workspace(&agents, &pipelines, &ConfigToolNameResolver::new(vec![])).unwrap_err();
        assert!(matches!(err, ConfigError::CyclicDependency { .. }));
    }

    #[test]
    fn multi_node_cycle_detected() {
        let mut agents = HashMap::new();
        agents.insert("a".into(), agent("a", vec![]));

        let mut pipelines = HashMap::new();
        pipelines.insert(
            "p".into(),
            pipeline_from_steps(
                "p",
                vec![
                    ("s1", "a", vec!["s3"]),
                    ("s2", "a", vec!["s1"]),
                    ("s3", "a", vec!["s2"]),
                ],
            ),
        );

        let err = validate_workspace(&agents, &pipelines, &ConfigToolNameResolver::new(vec![])).unwrap_err();
        assert!(matches!(err, ConfigError::CyclicDependency { .. }));
    }

    #[test]
    fn valid_dag_no_cycle() {
        let mut agents = HashMap::new();
        agents.insert("a".into(), agent("a", vec![]));

        let mut pipelines = HashMap::new();
        pipelines.insert(
            "p".into(),
            pipeline_from_steps(
                "p",
                vec![
                    ("s1", "a", vec![]),
                    ("s2", "a", vec!["s1"]),
                    ("s3", "a", vec!["s1"]),
                    ("s4", "a", vec!["s2", "s3"]),
                ],
            ),
        );

        assert!(validate_workspace(&agents, &pipelines, &ConfigToolNameResolver::new(vec![])).is_ok());
    }

    #[test]
    fn mcp_tool_accepted_when_server_declared() {
        let mut agents = HashMap::new();
        agents.insert(
            "a".into(),
            agent("a", vec!["mcp__fs__read_text_file", "mcp__fs__write_file"]),
        );

        let resolver =
            ConfigToolNameResolver::new(vec![]).with_mcp_servers(vec!["fs".into()]);
        assert!(validate_workspace(&agents, &HashMap::new(), &resolver).is_ok());
    }

    #[test]
    fn mcp_tool_rejected_when_server_not_declared() {
        let mut agents = HashMap::new();
        agents.insert("a".into(), agent("a", vec!["mcp__github__create_issue"]));

        // project.mcp_servers only declares `fs`; the agent references `github`.
        let resolver =
            ConfigToolNameResolver::new(vec![]).with_mcp_servers(vec!["fs".into()]);
        let err = validate_workspace(&agents, &HashMap::new(), &resolver).unwrap_err();
        assert!(matches!(err, ConfigError::Validation { message, .. } if message.contains("mcp__github__create_issue")));
    }

    #[test]
    fn mcp_prefix_without_tool_segment_rejected() {
        // `mcp__fs__` with empty tool name is not a valid reference.
        let mut agents = HashMap::new();
        agents.insert("a".into(), agent("a", vec!["mcp__fs__"]));
        let resolver =
            ConfigToolNameResolver::new(vec![]).with_mcp_servers(vec!["fs".into()]);
        let err = validate_workspace(&agents, &HashMap::new(), &resolver).unwrap_err();
        assert!(matches!(err, ConfigError::Validation { .. }));
    }

    #[test]
    fn invalid_output_step() {
        let mut agents = HashMap::new();
        agents.insert("a".into(), agent("a", vec![]));

        let mut pipelines = HashMap::new();
        let mut p = pipeline_from_steps("p", vec![("s1", "a", vec![])]);
        p.output = Some(super::super::pipeline::PipelineOutput {
            step: "nonexistent".into(),
        });
        pipelines.insert("p".into(), p);

        let err = validate_workspace(&agents, &pipelines, &ConfigToolNameResolver::new(vec![])).unwrap_err();
        assert!(matches!(err, ConfigError::Validation { message, .. } if message.contains("nonexistent")));
    }
}
