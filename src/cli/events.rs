use std::path::Path;

use super::event_fmt::{format_event, EventColor, BOLD, DIM, RESET};
use crate::events::store::{open_store_async, StoreBackend};
use crate::events::Event;

use super::{resolve_store_backend_for_cli, runtime};

pub fn exec(
    stream: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    raw: bool,
    verbose: bool,
    root: impl AsRef<Path>,
) -> Result<(), String> {
    let backend = resolve_store_backend_for_cli(root.as_ref())?;
    if let StoreBackend::Sqlite { path } = &backend {
        if !path.exists() {
            return Err(format!(
                "no event store found at {}. Run a pipeline first or check --dir.",
                path.display()
            ));
        }
    }

    let rt = runtime();
    let store = rt
        .block_on(open_store_async(backend))
        .map_err(|e| format!("failed to open event store: {e}"))?;

    match stream {
        Some(stream_id) => {
            let mut events = rt
                .block_on(store.read_stream(stream_id, 1))
                .map_err(|e| format!("failed to read stream: {e}"))?;

            // A run's tool calls and LLM calls live in per-step sub-streams
            // (`<run>:step:<id>`, ADR-0016 §6). Showing only the parent hid
            // exactly the events a failure investigation needs, so merge them
            // in, in time order.
            let step_prefix = format!("{stream_id}:step:");
            let sub_streams: Vec<String> = rt
                .block_on(store.list_streams())
                .map_err(|e| format!("failed to list streams: {e}"))?
                .into_iter()
                .map(|(sid, _)| sid)
                .filter(|sid| sid.starts_with(&step_prefix))
                .collect();
            for sid in &sub_streams {
                events.extend(
                    rt.block_on(store.read_stream(sid, 1))
                        .map_err(|e| format!("failed to read stream {sid}: {e}"))?,
                );
            }
            events.sort_by_key(|e| e.timestamp);

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
                    "{}Stream '{}'{}: {} event(s){}{}",
                    BOLD,
                    stream_id,
                    RESET,
                    filtered.len(),
                    if sub_streams.is_empty() {
                        String::new()
                    } else {
                        format!(" {DIM}(incl. {} step stream(s)){RESET}", sub_streams.len())
                    },
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
                    let step = event.stream_id.strip_prefix(&step_prefix);
                    print_rich(event, verbose, step);
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
                    print_rich(event, verbose, None);
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

/// `step`: the step id when the event comes from a `<run>:step:<id>`
/// sub-stream, shown so merged step events stay attributable.
fn print_rich(event: &Event, verbose: bool, step: Option<&str>) {
    let formatted = format_event(event);
    let pad = "  ".repeat(formatted.indent as usize);
    let color = formatted.color.ansi();
    let ts = event.timestamp.format("%H:%M:%S%.3f");
    let tag = event.kind_tag();

    let step = step.map(|s| format!(" [{s}]")).unwrap_or_default();
    println!(
        "{pad}{DIM}#{:<4} {ts}{RESET}{step} {color}{BOLD}{tag}{RESET} {DIM}source={}{RESET}",
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
