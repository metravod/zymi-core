//! Observation masking for context window management (ADR-0016 §3).
//!
//! [`mask_observations`] replaces older tool results with compact placeholders,
//! keeping the last `observation_window` turns in full.  This is the primary
//! cost-reduction mechanism — ~2× savings with no quality loss (JetBrains
//! research, December 2025; arXiv 2508.21433).

use std::collections::HashMap;

use crate::types::Message;

/// Default observation window: number of recent turns kept in full.
/// Empirical optimum from the JetBrains study.
pub const DEFAULT_OBSERVATION_WINDOW: usize = 10;

/// Apply observation masking to a message list.
///
/// - **Prefix** (leading `System` / `User` / `UserMultimodal` messages) is
///   never masked.
/// - The last `observation_window` **turns** are kept in full.
/// - Older turns have their `ToolResult` content replaced with a compact
///   placeholder preserving tool name, primary argument, status, and
///   original size.  The `Assistant` message's `content` is truncated to
///   200 chars.
/// - **Invariant:** `tool_use` / `tool_result` pairs are never split.  If
///   an `Assistant` message with `tool_calls` is present, all its
///   `ToolResult` messages are present (masked or full).
pub fn mask_observations(messages: &[Message], observation_window: usize) -> Vec<Message> {
    if messages.is_empty() {
        return Vec::new();
    }

    // 1. Find prefix boundary: first message that isn't System/User.
    let prefix_end = messages
        .iter()
        .position(|m| !is_prefix_message(m))
        .unwrap_or(messages.len());

    let body = &messages[prefix_end..];
    if body.is_empty() {
        return messages.to_vec();
    }

    // 2. Group body into segments.
    let segments = identify_segments(body);

    // 3. Count actual turns and determine how many to mask.
    let turn_count = segments
        .iter()
        .filter(|s| matches!(s, Segment::Turn { .. }))
        .count();
    let mask_count = turn_count.saturating_sub(observation_window);

    if mask_count == 0 {
        return messages.to_vec();
    }

    // 4. Build output.
    let mut result: Vec<Message> = Vec::with_capacity(messages.len());
    result.extend_from_slice(&messages[..prefix_end]);

    let mut turns_seen = 0;
    for segment in &segments {
        match segment {
            Segment::Turn { start, end } => {
                let turn_msgs = &body[*start..*end];
                if turns_seen < mask_count {
                    result.extend(mask_turn(turn_msgs));
                } else {
                    result.extend(turn_msgs.iter().cloned());
                }
                turns_seen += 1;
            }
            Segment::PassThrough(idx) => {
                result.push(body[*idx].clone());
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

const CONTENT_PREVIEW_CHARS: usize = 200;
const ARGS_PREVIEW_CHARS: usize = 60;

fn is_prefix_message(m: &Message) -> bool {
    matches!(
        m,
        Message::System(_) | Message::User(_) | Message::UserMultimodal { .. }
    )
}

/// A contiguous segment of the message body.
pub(crate) enum Segment {
    /// An `Assistant` message followed by zero or more `ToolResult` messages.
    Turn { start: usize, end: usize },
    /// A non-turn message (e.g. `User` in a multi-turn conversation).
    PassThrough(usize),
}

impl Segment {
    pub(crate) fn is_turn(&self) -> bool {
        matches!(self, Segment::Turn { .. })
    }
}

pub(crate) fn identify_segments(body: &[Message]) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut i = 0;

    while i < body.len() {
        if matches!(body[i], Message::Assistant { .. }) {
            let start = i;
            i += 1;
            while i < body.len() && matches!(body[i], Message::ToolResult { .. }) {
                i += 1;
            }
            segments.push(Segment::Turn { start, end: i });
        } else {
            segments.push(Segment::PassThrough(i));
            i += 1;
        }
    }

    segments
}

/// Mask a single turn: truncate assistant content, replace tool results
/// with placeholders.
fn mask_turn(turn_messages: &[Message]) -> Vec<Message> {
    debug_assert!(!turn_messages.is_empty());

    let (content, tool_calls) = match &turn_messages[0] {
        Message::Assistant {
            content,
            tool_calls,
        } => (content, tool_calls),
        _ => return turn_messages.to_vec(),
    };

    let mut result = Vec::with_capacity(turn_messages.len());

    // Masked assistant: truncate long content.
    result.push(Message::Assistant {
        content: content.as_ref().map(|c| {
            if c.len() <= CONTENT_PREVIEW_CHARS {
                c.clone()
            } else {
                let end = c.floor_char_boundary(CONTENT_PREVIEW_CHARS);
                format!("{}…", &c[..end])
            }
        }),
        tool_calls: tool_calls.clone(),
    });

    // Build lookup: tool_call_id → (name, arguments_json).
    let tc_map: HashMap<&str, (&str, &str)> = tool_calls
        .iter()
        .map(|tc| (tc.id.as_str(), (tc.name.as_str(), tc.arguments.as_str())))
        .collect();

    // Replace each ToolResult with a placeholder.
    for msg in &turn_messages[1..] {
        if let Message::ToolResult {
            tool_call_id,
            content,
        } = msg
        {
            let (tool_name, args_json) = tc_map
                .get(tool_call_id.as_str())
                .copied()
                .unwrap_or(("unknown", "{}"));

            let args_preview = extract_args_preview(tool_name, args_json);
            let status = if is_tool_error(content) {
                "error"
            } else {
                "ok"
            };
            let original_chars = content.len();

            let placeholder = if args_preview.is_empty() {
                format!("[masked: {tool_name} → {status}, {original_chars} chars]")
            } else {
                format!(
                    "[masked: {tool_name}(\"{args_preview}\") → {status}, {original_chars} chars]"
                )
            };

            result.push(Message::ToolResult {
                tool_call_id: tool_call_id.clone(),
                content: placeholder,
            });
        } else {
            result.push(msg.clone());
        }
    }

    result
}

fn is_tool_error(content: &str) -> bool {
    content.starts_with("[tool error]")
        || content.starts_with("[denied")
        || content.starts_with("[rejected")
        || content.starts_with("[requires approval")
}

/// Extract the most identifying argument for the placeholder.
///
/// Per ADR-0016 §3, the primary argument is tool-specific:
/// - `execute_shell_command` → `command`
/// - `read_file` / `write_file` → `path`
/// - `write_memory` / `read_memory` → `key`
/// - `web_search` → `query`
/// - `web_scrape` → `url`
/// - fallback → first string-valued argument
fn extract_args_preview(tool_name: &str, arguments_json: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(arguments_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    let primary_key = match tool_name {
        "execute_shell_command" => "command",
        "read_file" | "write_file" => "path",
        "write_memory" | "read_memory" => "key",
        "web_search" => "query",
        "web_scrape" => "url",
        _ => {
            // Fallback: first string-valued argument.
            if let Some(obj) = parsed.as_object() {
                for (_, v) in obj {
                    if let Some(s) = v.as_str() {
                        return truncate_with_ellipsis(s, ARGS_PREVIEW_CHARS);
                    }
                }
            }
            return String::new();
        }
    };

    parsed
        .get(primary_key)
        .and_then(|v| v.as_str())
        .map(|s| truncate_with_ellipsis(s, ARGS_PREVIEW_CHARS))
        .unwrap_or_default()
}

fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max.saturating_sub(1));
        format!("{}…", &s[..end])
    }
}

/// Total character count across all messages (same metric as
/// `LlmCallStarted.approx_context_chars`).
pub fn approx_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| match m {
            Message::System(s) | Message::User(s) => s.len(),
            Message::Assistant { content, .. } => content.as_ref().map_or(0, |c| c.len()),
            Message::ToolResult { content, .. } => content.len(),
            Message::UserMultimodal { parts } => parts
                .iter()
                .map(|p| match p {
                    crate::types::ContentPart::Text(t) => t.len(),
                    _ => 0,
                })
                .sum(),
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolCallInfo;

    fn tc(id: &str, name: &str, args: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    fn assistant(content: Option<&str>, tool_calls: Vec<ToolCallInfo>) -> Message {
        Message::Assistant {
            content: content.map(|s| s.to_string()),
            tool_calls,
        }
    }

    fn tool_result(tool_call_id: &str, content: &str) -> Message {
        Message::ToolResult {
            tool_call_id: tool_call_id.to_string(),
            content: content.to_string(),
        }
    }

    /// Build a realistic 3-turn conversation.
    fn three_turn_conversation() -> Vec<Message> {
        vec![
            Message::System("You are an agent.".to_string()),
            Message::User("Do the thing.".to_string()),
            // Turn 1: read a file
            assistant(
                Some("I'll read the file first."),
                vec![tc(
                    "tc1",
                    "read_file",
                    r#"{"path":"src/main.rs"}"#,
                )],
            ),
            tool_result("tc1", &"x".repeat(5000)),
            // Turn 2: run a shell command
            assistant(
                Some("Now I'll run tests."),
                vec![tc(
                    "tc2",
                    "execute_shell_command",
                    r#"{"command":"cargo test --lib"}"#,
                )],
            ),
            tool_result("tc2", "test result: all 42 tests passed"),
            // Turn 3: write a file
            assistant(
                Some("Let me write the fix."),
                vec![tc(
                    "tc3",
                    "write_file",
                    r#"{"path":"src/lib.rs","content":"fixed"}"#,
                )],
            ),
            tool_result("tc3", "ok"),
        ]
    }

    #[test]
    fn empty_messages() {
        let result = mask_observations(&[], 10);
        assert!(result.is_empty());
    }

    #[test]
    fn only_prefix_no_turns() {
        let msgs = vec![
            Message::System("sys".to_string()),
            Message::User("hello".to_string()),
        ];
        let result = mask_observations(&msgs, 1);
        assert_eq!(result.len(), 2);
        // Content unchanged.
        assert!(matches!(&result[0], Message::System(s) if s == "sys"));
        assert!(matches!(&result[1], Message::User(s) if s == "hello"));
    }

    #[test]
    fn window_larger_than_turns_returns_unchanged() {
        let msgs = three_turn_conversation();
        let original_chars = approx_chars(&msgs);
        let result = mask_observations(&msgs, 10); // 10 > 3 turns
        assert_eq!(result.len(), msgs.len());
        assert_eq!(approx_chars(&result), original_chars);
    }

    #[test]
    fn prefix_never_masked() {
        let msgs = three_turn_conversation();
        let result = mask_observations(&msgs, 0); // mask ALL turns
        // System and User messages are preserved verbatim.
        assert!(matches!(&result[0], Message::System(s) if s == "You are an agent."));
        assert!(matches!(&result[1], Message::User(s) if s == "Do the thing."));
    }

    #[test]
    fn recent_turns_kept_in_full() {
        let msgs = three_turn_conversation();
        let result = mask_observations(&msgs, 2); // keep last 2, mask 1

        // Turn 3 (last) should be fully preserved — check the ToolResult.
        let last_tool_result = result.last().unwrap();
        assert!(matches!(last_tool_result, Message::ToolResult { content, .. } if content == "ok"));

        // Turn 2 (second-to-last) should also be preserved.
        let turn2_result = &result[5]; // prefix(2) + masked_turn1(2) + turn2_assistant(1) → idx 5
        assert!(
            matches!(turn2_result, Message::ToolResult { content, .. } if content == "test result: all 42 tests passed")
        );
    }

    #[test]
    fn older_turns_have_placeholder_tool_results() {
        let msgs = three_turn_conversation();
        let result = mask_observations(&msgs, 2); // mask turn 1

        // Turn 1's ToolResult should be a placeholder.
        let masked_result = &result[3]; // prefix(2) + assistant(1) → idx 3
        match masked_result {
            Message::ToolResult {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id, "tc1");
                assert!(content.starts_with("[masked: read_file("));
                assert!(content.contains("src/main.rs"));
                assert!(content.contains("→ ok"));
                assert!(content.contains("5000 chars"));
            }
            _ => panic!("expected ToolResult, got {masked_result:?}"),
        }
    }

    #[test]
    fn tool_use_tool_result_pairs_never_split() {
        let msgs = three_turn_conversation();
        let result = mask_observations(&msgs, 0); // mask all turns

        // Every Assistant with tool_calls must be followed by matching ToolResults.
        let mut i = 0;
        while i < result.len() {
            if let Message::Assistant { tool_calls, .. } = &result[i] {
                if !tool_calls.is_empty() {
                    let expected_ids: Vec<&str> =
                        tool_calls.iter().map(|tc| tc.id.as_str()).collect();
                    for (j, expected_id) in expected_ids.iter().enumerate() {
                        let next = &result[i + 1 + j];
                        match next {
                            Message::ToolResult { tool_call_id, .. } => {
                                assert_eq!(tool_call_id, expected_id);
                            }
                            _ => panic!(
                                "expected ToolResult for {expected_id} at index {}, got {next:?}",
                                i + 1 + j
                            ),
                        }
                    }
                }
            }
            i += 1;
        }
    }

    #[test]
    fn masked_output_strictly_smaller() {
        let msgs = three_turn_conversation();
        let original_chars = approx_chars(&msgs);
        let result = mask_observations(&msgs, 1);
        let masked_chars = approx_chars(&result);
        assert!(
            masked_chars < original_chars,
            "masked ({masked_chars}) should be < original ({original_chars})"
        );
    }

    #[test]
    fn window_zero_masks_all_turns() {
        let msgs = three_turn_conversation();
        let result = mask_observations(&msgs, 0);

        // All three ToolResults should be placeholders.
        for msg in &result {
            if let Message::ToolResult { content, .. } = msg {
                assert!(
                    content.starts_with("[masked:"),
                    "expected placeholder, got: {content}"
                );
            }
        }
    }

    #[test]
    fn args_preview_shell_command() {
        let preview = extract_args_preview(
            "execute_shell_command",
            r#"{"command":"cargo test --lib"}"#,
        );
        assert_eq!(preview, "cargo test --lib");
    }

    #[test]
    fn args_preview_read_file() {
        let preview =
            extract_args_preview("read_file", r#"{"path":"src/handlers/run_pipeline.rs"}"#);
        assert_eq!(preview, "src/handlers/run_pipeline.rs");
    }

    #[test]
    fn args_preview_write_memory() {
        let preview = extract_args_preview(
            "write_memory",
            r#"{"key":"research_findings","value":"some data"}"#,
        );
        assert_eq!(preview, "research_findings");
    }

    #[test]
    fn args_preview_web_search() {
        let preview = extract_args_preview(
            "web_search",
            r#"{"query":"rust async trait object safety"}"#,
        );
        assert_eq!(preview, "rust async trait object safety");
    }

    #[test]
    fn args_preview_fallback_first_string() {
        let preview = extract_args_preview(
            "custom_tool",
            r#"{"count":5,"label":"hello world"}"#,
        );
        assert_eq!(preview, "hello world");
    }

    #[test]
    fn args_preview_truncates_long_arg() {
        let long_cmd = "a]".repeat(40); // 80 chars > 60 limit
        let json = format!(r#"{{"command":"{long_cmd}"}}"#);
        let preview = extract_args_preview("execute_shell_command", &json);
        assert!(preview.chars().count() <= 60);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn args_preview_invalid_json() {
        let preview = extract_args_preview("read_file", "not json");
        assert!(preview.is_empty());
    }

    #[test]
    fn error_tool_result_shows_error_status() {
        let msgs = vec![
            Message::System("sys".to_string()),
            Message::User("go".to_string()),
            assistant(
                None,
                vec![tc("e1", "execute_shell_command", r#"{"command":"rm -rf /"}"#)],
            ),
            tool_result("e1", "[denied by policy] dangerous command"),
            // Second turn — kept in full.
            assistant(
                Some("ok"),
                vec![tc("e2", "read_file", r#"{"path":"safe.txt"}"#)],
            ),
            tool_result("e2", "safe content"),
        ];

        let result = mask_observations(&msgs, 1);
        let masked = &result[3]; // first turn's ToolResult
        match masked {
            Message::ToolResult { content, .. } => {
                assert!(content.contains("→ error"), "got: {content}");
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn assistant_content_truncated_in_masked_turn() {
        let long_content = "x".repeat(500);
        let msgs = vec![
            Message::System("sys".to_string()),
            Message::User("go".to_string()),
            assistant(
                Some(&long_content),
                vec![tc("t1", "read_file", r#"{"path":"a.rs"}"#)],
            ),
            tool_result("t1", "file content"),
            assistant(
                Some("short"),
                vec![tc("t2", "read_file", r#"{"path":"b.rs"}"#)],
            ),
            tool_result("t2", "file content"),
        ];

        let result = mask_observations(&msgs, 1);
        // Turn 1 is masked — assistant content should be truncated.
        match &result[2] {
            Message::Assistant { content, .. } => {
                let c = content.as_ref().unwrap();
                assert!(c.len() <= 204); // 200 + "…" (3 bytes)
                assert!(c.ends_with('…'));
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn multi_tool_turn_all_results_masked() {
        let msgs = vec![
            Message::System("sys".to_string()),
            Message::User("go".to_string()),
            // Turn 1: two tool calls
            assistant(
                None,
                vec![
                    tc("m1", "read_file", r#"{"path":"a.rs"}"#),
                    tc("m2", "read_file", r#"{"path":"b.rs"}"#),
                ],
            ),
            tool_result("m1", &"a".repeat(3000)),
            tool_result("m2", &"b".repeat(4000)),
            // Turn 2: kept
            assistant(Some("done"), vec![]),
        ];

        let result = mask_observations(&msgs, 1);

        // Both ToolResults from turn 1 should be masked.
        assert!(matches!(&result[3], Message::ToolResult { content, .. } if content.starts_with("[masked:")));
        assert!(matches!(&result[4], Message::ToolResult { content, .. } if content.starts_with("[masked:")));
        // Verify char counts in placeholders.
        match &result[3] {
            Message::ToolResult { content, .. } => assert!(content.contains("3000 chars")),
            _ => unreachable!(),
        }
        match &result[4] {
            Message::ToolResult { content, .. } => assert!(content.contains("4000 chars")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn user_message_in_body_passed_through() {
        // Multi-turn: User message appears between turns.
        let msgs = vec![
            Message::System("sys".to_string()),
            Message::User("first task".to_string()),
            assistant(
                Some("done with first"),
                vec![tc("t1", "read_file", r#"{"path":"a.rs"}"#)],
            ),
            tool_result("t1", &"x".repeat(2000)),
            Message::User("second task".to_string()),
            assistant(
                Some("done with second"),
                vec![tc("t2", "read_file", r#"{"path":"b.rs"}"#)],
            ),
            tool_result("t2", "short"),
        ];

        let result = mask_observations(&msgs, 1);
        // The interspersed User message should be preserved.
        assert!(matches!(&result[4], Message::User(s) if s == "second task"));
    }
}
