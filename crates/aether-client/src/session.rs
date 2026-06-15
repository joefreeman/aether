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

/// A parked RPC result mapping (see [`Session::pending`]).
pub(crate) type PendingRpc = Box<
    dyn FnOnce(Result<serde_json::Value, super::transport::RpcError>) -> super::update::Event
        + Send,
>;

/// The session's connection lifecycle. The server is authoritative, so a dead socket just
/// freezes the window: the last buffer view stays rendered, editing input is suspended, and a
/// retry loop —
/// re-running discovery each attempt, since a restarted daemon gets a fresh port — rebuilds
/// the session when the server is back. On localhost the only real disconnect cause *is* a
/// daemon restart, so this is what makes "restart the daemon" seamless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Connected,
    /// Initial boot: no connection has ever been established yet — the client launched (possibly
    /// before the daemon) and is dialing. Distinct from [`Self::Reconnecting`] because there's no
    /// prior session to restore and nothing unsaved to lose, so the UI says "Connecting…" rather
    /// than "Reconnecting…". The shells render their boot backdrop in this state.
    Connecting,
    /// The socket died; a backoff retry is in flight. `had_unsaved` remembers whether edits
    /// were pending at disconnect — landing on a *restarted* daemon then means they're gone
    /// (buffers live in daemon memory), which warrants a warning.
    Reconnecting {
        attempt: u32,
        had_unsaved: bool,
    },
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
    /// The query value. Text editing (caret, insert, delete) is owned by each shell's search
    /// input, which syncs the whole value via [`super::update`]'s `search_set_query`.
    pub query: String,
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
    Confirm {
        /// Why we're asking — structured so each shell composes its own prompt text. The core
        /// states the reason; wording, punctuation and the `[y/N]` / Yes-No affordance are the
        /// shell's presentational choice.
        kind: ConfirmKind,
        action: ConfirmAction,
    },
    SaveAs {
        path_index: u32,
        /// The typed path value. Text editing (caret, insert, delete) is owned by each shell's
        /// input — native `text_input`/`<input>` in the rich clients, a shell-local editor in the
        /// TUI — which syncs the whole value here via [`super::update`]'s `prompt_set_input`. The
        /// core keeps only the value (for save / root-cycle) and the command keys (Enter/Esc/Tab).
        input: String,
    },
    /// LSP server detail (from the LspServers picker): info rows + `r` to restart.
    LspInfo(Box<LspServerStatus>),
}

/// A single editable text field. The project-settings overlay holds two (name + add-root). Text
/// editing (caret, insert, delete) is owned by each shell's input — native `text_input`/`<input>`
/// in the rich clients, a shell-local editor in the TUI — which syncs the whole value via
/// [`super::update`]'s `project_settings_set_name` / `_set_add`. The core keeps only the value.
#[derive(Debug, Clone, Default)]
pub struct TextField {
    pub text: String,
}

impl TextField {
    pub fn new(text: String) -> Self {
        TextField { text }
    }

    /// Replace the content wholesale.
    pub fn set(&mut self, text: String) {
        self.text = text;
    }

    pub fn clear(&mut self) {
        self.text.clear();
    }
}

/// The project-settings overlay state (`Space ,`), migrated from the TUI's shell-local
/// `ProjectSettingsState` into the core so every shell renders it. Shows an editable
/// project-name field, then the active project's roots, then an always-present "add root" input
/// row; `selected` is the focused field.
///
/// Selection model: `selected == 0` is the name field; `1..=roots.len()` are the root rows
/// (root `i` at index `i + 1`); `roots.len() + 1` is the add-root input row. The input row is
/// always reachable, which is why we focus it on open — most overlay opens are to add a root.
#[derive(Debug, Clone, Default)]
pub struct ProjectSettings {
    /// The project's *committed* name — the key used for root RPCs and the rename source.
    /// Updated only when a rename succeeds; `name` holds the in-progress edit.
    pub project_name: String,
    /// Editable buffer for the name field (index 0). Seeded from `project_name` on open;
    /// committed on blur (focus leaving the field) via `project/rename`.
    pub name: TextField,
    pub roots: Vec<String>,
    pub selected: usize,
    /// Text being typed into the add-root input row.
    pub add: TextField,
    /// In-dialog error from the last add/remove/rename attempt. Rendered as the bottom line of
    /// the overlay. Cleared when the user edits a field or initiates another action.
    pub error: Option<String>,
}

impl ProjectSettings {
    /// Selection index of the add-root input row (one past the last root).
    pub fn input_index(&self) -> usize {
        self.roots.len() + 1
    }

    pub fn on_name(&self) -> bool {
        self.selected == 0
    }

    pub fn on_input(&self) -> bool {
        self.selected == self.input_index()
    }

    /// The root under the current selection, when a root row is focused.
    pub fn selected_root(&self) -> Option<&String> {
        self.selected.checked_sub(1).and_then(|i| self.roots.get(i))
    }
}

/// Why a confirmation is being asked — the *reason*, carrying the data each shell needs to compose
/// its own prompt text. Presentation (wording, punctuation, the `[y/N]` vs Yes/No affordance) is
/// the shell's decision; the core only states the reason. Paired with a [`ConfirmAction`] (what
/// accepting does) inside [`Prompt::Confirm`].
#[derive(Debug, Clone)]
pub enum ConfirmKind {
    /// Saving would overwrite an existing file. `path` is the save-as relative path (`None` for an
    /// in-place save).
    Overwrite { path: Option<String> },
    /// The file changed on disk since it was loaded; saving overwrites those changes.
    OverwriteModified,
    /// The file was removed on disk since it was loaded; saving recreates it.
    RecreateDeleted,
    /// Reloading a buffer with unsaved changes.
    DiscardOnReload,
    /// Closing a buffer with unsaved changes. `label` is the buffer's display label.
    DiscardOnClose { label: String },
    /// Trashing a file/directory from the Files/Explorer picker. `noun` is "file"/"directory".
    Delete { noun: &'static str, name: String },
    /// Removing a root from the project-settings overlay.
    RemoveRoot { path: String },
    /// Deleting a project (its config) from the project switcher. Forgets the definition, not the
    /// files under its roots.
    DeleteProject { name: String },
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
    /// Trash a file/directory from the Files/Explorer picker (`path/delete`). `noun` is
    /// "file"/"directory" for the success toast; the still-open picker is re-listed after.
    DeletePath { path: String, noun: &'static str },
    /// Remove a root from the project-settings overlay (`project/remove_root`). Carries the
    /// committed project name and the root path so the request is self-contained — the overlay's
    /// selection may have moved (or the overlay closed) by the time the confirm resolves.
    RemoveProjectRoot { project: String, path: String },
    /// Delete a project (`project/delete`) from the switcher. The server refuses if it's active
    /// anywhere or has dirty buffers; the refreshed picker list rides a `picker/update` push.
    DeleteProject { name: String },
}

/// Outcome of a `buffer/save` attempt: saved, or refused pending user confirmation.
#[derive(Debug)]
pub enum SaveTry {
    Saved {
        result: BufferSaveResult,
        target: Option<(u32, String)>,
    },
    NeedsConfirm {
        kind: ConfirmKind,
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
    /// In-flight RPC result mappings, keyed by the token carried in `Effect::Request`.
    /// Each entry turns the raw JSON outcome into the [`Event`](super::update::Event) the
    /// request was for; `on_rpc_result` pops and runs it. Cleared on connection loss —
    /// results from a dead connection never arrive.
    pub(crate) pending_rpcs: std::collections::HashMap<u64, PendingRpc>,
    /// Token source for `Effect::Request`.
    pub(crate) next_token: u64,

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
    pub drag: Option<(LogicalPosition, Granularity)>,
    /// Cursor-line blame, rendered as dim text after the line: `(line, "author · age")`.
    pub blame: Option<(u32, String)>,
    /// The `(line, revision)` the in-flight/most-recent blame request was for.
    pub blame_requested: Option<(u32, u64)>,
    /// A modal confirm / save-as dialog; owns the keyboard while open.
    pub prompt: Option<Prompt>,
    /// An open picker overlay; owns the keyboard while open.
    pub picker: Option<PickerState>,
    /// The project-settings overlay (`Space ,`); owns the keyboard while open.
    pub project_settings: Option<ProjectSettings>,
    pub conn: ConnState,
    /// A content scroll anchor captured before a re-layout (wrap / diff toggle), so the view can be
    /// restored to the same content afterwards. Set by [`Session::capture_scroll_anchor`] and
    /// consumed by [`Session::resolve_scroll_anchor`]. See [`crate::grid::ScrollAnchor`].
    relayout_anchor: Option<crate::grid::ScrollAnchor>,
}

/// Tab stop width used for all cell math (mirrors the value the shells pass to the server on
/// subscribe). Single-sourced here so the anchor math agrees with the rendered layout.
pub const TAB_WIDTH: u32 = 4;

impl Session {
    pub fn new(project: String, project_paths: Vec<String>, buffer: BufferInfo) -> Self {
        Session {
            pending_rpcs: std::collections::HashMap::new(),
            next_token: 0,
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
            drag: None,
            blame: None,
            blame_requested: None,
            prompt: None,
            picker: None,
            project_settings: None,
            conn: ConnState::Connected,
            relayout_anchor: None,
        }
    }

    /// Capture a content scroll anchor for the current view, ahead of a wrap/diff re-layout. The
    /// shell supplies its current top visual row and viewport height (the only geometry the core
    /// lacks); the cursor and window come from the session. Pairs with [`resolve_scroll_anchor`].
    pub fn capture_scroll_anchor(&mut self, top_row: u32, viewport_rows: u32) {
        self.relayout_anchor = self.window.as_ref().map(|w| {
            crate::grid::capture_scroll_anchor(
                w,
                top_row,
                viewport_rows,
                self.buffer.cursor.position,
                TAB_WIDTH,
            )
        });
    }

    /// Consume the anchor captured by [`capture_scroll_anchor`] and resolve it against the current
    /// (post-relayout) window into a new absolute top visual row. `None` when no anchor is pending
    /// (so the shell falls back to its usual clamp + reveal-cursor).
    pub fn resolve_scroll_anchor(&mut self) -> Option<u32> {
        let anchor = self.relayout_anchor.take()?;
        let w = self.window.as_ref()?;
        Some(crate::grid::resolve_scroll_anchor(
            w,
            anchor,
            self.buffer.cursor.position,
            TAB_WIDTH,
        ))
    }

    /// The logical line the pending relayout anchor references — a re-subscribe (the TUI's wrap
    /// path) must load a window around it so [`resolve_scroll_anchor`] can place it. `None` when no
    /// anchor is pending.
    pub fn relayout_anchor_line(&self) -> Option<u32> {
        self.relayout_anchor
            .map(|a| a.reference_line(self.buffer.cursor.position))
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

    /// A boot placeholder ([`Session::placeholder`]): no project activated and no real buffer
    /// (the sentinel `buffer_id == 0`, which the server never assigns). Shells render their
    /// no-project view — no editor, no viewport subscribe — until a project is picked and
    /// [`Session::adopt_switch`](crate::update) lands the first real buffer.
    pub fn is_placeholder(&self) -> bool {
        self.buffer.buffer_id == 0
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

/// The hover popover's *content* — what the core decides to show. Markdown is parsed to a shared
/// AST here (in the core) so every shell renders the same structure rather than re-parsing.
#[derive(Debug)]
pub enum HoverText {
    Blocks(Vec<HoverBlock>),
    Markdown(Vec<crate::markdown::Block>),
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
