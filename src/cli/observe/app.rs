//! Observe TUI state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::config::pipeline::load_pipeline;
use crate::config::PipelineConfig;
use crate::events::store::EventStore;
use crate::events::{Event, EventKind};

use super::super::runs_data::{list_runs, RunSummary};
use super::data::load_run_events;
use super::graph::Graph;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Runs,
    Graph,
    Events,
}

impl Focus {
    pub fn next(self) -> Self {
        match self {
            Focus::Runs => Focus::Graph,
            Focus::Graph => Focus::Events,
            Focus::Events => Focus::Runs,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Focus::Runs => Focus::Events,
            Focus::Graph => Focus::Runs,
            Focus::Events => Focus::Graph,
        }
    }
}

/// Modal popup that drives the fork-resume flow (ADR-0018) from the
/// pipeline graph. Exists only while the user is interacting with it.
#[derive(Debug, Clone)]
pub struct ForkPrompt {
    /// Stream id of the parent run we're forking from.
    pub parent_stream_id: String,
    /// Step id at which to fork — the focused graph node.
    pub fork_at_step: String,
    pub state: ForkState,
    /// When the popup was opened — used to disambiguate the new fork run
    /// from earlier forks with the same `(parent, step)` key.
    pub opened_at: DateTime<Utc>,
    /// Stream id of the forked run, once we locate it in the store.
    pub tail_stream_id: Option<String>,
    /// Latest events of the forked run, for live-log rendering.
    pub tail_events: Vec<Event>,
}

#[derive(Debug, Clone)]
pub enum ForkState {
    /// "Fork from <step>? [Enter] confirm  [Esc] cancel"
    Confirm,
    /// Resume task is in flight.
    Running,
    /// Resume completed — message + new stream id.
    Done {
        new_stream_id: String,
        message: String,
    },
    /// Resume failed; message holds the error.
    Failed(String),
}

pub struct App {
    pub root: PathBuf,
    pub store: Arc<dyn EventStore>,

    pub runs: Vec<RunSummary>,
    pub run_cursor: usize,

    pub events: Vec<Event>,
    pub event_cursor: usize,
    pub expanded_events: Vec<usize>,

    pub graph: Option<Graph>,
    pub graph_cursor: usize,
    pub graph_warning: Option<String>,

    pub focus: Focus,
    pub follow_tail: bool,
    pub should_quit: bool,

    pub fork_prompt: Option<ForkPrompt>,
}

impl App {
    pub fn new(root: PathBuf, store: Arc<dyn EventStore>) -> Self {
        Self {
            root,
            store,
            runs: Vec::new(),
            run_cursor: 0,
            events: Vec::new(),
            event_cursor: 0,
            expanded_events: Vec::new(),
            graph: None,
            graph_cursor: 0,
            graph_warning: None,
            focus: Focus::Runs,
            follow_tail: false,
            should_quit: false,
            fork_prompt: None,
        }
    }

    /// Open the fork-confirm popup for the currently focused graph node and
    /// the currently selected run. Returns false if there's nothing to fork
    /// from (no run / no graph node selected).
    pub fn start_fork_prompt(&mut self) -> bool {
        let Some(run) = self.runs.get(self.run_cursor) else {
            return false;
        };
        let Some(graph) = &self.graph else { return false };
        let Some(node) = graph.node_at(self.graph_cursor) else {
            return false;
        };
        self.fork_prompt = Some(ForkPrompt {
            parent_stream_id: run.stream_id.clone(),
            fork_at_step: node.id.clone(),
            state: ForkState::Confirm,
            opened_at: Utc::now(),
            tail_stream_id: None,
            tail_events: Vec::new(),
        });
        true
    }

    /// While the fork-resume popup is in `Running` state, locate the newly
    /// spawned stream and pull its latest events for the live log. Cheap
    /// enough to run on a 500ms tick.
    pub async fn poll_fork_tail(&mut self) -> Result<(), String> {
        let Some(prompt) = &self.fork_prompt else { return Ok(()) };
        if !matches!(prompt.state, ForkState::Running) {
            return Ok(());
        }

        let stream_id = match &prompt.tail_stream_id {
            Some(id) => id.clone(),
            None => {
                let runs = list_runs(self.store.clone(), None).await?;
                let parent = prompt.parent_stream_id.clone();
                let step = prompt.fork_at_step.clone();
                let opened_at = prompt.opened_at;
                let found = runs.into_iter().find(|r| {
                    r.fork
                        .as_ref()
                        .is_some_and(|f| f.parent_stream_id == parent && f.fork_at_step == step)
                        && r.started_at >= opened_at
                });
                let Some(run) = found else { return Ok(()) };
                let id = run.stream_id.clone();
                if let Some(p) = self.fork_prompt.as_mut() {
                    p.tail_stream_id = Some(id.clone());
                }
                id
            }
        };

        let events = load_run_events(self.store.clone(), &stream_id).await?;
        if let Some(p) = self.fork_prompt.as_mut() {
            p.tail_events = events;
        }
        Ok(())
    }

    pub fn clear_fork_prompt(&mut self) {
        self.fork_prompt = None;
    }

    pub async fn reload_runs(&mut self) -> Result<(), String> {
        let prev_stream = self
            .runs
            .get(self.run_cursor)
            .map(|r| r.stream_id.clone());
        self.runs = list_runs(self.store.clone(), None).await?;
        if let Some(stream) = prev_stream {
            if let Some(idx) = self.runs.iter().position(|r| r.stream_id == stream) {
                self.run_cursor = idx;
            } else {
                self.run_cursor = 0;
            }
        } else {
            self.run_cursor = 0;
        }
        Ok(())
    }

    pub fn select_run_by_stream(&mut self, stream_id: &str) {
        if let Some(idx) = self.runs.iter().position(|r| r.stream_id == stream_id) {
            self.run_cursor = idx;
        }
    }

    pub async fn load_selected_run(&mut self) -> Result<(), String> {
        let Some(run) = self.runs.get(self.run_cursor).cloned() else {
            self.events.clear();
            self.graph = None;
            return Ok(());
        };

        self.events = load_run_events(self.store.clone(), &run.stream_id).await?;
        self.event_cursor = 0;
        self.expanded_events.clear();

        self.rebuild_graph(&run.pipeline);
        self.graph_cursor = 0;

        if self.follow_tail {
            self.event_cursor = self.events.len().saturating_sub(1);
        }

        Ok(())
    }

    fn rebuild_graph(&mut self, pipeline_name: &str) {
        let path = self.root.join("pipelines").join(format!("{pipeline_name}.yml"));
        let path = if path.exists() {
            path
        } else {
            self.root
                .join("pipelines")
                .join(format!("{pipeline_name}.yaml"))
        };

        self.graph_warning = None;
        let vars: HashMap<String, String> = HashMap::new();
        match load_pipeline(&path, &vars) {
            Ok(cfg) => {
                let graph = Graph::build(&cfg, &self.events);
                self.graph = Some(graph);
            }
            Err(e) => {
                self.graph_warning = Some(format!("pipeline config not loadable: {e}"));
                self.graph = Some(self.linear_fallback(pipeline_name));
            }
        }
    }

    /// Linear fallback built from `WorkflowNodeStarted` events when the
    /// pipeline config is missing. Constructs a synthetic [`PipelineConfig`]
    /// with one step per observed node in order of first appearance.
    fn linear_fallback(&self, name: &str) -> Graph {
        use crate::config::pipeline::PipelineStep;

        let mut seen: Vec<String> = Vec::new();
        for event in &self.events {
            if let EventKind::WorkflowNodeStarted { node_id, .. } = &event.kind {
                if !seen.contains(node_id) {
                    seen.push(node_id.clone());
                }
            }
        }

        let steps = seen
            .iter()
            .enumerate()
            .map(|(i, id)| PipelineStep {
                id: id.clone(),
                agent: String::new(),
                task: String::new(),
                depends_on: if i == 0 {
                    Vec::new()
                } else {
                    vec![seen[i - 1].clone()]
                },
            })
            .collect();

        let cfg = PipelineConfig {
            name: name.to_string(),
            description: None,
            inputs: Vec::new(),
            steps,
            output: None,
        };

        Graph::build(&cfg, &self.events)
    }

    pub fn move_run_cursor(&mut self, delta: i32) {
        self.run_cursor = clamp_step(self.run_cursor, delta, self.runs.len());
    }

    pub fn move_graph_cursor(&mut self, delta: i32) {
        let len = self.graph.as_ref().map(|g| g.nodes.len()).unwrap_or(0);
        self.graph_cursor = clamp_step(self.graph_cursor, delta, len);
    }

    pub fn move_event_cursor(&mut self, delta: i32) {
        self.event_cursor = clamp_step(self.event_cursor, delta, self.events.len());
    }

    pub fn toggle_event_expanded(&mut self) {
        if let Some(pos) = self.expanded_events.iter().position(|&i| i == self.event_cursor) {
            self.expanded_events.remove(pos);
        } else {
            self.expanded_events.push(self.event_cursor);
        }
    }

    pub fn focus_event_on_selected_node(&mut self) {
        let Some(graph) = &self.graph else { return };
        let Some(node) = graph.node_at(self.graph_cursor) else { return };
        let target_id = node.id.clone();
        if let Some(idx) = self.events.iter().position(|e| {
            matches!(
                &e.kind,
                EventKind::WorkflowNodeStarted { node_id, .. } if *node_id == target_id
            )
        }) {
            self.event_cursor = idx;
            self.focus = Focus::Events;
        }
    }
}

fn clamp_step(cursor: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let max = len - 1;
    let new = cursor as i32 + delta;
    new.clamp(0, max as i32) as usize
}

