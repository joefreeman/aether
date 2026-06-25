//! LSP-related messages.
//!
//! Aether-server is itself an LSP *client*: it hosts language-server subprocesses and translates
//! between them and connected editor clients (see `docs/lsp.md`). The TUI never speaks LSP. These
//! messages surface language-server *health* to the client — the status of each server in the
//! active project plus live transitions — and let the client request a restart.
//!
//! Defined in Phase 0 ahead of the transport so the wire shape is pinned and the status UI can be
//! built against it; the server side that emits these lands in Phase 1.

use crate::cursor::CursorState;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::{BufferId, LogicalPosition};
use serde::{Deserialize, Serialize};

// ---- lsp/hover ----------------------------------------------------------------------------------

/// Hover info (type signature + docs) for the client's cursor in `buffer_id`. Cursor-relative —
/// like the input commands, it carries no position; the server uses the cursor it already tracks.
pub struct LspHover;
impl RpcMethod for LspHover {
    const NAME: &'static str = "lsp/hover";
    type Params = LspBufferParams;
    type Result = LspHoverResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LspHoverResult {
    /// Hover text, or `None` when the server has nothing for the cursor / no server is attached.
    pub contents: Option<String>,
    /// Whether `contents` is Markdown — the LSP `MarkupContent.kind` (or a `MarkedString` code
    /// block), preserved so the client renders it as Markdown vs. literal plain text. `false` for
    /// `kind: "plaintext"` (and when there's no content).
    #[serde(default)]
    pub markdown: bool,
    /// Whether the server could even answer — lets the client say "still starting" / "crashed"
    /// instead of a misleading "no hover info" when `contents` is empty only because no ready
    /// server replied.
    #[serde(default)]
    pub readiness: LspReadiness,
}

/// Whether the language server backing a buffer can currently serve a cursor request (hover,
/// goto-definition). Lets those results tell "server still starting / crashed / absent" apart from
/// "ready server replied, but there's nothing here" — so the client shows a precise message rather
/// than a catch-all "nothing found". Mirrors [`FormatStatus`]'s readiness arms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspReadiness {
    /// A ready server answered — any empty payload is genuine ("nothing here").
    #[default]
    Ready,
    /// No server is attached to this buffer (unsupported language, or not file-backed).
    NoServer,
    /// A server exists for this language but isn't `Ready` yet — try again shortly.
    Starting,
    /// The attached server crashed or was stopped — it can't answer until it's running again.
    Unavailable,
}

// ---- lsp/goto_definition ------------------------------------------------------------------------

/// Resolve the definition of the symbol at the client's cursor. Cursor-relative (no position on
/// the wire). The client navigates to the returned location itself (`buffer/open` + jump).
pub struct LspGotoDefinition;
impl RpcMethod for LspGotoDefinition {
    const NAME: &'static str = "lsp/goto_definition";
    type Params = LspBufferParams;
    type Result = LspGotoDefinitionResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LspGotoDefinitionResult {
    /// `None` when there's no definition (or no server). The first location is returned when the
    /// server offers several.
    pub location: Option<LspLocation>,
    /// Whether the server could answer — distinguishes "no definition" from "server not ready yet".
    #[serde(default)]
    pub readiness: LspReadiness,
}

/// A resolved source location, in the editor's own coordinates (absolute path + byte-column
/// position) — already converted from the server's LSP position encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspLocation {
    /// Absolute filesystem path to the target file.
    pub path: String,
    /// Start of the identifier span (the jump target / selection anchor).
    pub position: LogicalPosition,
    /// Inclusive last position of the identifier span — equal to `position` when the server gives
    /// no distinct span (empty / multi-line / malformed range). Lets the caller land the identifier
    /// *selected* (anchor at `position`, cursor here), matching the outline picker.
    pub end: LogicalPosition,
}

/// Params for the cursor-relative LSP requests: just the buffer; the server uses its own cursor.
#[derive(Debug, Serialize, Deserialize)]
pub struct LspBufferParams {
    pub buffer_id: BufferId,
}

// ---- lsp/document_highlight ---------------------------------------------------------------------

/// Highlight every occurrence of the symbol under the cursor (`textDocument/documentHighlight`).
/// Cursor-relative and fire-and-forget: the server resolves the symbol, stores the occurrence
/// ranges keyed by `(client, buffer)`, and pushes the refreshed viewport — the occurrences ride
/// `viewport/lines_changed` as ordinary match highlights (the same styling as search matches), so
/// there's nothing to return. The client fires this as the cursor settles, but only when no search
/// is active: a search owns the highlight layer, and the server drops any symbol set while one is.
pub struct LspDocumentHighlight;
impl RpcMethod for LspDocumentHighlight {
    const NAME: &'static str = "lsp/document_highlight";
    type Params = LspDocumentHighlightParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LspDocumentHighlightParams {
    pub buffer_id: BufferId,
    /// `true` resolves and paints the symbol under the cursor; `false` clears any existing set for
    /// the buffer. The client sends `false` when it leaves Normal mode (Insert, or the search
    /// prompt before a query masks the set), where a stale symbol highlight must not linger.
    pub active: bool,
}

/// Identifies the language server backing a buffer — its `(language, workspace_root)` key. Returned
/// in `buffer/open` so the client can show *this buffer's* server health: servers are keyed by
/// `(language, workspace_root)`, so language alone is ambiguous when a monorepo runs several
/// same-language servers at different roots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspServerRef {
    pub language: String,
    pub workspace_root: String,
}

// ---- lsp/format ---------------------------------------------------------------------------------

/// Format the whole buffer via the language server (`textDocument/formatting`). The server
/// requests the edits, applies them to the buffer itself (one undo step), and pushes the
/// re-rendered viewports — so this is just a trigger, like the other cursor-relative commands.
pub struct LspFormat;
impl RpcMethod for LspFormat {
    const NAME: &'static str = "lsp/format";
    type Params = LspBufferParams;
    type Result = LspFormatResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LspFormatResult {
    /// Cursor after formatting (clamped into the reformatted buffer). Unchanged unless
    /// `status == Applied`.
    pub cursor: CursorState,
    /// Why formatting did or didn't change the buffer — lets the client show a specific message
    /// instead of a catch-all.
    pub status: FormatStatus,
}

/// Outcome of an [`LspFormat`] request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatStatus {
    /// Edits were applied; the buffer changed.
    Applied,
    /// A formatter ran but produced no changes (already canonical), or the edits were dropped as
    /// stale (the buffer changed under the request).
    NoChange,
    /// A server exists for this language but isn't `Ready` yet — try again shortly.
    NotReady,
    /// The language server crashed or was stopped — it can't format until it's running again.
    Unavailable,
    /// A ready server is attached but doesn't advertise a document formatter for this language.
    Unsupported,
}

// ---- lsp/navigate_diagnostic --------------------------------------------------------------------

/// Jump the cursor to the next/previous diagnostic in the buffer. Mirrors `git/navigate_hunk`: the
/// server holds the diagnostics, so it resolves the target and moves the cursor authoritatively.
pub struct LspNavigateDiagnostic;
impl RpcMethod for LspNavigateDiagnostic {
    const NAME: &'static str = "lsp/navigate_diagnostic";
    type Params = LspNavigateDiagnosticParams;
    type Result = LspNavigateDiagnosticResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LspNavigateDiagnosticParams {
    pub buffer_id: BufferId,
    pub direction: DiagnosticDirection,
    /// How many diagnostics to skip in `direction`. Defaults to 1; when fewer than `count` remain
    /// the cursor lands on the furthest reachable diagnostic rather than not moving at all.
    #[serde(
        default = "crate::count_one",
        skip_serializing_if = "crate::count_is_one"
    )]
    pub count: u32,
    /// Grow the selection to the landing diagnostic (Shift) rather than collapsing to a point there:
    /// the anchor is kept and the cursor jumps to the diagnostic.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub extend: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticDirection {
    Next,
    Prev,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LspNavigateDiagnosticResult {
    /// Cursor after the jump. Equal to the incoming cursor when `moved` is false.
    pub cursor: CursorState,
    /// False when there's no diagnostic in the requested direction (cursor unchanged).
    pub moved: bool,
}

// ---- lsp/restart_server -------------------------------------------------------------------------

/// Restart the language server for a given language: shut it down and respawn, then re-open every
/// currently-open buffer of that language against the fresh process.
pub struct LspRestartServer;
impl RpcMethod for LspRestartServer {
    const NAME: &'static str = "lsp/restart_server";
    type Params = LspRestartServerParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LspRestartServerParams {
    /// The language whose server should be restarted, e.g. `"rust"`.
    pub language: String,
}

// ---- lsp/status_changed (notification) ----------------------------------------------------------

/// Pushed whenever a server's [`LspStatus`] transitions, so the status bar / dialog updates live
/// without polling.
pub struct LspStatusChanged;
impl NotificationMethod for LspStatusChanged {
    const NAME: &'static str = "lsp/status_changed";
    type Params = LspServerStatus;
}

// ---- lsp/diagnostics_changed (notification) -----------------------------------------------------

/// Pushed when a buffer's diagnostics change, carrying per-severity counts for the status bar.
/// (The per-line spans for squiggles/gutter ride `viewport/lines_changed`; this is the buffer-wide
/// summary the client can't derive from just the visible window.)
pub struct LspDiagnosticsChanged;
impl NotificationMethod for LspDiagnosticsChanged {
    const NAME: &'static str = "lsp/diagnostics_changed";
    type Params = LspDiagnosticsChangedParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LspDiagnosticsChangedParams {
    pub buffer_id: BufferId,
    pub counts: DiagnosticCounts,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticCounts {
    pub errors: u32,
    pub warnings: u32,
    pub infos: u32,
    pub hints: u32,
}

impl DiagnosticCounts {
    pub fn is_empty(&self) -> bool {
        self.errors == 0 && self.warnings == 0 && self.infos == 0 && self.hints == 0
    }
}

// ---- shared payloads ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspServerStatus {
    /// Display name reported by the server, e.g. `"rust-analyzer"`.
    pub name: String,
    /// The language this server backs, e.g. `"rust"`.
    pub language: String,
    /// Absolute workspace root the server was launched against.
    pub workspace_root: String,
    pub status: LspStatus,
    /// Work the server is currently doing, from `$/progress` (e.g. indexing, `cargo check`). One
    /// entry per active progress token — servers run several at once. Non-empty means "busy": the
    /// status-bar glyph shows the busy spinner, and the LSP picker lists these. Empty when idle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress: Vec<LspProgress>,
}

/// One in-flight `$/progress` work-done operation reported by a language server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspProgress {
    /// Short title from the `begin` message, e.g. `"Indexing"` or `"cargo check"`.
    pub title: String,
    /// Latest detail message from `begin`/`report`, when the server sends one (e.g. `"1/430"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Completion percentage (0–100) when the server reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percentage: Option<u32>,
}

/// Lifecycle of a single language server. Internally tagged on `state` for a flat wire shape:
/// `{"state":"ready"}`, `{"state":"crashed","code":1,"message":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LspStatus {
    /// Subprocess spawned; handshake not yet sent.
    Starting,
    /// `initialize` sent, awaiting the capabilities response.
    Initializing,
    /// Handshake complete; serving requests.
    Ready,
    /// Shutting down and respawning (see [`LspRestartServer`]).
    Restarting,
    /// The subprocess exited unexpectedly.
    Crashed { code: Option<i32>, message: String },
    /// Cleanly shut down; not running.
    Stopped,
}
