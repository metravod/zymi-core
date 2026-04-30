//! `type: http_poll` — interval-polled HTTP source (ADR-0021).
//!
//! A Tokio timer loop hits `url` every `interval_secs`, extracts each item
//! via JSONPath, applies optional field filters, and publishes one
//! [`EventKind::UserMessageReceived`] per surviving item. A per-connector
//! cursor (e.g. Telegram `update_id`) is persisted to
//! `<project_root>/.zymi/connectors.db` so restarts don't re-fire already
//! seen items.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Method;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json_path::JsonPath;
use serde_yml::Value as YamlValue;
use tokio_util::sync::CancellationToken;

use crate::connectors::cursor_store::CursorStore;
use crate::connectors::{ConnectorError, InboundConnector, PluginContext, PluginHandle};
use crate::events::{Event, EventKind};
use crate::plugin::PluginBuilder;
use crate::types::Message;

#[derive(Debug, Clone, Deserialize)]
struct HttpPollConfig {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    url: String,
    #[serde(default = "default_method")]
    method: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default = "default_interval")]
    interval_secs: u64,
    extract: PollExtract,
    #[serde(default)]
    cursor: Option<CursorConfig>,
    #[serde(default)]
    filter: HashMap<String, FilterRule>,
    /// When set, each published `UserMessageReceived` is paired with a
    /// `PipelineRequested` targeting this pipeline, so a single
    /// `zymi serve <pipeline>` process picks up every inbound message.
    #[serde(default)]
    pipeline: Option<String>,
    #[serde(default = "default_pipeline_input")]
    pipeline_input: String,
}

fn default_pipeline_input() -> String {
    "message".into()
}

fn default_method() -> String {
    "GET".into()
}
fn default_interval() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize)]
struct PollExtract {
    items: String,
    stream_id: String,
    content: String,
    #[serde(default)]
    user: Option<String>,
}

/// `cursor:` block — where does the next-page token live and which field
/// of the item do we persist as "last-seen".
#[derive(Debug, Clone, Deserialize)]
struct CursorConfig {
    /// Query string parameter name to send on the next request.
    /// E.g. `offset` for Telegram, `pageToken` for Gmail.
    param: String,
    /// JSONPath on each item (not the response root!) that yields the
    /// value to advance the cursor to. For Telegram: `$.update_id` + `+1`.
    from_item: String,
    /// Apply `+1` to the numeric cursor before persisting — Telegram's
    /// `getUpdates` requires this to skip the ack'd batch.
    #[serde(default)]
    plus_one: bool,
    /// If true, cursor state survives restarts. Defaults to `true` —
    /// the whole point of this field.
    #[serde(default = "default_true")]
    persist: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum FilterRule {
    OneOf { one_of: Vec<JsonValue> },
    Equals { equals: JsonValue },
}

struct Compiled {
    items: JsonPath,
    stream_id: JsonPath,
    content: JsonPath,
    user: Option<JsonPath>,
    cursor_path: Option<JsonPath>,
    filter: Vec<(JsonPath, FilterRule)>,
}

impl Compiled {
    fn compile(cfg: &HttpPollConfig) -> Result<Self, ConnectorError> {
        let items = compile_path(&cfg.extract.items, "extract.items")?;
        let stream_id = compile_path(&cfg.extract.stream_id, "extract.stream_id")?;
        let content = compile_path(&cfg.extract.content, "extract.content")?;
        let user = cfg
            .extract
            .user
            .as_deref()
            .map(|p| compile_path(p, "extract.user"))
            .transpose()?;
        let cursor_path = cfg
            .cursor
            .as_ref()
            .map(|c| compile_path(&c.from_item, "cursor.from_item"))
            .transpose()?;

        let mut filter = Vec::with_capacity(cfg.filter.len());
        for (path_str, rule) in &cfg.filter {
            let path = compile_path(path_str, "filter")?;
            filter.push((path, rule.clone()));
        }

        Ok(Self {
            items,
            stream_id,
            content,
            user,
            cursor_path,
            filter,
        })
    }
}

fn compile_path(path: &str, field: &str) -> Result<JsonPath, ConnectorError> {
    JsonPath::parse(path).map_err(|e| {
        ConnectorError::InvalidConfig(format!("{field}: invalid JSONPath '{path}': {e}"))
    })
}

pub struct HttpPollBuilder;

impl PluginBuilder<dyn InboundConnector> for HttpPollBuilder {
    fn type_name(&self) -> &'static str {
        "http_poll"
    }

    fn build(
        &self,
        name: String,
        entry: YamlValue,
    ) -> Result<Box<dyn InboundConnector>, Box<dyn std::error::Error + Send + Sync>> {
        let cfg: HttpPollConfig = serde_yml::from_value(entry)?;
        let compiled = Compiled::compile(&cfg)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        let _ = name;
        Ok(Box::new(HttpPollConnector {
            cfg,
            compiled: Arc::new(compiled),
        }))
    }
}

struct HttpPollConnector {
    cfg: HttpPollConfig,
    compiled: Arc<Compiled>,
}

#[async_trait]
impl InboundConnector for HttpPollConnector {
    fn type_name(&self) -> &'static str {
        "http_poll"
    }

    async fn start(
        &self,
        resolved_name: String,
        ctx: PluginContext,
    ) -> Result<PluginHandle, ConnectorError> {
        let cursor_store = match &self.cfg.cursor {
            Some(c) if c.persist => Some(
                CursorStore::open(&ctx.project_root)
                    .map_err(|e| ConnectorError::Internal(e.to_string()))?,
            ),
            _ => None,
        };

        let method = Method::from_bytes(self.cfg.method.to_uppercase().as_bytes())
            .map_err(|_| ConnectorError::InvalidConfig(format!(
                "invalid HTTP method '{}'",
                self.cfg.method
            )))?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.cfg.interval_secs.max(5) * 2))
            .build()
            .map_err(|e| ConnectorError::Internal(format!("reqwest build: {e}")))?;

        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task_ctx = ctx.clone();
        let task_name = resolved_name.clone();
        let cfg = self.cfg.clone();
        let compiled = Arc::clone(&self.compiled);

        let join = tokio::spawn(async move {
            run_poll_loop(
                task_name,
                cfg,
                method,
                client,
                compiled,
                task_ctx,
                cursor_store,
                task_shutdown,
            )
            .await;
        });

        Ok(PluginHandle {
            name: resolved_name,
            type_name: "http_poll",
            shutdown,
            join,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_poll_loop(
    name: String,
    cfg: HttpPollConfig,
    method: Method,
    client: reqwest::Client,
    compiled: Arc<Compiled>,
    ctx: PluginContext,
    cursor_store: Option<CursorStore>,
    shutdown: CancellationToken,
) {
    log::info!("http_poll '{name}' started (every {}s)", cfg.interval_secs);
    let mut interval = tokio::time::interval(Duration::from_secs(cfg.interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // First tick fires immediately — skip it so the first real call waits
    // one interval. Matches `http_poll`'s "check every N seconds" docs.
    interval.tick().await;

    let mut cursor: Option<String> = if let Some(store) = &cursor_store {
        match store.get(&name).await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("http_poll '{name}' cursor load failed: {e}");
                None
            }
        }
    } else {
        None
    };

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {
                match poll_once(
                    &name,
                    &cfg,
                    &method,
                    &client,
                    &compiled,
                    &ctx,
                    cursor.as_deref(),
                )
                .await
                {
                    Ok(Some(next)) => {
                        if let Some(store) = &cursor_store {
                            if let Err(e) = store.set(&name, &next).await {
                                log::warn!("http_poll '{name}' cursor save failed: {e}");
                            }
                        }
                        cursor = Some(next);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!("http_poll '{name}' tick failed: {e}");
                    }
                }
            }
        }
    }
    log::info!("http_poll '{name}' stopped");
}

async fn poll_once(
    name: &str,
    cfg: &HttpPollConfig,
    method: &Method,
    client: &reqwest::Client,
    compiled: &Compiled,
    ctx: &PluginContext,
    cursor: Option<&str>,
) -> Result<Option<String>, String> {
    let mut req = client.request(method.clone(), &cfg.url);
    for (k, v) in &cfg.headers {
        req = req.header(k, v);
    }
    if let (Some(c), Some(cur)) = (cfg.cursor.as_ref(), cursor) {
        req = req.query(&[(c.param.as_str(), cur)]);
    }

    let resp = req.send().await.map_err(|e| format!("request: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {}", truncate(&body, 200)));
    }
    let body: JsonValue = resp.json().await.map_err(|e| format!("json: {e}"))?;

    let items: Vec<&JsonValue> = compiled.items.query(&body).all();
    if items.is_empty() {
        return Ok(None);
    }

    let mut max_cursor: Option<String> = None;
    for item in &items {
        if !passes_filter(&compiled.filter, item) {
            continue;
        }
        let stream_id = match first_string(&compiled.stream_id, item) {
            Some(v) => v,
            None => {
                log::debug!(
                    "http_poll '{name}': item missing stream_id, skipping"
                );
                continue;
            }
        };
        let content = match first_string(&compiled.content, item) {
            Some(v) => v,
            None => {
                log::debug!(
                    "http_poll '{name}': item missing content, skipping"
                );
                continue;
            }
        };
        let _user = compiled
            .user
            .as_ref()
            .and_then(|p| first_string(p, item));

        let correlation = uuid::Uuid::new_v4();
        let event = Event::new(
            stream_id.clone(),
            EventKind::UserMessageReceived {
                content: Message::User(content.clone()),
                connector: name.to_string(),
            },
            name.to_string(),
        )
        .with_correlation(correlation);

        if let Err(e) = ctx.bus.publish(event).await {
            log::warn!("http_poll '{name}' publish failed: {e}");
        }

        if let Some(pipeline) = cfg.pipeline.as_deref() {
            let mut inputs = HashMap::new();
            inputs.insert(cfg.pipeline_input.clone(), content);
            let req = Event::new(
                stream_id,
                EventKind::PipelineRequested {
                    pipeline: pipeline.to_string(),
                    inputs,
                },
                name.to_string(),
            )
            .with_correlation(correlation);
            if let Err(e) = ctx.bus.publish(req).await {
                log::warn!("http_poll '{name}' pipeline publish failed: {e}");
            }
        }
    }

    // Cursor advance: always, even for filtered-out items — otherwise a
    // blocked user wedges the cursor forever (Telegram offset semantics).
    if let (Some(cpath), Some(cfg_cur)) = (compiled.cursor_path.as_ref(), cfg.cursor.as_ref()) {
        for item in &items {
            let v = cpath.query(item).all().first().copied().cloned();
            if let Some(val) = v {
                let as_str = cursor_value(&val, cfg_cur.plus_one);
                if let Some(s) = as_str {
                    match &max_cursor {
                        None => max_cursor = Some(s),
                        Some(cur) => {
                            if cursor_is_greater(&s, cur) {
                                max_cursor = Some(s);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(max_cursor)
}

fn passes_filter(filter: &[(JsonPath, FilterRule)], item: &JsonValue) -> bool {
    for (path, rule) in filter {
        let observed = path.query(item).all();
        match rule {
            FilterRule::OneOf { one_of } => {
                let ok = observed.iter().any(|v| one_of.iter().any(|w| json_eq(v, w)));
                if !ok {
                    return false;
                }
            }
            FilterRule::Equals { equals } => {
                let ok = observed.iter().any(|v| json_eq(v, equals));
                if !ok {
                    return false;
                }
            }
        }
    }
    true
}

fn json_eq(a: &JsonValue, b: &JsonValue) -> bool {
    match (a, b) {
        (JsonValue::String(l), JsonValue::String(r)) => l == r,
        (JsonValue::Number(l), JsonValue::Number(r)) => l == r,
        (JsonValue::Bool(l), JsonValue::Bool(r)) => l == r,
        (JsonValue::Null, JsonValue::Null) => true,
        // Accept cross-type equality string vs number — YAML filter lists
        // often end up as strings even when the upstream emits ints
        // (Telegram user ids). Compare via string repr.
        (l, r) => l.to_string().trim_matches('"') == r.to_string().trim_matches('"'),
    }
}

fn first_string(path: &JsonPath, v: &JsonValue) -> Option<String> {
    path.query(v).all().first().and_then(|v| render_as_string(v))
}

fn render_as_string(v: &JsonValue) -> Option<String> {
    match v {
        JsonValue::String(s) => Some(s.clone()),
        JsonValue::Number(n) => Some(n.to_string()),
        JsonValue::Bool(b) => Some(b.to_string()),
        JsonValue::Null => None,
        other => Some(other.to_string()),
    }
}

fn cursor_value(v: &JsonValue, plus_one: bool) -> Option<String> {
    match v {
        JsonValue::Number(n) => {
            if plus_one {
                n.as_i64()
                    .map(|i| (i + 1).to_string())
                    .or_else(|| n.as_u64().map(|u| (u + 1).to_string()))
            } else {
                Some(n.to_string())
            }
        }
        JsonValue::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn cursor_is_greater(a: &str, b: &str) -> bool {
    match (a.parse::<i64>(), b.parse::<i64>()) {
        (Ok(l), Ok(r)) => l > r,
        _ => a > b,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(src: &str) -> YamlValue {
        serde_yml::from_str(src).unwrap()
    }

    #[test]
    fn builder_parses_telegram_like_config() {
        let b = HttpPollBuilder;
        let plugin = b
            .build(
                "tg".into(),
                yaml(
                    r#"
type: http_poll
url: "https://example.test/bot/getUpdates"
interval_secs: 2
extract:
  items:      "$.result[*]"
  stream_id:  "$.message.chat.id"
  content:    "$.message.text"
cursor:
  param: offset
  from_item: "$.update_id"
  plus_one: true
filter:
  "$.message.from.username":
    one_of: [alice, bob]
"#,
                ),
            )
            .unwrap();
        assert_eq!(plugin.type_name(), "http_poll");
    }

    #[test]
    fn filter_one_of_accepts_listed_usernames() {
        let path = JsonPath::parse("$.from.username").unwrap();
        let rule = FilterRule::OneOf {
            one_of: vec![JsonValue::String("alice".into())],
        };
        let item = serde_json::json!({"from": {"username": "alice"}});
        assert!(passes_filter(&[(path.clone(), rule.clone())], &item));

        let item = serde_json::json!({"from": {"username": "mallory"}});
        assert!(!passes_filter(&[(path, rule)], &item));
    }

    #[test]
    fn cursor_plus_one_increments_integers() {
        assert_eq!(
            cursor_value(&JsonValue::Number(42.into()), true).as_deref(),
            Some("43")
        );
        assert_eq!(
            cursor_value(&JsonValue::Number(42.into()), false).as_deref(),
            Some("42")
        );
    }

    #[test]
    fn cursor_is_greater_handles_numeric_strings() {
        assert!(cursor_is_greater("11", "9"));
        assert!(!cursor_is_greater("9", "11"));
    }

    #[tokio::test]
    async fn end_to_end_polls_publishes_user_message_and_pipeline_request() {
        use crate::events::bus::EventBus;
        use crate::events::store::SqliteEventStore;
        use axum::routing::get;
        use axum::Router;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as SArc;

        // Mock `/getUpdates` returning a Telegram-shaped payload once, then
        // an empty result on subsequent hits (so cursor advances, no loop).
        let hits = SArc::new(AtomicUsize::new(0));
        let hits_for_route = SArc::clone(&hits);
        let app = Router::new().route(
            "/getUpdates",
            get(move || {
                let hits = SArc::clone(&hits_for_route);
                async move {
                    let n = hits.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        axum::Json(serde_json::json!({
                            "ok": true,
                            "result": [
                                {
                                    "update_id": 100,
                                    "message": {
                                        "chat": {"id": 42},
                                        "from": {"username": "alice"},
                                        "text": "hello bot"
                                    }
                                },
                                {
                                    "update_id": 101,
                                    "message": {
                                        "chat": {"id": 42},
                                        "from": {"username": "mallory"},
                                        "text": "not allowed"
                                    }
                                }
                            ]
                        }))
                    } else {
                        axum::Json(serde_json::json!({"ok": true, "result": []}))
                    }
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let shutdown_srv = shutdown.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown_srv.cancelled().await })
                .await;
        });

        let dir = tempfile::tempdir().unwrap();
        let store = SArc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = SArc::new(EventBus::new(store));
        let mut rx = bus.subscribe().await;

        let yaml_cfg = format!(
            r#"
type: http_poll
url: "http://{addr}/getUpdates"
interval_secs: 1
extract:
  items: "$.result[*]"
  stream_id: "$.message.chat.id"
  content: "$.message.text"
  user: "$.message.from.username"
cursor:
  param: offset
  from_item: "$.update_id"
  plus_one: true
  persist: true
filter:
  "$.message.from.username":
    one_of: [alice]
pipeline: chat
pipeline_input: message
"#
        );
        let connector = HttpPollBuilder
            .build("telegram".into(), yaml(&yaml_cfg))
            .unwrap();

        let ctx = PluginContext {
            bus: SArc::clone(&bus),
            project_root: dir.path().to_path_buf(),
        };
        let handle = connector.start("telegram".into(), ctx).await.unwrap();

        // Collect events for up to 3s, or until both expected kinds arrive.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut saw_user_msg = false;
        let mut saw_pipeline_req = false;
        let mut saw_mallory = false;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Some(ev)) => match &ev.kind {
                    EventKind::UserMessageReceived { content, .. } => {
                        if let Message::User(t) = content {
                            if t == "hello bot" {
                                saw_user_msg = true;
                            }
                            if t == "not allowed" {
                                saw_mallory = true;
                            }
                        }
                    }
                    EventKind::PipelineRequested {
                        pipeline, inputs, ..
                    } => {
                        if pipeline == "chat"
                            && inputs.get("message").map(|s| s.as_str()) == Some("hello bot")
                        {
                            saw_pipeline_req = true;
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
            if saw_user_msg && saw_pipeline_req {
                break;
            }
        }

        handle.shutdown.cancel();
        let _ = handle.join.await;
        shutdown.cancel();
        let _ = server.await;

        assert!(saw_user_msg, "expected UserMessageReceived for 'hello bot'");
        assert!(saw_pipeline_req, "expected PipelineRequested for chat");
        assert!(!saw_mallory, "filter should drop mallory");
    }
}
