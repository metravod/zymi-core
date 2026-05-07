//! Small helpers shared by the HTTP connectors (`http_inbound`, `http_poll`,
//! `http_post`). Kept deliberately narrow — only the bits that are
//! byte-identical across files. The three connectors do genuinely different
//! work (outbound sink with retry/templating, inbound polling with cursor,
//! inbound webhook with auth), so this is *not* a "common HTTP layer";
//! resist the urge to grow it.
//!
//! For richer JSONPath logic that's specific to one connector (item
//! iteration, filter rules, cursor extraction), keep it co-located with that
//! connector — the shape diverges as soon as you go past primitives.

use serde_json::Value as JsonValue;
use serde_json_path::JsonPath;

use crate::connectors::ConnectorError;

/// Compile a JSONPath expression, prefixing the YAML field name onto the
/// error message so config typos point at the right line.
pub(crate) fn compile_path(path: &str, field: &str) -> Result<JsonPath, ConnectorError> {
    JsonPath::parse(path).map_err(|e| {
        ConnectorError::InvalidConfig(format!("{field}: invalid JSONPath '{path}': {e}"))
    })
}

/// Run a JSONPath query and return the first matching node rendered as a
/// string. `None` if no node matches or the matched node is JSON `null`.
pub(crate) fn first_string(path: &JsonPath, v: &JsonValue) -> Option<String> {
    path.query(v).all().first().and_then(|v| render_as_string(v))
}

/// Stringify a JSON value for use as a stream id / message content.
/// Strings round-trip verbatim; numbers/bools format via `to_string`;
/// `null` becomes `None` so the caller can treat "missing" and "null"
/// uniformly.
pub(crate) fn render_as_string(v: &JsonValue) -> Option<String> {
    match v {
        JsonValue::String(s) => Some(s.clone()),
        JsonValue::Number(n) => Some(n.to_string()),
        JsonValue::Bool(b) => Some(b.to_string()),
        JsonValue::Null => None,
        other => Some(other.to_string()),
    }
}

/// Truncate a log-message body to `n` bytes with a trailing ellipsis.
/// Slices on a byte boundary — fine for the log line, but do not use this
/// for anything user-visible without char-boundary care.
pub(crate) fn truncate_body(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
