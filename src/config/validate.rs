use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::agent::{AgentConfig, KNOWN_TOOLS};
use super::error::ConfigError;
use super::pipeline::PipelineConfig;

/// Validate cross-references and structural invariants across the workspace.
///
/// Checks performed:
/// 1. Pipeline steps reference agents that exist.
/// 2. Pipeline `depends_on` references valid step IDs within the same pipeline.
/// 3. Pipeline `output.step` references a valid step ID.
/// 4. No cycles in pipeline DAGs.
/// 5. Agent tool names are known Intention tags.
pub fn validate_workspace(
    agents: &HashMap<String, AgentConfig>,
    pipelines: &HashMap<String, PipelineConfig>,
) -> Result<(), ConfigError> {
    for agent in agents.values() {
        validate_agent_tools(agent)?;
    }

    for pipeline in pipelines.values() {
        validate_pipeline_refs(pipeline, agents)?;
        validate_pipeline_dag(pipeline)?;
    }

    Ok(())
}

/// Check that all tool names in an agent config are known Intention tags.
fn validate_agent_tools(agent: &AgentConfig) -> Result<(), ConfigError> {
    for tool in &agent.tools {
        if !KNOWN_TOOLS.contains(&tool.as_str()) {
            return Err(ConfigError::Validation {
                message: format!(
                    "agent `{}` references unknown tool `{}`",
                    agent.name, tool
                ),
                help: format!("known tools: {}", KNOWN_TOOLS.join(", ")),
                path: PathBuf::from(format!("agents/{}.yml", agent.name)),
            });
        }
    }
    Ok(())
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

        assert!(validate_workspace(&agents, &pipelines).is_ok());
    }

    #[test]
    fn unknown_tool_rejected() {
        let mut agents = HashMap::new();
        agents.insert("a".into(), agent("a", vec!["nonexistent_tool"]));

        let err = validate_workspace(&agents, &HashMap::new()).unwrap_err();
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

        let err = validate_workspace(&agents, &pipelines).unwrap_err();
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

        let err = validate_workspace(&agents, &pipelines).unwrap_err();
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

        let err = validate_workspace(&agents, &pipelines).unwrap_err();
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

        let err = validate_workspace(&agents, &pipelines).unwrap_err();
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

        assert!(validate_workspace(&agents, &pipelines).is_ok());
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

        let err = validate_workspace(&agents, &pipelines).unwrap_err();
        assert!(matches!(err, ConfigError::Validation { message, .. } if message.contains("nonexistent")));
    }
}
