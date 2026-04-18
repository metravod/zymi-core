use std::path::Path;

use super::event_fmt::{format_event, EventColor, BOLD, DIM, RESET};
use crate::events::store::{open_store, StoreBackend};
use crate::events::Event;

use super::{runtime, store_path};

pub fn exec(
    stream: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    raw: bool,
    verbose: bool,
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
                if !raw {
                    println!("No events found in stream '{stream_id}'.");
                }
                return Ok(());
            }

            if !raw {
                println!(
                    "{}Stream '{}'{}: {} event(s){}",
                    BOLD,
                    stream_id,
                    RESET,
                    filtered.len(),
                    if let Some(k) = kind {
                        format!(" {DIM}(filtered: {k}){RESET}")
                    } else {
                        String::new()
                    }
                );
                println!();
            }

            for event in filtered {
                if raw {
                    print_raw(event)?;
                } else {
                    print_rich(event, verbose);
                }
            }
        }
        None => {
            let events = rt
                .block_on(store.read_all(0, limit))
                .map_err(|e| format!("failed to read events: {e}"))?;

            let filtered: Vec<_> = events
                .iter()
                .filter(|e| kind.is_none_or(|k| e.kind_tag() == k))
                .collect();

            if filtered.is_empty() {
                if !raw {
                    println!("No events in the store.");
                }
                return Ok(());
            }

            if !raw {
                let streams = rt
                    .block_on(store.list_streams())
                    .map_err(|e| format!("failed to list streams: {e}"))?;

                println!(
                    "{BOLD}Event store{RESET}: {} stream(s), showing up to {} event(s)",
                    streams.len(),
                    limit
                );
                for (sid, count) in &streams {
                    println!("  {DIM}{sid}{RESET}: {count} event(s)");
                }
                println!();
            }

            for event in filtered {
                if raw {
                    print_raw(event)?;
                } else {
                    print_rich(event, verbose);
                }
            }
        }
    }

    Ok(())
}

fn print_raw(event: &Event) -> Result<(), String> {
    let j = serde_json::to_string(event).map_err(|e| format!("serialization error: {e}"))?;
    println!("{j}");
    Ok(())
}

fn print_rich(event: &Event, verbose: bool) {
    let formatted = format_event(event);
    let pad = "  ".repeat(formatted.indent as usize);
    let color = formatted.color.ansi();
    let ts = event.timestamp.format("%H:%M:%S%.3f");
    let tag = event.kind_tag();

    println!(
        "{pad}{DIM}#{:<4} {ts}{RESET} {color}{BOLD}{tag}{RESET} {DIM}source={}{RESET}",
        event.sequence, event.source,
    );

    let detail = if verbose {
        &formatted.full_detail
    } else {
        &formatted.short_detail
    };
    for line in detail.lines() {
        if line.is_empty() {
            continue;
        }
        println!("{pad}  {}{line}{RESET}", detail_color(formatted.color));
    }
}

/// Detail lines use a muted version of the header colour for readability.
fn detail_color(c: EventColor) -> &'static str {
    match c {
        EventColor::Default => "",
        _ => DIM,
    }
}
