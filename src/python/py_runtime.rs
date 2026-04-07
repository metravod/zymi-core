//! Python bridge over [`crate::runtime::Runtime`] — slice 2 of the runtime
//! unification (ADR-0013).
//!
//! Before slice 2, Python drove pipelines by shelling out to `cli_main`
//! (effectively `zymi run` in-process). Now Python builds a [`Runtime`]
//! directly and dispatches [`crate::commands::RunPipeline`] commands at
//! the same [`crate::handlers::run_pipeline::handle`] function the CLI
//! uses. Both entrypoints share one canonical execution path, matching
//! what ADR-0013 v1.1 describes as the "Adapters → Application Layer"
//! arrow.
//!
//! Scope deliberately kept small:
//!
//! - `Runtime.for_project(path, approval="terminal")` loads a project
//!   from disk and builds a runtime. `approval` accepts `"terminal"`
//!   (default) or `"none"`; a pluggable Python callback handler is a
//!   follow-up, not part of slice 2.
//! - `run_pipeline(name, inputs)` is synchronous from Python — it blocks
//!   on the shared tokio runtime. Asyncio integration is a follow-up.
//! - `bus()` and `store()` return Python wrappers sharing the *same*
//!   `Arc` as the runtime, so Python subscribers see the events the
//!   handler publishes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use crate::approval::{ApprovalHandler, TerminalApprovalHandler};
use crate::commands::RunPipeline;
use crate::config::load_project_dir;
use crate::handlers::run_pipeline as handler;
use crate::runtime::Runtime;

use super::bus::PyEventBus;
use super::runtime as shared_tokio;
use super::store::PyEventStore;

/// Python-visible result of a single pipeline step.
#[pyclass(name = "StepResult")]
#[derive(Clone)]
pub struct PyStepResult {
    #[pyo3(get)]
    pub step_id: String,
    #[pyo3(get)]
    pub agent_name: String,
    #[pyo3(get)]
    pub output: String,
    #[pyo3(get)]
    pub iterations: usize,
    #[pyo3(get)]
    pub success: bool,
}

#[pymethods]
impl PyStepResult {
    fn __repr__(&self) -> String {
        format!(
            "StepResult(step_id={:?}, success={}, iterations={})",
            self.step_id, self.success, self.iterations
        )
    }
}

/// Python-visible result of a full pipeline run.
#[pyclass(name = "RunPipelineResult")]
pub struct PyRunPipelineResult {
    #[pyo3(get)]
    pub pipeline_name: String,
    #[pyo3(get)]
    pub success: bool,
    #[pyo3(get)]
    pub final_output: Option<String>,
    step_results: HashMap<String, PyStepResult>,
}

#[pymethods]
impl PyRunPipelineResult {
    /// Ordered mapping of `step_id → StepResult`.
    fn step_results(&self) -> HashMap<String, PyStepResult> {
        self.step_results.clone()
    }

    /// Lookup a single step result by id.
    fn step(&self, step_id: &str) -> Option<PyStepResult> {
        self.step_results.get(step_id).cloned()
    }

    fn __repr__(&self) -> String {
        format!(
            "RunPipelineResult(pipeline={:?}, success={}, steps={})",
            self.pipeline_name,
            self.success,
            self.step_results.len()
        )
    }
}

/// Python wrapper over [`crate::runtime::Runtime`].
#[pyclass(name = "Runtime")]
pub struct PyRuntime {
    inner: Arc<Runtime>,
}

#[pymethods]
impl PyRuntime {
    /// Load a project from disk and build a [`Runtime`] for it.
    ///
    /// Args:
    ///     path: Project root containing ``project.yml``.
    ///     approval: One of ``"terminal"`` (default) or ``"none"``.
    ///         Matches the behaviour of ``zymi run`` / ``zymi serve``:
    ///         with ``"terminal"``, any tool call requiring human
    ///         approval prompts on stdin and fail-closes on EOF; with
    ///         ``"none"``, such tool calls resolve to
    ///         ``[requires approval but no approval handler configured]``.
    ///         A pluggable Python callback handler is a follow-up.
    #[classmethod]
    #[pyo3(signature = (path, approval = "terminal"))]
    fn for_project(
        _cls: &Bound<'_, pyo3::types::PyType>,
        path: &str,
        approval: &str,
    ) -> PyResult<Self> {
        let root = PathBuf::from(path);
        if !root.join("project.yml").exists() {
            return Err(PyRuntimeError::new_err(format!(
                "no project.yml found in {}. Run `zymi init` first.",
                root.display()
            )));
        }

        let workspace = load_project_dir(&root)
            .map_err(|e| PyRuntimeError::new_err(format!("failed to load project: {e}")))?;

        let mut builder = Runtime::builder(workspace, root);
        match approval {
            "terminal" => {
                let handler: Arc<dyn ApprovalHandler> = Arc::new(TerminalApprovalHandler::new());
                builder = builder.with_approval_handler(handler);
            }
            "none" => {}
            other => {
                return Err(PyValueError::new_err(format!(
                    "unknown approval handler {other:?} — expected \"terminal\" or \"none\""
                )));
            }
        }

        let runtime = builder
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("failed to build runtime: {e}")))?;

        Ok(Self {
            inner: Arc::new(runtime),
        })
    }

    /// Execute a pipeline by name. Blocks the calling thread until the
    /// pipeline completes.
    ///
    /// Args:
    ///     name: Pipeline name as declared in ``pipelines/<name>.yml``.
    ///     inputs: Mapping of input name to string value. Defaults to
    ///         an empty dict.
    #[pyo3(signature = (name, inputs = None))]
    fn run_pipeline(
        &self,
        py: Python<'_>,
        name: &str,
        inputs: Option<HashMap<String, String>>,
    ) -> PyResult<PyRunPipelineResult> {
        let rt = Arc::clone(&self.inner);
        let cmd = RunPipeline::new(name.to_string(), inputs.unwrap_or_default());

        let result = py
            .allow_threads(|| shared_tokio().block_on(handler::handle(&rt, cmd)))
            .map_err(|e| PyRuntimeError::new_err(format!("pipeline run failed: {e}")))?;

        let step_results = result
            .step_results
            .into_iter()
            .map(|(id, sr)| {
                (
                    id,
                    PyStepResult {
                        step_id: sr.step_id,
                        agent_name: sr.agent_name,
                        output: sr.output,
                        iterations: sr.iterations,
                        success: sr.success,
                    },
                )
            })
            .collect();

        Ok(PyRunPipelineResult {
            pipeline_name: result.pipeline_name,
            success: result.success,
            final_output: result.final_output,
            step_results,
        })
    }

    /// Return a Python ``EventBus`` wrapping the same ``Arc`` the
    /// runtime publishes into. Subscribing via the returned bus sees the
    /// same events the handler emits.
    fn bus(&self) -> PyEventBus {
        PyEventBus::from_arc(Arc::clone(self.inner.bus()))
    }

    /// Return a Python ``EventStore`` wrapping the same ``Arc`` the
    /// runtime uses. Reading through it goes straight to the same SQLite
    /// file the pipeline writes to.
    fn store(&self) -> PyEventStore {
        PyEventStore::from_arc(Arc::clone(self.inner.store()))
    }

    /// Absolute path of the project root.
    fn project_root(&self) -> String {
        self.inner.project_root().display().to_string()
    }

    /// Names of pipelines declared in the loaded workspace.
    fn pipelines(&self) -> Vec<String> {
        let mut names: Vec<String> =
            self.inner.workspace().pipelines.keys().cloned().collect();
        names.sort();
        names
    }

    fn __repr__(&self) -> String {
        format!(
            "Runtime(project_root={:?}, pipelines={})",
            self.inner.project_root().display().to_string(),
            self.inner.workspace().pipelines.len()
        )
    }
}
