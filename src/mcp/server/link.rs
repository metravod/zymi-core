//! Server→client side-channel for the bidirectional stdio transport
//! (ADR-0033 2b-sync).
//!
//! The base server (Slice 1/2a) is request→response: the client drives, the
//! server answers. The elicitation approval bridge inverts that for one case
//! — while a synchronous `tools/call` is parked on an event-sourced approval
//! (ADR-0022), the server must issue an `elicitation/create` *request* to the
//! client and await its response. [`McpClientLink`] is the handle that lets
//! server-side tasks do that without owning the wire: every outbound frame is
//! pushed onto an mpsc that a single writer task drains, and responses to our
//! own requests are matched back by JSON-RPC `id`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex};

use super::protocol::{ClientCaps, Notification, RpcResponse};

/// A single frame the server writes to the wire: a response to an inbound
/// request, a server-initiated notification (e.g. `notifications/tasks/created`),
/// or a server-initiated request (e.g. `elicitation/create`) correlated to a
/// client response by `id`.
pub enum Outbound {
    Response(RpcResponse),
    Notification(Notification),
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

/// Lets server-side tasks issue JSON-RPC *requests* to the connected client
/// and await the correlated response, while the main read loop keeps pumping.
/// Shared via `Arc`; cheap to clone.
pub struct McpClientLink {
    outbound: mpsc::UnboundedSender<Outbound>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
    caps: OnceLock<ClientCaps>,
}

impl McpClientLink {
    /// Construct a link plus the receiver the serve loop's writer task drains.
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<Outbound>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let link = Arc::new(Self {
            outbound: tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            caps: OnceLock::new(),
        });
        (link, rx)
    }

    /// A sender clone for the serve loop to emit responses/notifications.
    pub fn outbound(&self) -> mpsc::UnboundedSender<Outbound> {
        self.outbound.clone()
    }

    /// Record the client's advertised capabilities (once, on `initialize`).
    /// Subsequent calls are no-ops.
    pub fn set_caps(&self, caps: ClientCaps) {
        let _ = self.caps.set(caps);
    }

    /// Whether the client advertised `capabilities.elicitation`. `false`
    /// until `initialize` is seen — and `initialize` always precedes any
    /// `tools/call`, so it's known by the time an approval can fire.
    pub fn supports_elicitation(&self) -> bool {
        self.caps.get().map(|c| c.elicitation).unwrap_or(false)
    }

    /// Issue a server→client request and await its response. Returns the
    /// JSON-RPC `result` on success, or a stringified error (the client's
    /// error payload, or the link tearing down) otherwise.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = format!("zymi-srv-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        let frame = Outbound::Request {
            id: Value::String(id.clone()),
            method: method.to_string(),
            params,
        };
        if self.outbound.send(frame).is_err() {
            self.pending.lock().await.remove(&id);
            return Err("mcp client link closed".into());
        }
        match rx.await {
            Ok(res) => res,
            Err(_) => Err("mcp client link dropped before response".into()),
        }
    }

    /// Route an inbound response (to one of our server-initiated requests)
    /// to the waiting [`Self::request`] call. Returns `true` if it matched a
    /// pending request id.
    pub async fn deliver_response(&self, id: &Value, payload: Result<Value, String>) -> bool {
        let Some(id_str) = id.as_str() else {
            return false;
        };
        if let Some(tx) = self.pending.lock().await.remove(id_str) {
            let _ = tx.send(payload);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn request_resolves_on_matching_response() {
        let (link, mut rx) = McpClientLink::new();
        let link2 = Arc::clone(&link);

        // Caller issues a request; a "client" reads the frame and replies.
        let caller =
            tokio::spawn(async move { link2.request("elicitation/create", json!({})).await });

        let frame = rx.recv().await.unwrap();
        let id = match frame {
            Outbound::Request { id, method, .. } => {
                assert_eq!(method, "elicitation/create");
                id
            }
            _ => panic!("expected a server-initiated request frame"),
        };
        assert!(link.deliver_response(&id, Ok(json!({ "ok": true }))).await);

        let result = caller.await.unwrap().unwrap();
        assert_eq!(result["ok"], true);
    }

    #[tokio::test]
    async fn request_errors_when_client_returns_error() {
        let (link, mut rx) = McpClientLink::new();
        let link2 = Arc::clone(&link);
        let caller = tokio::spawn(async move { link2.request("elicitation/create", json!({})).await });

        let id = match rx.recv().await.unwrap() {
            Outbound::Request { id, .. } => id,
            _ => panic!("expected request"),
        };
        link.deliver_response(&id, Err("user bailed".into())).await;

        let err = caller.await.unwrap().unwrap_err();
        assert_eq!(err, "user bailed");
    }

    #[tokio::test]
    async fn deliver_to_unknown_id_is_false() {
        let (link, _rx) = McpClientLink::new();
        assert!(!link.deliver_response(&json!("nope"), Ok(Value::Null)).await);
    }
}
