//! Shared event formatting used by the `events` CLI dump and the `observe` TUI.
//!
//! Both views describe the same event kinds, so the mapping
//! `EventKind → (icon, label, short_detail, full_detail, color)` lives here.
//! The `events` CLI composes this with ANSI colour + indentation; the TUI
//! composes it with ratatui spans + panel layout.

use crate::events::{Event, EventKind};

// ── ANSI colour constants ─────────────────────────────────────────────

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";
pub const MAGENTA: &str = "\x1b[35m";

/// Semantic colour category. CLI maps to ANSI, TUI maps to [`ratatui::style::Color`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventColor {
    Default,
    Success,
    Failure,
    Warning,
    Info,
    Highlight,
    Dim,
}

impl EventColor {
    pub fn ansi(self) -> &'static str {
        match self {
            EventColor::Default => "",
            EventColor::Success => GREEN,
            EventColor::Failure => RED,
            EventColor::Warning => YELLOW,
            EventColor::Info => CYAN,
            EventColor::Highlight => MAGENTA,
            EventColor::Dim => DIM,
        }
    }
}

/// Formatted view of a single event, independent of the render backend.
pub struct FormattedEvent {
    /// Short unicode glyph (1–2 chars) for the event kind.
    pub icon: &'static str,
    /// One-line human label (e.g. `"LLM call"`, `"Tool done"`).
    pub label: String,
    /// One-line detail, truncated for list views.
    pub short_detail: String,
    /// Multi-line detail for expanded views (no truncation).
    pub full_detail: String,
    /// Semantic colour.
    pub color: EventColor,
    /// Indentation level (0 = pipeline/workflow, 1 = node/agent, 2 = llm/tool).
    pub indent: u8,
}

/// Indentation level based on event hierarchy.
pub fn indent_level(kind: &EventKind) -> u8 {
    match kind {
        EventKind::PipelineRequested { .. }
        | EventKind::PipelineCompleted { .. }
        | EventKind::ResumeForked { .. }
        | EventKind::UserMessageReceived { .. }
        | EventKind::ScheduledTaskTriggered { .. }
        | EventKind::ResponseReady { .. }
        | EventKind::WorkflowStarted { .. }
        | EventKind::WorkflowCompleted { .. } => 0,

        EventKind::WorkflowNodeStarted { .. }
        | EventKind::WorkflowNodeCompleted { .. }
        | EventKind::AgentProcessingStarted { .. }
        | EventKind::AgentProcessingCompleted { .. }
        | EventKind::ShellSessionStarted { .. }
        | EventKind::ShellSessionClosed { .. }
        | EventKind::MemoryWritten { .. }
        | EventKind::MemoryDeleted { .. }
        | EventKind::ContextCompacted { .. } => 1,

        EventKind::LlmCallStarted { .. }
        | EventKind::LlmCallCompleted { .. }
        | EventKind::ToolCallRequested { .. }
        | EventKind::ToolCallCompleted { .. }
        | EventKind::ApprovalRequested { .. }
        | EventKind::ApprovalDecided { .. }
        | EventKind::IntentionEmitted { .. }
        | EventKind::IntentionEvaluated { .. } => 2,
    }
}

/// Format a single event for display. Pure function over event data.
pub fn format_event(event: &Event) -> FormattedEvent {
    let indent = indent_level(&event.kind);

    match &event.kind {
        EventKind::ResumeForked {
            parent_stream_id,
            parent_correlation_id,
            fork_at_step,
        } => FormattedEvent {
            icon: "↩",
            label: "Resume".into(),
            short_detail: format!("from {} at step '{fork_at_step}'", truncate(parent_stream_id, 40)),
            full_detail: format!(
                "parent: {parent_stream_id}\nparent_corr: {parent_correlation_id}\nfork_at: {fork_at_step}"
            ),
            color: EventColor::Highlight,
            indent,
        },

        EventKind::PipelineRequested { pipeline, inputs } => {
            let keys: Vec<&str> = inputs.keys().map(|k| k.as_str()).collect();
            let short = format!("pipeline={pipeline} inputs=[{}]", keys.join(", "));
            let mut full = format!("pipeline: {pipeline}\n");
            for (k, v) in inputs {
                full.push_str(&format!("  {k}: {v}\n"));
            }
            FormattedEvent {
                icon: "●",
                label: "Pipeline".into(),
                short_detail: short,
                full_detail: full,
                color: EventColor::Info,
                indent,
            }
        }
        EventKind::PipelineCompleted {
            pipeline,
            success,
            final_output,
            error,
        } => {
            let color = if *success { EventColor::Success } else { EventColor::Failure };
            let short = if let Some(err) = error {
                format!("pipeline={pipeline} FAIL error={}", truncate(err, 60))
            } else {
                format!("pipeline={pipeline} ok")
            };
            let mut full = format!("pipeline: {pipeline}\nsuccess: {success}\n");
            if let Some(err) = error {
                full.push_str(&format!("error: {err}\n"));
            }
            if let Some(out) = final_output {
                full.push_str(&format!("output: {out}\n"));
            }
            FormattedEvent {
                icon: if *success { "✓" } else { "✗" },
                label: "Pipeline done".into(),
                short_detail: short,
                full_detail: full,
                color,
                indent,
            }
        }

        EventKind::UserMessageReceived { content, connector } => {
            let text = content.user_text().unwrap_or("<multimodal>");
            FormattedEvent {
                icon: "@",
                label: "User".into(),
                short_detail: format!("{connector}: {}", truncate(text, 80)),
                full_detail: format!("connector: {connector}\n{text}"),
                color: EventColor::Info,
                indent,
            }
        }
        EventKind::ScheduledTaskTriggered { entry_id, task } => FormattedEvent {
            icon: "*",
            label: "Scheduled".into(),
            short_detail: format!("{entry_id}: {}", truncate(task, 60)),
            full_detail: format!("entry: {entry_id}\ntask: {task}"),
            color: EventColor::Info,
            indent,
        },

        EventKind::AgentProcessingStarted { conversation_id } => FormattedEvent {
            icon: "▸",
            label: "Processing".into(),
            short_detail: format!("conversation={conversation_id}"),
            full_detail: format!("conversation: {conversation_id}"),
            color: EventColor::Default,
            indent,
        },
        EventKind::AgentProcessingCompleted { success, conversation_id } => FormattedEvent {
            icon: if *success { "✓" } else { "✗" },
            label: "Completed".into(),
            short_detail: format!(
                "{}conversation={conversation_id}",
                if *success { "" } else { "FAIL " }
            ),
            full_detail: format!("conversation: {conversation_id}\nsuccess: {success}"),
            color: if *success { EventColor::Success } else { EventColor::Failure },
            indent,
        },

        EventKind::LlmCallStarted {
            iteration,
            message_count,
            approx_context_chars,
        } => {
            let ctx_k = *approx_context_chars / 1024;
            FormattedEvent {
                icon: "·",
                label: "LLM call".into(),
                short_detail: format!("iter={iteration} msgs={message_count} ~{ctx_k}k"),
                full_detail: format!(
                    "iteration: {iteration}\nmessages: {message_count}\ncontext: {approx_context_chars} chars (~{ctx_k}k)"
                ),
                color: EventColor::Default,
                indent,
            }
        }
        EventKind::LlmCallCompleted {
            has_tool_calls,
            usage,
            content_preview,
            ..
        } => {
            let tokens = usage
                .as_ref()
                .map(|u| format!("{}in/{}out", u.input_tokens, u.output_tokens))
                .unwrap_or_else(|| "n/a".into());
            let tools = if *has_tool_calls { " +tools" } else { "" };
            let preview = content_preview.as_deref().unwrap_or("");
            let short = format!("tokens={tokens}{tools} {}", truncate(preview, 60));
            let mut full = format!("tokens: {tokens}\ntool_calls: {has_tool_calls}\n");
            if !preview.is_empty() {
                full.push_str(&format!("response: {preview}"));
            }
            FormattedEvent {
                icon: "✓",
                label: "LLM done".into(),
                short_detail: short,
                full_detail: full,
                color: EventColor::Success,
                indent,
            }
        }

        EventKind::ToolCallRequested {
            tool_name,
            arguments,
            call_id,
        } => FormattedEvent {
            icon: "⚙",
            label: format!("Tool: {tool_name}"),
            short_detail: truncate(arguments, 80).to_string(),
            full_detail: format!("call_id: {call_id}\ntool: {tool_name}\nargs: {arguments}"),
            color: EventColor::Highlight,
            indent,
        },
        EventKind::ToolCallCompleted {
            call_id,
            result_preview,
            is_error,
            duration_ms,
            ..
        } => FormattedEvent {
            icon: if *is_error { "✗" } else { "✓" },
            label: "Tool done".into(),
            short_detail: format!("{}ms {}", duration_ms, truncate(result_preview, 60)),
            full_detail: format!(
                "call_id: {call_id}\nduration: {duration_ms}ms\nerror: {is_error}\nresult: {result_preview}"
            ),
            color: if *is_error { EventColor::Failure } else { EventColor::Success },
            indent,
        },

        EventKind::ApprovalRequested { description, approval_id } => FormattedEvent {
            icon: "?",
            label: "Approval".into(),
            short_detail: truncate(description, 80).to_string(),
            full_detail: format!("id: {approval_id}\n{description}"),
            color: EventColor::Warning,
            indent,
        },
        EventKind::ApprovalDecided { approved, approval_id } => FormattedEvent {
            icon: if *approved { "✓" } else { "✗" },
            label: "Decision".into(),
            short_detail: if *approved { "approved".into() } else { "rejected".into() },
            full_detail: format!("id: {approval_id}\napproved: {approved}"),
            color: if *approved { EventColor::Success } else { EventColor::Failure },
            indent,
        },

        EventKind::IntentionEmitted { intention_tag, intention_data } => FormattedEvent {
            icon: "!",
            label: "Intention".into(),
            short_detail: intention_tag.clone(),
            full_detail: format!("tag: {intention_tag}\ndata: {intention_data}"),
            color: EventColor::Warning,
            indent,
        },
        EventKind::IntentionEvaluated { intention_tag, verdict } => FormattedEvent {
            icon: "=",
            label: "Contract".into(),
            short_detail: format!("{intention_tag}: {verdict}"),
            full_detail: format!("tag: {intention_tag}\nverdict: {verdict}"),
            color: EventColor::Warning,
            indent,
        },

        EventKind::ResponseReady { conversation_id, content } => FormattedEvent {
            icon: "»",
            label: "Response".into(),
            short_detail: truncate(content, 80).to_string(),
            full_detail: format!("conversation: {conversation_id}\n{content}"),
            color: EventColor::Success,
            indent,
        },

        EventKind::WorkflowStarted { node_count, user_message } => FormattedEvent {
            icon: "◆",
            label: "Workflow".into(),
            short_detail: format!("{node_count} node(s) — {}", truncate(user_message, 60)),
            full_detail: format!("nodes: {node_count}\nmessage: {user_message}"),
            color: EventColor::Info,
            indent,
        },
        EventKind::WorkflowNodeStarted { node_id, description } => FormattedEvent {
            icon: "▸",
            label: format!("WF:{node_id}"),
            short_detail: truncate(description, 80).to_string(),
            full_detail: format!("node: {node_id}\n{description}"),
            color: EventColor::Info,
            indent,
        },
        EventKind::WorkflowNodeCompleted { node_id, success } => FormattedEvent {
            icon: if *success { "✓" } else { "✗" },
            label: format!("WF:{node_id}"),
            short_detail: if *success { "ok".into() } else { "FAIL".into() },
            full_detail: format!("node: {node_id}\nsuccess: {success}"),
            color: if *success { EventColor::Success } else { EventColor::Failure },
            indent,
        },
        EventKind::WorkflowCompleted { success } => FormattedEvent {
            icon: if *success { "✓" } else { "✗" },
            label: "WF done".into(),
            short_detail: if *success { "ok".into() } else { "FAIL".into() },
            full_detail: format!("success: {success}"),
            color: if *success { EventColor::Success } else { EventColor::Failure },
            indent,
        },

        EventKind::ShellSessionStarted { stream_id, pid, shell_path } => FormattedEvent {
            icon: "$",
            label: "Shell".into(),
            short_detail: format!("{shell_path} pid={pid}"),
            full_detail: format!("stream: {stream_id}\nshell: {shell_path}\npid: {pid}"),
            color: EventColor::Info,
            indent,
        },
        EventKind::ShellSessionClosed { stream_id, reason } => FormattedEvent {
            icon: "$",
            label: "Shell closed".into(),
            short_detail: format!("reason={reason}"),
            full_detail: format!("stream: {stream_id}\nreason: {reason}"),
            color: EventColor::Dim,
            indent,
        },

        EventKind::MemoryWritten { key, value, previous_value_seq } => {
            let overwrite = if previous_value_seq.is_some() { " (overwrite)" } else { "" };
            FormattedEvent {
                icon: "+",
                label: "Memory".into(),
                short_detail: format!("{key}{overwrite} = {}", truncate(value, 60)),
                full_detail: format!("key: {key}{overwrite}\nvalue: {value}"),
                color: EventColor::Info,
                indent,
            }
        }
        EventKind::MemoryDeleted { key, .. } => FormattedEvent {
            icon: "✗",
            label: "Memory del".into(),
            short_detail: key.clone(),
            full_detail: format!("key: {key}"),
            color: EventColor::Warning,
            indent,
        },

        EventKind::ContextCompacted {
            replaces_seq_range,
            summary,
            bytes_saved,
        } => {
            let (lo, hi) = replaces_seq_range;
            let sign = if *bytes_saved >= 0 { "+" } else { "" };
            FormattedEvent {
                icon: "~",
                label: "Compact".into(),
                short_detail: format!("seq={lo}..={hi} {sign}{bytes_saved} bytes"),
                full_detail: format!(
                    "replaces: {lo}..={hi}\nbytes_saved: {bytes_saved}\nsummary: {summary}"
                ),
                color: if *bytes_saved < 0 { EventColor::Warning } else { EventColor::Info },
                indent,
            }
        }
    }
}

/// UTF-8-safe truncation to `max` bytes (rounded down to char boundary).
pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let end = s.floor_char_boundary(max);
        &s[..end]
    }
}
