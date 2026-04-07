use std::sync::Arc;

use pyo3::prelude::*;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use crate::events::bus::EventBus;
use crate::events::store::EventStore;
use crate::events::Event;

use super::event::PyEvent;
use super::runtime;
use super::store::PyEventStore;

/// Python wrapper for the in-process event bus.
///
/// Persists events to the store (source of truth), then fans out to subscribers.
#[pyclass(name = "EventBus")]
pub struct PyEventBus {
    pub(crate) inner: Arc<EventBus>,
}

impl PyEventBus {
    /// Wrap a pre-existing `Arc<EventBus>`. Used by
    /// [`super::runtime::PyRuntime`] so Python subscribers see the same
    /// events the runtime publishes, without constructing a second bus on
    /// top of the same store.
    #[cfg(feature = "runtime")]
    pub(crate) fn from_arc(inner: Arc<EventBus>) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyEventBus {
    /// Create a new EventBus backed by the given EventStore.
    #[new]
    fn new(store: &PyEventStore) -> Self {
        let store: Arc<dyn EventStore> = store.inner.clone();
        Self {
            inner: Arc::new(EventBus::new(store)),
        }
    }

    /// Publish an event to the bus. Persists to store first, then delivers to subscribers.
    fn publish(&self, py: Python<'_>, event: PyEvent) -> PyResult<()> {
        let bus = self.inner.clone();
        let inner_event = event.inner;
        py.allow_threads(|| {
            runtime().block_on(bus.publish(inner_event))
        })
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("{e}")))
    }

    /// Subscribe to all events. Returns a Subscription.
    fn subscribe(&self, py: Python<'_>) -> PySubscription {
        let bus = self.inner.clone();
        let rx = py.allow_threads(|| {
            runtime().block_on(bus.subscribe())
        });
        PySubscription {
            rx: Arc::new(Mutex::new(rx)),
        }
    }

    /// Subscribe to events matching a specific correlation ID.
    fn subscribe_correlation(&self, py: Python<'_>, correlation_id: &str) -> PyResult<PySubscription> {
        let uuid = Uuid::parse_str(correlation_id)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(format!("Invalid UUID: {e}")))?;
        let bus = self.inner.clone();
        let rx = py.allow_threads(|| {
            runtime().block_on(bus.subscribe_correlation(uuid))
        });
        Ok(PySubscription {
            rx: Arc::new(Mutex::new(rx)),
        })
    }

    fn __repr__(&self) -> String {
        "EventBus()".to_string()
    }
}

/// A subscription to events from the EventBus.
///
/// Supports blocking recv with optional timeout, non-blocking try_recv,
/// and Python iteration protocol.
#[pyclass(name = "Subscription")]
pub struct PySubscription {
    rx: Arc<Mutex<mpsc::Receiver<Arc<Event>>>>,
}

#[pymethods]
impl PySubscription {
    /// Receive the next event, blocking until one is available.
    ///
    /// Args:
    ///     timeout_secs: Optional timeout in seconds. Returns None on timeout.
    ///
    /// Returns None if the bus is closed or timeout expires.
    #[pyo3(signature = (timeout_secs = None))]
    fn recv(&self, py: Python<'_>, timeout_secs: Option<f64>) -> PyResult<Option<PyEvent>> {
        let rx = self.rx.clone();
        py.allow_threads(|| {
            runtime().block_on(async {
                let mut guard = rx.lock().await;
                match timeout_secs {
                    Some(secs) => {
                        let duration = std::time::Duration::from_secs_f64(secs);
                        match tokio::time::timeout(duration, guard.recv()).await {
                            Ok(Some(event)) => Ok(Some(PyEvent::from_inner((*event).clone()))),
                            Ok(None) => Ok(None),
                            Err(_) => Ok(None), // timeout
                        }
                    }
                    None => match guard.recv().await {
                        Some(event) => Ok(Some(PyEvent::from_inner((*event).clone()))),
                        None => Ok(None),
                    },
                }
            })
        })
    }

    /// Try to receive an event without blocking. Returns None if no event is ready.
    fn try_recv(&self) -> PyResult<Option<PyEvent>> {
        let rx = self.rx.clone();
        runtime().block_on(async {
            let mut guard = rx.lock().await;
            match guard.try_recv() {
                Ok(event) => Ok(Some(PyEvent::from_inner((*event).clone()))),
                Err(_) => Ok(None),
            }
        })
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Iterate: blocks until next event. Stops when the bus is closed.
    fn __next__(&self, py: Python<'_>) -> PyResult<Option<PyEvent>> {
        self.recv(py, None)
    }

    fn __repr__(&self) -> String {
        "Subscription()".to_string()
    }
}
