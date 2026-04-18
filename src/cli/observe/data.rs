//! Data loading for the observe TUI.

use std::sync::Arc;

use crate::events::store::EventStore;
use crate::events::Event;

/// Load all events of a stream in order.
pub async fn load_run_events(
    store: Arc<dyn EventStore>,
    stream_id: &str,
) -> Result<Vec<Event>, String> {
    store
        .read_stream(stream_id, 1)
        .await
        .map_err(|e| format!("failed to read stream {stream_id}: {e}"))
}
