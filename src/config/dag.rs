use std::collections::{HashMap, HashSet, VecDeque};

use super::error::ConfigError;
use super::pipeline::PipelineConfig;

/// A level-based execution plan built from a pipeline DAG.
///
/// Steps within the same level have no dependencies on each other and can run
/// in parallel. Levels must be executed sequentially (level 0 before level 1, etc.).
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    /// Each inner `Vec` is one execution level. Steps within a level are
    /// independent and can run in parallel.
    pub levels: Vec<Vec<String>>,
}

impl ExecutionPlan {
    /// Total number of steps across all levels.
    pub fn step_count(&self) -> usize {
        self.levels.iter().map(|l| l.len()).sum()
    }

    /// True when every level contains exactly one step (fully sequential).
    pub fn is_sequential(&self) -> bool {
        self.levels.iter().all(|l| l.len() == 1)
    }

    /// Return the level index that contains the given step id, if any.
    pub fn level_of(&self, step_id: &str) -> Option<usize> {
        self.levels
            .iter()
            .position(|l| l.iter().any(|s| s == step_id))
    }
}

/// Build an [`ExecutionPlan`] from a validated pipeline config.
///
/// Uses Kahn's algorithm (BFS topological sort) which naturally groups nodes
/// into levels based on the longest path from a root. Steps in the same level
/// share no dependency edges and can execute concurrently.
///
/// # Errors
///
/// Returns [`ConfigError::CyclicDependency`] if the graph contains a cycle.
/// In normal usage the config validator catches this earlier, but this function
/// is safe to call independently.
pub fn build_execution_plan(pipeline: &PipelineConfig) -> Result<ExecutionPlan, ConfigError> {
    if pipeline.steps.is_empty() {
        return Ok(ExecutionPlan { levels: vec![] });
    }

    let step_ids: HashSet<&str> = pipeline.steps.iter().map(|s| s.id.as_str()).collect();

    // Build adjacency list (dependency -> dependents) and in-degree map.
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();

    for step in &pipeline.steps {
        in_degree.entry(step.id.as_str()).or_insert(0);
        dependents.entry(step.id.as_str()).or_default();

        for dep in &step.depends_on {
            // Skip unknown deps — validation catches these separately.
            if step_ids.contains(dep.as_str()) {
                dependents.entry(dep.as_str()).or_default().push(step.id.as_str());
                *in_degree.entry(step.id.as_str()).or_insert(0) += 1;
            }
        }
    }

    // Kahn's BFS — process level by level.
    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut levels: Vec<Vec<String>> = Vec::new();
    let mut visited = 0usize;

    while !queue.is_empty() {
        let mut next_queue = VecDeque::new();
        let mut level = Vec::with_capacity(queue.len());

        for node in queue.drain(..) {
            visited += 1;
            level.push(node.to_string());

            for &dependent in dependents.get(node).into_iter().flatten() {
                let deg = in_degree.get_mut(dependent).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    next_queue.push_back(dependent);
                }
            }
        }

        // Stable ordering within a level for deterministic plans.
        level.sort();
        levels.push(level);
        queue = next_queue;
    }

    if visited != step_ids.len() {
        // Collect remaining nodes for error reporting.
        let cycle: Vec<String> = in_degree
            .iter()
            .filter(|(_, &deg)| deg > 0)
            .map(|(&id, _)| id.to_string())
            .collect();
        return Err(ConfigError::CyclicDependency {
            pipeline: pipeline.name.clone(),
            cycle,
        });
    }

    Ok(ExecutionPlan { levels })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::pipeline::{PipelineOutput, PipelineStep};

    fn pipeline(name: &str, steps: Vec<(&str, Vec<&str>)>) -> PipelineConfig {
        PipelineConfig {
            name: name.into(),
            description: None,
            inputs: vec![],
            steps: steps
                .into_iter()
                .map(|(id, deps)| PipelineStep {
                    id: id.into(),
                    agent: "a".into(),
                    task: "t".into(),
                    depends_on: deps.into_iter().map(String::from).collect(),
                })
                .collect(),
            output: None,
        }
    }

    #[test]
    fn empty_pipeline() {
        let p = pipeline("empty", vec![]);
        let plan = build_execution_plan(&p).unwrap();
        assert!(plan.levels.is_empty());
        assert_eq!(plan.step_count(), 0);
    }

    #[test]
    fn single_step() {
        let p = pipeline("one", vec![("s1", vec![])]);
        let plan = build_execution_plan(&p).unwrap();
        assert_eq!(plan.levels, vec![vec!["s1"]]);
        assert!(plan.is_sequential());
    }

    #[test]
    fn linear_chain() {
        // s1 -> s2 -> s3
        let p = pipeline(
            "chain",
            vec![
                ("s1", vec![]),
                ("s2", vec!["s1"]),
                ("s3", vec!["s2"]),
            ],
        );
        let plan = build_execution_plan(&p).unwrap();
        assert_eq!(plan.levels.len(), 3);
        assert_eq!(plan.levels[0], vec!["s1"]);
        assert_eq!(plan.levels[1], vec!["s2"]);
        assert_eq!(plan.levels[2], vec!["s3"]);
        assert!(plan.is_sequential());
    }

    #[test]
    fn wide_parallel() {
        // s1, s2, s3 — all independent
        let p = pipeline(
            "wide",
            vec![("s1", vec![]), ("s2", vec![]), ("s3", vec![])],
        );
        let plan = build_execution_plan(&p).unwrap();
        assert_eq!(plan.levels.len(), 1);
        assert_eq!(plan.levels[0], vec!["s1", "s2", "s3"]);
        assert!(!plan.is_sequential());
    }

    #[test]
    fn diamond() {
        //     s1
        //    /  \
        //  s2    s3
        //    \  /
        //     s4
        let p = pipeline(
            "diamond",
            vec![
                ("s1", vec![]),
                ("s2", vec!["s1"]),
                ("s3", vec!["s1"]),
                ("s4", vec!["s2", "s3"]),
            ],
        );
        let plan = build_execution_plan(&p).unwrap();
        assert_eq!(plan.levels.len(), 3);
        assert_eq!(plan.levels[0], vec!["s1"]);
        assert_eq!(plan.levels[1], vec!["s2", "s3"]); // parallel
        assert_eq!(plan.levels[2], vec!["s4"]);
        assert!(!plan.is_sequential());
    }

    #[test]
    fn complex_dag() {
        //  s1   s2
        //  |  X  |
        //  s3   s4
        //    \ /
        //     s5
        let p = pipeline(
            "complex",
            vec![
                ("s1", vec![]),
                ("s2", vec![]),
                ("s3", vec!["s1", "s2"]),
                ("s4", vec!["s1", "s2"]),
                ("s5", vec!["s3", "s4"]),
            ],
        );
        let plan = build_execution_plan(&p).unwrap();
        assert_eq!(plan.levels.len(), 3);
        assert_eq!(plan.levels[0], vec!["s1", "s2"]);
        assert_eq!(plan.levels[1], vec!["s3", "s4"]);
        assert_eq!(plan.levels[2], vec!["s5"]);
    }

    #[test]
    fn level_of_lookup() {
        let p = pipeline(
            "lookup",
            vec![
                ("s1", vec![]),
                ("s2", vec!["s1"]),
                ("s3", vec!["s1"]),
            ],
        );
        let plan = build_execution_plan(&p).unwrap();
        assert_eq!(plan.level_of("s1"), Some(0));
        assert_eq!(plan.level_of("s2"), Some(1));
        assert_eq!(plan.level_of("s3"), Some(1));
        assert_eq!(plan.level_of("nonexistent"), None);
    }

    #[test]
    fn cycle_detected() {
        let p = pipeline(
            "cycle",
            vec![
                ("s1", vec!["s3"]),
                ("s2", vec!["s1"]),
                ("s3", vec!["s2"]),
            ],
        );
        let err = build_execution_plan(&p).unwrap_err();
        assert!(matches!(err, ConfigError::CyclicDependency { .. }));
    }

    #[test]
    fn self_cycle_detected() {
        let p = pipeline("self", vec![("s1", vec!["s1"])]);
        let err = build_execution_plan(&p).unwrap_err();
        assert!(matches!(err, ConfigError::CyclicDependency { .. }));
    }

    #[test]
    fn output_step_level() {
        let mut p = pipeline(
            "out",
            vec![
                ("search", vec![]),
                ("analyze", vec!["search"]),
                ("summarize", vec!["analyze"]),
            ],
        );
        p.output = Some(PipelineOutput {
            step: "summarize".into(),
        });

        let plan = build_execution_plan(&p).unwrap();
        // Output step should be in the last level.
        let output_level = plan.level_of("summarize").unwrap();
        assert_eq!(output_level, plan.levels.len() - 1);
    }
}
