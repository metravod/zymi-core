//! HTTP approval channel (`type: http`) — ADR-0022.
//!
//! Replaces the v0.3 `WebhookApprovalHandler`: instead of a
//! [`crate::approval::ApprovalHandler`] trait impl with an in-memory
//! `pending: HashMap`, this module exposes [`HttpApprovalChannel`],
//! which subscribes to the bus, queries the event store for pending
//! requests on demand, and publishes
//! [`crate::events::EventKind::ApprovalGranted`] /
//! [`crate::events::EventKind::ApprovalDenied`] back onto the bus when
//! the operator decides via HTTP.
//!
//! The single source of truth for "who is waiting on what" is now the
//! event store — [`HttpApprovalChannel`] holds no per-request state.
//! That fixes the three defects ADR-0022 calls out for the old
//! handler: not restart-safe (HashMap died with the process), TUI
//! blind spot (decisions never flowed onto the bus), audit gap
//! (decisions weren't hash-chained).
//!
//! ### Routes
//!
//! - `GET /health` → `"ok"` plaintext.
//! - `GET /approvals` → JSON array of currently-pending requests for
//!   this channel: every `ApprovalRequested` whose matching
//!   `ApprovalGranted` / `ApprovalDenied` is not yet in the store.
//! - `POST /approvals/{id}` (body: `{"approved": bool, "reason"?: string}`)
//!   → publishes `ApprovalGranted` or `ApprovalDenied` on the bus.
//!   Returns 404 if no matching open request exists.
//!
//! Optional bearer-token auth gates `GET /approvals` and
//! `POST /approvals/{id}`. `GET /health` is always public.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::approval::{ApprovalChannel, ChannelHandle};
use crate::events::bus::EventBus;
use crate::events::store::EventStore;
use crate::events::{Event, EventKind};
use crate::plugin::PluginBuilder;
use serde_yml::Value as YamlValue;

/// Configuration for [`HttpApprovalChannel`].
#[derive(Clone, Debug)]
pub struct HttpApprovalChannelConfig {
    /// Channel name — must match `ApprovalRequested.channel` set by
    /// the orchestrator.
    pub name: String,
    /// Address the axum server binds to. Use `127.0.0.1:0` to let the
    /// OS pick an ephemeral port (the bound address is logged at
    /// startup).
    pub bind: SocketAddr,
    /// Optional bearer token enforced on `GET /approvals` and
    /// `POST /approvals/{id}`. `None` means open access (only
    /// appropriate for localhost binds).
    pub bearer_token: Option<String>,
    /// Optional URL the channel POSTs to whenever an
    /// `ApprovalRequested` lands for this channel. Lets external
    /// dashboards / Slack bots react immediately without polling.
    pub callback_url: Option<String>,
}

impl HttpApprovalChannelConfig {
    pub fn new(name: impl Into<String>, bind: SocketAddr) -> Self {
        Self {
            name: name.into(),
            bind,
            bearer_token: None,
            callback_url: None,
        }
    }

    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    pub fn with_callback_url(mut self, url: impl Into<String>) -> Self {
        self.callback_url = Some(url.into());
        self
    }
}

/// HTTP approval channel plugin (ADR-0022 `type: http`).
pub struct HttpApprovalChannel {
    config: HttpApprovalChannelConfig,
}

impl HttpApprovalChannel {
    pub fn new(config: HttpApprovalChannelConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ApprovalChannel for HttpApprovalChannel {
    fn name(&self) -> &str {
        &self.config.name
    }

    async fn start(&self, bus: Arc<EventBus>) -> Result<ChannelHandle, String> {
        let store = Arc::clone(bus.store());
        let state = Arc::new(HttpChannelState {
            channel_name: self.config.name.clone(),
            bus: Arc::clone(&bus),
            store,
            bearer_token: self.config.bearer_token.clone(),
        });

        let listener = tokio::net::TcpListener::bind(self.config.bind)
            .await
            .map_err(|e| format!("failed to bind {}: {e}", self.config.bind))?;
        let actual_addr = listener
            .local_addr()
            .map_err(|e| format!("failed to read bound addr: {e}"))?;
        log::info!(
            "http approval channel '{}' listening on {actual_addr}",
            self.config.name
        );

        let app = build_router(Arc::clone(&state));
        let server_join = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                log::error!("http approval server error: {e}");
            }
        });

        // Callback notifier: subscribe to the bus and POST callback_url
        // for every ApprovalRequested addressed to this channel. Only
        // spawned if a callback URL is configured.
        let callback_join = self.config.callback_url.clone().map(|url| {
            let bus = Arc::clone(&bus);
            let channel_name = self.config.name.clone();
            tokio::spawn(async move {
                let client = reqwest::Client::new();
                let mut rx = bus.subscribe().await;
                while let Some(ev) = rx.recv().await {
                    let EventKind::ApprovalRequested {
                        approval_id,
                        stream_id,
                        description,
                        explanation,
                        channel,
                    } = &ev.kind
                    else {
                        continue;
                    };
                    if channel != &channel_name {
                        continue;
                    }
                    let info = ApprovalInfo {
                        id: approval_id.clone(),
                        stream_id: stream_id.clone(),
                        description: description.clone(),
                        explanation: explanation.clone(),
                        channel: channel.clone(),
                        created_at: ev.timestamp.to_rfc3339(),
                    };
                    if let Err(e) = client.post(&url).json(&info).send().await {
                        log::warn!("http approval callback {url} failed: {e}");
                    }
                }
            })
        });

        let mut tasks = vec![server_join];
        if let Some(j) = callback_join {
            tasks.push(j);
        }
        Ok(ChannelHandle::from_tasks(self.config.name.clone(), tasks))
    }
}

/// `type: http` builder for the YAML approval-channel registry.
///
/// Config shape:
/// ```yaml
/// approvals:
///   - type: http
///     name: ops_api
///     bind: "127.0.0.1:8081"
///     bearer_token: ${env.APPROVAL_TOKEN}    # optional
///     callback_url: ${env.APPROVAL_CALLBACK} # optional
/// ```
pub struct HttpApprovalBuilder;

#[derive(Debug, Deserialize)]
struct HttpApprovalYamlConfig {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    bind: String,
    #[serde(default)]
    bearer_token: Option<String>,
    #[serde(default)]
    callback_url: Option<String>,
}

impl PluginBuilder<dyn ApprovalChannel> for HttpApprovalBuilder {
    fn type_name(&self) -> &'static str {
        "http"
    }

    fn build(
        &self,
        name: String,
        entry: YamlValue,
    ) -> Result<Box<dyn ApprovalChannel>, Box<dyn std::error::Error + Send + Sync>> {
        let cfg: HttpApprovalYamlConfig = serde_yml::from_value(entry)?;
        let bind: SocketAddr = cfg
            .bind
            .parse()
            .map_err(|e| format!("invalid bind '{}': {e}", cfg.bind))?;
        let mut config = HttpApprovalChannelConfig::new(name, bind);
        if let Some(token) = cfg.bearer_token {
            config = config.with_bearer_token(token);
        }
        if let Some(url) = cfg.callback_url {
            config = config.with_callback_url(url);
        }
        Ok(Box::new(HttpApprovalChannel::new(config)))
    }
}

/// Public DTO for `GET /approvals`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalInfo {
    pub id: String,
    pub stream_id: String,
    pub description: String,
    pub explanation: Option<String>,
    pub channel: String,
    pub created_at: String,
}

/// Body of `POST /approvals/{id}`.
#[derive(Debug, Deserialize)]
pub struct DecideRequest {
    pub approved: bool,
    #[serde(default)]
    pub reason: Option<String>,
    /// Optional free-form attribution recorded in the
    /// `ApprovalGranted.decided_by` / `ApprovalDenied.decided_by`
    /// field. Defaults to `"http"`.
    #[serde(default)]
    pub decided_by: Option<String>,
}

/// Response of `POST /approvals/{id}`.
#[derive(Debug, Serialize)]
struct DecideResponse {
    id: String,
    approved: bool,
}

struct HttpChannelState {
    channel_name: String,
    bus: Arc<EventBus>,
    store: Arc<dyn EventStore>,
    bearer_token: Option<String>,
}

impl HttpChannelState {
    fn check_auth(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        let Some(expected) = self.bearer_token.as_deref() else {
            return Ok(());
        };
        let Some(value) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        let presented = value.strip_prefix("Bearer ").unwrap_or(value);
        // Constant-time compare to keep token-length leakage bounded.
        if presented.len() == expected.len()
            && presented
                .bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
        {
            Ok(())
        } else {
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

fn build_router(state: Arc<HttpChannelState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/approvals", get(list_approvals))
        .route("/approvals/:id", post(decide_approval))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn list_approvals(
    State(state): State<Arc<HttpChannelState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ApprovalInfo>>, StatusCode> {
    state.check_auth(&headers)?;
    let pending = pending_approvals(state.as_ref())
        .await
        .map_err(|e| {
            log::error!("http approval list failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(pending))
}

async fn decide_approval(
    State(state): State<Arc<HttpChannelState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<DecideRequest>,
) -> Result<Json<DecideResponse>, StatusCode> {
    state.check_auth(&headers)?;

    // Look up the original ApprovalRequested for this id so we can
    // mirror its stream_id / correlation_id in the decision event.
    let pending = pending_approvals(state.as_ref()).await.map_err(|e| {
        log::error!("http approval lookup failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let request = pending
        .into_iter()
        .find(|p| p.id == id && p.channel == state.channel_name)
        .ok_or(StatusCode::NOT_FOUND)?;

    // Re-walk to recover the originating Event (we need correlation_id;
    // pending_approvals stripped it). Lightweight: only does a second
    // scan when the token check + body parse succeeded.
    let originating_correlation = correlation_id_for(state.as_ref(), &id)
        .await
        .map_err(|e| {
            log::error!("http approval correlation lookup failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let decided_by = body.decided_by.clone().unwrap_or_else(|| "http".into());
    let decision_kind = if body.approved {
        EventKind::ApprovalGranted {
            approval_id: id.clone(),
            stream_id: request.stream_id.clone(),
            decided_by,
            reason: body.reason,
        }
    } else {
        EventKind::ApprovalDenied {
            approval_id: id.clone(),
            stream_id: request.stream_id.clone(),
            decided_by,
            reason: body.reason,
        }
    };

    let mut event = Event::new(request.stream_id, decision_kind, "approval_channel".into());
    if let Some(corr) = originating_correlation {
        event = event.with_correlation(corr);
    }

    if let Err(e) = state.bus.publish(event).await {
        log::error!("http approval publish failed: {e}");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    Ok(Json(DecideResponse {
        id,
        approved: body.approved,
    }))
}

/// Walk the event store and collect every `ApprovalRequested` whose
/// matching `ApprovalGranted` / `ApprovalDenied` (keyed by
/// `approval_id`) is not yet present. Filtered to the channel this
/// state belongs to. The store itself is the single source of truth
/// for restart-safety (ADR-0022) — there's no in-memory pending map
/// to keep in sync.
async fn pending_approvals(
    state: &HttpChannelState,
) -> Result<Vec<ApprovalInfo>, String> {
    let mut requested: Vec<ApprovalInfo> = Vec::new();
    let mut decided: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut from = 0u64;
    let batch = 1024usize;
    loop {
        let events = state
            .store
            .read_all(from, batch)
            .await
            .map_err(|e| format!("read_all: {e}"))?;
        if events.is_empty() {
            break;
        }
        for ev in &events {
            match &ev.kind {
                EventKind::ApprovalRequested {
                    approval_id,
                    stream_id,
                    description,
                    explanation,
                    channel,
                } if channel == &state.channel_name => {
                    requested.push(ApprovalInfo {
                        id: approval_id.clone(),
                        stream_id: stream_id.clone(),
                        description: description.clone(),
                        explanation: explanation.clone(),
                        channel: channel.clone(),
                        created_at: ev.timestamp.to_rfc3339(),
                    });
                }
                EventKind::ApprovalGranted { approval_id, .. } => {
                    decided.insert(approval_id.clone());
                }
                EventKind::ApprovalDenied { approval_id, .. } => {
                    decided.insert(approval_id.clone());
                }
                _ => {}
            }
        }
        // `read_all`'s `from_global_seq` is exclusive, so advance past
        // the highest-seq event in this batch via Event.sequence works
        // only within a stream — instead use the batch-size convention:
        // ask for the next page by querying with limit and breaking
        // when fewer than `batch` come back.
        if events.len() < batch {
            break;
        }
        // Safe-ish increment: use the highest sequence we saw plus 1
        // is wrong (sequence is per-stream, not global). The store
        // maps `read_all`'s `from_global_seq` to its internal id. The
        // canonical way to paginate is to use `tail()`, which returns
        // `TailedEvent.global_seq`. Switch to that for correctness.
        let tailed = state
            .store
            .tail(from, batch)
            .await
            .map_err(|e| format!("tail: {e}"))?;
        if let Some(last) = tailed.last() {
            from = last.global_seq;
        } else {
            break;
        }
    }

    requested.retain(|a| !decided.contains(&a.id));
    Ok(requested)
}

/// Find the correlation_id of the originating `ApprovalRequested` for
/// a given approval_id so the published decision can join the same
/// correlated subscription the orchestrator is listening on.
async fn correlation_id_for(
    state: &HttpChannelState,
    approval_id: &str,
) -> Result<Option<uuid::Uuid>, String> {
    let mut from = 0u64;
    let batch = 1024usize;
    loop {
        let tailed = state
            .store
            .tail(from, batch)
            .await
            .map_err(|e| format!("tail: {e}"))?;
        if tailed.is_empty() {
            return Ok(None);
        }
        for t in &tailed {
            if let EventKind::ApprovalRequested {
                approval_id: aid, ..
            } = &t.event.kind
            {
                if aid == approval_id {
                    return Ok(t.event.correlation_id);
                }
            }
        }
        if tailed.len() < batch {
            return Ok(None);
        }
        from = tailed.last().map(|t| t.global_seq).unwrap_or(from);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::store::SqliteEventStore;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tower::ServiceExt;
    use uuid::Uuid;

    async fn setup() -> (TempDir, Arc<EventBus>) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_http_approvals.db");
        let store = Arc::new(SqliteEventStore::new(&db_path).unwrap());
        let bus = Arc::new(EventBus::new(store));
        (dir, bus)
    }

    fn approval_request_event(channel: &str, approval_id: &str, stream: &str) -> Event {
        let corr = Uuid::new_v4();
        Event::new(
            stream.to_string(),
            EventKind::ApprovalRequested {
                approval_id: approval_id.to_string(),
                stream_id: stream.to_string(),
                description: "rm -rf /tmp/cache".into(),
                explanation: Some("clears stale build cache".into()),
                channel: channel.to_string(),
            },
            "orchestrator".into(),
        )
        .with_correlation(corr)
    }

    #[tokio::test]
    async fn list_approvals_returns_only_pending_for_channel() {
        let (_dir, bus) = setup().await;
        // Two pending requests on our channel + one already granted +
        // one on a different channel that should not leak through.
        bus.publish(approval_request_event("ops", "a-1", "s1"))
            .await
            .unwrap();
        bus.publish(approval_request_event("ops", "a-2", "s2"))
            .await
            .unwrap();
        bus.publish(approval_request_event("other", "a-3", "s3"))
            .await
            .unwrap();
        bus.publish(Event::new(
            "s1".into(),
            EventKind::ApprovalGranted {
                approval_id: "a-1".into(),
                stream_id: "s1".into(),
                decided_by: "test".into(),
                reason: None,
            },
            "approval_channel".into(),
        ))
        .await
        .unwrap();

        let state = Arc::new(HttpChannelState {
            channel_name: "ops".into(),
            bus: Arc::clone(&bus),
            store: Arc::clone(bus.store()),
            bearer_token: None,
        });
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/approvals")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let approvals: Vec<ApprovalInfo> = serde_json::from_slice(&body).unwrap();
        assert_eq!(approvals.len(), 1, "got {approvals:?}");
        assert_eq!(approvals[0].id, "a-2");
        assert_eq!(approvals[0].channel, "ops");
    }

    #[tokio::test]
    async fn post_decide_publishes_granted_with_correlation() {
        let (_dir, bus) = setup().await;
        let req = approval_request_event("ops", "a-77", "stream-x");
        let original_corr = req.correlation_id.expect("test event has correlation");
        bus.publish(req).await.unwrap();

        let state = Arc::new(HttpChannelState {
            channel_name: "ops".into(),
            bus: Arc::clone(&bus),
            store: Arc::clone(bus.store()),
            bearer_token: None,
        });

        // Subscribe filtered by correlation_id BEFORE the POST so we
        // catch the published decision deterministically.
        let mut rx = bus.subscribe_correlation(original_corr).await;

        let app = build_router(Arc::clone(&state));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/approvals/a-77")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"approved": true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Fish out the granted event (skip the original request which
        // is already in the store; subscribe_correlation only fires on
        // future publishes, so the very next event should be ours).
        let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("decision event timeout")
            .expect("bus closed");
        match &ev.kind {
            EventKind::ApprovalGranted {
                approval_id,
                decided_by,
                ..
            } => {
                assert_eq!(approval_id, "a-77");
                assert_eq!(decided_by, "http");
            }
            other => panic!("expected ApprovalGranted, got {other:?}"),
        }
        assert_eq!(ev.correlation_id, Some(original_corr));
    }

    #[tokio::test]
    async fn post_decide_unknown_id_404() {
        let (_dir, bus) = setup().await;
        let state = Arc::new(HttpChannelState {
            channel_name: "ops".into(),
            bus: Arc::clone(&bus),
            store: Arc::clone(bus.store()),
            bearer_token: None,
        });
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/approvals/does-not-exist")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"approved": false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn bearer_token_gates_listing() {
        let (_dir, bus) = setup().await;
        bus.publish(approval_request_event("ops", "a-1", "s1"))
            .await
            .unwrap();

        let state = Arc::new(HttpChannelState {
            channel_name: "ops".into(),
            bus: Arc::clone(&bus),
            store: Arc::clone(bus.store()),
            bearer_token: Some("s3cret".into()),
        });
        let app = build_router(state);

        // Missing token → 401.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/approvals")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Wrong token → 401.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/approvals")
                    .header("authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Right token → 200.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/approvals")
                    .header("authorization", "Bearer s3cret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
