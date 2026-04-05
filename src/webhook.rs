use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};

use crate::approval::ApprovalHandler;

/// A pending approval request waiting for an external decision via HTTP.
struct PendingApproval {
    description: String,
    explanation: Option<String>,
    created_at: DateTime<Utc>,
    sender: oneshot::Sender<bool>,
}

/// Shared state for the webhook HTTP server.
struct WebhookState {
    pending: Mutex<HashMap<String, PendingApproval>>,
}

/// Info about a pending approval, returned by GET /approvals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalInfo {
    pub id: String,
    pub description: String,
    pub explanation: Option<String>,
    pub created_at: String,
}

/// Request body for POST /approvals/{id}.
#[derive(Debug, Deserialize)]
pub struct DecideRequest {
    pub approved: bool,
}

/// Response for POST /approvals/{id}.
#[derive(Debug, Serialize)]
struct DecideResponse {
    id: String,
    approved: bool,
}

/// HTTP webhook approval handler.
///
/// Implements [`ApprovalHandler`] by blocking on a oneshot channel until an
/// external system POSTs a decision to the HTTP endpoint.
///
/// Endpoints:
/// - `GET /health` — returns "ok"
/// - `GET /approvals` — list pending approval requests
/// - `POST /approvals/{id}` — approve or deny: `{"approved": true/false}`
///
/// Optionally POSTs a notification to `callback_url` when a new approval is needed,
/// so external systems (Slack bots, dashboards) can react immediately.
pub struct WebhookApprovalHandler {
    state: Arc<WebhookState>,
    timeout: Duration,
    callback_url: Option<String>,
    http_client: reqwest::Client,
}

impl WebhookApprovalHandler {
    /// Create a new handler with the given approval timeout.
    ///
    /// If `callback_url` is set, a POST notification will be sent to it
    /// whenever a new approval is required.
    pub fn new(timeout: Duration, callback_url: Option<String>) -> Self {
        Self {
            state: Arc::new(WebhookState {
                pending: Mutex::new(HashMap::new()),
            }),
            timeout,
            callback_url,
            http_client: reqwest::Client::new(),
        }
    }

    /// Start the HTTP server and return the handler.
    ///
    /// The server runs in a background tokio task. The returned `Arc` can be
    /// used as an `ApprovalHandler` with the orchestrator.
    pub async fn start(
        addr: SocketAddr,
        timeout: Duration,
        callback_url: Option<String>,
    ) -> Result<Arc<Self>, std::io::Error> {
        let handler = Arc::new(Self::new(timeout, callback_url));

        let app = build_router(handler.state.clone());

        let listener = tokio::net::TcpListener::bind(addr).await?;
        let actual_addr = listener.local_addr()?;
        log::info!("Webhook approval server listening on {actual_addr}");

        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                log::error!("Webhook server error: {e}");
            }
        });

        Ok(handler)
    }

}

#[async_trait]
impl ApprovalHandler for WebhookApprovalHandler {
    async fn request_approval(
        &self,
        tool_description: &str,
        explanation: Option<&str>,
    ) -> Result<bool, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        let now = Utc::now();

        let info = ApprovalInfo {
            id: id.clone(),
            description: tool_description.to_string(),
            explanation: explanation.map(|s| s.to_string()),
            created_at: now.to_rfc3339(),
        };

        // Register pending approval
        {
            let mut pending = self.state.pending.lock().await;
            pending.insert(
                id.clone(),
                PendingApproval {
                    description: tool_description.to_string(),
                    explanation: explanation.map(|s| s.to_string()),
                    created_at: now,
                    sender: tx,
                },
            );
        }

        // Fire-and-forget notification to callback URL (spawned so it doesn't eat timeout budget)
        {
            let client = self.http_client.clone();
            let callback_url = self.callback_url.clone();
            let info_clone = info;
            tokio::spawn(async move {
                if let Some(url) = callback_url {
                    let _ = client
                        .post(&url)
                        .json(&info_clone)
                        .send()
                        .await
                        .map_err(|e| log::warn!("Failed to notify callback {url}: {e}"));
                }
            });
        }

        // Block until decision or timeout
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(approved)) => Ok(approved),
            Ok(Err(_)) => {
                // Channel closed without sending — treat as denied
                self.state.pending.lock().await.remove(&id);
                Ok(false)
            }
            Err(_) => {
                // Timeout — remove pending and deny
                self.state.pending.lock().await.remove(&id);
                Ok(false)
            }
        }
    }
}

fn build_router(state: Arc<WebhookState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/approvals", get(list_approvals))
        .route("/approvals/:id", post(decide_approval))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn list_approvals(State(state): State<Arc<WebhookState>>) -> Json<Vec<ApprovalInfo>> {
    let pending = state.pending.lock().await;
    let infos: Vec<ApprovalInfo> = pending
        .iter()
        .map(|(id, p)| ApprovalInfo {
            id: id.clone(),
            description: p.description.clone(),
            explanation: p.explanation.clone(),
            created_at: p.created_at.to_rfc3339(),
        })
        .collect();
    Json(infos)
}

async fn decide_approval(
    State(state): State<Arc<WebhookState>>,
    Path(id): Path<String>,
    Json(body): Json<DecideRequest>,
) -> Result<Json<DecideResponse>, StatusCode> {
    let mut pending = state.pending.lock().await;

    match pending.remove(&id) {
        Some(approval) => {
            // Send decision; if receiver is dropped, that's fine
            let _ = approval.sender.send(body.approved);
            Ok(Json(DecideResponse {
                id,
                approved: body.approved,
            }))
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn handler_approves_via_state() {
        let handler = WebhookApprovalHandler::new(Duration::from_secs(5), None);

        // Spawn approval request in background
        let state = handler.state.clone();
        let handle = tokio::spawn(async move {
            // Wait for pending approval to appear
            loop {
                let pending = state.pending.lock().await;
                if let Some((id, _)) = pending.iter().next() {
                    let id = id.clone();
                    drop(pending);

                    // Simulate external POST: approve
                    let mut p = state.pending.lock().await;
                    if let Some(approval) = p.remove(&id) {
                        let _ = approval.sender.send(true);
                    }
                    break;
                }
                drop(pending);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let result = handler
            .request_approval("run dangerous command", Some("sudo reboot"))
            .await
            .unwrap();

        handle.await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn handler_denies_via_state() {
        let handler = WebhookApprovalHandler::new(Duration::from_secs(5), None);

        let state = handler.state.clone();
        let handle = tokio::spawn(async move {
            loop {
                let pending = state.pending.lock().await;
                if let Some((id, _)) = pending.iter().next() {
                    let id = id.clone();
                    drop(pending);

                    let mut p = state.pending.lock().await;
                    if let Some(approval) = p.remove(&id) {
                        let _ = approval.sender.send(false);
                    }
                    break;
                }
                drop(pending);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let result = handler
            .request_approval("delete everything", None)
            .await
            .unwrap();

        handle.await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn handler_timeout_denies() {
        let handler = WebhookApprovalHandler::new(Duration::from_millis(50), None);

        let result = handler
            .request_approval("will timeout", None)
            .await
            .unwrap();

        // Timeout should result in denial
        assert!(!result);

        // Pending should be cleaned up
        assert!(handler.state.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn http_approve_flow() {
        let handler = WebhookApprovalHandler::new(Duration::from_secs(5), None);
        let app = build_router(handler.state.clone());

        // Health check
        let resp = app
            .clone()
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"ok");

        // Start an approval request in background
        let state = handler.state.clone();
        let approval_task = tokio::spawn(async move {
            handler
                .request_approval("test action", None)
                .await
                .unwrap()
        });

        // Wait for it to register
        tokio::time::sleep(Duration::from_millis(50)).await;

        // List approvals
        let resp = app
            .clone()
            .oneshot(Request::builder().uri("/approvals").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let approvals: Vec<ApprovalInfo> = serde_json::from_slice(&body).unwrap();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].description, "test action");

        let approval_id = approvals[0].id.clone();

        // Approve it
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/approvals/{approval_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"approved": true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The approval request should resolve to true
        let result = approval_task.await.unwrap();
        assert!(result);

        // Pending should be empty now
        assert!(state.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn http_decide_unknown_id_returns_404() {
        let handler = WebhookApprovalHandler::new(Duration::from_secs(5), None);
        let app = build_router(handler.state.clone());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/approvals/nonexistent-id")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"approved": true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
