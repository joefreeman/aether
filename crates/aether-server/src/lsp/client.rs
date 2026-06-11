//! JSON-RPC client/router over an LSP byte stream.
//!
//! [`connect`] wires a reader task and a writer task around a duplex byte stream and hands back an
//! [`LspClient`]. The client lets callers issue requests (awaiting a typed response by matching the
//! JSON-RPC `id`) and fire notifications. Anything the *server* initiates — its notifications
//! (`publishDiagnostics`) and its own requests (`workspace/applyEdit`) — is demultiplexed onto the
//! returned [`LspInbound`] channel for a higher layer to handle.
//!
//! This is the third instance of the select-loop/id-table shape in the codebase (the WS accept loop
//! in `connection.rs` and the TUI's `client.rs` are the others), pointed at a subprocess.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};

use super::transport;

/// A message from the language server that isn't a response to one of our requests.
#[derive(Debug)]
pub enum LspInbound {
    /// Server→client notification (no `id`), e.g. `textDocument/publishDiagnostics`.
    Notification { method: String, params: Value },
    /// Server→client request (has `id`) that we must answer via [`LspClient::respond`], e.g.
    /// `workspace/applyEdit`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("language server returned error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("language server connection closed")]
    Closed,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspError>>>>>;

/// Handle to a connected language server. Cheaply cloneable; all clones share one connection.
#[derive(Clone)]
pub struct LspClient {
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    pending: Pending,
    next_id: Arc<AtomicI64>,
}

impl LspClient {
    /// Issue a request and await its response. Resolves to the `result` value, or an error if the
    /// server replied with one or the connection dropped.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if self
            .outgoing
            .send(serde_json::to_vec(&msg).expect("serialize"))
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            return Err(LspError::Closed);
        }
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(LspError::Closed), // reader task dropped the sender
        }
    }

    /// Fire a notification (no response). Notifications that mutate document state
    /// (`didOpen`/`didChange`/`didClose`) must be sent in order by the caller — this method
    /// preserves submission order, so issuing them sequentially is sufficient.
    pub fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.outgoing
            .send(serde_json::to_vec(&msg).expect("serialize"))
            .map_err(|_| LspError::Closed)
    }

    /// Answer a server-initiated request (an [`LspInbound::Request`]).
    pub fn respond(&self, id: Value, result: Value) -> Result<(), LspError> {
        let msg = json!({"jsonrpc": "2.0", "id": id, "result": result});
        self.outgoing
            .send(serde_json::to_vec(&msg).expect("serialize"))
            .map_err(|_| LspError::Closed)
    }
}

/// Drive an LSP connection over the given byte streams. Spawns a writer task (drains outgoing
/// messages onto `writer`) and a reader task (frames inbound messages off `reader`, resolving
/// responses and forwarding everything else to the returned channel). Both tasks exit when the
/// connection closes; at that point every in-flight request fails with [`LspError::Closed`].
pub fn connect<R, W>(reader: R, writer: W) -> (LspClient, mpsc::UnboundedReceiver<LspInbound>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<LspInbound>();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

    // Writer task: serialize outgoing messages onto the stream in submission order.
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(bytes) = out_rx.recv().await {
            if transport::write_frame(&mut writer, &bytes).await.is_err() {
                break;
            }
        }
    });

    // Reader task: frame, parse, route.
    let pending_reader = pending.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        loop {
            match transport::read_frame(&mut reader).await {
                Ok(Some(body)) => {
                    if let Ok(msg) = serde_json::from_slice::<Value>(&body) {
                        route_inbound(msg, &pending_reader, &in_tx).await;
                    } else {
                        tracing::warn!("lsp: dropping unparseable message from server");
                    }
                }
                Ok(None) => break, // clean EOF
                Err(e) => {
                    tracing::debug!(error = %e, "lsp: read error, closing connection");
                    break;
                }
            }
        }
        // Connection gone: fail every in-flight request so awaiters don't hang.
        for (_, tx) in pending_reader.lock().await.drain() {
            let _ = tx.send(Err(LspError::Closed));
        }
    });

    let client = LspClient {
        outgoing: out_tx,
        pending,
        next_id: Arc::new(AtomicI64::new(1)),
    };
    (client, in_rx)
}

/// Classify one parsed message and dispatch it: responses resolve a pending request; server
/// notifications and server requests go to the inbound channel.
async fn route_inbound(msg: Value, pending: &Pending, inbound: &mpsc::UnboundedSender<LspInbound>) {
    let has_method = msg.get("method").is_some();
    match (msg.get("id"), has_method) {
        // Response: id, no method.
        (Some(id), false) => {
            let Some(id) = id.as_i64() else { return };
            let Some(tx) = pending.lock().await.remove(&id) else {
                tracing::warn!(id, "lsp: response for unknown request id");
                return;
            };
            let result = match msg.get("error") {
                Some(err) => Err(LspError::Rpc {
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(result);
        }
        // Server→client request: id and method.
        (Some(id), true) => {
            let _ = inbound.send(LspInbound::Request {
                id: id.clone(),
                method: method_of(&msg),
                params: params_of(&msg),
            });
        }
        // Notification: method, no id.
        (None, true) => {
            let _ = inbound.send(LspInbound::Notification {
                method: method_of(&msg),
                params: params_of(&msg),
            });
        }
        // Neither id nor method: malformed; drop.
        (None, false) => {}
    }
}

fn method_of(msg: &Value) -> String {
    msg.get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn params_of(msg: &Value) -> Value {
    msg.get("params").cloned().unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn within<T>(fut: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(2), fut)
            .await
            .expect("timed out")
    }

    /// A trivial mock server: replies to every request by echoing the method name back in the
    /// result, and forwards received notifications to `notif_tx`.
    async fn echo_server<R, W>(
        reader: R,
        mut writer: W,
        notif_tx: mpsc::UnboundedSender<(String, Value)>,
    ) where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        while let Ok(Some(body)) = transport::read_frame(&mut reader).await {
            let msg: Value = serde_json::from_slice(&body).unwrap();
            let method = msg["method"].as_str().unwrap_or_default().to_string();
            if let Some(id) = msg.get("id") {
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": {"method": method}});
                transport::write_frame(&mut writer, &serde_json::to_vec(&reply).unwrap())
                    .await
                    .unwrap();
            } else {
                let _ = notif_tx.send((method, params_of(&msg)));
            }
        }
    }

    /// Connect a client to a freshly-spawned echo server over in-memory pipes.
    fn client_with_echo_server() -> (LspClient, mpsc::UnboundedReceiver<(String, Value)>) {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);
        let (notif_tx, notif_rx) = mpsc::unbounded_channel();
        tokio::spawn(echo_server(sr, sw, notif_tx));
        let (client, _inbound) = connect(cr, cw);
        (client, notif_rx)
    }

    #[tokio::test]
    async fn concurrent_requests_resolve_by_id() {
        let (client, _n) = client_with_echo_server();
        let (ra, rb, rc) = within(async {
            tokio::join!(
                client.request("alpha", json!({})),
                client.request("beta", json!({})),
                client.request("gamma", json!({})),
            )
        })
        .await;
        assert_eq!(ra.unwrap()["method"], "alpha");
        assert_eq!(rb.unwrap()["method"], "beta");
        assert_eq!(rc.unwrap()["method"], "gamma");
    }

    #[tokio::test]
    async fn notifications_reach_the_server_in_order() {
        let (client, mut notif_rx) = client_with_echo_server();
        client
            .notify("textDocument/didOpen", json!({"n": 1}))
            .unwrap();
        client
            .notify("textDocument/didChange", json!({"n": 2}))
            .unwrap();
        let first = within(notif_rx.recv()).await.unwrap();
        let second = within(notif_rx.recv()).await.unwrap();
        assert_eq!(first.0, "textDocument/didOpen");
        assert_eq!(first.1["n"], 1);
        assert_eq!(second.0, "textDocument/didChange");
        assert_eq!(second.1["n"], 2);
    }

    #[tokio::test]
    async fn server_notifications_reach_inbound() {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_io);
        let (_sr, mut sw) = tokio::io::split(server_io);
        // Server unilaterally publishes diagnostics.
        tokio::spawn(async move {
            let notif = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {"uri": "file:///x.rs", "diagnostics": []}
            });
            transport::write_frame(&mut sw, &serde_json::to_vec(&notif).unwrap())
                .await
                .unwrap();
            // keep the stream open
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let (_client, mut inbound) = connect(cr, cw);
        let msg = within(inbound.recv()).await.unwrap();
        match msg {
            LspInbound::Notification { method, params } => {
                assert_eq!(method, "textDocument/publishDiagnostics");
                assert_eq!(params["uri"], "file:///x.rs");
            }
            other => panic!("expected notification, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_requests_reach_inbound_and_can_be_answered() {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, mut sw) = tokio::io::split(server_io);
        // Server asks the client to apply an edit, then reads the client's response.
        let server = tokio::spawn(async move {
            let req =
                json!({"jsonrpc": "2.0", "id": 99, "method": "workspace/applyEdit", "params": {}});
            transport::write_frame(&mut sw, &serde_json::to_vec(&req).unwrap())
                .await
                .unwrap();
            let mut reader = BufReader::new(sr);
            let body = transport::read_frame(&mut reader).await.unwrap().unwrap();
            serde_json::from_slice::<Value>(&body).unwrap()
        });
        let (client, mut inbound) = connect(cr, cw);
        match within(inbound.recv()).await.unwrap() {
            LspInbound::Request { id, method, .. } => {
                assert_eq!(method, "workspace/applyEdit");
                client.respond(id, json!({"applied": true})).unwrap();
            }
            other => panic!("expected request, got {other:?}"),
        }
        let echoed = within(server).await.unwrap();
        assert_eq!(echoed["id"], 99);
        assert_eq!(echoed["result"]["applied"], true);
    }

    #[tokio::test]
    async fn server_error_response_surfaces() {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, mut sw) = tokio::io::split(server_io);
        tokio::spawn(async move {
            let mut reader = BufReader::new(sr);
            let body = transport::read_frame(&mut reader).await.unwrap().unwrap();
            let req: Value = serde_json::from_slice(&body).unwrap();
            let reply = json!({
                "jsonrpc": "2.0", "id": req["id"],
                "error": {"code": -32601, "message": "method not found"}
            });
            transport::write_frame(&mut sw, &serde_json::to_vec(&reply).unwrap())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let (client, _inbound) = connect(cr, cw);
        let err = within(client.request("bogus", json!({})))
            .await
            .unwrap_err();
        match err {
            LspError::Rpc { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "method not found");
            }
            other => panic!("expected rpc error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pending_request_fails_when_connection_closes() {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);
        // Server reads the request (so the send succeeds) then drops, closing the stream.
        tokio::spawn(async move {
            let mut reader = BufReader::new(sr);
            let _ = transport::read_frame(&mut reader).await;
            drop(sw);
            drop(reader);
        });
        let (client, _inbound) = connect(cr, cw);
        let err = within(client.request("x", json!({}))).await.unwrap_err();
        assert!(matches!(err, LspError::Closed));
    }

    #[tokio::test]
    async fn notify_eventually_errors_after_close() {
        let (client_io, server_io) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_io);
        let (client, _inbound) = connect(cr, cw);
        drop(server_io); // close the peer

        // The writer task only learns the stream is dead when it next tries to write, so the first
        // notification may be buffered before the failure propagates back. Poll until it does.
        let mut last = Ok(());
        for _ in 0..50 {
            last = client.notify("x", json!({}));
            if last.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(matches!(last, Err(LspError::Closed)));
    }
}
