pub mod bus;
pub mod event;
pub mod store;
pub mod tool;

use std::sync::OnceLock;

use pyo3::prelude::*;
use tokio::runtime::Runtime;

use bus::{PyEventBus, PySubscription};
use event::PyEvent;
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
#[cfg(feature = "cli")]
#[pyfunction]
fn cli_main(args: Vec<String>) {
    crate::cli::run_from_args(args);
}

/// The native Python module. Importable as `import _zymi_core`.
#[pymodule]
fn _zymi_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyEvent>()?;
    m.add_class::<PyEventStore>()?;
    m.add_class::<PyEventBus>()?;
    m.add_class::<PySubscription>()?;
    m.add_class::<PyToolRegistry>()?;
    #[cfg(feature = "cli")]
    m.add_function(wrap_pyfunction!(cli_main, m)?)?;
    Ok(())
}
