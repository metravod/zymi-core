//! Vertical DAG renderer for pipeline graphs.
//!
//! Algorithm:
//! 1. Assign each node a rank = `max(rank(p) for p in depends_on) + 1`.
//! 2. Render nodes of the same rank side-by-side, ranks stacked top-to-bottom.
//! 3. Draw straight `│` between a node and a parent at the previous rank when
//!    both occupy the same column, else draw a step connector.
//!
//! Pipeline DAGs are tiny (≤10 nodes typical), so we don't minimise crossings.
//! When a rank has more than [`MAX_NODES_PER_RANK`] nodes, we fall back to a
//! compact `rank N: [a, b, c, …]` line. That preserves information without
//! wrapping to unreadable widths.

use std::collections::HashMap;

use crate::config::PipelineConfig;
use crate::events::{Event, EventKind};

const MAX_NODES_PER_RANK: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    Pending,
    Running,
    Ok,
    Failed,
}

impl NodeStatus {
    pub fn glyph(self) -> char {
        match self {
            NodeStatus::Pending => '·',
            NodeStatus::Running => '⏳',
            NodeStatus::Ok => '✓',
            NodeStatus::Failed => '✗',
        }
    }
}

#[derive(Debug, Clone)]
pub struct GraphNode {
    pub id: String,
    pub agent: String,
    pub rank: usize,
    pub status: NodeStatus,
}

#[derive(Debug, Clone)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub rank_counts: Vec<usize>,
    /// `true` when at least one rank exceeded [`MAX_NODES_PER_RANK`] and we
    /// rendered the compact fallback instead of the graph.
    pub compact: bool,
}

impl Graph {
    /// Build a graph from a pipeline config and overlay per-node status from
    /// the run's events.
    pub fn build(pipeline: &PipelineConfig, events: &[Event]) -> Self {
        let ranks = assign_ranks(pipeline);
        let max_rank = ranks.values().copied().max().unwrap_or(0);

        let mut rank_counts = vec![0usize; max_rank + 1];
        let mut nodes = Vec::with_capacity(pipeline.steps.len());
        for step in &pipeline.steps {
            let rank = ranks[&step.id];
            rank_counts[rank] += 1;
            nodes.push(GraphNode {
                id: step.id.clone(),
                agent: step.agent.clone(),
                rank,
                status: NodeStatus::Pending,
            });
        }

        let statuses = derive_statuses(events);
        for node in &mut nodes {
            if let Some(s) = statuses.get(&node.id) {
                node.status = *s;
            }
        }

        let compact = rank_counts.iter().any(|&c| c > MAX_NODES_PER_RANK);
        Graph {
            nodes,
            rank_counts,
            compact,
        }
    }

    /// Return the selected node id or `None` if the graph is empty.
    pub fn node_at(&self, idx: usize) -> Option<&GraphNode> {
        self.nodes.get(idx)
    }

    /// Render as a list of text lines for ratatui. `selected_idx` is the
    /// index into [`Graph::nodes`] that should be highlighted.
    pub fn render_lines(&self, selected_idx: Option<usize>) -> Vec<String> {
        if self.nodes.is_empty() {
            return vec!["(empty pipeline)".into()];
        }

        if self.compact {
            return self.render_compact(selected_idx);
        }

        let mut lines = Vec::new();
        let selected_id = selected_idx.and_then(|i| self.nodes.get(i).map(|n| n.id.as_str()));

        // Group nodes by rank.
        let mut by_rank: Vec<Vec<&GraphNode>> = vec![Vec::new(); self.rank_counts.len()];
        for node in &self.nodes {
            by_rank[node.rank].push(node);
        }

        for (r, row) in by_rank.iter().enumerate() {
            if r > 0 {
                lines.push(connector_row(row.len()));
            }
            lines.push(node_row(row, selected_id));
        }

        lines
    }

    fn render_compact(&self, selected_idx: Option<usize>) -> Vec<String> {
        let mut lines = Vec::new();
        let selected_id = selected_idx.and_then(|i| self.nodes.get(i).map(|n| n.id.as_str()));
        let mut by_rank: Vec<Vec<&GraphNode>> = vec![Vec::new(); self.rank_counts.len()];
        for node in &self.nodes {
            by_rank[node.rank].push(node);
        }

        lines.push("(compact: ranks too wide to draw)".into());
        for (r, row) in by_rank.iter().enumerate() {
            let items: Vec<String> = row
                .iter()
                .map(|n| {
                    let marker = if Some(n.id.as_str()) == selected_id { ">" } else { " " };
                    format!("{marker}{} {}", n.status.glyph(), n.id)
                })
                .collect();
            lines.push(format!("rank {r}: {}", items.join(", ")));
        }
        lines
    }
}

fn assign_ranks(pipeline: &PipelineConfig) -> HashMap<String, usize> {
    let mut ranks: HashMap<String, usize> = HashMap::new();
    // Kahn-ish: repeat until stable. Pipeline configs are validated elsewhere
    // to be acyclic, so this converges in ≤ len(steps) passes.
    let n = pipeline.steps.len();
    for _ in 0..=n {
        let mut changed = false;
        for step in &pipeline.steps {
            let candidate = step
                .depends_on
                .iter()
                .map(|dep| ranks.get(dep).copied().unwrap_or(0))
                .max()
                .map(|r| r + 1)
                .unwrap_or(0);
            let actual = if step.depends_on.is_empty() { 0 } else { candidate };
            match ranks.get(&step.id) {
                Some(&existing) if existing == actual => {}
                _ => {
                    ranks.insert(step.id.clone(), actual);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    ranks
}

fn derive_statuses(events: &[Event]) -> HashMap<String, NodeStatus> {
    let mut out: HashMap<String, NodeStatus> = HashMap::new();
    for event in events {
        match &event.kind {
            EventKind::WorkflowNodeStarted { node_id, .. } => {
                out.insert(node_id.clone(), NodeStatus::Running);
            }
            EventKind::WorkflowNodeCompleted { node_id, success } => {
                out.insert(
                    node_id.clone(),
                    if *success { NodeStatus::Ok } else { NodeStatus::Failed },
                );
            }
            _ => {}
        }
    }
    out
}

fn node_row(row: &[&GraphNode], selected_id: Option<&str>) -> String {
    row.iter()
        .map(|n| {
            let marker = if Some(n.id.as_str()) == selected_id { ">" } else { " " };
            format!("{marker}[{} {}]", n.status.glyph(), truncate(&n.id, 10))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn connector_row(n_nodes: usize) -> String {
    // Simple downward arrow per node in the row; good enough for linear chains
    // and narrow forks. Not trying to solve general DAG routing.
    let mut s = String::new();
    for i in 0..n_nodes {
        if i > 0 {
            s.push_str("      ");
        }
        s.push_str("   │    ");
    }
    s
}

fn truncate(s: &str, max: usize) -> &str {
    if s.chars().count() <= max {
        s
    } else {
        let end = s.floor_char_boundary(max);
        &s[..end]
    }
}
