//! An **in-process fake language server** for integration tests.
//!
//! Speaks just enough LSP over the in-memory transport ([`super::transport`]) to exercise Aether's
//! LSP handling deterministically — no real server binary, no subprocess, no multi-second
//! cold-start. A test registers a [`DummyLspConfig`] per language on
//! [`super::manager::LspManager::dummy_configs`]; [`super::manager::launch`] then wires this server
//! through the same [`super::manager`] `bring_up` path a real process uses (over
//! [`tokio::io::duplex`] pipes instead of stdio).
//!
//! This is a *test seam* that ships in the library the same way [`crate::spawn_for_test`] does: it
//! is inert in production (the config map is only ever populated by test spawn helpers) and lets
//! the LSP integration tests run in the normal suite instead of being `#[ignore]`d behind a real
//! server. It answers the handshake, publishes canned diagnostics on open/change, and replies to
//! hover / definition / references / formatting requests from the config.

use super::transport;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};

/// A half-open span `[start, end)` in LSP `(line, character)` coordinates. Test content is ASCII,
/// so character == column == byte regardless of the negotiated position encoding.
#[derive(Clone, Debug)]
pub struct DummyRange {
    pub line: u32,
    pub character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

impl DummyRange {
    /// A single-line span on `line`, columns `[start, end)`.
    pub fn on(line: u32, start: u32, end: u32) -> Self {
        DummyRange {
            line,
            character: start,
            end_line: line,
            end_character: end,
        }
    }
    fn to_json(&self) -> Value {
        json!({
            "start": { "line": self.line, "character": self.character },
            "end": { "line": self.end_line, "character": self.end_character },
        })
    }
}

/// One canned diagnostic (published via `textDocument/publishDiagnostics`).
#[derive(Clone, Debug)]
pub struct DummyDiagnostic {
    pub range: DummyRange,
    /// LSP severity: 1 = Error, 2 = Warning, 3 = Information, 4 = Hint.
    pub severity: u8,
    pub message: String,
}

/// One canned whole-document formatting edit.
#[derive(Clone, Debug)]
pub struct DummyTextEdit {
    pub range: DummyRange,
    pub new_text: String,
}

/// When a dummy server should publish its diagnostics, re-evaluated on every `didOpen` / `didChange`
/// against the (full-sync) document text — the knob for the "an edit clears the error" tests.
#[derive(Clone, Debug)]
pub enum DiagnosticsTrigger {
    /// Publish while the text *contains* this substring; clear once it's gone (e.g. an error token
    /// that an undo removes).
    Present(String),
    /// Publish while the text does *not* contain this substring; clear once it appears (e.g. a `//`
    /// that comments the offending line out).
    Absent(String),
}

/// The canned behaviour of one dummy server. Everything defaults to "unsupported / empty".
#[derive(Clone, Debug, Default)]
pub struct DummyLspConfig {
    /// Diagnostics for an opened document. With `diagnostics_trigger`, they're published only when
    /// the trigger condition holds (re-evaluated on every `didOpen` / `didChange`), so an edit
    /// clears them — exercising Aether's didChange round-trip. Without it, published on open.
    pub diagnostics: Vec<DummyDiagnostic>,
    pub diagnostics_trigger: Option<DiagnosticsTrigger>,
    /// `textDocument/hover` contents (rendered as Markdown), or `None` to answer null.
    pub hover: Option<String>,
    /// `textDocument/definition` target (in the *requested* document), or `None` to answer null.
    pub definition: Option<DummyRange>,
    /// `textDocument/references` targets (in the requested document).
    pub references: Vec<DummyRange>,
    /// `textDocument/formatting` edits; also flips `documentFormattingProvider` in the handshake so
    /// Aether's `lsp/format` doesn't short-circuit with "no formatter".
    pub formatting: Vec<DummyTextEdit>,
}

/// Run one dummy server to completion over `reader`/`writer` (the server ends of a duplex pipe).
/// Returns when the client half closes or an `exit` notification arrives.
pub async fn serve<R, W>(reader: R, writer: W, config: DummyLspConfig)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let mut writer = writer;
    // Latest full text per document URI — full-document sync (see `lifecycle`) means each change
    // carries the whole buffer, so the diagnostics trigger just checks `contains`.
    let mut docs: HashMap<String, String> = HashMap::new();

    while let Ok(Some(body)) = transport::read_frame(&mut reader).await {
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();

        match method {
            "initialize" => {
                respond(
                    &mut writer,
                    id,
                    json!({
                        "capabilities": {
                            // Accept Aether's preferred UTF-8 so positions stay byte==char for ASCII.
                            "positionEncoding": "utf-8",
                            "textDocumentSync": 1, // full
                            "hoverProvider": config.hover.is_some(),
                            "definitionProvider": config.definition.is_some(),
                            "referencesProvider": !config.references.is_empty(),
                            "documentFormattingProvider": !config.formatting.is_empty(),
                        },
                        "serverInfo": { "name": "dummy-lsp" },
                    }),
                )
                .await;
            }
            "shutdown" => respond(&mut writer, id, Value::Null).await,
            "exit" => break,
            "textDocument/didOpen" => {
                let uri = str_at(&msg, &["params", "textDocument", "uri"]);
                let text = str_at(&msg, &["params", "textDocument", "text"]);
                docs.insert(uri.clone(), text.clone());
                publish_diagnostics(&mut writer, &config, &uri, &text).await;
            }
            "textDocument/didChange" => {
                let uri = str_at(&msg, &["params", "textDocument", "uri"]);
                // Full sync: the last content change carries the whole document text.
                let text = msg
                    .pointer("/params/contentChanges")
                    .and_then(Value::as_array)
                    .and_then(|c| c.last())
                    .map(|c| {
                        c.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string()
                    })
                    .unwrap_or_default();
                docs.insert(uri.clone(), text.clone());
                publish_diagnostics(&mut writer, &config, &uri, &text).await;
            }
            "textDocument/hover" => {
                let result = config
                    .hover
                    .as_ref()
                    .map(|md| json!({ "contents": { "kind": "markdown", "value": md } }))
                    .unwrap_or(Value::Null);
                respond(&mut writer, id, result).await;
            }
            "textDocument/definition" => {
                let uri = str_at(&msg, &["params", "textDocument", "uri"]);
                let result = config
                    .definition
                    .as_ref()
                    .map(|r| json!({ "uri": uri, "range": r.to_json() }))
                    .unwrap_or(Value::Null);
                respond(&mut writer, id, result).await;
            }
            "textDocument/references" => {
                let uri = str_at(&msg, &["params", "textDocument", "uri"]);
                let locs: Vec<Value> = config
                    .references
                    .iter()
                    .map(|r| json!({ "uri": uri, "range": r.to_json() }))
                    .collect();
                respond(&mut writer, id, Value::Array(locs)).await;
            }
            "textDocument/formatting" => {
                let edits: Vec<Value> = config
                    .formatting
                    .iter()
                    .map(|e| json!({ "range": e.range.to_json(), "newText": e.new_text }))
                    .collect();
                respond(&mut writer, id, Value::Array(edits)).await;
            }
            // Any other request must still get a reply so the client doesn't wait forever; other
            // notifications (initialized, didSave, didClose, …) need none.
            _ if id.is_some() => respond(&mut writer, id, Value::Null).await,
            _ => {}
        }
    }
}

/// Publish `config.diagnostics` for `uri` — or an empty set, when a trigger is set and `text` no
/// longer contains it (a clear).
async fn publish_diagnostics<W: AsyncWrite + Unpin>(
    writer: &mut W,
    config: &DummyLspConfig,
    uri: &str,
    text: &str,
) {
    let active = match &config.diagnostics_trigger {
        None => true,
        Some(DiagnosticsTrigger::Present(s)) => text.contains(s.as_str()),
        Some(DiagnosticsTrigger::Absent(s)) => !text.contains(s.as_str()),
    };
    let diagnostics: Vec<Value> = if active {
        config
            .diagnostics
            .iter()
            .map(|d| {
                json!({
                    "range": d.range.to_json(),
                    "severity": d.severity,
                    "message": d.message,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    let notif = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": uri, "diagnostics": diagnostics },
    });
    let _ = transport::write_frame(writer, &serde_json::to_vec(&notif).unwrap()).await;
}

/// Send a JSON-RPC response for `id` (a no-op when `id` is `None`, i.e. the message was a
/// notification).
async fn respond<W: AsyncWrite + Unpin>(writer: &mut W, id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let _ = transport::write_frame(writer, &serde_json::to_vec(&reply).unwrap()).await;
}

/// A string at a nested key path in `msg`, or `""`.
fn str_at(msg: &Value, path: &[&str]) -> String {
    let mut cur = msg;
    for key in path {
        cur = &cur[key];
    }
    cur.as_str().unwrap_or("").to_string()
}
