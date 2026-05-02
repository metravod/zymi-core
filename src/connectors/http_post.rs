//! `type: http_post` — outbound HTTP sink (ADR-0021).
//!
//! Subscribes to the bus, filters on `on: [EventKind]`, renders a
//! MiniJinja body/URL template over the flat event context, and POSTs to
//! the target. Retries with a configurable backoff list.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use minijinja::Environment;
use reqwest::Method;
use serde::Deserialize;
use serde_yml::Value as YamlValue;
use tokio_util::sync::CancellationToken;

use crate::connectors::context::EventContext;
use crate::connectors::{ConnectorError, OutboundSink, PluginContext, PluginHandle};
use crate::plugin::PluginBuilder;

#[derive(Debug, Clone, Deserialize)]
struct HttpPostConfig {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    url: String,
    #[serde(default = "default_method")]
    method: String,
    /// EventKind tags to react to (e.g. `["response_ready"]`). When absent,
    /// fires on every event — almost never what you want.
    #[serde(default)]
    on: Vec<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body_template: Option<String>,
    #[serde(default)]
    retry: RetryConfig,
}

fn default_method() -> String {
    "POST".into()
}

#[derive(Debug, Clone, Deserialize)]
struct RetryConfig {
    #[serde(default = "default_retry_attempts")]
    attempts: u32,
    #[serde(default = "default_backoff")]
    backoff_secs: Vec<u64>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: default_retry_attempts(),
            backoff_secs: default_backoff(),
        }
    }
}

fn default_retry_attempts() -> u32 {
    3
}
fn default_backoff() -> Vec<u64> {
    vec![1, 5, 30]
}

impl RetryConfig {
    fn delay_for(&self, attempt: u32) -> Duration {
        if self.backoff_secs.is_empty() {
            return Duration::from_secs(1);
        }
        let idx = (attempt.saturating_sub(1) as usize).min(self.backoff_secs.len() - 1);
        Duration::from_secs(self.backoff_secs[idx])
    }
}

/// Kind tags into an `Arc<HashSet>` — comparing against Event.kind_tag() is
/// O(1) after that.
fn compile_on(list: &[String]) -> std::collections::HashSet<String> {
    list.iter()
        .map(|s| normalise_kind(s))
        .collect::<std::collections::HashSet<_>>()
}

/// Accept both `ResponseReady` and `response_ready` spellings — docs for
/// other ADRs use the PascalCase variant, while `Event::kind_tag()` emits
/// snake_case.
fn normalise_kind(s: &str) -> String {
    if s.chars().any(|c| c == '_') {
        s.to_string()
    } else {
        let mut out = String::with_capacity(s.len() + 4);
        for (i, c) in s.chars().enumerate() {
            if c.is_uppercase() && i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        }
        out
    }
}

pub struct HttpPostBuilder;

impl PluginBuilder<dyn OutboundSink> for HttpPostBuilder {
    fn type_name(&self) -> &'static str {
        "http_post"
    }

    fn build(
        &self,
        name: String,
        entry: YamlValue,
    ) -> Result<Box<dyn OutboundSink>, Box<dyn std::error::Error + Send + Sync>> {
        let cfg: HttpPostConfig = serde_yml::from_value(entry)?;

        // Compile templates eagerly so invalid syntax surfaces at load.
        let mut env = Environment::new();
        env.add_template_owned("url", cfg.url.clone())
            .map_err(|e| format!("url template: {e}"))?;
        if let Some(body) = &cfg.body_template {
            env.add_template_owned("body", body.clone())
                .map_err(|e| format!("body_template: {e}"))?;
        }
        for (k, v) in &cfg.headers {
            env.add_template_owned(format!("header:{k}"), v.clone())
                .map_err(|e| format!("header '{k}' template: {e}"))?;
        }

        let filter = compile_on(&cfg.on);

        let _ = name;
        Ok(Box::new(HttpPostSink { cfg, filter }))
    }
}

struct HttpPostSink {
    cfg: HttpPostConfig,
    filter: std::collections::HashSet<String>,
}

#[async_trait]
impl OutboundSink for HttpPostSink {
    fn type_name(&self) -> &'static str {
        "http_post"
    }

    async fn start(
        &self,
        resolved_name: String,
        ctx: PluginContext,
    ) -> Result<PluginHandle, ConnectorError> {
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task_name = resolved_name.clone();
        let cfg = self.cfg.clone();
        let filter = self.filter.clone();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| ConnectorError::Internal(format!("reqwest build: {e}")))?;

        let method = Method::from_bytes(cfg.method.to_uppercase().as_bytes())
            .map_err(|_| ConnectorError::InvalidConfig(format!(
                "invalid HTTP method '{}'",
                cfg.method
            )))?;

        let mut rx = ctx.bus.subscribe().await;
        let task_bus = Arc::clone(&ctx.bus);

        let join = tokio::spawn(async move {
            log::info!("http_post '{task_name}' started (on: {:?})", cfg.on);
            loop {
                tokio::select! {
                    _ = task_shutdown.cancelled() => break,
                    maybe_event = rx.recv() => {
                        let Some(event) = maybe_event else { break };
                        if !filter.is_empty() && !filter.contains(event.kind_tag()) {
                            continue;
                        }
                        let triggered_by = event.kind_tag().to_string();
                        let stream_id = event.stream_id.clone();
                        let correlation = event.correlation_id;
                        match dispatch(&task_name, &cfg, &client, &method, &event).await {
                            Ok(status) => {
                                publish_outcome(
                                    &task_bus,
                                    &task_name,
                                    &stream_id,
                                    correlation,
                                    crate::events::EventKind::OutboundDispatched {
                                        sink: task_name.clone(),
                                        sink_type: "http_post".into(),
                                        status,
                                        triggered_by,
                                    },
                                ).await;
                            }
                            Err(e) => {
                                log::warn!("http_post '{task_name}' dispatch failed: {e}");
                                publish_outcome(
                                    &task_bus,
                                    &task_name,
                                    &stream_id,
                                    correlation,
                                    crate::events::EventKind::OutboundFailed {
                                        sink: task_name.clone(),
                                        sink_type: "http_post".into(),
                                        reason: e,
                                        triggered_by,
                                    },
                                ).await;
                            }
                        }
                    }
                }
            }
            log::info!("http_post '{task_name}' stopped");
        });

        Ok(PluginHandle {
            name: resolved_name,
            type_name: "http_post",
            shutdown,
            join,
        })
    }
}

async fn publish_outcome(
    bus: &Arc<crate::events::bus::EventBus>,
    sink_name: &str,
    stream_id: &str,
    correlation: Option<uuid::Uuid>,
    kind: crate::events::EventKind,
) {
    let mut event = crate::events::Event::new(stream_id.to_string(), kind, sink_name.to_string());
    if let Some(c) = correlation {
        event = event.with_correlation(c);
    }
    if let Err(e) = bus.publish(event).await {
        log::warn!("http_post '{sink_name}' could not record outcome event: {e}");
    }
}

async fn dispatch(
    name: &str,
    cfg: &HttpPostConfig,
    client: &reqwest::Client,
    method: &Method,
    event: &Arc<crate::events::Event>,
) -> Result<u16, String> {
    let ctx = EventContext::from_event(event);
    let env = build_env(cfg)?;

    let url = render(&env, "url", &ctx)?;
    let body = cfg
        .body_template
        .as_deref()
        .map(|_| render(&env, "body", &ctx))
        .transpose()?;
    let mut headers: HashMap<String, String> = HashMap::new();
    for k in cfg.headers.keys() {
        let rendered = render(&env, &format!("header:{k}"), &ctx)?;
        headers.insert(k.clone(), rendered);
    }

    for attempt in 1..=cfg.retry.attempts {
        let mut req = client.request(method.clone(), &url);
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        if let Some(b) = body.as_deref() {
            req = req.body(b.to_string());
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => return Ok(resp.status().as_u16()),
            Ok(resp) => {
                let status = resp.status();
                // Capture Retry-After before we consume the body — we
                // want to honour it on 429 / 503 regardless of what the
                // body contains. Telegram additionally puts the hint
                // inside the JSON body (`parameters.retry_after`),
                // which we only parse for 429 as a secondary source.
                let retry_after_header = parse_retry_after_header(&resp);
                let body = resp.text().await.unwrap_or_default();
                let retry_after = retry_after_header.or_else(|| {
                    if status.as_u16() == 429 {
                        parse_retry_after_from_body(&body)
                    } else {
                        None
                    }
                });
                log::warn!(
                    "http_post '{name}' attempt {attempt}/{} got HTTP {status}: {}",
                    cfg.retry.attempts,
                    truncate(&body, 200)
                );
                // 429 is a rate-limit and *is* retryable, unlike other
                // 4xx. 503 is server-side, already server_error, but it
                // also publishes Retry-After — honour it.
                let should_retry = status.is_server_error() || status.as_u16() == 429;
                if !should_retry {
                    return Err(format!("HTTP {status}"));
                }
                if attempt < cfg.retry.attempts {
                    let delay = retry_after
                        .map(|secs| {
                            // Clamp the server's hint to a sane ceiling
                            // so a pathological `Retry-After: 86400`
                            // doesn't wedge the sink for a day.
                            Duration::from_secs(secs.min(300))
                        })
                        .unwrap_or_else(|| cfg.retry.delay_for(attempt));
                    tokio::time::sleep(delay).await;
                }
                continue;
            }
            Err(e) => {
                log::warn!(
                    "http_post '{name}' attempt {attempt}/{} network error: {e}",
                    cfg.retry.attempts
                );
            }
        }
        if attempt < cfg.retry.attempts {
            tokio::time::sleep(cfg.retry.delay_for(attempt)).await;
        }
    }
    Err(format!("gave up after {} attempts", cfg.retry.attempts))
}

/// Parse `Retry-After` as an integer number of seconds. The HTTP spec also
/// allows an HTTP-date form, but 429/503 from Telegram / Slack / GitHub
/// always use the delta-seconds form in practice — and a misparsed date
/// falling through to the configured backoff is the safe failure mode.
fn parse_retry_after_header(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Telegram's `getUpdates` / `sendMessage` return
/// `{ ok: false, error_code: 429, parameters: { retry_after: N } }` on
/// flood-wait. Pull `N` out for the retry hint.
fn parse_retry_after_from_body(body: &str) -> Option<u64> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    parsed
        .get("parameters")
        .and_then(|p| p.get("retry_after"))
        .and_then(|n| n.as_u64())
}

fn build_env(cfg: &HttpPostConfig) -> Result<Environment<'static>, String> {
    let mut env = Environment::new();
    env.add_template_owned("url", cfg.url.clone())
        .map_err(|e| format!("url template: {e}"))?;
    if let Some(body) = &cfg.body_template {
        env.add_template_owned("body", body.clone())
            .map_err(|e| format!("body_template: {e}"))?;
    }
    for (k, v) in &cfg.headers {
        env.add_template_owned(format!("header:{k}"), v.clone())
            .map_err(|e| format!("header '{k}' template: {e}"))?;
    }
    Ok(env)
}

fn render(env: &Environment<'static>, name: &str, ctx: &EventContext) -> Result<String, String> {
    let tmpl = env.get_template(name).map_err(|e| e.to_string())?;
    tmpl.render(minijinja::context! { event => ctx })
        .map_err(|e| e.to_string())
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
    use crate::events::{Event, EventKind};

    fn yaml(src: &str) -> YamlValue {
        serde_yml::from_str(src).unwrap()
    }

    #[test]
    fn builder_accepts_standard_config() {
        let b = HttpPostBuilder;
        let sink = b
            .build(
                "tg_reply".into(),
                yaml(
                    r#"
type: http_post
url: "https://api.telegram.org/bot{{ token }}/sendMessage"
on: [ResponseReady]
body_template: '{"chat_id":"{{ event.stream_id }}","text":"{{ event.content }}"}'
retry:
  attempts: 2
  backoff_secs: [1, 2]
"#,
                ),
            )
            .unwrap();
        assert_eq!(sink.type_name(), "http_post");
    }

    #[test]
    fn normalise_kind_converts_pascal_to_snake() {
        assert_eq!(normalise_kind("ResponseReady"), "response_ready");
        assert_eq!(normalise_kind("response_ready"), "response_ready");
        assert_eq!(normalise_kind("A"), "a");
    }

    #[test]
    fn compile_on_builds_tagset_accepting_both_cases() {
        let set = compile_on(&["ResponseReady".into(), "user_message_received".into()]);
        assert!(set.contains("response_ready"));
        assert!(set.contains("user_message_received"));
    }

    #[test]
    fn retry_delay_picks_from_list() {
        let r = RetryConfig {
            attempts: 5,
            backoff_secs: vec![1, 4, 9],
        };
        assert_eq!(r.delay_for(1), Duration::from_secs(1));
        assert_eq!(r.delay_for(2), Duration::from_secs(4));
        assert_eq!(r.delay_for(3), Duration::from_secs(9));
        assert_eq!(r.delay_for(99), Duration::from_secs(9));
    }

    #[tokio::test]
    async fn end_to_end_posts_on_response_ready() {
        use crate::events::bus::EventBus;
        use crate::events::store::SqliteEventStore;
        use axum::extract::State;
        use axum::routing::post;
        use axum::Router;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as SArc;
        use std::sync::Mutex as SMutex;

        let received: SArc<SMutex<Vec<(String, String)>>> = SArc::new(SMutex::new(Vec::new()));
        let hits = SArc::new(AtomicUsize::new(0));
        let state = (SArc::clone(&received), SArc::clone(&hits));
        let app = Router::new()
            .route(
                "/send",
                post(
                    |State(st): State<(
                        SArc<SMutex<Vec<(String, String)>>>,
                        SArc<AtomicUsize>,
                    )>,
                     headers: axum::http::HeaderMap,
                     body: String| async move {
                        st.1.fetch_add(1, Ordering::SeqCst);
                        let ct = headers
                            .get("content-type")
                            .and_then(|h| h.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        st.0.lock().unwrap().push((ct, body));
                        axum::http::StatusCode::OK
                    },
                ),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv_shutdown = tokio_util::sync::CancellationToken::new();
        let srv_shutdown_inner = srv_shutdown.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move { srv_shutdown_inner.cancelled().await })
                .await;
        });

        let dir = tempfile::tempdir().unwrap();
        let store = SArc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = SArc::new(EventBus::new(store));

        let cfg_yaml = format!(
            r#"
type: http_post
name: reply
url: "http://{addr}/send?chat={{{{ event.stream_id }}}}"
on: [ResponseReady]
method: POST
headers:
  Content-Type: "application/json"
body_template: '{{"text":"{{{{ event.content }}}}"}}'
retry:
  attempts: 1
  backoff_secs: [1]
"#
        );
        let sink = HttpPostBuilder
            .build("reply".into(), yaml(&cfg_yaml))
            .unwrap();

        let ctx = PluginContext {
            bus: SArc::clone(&bus),
            project_root: dir.path().to_path_buf(),
            cursor_store: SArc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        let handle = sink.start("reply".into(), ctx).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Publish something that should NOT trigger (different kind).
        bus.publish(crate::events::Event::new(
            "abc".into(),
            EventKind::AgentProcessingStarted {
                conversation_id: "abc".into(),
            },
            "agent".into(),
        ))
        .await
        .unwrap();

        // Now the one that should fire.
        bus.publish(crate::events::Event::new(
            "chat-42".into(),
            EventKind::ResponseReady {
                conversation_id: "chat-42".into(),
                content: "privet".into(),
            },
            "agent".into(),
        ))
        .await
        .unwrap();

        // Wait up to 2s for delivery.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if hits.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        handle.shutdown.cancel();
        let _ = handle.join.await;
        srv_shutdown.cancel();
        let _ = server.await;

        let got = received.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "expected exactly one POST, got {got:?}");
        assert!(got[0].0.starts_with("application/json"), "content-type: {}", got[0].0);
        assert_eq!(got[0].1, r#"{"text":"privet"}"#);
    }

    #[test]
    fn parses_telegram_retry_after_from_body() {
        let body = r#"{"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":7}}"#;
        assert_eq!(parse_retry_after_from_body(body), Some(7));
    }

    #[test]
    fn parse_retry_after_body_ignores_unrelated_json() {
        assert_eq!(parse_retry_after_from_body(r#"{"foo":"bar"}"#), None);
        assert_eq!(parse_retry_after_from_body("not json"), None);
    }

    #[tokio::test]
    async fn retries_429_honouring_retry_after_header() {
        use crate::events::bus::EventBus;
        use crate::events::store::SqliteEventStore;
        use axum::extract::State;
        use axum::http::HeaderMap;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::Router;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as SArc;

        let hits = SArc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/send",
                post(
                    |State(hits): State<SArc<AtomicUsize>>, _: HeaderMap, _: String| async move {
                        let n = hits.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            let mut h = HeaderMap::new();
                            h.insert("retry-after", "1".parse().unwrap());
                            (axum::http::StatusCode::TOO_MANY_REQUESTS, h, "slow down")
                                .into_response()
                        } else {
                            axum::http::StatusCode::OK.into_response()
                        }
                    },
                ),
            )
            .with_state(SArc::clone(&hits));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv_shutdown = tokio_util::sync::CancellationToken::new();
        let srv_shutdown_inner = srv_shutdown.clone();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move { srv_shutdown_inner.cancelled().await })
                .await;
        });

        let dir = tempfile::tempdir().unwrap();
        let store = SArc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = SArc::new(EventBus::new(store));
        let cfg_yaml = format!(
            r#"
type: http_post
name: r
url: "http://{addr}/send"
on: [ResponseReady]
body_template: 'x'
retry:
  attempts: 3
  backoff_secs: [30]
"#
        );
        let sink = HttpPostBuilder.build("r".into(), yaml(&cfg_yaml)).unwrap();
        let ctx = PluginContext {
            bus: SArc::clone(&bus),
            project_root: dir.path().to_path_buf(),
            cursor_store: SArc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        let handle = sink.start("r".into(), ctx).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = std::time::Instant::now();
        bus.publish(crate::events::Event::new(
            "s".into(),
            EventKind::ResponseReady {
                conversation_id: "s".into(),
                content: "x".into(),
            },
            "agent".into(),
        ))
        .await
        .unwrap();

        // Wait up to 5s. We should see two hits — the 429 and the retry.
        // If we respected backoff_secs[0]=30 we'd blow past the timeout;
        // the Retry-After header of 1s is what lets us pass.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if hits.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let elapsed = started.elapsed();
        assert_eq!(hits.load(Ordering::SeqCst), 2, "expected two hits");
        assert!(
            elapsed < Duration::from_secs(10),
            "Retry-After was ignored — elapsed={elapsed:?}"
        );

        handle.shutdown.cancel();
        let _ = handle.join.await;
        srv_shutdown.cancel();
        let _ = server.await;
    }

    #[test]
    fn render_url_and_body_from_event() {
        let cfg = HttpPostConfig {
            name: None,
            url: "https://example.test/bot/send?chat={{ event.stream_id }}".into(),
            method: "POST".into(),
            on: vec!["ResponseReady".into()],
            headers: HashMap::new(),
            body_template: Some(r#"{"text":"{{ event.content }}"}"#.into()),
            retry: RetryConfig::default(),
        };
        let env = build_env(&cfg).unwrap();
        let event = Event::new(
            "chat-42".into(),
            EventKind::ResponseReady {
                conversation_id: "chat-42".into(),
                content: "privet".into(),
            },
            "agent".into(),
        );
        let ctx = EventContext::from_event(&event);
        let url = render(&env, "url", &ctx).unwrap();
        let body = render(&env, "body", &ctx).unwrap();
        assert_eq!(url, "https://example.test/bot/send?chat=chat-42");
        assert_eq!(body, r#"{"text":"privet"}"#);
    }
}
