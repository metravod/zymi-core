use std::path::Path;
use std::sync::Arc;

use pyo3::prelude::*;

use crate::events::store::{EventStore, SqliteEventStore};

use super::event::PyEvent;
use super::runtime;

/// Python wrapper for SqliteEventStore.
///
/// All async operations run on a shared tokio runtime with GIL released.
#[pyclass(name = "EventStore")]
pub struct PyEventStore {
    pub(crate) inner: Arc<SqliteEventStore>,
}

#[pymethods]
impl PyEventStore {
    /// Open or create a SQLite-backed event store.
    ///
    /// Args:
    ///     path: Path to the SQLite database file.
    #[new]
    fn new(path: &str) -> PyResult<Self> {
        let store = SqliteEventStore::new(Path::new(path))
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyIOError, _>(format!("{e}")))?;
        Ok(Self {
            inner: Arc::new(store),
        })
    }

    /// Append an event to its stream. Assigns the next sequence number.
    /// Returns the assigned sequence number.
    fn append(&self, py: Python<'_>, event: &mut PyEvent) -> PyResult<u64> {
        let store = self.inner.clone();
        let inner = &mut event.inner;
        py.allow_threads(|| {
            runtime().block_on(store.append(inner))
        })
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("{e}")))
    }

    /// Read all events in a stream starting from `from_seq` (inclusive).
    fn read_stream(&self, py: Python<'_>, stream_id: &str, from_seq: u64) -> PyResult<Vec<PyEvent>> {
        let store = self.inner.clone();
        let stream_id = stream_id.to_owned();
        let events = py.allow_threads(|| {
            runtime().block_on(store.read_stream(&stream_id, from_seq))
        })
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("{e}")))?;

        Ok(events.into_iter().map(PyEvent::from_inner).collect())
    }

    /// Read events across all streams (global order).
    fn read_all(&self, py: Python<'_>, from_global_seq: u64, limit: usize) -> PyResult<Vec<PyEvent>> {
        let store = self.inner.clone();
        let events = py.allow_threads(|| {
            runtime().block_on(store.read_all(from_global_seq, limit))
        })
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("{e}")))?;

        Ok(events.into_iter().map(PyEvent::from_inner).collect())
    }

    /// Get the last sequence number for a stream, or 0 if empty.
    fn last_sequence(&self, py: Python<'_>, stream_id: &str) -> PyResult<u64> {
        let store = self.inner.clone();
        let stream_id = stream_id.to_owned();
        py.allow_threads(|| {
            runtime().block_on(store.last_sequence(&stream_id))
        })
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("{e}")))
    }

    /// Count events in a stream, optionally filtered by kind tag.
    #[pyo3(signature = (stream_id, kind_tag=None))]
    fn count(&self, py: Python<'_>, stream_id: &str, kind_tag: Option<&str>) -> PyResult<u64> {
        let store = self.inner.clone();
        let stream_id = stream_id.to_owned();
        let kind_tag = kind_tag.map(|s| s.to_owned());
        py.allow_threads(|| {
            runtime().block_on(store.count(&stream_id, kind_tag.as_deref()))
        })
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("{e}")))
    }

    /// Verify the hash chain integrity for a stream.
    /// Returns the number of verified events.
    fn verify_chain(&self, py: Python<'_>, stream_id: &str) -> PyResult<u64> {
        let store = self.inner.clone();
        let stream_id = stream_id.to_owned();
        py.allow_threads(|| {
            runtime().block_on(store.verify_chain(&stream_id))
        })
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("{e}")))
    }

    fn __repr__(&self) -> String {
        "EventStore(sqlite)".to_string()
    }
}
