//! Session state — the platform-free heart of a window's editing context
//! (docs/client-core.md): connection lifecycle, buffer identity, modal state, search,
//! prompts. The shell keeps the presentation companions (pixel scroll, animation, parsed
//! hover markdown) on its own struct.

use super::keymap::Action;
use super::picker::PickerState;
use aether_protocol::buffer::{BufferOpenResult, BufferReloadResult, BufferSaveResult};
use aether_protocol::cursor::{CursorState, Direction, Granularity, Motion};
use aether_protocol::git::CommitInfo;
use aether_protocol::input::SurroundTarget;
use aether_protocol::lsp::{DiagnosticCounts, LspServerRef, LspServerStatus};
use aether_protocol::search::SearchSummary;
use aether_protocol::viewport::{DiagnosticSeverity, ScrollPosition, Window, WrapMode};
use aether_protocol::{BufferId, LogicalPosition, ViewportId};

/// The session's connection lifecycle. The server is authoritative, so a dead socket just
/// freezes the window: the last buffer view stays rendered, editing input is suspended, and a
/// retry loop —
/// re-running discovery each attempt, since a restarted daemon gets a fresh port — rebuilds
/// the session when the server is back. On localhost the only real disconnect cause *is* a
/// daemon restart, so this is what makes "restart the daemon" seamless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connected,
    /// The socket died; a backoff retry is in flight. `had_unsaved` remembers whether edits
    /// were pending at disconnect — landing on a *restarted* daemon then means they're gone
    /// (buffers live in daemon memory), which warrants a warning.
    Reconnecting { attempt: u32, had_unsaved: bool },
    /// A live server answered but the session couldn't be re-established (the project is
    /// gone). Terminal — the window stays frozen.
    Failed,
}

/// Backoff before reconnect attempt `attempt`: 250ms doubling to a 5s ceiling, retrying
/// indefinitely — a failed localhost dial is instant and free, and the daemon coming back is
/// the expected outcome, not the exception.
pub fn reconnect_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis((250u64 << attempt.min(5)).min(5000))
}

#[derive(Clone, Debug)]
pub struct BufferInfo {
    pub buffer_id: BufferId,
    pub label: String,
    /// Canonical absolute path on disk; `None` for scratch buffers.
    pub path: Option<String>,
    pub language: Option<String>,
    pub revision: u64,
    pub saved_revision: u64,
    pub cursor: CursorState,
    pub scroll: Option<ScrollPosition>,
    pub transient: bool,
    /// The language server backing this buffer, if any — keys `lsp/status_changed` updates.
    pub lsp_server: Option<LspServerRef>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Normal,
    Insert,
    Search,
}

/// Client-side search-prompt state; the query/match list itself is server-owned.
#[derive(Default)]
pub struct SearchState {
    pub query: String,
    /// Byte cursor within `query`.
    pub cursor: usize,
    /// A committed search exists (highlights shown, `n`/`Alt-n` cycle it).
    pub active: bool,
    pub summary: Option<SearchSummary>,
    pub history: Vec<String>,
    pub history_cursor: Option<usize>,
    pub history_draft: String,
    /// The `?` variant: grow the selection from the entry point to each incremental match.
    pub extend_to_cursor: bool,
    /// State to restore on Esc, snapshotted when the prompt opens.
    pub snapshot: Option<SearchSnapshot>,
}

pub struct SearchSnapshot {
    pub cursor: CursorState,
    pub query: String,
    pub active: bool,
}

/// A modal dialog owning the keyboard: the `[y/N]`-style confirmation or the save-as path
/// input. Mirrors the web client's `modal.ts` (Enter/`y` accepts, Esc declines, a click on the
/// editor behind it cancels).
#[derive(Debug)]
pub enum Prompt {
    Confirm { message: String, action: ConfirmAction },
    SaveAs { path_index: u32, input: String, cursor: usize },
    /// LSP server detail (from the LspServers picker): info rows + `r` to restart.
    LspInfo(Box<LspServerStatus>),
}

/// What accepting a confirmation does.
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    /// Retry `buffer/save` with `overwrite: true`; `target` carries the save-as path (None for
    /// the in-place save).
    Save { target: Option<(u32, String)> },
    /// Retry `buffer/reload` with `force: true`.
    ReloadDiscard,
    /// Close the buffer despite unsaved changes.
    CloseDiscard,
}

/// Outcome of a `buffer/save` attempt: saved, or refused pending user confirmation.
#[derive(Debug)]
pub enum SaveTry {
    Saved {
        result: BufferSaveResult,
        target: Option<(u32, String)>,
    },
    NeedsConfirm {
        message: String,
        action: ConfirmAction,
    },
}

/// Outcome of a `buffer/reload` attempt.
#[derive(Debug)]
pub enum ReloadTry {
    Reloaded(BufferReloadResult),
    NeedsConfirm,
}

#[derive(Clone, Copy, Debug)]
pub enum Pending {
    None,
    Leader,
    Find {
        dir: Direction,
        till: bool,
        extend: bool,
        count: u32,
    },
    /// `Ctrl-s` armed: the next keystroke names the surround delimiter.
    Surround(SurroundTarget),
}

/// What `r` replays — the TUI's `RepeatTarget`: the binding intent for table actions, the
/// resolved motion (with its target char) for find.
#[derive(Debug, Clone)]
pub enum RepeatTarget {
    Action { action: Action, count: u32 },
    Find(Motion),
}

#[derive(Debug, Clone, Copy)]
pub enum PasteKind {
    /// Normal-mode `Ctrl-v`: collapse to selection start, insert, select pasted.
    Before { count: u32 },
    /// Normal-mode `Ctrl-r`: insert over the selection (the server replaces it), select pasted.
    Replace { count: u32 },
    /// Insert-mode `Ctrl-v`: plain insert at the caret.
    AtCursor,
    /// Insert-mode `Ctrl-r`: replace the whole line.
    Line,
}

/// The window's editing context over its server connection — exactly what the server calls a
/// client. `App` holds the window-level shell (chrome, toasts, metrics) around it.
pub struct Session {
    pub project: String,
    pub project_paths: Vec<String>,
    pub buffer: BufferInfo,
    pub mode: Mode,
    pub pending: Pending,
    pub count: Option<u32>,
    pub last_repeat: Option<RepeatTarget>,
    pub search: SearchState,

    pub viewport_id: Option<ViewportId>,
    pub window: Option<Window>,
    pub wrap: WrapMode,
    /// Inline diff view toggle — sticky across buffer switches (re-enabled after each
    /// subscribe), like the TUI's `ViewSettings`.
    pub diff_view: bool,
    pub diagnostics: DiagnosticCounts,
    pub lsp: Option<LspServerStatus>,
    pub externally_modified: bool,
    pub externally_deleted: bool,
    pub sent_grid: Option<(u32, u32)>, // (cols, rows) last sent to the server
    /// The scroll position the in-flight `viewport/subscribe` asked for; seeds the shell's
    /// scroll when the result arrives.
    pub subscribe_scroll: ScrollPosition,
    pub fetch_in_flight: bool,
    pub refetch_queued: bool,
    pub reveal_after_fetch: bool,
    pub drag: Option<(LogicalPosition, Granularity)>,
    /// Cursor-line blame, rendered as dim text after the line: `(line, "author · age")`.
    pub blame: Option<(u32, String)>,
    /// The `(line, revision)` the in-flight/most-recent blame request was for.
    pub blame_requested: Option<(u32, u64)>,
    /// A modal confirm / save-as dialog; owns the keyboard while open.
    pub prompt: Option<Prompt>,
    /// An open picker overlay; owns the keyboard while open.
    pub picker: Option<PickerState>,
    pub conn: ConnState,
}

impl Session {
    pub fn new(project: String, project_paths: Vec<String>, buffer: BufferInfo) -> Self {
        Session {
            project,
            project_paths,
            buffer,
            mode: Mode::Normal,
            pending: Pending::None,
            count: None,
            last_repeat: None,
            search: SearchState::default(),
            viewport_id: None,
            window: None,
            wrap: WrapMode::Soft,
            diff_view: false,
            diagnostics: DiagnosticCounts::default(),
            lsp: None,
            externally_modified: false,
            externally_deleted: false,
            sent_grid: None,
            subscribe_scroll: ScrollPosition {
                logical_line: 0,
                sub_row: 0.0,
            },
            fetch_in_flight: false,
            refetch_queued: false,
            reveal_after_fetch: false,
            drag: None,
            blame: None,
            blame_requested: None,
            prompt: None,
            picker: None,
            conn: ConnState::Connected,
        }
    }

    /// An inert stand-in for the boot chooser (no project picked yet): never rendered and
    /// never addressed — `update_boot` owns every message while `App.boot` is set.
    pub fn placeholder() -> Self {
        Session::new(
            String::new(),
            Vec::new(),
            BufferInfo {
                buffer_id: 0,
                label: String::new(),
                path: None,
                language: None,
                revision: 0,
                saved_revision: 0,
                cursor: CursorState::default(),
                scroll: None,
                transient: false,
                lsp_server: None,
            },
        )
    }
}

/// Build the client-side buffer record from a `buffer/open` result.
pub fn buffer_info(open: BufferOpenResult, roots: &[String]) -> BufferInfo {
    let label = match (&open.path, open.scratch_number) {
        (Some(path), _) => strip_longest_root(path, roots)
            .map(|(_, rel)| rel)
            .unwrap_or_else(|| path.clone()),
        (None, Some(n)) => format!("(scratch {n})"),
        (None, None) => "(scratch)".into(),
    };
    BufferInfo {
        buffer_id: open.buffer_id,
        label,
        path: open.path,
        language: open.language,
        revision: open.revision,
        saved_revision: open.saved_revision,
        cursor: open.cursor,
        scroll: open.scroll,
        transient: open.transient,
        lsp_server: open.lsp_server,
    }
}

/// Find the project root that contains `abs` (longest match wins, for nested roots) and return
/// `(path_index, relative_path)`.
pub fn strip_longest_root(abs: &str, roots: &[String]) -> Option<(u32, String)> {
    let abs_path = std::path::Path::new(abs);
    roots
        .iter()
        .enumerate()
        .filter_map(|(i, root)| {
            abs_path
                .strip_prefix(root)
                .ok()
                .map(|rel| (i as u32, root.len(), rel.to_string_lossy().into_owned()))
        })
        .max_by_key(|(_, len, _)| *len)
        .map(|(i, _, rel)| (i, rel))
}

/// The earlier of two positions (line-major).
pub fn min_pos(a: LogicalPosition, b: LogicalPosition) -> LogicalPosition {
    if (a.line, a.col) <= (b.line, b.col) {
        a
    } else {
        b
    }
}

/// The later of two positions (line-major).
pub fn max_pos(a: LogicalPosition, b: LogicalPosition) -> LogicalPosition {
    if (a.line, a.col) >= (b.line, b.col) {
        a
    } else {
        b
    }
}

/// One paragraph of the hover popover; diagnostics colour theirs by severity.
#[derive(Debug)]
pub struct HoverBlock {
    pub severity: Option<DiagnosticSeverity>,
    pub text: String,
}

/// The hover popover's *content* — what the core decides to show. The shell renders it
/// (parsing markdown into its widget model is presentation, so the parse lives there).
#[derive(Debug)]
pub enum HoverText {
    Blocks(Vec<HoverBlock>),
    Markdown(String),
}

/// What `Space o`'s blame → commit-info chain resolved to.
#[derive(Debug)]
pub enum CommitDetails {
    Info(Box<CommitInfo>),
    /// No popup — a transient note instead (uncommitted line, no blame, commit not found).
    Note(&'static str),
}

pub fn severity_label(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "Error",
        DiagnosticSeverity::Warning => "Warning",
        DiagnosticSeverity::Information => "Info",
        DiagnosticSeverity::Hint => "Hint",
    }
}
