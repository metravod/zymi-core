use pyo3::prelude::*;
use pyo3::types::PyDict;
use uuid::Uuid;

use crate::events::{Event, EventKind};

/// Python wrapper for the core Event type.
///
/// EventKind is represented as a Python dict matching the serde JSON format:
/// `{"type": "user_message_received", "data": {"content": ..., "connector": "telegram"}}`
#[pyclass(name = "Event")]
#[derive(Clone)]
pub struct PyEvent {
    pub(crate) inner: Event,
}

impl PyEvent {
    pub fn from_inner(event: Event) -> Self {
        Self { inner: event }
    }
}

#[pymethods]
impl PyEvent {
    /// Create a new Event.
    ///
    /// Args:
    ///     stream_id: Stream identifier (typically conversation_id).
    ///     kind: Event kind as a dict, e.g. `{"type": "response_ready", "data": {"conversation_id": "c1", "content": "hi"}}`.
    ///     source: Origin of the event (e.g. "python", "agent", "cli").
    #[new]
    fn new(stream_id: String, kind: &Bound<'_, PyDict>, source: String) -> PyResult<Self> {
        let kind_json = pythonize_dict_to_json(kind)?;
        let event_kind: EventKind = serde_json::from_str(&kind_json)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(format!("Invalid event kind: {e}")))?;

        Ok(Self {
            inner: Event::new(stream_id, event_kind, source),
        })
    }

    /// Unique event identifier (UUID).
    #[getter]
    fn id(&self) -> String {
        self.inner.id.to_string()
    }

    /// Stream identifier.
    #[getter]
    fn stream_id(&self) -> &str {
        &self.inner.stream_id
    }

    /// Sequence number within the stream (assigned by EventStore on append).
    #[getter]
    fn sequence(&self) -> u64 {
        self.inner.sequence
    }

    /// ISO 8601 timestamp.
    #[getter]
    fn timestamp(&self) -> String {
        self.inner.timestamp.to_rfc3339()
    }

    /// Event source.
    #[getter]
    fn source(&self) -> &str {
        &self.inner.source
    }

    /// Correlation ID (UUID string or None).
    #[getter]
    fn correlation_id(&self) -> Option<String> {
        self.inner.correlation_id.map(|u| u.to_string())
    }

    /// Causation ID (UUID string or None).
    #[getter]
    fn causation_id(&self) -> Option<String> {
        self.inner.causation_id.map(|u| u.to_string())
    }

    /// Short string tag for the event kind (e.g. "user_message_received").
    #[getter]
    fn kind_tag(&self) -> &str {
        self.inner.kind_tag()
    }

    /// Event kind as a Python dict.
    #[getter]
    fn kind<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let json = serde_json::to_string(&self.inner.kind)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Serialization error: {e}")))?;
        json_to_pydict(py, &json)
    }

    /// Set correlation ID.
    fn with_correlation(&mut self, id: &str) -> PyResult<()> {
        let uuid = Uuid::parse_str(id)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(format!("Invalid UUID: {e}")))?;
        self.inner.correlation_id = Some(uuid);
        Ok(())
    }

    /// Set causation ID.
    fn with_causation(&mut self, id: &str) -> PyResult<()> {
        let uuid = Uuid::parse_str(id)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(format!("Invalid UUID: {e}")))?;
        self.inner.causation_id = Some(uuid);
        Ok(())
    }

    /// Serialize the full event to JSON.
    fn to_json(&self) -> PyResult<String> {
        serde_json::to_string(&self.inner)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("Serialization error: {e}")))
    }

    /// Deserialize an event from JSON.
    #[staticmethod]
    fn from_json(json: &str) -> PyResult<Self> {
        let event: Event = serde_json::from_str(json)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyValueError, _>(format!("Invalid JSON: {e}")))?;
        Ok(Self { inner: event })
    }

    fn __repr__(&self) -> String {
        format!(
            "Event(stream_id='{}', kind='{}', seq={}, source='{}')",
            self.inner.stream_id,
            self.inner.kind_tag(),
            self.inner.sequence,
            self.inner.source,
        )
    }
}

/// Convert a Python dict to a JSON string via Python's json.dumps.
fn pythonize_dict_to_json(dict: &Bound<'_, PyDict>) -> PyResult<String> {
    let py = dict.py();
    let json_mod = py.import_bound("json")?;
    let json_str = json_mod.call_method1("dumps", (dict,))?;
    json_str.extract::<String>()
}

/// Convert a JSON string to a Python dict via Python's json.loads.
fn json_to_pydict<'py>(py: Python<'py>, json: &str) -> PyResult<Bound<'py, PyDict>> {
    let json_mod = py.import_bound("json")?;
    let obj = json_mod.call_method1("loads", (json,))?;
    obj.downcast_into::<PyDict>()
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyTypeError, _>(format!("Expected dict: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventKind;
    use crate::types::Message;

    #[test]
    fn event_roundtrip_via_json() {
        let event = Event::new(
            "s1".into(),
            EventKind::UserMessageReceived {
                content: Message::User("hello".into()),
                connector: "test".into(),
            },
            "test".into(),
        );
        let py_event = PyEvent::from_inner(event);
        let json = py_event.to_json().unwrap();
        let restored = PyEvent::from_json(&json).unwrap();
        assert_eq!(restored.inner.stream_id, "s1");
        assert_eq!(restored.inner.kind_tag(), "user_message_received");
    }

    #[test]
    fn event_repr() {
        let event = Event::new(
            "conv-1".into(),
            EventKind::ResponseReady {
                conversation_id: "conv-1".into(),
                content: "hi".into(),
            },
            "agent".into(),
        );
        let py_event = PyEvent::from_inner(event);
        let repr = py_event.__repr__();
        assert!(repr.contains("response_ready"));
        assert!(repr.contains("conv-1"));
    }
}
