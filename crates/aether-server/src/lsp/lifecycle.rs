//! LSP session lifecycle and document synchronization, expressed over an [`LspClient`].
//!
//! [`initialize`] performs the handshake (advertising our position-encoding preference and reading
//! back the server's capabilities); [`shutdown`] tears it down. The `did_*` helpers send the
//! document-sync notifications. Phase 1 uses **full-document sync**: every change resends the whole
//! buffer text — correct and trivial; incremental ranges (using a `TextChange` from `apply_edit`)
//! are a later optimization (see `docs/lsp.md`).

use serde_json::{json, Value};
use std::path::Path;

use super::client::{LspClient, LspError};
use super::position::PositionEncoding;
use super::uri;

/// The slice of a server's `initialize` response we actually use.
#[derive(Debug, Clone)]
pub struct ServerCaps {
    /// Server-reported name (`serverInfo.name`), e.g. `"rust-analyzer"`. `None` when the server
    /// reports no `serverInfo` (the vscode json/css/html servers don't) — the manager then keeps
    /// the launch command as the display name rather than showing "unknown".
    pub name: Option<String>,
    /// The position encoding the server selected from those we offered. Defaults to UTF-16 (the
    /// encoding every server must support) when the field is absent.
    pub position_encoding: PositionEncoding,
    /// Whether the server advertises `documentFormattingProvider` (whole-document formatting).
    /// Many servers don't (pyright, bash-language-server, marksman, …); `lsp/format` uses this to
    /// say "no formatter for X" rather than a vague "nothing to format".
    pub document_formatting: bool,
}

/// Perform the `initialize`/`initialized` handshake against `workspace_root`. `init_options`, when
/// present, is sent as the server-specific `initializationOptions` (see [`super::config`]).
pub async fn initialize(
    client: &LspClient,
    workspace_root: &Path,
    init_options: Option<Value>,
) -> Result<ServerCaps, LspError> {
    let root_uri = uri::path_to_uri(workspace_root);
    let folder_name = workspace_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root_uri.clone());

    let mut params = json!({
        "processId": std::process::id(),
        "clientInfo": { "name": "aether", "version": env!("CARGO_PKG_VERSION") },
        "rootUri": root_uri,
        "workspaceFolders": [ { "uri": root_uri, "name": folder_name } ],
        "capabilities": {
            // Advertise UTF-8 first; servers that support it let us skip UTF-16 conversion.
            "general": { "positionEncodings": ["utf-8", "utf-16"] },
            "textDocument": {
                "synchronization": { "dynamicRegistration": false, "didSave": true },
                "publishDiagnostics": { "relatedInformation": true },
                // We render hover text as Markdown (preferred) or plain — advertise both so servers
                // (e.g. rust-analyzer) send rich Markdown instead of falling back to plaintext.
                "hover": { "contentFormat": ["markdown", "plaintext"] },
                // We parse `LocationLink` (its precise `targetSelectionRange`), so let servers send
                // it for goto-definition instead of the coarser `Location`.
                "definition": { "linkSupport": true },
                // We flatten the hierarchical `DocumentSymbol[]` ourselves (tracking nesting depth
                // for the symbol picker's top-level collapse). Without this, servers (e.g.
                // rust-analyzer) fall back to the flat `SymbolInformation[]` form — everything depth
                // 0, so the "top" chip has nothing to hide.
                "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
            },
            // Let servers report background work (indexing, `cargo check`, …) via `$/progress`,
            // which we surface as the status-bar busy glyph and in the LSP picker.
            "window": { "workDoneProgress": true },
        },
    });
    if let Some(opts) = init_options {
        params["initializationOptions"] = opts;
    }

    let result = client.request("initialize", params).await?;

    let position_encoding = result
        .get("capabilities")
        .and_then(|c| c.get("positionEncoding"))
        .and_then(Value::as_str)
        .map(PositionEncoding::from_lsp)
        .unwrap_or(PositionEncoding::Utf16);
    let name = result
        .get("serverInfo")
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    // `documentFormattingProvider` is `true` / `false` / a `DocumentFormattingOptions` object /
    // absent. Supported iff present and not explicitly `false`.
    let document_formatting = result
        .get("capabilities")
        .and_then(|c| c.get("documentFormattingProvider"))
        .map(|v| v != &Value::Bool(false))
        .unwrap_or(false);

    client.notify("initialized", json!({}))?;
    Ok(ServerCaps {
        name,
        position_encoding,
        document_formatting,
    })
}

/// Request `shutdown` and send `exit`, the graceful close sequence.
pub async fn shutdown(client: &LspClient) -> Result<(), LspError> {
    client.request("shutdown", Value::Null).await?;
    client.notify("exit", Value::Null)
}

/// Notify the server that a document is now open. `version` is the buffer revision; `language_id`
/// is the LSP language identifier (matches our `LanguageConfig::name` for the languages we wire).
pub fn did_open(
    client: &LspClient,
    uri: &str,
    language_id: &str,
    version: i64,
    text: &str,
) -> Result<(), LspError> {
    client.notify(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": language_id,
            "version": version,
            "text": text,
        }}),
    )
}

/// Full-document change notification: resend the whole buffer.
pub fn did_change_full(
    client: &LspClient,
    uri: &str,
    version: i64,
    text: &str,
) -> Result<(), LspError> {
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [ { "text": text } ],
        }),
    )
}

pub fn did_close(client: &LspClient, uri: &str) -> Result<(), LspError> {
    client.notify(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::client::connect;
    use crate::lsp::transport;
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncWrite, BufReader};
    use tokio::sync::mpsc;

    /// Records the most recent `initialize` params and forwards every notification it receives.
    async fn capability_server<R, W>(
        reader: R,
        mut writer: W,
        capabilities: Value,
        events: mpsc::UnboundedSender<(String, Value)>,
    ) where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        while let Ok(Some(body)) = transport::read_frame(&mut reader).await {
            let msg: Value = serde_json::from_slice(&body).unwrap();
            let method = msg["method"].as_str().unwrap_or_default().to_string();
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            match msg.get("id") {
                Some(id) => {
                    let result = match method.as_str() {
                        "initialize" => json!({
                            "capabilities": capabilities,
                            "serverInfo": { "name": "mock-ls", "version": "1.0" }
                        }),
                        _ => Value::Null, // e.g. shutdown
                    };
                    let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                    transport::write_frame(&mut writer, &serde_json::to_vec(&reply).unwrap())
                        .await
                        .unwrap();
                    let _ = events.send((format!("request:{method}"), params));
                }
                None => {
                    let _ = events.send((method, params));
                }
            }
        }
    }

    fn connect_to_server(
        capabilities: Value,
    ) -> (LspClient, mpsc::UnboundedReceiver<(String, Value)>) {
        let (client_io, server_io) = tokio::io::duplex(16384);
        let (cr, cw) = tokio::io::split(client_io);
        let (sr, sw) = tokio::io::split(server_io);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        tokio::spawn(capability_server(sr, sw, capabilities, ev_tx));
        let (client, _inbound) = connect(cr, cw);
        (client, ev_rx)
    }

    async fn recv(rx: &mut mpsc::UnboundedReceiver<(String, Value)>) -> (String, Value) {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed")
    }

    #[tokio::test]
    async fn handshake_reads_encoding_and_name_and_sends_initialized() {
        let (client, mut ev) = connect_to_server(json!({ "positionEncoding": "utf-8" }));
        let caps = initialize(&client, Path::new("/home/joe/proj"), None)
            .await
            .unwrap();
        assert_eq!(caps.name.as_deref(), Some("mock-ls"));
        assert_eq!(caps.position_encoding, PositionEncoding::Utf8);

        // The server saw `initialize` (with our root + offered encodings) then `initialized`.
        let (m1, p1) = recv(&mut ev).await;
        assert_eq!(m1, "request:initialize");
        assert_eq!(p1["rootUri"], "file:///home/joe/proj");
        assert_eq!(
            p1["capabilities"]["general"]["positionEncodings"][0],
            "utf-8"
        );
        let (m2, _) = recv(&mut ev).await;
        assert_eq!(m2, "initialized");
    }

    #[tokio::test]
    async fn handshake_defaults_to_utf16_when_server_is_silent() {
        let (client, mut ev) = connect_to_server(json!({})); // no positionEncoding
        let caps = initialize(&client, Path::new("/p"), None).await.unwrap();
        assert_eq!(caps.position_encoding, PositionEncoding::Utf16);
        let _ = recv(&mut ev).await;
    }

    #[tokio::test]
    async fn handshake_reads_formatting_capability() {
        // Advertised → true; absent → false (the default).
        let (client, mut ev) = connect_to_server(json!({ "documentFormattingProvider": true }));
        assert!(
            initialize(&client, Path::new("/p"), None)
                .await
                .unwrap()
                .document_formatting
        );
        let _ = recv(&mut ev).await;

        let (client2, mut ev2) = connect_to_server(json!({}));
        assert!(
            !initialize(&client2, Path::new("/p"), None)
                .await
                .unwrap()
                .document_formatting
        );
        let _ = recv(&mut ev2).await;
    }

    #[tokio::test]
    async fn handshake_forwards_init_options() {
        let (client, mut ev) = connect_to_server(json!({}));
        initialize(
            &client,
            Path::new("/p"),
            Some(json!({ "provideFormatter": true })),
        )
        .await
        .unwrap();
        let (m, p) = recv(&mut ev).await;
        assert_eq!(m, "request:initialize");
        assert_eq!(p["initializationOptions"]["provideFormatter"], true);
    }

    #[tokio::test]
    async fn document_sync_messages_have_expected_shape() {
        let (client, mut ev) = connect_to_server(json!({}));
        let uri = "file:///home/joe/main.rs";

        did_open(&client, uri, "rust", 1, "fn main() {}").unwrap();
        let (m, p) = recv(&mut ev).await;
        assert_eq!(m, "textDocument/didOpen");
        assert_eq!(p["textDocument"]["uri"], uri);
        assert_eq!(p["textDocument"]["languageId"], "rust");
        assert_eq!(p["textDocument"]["version"], 1);
        assert_eq!(p["textDocument"]["text"], "fn main() {}");

        did_change_full(&client, uri, 2, "fn main() { todo!() }").unwrap();
        let (m, p) = recv(&mut ev).await;
        assert_eq!(m, "textDocument/didChange");
        assert_eq!(p["textDocument"]["version"], 2);
        assert_eq!(p["contentChanges"][0]["text"], "fn main() { todo!() }");
        assert!(
            p["contentChanges"][0].get("range").is_none(),
            "full sync sends no range"
        );

        did_close(&client, uri).unwrap();
        let (m, p) = recv(&mut ev).await;
        assert_eq!(m, "textDocument/didClose");
        assert_eq!(p["textDocument"]["uri"], uri);
    }

    #[tokio::test]
    async fn shutdown_requests_then_exits() {
        let (client, mut ev) = connect_to_server(json!({}));
        shutdown(&client).await.unwrap();
        let (m1, _) = recv(&mut ev).await;
        assert_eq!(m1, "request:shutdown");
        let (m2, _) = recv(&mut ev).await;
        assert_eq!(m2, "exit");
    }
}
