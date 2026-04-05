use std::path::Path;

use crate::events::store::{EventStore, SqliteEventStore};
use crate::events::EventKind;

use super::{runtime, store_path};

pub fn exec(
    stream_id: &str,
    from_seq: u64,
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

    let store = SqliteEventStore::new(&db_path)
        .map_err(|e| format!("failed to open event store: {e}"))?;

    let rt = runtime();

    let events = rt
        .block_on(store.read_stream(stream_id, from_seq))
        .map_err(|e| format!("failed to read stream: {e}"))?;

    if events.is_empty() {
        println!("No events in stream '{stream_id}' from sequence {from_seq}.");
        return Ok(());
    }

    println!(
        "Replaying stream '{}': {} event(s) from seq {}",
        stream_id,
        events.len(),
        from_seq
    );
    println!();

    for event in &events {
        if json {
            let j = serde_json::to_string(event)
                .map_err(|e| format!("serialization error: {e}"))?;
            println!("{j}");
        } else {
            print_replay_event(event);
        }
    }

    println!();
    println!("Replay complete: {} event(s).", events.len());

    Ok(())
}

fn print_replay_event(event: &crate::events::Event) {
    println!(
        "[seq={} t={}] {} (source={})",
        event.sequence,
        event.timestamp.format("%H:%M:%S%.3f"),
        event.kind_tag(),
        event.source,
    );

    // Print relevant details for common event kinds
    match &event.kind {
        EventKind::UserMessageReceived { content, connector } => {
            if let Some(text) = content.user_text() {
                println!("  connector={connector} message={}", truncate(text, 80));
            }
        }
        EventKind::ResponseReady { content, .. } => {
            println!("  response={}", truncate(content, 80));
        }
        EventKind::ToolCallRequested {
            tool_name,
            arguments,
            ..
        } => {
            println!("  tool={tool_name} args={}", truncate(arguments, 60));
        }
        EventKind::ToolCallCompleted {
            result_preview,
            is_error,
            duration_ms,
            ..
        } => {
            let status = if *is_error { "ERROR" } else { "ok" };
            println!(
                "  status={status} duration={duration_ms}ms result={}",
                truncate(result_preview, 60)
            );
        }
        EventKind::IntentionEmitted {
            intention_tag,
            intention_data,
        } => {
            println!(
                "  intention={intention_tag} data={}",
                truncate(intention_data, 60)
            );
        }
        EventKind::IntentionEvaluated {
            intention_tag,
            verdict,
        } => {
            println!("  intention={intention_tag} verdict={verdict}");
        }
        EventKind::LlmCallCompleted {
            has_tool_calls,
            usage,
            ..
        } => {
            let tokens = usage
                .as_ref()
                .map(|u| format!("{}in/{}out", u.input_tokens, u.output_tokens))
                .unwrap_or_else(|| "n/a".into());
            println!("  tool_calls={has_tool_calls} tokens={tokens}");
        }
        _ => {}
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let end = s.floor_char_boundary(max);
        &s[..end]
    }
}
