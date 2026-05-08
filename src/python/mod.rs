pub mod auto_discover;
pub mod bus;
pub mod event;
#[cfg(feature = "runtime")]
pub mod py_runtime;
pub mod store;
pub mod tool;

use std::sync::OnceLock;

use pyo3::prelude::*;
use tokio::runtime::Runtime;

use bus::{PyEventBus, PySubscription};
use event::PyEvent;
#[cfg(feature = "runtime")]
use py_runtime::{PyRunPipelineResult, PyRuntime, PyStepResult};
use store::PyEventStore;
use tool::PyToolRegistry;

/// Shared tokio runtime for all Python ↔ async-Rust bridging.
/// Created lazily on first use; lives for the process lifetime.
pub(crate) fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime for zymi-core Python bridge")
    })
}

/// Run the CLI from Python with explicit argv (e.g. `["zymi", "init"]`).
///
/// Releases the GIL for the duration of the CLI run. The CLI builds its own
/// tokio runtime and `block_on`s the pipeline; without `allow_threads`, the
/// main thread keeps the GIL while sitting in Rust, and any Python `@tool`
/// invoked through `tokio::task::spawn_blocking` + `Python::with_gil` would
/// deadlock waiting on the GIL we still hold.
#[cfg(feature = "cli")]
#[pyfunction]
fn cli_main(py: Python<'_>, args: Vec<String>) {
    py.allow_threads(|| crate::cli::run_from_args(args));
}

/// The native Python module. Importable as `from zymi import _zymi_core`.
#[pymodule]
fn _zymi_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyEvent>()?;
    m.add_class::<PyEventStore>()?;
    m.add_class::<PyEventBus>()?;
    m.add_class::<PySubscription>()?;
    m.add_class::<PyToolRegistry>()?;
    #[cfg(feature = "runtime")]
    {
        m.add_class::<PyRuntime>()?;
        m.add_class::<PyRunPipelineResult>()?;
        m.add_class::<PyStepResult>()?;
    }
    #[cfg(feature = "cli")]
    m.add_function(wrap_pyfunction!(cli_main, m)?)?;
    Ok(())
}
