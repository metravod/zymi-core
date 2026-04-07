use std::path::Path;

use std::sync::Arc;

use crate::events::store::{open_store, EventStore, StoreBackend};

use super::{runtime, store_path};

pub fn exec(stream: Option<&str>, root: impl AsRef<Path>) -> Result<(), String> {
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
        Some(stream_id) => verify_stream(&store, &rt, stream_id),
        None => {
            let streams = rt
                .block_on(store.list_streams())
                .map_err(|e| format!("failed to list streams: {e}"))?;

            if streams.is_empty() {
                println!("No streams in the store. Nothing to verify.");
                return Ok(());
            }

            let mut all_ok = true;
            for (stream_id, event_count) in &streams {
                match rt.block_on(store.verify_chain(stream_id)) {
                    Ok(verified) => {
                        println!("  {stream_id}: {verified}/{event_count} events OK");
                    }
                    Err(e) => {
                        println!("  {stream_id}: FAILED — {e}");
                        all_ok = false;
                    }
                }
            }

            println!();
            if all_ok {
                println!("All {} stream(s) verified successfully.", streams.len());
            } else {
                return Err("hash chain verification failed for one or more streams".into());
            }

            Ok(())
        }
    }
}

fn verify_stream(
    store: &Arc<dyn EventStore>,
    rt: &tokio::runtime::Runtime,
    stream_id: &str,
) -> Result<(), String> {
    match rt.block_on(store.verify_chain(stream_id)) {
        Ok(count) => {
            if count == 0 {
                println!("Stream '{stream_id}': empty (nothing to verify).");
            } else {
                println!("Stream '{stream_id}': {count} event(s) verified, hash chain intact.");
            }
            Ok(())
        }
        Err(e) => Err(format!("stream '{stream_id}': hash chain BROKEN — {e}")),
    }
}
