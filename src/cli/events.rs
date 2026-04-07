use std::path::Path;

use crate::events::store::{open_store, StoreBackend};

use super::{runtime, store_path};

pub fn exec(
    stream: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    json: bool,
    root: impl AsRef<Path>,
) -> Result<(), String> {
    let db_path = store_path(root.as_ref());
    if !db_path.exists() {
        return Err(format!(
            "no event store found at {}. Run a pipeline first or check --dir.",
            db_path.display()
        ));
    }

    let store = open_store(StoreBackend::Sqlite { path: db_path.clone() })
        .map_err(|e| format!("failed to open event store: {e}"))?;

    let rt = runtime();

    match stream {
        Some(stream_id) => {
            let events = rt
                .block_on(store.read_stream(stream_id, 1))
                .map_err(|e| format!("failed to read stream: {e}"))?;

            let filtered: Vec<_> = events
                .iter()
                .filter(|e| kind.is_none_or(|k| e.kind_tag() == k))
                .take(limit)
                .collect();

            if filtered.is_empty() {
                println!("No events found in stream '{stream_id}'.");
                return Ok(());
            }

            println!(
                "Stream '{}': {} event(s){}",
                stream_id,
                filtered.len(),
                if let Some(k) = kind {
                    format!(" (filtered by kind='{k}')")
                } else {
                    String::new()
                }
            );
            println!();

            for event in filtered {
                if json {
                    let j = serde_json::to_string(event)
                        .map_err(|e| format!("serialization error: {e}"))?;
                    println!("{j}");
                } else {
                    print_event(event);
                }
            }
        }
        None => {
            // Show all events globally
            let events = rt
                .block_on(store.read_all(0, limit))
                .map_err(|e| format!("failed to read events: {e}"))?;

            let filtered: Vec<_> = events
                .iter()
                .filter(|e| kind.is_none_or(|k| e.kind_tag() == k))
                .collect();

            if filtered.is_empty() {
                println!("No events in the store.");
                return Ok(());
            }

            // Also show stream summary
            let streams = rt
                .block_on(store.list_streams())
                .map_err(|e| format!("failed to list streams: {e}"))?;

            println!(
                "Event store: {} stream(s), showing up to {} event(s)",
                streams.len(),
                limit
            );
            for (sid, count) in &streams {
                println!("  {sid}: {count} event(s)");
            }
            println!();

            for event in filtered {
                if json {
                    let j = serde_json::to_string(event)
                        .map_err(|e| format!("serialization error: {e}"))?;
                    println!("{j}");
                } else {
                    print_event(event);
                }
            }
        }
    }

    Ok(())
}

fn print_event(event: &crate::events::Event) {
    println!(
        "  #{:<4} [{:<30}] stream={:<16} source={:<12} {}",
        event.sequence,
        event.kind_tag(),
        event.stream_id,
        event.source,
        event.timestamp.format("%Y-%m-%d %H:%M:%S"),
    );
}
