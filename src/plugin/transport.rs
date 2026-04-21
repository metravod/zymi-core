//! JSON-RPC 2.0 transport over newline-delimited JSON streams.
//!
//! One complete JSON object per line, no embedded newlines. This differs from
//! LSP's `Content-Length` framing — both MCP (ADR-0023) and the external-
//! process plugin protocol (ADR-0021) pick this simpler line form.
//!
//! This module owns the wire format only. Subprocess lifecycle, protocol-
//! specific handshakes, and capability negotiation live in higher layers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TransportError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("transport closed")]
    Closed,
    #[error("request timed out")]
    Timeout,
    #[error("rpc error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
}

#[derive(Debug, Serialize)]
struct OutboundRequest<'a> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<&'a Value>,
}

#[derive(Debug, Serialize)]
struct OutboundNotification<'a> {
    jsonrpc: &'a str,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<&'a Value>,
}

#[derive(Debug, Deserialize)]
struct InboundRpcError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

/// Loose-shape inbound frame: response carries `id` + (`result` | `error`),
/// server-initiated message carries `method` (with or without `id`).
#[derive(Debug, Deserialize)]
struct InboundFrame {
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<InboundRpcError>,
    #[serde(default)]
    method: Option<String>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, TransportError>>>>>;

/// JSON-RPC 2.0 transport. Spawns a background reader task on construction;
/// the task is aborted on drop.
pub struct Transport {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    pending: Pending,
    next_id: AtomicU64,
    reader_task: JoinHandle<()>,
}

impl Transport {
    /// Build a transport over the given reader/writer halves. The reader is
    /// consumed by a background task that demultiplexes responses to pending
    /// requests by `id`. Server-initiated frames (notifications, sampling
    /// requests) are logged at `debug` and dropped — callers that need them
    /// will grow a subscriber hook in a later slice.
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncBufRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pending_for_task = pending.clone();
        let reader_task = tokio::spawn(async move {
            run_reader(reader, pending_for_task).await;
        });
        Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            pending,
            next_id: AtomicU64::new(1),
            reader_task,
        }
    }

    /// Send a JSON-RPC request and await the matching response, bounded by
    /// `timeout_dur`. On timeout the pending entry is cleaned up so a late
    /// response is dropped silently rather than leaking memory.
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout_dur: Duration,
    ) -> Result<Value, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = OutboundRequest {
            jsonrpc: JSONRPC_VERSION,
            id,
            method,
            params: params.as_ref(),
        };
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');

        {
            let mut w = self.writer.lock().await;
            if let Err(e) = w.write_all(&line).await {
                self.pending.lock().await.remove(&id);
                return Err(e.into());
            }
            if let Err(e) = w.flush().await {
                self.pending.lock().await.remove(&id);
                return Err(e.into());
            }
        }

        match timeout(timeout_dur, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(TransportError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(TransportError::Timeout)
            }
        }
    }

    /// Send a JSON-RPC notification (no `id`, no response expected).
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), TransportError> {
        let note = OutboundNotification {
            jsonrpc: JSONRPC_VERSION,
            method,
            params: params.as_ref(),
        };
        let mut line = serde_json::to_vec(&note)?;
        line.push(b'\n');
        let mut w = self.writer.lock().await;
        w.write_all(&line).await?;
        w.flush().await?;
        Ok(())
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

async fn run_reader<R>(mut reader: R, pending: Pending)
where
    R: AsyncBufRead + Send + Unpin,
{
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => {
                fail_all_pending(&pending, TransportError::Closed).await;
                return;
            }
            Ok(_) => {
                let trimmed = buf.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let frame: InboundFrame = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(_) => {
                        // Servers occasionally print log lines to stdout. The
                        // spec disallows it but real servers do it; skip
                        // gracefully rather than killing the transport.
                        log::debug!("plugin transport: skipping non-jsonrpc line: {trimmed}");
                        continue;
                    }
                };
                if frame.method.is_some() {
                    // Server-initiated frame (notification or request). v1
                    // has no subscriber hook; higher layers that need these
                    // will grow one in a later slice.
                    log::debug!("plugin transport: dropping server-initiated frame: {trimmed}");
                    continue;
                }
                let Some(id) = frame.id else {
                    log::debug!("plugin transport: response without id: {trimmed}");
                    continue;
                };
                let entry = pending.lock().await.remove(&id);
                if let Some(tx) = entry {
                    let result = if let Some(err) = frame.error {
                        Err(TransportError::Rpc {
                            code: err.code,
                            message: err.message,
                            data: err.data,
                        })
                    } else {
                        Ok(frame.result.unwrap_or(Value::Null))
                    };
                    let _ = tx.send(result);
                } else {
                    log::debug!("plugin transport: response for unknown id {id}");
                }
            }
            Err(e) => {
                log::debug!("plugin transport: reader io error: {e}");
                fail_all_pending(&pending, TransportError::Closed).await;
                return;
            }
        }
    }
}

async fn fail_all_pending(pending: &Pending, _err: TransportError) {
    let mut p = pending.lock().await;
    for (_, tx) in p.drain() {
        let _ = tx.send(Err(TransportError::Closed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{duplex, BufReader};

    /// Helper: build a transport plus a "fake server" handle (server's reader
    /// over what the client writes, and writer to what the client reads).
    fn make_pair() -> (
        Transport,
        BufReader<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
    ) {
        let (client_to_server_w, client_to_server_r) = duplex(4096);
        let (server_to_client_w, server_to_client_r) = duplex(4096);
        let transport = Transport::new(BufReader::new(server_to_client_r), client_to_server_w);
        (
            transport,
            BufReader::new(client_to_server_r),
            server_to_client_w,
        )
    }

    async fn read_one_request(reader: &mut BufReader<tokio::io::DuplexStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    async fn write_line(writer: &mut tokio::io::DuplexStream, value: Value) {
        let mut data = serde_json::to_vec(&value).unwrap();
        data.push(b'\n');
        writer.write_all(&data).await.unwrap();
        writer.flush().await.unwrap();
    }

    #[tokio::test]
    async fn request_response_roundtrip() {
        let (transport, mut srv_in, mut srv_out) = make_pair();

        let server = tokio::spawn(async move {
            let req = read_one_request(&mut srv_in).await;
            let id = req["id"].as_u64().unwrap();
            assert_eq!(req["jsonrpc"], "2.0");
            assert_eq!(req["method"], "tools/list");
            write_line(
                &mut srv_out,
                json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}}),
            )
            .await;
        });

        let result = transport
            .request("tools/list", None, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(result, json!({"tools": []}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rpc_error_is_propagated() {
        let (transport, mut srv_in, mut srv_out) = make_pair();

        let server = tokio::spawn(async move {
            let req = read_one_request(&mut srv_in).await;
            let id = req["id"].as_u64().unwrap();
            write_line(
                &mut srv_out,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": "method not found"}
                }),
            )
            .await;
        });

        let err = transport
            .request("bogus", None, Duration::from_secs(2))
            .await
            .unwrap_err();
        match err {
            TransportError::Rpc { code, message, .. } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "method not found");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn non_json_lines_are_skipped() {
        let (transport, mut srv_in, mut srv_out) = make_pair();

        let server = tokio::spawn(async move {
            let req = read_one_request(&mut srv_in).await;
            let id = req["id"].as_u64().unwrap();
            srv_out.write_all(b"server starting up...\n").await.unwrap();
            srv_out.write_all(b"definitely not json\n").await.unwrap();
            write_line(
                &mut srv_out,
                json!({"jsonrpc": "2.0", "id": id, "result": "ok"}),
            )
            .await;
        });

        let result = transport
            .request("ping", None, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(result, json!("ok"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn server_notifications_do_not_block_responses() {
        let (transport, mut srv_in, mut srv_out) = make_pair();

        let server = tokio::spawn(async move {
            let req = read_one_request(&mut srv_in).await;
            let id = req["id"].as_u64().unwrap();
            // Interleave a server notification before the response.
            write_line(
                &mut srv_out,
                json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"value": 1}}),
            )
            .await;
            write_line(
                &mut srv_out,
                json!({"jsonrpc": "2.0", "id": id, "result": "done"}),
            )
            .await;
        });

        let result = transport
            .request("work", None, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(result, json!("done"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn closed_stream_fails_pending_request() {
        let (client_to_server_w, _client_to_server_r) = duplex(1024);
        let (server_to_client_w, server_to_client_r) = duplex(1024);
        let transport = Transport::new(BufReader::new(server_to_client_r), client_to_server_w);
        drop(server_to_client_w); // Server EOF immediately.

        let err = transport
            .request("ping", None, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Closed), "got {err:?}");
    }

    #[tokio::test]
    async fn timeout_cleans_up_pending_entry() {
        let (transport, _srv_in, _srv_out) = make_pair();
        // Server never responds.
        let err = transport
            .request("ping", None, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::Timeout), "got {err:?}");
        assert!(transport.pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn notification_carries_no_id() {
        let (transport, mut srv_in, _srv_out) = make_pair();

        transport
            .notify("notifications/initialized", Some(json!({"foo": 1})))
            .await
            .unwrap();

        let mut line = String::new();
        srv_in.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "notifications/initialized");
        assert_eq!(v["params"]["foo"], 1);
        assert!(v.get("id").is_none(), "notification must not carry id");
    }

    #[tokio::test]
    async fn ids_are_monotonic_per_transport() {
        let (transport, mut srv_in, mut srv_out) = make_pair();

        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let req = read_one_request(&mut srv_in).await;
                let id = req["id"].as_u64().unwrap();
                write_line(
                    &mut srv_out,
                    json!({"jsonrpc": "2.0", "id": id, "result": id}),
                )
                .await;
            }
        });

        let a = transport.request("a", None, Duration::from_secs(2)).await.unwrap();
        let b = transport.request("b", None, Duration::from_secs(2)).await.unwrap();
        let c = transport.request("c", None, Duration::from_secs(2)).await.unwrap();
        assert_eq!(a, json!(1));
        assert_eq!(b, json!(2));
        assert_eq!(c, json!(3));
        server.await.unwrap();
    }
}
