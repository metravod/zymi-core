use std::path::Path;

use crate::events::store::{open_store, StoreBackend};
use crate::events::{Event, EventKind};

use super::{runtime, store_path};

pub fn exec(
    stream: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    json: bool,
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
                println!("No events found in stream '{stream_id}'.");
                return Ok(());
            }

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

            for event in filtered {
                if json {
                    print_json(event)?;
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
                println!("No events in the store.");
                return Ok(());
            }

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

            for event in filtered {
                if json {
                    print_json(event)?;
                } else {
                    print_rich(event, verbose);
                }
            }
        }
    }

    Ok(())
}

// ── ANSI helpers ──────────────────────────────────────────────────────

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";

// ── Hierarchy / indent ────────────────────────────────────────────────

fn indent(kind: &EventKind) -> &'static str {
    match kind {
        // Level 0 — pipeline / workflow boundaries, user I/O
        EventKind::PipelineRequested { .. }
        | EventKind::PipelineCompleted { .. }
        | EventKind::UserMessageReceived { .. }
        | EventKind::ScheduledTaskTriggered { .. }
        | EventKind::ResponseReady { .. }
        | EventKind::WorkflowStarted { .. }
        | EventKind::WorkflowCompleted { .. } => "",

        // Level 1 — node / agent / shell session boundaries
        EventKind::WorkflowNodeStarted { .. }
        | EventKind::WorkflowNodeCompleted { .. }
        | EventKind::AgentProcessingStarted { .. }
        | EventKind::AgentProcessingCompleted { .. }
        | EventKind::ShellSessionStarted { .. }
        | EventKind::ShellSessionClosed { .. } => "  ",

        // Level 2 — LLM / tool / intention detail
        EventKind::LlmCallStarted { .. }
        | EventKind::LlmCallCompleted { .. }
        | EventKind::ToolCallRequested { .. }
        | EventKind::ToolCallCompleted { .. }
        | EventKind::ApprovalRequested { .. }
        | EventKind::ApprovalDecided { .. }
        | EventKind::IntentionEmitted { .. }
        | EventKind::IntentionEvaluated { .. } => "    ",
    }
}

fn kind_color(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::UserMessageReceived { .. }
        | EventKind::ScheduledTaskTriggered { .. } => CYAN,

        EventKind::ResponseReady { .. } => GREEN,

        EventKind::AgentProcessingCompleted { success, .. } if !success => RED,
        EventKind::WorkflowNodeCompleted { success, .. } if !success => RED,
        EventKind::WorkflowCompleted { success, .. } if !success => RED,
        EventKind::PipelineCompleted { success, .. } if !success => RED,
        EventKind::ToolCallCompleted { is_error, .. } if *is_error => RED,

        EventKind::AgentProcessingCompleted { .. }
        | EventKind::WorkflowNodeCompleted { .. }
        | EventKind::WorkflowCompleted { .. }
        | EventKind::PipelineCompleted { .. }
        | EventKind::ToolCallCompleted { .. } => GREEN,

        EventKind::ApprovalRequested { .. }
        | EventKind::ApprovalDecided { .. }
        | EventKind::IntentionEmitted { .. }
        | EventKind::IntentionEvaluated { .. } => YELLOW,

        EventKind::ToolCallRequested { .. } => MAGENTA,

        EventKind::ShellSessionStarted { .. } => CYAN,
        EventKind::ShellSessionClosed { .. } => DIM,

        _ => "",
    }
}

// ── Formatters ────────────────────────────────────────────────────────

fn print_json(event: &Event) -> Result<(), String> {
    let j = serde_json::to_string(event).map_err(|e| format!("serialization error: {e}"))?;
    println!("{j}");
    Ok(())
}

fn print_rich(event: &Event, verbose: bool) {
    let pad = indent(&event.kind);
    let color = kind_color(&event.kind);
    let tag = event.kind_tag();
    let ts = event.timestamp.format("%H:%M:%S%.3f");

    println!(
        "{pad}{DIM}#{:<4} {ts}{RESET} {color}{BOLD}{tag}{RESET} {DIM}source={}{RESET}",
        event.sequence, event.source,
    );

    // Per-kind detail line(s)
    match &event.kind {
        EventKind::UserMessageReceived { content, connector } => {
            let text = content.user_text().unwrap_or("<multimodal>");
            println!("{pad}  {DIM}connector={connector}{RESET} {}", truncate(text, 100));
        }
        EventKind::ScheduledTaskTriggered { entry_id, task } => {
            println!("{pad}  entry={entry_id} task={}", truncate(task, 80));
        }

        EventKind::AgentProcessingStarted { conversation_id } => {
            println!("{pad}  {DIM}conversation={conversation_id}{RESET}");
        }
        EventKind::AgentProcessingCompleted {
            conversation_id,
            success,
        } => {
            let label = status_label(*success);
            println!("{pad}  {label} {DIM}conversation={conversation_id}{RESET}");
        }

        EventKind::LlmCallStarted {
            iteration,
            message_count,
            approx_context_chars,
        } => {
            println!(
                "{pad}  iter={iteration} msgs={message_count} ~{approx_context_chars} chars"
            );
        }
        EventKind::LlmCallCompleted {
            has_tool_calls,
            usage,
            content_preview,
        } => {
            let tokens = usage
                .as_ref()
                .map(|u| format!("{}in/{}out", u.input_tokens, u.output_tokens))
                .unwrap_or_else(|| "n/a".into());
            print!("{pad}  tokens={tokens} tool_calls={has_tool_calls}");
            if verbose {
                if let Some(preview) = content_preview {
                    print!(" preview={}", truncate(preview, 120));
                }
            }
            println!();
        }

        EventKind::ToolCallRequested {
            tool_name,
            arguments,
            call_id,
        } => {
            print!("{pad}  {MAGENTA}{tool_name}{RESET}");
            if verbose {
                println!(" id={call_id} args={}", truncate(arguments, 120));
            } else {
                println!(" args={}", truncate(arguments, 60));
            }
        }
        EventKind::ToolCallCompleted {
            call_id,
            result_preview,
            is_error,
            duration_ms,
        } => {
            let label = status_label(!is_error);
            print!("{pad}  {label} {DIM}{duration_ms}ms{RESET}");
            if verbose {
                println!(" id={call_id} result={}", truncate(result_preview, 120));
            } else {
                println!(" result={}", truncate(result_preview, 60));
            }
        }

        EventKind::ApprovalRequested {
            description,
            approval_id,
        } => {
            println!(
                "{pad}  {YELLOW}⏳ waiting{RESET} id={approval_id} {}",
                truncate(description, 80)
            );
        }
        EventKind::ApprovalDecided {
            approval_id,
            approved,
        } => {
            let label = if *approved {
                format!("{GREEN}approved{RESET}")
            } else {
                format!("{RED}denied{RESET}")
            };
            println!("{pad}  {label} id={approval_id}");
        }

        EventKind::IntentionEmitted {
            intention_tag,
            intention_data,
        } => {
            print!("{pad}  {YELLOW}{intention_tag}{RESET}");
            if verbose {
                println!(" data={}", truncate(intention_data, 120));
            } else {
                println!();
            }
        }
        EventKind::IntentionEvaluated {
            intention_tag,
            verdict,
        } => {
            println!("{pad}  {intention_tag} -> {verdict}");
        }

        EventKind::ResponseReady {
            content,
            ..
        } => {
            println!("{pad}  {}", truncate(content, 120));
        }

        EventKind::WorkflowStarted {
            user_message,
            node_count,
        } => {
            println!(
                "{pad}  {node_count} node(s) — {}",
                truncate(user_message, 80)
            );
        }
        EventKind::WorkflowNodeStarted {
            node_id,
            description,
        } => {
            println!("{pad}  {CYAN}{node_id}{RESET} — {}", truncate(description, 80));
        }
        EventKind::WorkflowNodeCompleted { node_id, success } => {
            let label = status_label(*success);
            println!("{pad}  {label} {node_id}");
        }
        EventKind::WorkflowCompleted { success } => {
            let label = status_label(*success);
            println!("{pad}  {label}");
        }

        EventKind::ShellSessionStarted {
            stream_id,
            pid,
            shell_path,
        } => {
            println!("{pad}  {CYAN}{shell_path}{RESET} pid={pid} {DIM}stream={stream_id}{RESET}");
        }
        EventKind::ShellSessionClosed {
            stream_id, reason, ..
        } => {
            println!("{pad}  reason={reason} {DIM}stream={stream_id}{RESET}");
        }

        EventKind::PipelineRequested {
            pipeline, inputs, ..
        } => {
            let keys: Vec<&str> = inputs.keys().map(|k| k.as_str()).collect();
            println!("{pad}  pipeline={pipeline} inputs=[{}]", keys.join(", "));
        }
        EventKind::PipelineCompleted {
            pipeline,
            success,
            final_output,
            error,
        } => {
            let label = status_label(*success);
            print!("{pad}  {label} pipeline={pipeline}");
            if let Some(err) = error {
                print!(" {RED}error={}{RESET}", truncate(err, 80));
            }
            if verbose {
                if let Some(out) = final_output {
                    print!(" output={}", truncate(out, 120));
                }
            }
            println!();
        }
    }
}

fn status_label(success: bool) -> String {
    if success {
        format!("{GREEN}ok{RESET}")
    } else {
        format!("{RED}FAIL{RESET}")
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
