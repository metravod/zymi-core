use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::agent::AgentConfig;
use super::error::ConfigError;
use super::pipeline::{PipelineConfig, PipelineStepKind};

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
        validate_pipeline_refs(pipeline, agents, known_tools)?;
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

/// Check that pipeline steps reference existing agents/tools and valid step IDs.
fn validate_pipeline_refs(
    pipeline: &PipelineConfig,
    agents: &HashMap<String, AgentConfig>,
    known_tools: &dyn ToolNameResolver,
) -> Result<(), ConfigError> {
    let step_ids: HashSet<&str> = pipeline.steps.iter().map(|s| s.id.as_str()).collect();
    let path = PathBuf::from(format!("pipelines/{}.yml", pipeline.name));

    for step in &pipeline.steps {
        match &step.kind {
            PipelineStepKind::Agent { agent, .. } => {
                if !agents.contains_key(agent) {
                    return Err(ConfigError::Validation {
                        message: format!(
                            "step `{}` references unknown agent `{}`",
                            step.id, agent
                        ),
                        help: format!(
                            "available agents: {}",
                            agents.keys().cloned().collect::<Vec<_>>().join(", ")
                        ),
                        path: path.clone(),
                    });
                }
            }
            PipelineStepKind::Tool { tool, args } => {
                // ADR-0024: tool name must resolve in the catalog. The
                // ConfigToolNameResolver covers builtins + declarative +
                // mcp__server__* prefixes; per-MCP-tool existence is
                // deferred to runtime as for agent.tools.
                if !known_tools.knows(tool) {
                    return Err(ConfigError::Validation {
                        message: format!(
                            "tool step `{}` references unknown tool `{}`",
                            step.id, tool
                        ),
                        help: format!(
                            "known tools: {}",
                            known_tools.all_tool_names().join(", ")
                        ),
                        path: path.clone(),
                    });
                }
                // ADR-0024: any `${steps.<other>.output}` ref inside `args`
                // must be in `depends_on`.
                let referenced = collect_step_refs(args);
                for other in referenced {
                    if other == step.id {
                        // self-ref is the cycle check's domain
                        continue;
                    }
                    if !step.depends_on.iter().any(|d| d == &other) {
                        return Err(ConfigError::Validation {
                            message: format!(
                                "tool step `{}` references `${{steps.{}.output}}` in args but `{}` is not in depends_on",
                                step.id, other, other
                            ),
                            help: format!(
                                "add `{}` to step `{}`'s depends_on list",
                                other, step.id
                            ),
                            path: path.clone(),
                        });
                    }
                    if !step_ids.contains(other.as_str()) {
                        return Err(ConfigError::Validation {
                            message: format!(
                                "tool step `{}` references unknown step `${{steps.{}.output}}`",
                                step.id, other
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

/// Walk a YAML value and collect all `${steps.<id>.output}` references.
///
/// Used by [`validate_pipeline_refs`] to check that tool-step `args:` only
/// reference upstream steps that are in `depends_on` (ADR-0024 §validator).
fn collect_step_refs(value: &serde_yml::Value) -> Vec<String> {
    let mut out = Vec::new();
    walk_step_refs(value, &mut out);
    out
}

fn walk_step_refs(value: &serde_yml::Value, out: &mut Vec<String>) {
    match value {
        serde_yml::Value::String(s) => extract_step_refs(s, out),
        serde_yml::Value::Sequence(seq) => {
            for v in seq {
                walk_step_refs(v, out);
            }
        }
        serde_yml::Value::Mapping(map) => {
            for (k, v) in map {
                walk_step_refs(k, out);
                walk_step_refs(v, out);
            }
        }
        _ => {}
    }
}

fn extract_step_refs(s: &str, out: &mut Vec<String>) {
    // Match `${steps.<id>.output}` — keep the regex local to avoid pulling
    // a heavy dep here. Manual scan: find each `${steps.` and read until `}`.
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 8 < bytes.len() {
        if &bytes[i..i + 8] == b"${steps." {
            let start = i + 8;
            if let Some(end_rel) = s[start..].find('}') {
                let inner = &s[start..start + end_rel];
                if let Some(id) = inner.strip_suffix(".output") {
                    if !id.is_empty() && !out.iter().any(|s| s == id) {
                        out.push(id.to_string());
                    }
                }
                i = start + end_rel + 1;
                continue;
            }
        }
        i += 1;
    }
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
                    kind: super::super::pipeline::PipelineStepKind::Agent {
                        agent: ag.into(),
                        task: "task".into(),
                    },
                    depends_on: deps.into_iter().map(String::from).collect(),
                })
                .collect(),
            output: None,
            approval_channel: None,
        }
    }

    fn tool_step(id: &str, tool: &str, args_yaml: &str, deps: Vec<&str>) -> super::super::pipeline::PipelineStep {
        let args: serde_yml::Value = serde_yml::from_str(args_yaml).unwrap();
        super::super::pipeline::PipelineStep {
            id: id.into(),
            kind: super::super::pipeline::PipelineStepKind::Tool {
                tool: tool.into(),
                args,
            },
            depends_on: deps.into_iter().map(String::from).collect(),
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
    fn tool_step_unknown_tool_rejected() {
        let agents = HashMap::new();
        let mut pipelines = HashMap::new();
        let mut p = pipeline_from_steps("p", vec![]);
        p.steps.push(tool_step("s1", "ghost_tool", "{}", vec![]));
        pipelines.insert("p".into(), p);

        let err = validate_workspace(
            &agents,
            &pipelines,
            &ConfigToolNameResolver::new(vec![]),
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Validation { message, .. } if message.contains("ghost_tool")));
    }

    #[test]
    fn tool_step_known_declarative_tool_accepted() {
        let agents = HashMap::new();
        let mut pipelines = HashMap::new();
        let mut p = pipeline_from_steps("p", vec![]);
        p.steps.push(tool_step("s1", "my_http_tool", "{}", vec![]));
        pipelines.insert("p".into(), p);

        let resolver = ConfigToolNameResolver::new(vec!["my_http_tool".into()]);
        assert!(validate_workspace(&agents, &pipelines, &resolver).is_ok());
    }

    #[test]
    fn tool_step_args_step_ref_must_be_in_depends_on() {
        let agents = HashMap::new();
        let mut pipelines = HashMap::new();
        let mut p = pipeline_from_steps("p", vec![]);
        p.steps.push(tool_step("a", "t", "{}", vec![]));
        p.steps.push(tool_step(
            "b",
            "t",
            r#"{"x":"${steps.a.output}"}"#,
            // depends_on intentionally empty — should fail
            vec![],
        ));
        pipelines.insert("p".into(), p);

        let resolver = ConfigToolNameResolver::new(vec!["t".into()]);
        let err = validate_workspace(&agents, &pipelines, &resolver).unwrap_err();
        assert!(matches!(err, ConfigError::Validation { message, .. } if message.contains("depends_on")));
    }

    #[test]
    fn tool_step_args_step_ref_with_proper_depends_on_ok() {
        let agents = HashMap::new();
        let mut pipelines = HashMap::new();
        let mut p = pipeline_from_steps("p", vec![]);
        p.steps.push(tool_step("a", "t", "{}", vec![]));
        p.steps.push(tool_step(
            "b",
            "t",
            r#"{"x":"prefix-${steps.a.output}-suffix"}"#,
            vec!["a"],
        ));
        pipelines.insert("p".into(), p);

        let resolver = ConfigToolNameResolver::new(vec!["t".into()]);
        assert!(validate_workspace(&agents, &pipelines, &resolver).is_ok());
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
