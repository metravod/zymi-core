//! `type: http_inbound` — generic webhook receiver (ADR-0021).
//!
//! One axum task binds a configured `host:port/path`, validates inbound auth
//! (bearer / HMAC-SHA256 / none), applies a JSONPath extractor to the body,
//! and publishes a synthesised [`EventKind::UserMessageReceived`] onto the
//! bus. Invalid bodies / failed auth return HTTP 400–401 with a structured
//! error; no event is published in those cases.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_json_path::JsonPath;
use serde_yml::Value as YamlValue;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;

use crate::connectors::{
    ConnectorError, InboundConnector, PluginContext, PluginHandle,
};
use crate::events::{Event, EventKind};
use crate::plugin::PluginBuilder;
use crate::types::Message;

/// YAML shape for `type: http_inbound`.
#[derive(Debug, Clone, Deserialize)]
struct HttpInboundConfig {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default = "default_host")]
    host: String,
    port: u16,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default)]
    auth: AuthConfig,
    extract: ExtractConfig,
    /// Optional: when set, every accepted webhook additionally publishes a
    /// [`EventKind::PipelineRequested`] targeting this pipeline, so
    /// `zymi serve <pipeline>` picks it up without extra glue. `pipeline_input`
    /// controls which YAML input key carries the extracted `content`
    /// (defaults to `"message"`).
    #[serde(default)]
    pipeline: Option<String>,
    #[serde(default = "default_pipeline_input")]
    pipeline_input: String,
}

fn default_pipeline_input() -> String {
    "message".into()
}

fn default_host() -> String {
    "127.0.0.1".into()
}

fn default_path() -> String {
    "/".into()
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AuthConfig {
    #[default]
    None,
    Bearer {
        token: String,
    },
    Hmac {
        header: String,
        secret: String,
        /// Optional prefix appended before the digest hex (e.g. `sha256=`
        /// for GitHub webhooks).
        #[serde(default)]
        prefix: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct ExtractConfig {
    stream_id: String,
    content: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    correlation_id: Option<String>,
}

struct CompiledExtract {
    stream_id: JsonPath,
    content: JsonPath,
    #[allow(dead_code)]
    user: Option<JsonPath>,
    #[allow(dead_code)]
    correlation_id: Option<JsonPath>,
}

impl CompiledExtract {
    fn compile(cfg: &ExtractConfig) -> Result<Self, ConnectorError> {
        Ok(Self {
            stream_id: compile_path(&cfg.stream_id, "extract.stream_id")?,
            content: compile_path(&cfg.content, "extract.content")?,
            user: cfg
                .user
                .as_deref()
                .map(|p| compile_path(p, "extract.user"))
                .transpose()?,
            correlation_id: cfg
                .correlation_id
                .as_deref()
                .map(|p| compile_path(p, "extract.correlation_id"))
                .transpose()?,
        })
    }
}

fn compile_path(path: &str, field: &str) -> Result<JsonPath, ConnectorError> {
    JsonPath::parse(path).map_err(|e| {
        ConnectorError::InvalidConfig(format!("{field}: invalid JSONPath '{path}': {e}"))
    })
}

pub struct HttpInboundBuilder;

impl PluginBuilder<dyn InboundConnector> for HttpInboundBuilder {
    fn type_name(&self) -> &'static str {
        "http_inbound"
    }

    fn build(
        &self,
        name: String,
        entry: YamlValue,
    ) -> Result<Box<dyn InboundConnector>, Box<dyn std::error::Error + Send + Sync>> {
        let cfg: HttpInboundConfig = serde_yml::from_value(entry)?;
        let extract = CompiledExtract::compile(&cfg.extract)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        let _ = name; // resolved name arrives via `start`; config carries its own optional name.
        Ok(Box::new(HttpInboundConnector {
            cfg,
            extract: Arc::new(extract),
        }))
    }
}

struct HttpInboundConnector {
    cfg: HttpInboundConfig,
    extract: Arc<CompiledExtract>,
}

#[async_trait]
impl InboundConnector for HttpInboundConnector {
    fn type_name(&self) -> &'static str {
        "http_inbound"
    }

    async fn start(
        &self,
        resolved_name: String,
        ctx: PluginContext,
    ) -> Result<PluginHandle, ConnectorError> {
        let addr: SocketAddr = format!("{}:{}", self.cfg.host, self.cfg.port)
            .parse()
            .map_err(|e| {
                ConnectorError::InvalidConfig(format!(
                    "invalid host/port '{}:{}': {e}",
                    self.cfg.host, self.cfg.port
                ))
            })?;
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|source| {
            ConnectorError::Bind {
                addr: addr.to_string(),
                source,
            }
        })?;
        let actual_addr = listener.local_addr().map_err(|e| ConnectorError::Bind {
            addr: addr.to_string(),
            source: e,
        })?;

        let state = Arc::new(HandlerState {
            connector_name: resolved_name.clone(),
            auth: self.cfg.auth.clone(),
            extract: Arc::clone(&self.extract),
            bus: Arc::clone(&ctx.bus),
            pipeline: self.cfg.pipeline.clone(),
            pipeline_input: self.cfg.pipeline_input.clone(),
        });

        let app = Router::new()
            .route(&self.cfg.path, post(handle_webhook))
            .with_state(state);

        let shutdown = CancellationToken::new();
        let shutdown_task = shutdown.clone();
        let name_log = resolved_name.clone();
        let path = self.cfg.path.clone();
        let join = tokio::spawn(async move {
            log::info!(
                "http_inbound '{name_log}' listening on {actual_addr}{path}",
            );
            let serve_result = axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown_task.cancelled().await })
                .await;
            if let Err(e) = serve_result {
                log::warn!("http_inbound '{name_log}' stopped with error: {e}");
            } else {
                log::info!("http_inbound '{name_log}' stopped");
            }
        });

        Ok(PluginHandle {
            name: resolved_name,
            type_name: "http_inbound",
            shutdown,
            join,
        })
    }
}

struct HandlerState {
    connector_name: String,
    auth: AuthConfig,
    extract: Arc<CompiledExtract>,
    bus: Arc<crate::events::bus::EventBus>,
    pipeline: Option<String>,
    pipeline_input: String,
}

#[derive(serde::Serialize)]
struct WebhookError {
    error: &'static str,
    detail: String,
}

async fn handle_webhook(
    State(state): State<Arc<HandlerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if let Err((code, err)) = validate_auth(&state.auth, &headers, &body) {
        return (code, axum::Json(err)).into_response();
    }

    let parsed: JsonValue = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(WebhookError {
                    error: "invalid_json",
                    detail: e.to_string(),
                }),
            )
                .into_response()
        }
    };

    let extracted = match extract_fields(&state.extract, &parsed) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(WebhookError {
                    error: "extract_failed",
                    detail: e,
                }),
            )
                .into_response()
        }
    };

    let Extracted {
        stream_id,
        content,
        correlation_id,
    } = extracted;

    let correlation = correlation_id
        .as_deref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .unwrap_or_else(uuid::Uuid::new_v4);

    let event = Event::new(
        stream_id.clone(),
        EventKind::UserMessageReceived {
            content: Message::User(content.clone()),
            connector: state.connector_name.clone(),
        },
        state.connector_name.clone(),
    )
    .with_correlation(correlation);

    if let Err(e) = state.bus.publish(event).await {
        log::warn!(
            "http_inbound '{}' failed to publish event: {e}",
            state.connector_name
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(WebhookError {
                error: "publish_failed",
                detail: e.to_string(),
            }),
        )
            .into_response();
    }

    if let Some(pipeline) = state.pipeline.as_deref() {
        let mut inputs = std::collections::HashMap::new();
        inputs.insert(state.pipeline_input.clone(), content);
        let req = Event::new(
            stream_id,
            EventKind::PipelineRequested {
                pipeline: pipeline.to_string(),
                inputs,
            },
            state.connector_name.clone(),
        )
        .with_correlation(correlation);
        if let Err(e) = state.bus.publish(req).await {
            log::warn!(
                "http_inbound '{}' failed to publish PipelineRequested: {e}",
                state.connector_name
            );
        }
    }

    StatusCode::OK.into_response()
}

#[derive(Debug)]
struct Extracted {
    stream_id: String,
    content: String,
    correlation_id: Option<String>,
}

fn extract_fields(cfg: &CompiledExtract, body: &JsonValue) -> Result<Extracted, String> {
    let stream_id = extract_string(&cfg.stream_id, body, "extract.stream_id")?;
    let content = extract_string(&cfg.content, body, "extract.content")?;
    let correlation_id = cfg
        .correlation_id
        .as_ref()
        .and_then(|p| first_string(p, body));
    // `user` is intentionally not threaded through today — it only affects
    // future projection views / per-user contracts. Keep the extractor
    // compiled so config surfaces the error if the path is malformed.
    Ok(Extracted {
        stream_id,
        content,
        correlation_id,
    })
}

fn extract_string(path: &JsonPath, body: &JsonValue, field: &str) -> Result<String, String> {
    let nodes = path.query(body).all();
    let first = nodes.first().ok_or_else(|| format!("{field}: no match"))?;
    render_as_string(first).ok_or_else(|| format!("{field}: value is null"))
}

fn first_string(path: &JsonPath, body: &JsonValue) -> Option<String> {
    path.query(body).all().first().and_then(|v| render_as_string(v))
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

fn validate_auth(
    auth: &AuthConfig,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), (StatusCode, WebhookError)> {
    match auth {
        AuthConfig::None => Ok(()),
        AuthConfig::Bearer { token } => {
            let provided = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or("");
            if constant_time_eq(provided.as_bytes(), token.as_bytes()) {
                Ok(())
            } else {
                Err((
                    StatusCode::UNAUTHORIZED,
                    WebhookError {
                        error: "unauthorized",
                        detail: "bearer token missing or invalid".into(),
                    },
                ))
            }
        }
        AuthConfig::Hmac {
            header,
            secret,
            prefix,
        } => {
            let provided = headers
                .get(header.as_str())
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let expected = sign_hmac(secret.as_bytes(), body);
            let expected_full = match prefix.as_deref() {
                Some(p) => format!("{p}{expected}"),
                None => expected,
            };
            if constant_time_eq(provided.as_bytes(), expected_full.as_bytes()) {
                Ok(())
            } else {
                Err((
                    StatusCode::UNAUTHORIZED,
                    WebhookError {
                        error: "unauthorized",
                        detail: format!("invalid HMAC signature in header '{header}'"),
                    },
                ))
            }
        }
    }
}

fn sign_hmac(secret: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(src: &str) -> YamlValue {
        serde_yml::from_str(src).unwrap()
    }

    #[test]
    fn builder_parses_minimum_config() {
        let builder = HttpInboundBuilder;
        let built = builder
            .build(
                "tg".into(),
                yaml(
                    r#"
type: http_inbound
port: 8080
path: /webhook
extract:
  stream_id: "$.chat_id"
  content: "$.text"
"#,
                ),
            )
            .expect("build ok");
        assert_eq!(built.type_name(), "http_inbound");
    }

    #[test]
    fn builder_rejects_bad_jsonpath() {
        let builder = HttpInboundBuilder;
        let result = builder.build(
            "tg".into(),
            yaml(
                r#"
type: http_inbound
port: 8080
extract:
  stream_id: "not a path"
  content: "$.text"
"#,
            ),
        );
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected build failure"),
        };
        let msg = err.to_string();
        assert!(msg.contains("stream_id"), "got {msg}");
    }

    #[test]
    fn extract_string_pulls_nested_values() {
        let cfg = ExtractConfig {
            stream_id: "$.message.chat.id".into(),
            content: "$.message.text".into(),
            user: Some("$.message.from.username".into()),
            correlation_id: None,
        };
        let compiled = CompiledExtract::compile(&cfg).unwrap();
        let body = serde_json::json!({
            "message": {
                "chat": {"id": 12345},
                "from": {"username": "alice"},
                "text": "hello bot",
            }
        });
        let ex = extract_fields(&compiled, &body).unwrap();
        assert_eq!(ex.stream_id, "12345");
        assert_eq!(ex.content, "hello bot");
    }

    #[test]
    fn extract_missing_field_is_error() {
        let cfg = ExtractConfig {
            stream_id: "$.chat_id".into(),
            content: "$.text".into(),
            user: None,
            correlation_id: None,
        };
        let compiled = CompiledExtract::compile(&cfg).unwrap();
        let body = serde_json::json!({"text": "hi"});
        let err = extract_fields(&compiled, &body).unwrap_err();
        assert!(err.contains("stream_id"), "got {err}");
    }

    #[test]
    fn hmac_signature_validates_when_matching() {
        let headers = {
            let mut h = HeaderMap::new();
            let sig = sign_hmac(b"topsecret", b"{\"a\":1}");
            h.insert("x-signature", sig.parse().unwrap());
            h
        };
        let auth = AuthConfig::Hmac {
            header: "x-signature".into(),
            secret: "topsecret".into(),
            prefix: None,
        };
        assert!(validate_auth(&auth, &headers, b"{\"a\":1}").is_ok());
    }

    #[test]
    fn hmac_rejects_wrong_signature() {
        let headers = {
            let mut h = HeaderMap::new();
            h.insert("x-signature", "deadbeef".parse().unwrap());
            h
        };
        let auth = AuthConfig::Hmac {
            header: "x-signature".into(),
            secret: "topsecret".into(),
            prefix: None,
        };
        let err = validate_auth(&auth, &headers, b"{}").unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn hmac_with_prefix_matches() {
        let headers = {
            let mut h = HeaderMap::new();
            let raw = sign_hmac(b"topsecret", b"body");
            h.insert(
                "x-signature",
                format!("sha256={raw}").parse().unwrap(),
            );
            h
        };
        let auth = AuthConfig::Hmac {
            header: "x-signature".into(),
            secret: "topsecret".into(),
            prefix: Some("sha256=".into()),
        };
        assert!(validate_auth(&auth, &headers, b"body").is_ok());
    }

    #[test]
    fn bearer_validates_exact_token() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer s3cret".parse().unwrap(),
        );
        let auth = AuthConfig::Bearer {
            token: "s3cret".into(),
        };
        assert!(validate_auth(&auth, &h, b"").is_ok());

        let mut bad = HeaderMap::new();
        bad.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer wrong".parse().unwrap(),
        );
        assert!(validate_auth(&auth, &bad, b"").is_err());
    }

    #[tokio::test]
    async fn end_to_end_post_publishes_user_message_received() {
        use crate::events::bus::EventBus;
        use crate::events::store::SqliteEventStore;
        use std::sync::Arc as SArc;

        let dir = tempfile::tempdir().unwrap();
        let store = SArc::new(SqliteEventStore::new(&dir.path().join("events.db")).unwrap());
        let bus = SArc::new(EventBus::new(store.clone()));
        let mut rx = bus.subscribe().await;

        let builder = HttpInboundBuilder;
        let connector = builder
            .build(
                "hook".into(),
                yaml(
                    r#"
type: http_inbound
port: 0
path: /
extract:
  stream_id: "$.chat_id"
  content: "$.text"
"#,
                ),
            )
            .unwrap();
        let ctx = PluginContext {
            bus: SArc::clone(&bus),
            project_root: dir.path().to_path_buf(),
            cursor_store: SArc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        let handle = connector.start("hook".into(), ctx).await.unwrap();

        // Startup raced the first request once on CI — give axum a tick.
        tokio::task::yield_now().await;

        // Re-parse config to discover the bound port. Since port: 0 was
        // used, we read it back from the OS via a side-channel: the
        // listener is long-gone. Instead, rebuild with an explicit
        // ephemeral port using std's TcpListener.
        let tcp = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = tcp.local_addr().unwrap().port();
        drop(tcp);

        // Restart the connector on a real port since the previous one
        // bound to 0 and we can't recover the port without extra plumbing.
        handle.shutdown.cancel();
        let _ = handle.join.await;

        let connector = HttpInboundBuilder
            .build(
                "hook".into(),
                yaml(&format!(
                    r#"
type: http_inbound
port: {port}
path: /
extract:
  stream_id: "$.chat_id"
  content: "$.text"
"#
                )),
            )
            .unwrap();
        let ctx = PluginContext {
            bus: SArc::clone(&bus),
            project_root: dir.path().to_path_buf(),
            cursor_store: SArc::new(
                crate::connectors::cursor_store::SqliteCursorStore::in_memory(),
            ),
        };
        let handle = connector.start("hook".into(), ctx).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/"))
            .json(&serde_json::json!({"chat_id": "42", "text": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let ev = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("event")
            .expect("some");
        match &ev.kind {
            EventKind::UserMessageReceived { content, connector } => {
                assert_eq!(connector, "hook");
                if let Message::User(t) = content {
                    assert_eq!(t, "hi");
                } else {
                    panic!("unexpected message variant");
                }
            }
            other => panic!("unexpected kind: {:?}", other.tag()),
        }
        assert_eq!(ev.stream_id, "42");

        handle.shutdown.cancel();
        let _ = handle.join.await;
    }
}
