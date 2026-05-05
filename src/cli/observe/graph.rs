//! Vertical DAG renderer for pipeline graphs.
//!
//! Algorithm:
//! 1. Assign each node a rank = `max(rank(p) for p in depends_on) + 1`.
//! 2. Stack ranks top-to-bottom. Nodes sharing a rank are stacked vertically
//!    inside a framed "Level N" box; parallelism is labelled in the header.
//! 3. Boxes are connected by a thin `│` trunk between them.
//!
//! Pipeline DAGs are tiny (≤10 nodes typical), so we don't minimise crossings
//! or solve arbitrary DAG routing — this is a pretty-printer, not a graph lib.

use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::config::PipelineConfig;
use crate::events::{Event, EventKind};

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
            NodeStatus::Running => '◷',
            NodeStatus::Ok => '✓',
            NodeStatus::Failed => '✗',
        }
    }

    fn color(self) -> Color {
        match self {
            NodeStatus::Pending => Color::DarkGray,
            NodeStatus::Running => Color::Yellow,
            NodeStatus::Ok => Color::Green,
            NodeStatus::Failed => Color::Red,
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
            // Tool steps don't have an agent name; show the tool name
            // prefixed so the graph view stays informative (ADR-0024).
            let agent = match &step.kind {
                crate::config::pipeline::PipelineStepKind::Agent { agent, .. } => agent.clone(),
                crate::config::pipeline::PipelineStepKind::Tool { tool, .. } => {
                    format!("tool:{tool}")
                }
            };
            nodes.push(GraphNode {
                id: step.id.clone(),
                agent,
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

        Graph { nodes, rank_counts }
    }

    /// Return the selected node or `None` if the graph is empty.
    pub fn node_at(&self, idx: usize) -> Option<&GraphNode> {
        self.nodes.get(idx)
    }

    /// Render as styled lines for ratatui. `width` is the usable content width
    /// inside the surrounding block (i.e. `area.width - 2`). `selected_idx` is
    /// the index into [`Graph::nodes`] that should be highlighted.
    pub fn render_lines(
        &self,
        selected_idx: Option<usize>,
        width: usize,
    ) -> Vec<Line<'static>> {
        let dim = Style::default().fg(Color::DarkGray);

        if self.nodes.is_empty() {
            return vec![Line::from(Span::styled("(empty pipeline)", dim))];
        }

        // Clamp to sensible bounds. Narrow terminals get 24, wide get 60.
        let box_w = width.clamp(24, 60);
        let inner = box_w - 2;

        let selected_id = selected_idx.and_then(|i| self.nodes.get(i).map(|n| n.id.as_str()));

        let mut by_rank: Vec<Vec<&GraphNode>> = vec![Vec::new(); self.rank_counts.len()];
        for node in &self.nodes {
            by_rank[node.rank].push(node);
        }

        let mut lines: Vec<Line<'static>> = Vec::new();
        let trunk_col = inner / 2 + 1; // column offset inside the box for the trunk

        for (r, row) in by_rank.iter().enumerate() {
            // Connector trunk between boxes.
            if r > 0 {
                let trunk = format!("{}│", " ".repeat(trunk_col));
                lines.push(Line::from(Span::styled(trunk, dim)));
            }

            // Top border: ╭─ Level N [(parallel)] ─...─╮
            let suffix = if row.len() > 1 { " (parallel)" } else { "" };
            let label = format!(" Level {}{} ", r + 1, suffix);
            let label_len = label.chars().count();
            let dashes_after = inner.saturating_sub(label_len + 1);
            let top = format!("╭─{label}{}╮", "─".repeat(dashes_after));
            lines.push(Line::from(Span::styled(top, dim)));

            // Node rows.
            for node in row {
                let is_sel = Some(node.id.as_str()) == selected_id;
                let marker = if is_sel { '▸' } else { ' ' };
                let body = format!(
                    "{} {} {}",
                    marker,
                    node.status.glyph(),
                    truncate_chars(&node.id, inner.saturating_sub(6))
                );
                let pad = (inner - 2).saturating_sub(body.chars().count());
                let node_style = if is_sel {
                    Style::default()
                        .fg(node.status.color())
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    Style::default().fg(node.status.color())
                };
                let spans = vec![
                    Span::styled("│ ", dim),
                    Span::styled(body, node_style),
                    Span::styled(format!("{} │", " ".repeat(pad)), dim),
                ];
                lines.push(Line::from(spans));
            }

            // Bottom border.
            let bottom = format!("╰{}╯", "─".repeat(inner));
            lines.push(Line::from(Span::styled(bottom, dim)));
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

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let take = max.saturating_sub(1);
    let head: String = s.chars().take(take).collect();
    format!("{head}…")
}
