//! Application state and event loop. Modal editing (Normal vs Insert) lives entirely here; the
//! server has no notion of mode.

use crate::client::Client;
use crate::clipboard;
use crate::keymap::{self, Action, InsertWhere, ScrollDir, ScrollUnit};
use crate::text_input::PromptKeyOutcome;
use crate::ui;
use aether_protocol::buffer::{
    BufferClose, BufferCloseParams, BufferClosed, BufferClosedParams, BufferCopy, BufferCopyParams,
    BufferCopyResult, BufferCut,
    BufferCutResult, BufferOpen, BufferOpenParams, BufferOpenResult, BufferReload,
    BufferReloadParams, BufferSave, BufferSaveParams, BufferState, BufferStateParams, CopyScope,
};
use aether_protocol::cursor::{
    CursorBufferOnlyParams, CursorContract, CursorExpand, CursorMove, CursorMoveParams, CursorRedo,
    CursorSelectLine, CursorSelectLineParams, CursorSet, CursorSetParams, CursorState,
    CursorSwapAnchor, CursorSwapAnchorParams, CursorUndo, CursorUndoParams, CursorUndoResult,
    Direction, Motion, VerticalDirection,
};
use aether_protocol::envelope::{ClientInbound, NotificationMethod};
use aether_protocol::error::ErrorCode;
use aether_protocol::git::{
    BlameInfo, GitBlameLine, GitBlameLineParams, GitNavigateHunk, GitNavigateHunkParams,
    GitSetDiffView, GitSetDiffViewParams, HunkDirection,
};
use aether_protocol::nav::{
    NavBack, NavForward, NavRecord, NavRecordParams, NavStepParams, NavStepResult,
};
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputBackspace, InputDedent, InputDelete, InputIndent,
    InputJoinLines, InputMoveLines, InputMoveLinesParams, InputNewlineAndIndent, InputRedo,
    InputSurroundParams, InputText, InputTextParams, InputToggleComment, InputUndo,
    InputUnsurroundParams, SurroundTarget, UndoResult,
};
use aether_protocol::lsp::{
    DiagnosticCounts, DiagnosticDirection, FormatStatus, LspDiagnosticsChanged,
    LspDiagnosticsChangedParams, LspFormat, LspNavigateDiagnostic, LspNavigateDiagnosticParams,
    LspRestartServer, LspRestartServerParams, LspServerRef, LspServerStatus, LspStatusChanged,
};
use aether_protocol::picker::{
    PickerGrepNavigate, PickerGrepNavigateParams, PickerHide, PickerHideParams, PickerItem,
    PickerKind, PickerQuery, PickerQueryParams, PickerSelect, PickerSelectParams,
    PickerSelectResult, PickerUpdate, PickerUpdateParams, PickerView, PickerViewParams,
};
use aether_protocol::project::{ProjectActivate, ProjectActivateParams, ProjectActivateResult};
use aether_protocol::search::{
    SearchClear, SearchClearParams, SearchNavParams, SearchNext, SearchPrev, SearchSet,
    SearchSetParams, SearchStateChanged, SearchSummary,
};
use aether_protocol::viewport::{
    DiagnosticSeverity, LogicalLineRender, ScrollPosition, ViewportLinesChanged,
    ViewportLinesChangedParams,
    ViewportResize, ViewportResizeParams, ViewportScroll, ViewportScrollParams, ViewportSetWrap,
    ViewportSetWrapParams, ViewportSubscribe, ViewportSubscribeParams, ViewportSubscribeResult,
    WrapMode,
};
use aether_protocol::{BufferId, LogicalPosition, ViewportId};
use anyhow::{Context, Result};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{stdout, Stdout};
use tokio::sync::mpsc;

/// Editor's modal-edit state — toggled by the user's keybindings (`i` enters Insert, `Esc`
/// returns to Normal, etc.). Lives entirely client-side; the server has no notion of mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorMode {
    #[default]
    Normal,
    Insert,
    Search,
}

/// Multi-key prefixes the next keystroke completes. `Space` is the only one for now (used by
/// the `Space f` / `Space b` picker bindings) — adding more is "add a variant and a match arm
/// in `handle_leader_key`".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingLeader {
    Space,
}

/// Captured state for a pending `f`/`t` keystroke — the next char the user types becomes the
/// target of a `Motion::FindChar`.
#[derive(Debug, Clone, Copy)]
pub struct PendingFind {
    pub direction: Direction,
    pub till: bool,
    pub extend: bool,
    pub count: u32,
}

/// What `r`/`Shift-r` replays. The repeat unit is the binding *intent*, not a resolved `Motion`:
/// most actions are remembered as the `Action` (plus the `count` they were issued with) and replayed
/// straight back through `run_action`, which reconstructs the motion against live state each time.
/// The one exception is find-char: `BeginFind` only arms a capture, so the actual target char isn't
/// known until the next keystroke — that resolved `Motion::FindChar` is stored directly.
#[derive(Debug, Clone)]
pub enum RepeatTarget {
    /// Re-run this binding via `run_action(action, count, extend)`. Covers every repeatable action
    /// except find — see [`Action::is_repeatable`].
    Action { action: Action, count: u32 },
    /// Replay a resolved `f`/`t` find (the target char is baked in). Recorded when the char is typed.
    Find(Motion),
}

/// Client-side mirror of the server's search state. The server owns the match list (and pushes
/// per-line highlights via viewport line renders); the client just tracks the query, the latest
/// summary, the history list, and the snapshot used to revert from EditorMode::Search via Esc.
#[derive(Debug, Default)]
pub struct SearchState {
    /// The current query — live while in EditorMode::Search, the committed query otherwise.
    pub query: crate::text_input::TextInput,
    /// True when there is a committed search on the server (set via `search/set` with a non-empty
    /// query and not later cleared). Used to gate highlighting and the `n`/`Alt-n` bindings.
    pub active: bool,
    /// Server-pushed summary (total, truncated, current_index). `None` before any search runs.
    pub summary: Option<SearchSummary>,
    /// Snapshot of pre-search-mode state, used by Esc to revert.
    pub snapshot: Option<SearchSnapshot>,
    /// Committed queries, oldest first. Up/Down in EditorMode::Search browses this; `n`/`Alt-n` with
    /// no active search re-activates the most recent entry.
    pub history: Vec<String>,
    /// `None` while the user is typing a fresh query; `Some(i)` while they're browsing the entry
    /// at `history[i]`. Any edit (typing/backspace) snaps back to `None`.
    pub history_cursor: Option<usize>,
    /// The live-typed query, stashed when the user steps into history with Up so that Down can
    /// restore it on the way back out.
    pub history_draft: String,
    /// True while in a `?`-initiated search: instead of re-selecting just the matched word, each
    /// incremental match grows the selection from where `?` was pressed (the snapshot anchor) to the
    /// match. Reset on every entry into search mode and on commit/abort.
    pub extend_to_cursor: bool,
}

#[derive(Debug)]
pub struct SearchSnapshot {
    pub cursor: CursorState,
    pub scroll_logical_line: u32,
    pub query: String,
    pub active: bool,
}

/// Severity tag on a transient status-row message. Drives the colour the renderer uses so the
/// user can read the meaning of a message at a glance — success in blue (matching the
/// committed-prefix palette), warnings in yellow, errors in red, neutral info in the default
/// foreground. Default = `Info` so an unconfigured default-constructed [`StatusMessage`] reads
/// as plain text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StatusKind {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

/// Status-row message body + its kind. Use the `info`/`success`/`warning`/`error` constructors
/// at call sites — those name the semantic so a quick scan of the code reveals what the
/// message means without having to read the text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusMessage {
    pub text: String,
    pub kind: StatusKind,
}

impl StatusMessage {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Info,
        }
    }
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Success,
        }
    }
    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Warning,
        }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Error,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

impl std::fmt::Display for StatusMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Top-level UI state. Anything that exists regardless of whether a buffer is open lives on
/// `AppState`; anything that's per-screen (editor vs file browser) lives inside `Screen`.
///
/// Overlays — the picker and the save-as prompt — sit on top of either screen as `Option`s on
/// `AppState`. They don't change which screen is underneath, so opening/closing them needs no
/// "return mode" bookkeeping.
pub struct AppState {
    /// Active project name. Empty string before a project is activated — the no-project view
    /// shows the project picker instead of the editor in that state.
    pub project_name: String,
    /// Active project's root paths (absolute, server-canonical). Empty before activation.
    pub project_paths: Vec<String>,
    /// One disambiguated label per entry in `project_paths`, aligned by index. Computed by
    /// `labels::root_labels` and refreshed via `refresh_root_labels` whenever `project_paths`
    /// changes. Used for UI rendering (status bar, picker prefixes, explorer breadcrumb) — the
    /// protocol is unaware.
    pub root_labels: Vec<String>,
    pub viewport_cols: u32,
    pub viewport_rows: u32,
    pub should_quit: bool,
    /// Transient feedback rendered on the status row. Carries a `StatusKind` so the renderer
    /// can colour the message — "saved" reads as success, "save failed" reads as error, etc.
    /// Constructed via `StatusMessage::info` / `::success` / `::warning` / `::error`.
    pub status: StatusMessage,
    /// Mirror of the most recently emitted terminal-title escape sequence. We only re-emit
    /// when [`terminal_title`] derives something different, so frame-loop draws don't spam OSC
    /// sequences down stdout. Empty string at startup; populated on the first render.
    pub last_terminal_title: String,
    /// System clipboard handle. Held for the app's lifetime so the X11 selection isn't
    /// abandoned every operation. `None` if the clipboard couldn't be initialised (e.g. headless).
    pub clipboard: Option<arboard::Clipboard>,
    /// Multi-key chord state. `Some(Space)` after the user pressed the leader key; consumed by
    /// the next keystroke. Cleared in any other code path that decides the leader doesn't apply.
    pub pending_leader: Option<PendingLeader>,
    pub picker: crate::picker::PickerState,
    /// Active save-as prompt. Overlays on top of the buffer; bound to the active editor.
    pub save_prompt: Option<SavePromptState>,
    /// Active binary y/N confirmation prompt. Layers on top of any other overlay (including
    /// `save_prompt`, e.g. for the save-as overwrite confirm). Holds the question text and the
    /// action to run on `y`.
    pub confirm_prompt: Option<ConfirmPrompt>,
    /// `None` before a project is activated, or transiently while switching. Most key handlers
    /// early-return without touching state in that case; the no-project view (project picker)
    /// is rendered instead by `ui::draw`.
    pub editor: Option<EditorState>,
    /// Active project-settings overlay (`Space ,`). When `Some`, draws a centered modal listing
    /// the project's roots, with a permanent add-root input row at the bottom. Closed by Esc.
    pub project_settings: Option<ProjectSettingsState>,
    /// Keyboard-shortcut help overlay (`Space ?`). A read-only, client-local cheatsheet generated
    /// from the `keymap` tables — no server round-trip. Closed by Esc.
    pub help: HelpState,
    /// Latest language-server status per server, keyed by `(language, workspace_root)`, from
    /// `lsp/status_changed`. Drives the status-bar health indicator for the current buffer's
    /// server (keyed this way so several same-language servers don't collide). Persists for the
    /// session.
    pub lsp_status: std::collections::HashMap<(String, String), LspServerStatus>,
    /// Active hover popup (`Space k` hover / `Space j` diagnostic). `Some` shows a floating box; scroll keys / the
    /// wheel pan it, Esc or any other key dismisses it.
    pub hover: Option<HoverPopup>,
    /// Per-buffer diagnostic counts from `lsp/diagnostics_changed`, for the status-bar summary.
    pub diagnostic_counts: std::collections::HashMap<BufferId, DiagnosticCounts>,
    /// Set by a `buffer/closed` push — another client (or a path/project deletion) closed the
    /// buffer we have open. Drained by `flush_pending_external_close` in the main loop, which
    /// switches us to the indicated next buffer (or a scratch). Can't act here: `apply_notification`
    /// is synchronous and switching needs an RPC.
    pub pending_external_close: Option<BufferClosedParams>,
}

/// One block of hover-popup content. `severity` colors the block to match the gutter dot (for
/// diagnostics); `None` renders plain (for LSP hover text).
pub struct HoverBlock {
    pub text: String,
    pub severity: Option<DiagnosticSeverity>,
}

/// A showing hover/diagnostic popup: its content blocks plus the scroll offset within it (the box
/// caps its height and scrolls when the content is taller).
pub struct HoverPopup {
    pub blocks: Vec<HoverBlock>,
    pub scroll: crate::scroll::ScrollState,
}

impl HoverPopup {
    /// A plain, uncolored popup — for LSP hover (type signature + docs).
    pub fn plain(text: String) -> Self {
        Self::from_blocks(vec![HoverBlock {
            text,
            severity: None,
        }])
    }

    pub fn from_blocks(blocks: Vec<HoverBlock>) -> Self {
        Self {
            blocks,
            scroll: crate::scroll::ScrollState::default(),
        }
    }
}

/// State for the keyboard-shortcut help overlay. Open/closed, the selected tab, and a scroll
/// position; the content is generated on the fly from the `keymap` binding tables, so there's
/// nothing to cache here.
#[derive(Debug, Default)]
pub struct HelpState {
    pub open: bool,
    pub tab: HelpTab,
    pub scroll: crate::scroll::ScrollState,
}

/// Which tab the help overlay is showing. The tabs mirror the key-dispatch layers: one per editor
/// mode (`Normal`/`Insert`/`Search`), plus `Application` for the `Space`-leader chords. The shared
/// `Ctrl-` editing keys (the `Global` table) are surfaced on both the Normal and Insert tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HelpTab {
    #[default]
    Normal,
    Insert,
    Search,
    Application,
}

impl HelpTab {
    /// Tabs in left-to-right display order — the order `h`/`l` cycles through.
    pub const ALL: [HelpTab; 4] = [
        HelpTab::Normal,
        HelpTab::Insert,
        HelpTab::Search,
        HelpTab::Application,
    ];

    /// Label shown in the overlay's tab bar.
    pub fn label(self) -> &'static str {
        match self {
            HelpTab::Normal => "Normal",
            HelpTab::Insert => "Insert",
            HelpTab::Search => "Search",
            HelpTab::Application => "Application",
        }
    }

    /// Step `delta` tabs along [`HelpTab::ALL`], wrapping at both ends.
    fn step(self, delta: isize) -> HelpTab {
        let n = HelpTab::ALL.len() as isize;
        let i = HelpTab::ALL.iter().position(|t| *t == self).unwrap_or(0) as isize;
        HelpTab::ALL[(((i + delta) % n + n) % n) as usize]
    }

    /// The next tab to the right (wraps around to the first).
    pub fn next(self) -> HelpTab {
        self.step(1)
    }

    /// The previous tab to the left (wraps around to the last).
    pub fn prev(self) -> HelpTab {
        self.step(-1)
    }
}

/// Project-settings overlay. Shows an editable project-name field, then the active project's
/// roots, then an always-present "add root" input row at the bottom; `selected` is the focused
/// field. Source of truth for `roots` (and the committed `project_name`) is the server (synced
/// via `sync_project_paths` and the rename RPC).
///
/// Selection model: `selected == 0` is the name field; `1..=roots.len()` are the root rows (root
/// `i` at index `i + 1`); `roots.len() + 1` is the add-root input row. The input row is always
/// reachable, which is why we focus it on open — most overlay opens are to add a root.
#[derive(Debug, Clone, Default)]
pub struct ProjectSettingsState {
    /// The project's *committed* name — the key used for root RPCs and the rename source. Updated
    /// only when a rename succeeds; `name_input` holds the in-progress edit.
    pub project_name: String,
    /// Editable buffer for the name field (index 0). Seeded from `project_name` on open;
    /// committed on blur (focus leaving the field) via `project/rename`.
    pub name_input: crate::text_input::TextInput,
    pub roots: Vec<String>,
    pub selected: usize,
    /// Text being typed into the add-root input row.
    pub add_input: crate::text_input::TextInput,
    /// In-dialog error from the last add or remove attempt. Rendered as the bottom line of the
    /// overlay. Cleared when the user edits `add_input` or initiates another action.
    pub error: Option<String>,
    /// `true` when a delete is awaiting confirmation on the currently-selected root row. The row
    /// renders as `Remove "<path>"? [y/N]`; key handling is restricted to confirm/cancel until
    /// resolved (so a stray arrow press can't silently drop the pending state). The pending row
    /// is always `selected` — there's no separate index because navigation is locked while this
    /// is set.
    pub pending_delete: bool,
}

/// Generic `[y/N]` confirmation overlay. The save-as overwrite confirm and the close-with-
/// unsaved-changes confirm both route through this. Esc / Enter / `n` decline (matching the
/// uppercase `N` default); only `y` / `Y` proceeds.
#[derive(Debug, Clone)]
pub struct ConfirmPrompt {
    /// What gets shown on the status row, formatted as `" {message}? [y/N]"`.
    pub message: String,
    pub action: ConfirmAction,
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    /// Retry the save-as RPC with `overwrite: true`. The path is read from the open save-prompt
    /// (via `SavePromptState::save_target`) — the prompt stays open beneath the confirm — so
    /// nothing is carried in this variant.
    OverwriteSaveAs,
    /// Close `buffer_id` despite it being dirty. After closing, the client picks the next
    /// MRU buffer or spawns a scratch.
    CloseBuffer { buffer_id: BufferId },
    /// Retry `Ctrl-s` (in-place save) with `overwrite: true` after the server reported the
    /// file changed or was removed on disk. Routes through `save_buffer_force`.
    OverwriteExternalChange,
    /// Retry `buffer/reload` with `force: true` after the server reported the buffer was
    /// dirty. The user has accepted that the local edits will be discarded.
    ReloadDiscardChanges,
}

pub struct EditorState {
    pub mode: EditorMode,
    pub buffer_id: BufferId,
    pub viewport_id: ViewportId,
    pub cursor: CursorState,
    pub scroll_logical_line: u32,
    /// Visual rows of the top logical line (`scroll_logical_line`) hidden above the viewport — the
    /// fractional part of the scroll position. Lets scrolling advance by *visual* rows (so it
    /// doesn't jump over a wrapped line's rows or a diff hunk's phantom deleted rows) while the
    /// server stays logical-line based. Always `< ` the top line's visual height; reset to 0 by
    /// any logical-line-aligned scroll (cursor jumps, window refetches).
    pub scroll_skip_rows: u32,
    pub window_first_logical_line: u32,
    pub lines: Vec<LogicalLineRender>,
    /// Total logical lines in the buffer, kept fresh from every viewport response /
    /// `viewport/lines_changed` notification.
    pub line_count: u32,
    /// Highest legal `scroll_logical_line` — server-computed so it accounts for wrap, putting
    /// the buffer's last visual row at the bottom of the viewport.
    pub max_scroll_logical_line: u32,
    pub wrap: WrapMode,
    /// Inline diff view toggle. Server-authoritative (per-viewport); mirrored here so the
    /// keybinding can flip it. When on, the server interleaves phantom "deleted" rows into the
    /// pushed window.
    pub diff_view: bool,
    /// Horizontal scroll, in bytes. Only meaningful when `wrap == WrapMode::None`; reset to 0
    /// when soft wrap is on (wrapped content never overflows). Client-only.
    pub scroll_col: u32,
    /// Accumulated vertical-scroll delta from arrow-key / PageUp-PageDown bursts. Deferred
    /// to a coalesced `viewport/scroll` RPC at draw time.
    pub pending_scroll_lines: i64,
    /// Anchor position set by a left-mouse-button down. Subsequent drags use it as the
    /// selection anchor; cleared on mouse-up.
    pub drag_anchor: Option<LogicalPosition>,
    pub revision: u64,
    /// Revision at the most recent successful save. `dirty` is derived as
    /// `revision != saved_revision`.
    pub saved_revision: u64,
    /// Set when the server's file-watcher detected a disk change while this buffer was dirty
    /// (clean buffers reload silently). The user must `Ctrl-s` (and confirm overwrite) or
    /// `buffer/reload` to clear it. Updated from `BufferState` notifications.
    pub externally_modified: bool,
    /// Set when the server's file-watcher detected the buffer's file was removed on disk.
    /// Cleared by a save (which recreates the file) or by the file being recreated externally.
    pub externally_deleted: bool,
    /// Digit-prefix count for the next motion. Reset after consumption.
    pub pending_count: u32,
    /// Set after `f`/`t`/`F`/`T` (and Alt variants); the next keystroke is interpreted as the
    /// target character rather than a normal-mode binding.
    pub pending_find: Option<PendingFind>,
    /// Set after `Ctrl-s`; the next keystroke names the delimiter to surround with rather than a
    /// binding. The value records the target (selection in Normal, line in Insert). Mirrors
    /// `pending_find`'s next-key-is-data capture.
    pub pending_surround: Option<SurroundTarget>,
    /// The most recent repeatable action, replayed by `r` (no extend) or `Shift-r` (extend the
    /// selection with the replayed motion). See [`RepeatTarget`].
    pub last_repeat: Option<RepeatTarget>,
    pub search: SearchState,
    /// Git blame for the cursor's line, shown as end-of-line virtual text in Normal mode. Lazily
    /// fetched (see [`refresh_blame`]) when the cursor changes line or the buffer revision
    /// changes; never fetched in Insert mode so typing doesn't thrash the server's blame cache.
    pub blame: BlameState,
    /// Canonical absolute path of this buffer's file on disk, if any.
    pub file_path: Option<String>,
    pub file_label: String,
    /// The buffer's language id (e.g. `"rust"`), from `buffer/open`. `None` for unknown/plain-text
    /// buffers. Used for language-scoped UI (e.g. the "no formatter for {lang}" note).
    pub language: Option<String>,
    /// The language server backing this buffer, from `buffer/open` — its `(language,
    /// workspace_root)` key. `None` when no server is attached. Selects *which* server's health the
    /// status bar shows (language alone is ambiguous when a project runs several same-language
    /// servers at different roots).
    pub lsp_server: Option<LspServerRef>,
}

/// Client-side cache of the cursor line's blame. `key` records the `(line, revision)` the `info`
/// was fetched for, so a stale entry is never shown for the wrong line and we skip refetching
/// when nothing relevant changed.
#[derive(Default)]
pub struct BlameState {
    pub key: Option<(u32, u64)>,
    pub info: Option<BlameInfo>,
}

pub use crate::save_prompt::SavePromptState;

impl EditorState {
    /// Mirror of the status bar's dirty indicator. `true` when the buffer has unsaved local
    /// changes (`revision != saved_revision`) or the server flagged it as externally-changed
    /// in a way the user needs to address.
    pub fn dirty_marker_visible(&self) -> bool {
        self.revision != self.saved_revision || self.externally_modified || self.externally_deleted
    }
}

impl AppState {
    /// `true` while a buffer is open. False during the no-project view and the brief window
    /// between switching projects (old editor torn down, new one not yet built).
    pub fn has_editor(&self) -> bool {
        self.editor.is_some()
    }

    /// Read access to the active editor. Panics if called without an active editor — handlers
    /// that touch it must gate on [`Self::has_editor`] (the dispatch layer does this for the
    /// editor-bound key handlers).
    pub fn ed(&self) -> &EditorState {
        self.editor
            .as_ref()
            .expect("BUG: AppState::ed() called without an active editor")
    }

    /// Mutable access to the active editor. Same gating contract as [`Self::ed`].
    pub fn ed_mut(&mut self) -> &mut EditorState {
        self.editor
            .as_mut()
            .expect("BUG: AppState::ed_mut() called without an active editor")
    }

    /// Single-character buffer-state marker, mirroring the status row:
    ///   `[x]` — file removed on disk; `[!]` — file modified on disk; `[+]` — unsaved local
    /// edits; `""` — clean. Highest-precedence wins so the user always sees the most urgent
    /// flag. Empty when no editor is attached.
    pub fn buffer_status_marker(&self) -> &'static str {
        if !self.has_editor() {
            return "";
        }
        let ed = self.ed();
        if ed.externally_deleted {
            "[x]"
        } else if ed.externally_modified {
            "[!]"
        } else if ed.revision != ed.saved_revision {
            "[+]"
        } else {
            ""
        }
    }
}

pub async fn bootstrap(
    client: &mut Client,
    project_name: Option<&str>,
    file: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<AppState> {
    let viewport_rows = rows.saturating_sub(1) as u32;
    // Reserve the gutter column up front so the content width (and everything derived from it,
    // including the cols we report to the server) accounts for it.
    let viewport_cols = (cols as u32).saturating_sub(crate::ui::GUTTER_WIDTH as u32);

    // No project named on the CLI: return an empty AppState with the Projects picker open. The
    // event loop will draw the picker and accept input until the user activates a project.
    let Some(project_name) = project_name else {
        let mut state = empty_app_state(viewport_cols, viewport_rows);
        open_picker(client, &mut state, PickerKind::Projects).await?;
        return Ok(state);
    };
    let activated: ProjectActivateResult = client
        .rpc::<ProjectActivate>(ProjectActivateParams {
            name: project_name.to_string(),
        })
        .await?;
    let project_paths = activated.project.paths.clone();

    // Classify the file arg: file → open it; directory → open scratch + auto-show the Explorer
    // popup pointed at that directory; outside every project root → error. Relative paths are
    // resolved against the *current working directory* (shell-conventional), not blindly joined
    // with root 0 — in a multi-root project the user may well be in a sub-tree of root 1.
    let (open_file, explorer_dir): (Option<(u32, String)>, Option<String>) = match file {
        None => (None, None),
        Some(f) => {
            let abs = resolve_cli_path(f)?;
            if abs.is_dir() {
                (None, Some(abs.display().to_string()))
            } else {
                let abs_str = abs.display().to_string();
                let (path_index, relative_path) = strip_longest_root(&abs_str, &project_paths)
                    .map(|(i, r)| (i as u32, r))
                    .ok_or_else(|| {
                        anyhow::anyhow!("{} is outside the project's roots", abs.display())
                    })?;
                (Some((path_index, relative_path)), None)
            }
        }
    };

    // Pick what to land on:
    //   1. Explicit file arg → open it.
    //   2. No file arg + the project has a most-recent buffer (from a prior session) → attach
    //      to that buffer rather than spawning a fresh scratch every launch.
    //   3. Otherwise → fresh scratch.
    let root_labels = crate::labels::root_labels(&project_paths);
    let editor = match open_file {
        Some((path_index, relative_path)) => {
            open_buffer_and_subscribe(
                client,
                viewport_cols,
                viewport_rows,
                &project_paths,
                &root_labels,
                aether_protocol::buffer::BufferOpenParams {
                    buffer_id: None,
                    path_index: Some(path_index),
                    relative_path: Some(relative_path),
                    language: None,
                    create_if_missing: false,
                    jump_to: None,
                },
            )
            .await?
        }
        None => {
            open_buffer_and_subscribe(
                client,
                viewport_cols,
                viewport_rows,
                &project_paths,
                &root_labels,
                aether_protocol::buffer::BufferOpenParams {
                    buffer_id: activated.last_buffer_id,
                    path_index: None,
                    relative_path: None,
                    language: None,
                    create_if_missing: false,
                    jump_to: None,
                },
            )
            .await?
        }
    };

    let mut state = AppState {
        project_name: activated.project.name,
        project_paths,
        root_labels,
        viewport_cols,
        viewport_rows,
        should_quit: false,
        status: StatusMessage::default(),
        last_terminal_title: String::new(),
        clipboard: clipboard::new_handle(),
        pending_leader: None,
        picker: crate::picker::PickerState::default(),
        save_prompt: None,
        confirm_prompt: None,
        editor: Some(editor),
        project_settings: None,
        help: HelpState::default(),
        lsp_status: std::collections::HashMap::new(),
        hover: None,
        diagnostic_counts: std::collections::HashMap::new(),
        pending_external_close: None,
    };

    // Seed the Explorer picker's remembered directory before the auto-open so it lists the
    // right place. `open_picker` would otherwise compute a default (parent of current file →
    // None for scratch, falling back to first project root) which is usually the same answer,
    // but for a `aether some/dir/` invocation we want the picker pointed at that dir.
    if let Some(dir) = explorer_dir {
        state.picker.explorer_dir = Some(dir);
        open_picker(client, &mut state, PickerKind::Explorer).await?;
    }

    Ok(state)
}

/// AppState with no project active. The editor field is `None`; project name/paths are empty.
/// Used by [`bootstrap`] when no `--project` was given, and as the carrier for the project
/// picker overlay before activation.
fn empty_app_state(viewport_cols: u32, viewport_rows: u32) -> AppState {
    AppState {
        project_name: String::new(),
        project_paths: Vec::new(),
        root_labels: Vec::new(),
        viewport_cols,
        viewport_rows,
        should_quit: false,
        status: StatusMessage::default(),
        last_terminal_title: String::new(),
        clipboard: clipboard::new_handle(),
        pending_leader: None,
        picker: crate::picker::PickerState::default(),
        save_prompt: None,
        confirm_prompt: None,
        editor: None,
        project_settings: None,
        help: HelpState::default(),
        lsp_status: std::collections::HashMap::new(),
        hover: None,
        diagnostic_counts: std::collections::HashMap::new(),
        pending_external_close: None,
    }
}

/// Activate `project_name`. Selecting the currently active project is a no-op (just closes the
/// picker). For a different project, sends `project/activate` and either reattaches to the
/// client's most-recently-used buffer in that project (returned in the response) or, if there's
/// no prior history, spawns a fresh scratch buffer so the user always lands in *some* editor.
async fn activate_project_and_rebuild_editor(
    client: &mut Client,
    state: &mut AppState,
    project_name: &str,
) -> Result<()> {
    state.picker.open = false;

    // Same-project re-select: nothing to tear down or rebuild. The picker closure above is the
    // entire effect.
    if project_name == state.project_name && state.has_editor() {
        state.status = StatusMessage::info(format!("already in project {project_name}"));
        return Ok(());
    }

    // Drop the local editor handle so we don't push notifications at a viewport the server is
    // about to tear down. `project/activate` does the server-side teardown for us.
    state.editor = None;

    let activated: ProjectActivateResult = client
        .rpc::<ProjectActivate>(ProjectActivateParams {
            name: project_name.to_string(),
        })
        .await?;
    state.project_name = activated.project.name;
    state.project_paths = activated.project.paths;
    refresh_root_labels(state);

    // Reattach to the last buffer the user had open in this project, if any. The server's MRU
    // survives project switches, so coming back to a project drops you on the buffer you left.
    // First visit (or every prior buffer is gone) → spawn a scratch.
    let open_params = aether_protocol::buffer::BufferOpenParams {
        buffer_id: activated.last_buffer_id,
        path_index: None,
        relative_path: None,
        language: None,
        create_if_missing: false,
        jump_to: None,
    };
    let project_paths = state.project_paths.clone();
    let root_labels = state.root_labels.clone();
    let editor = open_buffer_and_subscribe(
        client,
        state.viewport_cols,
        state.viewport_rows,
        &project_paths,
        &root_labels,
        open_params,
    )
    .await?;
    state.editor = Some(editor);
    state.status = StatusMessage::success(format!("activated project {}", state.project_name));
    // The picker was open when we entered (cursor = bar); now an editor's attached in normal
    // mode, so the cursor needs to flip back to block. Without this, the bar persists after the
    // switch and looks like we're stuck in insert mode.
    apply_cursor_style(state);
    Ok(())
}

/// Construct an `EditorState` by running `buffer/open` followed by `viewport/subscribe`. Used
/// by bootstrap only — runtime buffer switches start from a pre-resolved `BufferOpenResult`
/// (held by the caller for status reporting) and go through `subscribe_to_buffer`.
async fn open_buffer_and_subscribe(
    client: &mut Client,
    viewport_cols: u32,
    viewport_rows: u32,
    project_paths: &[String],
    root_labels: &[String],
    open_params: aether_protocol::buffer::BufferOpenParams,
) -> Result<EditorState> {
    let open: BufferOpenResult = client
        .rpc::<aether_protocol::buffer::BufferOpen>(open_params)
        .await?;
    build_editor_state_from_open(
        client,
        viewport_cols,
        viewport_rows,
        project_paths,
        root_labels,
        open,
        WrapMode::Soft,
    )
    .await
}

/// Subscribe a fresh viewport for `open.buffer_id` and build the `EditorState` describing it.
/// Shared by bootstrap and by `subscribe_to_buffer`; pure construction with no side effects
/// on the broader `AppState`. The caller picks `wrap` (bootstrap defaults to Soft; runtime
/// buffer switches inherit the prior editor's setting so the user's wrap toggle is sticky).
async fn build_editor_state_from_open(
    client: &mut Client,
    viewport_cols: u32,
    viewport_rows: u32,
    project_paths: &[String],
    root_labels: &[String],
    open: BufferOpenResult,
    wrap: WrapMode,
) -> Result<EditorState> {
    // Initial scroll: prefer a restored value (a buffer we'd seen before), otherwise centre
    // the viewport on the cursor. For a default cursor at (0,0) that's still line 0; for a
    // `buffer/open { jump_to }` open (e.g. a grep-picker selection landing in a fresh file),
    // this puts the jump destination on-screen instead of leaving the viewport stuck at the
    // top with the cursor off below.
    let initial_scroll = open.scroll.unwrap_or_else(|| {
        let half = viewport_rows / 2;
        ScrollPosition {
            logical_line: open.cursor.position.line.saturating_sub(half),
            sub_row: 0.0,
        }
    });
    let sub: ViewportSubscribeResult = client
        .rpc::<ViewportSubscribe>(ViewportSubscribeParams {
            buffer_id: open.buffer_id,
            cols: viewport_cols,
            rows: viewport_rows,
            overscan_rows: viewport_rows,
            scroll: initial_scroll,
            wrap,
            continuation_marker_width: ui::CONTINUATION_MARKER_WIDTH,
            tab_width: ui::TAB_WIDTH,
        })
        .await?;
    let file_label = match open.path.as_deref() {
        Some(p) => project_relative_label(p, project_paths, root_labels),
        None => format!(
            "(scratch {})",
            open.scratch_number.map(u64::from).unwrap_or(open.buffer_id)
        ),
    };
    Ok(EditorState {
        mode: EditorMode::Normal,
        buffer_id: open.buffer_id,
        viewport_id: sub.viewport_id,
        cursor: open.cursor,
        scroll_logical_line: initial_scroll.logical_line,
        scroll_skip_rows: 0,
        window_first_logical_line: sub.window.first_logical_line,
        lines: sub.window.lines,
        line_count: sub.window.line_count,
        max_scroll_logical_line: sub.window.max_scroll_logical_line,
        wrap,
        diff_view: false,
        scroll_col: 0,
        pending_scroll_lines: 0,
        drag_anchor: None,
        revision: open.revision,
        saved_revision: open.saved_revision,
        externally_modified: false,
        externally_deleted: false,
        pending_count: 0,
        pending_find: None,
        pending_surround: None,
        last_repeat: None,
        search: SearchState::default(),
        blame: BlameState::default(),
        file_path: open.path,
        file_label,
        language: open.language,
        lsp_server: open.lsp_server,
    })
}

pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: &mut Client,
    state: &mut AppState,
) -> Result<()> {
    // Background task forwards events into a channel. Doing it this way (rather than awaiting
    // `EventStream::next` directly in the main `select!`) means we can use `try_recv` to drain
    // backlogged events between draws — `tokio::sync::mpsc` supports non-blocking recv natively,
    // whereas `now_or_never` on the EventStream future leaves the stream in a state where later
    // events don't wake the task. A trackpad scroll burst now coalesces into one redraw.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<std::io::Result<Event>>();
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(ev) = events.next().await {
            if event_tx.send(ev).is_err() {
                break; // main loop dropped the receiver; we're shutting down
            }
        }
    });

    apply_cursor_style(state);
    terminal.draw(|f| ui::draw(f, state))?;
    refresh_terminal_title(state);
    while !state.should_quit {
        tokio::select! {
            ev = event_rx.recv() => {
                let Some(ev) = ev else { break };
                let ev = ev?;
                dispatch_terminal_event(client, state, ev).await?;
                // Drain any other events that piled up while we were dispatching. mpsc's
                // try_recv is non-blocking and safe to call repeatedly.
                while !state.should_quit {
                    match event_rx.try_recv() {
                        Ok(ev) => dispatch_terminal_event(client, state, ev?).await?,
                        Err(_) => break, // empty or disconnected — either way, we're done draining
                    }
                }
            }
            inbound = client.recv() => {
                let Some(inbound) = inbound? else { break };
                if let ClientInbound::Notification(n) = inbound {
                    apply_notification(state, n);
                }
            }
        }
        apply_pending_notifications(state, client);
        flush_pending_external_close(client, state).await?;
        flush_pending_scroll(client, state).await?;
        flush_pending_picker_scroll(client, state).await?;
        refresh_blame(client, state).await?;
        // Authoritative per-frame refresh: the mode-transition call sites don't fire when a key
        // *arms* or *resolves* a capture (`f`/`t`/`Ctrl-s`/`Space`), so reassert the style here so
        // the awaiting-key underline appears and clears reliably.
        apply_cursor_style(state);
        terminal.draw(|f| ui::draw(f, state))?;
        refresh_terminal_title(state);
    }
    Ok(())
}

/// Fetch and cache the blame for the cursor's current line so the renderer can show it as
/// end-of-line virtual text. Only runs in Normal mode: in Insert mode every keystroke bumps the
/// revision, and refetching (which makes the server recompute whole-file blame) per keystroke
/// would be wasteful and visually noisy. Skips the round-trip when the cached `(line, revision)`
/// is unchanged, so it's a cheap no-op on most frames. Best-effort — an RPC failure clears the
/// blame rather than propagating and disturbing editing.
async fn refresh_blame(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(ed) = state.editor.as_ref() else {
        return Ok(());
    };
    if ed.mode != EditorMode::Normal {
        return Ok(());
    }
    let key = (ed.cursor.position.line, ed.revision);
    if ed.blame.key == Some(key) {
        return Ok(());
    }
    let (buffer_id, line) = (ed.buffer_id, key.0);
    let info = match client
        .rpc::<GitBlameLine>(GitBlameLineParams { buffer_id, line })
        .await
    {
        Ok(r) => r.blame,
        Err(_) => None,
    };
    let ed = state.ed_mut();
    ed.blame.key = Some(key);
    ed.blame.info = info;
    Ok(())
}

/// Re-emit the terminal title via OSC if the derived title has changed since the last frame.
/// Cheap when state is unchanged (just a string compare); a single OSC write when it does
/// change. Failures are swallowed — the title is cosmetic and we'd rather have the editor
/// keep running than crash on a quirky terminal that doesn't accept the sequence.
fn refresh_terminal_title(state: &mut AppState) {
    let title = terminal_title(state);
    if title == state.last_terminal_title {
        return;
    }
    use std::io::Write;
    let mut stdout = std::io::stdout();
    let _ = crossterm::queue!(stdout, crossterm::terminal::SetTitle(&title));
    let _ = stdout.flush();
    state.last_terminal_title = title;
}

/// Derive the terminal title from the current state. Mirrors the left segment of the editor
/// status row — `[{project}] {file_label}` with an optional ` {marker}` — so the title
/// answers "what am I editing?" at a glance. Before any project is active we fall back to a
/// bare `Aether` placeholder; without a buffer (transient project-switch window) we just show
/// the project name.
fn terminal_title(state: &AppState) -> String {
    if state.project_name.is_empty() {
        return "Aether".to_string();
    }
    if !state.has_editor() {
        return format!("[{}]", state.project_name);
    }
    let marker = state.buffer_status_marker();
    let marker_suffix = if marker.is_empty() {
        String::new()
    } else {
        format!(" {marker}")
    };
    format!(
        "[{}] {}{}",
        state.project_name,
        state.ed().file_label,
        marker_suffix
    )
}

async fn dispatch_terminal_event(
    client: &mut Client,
    state: &mut AppState,
    ev: Event,
) -> Result<()> {
    // Clear the ephemeral status line before processing an event the user actually drives — a key
    // press/repeat, mouse action, resize, or paste. Anything the event itself sets (save/copy
    // feedback, search truncation, etc.) stays visible until the *next* such event. We must NOT
    // clear on events `handle_event` ignores — key *releases* (which the kitty keyboard protocol
    // reports) and focus changes — otherwise a release landing just after a slow save (its disk
    // `fsync` outlasts the keypress) would wipe the "saved" message before it could be read.
    if event_dismisses_status(&ev) {
        state.status = StatusMessage::default();
    }
    if let Event::Resize(cols, rows) = &ev {
        handle_resize(client, state, *cols, *rows).await
    } else {
        handle_event(client, state, ev).await
    }
}

/// Whether a terminal event should dismiss the ephemeral status line. True only for events the
/// user deliberately drives (key press/repeat, mouse click/scroll/drag, resize, paste). False for
/// the passive events `handle_event` ignores, so they can't silently clear a freshly-set message
/// (e.g. the "saved" feedback): key *releases* (reported under the kitty keyboard protocol), focus
/// changes, and — crucially — mouse *motion*, which the terminal streams continuously while the
/// cursor is over the window with mouse capture enabled.
fn event_dismisses_status(ev: &Event) -> bool {
    match ev {
        Event::Key(k) => matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat),
        Event::Mouse(m) => !matches!(m.kind, MouseEventKind::Moved),
        Event::FocusGained | Event::FocusLost => false,
        _ => true,
    }
}

/// Whether a keybinding is mid-entry and waiting for the next keystroke to complete it: `f`/`t`
/// (find char), `Ctrl-s` (surround delimiter), `Space` (leader), or a partially-typed count prefix
/// (`1`–`9`…). In each case the next key continues the in-flight command rather than starting a
/// fresh one — so we flag it with a distinct cursor shape.
fn awaiting_key(state: &AppState) -> bool {
    if state.pending_leader.is_some() {
        return true;
    }
    state.has_editor() && {
        let ed = state.ed();
        ed.pending_find.is_some() || ed.pending_surround.is_some() || ed.pending_count > 0
    }
}

fn apply_cursor_style(state: &AppState) {
    // A pending capture (`f`/`t`/`Ctrl-s`/`Space`) takes precedence: the underline signals "I'm
    // waiting for one more key." These captures only arm in the editor, never under an overlay.
    let style = if awaiting_key(state) {
        SetCursorStyle::SteadyUnderScore
    } else if state.picker.open
        || state.save_prompt.is_some()
        || state.confirm_prompt.is_some()
        || state.project_settings.is_some()
        || !state.has_editor()
    {
        // Overlays always use the bar cursor (they're text-prompt UIs). With no editor and no
        // overlay, fall back to the bar — there's nothing for the block cursor to sit on.
        SetCursorStyle::SteadyBar
    } else {
        match state.ed().mode {
            EditorMode::Normal => SetCursorStyle::SteadyBlock,
            EditorMode::Insert | EditorMode::Search => SetCursorStyle::SteadyBar,
        }
    };
    let _ = execute!(stdout(), style);
}

fn apply_pending_notifications(state: &mut AppState, client: &mut Client) {
    for n in client.drain_notifications() {
        apply_notification(state, n);
    }
}

/// Handle a `buffer/closed` push (another client, or a path/project deletion, closed the buffer we
/// had open): switch to the server-indicated next buffer — its MRU top — or a fresh scratch when
/// none remain. Best-effort: if attaching the next buffer fails (e.g. it raced closed too), fall
/// back to a scratch so we never strand the user on a dead buffer.
async fn flush_pending_external_close(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(p) = state.pending_external_close.take() else {
        return Ok(());
    };
    // Guard against having already moved off the buffer between the push and this drain.
    if !state.has_editor() || state.ed().buffer_id != p.buffer_id {
        return Ok(());
    }
    state.status = StatusMessage::warning("buffer closed by another client");
    match p.next_buffer_id {
        Some(next) if attach_buffer(client, state, next).await.is_ok() => Ok(()),
        _ => new_scratch(client, state).await,
    }
}

fn apply_notification(state: &mut AppState, n: aether_protocol::envelope::Notification) {
    // LSP status is project-scoped, not editor-bound — record it regardless of editor presence so
    // the status-bar indicator is fresh the moment a buffer appears.
    if n.method == LspStatusChanged::NAME {
        match serde_json::from_value::<LspServerStatus>(n.params) {
            Ok(s) => {
                state.lsp_status.insert((s.language.clone(), s.workspace_root.clone()), s);
            }
            Err(e) => state.status = StatusMessage::error(format!("bad lsp/status_changed: {e}")),
        }
        return;
    }
    if n.method == LspDiagnosticsChanged::NAME {
        match serde_json::from_value::<LspDiagnosticsChangedParams>(n.params) {
            Ok(p) => {
                state.diagnostic_counts.insert(p.buffer_id, p.counts);
            }
            Err(e) => state.status = StatusMessage::error(format!("bad lsp/diagnostics_changed: {e}")),
        }
        return;
    }
    // Editor-bound notifications: drop them entirely when there's no active editor (e.g. mid-
    // project-switch, or before the first activation). The picker/* notification is the only
    // one that still applies — pickers can outlive an editor (project picker, in particular).
    if !state.has_editor() && n.method != PickerUpdate::NAME {
        return;
    }
    if n.method == ViewportLinesChanged::NAME {
        match serde_json::from_value::<ViewportLinesChangedParams>(n.params) {
            Ok(p) if state.ed_mut().viewport_id == p.viewport_id => {
                splice_lines(state, p);
            }
            Ok(_) => {}
            Err(e) => state.status = StatusMessage::error(format!("bad notif params: {e}")),
        }
    } else if n.method == BufferState::NAME {
        match serde_json::from_value::<BufferStateParams>(n.params) {
            Ok(p) if state.ed_mut().buffer_id == p.buffer_id => {
                let ed = state.ed_mut();
                let was_synced = ed.revision == ed.saved_revision
                    && !ed.externally_modified
                    && !ed.externally_deleted;
                ed.saved_revision = p.saved_revision;
                ed.externally_modified = p.externally_modified;
                ed.externally_deleted = p.externally_deleted;
                if p.externally_deleted {
                    state.status = StatusMessage::warning(
                        "file removed on disk — save to recreate, or close buffer",
                    );
                } else if p.externally_modified {
                    state.status = StatusMessage::warning(
                        "file changed on disk — Ctrl-s to overwrite, or reload",
                    );
                } else if !was_synced && ed.revision == ed.saved_revision {
                    state.status =
                        StatusMessage::success(format!("saved (rev {})", ed.saved_revision));
                }
            }
            Ok(_) => {}
            Err(e) => state.status = StatusMessage::error(format!("bad buffer/state params: {e}")),
        }
    } else if n.method == SearchStateChanged::NAME {
        match serde_json::from_value::<SearchSummary>(n.params) {
            Ok(s) if state.ed_mut().buffer_id == s.buffer_id => {
                state.ed_mut().search.summary = Some(s);
            }
            Ok(_) => {}
            Err(e) => {
                state.status = StatusMessage::error(format!("bad search/state_changed params: {e}"))
            }
        }
    } else if n.method == BufferClosed::NAME {
        match serde_json::from_value::<BufferClosedParams>(n.params) {
            // Only react if it's the buffer we're actually on; switching needs an RPC, so stash it
            // for `flush_pending_external_close` to handle in the async loop.
            Ok(p) if state.ed_mut().buffer_id == p.buffer_id => {
                state.pending_external_close = Some(p);
            }
            Ok(_) => {}
            Err(e) => state.status = StatusMessage::error(format!("bad buffer/closed params: {e}")),
        }
    } else if n.method == PickerUpdate::NAME {
        match serde_json::from_value::<PickerUpdateParams>(n.params) {
            Ok(p) => {
                let applied = state.picker.apply_update(
                    p.kind,
                    p.generation,
                    p.offset,
                    p.items,
                    p.total_matches,
                    p.total_candidates,
                    p.ticking,
                );
                // `apply_update` may snap `selected` (resume re-anchor, or `pending_offset`
                // reconciliation) without touching `visible_start`. Slide the window now so the
                // highlight is on-screen on first draw, not only after the user presses arrow keys.
                if applied {
                    ensure_picker_selected_visible(state);
                }
            }
            Err(e) => state.status = StatusMessage::error(format!("bad picker/update params: {e}")),
        }
    }
}

fn splice_lines(state: &mut AppState, p: ViewportLinesChangedParams) {
    state.ed_mut().revision = p.revision;
    state.ed_mut().line_count = p.line_count;
    state.ed_mut().max_scroll_logical_line = p.max_scroll_logical_line;
    let local_start =
        (p.range.start_logical_line as i64) - (state.ed_mut().window_first_logical_line as i64);
    let local_end = (p.range.end_logical_line_exclusive as i64)
        - (state.ed_mut().window_first_logical_line as i64);
    if local_end < 0 || local_start > state.ed_mut().lines.len() as i64 {
        return;
    }
    let lo = local_start.max(0) as usize;
    let hi = (local_end as usize).min(state.ed_mut().lines.len());
    let replacement_len = p.replacement_lines.len();
    state.ed_mut().lines.splice(lo..hi, p.replacement_lines);
    // The server's notification covers the *current* (post-edit) viewport range. If the edit
    // shrank the buffer, the OLD `state.ed_mut().lines` could extend past the new range — truncate any
    // stale tail so subsequent draws never read a line that no longer exists.
    state.ed_mut().lines.truncate(lo + replacement_len);
}

async fn handle_event(client: &mut Client, state: &mut AppState, ev: Event) -> Result<()> {
    // Project-settings overlay (and its add-root sub-prompt) take priority over everything
    // else — they're modal and can be reached from both the editor view and the no-editor
    // view, so route them up here before any has_editor branching.
    // The help overlay sits above everything (it's openable with or without an editor) and is
    // scrollable by keys or the mouse wheel, so route both to it before any other overlay or
    // dispatch. Non-key/mouse events (e.g. resize) fall through to normal handling.
    if state.help.open {
        match &ev {
            Event::Key(k) if k.kind == KeyEventKind::Press || k.kind == KeyEventKind::Repeat => {
                return handle_help_key(state, *k);
            }
            Event::Mouse(m) => {
                handle_help_mouse(state, *m);
                return Ok(());
            }
            _ => {}
        }
    }
    // Hover popup: the mouse wheel pans it while it's showing.
    if state.hover.is_some() {
        if let (Event::Mouse(m), Some(h)) = (&ev, state.hover.as_mut()) {
            match m.kind {
                MouseEventKind::ScrollUp => {
                    h.scroll.scroll_by(-3);
                    return Ok(());
                }
                MouseEventKind::ScrollDown => {
                    h.scroll.scroll_by(3);
                    return Ok(());
                }
                _ => {}
            }
        }
    }
    if let Event::Key(k) = &ev {
        if k.kind == KeyEventKind::Press || k.kind == KeyEventKind::Repeat {
            // A showing hover popup intercepts scroll keys (pan it, consume), Esc (dismiss,
            // consume), and any other key (dismiss, then fall through so the key does its normal
            // thing). The hover action re-sets it later in this same dispatch.
            if state.hover.is_some() {
                let scrolled = if let Some(h) = state.hover.as_mut() {
                    match k.code {
                        KeyCode::Up => {
                            h.scroll.scroll_by(-1);
                            true
                        }
                        KeyCode::Down => {
                            h.scroll.scroll_by(1);
                            true
                        }
                        KeyCode::PageUp => {
                            h.scroll.page(false);
                            true
                        }
                        KeyCode::PageDown => {
                            h.scroll.page(true);
                            true
                        }
                        KeyCode::Home => {
                            h.scroll.scroll_to_top();
                            true
                        }
                        KeyCode::End => {
                            h.scroll.scroll_to_bottom();
                            true
                        }
                        _ => false,
                    }
                } else {
                    false
                };
                if scrolled {
                    return Ok(());
                }
                let was_esc = k.code == KeyCode::Esc;
                state.hover = None;
                if was_esc {
                    return Ok(());
                }
            }
            if state.project_settings.is_some() {
                return handle_project_settings_key(client, state, *k).await;
            }
        }
    }

    // No active editor: route through whichever overlay is on top, otherwise allow Space-leader
    // chords that don't need an editor (Space p / Space q) plus Ctrl-c. The full Normal-mode
    // keymap is gated behind `has_editor` because most bindings (motions, edits, save, etc.)
    // assume a buffer to act on.
    if !state.has_editor() {
        if let Event::Key(k) = ev {
            if k.kind != KeyEventKind::Press && k.kind != KeyEventKind::Repeat {
                return Ok(());
            }
            if state.picker.open {
                return handle_picker_key(client, state, k).await;
            }
            // Pending Space-leader: resolve it the same way the editor branch does. The
            // editor-requiring arms inside `handle_leader_key` early-return harmlessly when
            // there's no editor (see the `has_editor` guards there).
            if let Some(leader) = state.pending_leader.take() {
                return handle_leader_key(client, state, leader, k).await;
            }
            if k.code == KeyCode::Char(' ') && k.modifiers.is_empty() {
                state.pending_leader = Some(PendingLeader::Space);
            } else if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                state.should_quit = true;
            }
        }
        return Ok(());
    }
    // Track whether the cursor moved during this event. Pure-scroll bindings leave it alone, so
    // the viewport stays where the user scrolled; any binding that actually moves the cursor
    // triggers `ensure_cursor_in_window` to snap the view back to it.
    let cursor_before = state.ed_mut().cursor.position;
    match ev {
        Event::Key(k) => {
            if k.kind != KeyEventKind::Press && k.kind != KeyEventKind::Repeat {
                return Ok(());
            }
            // Pending leader chord (e.g. `Space f`): the next key resolves the binding. Handled
            // inline (not an early return) so a leader action that moves the cursor — next/prev
            // diagnostic, hunk nav, goto-definition — still hits the `ensure_cursor_in_window`
            // check below, like every other binding.
            if let Some(leader) = state.pending_leader.take() {
                handle_leader_key(client, state, leader, k).await?;
            // Overlays next — they sit on top of whichever screen is underneath. The
            // confirm prompt takes priority over everything else (it can layer on top of the
            // save prompt for the overwrite case).
            } else if state.confirm_prompt.is_some() {
                handle_confirm_prompt_key(client, state, k).await?;
            } else if state.save_prompt.is_some() {
                handle_save_prompt_key(client, state, k).await?;
            } else if state.picker.open {
                handle_picker_key(client, state, k).await?;
            } else {
                match state.ed_mut().mode {
                    EditorMode::Normal => handle_normal_key(client, state, k).await?,
                    EditorMode::Insert => handle_insert_key(client, state, k).await?,
                    EditorMode::Search => handle_search_key(client, state, k).await?,
                }
            }
        }
        Event::Mouse(m) => {
            if state.picker.open {
                handle_picker_mouse(client, state, m).await?;
            } else {
                handle_mouse_event(client, state, m).await?;
            }
        }
        _ => return Ok(()),
    }
    // Snap the view to the cursor if a binding moved it. Guard `has_editor`: a leader action can
    // close the last buffer (`Space c`) or quit (`Space q`), leaving no editor — and `ed_mut()`
    // would panic. (Before leader chords fell through to here, this couldn't happen.)
    if state.has_editor() && state.ed_mut().cursor.position != cursor_before {
        ensure_cursor_in_window(client, state).await?;
    }
    Ok(())
}

async fn handle_mouse_event(
    client: &mut Client,
    state: &mut AppState,
    m: MouseEvent,
) -> Result<()> {
    match m.kind {
        MouseEventKind::ScrollUp => scroll_lines(state, -3),
        MouseEventKind::ScrollDown => scroll_lines(state, 3),
        MouseEventKind::Down(MouseButton::Left) => {
            // Shift-click is left for the terminal's native text selection (copy-paste).
            if m.modifiers.contains(KeyModifiers::SHIFT) {
                return Ok(());
            }
            if let Some(pos) = ui::screen_to_logical(state, m.row, m.column) {
                let new = client
                    .rpc::<CursorSet>(CursorSetParams {
                        buffer_id: state.ed_mut().buffer_id,
                        position: pos,
                        anchor: pos,
                    })
                    .await?;
                state.ed_mut().cursor = new;
                state.ed_mut().drag_anchor = Some(new.position);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(anchor) = state.ed_mut().drag_anchor {
                if let Some(pos) = ui::screen_to_logical(state, m.row, m.column) {
                    let new = client
                        .rpc::<CursorSet>(CursorSetParams {
                            buffer_id: state.ed_mut().buffer_id,
                            position: pos,
                            anchor: anchor,
                        })
                        .await?;
                    state.ed_mut().cursor = new;
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            state.ed_mut().drag_anchor = None;
        }
        _ => {}
    }
    Ok(())
}

/// Normalize a `KeyEvent`'s `(code, modifiers)` so a `Shift-x` key reports as `('x', SHIFT)`
/// regardless of whether the terminal sent it as uppercase + no-shift or lowercase + shift.
fn normalize_key(k: KeyEvent) -> (KeyCode, KeyModifiers) {
    let mut mods = k.modifiers;
    let code = match k.code {
        KeyCode::Char(c) if c.is_ascii_uppercase() => {
            mods |= KeyModifiers::SHIFT;
            KeyCode::Char(c.to_ascii_lowercase())
        }
        other => other,
    };
    (code, mods)
}

/// Execute a resolved [`Action`]. This is the *execution* half of the data-driven keymap (the
/// *lookup* half lives in `keymap.rs`): the match arms hold the bodies that used to sit inline in
/// the per-mode `match (code, mods)` handlers. Runtime context the table can't carry — `count`,
/// whether Shift was held (`extend`), the viewport id, `last_motion` — is resolved here against
/// live `AppState`. Stateful captures that aren't chord lookups (digit accumulation, the `f`/`t`
/// continuation) stay in the handlers' preludes, not here.
async fn run_action(
    client: &mut Client,
    state: &mut AppState,
    action: Action,
    count: u32,
    extend: bool,
) -> Result<()> {
    match action {
        // ---- motions ----
        Action::MoveChar(direction) => {
            move_motion(client, state, Motion::Char { direction, count }, extend).await?
        }
        Action::MoveWord { dir, boundary } => {
            // Forward `w` is exclusive when extending — a selection built from it stops before the
            // next word's first char. Backward is always inclusive.
            move_motion(
                client,
                state,
                Motion::Word {
                    direction: dir,
                    count,
                    boundary,
                    exclusive: dir == Direction::Forward && extend,
                },
                extend,
            )
            .await?
        }
        Action::MoveWordEnd { dir, boundary } => {
            move_motion(
                client,
                state,
                Motion::WordEnd {
                    direction: dir,
                    count,
                    boundary,
                },
                extend,
            )
            .await?
        }
        Action::MoveVisualLine(direction) => {
            let viewport_id = state.ed_mut().viewport_id;
            move_motion(
                client,
                state,
                Motion::VisualLine {
                    viewport_id,
                    direction,
                    count,
                },
                extend,
            )
            .await?
        }
        Action::MoveLogicalLine(direction) => {
            move_motion(
                client,
                state,
                Motion::LogicalLine {
                    direction,
                    count,
                    preserve_col: true,
                },
                extend,
            )
            .await?
        }
        Action::MoveLineStart => move_motion(client, state, Motion::LineStart, extend).await?,
        Action::MoveLineEnd => move_motion(client, state, Motion::LineEnd, extend).await?,
        Action::MoveLineFirstNonblank => {
            move_motion(client, state, Motion::LineFirstNonblank, extend).await?
        }
        Action::GotoLine { last } => {
            let line = if last {
                state.ed_mut().line_count.saturating_sub(1)
            } else {
                count.saturating_sub(1)
            };
            let position = LogicalPosition { line, col: 0 };
            move_motion(client, state, Motion::Goto { position }, extend).await?
        }
        Action::MatchBracket { inner } => {
            move_motion(client, state, Motion::MatchBracket { inner }, extend).await?
        }
        Action::PageMotion { dir, half } => {
            let viewport_id = state.ed_mut().viewport_id;
            let span = if half {
                (state.viewport_rows / 2).max(1)
            } else {
                state.viewport_rows.max(1)
            };
            let lines = count.saturating_mul(span);
            move_motion(
                client,
                state,
                Motion::VisualLine {
                    viewport_id,
                    direction: dir,
                    count: lines,
                },
                extend,
            )
            .await?
        }
        // `]`/`[` navigate units (never extend); `}`/`{` jump to a unit edge (always extend).
        Action::NavUnit(Direction::Forward) => {
            move_motion(client, state, Motion::NextNavigationUnit, false).await?
        }
        Action::NavUnit(Direction::Backward) => {
            move_motion(client, state, Motion::PrevNavigationUnit, false).await?
        }
        Action::NavUnitEdge { start: false } => {
            move_motion(client, state, Motion::EndOfNavigationUnit, true).await?
        }
        Action::NavUnitEdge { start: true } => {
            move_motion(client, state, Motion::StartOfNavigationUnit, true).await?
        }

        // ---- selection / cursor history ----
        Action::SelectLine(direction) => {
            select_line(client, state, direction, extend, count).await?
        }
        Action::SwapAnchor => swap_anchor(client, state).await?,
        Action::CollapseSelection => {
            if !state.ed_mut().cursor.is_point() {
                clear_selection(client, state).await?;
            }
        }
        Action::TreeExpand => tree_expand(client, state, count).await?,
        Action::TreeContract => tree_contract(client, state, count).await?,
        Action::MotionUndo => motion_undo(client, state, count).await?,
        Action::MotionRedo => motion_redo(client, state, count).await?,
        Action::NavBack => nav_step(client, state, false).await?,
        Action::NavForward => nav_step(client, state, true).await?,
        Action::RepeatMotion => {
            // `r`'s own `count` is how many times to replay; the stored target keeps the original
            // motion's `count` baked in (so `2w` then `3r` steps 2 words three times). `extend` is
            // the live Shift on the `r` press, never how the original was issued — that's the
            // `r` (move) vs `Shift-r` (move + extend) distinction. Replaying an `Action` target
            // re-runs `run_action`, which re-records the same target (idempotent); `RepeatMotion`
            // itself isn't repeatable, so it never overwrites the target with itself.
            if let Some(target) = state.ed_mut().last_repeat.clone() {
                for _ in 0..count.max(1) {
                    match &target {
                        RepeatTarget::Action { action, count } => {
                            Box::pin(run_action(client, state, *action, *count, extend)).await?;
                        }
                        RepeatTarget::Find(motion) => {
                            move_motion(client, state, motion.clone(), extend).await?;
                        }
                    }
                }
            }
        }
        Action::CenterCursor => center_cursor(client, state).await?,
        Action::BeginFind { dir, till } => {
            state.ed_mut().pending_find = Some(PendingFind {
                direction: dir,
                till,
                extend,
                count,
            });
        }
        Action::BeginSurround(target) => state.ed_mut().pending_surround = Some(target),
        Action::Unsurround(target) => unsurround(client, state, target).await?,

        // ---- viewport scroll ----
        Action::Scroll { dir, unit } => match dir {
            ScrollDir::Up | ScrollDir::Down => {
                let rows = state.viewport_rows as i64;
                let mag = match unit {
                    ScrollUnit::Line => 1,
                    ScrollUnit::Half => (rows / 2).max(1),
                    ScrollUnit::Page => rows.max(1),
                };
                let delta = if matches!(dir, ScrollDir::Up) {
                    -mag
                } else {
                    mag
                };
                scroll_lines(state, delta);
            }
            ScrollDir::Left | ScrollDir::Right => {
                let cols = state.viewport_cols as i64;
                let mag = match unit {
                    ScrollUnit::Half => (cols / 2).max(1),
                    _ => 1,
                };
                let delta = if matches!(dir, ScrollDir::Left) {
                    -mag
                } else {
                    mag
                };
                scroll_cols(state, delta);
            }
        },

        // ---- mode transitions ----
        Action::EnterInsert(where_) => enter_insert_at(client, state, where_).await?,
        Action::LeaveInsert => leave_insert(state),
        Action::BeginLeader => state.pending_leader = Some(PendingLeader::Space),

        // ---- edits (Global Ctrl table + Insert keys) ----
        Action::Backspace => backspace(client, state).await?,
        Action::NewlineIndent => newline_and_indent(client, state).await?,
        Action::InsertTab => insert_text(client, state, "\t").await?,
        Action::DeletePoint => delete_selection(client, state).await?,
        Action::DeleteSelection => {
            for _ in 0..count.max(1) {
                delete_selection(client, state).await?;
            }
        }
        Action::DeleteLine => delete_line(client, state).await?,
        Action::Undo => undo(client, state, count).await?,
        Action::Redo => redo(client, state, count).await?,
        Action::ToggleWrap => toggle_wrap(client, state).await?,
        Action::ToggleDiffView => toggle_diff_view(client, state).await?,
        Action::NextHunk => navigate_hunk(client, state, HunkDirection::Next).await?,
        Action::PrevHunk => navigate_hunk(client, state, HunkDirection::Prev).await?,
        Action::MoveLines(dir) => move_lines(client, state, dir, count).await?,
        Action::JoinLines => join_lines(client, state, count).await?,
        Action::Indent => indent(client, state, count).await?,
        Action::Dedent => dedent(client, state, count).await?,
        Action::ToggleComment => toggle_comment(client, state).await?,
        Action::OpenLineBelow => open_line_below(client, state).await?,
        Action::OpenLineAbove => open_line_above(client, state).await?,
        Action::Copy => copy_to_clipboard(client, state, CopyScope::Selection).await?,
        Action::CopyLine => copy_to_clipboard(client, state, CopyScope::Line).await?,
        Action::Cut => cut_to_clipboard(client, state, CopyScope::Selection).await?,
        Action::CutLine => cut_to_clipboard(client, state, CopyScope::Line).await?,
        Action::Paste => paste_before(client, state, count).await?,
        Action::PasteAtCursor => paste_at_cursor(client, state).await?,
        Action::Change => change_selection(client, state).await?,
        Action::ChangeLine => change_line(client, state).await?,
        Action::ReplaceClipboard => paste_replace(client, state, count).await?,
        Action::ReplaceLineClipboard => replace_line_with_clipboard(client, state).await?,

        // ---- search ----
        Action::EnterSearch => enter_search_mode(client, state).await?,
        Action::EnterSearchToCursor => {
            enter_search_mode(client, state).await?;
            state.ed_mut().search.extend_to_cursor = true;
        }
        Action::SearchFromSelection => search_from_selection(client, state).await?,
        Action::SearchCycle(dir) => search_cycle(client, state, dir, count, extend).await?,
        Action::SearchAbort => abort_search(client, state).await?,
        Action::SearchCommit => commit_search(state),
        Action::SearchHistoryPrev => {
            history_up(state);
            run_incremental_search(client, state).await?;
        }
        Action::SearchHistoryNext => {
            history_down(state);
            run_incremental_search(client, state).await?;
        }
        Action::SearchCursorLeft => state.ed_mut().search.query.move_left(),
        Action::SearchCursorRight => state.ed_mut().search.query.move_right(),
        Action::SearchBackspace => {
            state.ed_mut().search.query.backspace();
            state.ed_mut().search.history_cursor = None;
            run_incremental_search(client, state).await?;
        }
        Action::GrepNavigate(dir) => grep_navigate(client, state, dir).await?,
        Action::DropSearch => {
            // Drop the active search (clears highlights, disables n/Alt-n). Use `d` to drop the
            // current selection instead.
            if state.ed_mut().search.active || state.ed_mut().search.summary.is_some() {
                let _ = client
                    .rpc::<SearchClear>(SearchClearParams {
                        buffer_id: state.ed_mut().buffer_id,
                    })
                    .await;
            }
            state.ed_mut().search.active = false;
            state.ed_mut().search.summary = None;
        }

        // ---- pickers / app-level ----
        Action::OpenPicker(kind) => open_picker(client, state, kind).await?,
        Action::OpenProjectSettings => {
            if !state.project_name.is_empty() {
                open_project_settings(state);
            }
        }
        Action::OpenHelp => {
            state.help.open = true;
            state.help.scroll.scroll_to_top();
        }
        Action::Quit => state.should_quit = true,
        Action::CloseBuffer => close_buffer(client, state).await?,
        Action::Save => save_buffer(client, state).await?,
        Action::SaveAs => begin_save_prompt(client, state).await?,
        Action::Reload => reload_buffer(client, state).await?,
        Action::NewScratch => {
            // Opening a fresh scratch is a buffer switch — record the origin so `Alt-Left` returns.
            record_nav(client, state).await;
            new_scratch(client, state).await?;
        }
        Action::Hover => lsp_hover(client, state).await?,
        Action::GotoDefinition => lsp_goto_definition(client, state).await?,
        Action::ShowDiagnostic => show_diagnostic(state),
        Action::NextDiagnostic => navigate_diagnostic(client, state, DiagnosticDirection::Next).await?,
        Action::PrevDiagnostic => navigate_diagnostic(client, state, DiagnosticDirection::Prev).await?,
        Action::Format => lsp_format(client, state).await?,
    }
    // Remember the action for `r`/`Shift-r` to replay. Recorded *after* a successful run (an RPC
    // error above short-circuits via `?` and leaves the previous target intact). Find is handled
    // separately at its capture site, since the target char isn't part of the `Action`.
    if action.is_repeatable() {
        state.ed_mut().last_repeat = Some(RepeatTarget::Action { action, count });
    }
    Ok(())
}

async fn handle_normal_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    // Pending `f`/`t`: the next keystroke names the target character. Use the raw key (skipping
    // `normalize_key`) so `f X` is case-sensitive. Any non-`Char` key (Esc, arrow, etc.) cancels.
    if let Some(pending) = state.ed_mut().pending_find.take() {
        if let KeyCode::Char(ch) = k.code {
            let motion = Motion::FindChar {
                ch,
                direction: pending.direction,
                count: pending.count,
                till: pending.till,
            };
            move_motion(client, state, motion.clone(), pending.extend).await?;
            // `BeginFind` only armed the capture; the repeatable thing is this resolved find, so
            // record it here (with its target char) rather than via `Action::is_repeatable`.
            state.ed_mut().last_repeat = Some(RepeatTarget::Find(motion));
        }
        return Ok(());
    }

    // Pending `Ctrl-s`: the next keystroke names the surround delimiter. Use the raw key so
    // `Ctrl-s "` and friends pass through verbatim. Any non-`Char` key (Esc, arrow, etc.) cancels.
    if let Some(target) = state.ed_mut().pending_surround.take() {
        if let KeyCode::Char(delimiter) = k.code {
            surround(client, state, delimiter, target).await?;
        }
        return Ok(());
    }

    let (code, mods) = normalize_key(k);

    // Digit accumulation for counts. `0` is the line-start motion unless we're already mid-count.
    if let KeyCode::Char(c @ '1'..='9') = code {
        if mods == KeyModifiers::NONE {
            let ed = state.ed_mut();
            ed.pending_count = ed
                .pending_count
                .saturating_mul(10)
                .saturating_add(c.to_digit(10).unwrap_or(0));
            return Ok(());
        }
    }
    if let KeyCode::Char('0') = code {
        if mods == KeyModifiers::NONE && state.ed_mut().pending_count > 0 {
            state.ed_mut().pending_count = state.ed_mut().pending_count.saturating_mul(10);
            return Ok(());
        }
    }

    // Whatever this command consumes for `count`, reset after.
    let count = if state.ed_mut().pending_count == 0 {
        1
    } else {
        state.ed_mut().pending_count
    };
    state.ed_mut().pending_count = 0;

    let extend = mods.contains(KeyModifiers::SHIFT);

    // Ctrl-modified editing shortcuts shared with Insert live in the `Global` table; the rest of
    // Normal mode is its own table. Both are data — see `keymap.rs`. The stateful prelude above
    // (find-char capture, digit counts) stays here because it isn't a chord lookup.
    if let Some(b) = keymap::lookup(keymap::KeyContext::Global, code, mods) {
        return run_action(client, state, b.action, count, extend).await;
    }
    if let Some(b) = keymap::lookup(keymap::KeyContext::Normal, code, mods) {
        run_action(client, state, b.action, count, extend).await?;
    }
    Ok(())
}

/// Implementation of `<` / `>`. Asks the server for the next/previous grep hit relative to the
/// current cursor and, if any, opens the target file at that position. Primes the new buffer's
/// search state with the picker's query so `n` / `Alt-n` follow-through works — mirrors what
/// picker selection of a grep hit does.
async fn grep_navigate(
    client: &mut Client,
    state: &mut AppState,
    direction: Direction,
) -> Result<()> {
    let target = client
        .rpc::<PickerGrepNavigate>(PickerGrepNavigateParams {
            direction,
            buffer_id: state.ed_mut().buffer_id,
        })
        .await?;
    let Some(target) = target else {
        return Ok(());
    };
    open_file_at_path(client, state, target.path, false, Some(target.position)).await?;
    if !target.query.is_empty() {
        let buffer_id = state.ed_mut().buffer_id;
        let r = client
            .rpc::<SearchSet>(SearchSetParams {
                buffer_id,
                query: target.query.clone(),
                anchor: Some(target.position),
                extend: false,
            })
            .await?;
        let ed = state.ed_mut();
        ed.cursor = r.cursor;
        ed.search.summary = Some(r.summary);
        ed.search.query.set(target.query.clone());
        ed.search.active = true;
        push_history(state, target.query);
    }
    Ok(())
}

/// Dispatch the second key of a chord opened by `state.pending_leader`. The leader itself was
/// already consumed (and taken out of `pending_leader`) by the caller; this just resolves what
/// the user wants to do.
async fn handle_leader_key(
    client: &mut Client,
    state: &mut AppState,
    leader: PendingLeader,
    k: KeyEvent,
) -> Result<()> {
    // `Space` is the only leader for now; the chord's second key resolves against the `Leader`
    // table. Actions that act on the current buffer are dropped without an editor (matching the
    // old "Esc cancels a half-typed chord" behaviour) so the pre-activation screen only surfaces
    // the editor-free actions (`Space p/q/,/?`).
    let _ = leader;
    let (code, mods) = normalize_key(k);
    if let Some(b) = keymap::lookup(keymap::KeyContext::Leader, code, mods) {
        if b.action.needs_editor() && !state.has_editor() {
            return Ok(());
        }
        run_action(client, state, b.action, 1, false).await?;
    }
    Ok(())
}

/// How many result rows the picker overlay can fit, given the current buffer-area dimensions.
/// Delegates to the ui module so the box geometry stays in one place. Distinct from the fetch
/// limit (see `PICKER_OVER_FETCH` below): the renderer shows this many rows, but we ask the
/// server for several pane-heights' worth so scrolling stays client-side.
fn picker_pane_rows(state: &AppState) -> u32 {
    crate::ui::picker_result_rows(state.viewport_cols, state.viewport_rows).max(1)
}

/// Over-fetch factor: we ask the server for `pane_rows * PICKER_OVER_FETCH` items so the user
/// can scroll several pages within the local cache before we have to round-trip for more. Higher
/// = fewer refetches but bigger pushes; 4× empirically covers the common case (scroll through a
/// page or two looking for a hit) without bloating updates.
const PICKER_OVER_FETCH: u32 = 4;

fn picker_fetch_limit(state: &AppState) -> u32 {
    picker_pane_rows(state) * PICKER_OVER_FETCH
}

async fn open_picker(client: &mut Client, state: &mut AppState, kind: PickerKind) -> Result<()> {
    let pane_rows = picker_pane_rows(state);
    let limit = picker_fetch_limit(state);
    // Pre-selection / centring policy per kind:
    //   - Grep: the server centres on the cursor's nearest hit via `center_on_cursor_grep_hit`
    //     below — keeps the picker in sync with where the user is in the buffer even when the
    //     cursor isn't sitting on a match exactly. Local `last_selected` is the fallback for
    //     the empty-cache / no-hits-after-cursor case.
    //   - Explorer: anchor on the active buffer's filename so the listing lands on the user's
    //     current file, regardless of where the user last navigated to.
    //   - Files / Buffers: no pre-selection, open at the top.
    let (center_on, resume_row_offset) = if kind == PickerKind::Explorer {
        (default_explorer_center_on(state), None)
    } else if kind.preserves_state() {
        match state.picker.last_selected.get(&kind).cloned() {
            Some((item, off)) => (Some(item), Some(off)),
            None => (None, None),
        }
    } else {
        (None, None)
    };
    // Explorer: always start in the active buffer's directory (or first project root for
    // scratch). The persisted `state.picker.explorer_dir` is meaningful *within* an open
    // session (navigation updates it; the path-prefix and Alt-Backspace step-up read it),
    // but on reopen we throw it away — the picker is contextual to the current buffer, not
    // a persistent file-manager session.
    let explorer_path_for_view: Option<String> = if kind == PickerKind::Explorer {
        default_explorer_dir(state)
    } else {
        None
    };
    // For Grep, ask the server to centre on the cursor's nearest hit (overriding our local
    // `center_on` when it resolves). This lets the picker open on "where you are" in the
    // result list even when the cursor isn't sitting on a match exactly — `cursor_grep_hit_item`
    // alone only covers the strict-on-a-match case. `.then(|| …)` (not `then_some`) so we don't
    // evaluate `state.ed_mut().buffer_id` for non-Grep kinds — Projects opens before any editor
    // exists.
    let center_on_cursor_grep_hit = (kind == PickerKind::Grep).then(|| state.ed_mut().buffer_id);
    // The diagnostics picker is scoped to the current buffer; the server builds its candidates from
    // that buffer's diagnostics on open.
    let diagnostics_buffer = (kind == PickerKind::Diagnostics).then(|| state.ed_mut().buffer_id);
    let view = client
        .rpc::<PickerView>(PickerViewParams {
            kind,
            reset: !kind.preserves_state(),
            offset: 0,
            limit,
            center_on: center_on.clone(),
            center_on_cursor_grep_hit,
            directory_path: explorer_path_for_view,
            buffer_id: diagnostics_buffer,
            explorer_roots: false,
        })
        .await?;
    // For grep, prefer the active buffer's search query as the input prefill. The server already
    // remembers the last grep query (returned via `view.query`), which becomes the fallback when
    // no buffer search is active — so we never need a client-side stash. If our prefill differs
    // from what the server has, we sync below by sending a `picker/query`; a matching prefill
    // short-circuits to the resume path (cached candidates, no extra RPC).
    let grep_prefill: Option<String> = if kind == PickerKind::Grep {
        compute_grep_prefill(state)
    } else {
        None
    };
    let initial_query = grep_prefill.clone().unwrap_or_else(|| view.query.clone());

    state.picker.open = true;
    state.picker.kind = Some(kind);
    state.picker.query.set(initial_query);
    state.picker.generation = view.generation;
    state.picker.offset = view.effective_offset;
    state.picker.limit = limit;
    state.picker.pane_rows = pane_rows;
    state.picker.items.clear();
    state.picker.visible_start = 0;
    state.picker.total_matches = 0;
    state.picker.total_candidates = view.total_candidates;
    state.picker.ticking = true;
    // The Buffers picker is MRU-ordered, so item 0 is the buffer you're already in. Default the
    // highlight to item 1 — the previously-viewed buffer — so `Enter` is a quick flip back.
    // `apply_update` clamps this to 0 when there's only one buffer.
    state.picker.selected = if kind == PickerKind::Buffers { 1 } else { 0 };
    // Prefer the server-resolved centre item (set when `center_on_cursor_grep_hit` resolved)
    // so `apply_update` snaps the highlight to the same row the server framed.
    state.picker.resume_target = view.effective_center_on.clone().or(center_on);
    state.picker.resume_row_offset = resume_row_offset;
    state.picker.pending_offset = None;
    state.picker.pending_delete = None;
    state.picker.lsp_detail = None;
    // Explorer: the server returns the canonical path + parent. Stash them so navigation
    // (Alt-h) and the header row know where they are. For other kinds, clear so a stale
    // explorer dir from a prior session doesn't leak into Files/Buffers/Grep state.
    if kind == PickerKind::Explorer {
        state.picker.explorer_dir = view.directory_path.clone();
        state.picker.explorer_parent = view.directory_parent.clone();
    } else {
        state.picker.explorer_dir = None;
        state.picker.explorer_parent = None;
    }
    apply_cursor_style(state);

    // Push the prefill to the server only when it actually changes the active query. Cache hit
    // on the server side (`last_completed_query == prefill`) makes this a near-no-op for
    // "buffer search query already matches last grep query"; otherwise a fresh search runs.
    if let Some(prefill) = grep_prefill {
        if prefill != view.query {
            send_picker_query(client, state).await?;
        }
    }
    Ok(())
}

/// Compute the prefill string for `Space g` from the active buffer's committed search query.
/// Returns `None` when no search is active.
fn compute_grep_prefill(state: &AppState) -> Option<String> {
    let ed = state.ed();
    if !ed.search.active || ed.search.query.is_empty() {
        return None;
    }
    Some(ed.search.query.text.clone())
}

/// Initial directory for a freshly-opened Explorer picker: parent of the active buffer's
/// file, or the first project root for scratch buffers. Walks up to the deepest *existing*
/// ancestor — a buffer attached via `create_if_missing` for a multi-segment path (e.g.
/// `foo/bar.rs` where `foo/` doesn't exist yet) would otherwise hand the server a
/// not-yet-existing dir to canonicalize, which fails the open.
fn default_explorer_dir(state: &AppState) -> Option<String> {
    if let Some(p) = state.ed().file_path.as_deref() {
        let mut cursor = std::path::Path::new(p).parent();
        while let Some(dir) = cursor {
            if dir.is_dir() {
                return Some(dir.display().to_string());
            }
            cursor = dir.parent();
        }
    }
    state.project_paths.first().cloned()
}

/// Pre-selection item for a freshly-opened Explorer picker — the active buffer's filename, so
/// the picker lands the highlight on the file the user is editing. `None` for scratch buffers
/// (no file to anchor on); also returns harmlessly to the server's offset-0 fallback when the
/// file isn't in the starting directory (e.g. multi-root edge cases).
fn default_explorer_center_on(state: &AppState) -> Option<PickerItem> {
    let p = state.ed().file_path.as_deref()?;
    let name = std::path::Path::new(p)
        .file_name()
        .and_then(|os| os.to_str())?
        .to_string();
    Some(PickerItem::DirEntry {
        name,
        is_dir: false,
        match_indices: Vec::new(),
    })
}

/// The leaf name of the directory we're stepping *out of* when Alt-h fires — used to land the
/// highlight on it inside the parent's listing so the user keeps their bearings. `None` if the
/// path has no file_name (root) or the picker has no current dir.
fn explorer_leaving_name(state: &AppState) -> Option<String> {
    let dir = state.picker.explorer_dir.as_deref()?;
    std::path::Path::new(dir)
        .file_name()
        .and_then(|os| os.to_str())
        .map(|s| s.to_string())
}

/// Switch the Explorer picker into Roots mode: one row per project root, with the disambiguated
/// label rendered client-side. Resets query / highlight / scroll like `picker_navigate_to_dir`.
/// Clears `explorer_dir` and `explorer_parent` so the next Alt-Backspace recognises "we're
/// already in Roots" and becomes a no-op.
async fn picker_enter_roots_mode(client: &mut Client, state: &mut AppState) -> Result<()> {
    let limit = state.picker.limit.max(1);
    let view = client
        .rpc::<PickerView>(PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit,
            center_on: None,
            center_on_cursor_grep_hit: None,
            directory_path: None,
            buffer_id: None,
            explorer_roots: true,
        })
        .await?;
    state.picker.query.clear();
    state.picker.generation = view.generation;
    state.picker.offset = view.effective_offset;
    state.picker.items.clear();
    state.picker.visible_start = 0;
    state.picker.total_matches = 0;
    state.picker.total_candidates = view.total_candidates;
    state.picker.ticking = true;
    state.picker.selected = 0;
    state.picker.pending_offset = None;
    state.picker.explorer_dir = None;
    state.picker.explorer_parent = None;
    state.picker.resume_target = None;
    state.picker.resume_row_offset = None;
    Ok(())
}

/// Issue a fresh `picker/view` for the Explorer picker with a new `directory_path`. Resets the
/// query (a filter that made sense in the old directory is rarely meaningful in the new one),
/// the highlight, and the scroll. If `pre_select_name` is set, the corresponding entry is
/// stashed as a resume anchor so the push lands the highlight on it — used by Alt-h to keep the
/// user's bearings on the directory they just left.
async fn picker_navigate_to_dir(
    client: &mut Client,
    state: &mut AppState,
    directory_path: String,
    pre_select_name: Option<&str>,
) -> Result<()> {
    let limit = state.picker.limit.max(1);
    let view = client
        .rpc::<PickerView>(PickerViewParams {
            kind: PickerKind::Explorer,
            reset: true,
            offset: 0,
            limit,
            center_on: None,
            center_on_cursor_grep_hit: None,
            directory_path: Some(directory_path),
            buffer_id: None,
            explorer_roots: false,
        })
        .await?;
    state.picker.query.clear();
    state.picker.generation = view.generation;
    state.picker.offset = view.effective_offset;
    state.picker.items.clear();
    state.picker.visible_start = 0;
    state.picker.total_matches = 0;
    state.picker.total_candidates = view.total_candidates;
    state.picker.ticking = true;
    state.picker.selected = 0;
    state.picker.pending_offset = None;
    state.picker.explorer_dir = view.directory_path.clone();
    state.picker.explorer_parent = view.directory_parent.clone();
    state.picker.resume_target = pre_select_name.map(|name| {
        aether_protocol::picker::PickerItem::DirEntry {
            name: name.to_string(),
            // The is_dir flag is part of the item but unused for identity-matching (we match by
            // name) — `true` is a sensible default since we only pre-select directories.
            is_dir: true,
            match_indices: Vec::new(),
        }
    });
    state.picker.resume_row_offset = None;
    Ok(())
}

async fn handle_picker_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    // A staged delete locks the picker into a [y/N] confirmation: `y` deletes; anything that reads
    // as "no" (n/N/Esc/Enter, matching the app's other confirms) cancels; every other key is
    // swallowed so a stray press can't silently drop the pending state.
    if let Some(pending) = state.picker.pending_delete.clone() {
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                state.picker.pending_delete = None;
                confirm_picker_delete(client, state, pending).await?;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                state.picker.pending_delete = None;
            }
            _ => {}
        }
        return Ok(());
    }

    // LSP-servers detail drill-down: the body shows one server's status/error. Keys scroll it;
    // Esc returns to the list (the picker stays open). Read-only — all other keys are swallowed.
    if state.picker.lsp_detail.is_some() {
        match k.code {
            KeyCode::Esc => state.picker.lsp_detail = None,
            KeyCode::Up => detail_scroll(state, -1),
            KeyCode::Down => detail_scroll(state, 1),
            KeyCode::PageUp => detail_page(state, false),
            KeyCode::PageDown => detail_page(state, true),
            KeyCode::Home => {
                if let Some(d) = state.picker.lsp_detail.as_mut() {
                    d.scroll.scroll_to_top();
                }
            }
            KeyCode::End => {
                if let Some(d) = state.picker.lsp_detail.as_mut() {
                    d.scroll.scroll_to_bottom();
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // Keep query input case-sensitive (so smartcase works), so skip `normalize_key`.
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => hide_picker(client, state).await?,
        // The LSP-servers picker: Enter drills into the highlighted server's status/error detail
        // (Esc there returns to the list); restart is `Ctrl-r`. Every other kind confirms the row.
        (KeyCode::Enter, _) => {
            if state.picker.kind == Some(PickerKind::LspServers) {
                open_lsp_detail(state);
            } else {
                select_picker_item(client, state).await?;
            }
        }
        // `Ctrl-r` restarts the highlighted server (LSP-servers picker only). Carries CONTROL so it
        // never reaches the query-insert arm; a no-op for the other kinds.
        (KeyCode::Char('r'), m) if m == KeyModifiers::CONTROL => {
            lsp_picker_restart(client, state).await?;
        }
        // `Alt-k` / `Alt-j` move the highlight up / down. Arrow keys are intentionally not bound
        // — modal navigation keeps the hand on the home row, matching the editor's vim-style
        // motions. The `Alt` modifier separates them from raw `j`/`k` typed into the query.
        (KeyCode::Char('k'), m) if m == KeyModifiers::ALT => {
            picker_move_selection(client, state, -1).await?
        }
        (KeyCode::Char('j'), m) if m == KeyModifiers::ALT => {
            picker_move_selection(client, state, 1).await?
        }
        (KeyCode::PageUp, _) => {
            let step = -(state.picker.pane_rows as i64);
            picker_move_selection(client, state, step).await?;
        }
        (KeyCode::PageDown, _) => {
            let step = state.picker.pane_rows as i64;
            picker_move_selection(client, state, step).await?;
        }
        // `Alt-Backspace` — multi-step "back": with a filter active, clear it; otherwise (Explorer
        // only) step up to the parent directory, or from a root's top into Roots mode.
        (KeyCode::Backspace, m) if m == KeyModifiers::ALT => picker_back(client, state).await?,
        // `Alt-h` / `Alt-l` are vim-style left/right. Their meaning is per-kind: in Grep they jump
        // the selection to the previous / next file's first hit; in the Explorer they step up a
        // level (`picker_back`) / enter the highlighted directory; elsewhere `Alt-h` clears the
        // filter (via `picker_back`) and `Alt-l` is a no-op.
        (KeyCode::Char('h'), m) if m == KeyModifiers::ALT => {
            if state.picker.kind == Some(PickerKind::Grep) {
                grep_jump_file(client, state, Direction::Backward).await?;
            } else {
                picker_back(client, state).await?;
            }
        }
        (KeyCode::Char('l'), m) if m == KeyModifiers::ALT => {
            if state.picker.kind == Some(PickerKind::Grep) {
                grep_jump_file(client, state, Direction::Forward).await?;
            } else {
                enter_highlighted_dir(client, state).await?;
            }
        }
        (KeyCode::Left, _) => state.picker.query.move_left(),
        (KeyCode::Right, _) => state.picker.query.move_right(),
        (KeyCode::Backspace, m) if m.is_empty() => {
            if !state.picker.query.is_empty() {
                state.picker.query.backspace();
                send_picker_query(client, state).await?;
            }
        }
        // `Delete` / `Ctrl-d` stages a delete on the highlighted row: a project (Projects picker),
        // or a file/directory (Files / Explorer pickers). `stage_delete` screens out the things
        // that aren't deletable (the synthetic "Create …" row, the active project, root rows).
        // Ctrl-d carries CONTROL so it never reaches the query-insert arm below; Delete isn't a
        // `Char` at all.
        (KeyCode::Delete, _) => stage_delete(state),
        (KeyCode::Char('d'), m) if m == KeyModifiers::CONTROL => stage_delete(state),
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            state.picker.query.insert_char(c);
            send_picker_query(client, state).await?;
        }
        _ => {}
    }
    Ok(())
}

/// The picker "back" gesture, shared by `Alt-Backspace` and `Alt-h`:
///   1. Non-empty filter → clear the filter (preserving the highlight as a resume anchor).
///   2. Filter empty + Explorer inside a subdirectory → step up to the parent directory.
///   3. Filter empty + Explorer at the top of a root → switch to Roots mode (multi-root only).
///   4. Otherwise → no-op (no further "back" to take).
///
/// We deliberately leave `resume_row_offset` as `None` on the clear path — a filtered listing
/// usually has the highlight near the top of the visible window, and pinning that offset onto the
/// larger unfiltered listing scrolls items off the top, making it look like the filter is still
/// active.
async fn picker_back(client: &mut Client, state: &mut AppState) -> Result<()> {
    if !state.picker.query.is_empty() {
        let anchor = state.picker.highlighted().cloned();
        state.picker.query.clear();
        send_picker_query(client, state).await?;
        state.picker.resume_target = anchor;
    } else if matches!(state.picker.kind, Some(PickerKind::Explorer)) {
        if let Some(parent) = state.picker.explorer_parent.clone() {
            let leaving = explorer_leaving_name(state);
            picker_navigate_to_dir(client, state, parent, leaving.as_deref()).await?;
        } else if state.picker.explorer_dir.is_some() && state.project_paths.len() > 1 {
            // At the top of a root (no parent inside the project) — escape into the Roots view.
            // Single-root projects skip this: there's only one root, so there's nothing to pick
            // between. Already-in-Roots case falls through (the `explorer_dir.is_none()` arm).
            picker_enter_roots_mode(client, state).await?;
        }
    }
    Ok(())
}

/// Enter the highlighted directory in the Explorer picker — a subdirectory `DirEntry`, or a root
/// in Roots mode. Returns `true` if it navigated, `false` if the highlight isn't a directory (a
/// file row, or a non-Explorer picker). Shared by `Enter` (which falls back to opening files) and
/// `Alt-l` (which only enters directories). The synthetic "+ create" row is a non-dir `DirEntry`,
/// so it falls through to `false` here — `Alt-l` won't trigger creation.
async fn enter_highlighted_dir(client: &mut Client, state: &mut AppState) -> Result<bool> {
    if state.picker.kind != Some(PickerKind::Explorer) {
        return Ok(false);
    }
    let Some(item) = state.picker.highlighted().cloned() else {
        return Ok(false);
    };
    match item {
        PickerItem::DirEntry {
            name, is_dir: true, ..
        } => {
            let target = std::path::Path::new(state.picker.explorer_dir.as_deref().unwrap_or(""))
                .join(&name)
                .display()
                .to_string();
            picker_navigate_to_dir(client, state, target, None).await?;
            Ok(true)
        }
        // Roots mode: a Root row navigates to that root's top. The client looks up the absolute
        // path from project_paths — the server stays out of presentation.
        PickerItem::Root { path_index, .. } => {
            if let Some(target) = state.project_paths.get(path_index as usize).cloned() {
                picker_navigate_to_dir(client, state, target, None).await?;
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Jump the grep picker's selection to the first hit of the next / previous file. The server finds
/// the boundary across the *whole* result list (so it works past the over-fetch window); the client
/// then moves there with natural scrolling:
///
/// - When the target is in the loaded window, reposition locally — no refetch, no flicker. The
///   target lands at the top of the pane, unless it's already on screen, in which case only the
///   highlight moves (`reveal_picker_selection_at_top`).
/// - When the target is past the window, refetch without a blank: keep the current items rendered,
///   adopt the server-framed offset, and let the arriving push swap the window in — snapping the
///   target to the top via `resume_row_offset = 0`.
///
/// No-op when there's no further file that way.
async fn grep_jump_file(
    client: &mut Client,
    state: &mut AppState,
    direction: Direction,
) -> Result<()> {
    use aether_protocol::picker::{PickerGrepFileJump, PickerGrepFileJumpParams};
    if state.picker.kind != Some(PickerKind::Grep) || state.picker.items.is_empty() {
        return Ok(());
    }
    let from_index = state.picker.offset + state.picker.selected as u32;
    let Some(target) = client
        .rpc::<PickerGrepFileJump>(PickerGrepFileJumpParams {
            from_index,
            direction,
        })
        .await?
    else {
        return Ok(()); // already at the first / last file
    };

    // In the loaded window → purely local move, no refetch.
    if let Some(idx) = state
        .picker
        .items
        .iter()
        .position(|i| crate::picker::item_key(i) == crate::picker::item_key(&target))
    {
        state.picker.selected = idx;
        reveal_picker_selection_at_top(state);
        return Ok(());
    }

    // Past the window → frame the target with a refetch. We leave `items` / `selected` /
    // `visible_start` untouched so the current window stays on screen until the push lands (no
    // blank frame); adopting the server-framed `offset` now means that push reconciles via the
    // cache-swap path, and `resume_target` + `resume_row_offset: 0` snap the target to the top.
    let limit = state.picker.limit.max(1);
    let view = client
        .rpc::<PickerView>(PickerViewParams {
            kind: PickerKind::Grep,
            reset: false,
            offset: 0,
            limit,
            center_on: Some(target.clone()),
            center_on_cursor_grep_hit: None,
            directory_path: None,
            buffer_id: None,
            explorer_roots: false,
        })
        .await?;
    state.picker.generation = view.generation;
    state.picker.offset = view.effective_offset;
    state.picker.total_candidates = view.total_candidates;
    state.picker.resume_target = view.effective_center_on.or(Some(target));
    state.picker.resume_row_offset = Some(0);
    Ok(())
}

/// Scroll the picker so `selected` sits at the *top* of the pane — unless it's already visible, in
/// which case leave the scroll alone (only the highlight moved). Used by grep file-jumps so landing
/// on a new file reveals it from its first hit, while an in-view jump doesn't yank the scroll.
fn reveal_picker_selection_at_top(state: &mut AppState) {
    let pane_rows = state.picker.pane_rows.max(1) as usize;
    let visible_count = crate::ui::picker_visible_item_count_from(
        &state.picker.items,
        state.picker.visible_start,
        pane_rows,
        state.picker.kind,
    );
    let visible_end = state.picker.visible_start + visible_count;
    let already_visible =
        state.picker.selected >= state.picker.visible_start && state.picker.selected < visible_end;
    if !already_visible {
        state.picker.visible_start = state.picker.selected;
    }
}

/// Stage a `[y/N]` delete confirmation for the highlighted row, if it's deletable. Dispatches by
/// picker kind: a project (Projects picker) or a file/directory (Files / Explorer pickers). The
/// synthetic "Create …" row, Root rows, Buffer/Grep rows, and the active project aren't deletable
/// and no-op — the active project surfaces an inline note (matching the server, which refuses it
/// anyway).
fn stage_delete(state: &mut AppState) {
    use crate::picker::{PendingDelete, PendingDeleteAction};
    let Some(kind) = state.picker.kind else {
        return;
    };
    if state.picker.highlighted_is_synthetic_create() {
        return;
    }
    let Some(item) = state.picker.highlighted().cloned() else {
        return;
    };
    let pending = match (kind, &item) {
        (PickerKind::Projects, PickerItem::Project { name, .. }) => {
            if *name == state.project_name {
                state.status = StatusMessage::info(format!(
                    "can't delete \"{name}\" — it's the active project"
                ));
                return;
            }
            PendingDelete {
                action: PendingDeleteAction::Project(name.clone()),
                item: item.clone(),
                noun: "project",
                name: name.clone(),
            }
        }
        (
            PickerKind::Files,
            PickerItem::File {
                path_index,
                relative_path,
                ..
            },
        ) => {
            let Some(root) = state.project_paths.get(*path_index as usize) else {
                return;
            };
            let abs = std::path::Path::new(root)
                .join(relative_path)
                .display()
                .to_string();
            PendingDelete {
                action: PendingDeleteAction::Path(abs),
                item: item.clone(),
                noun: "file",
                name: relative_path.clone(),
            }
        }
        (PickerKind::Explorer, PickerItem::DirEntry { name, is_dir, .. }) => {
            let Some(dir) = state.picker.explorer_dir.as_deref() else {
                return;
            };
            let abs = std::path::Path::new(dir).join(name).display().to_string();
            PendingDelete {
                action: PendingDeleteAction::Path(abs),
                item: item.clone(),
                noun: if *is_dir { "directory" } else { "file" },
                name: name.clone(),
            }
        }
        _ => return,
    };
    state.picker.pending_delete = Some(pending);
}

/// Execute a confirmed picker delete, dispatching to the project- or path-delete flow.
async fn confirm_picker_delete(
    client: &mut Client,
    state: &mut AppState,
    pending: crate::picker::PendingDelete,
) -> Result<()> {
    use crate::picker::PendingDeleteAction;
    match pending.action {
        PendingDeleteAction::Project(name) => confirm_delete_project(client, state, &name).await,
        PendingDeleteAction::Path(abs) => {
            confirm_delete_path(client, state, &abs, pending.noun, &pending.name).await
        }
    }
}

/// Send `project/delete` for a confirmed project, then rebuild the picker list from disk (the
/// server re-reads on view) so the deleted project drops out. On refusal — the project went
/// active, or a buffer is dirty — surface the server's message and leave the list as-is.
async fn confirm_delete_project(
    client: &mut Client,
    state: &mut AppState,
    name: &str,
) -> Result<()> {
    use aether_protocol::project::{ProjectDelete, ProjectDeleteParams};
    match client
        .rpc::<ProjectDelete>(ProjectDeleteParams {
            name: name.to_string(),
        })
        .await
    {
        Ok(()) => {
            state.status = StatusMessage::success(format!("deleted project \"{name}\""));
            // Re-view from disk. `open_picker` resets the query to empty, which is fine — the
            // refreshed full list (now missing the deleted project) reads clearly.
            open_picker(client, state, PickerKind::Projects).await?;
        }
        Err(e) => {
            let msg = if let Some(rpc_err) = e.downcast_ref::<crate::client::RpcError>() {
                rpc_err.message.clone()
            } else {
                e.to_string()
            };
            state.status = StatusMessage::error(msg);
        }
    }
    Ok(())
}

/// Send `path/delete` (move to OS trash) for a confirmed file/directory, then refresh the list. If
/// the delete closed the buffer we're currently editing, re-attach to the server-suggested next
/// buffer (or a fresh scratch), mirroring `remove_root`. On refusal — a buffer under the path is
/// dirty, or it's outside the project — surface the server's message.
async fn confirm_delete_path(
    client: &mut Client,
    state: &mut AppState,
    abs: &str,
    noun: &str,
    name: &str,
) -> Result<()> {
    use aether_protocol::path::{PathDelete, PathDeleteParams};
    match client
        .rpc::<PathDelete>(PathDeleteParams {
            path: abs.to_string(),
        })
        .await
    {
        Ok(res) => {
            if let Some(cur) = state.editor.as_ref().map(|e| e.buffer_id) {
                if res.closed_buffer_ids.contains(&cur) {
                    match res.next_buffer_id {
                        Some(next) => attach_buffer(client, state, next).await?,
                        None => new_scratch(client, state).await?,
                    }
                }
            }
            state.status = StatusMessage::success(format!("trashed {noun} \"{name}\""));
            refresh_picker_after_path_delete(client, state).await?;
        }
        Err(e) => {
            let msg = if let Some(rpc_err) = e.downcast_ref::<crate::client::RpcError>() {
                rpc_err.message.clone()
            } else {
                e.to_string()
            };
            state.status = StatusMessage::error(msg);
        }
    }
    Ok(())
}

/// Refresh the active picker after a file/directory delete so the trashed entry drops out: the
/// Explorer re-lists its current directory; the Files picker re-views (re-walking the now-
/// invalidated workspace index). The query resets — fine for a one-off management action.
async fn refresh_picker_after_path_delete(client: &mut Client, state: &mut AppState) -> Result<()> {
    match state.picker.kind {
        Some(PickerKind::Explorer) => {
            if let Some(dir) = state.picker.explorer_dir.clone() {
                picker_navigate_to_dir(client, state, dir, None).await?;
            }
        }
        Some(PickerKind::Files) => open_picker(client, state, PickerKind::Files).await?,
        _ => {}
    }
    Ok(())
}

/// Mouse wheel inside the picker moves the highlight. Click/drag is ignored for now — the
/// picker is a keyboard-driven modal; we can wire row-click-to-select later if it feels needed.
async fn handle_picker_mouse(
    client: &mut Client,
    state: &mut AppState,
    m: MouseEvent,
) -> Result<()> {
    match m.kind {
        MouseEventKind::ScrollUp => picker_move_selection(client, state, -1).await?,
        MouseEventKind::ScrollDown => picker_move_selection(client, state, 1).await?,
        _ => {}
    }
    Ok(())
}

/// Move selection by `delta` rows. Negative = up. When the move falls off the visible window, we
/// slide the server-side window via `picker/view` so the highlighted item stays in range.
async fn picker_move_selection(
    _client: &mut Client,
    state: &mut AppState,
    delta: i64,
) -> Result<()> {
    // A user-driven move cancels any pending resume re-anchor.
    state.picker.resume_target = None;
    state.picker.resume_row_offset = None;

    let items_len = state.picker.items.len();
    if items_len == 0 {
        return Ok(());
    }
    let pane_rows = state.picker.pane_rows.max(1) as usize;

    // Update `selected` within the local cache. Then slide `visible_start` to keep `selected`
    // on-screen — that's the cheap part, no RPC required.
    let new_selected = (state.picker.selected as i64 + delta).clamp(0, items_len as i64 - 1);
    state.picker.selected = new_selected as usize;
    ensure_picker_selected_visible(state);

    // Refetch trigger: when the visible window is within a pane's worth of the cache edge AND
    // there are more results past it, queue a `pending_offset` shift so `flush_pending_picker_scroll`
    // fires one RPC. The shift is sized to keep ~half the cache as history on the trailing side,
    // so backward-scroll right after the refetch also stays local.
    let kind = state.picker.kind;
    let cache_end = items_len;
    let server_more_forward =
        (state.picker.offset as usize) + cache_end < state.picker.total_matches as usize;
    let server_more_backward = state.picker.offset > 0;
    let visible_count = crate::ui::picker_visible_item_count_from(
        &state.picker.items,
        state.picker.visible_start,
        pane_rows,
        kind,
    );
    let visible_end = state.picker.visible_start + visible_count;
    let near_forward_edge = visible_end + pane_rows >= cache_end;
    let near_backward_edge = state.picker.visible_start < pane_rows;

    if delta > 0 && near_forward_edge && server_more_forward {
        // Slide the cache forward by roughly half the over-fetch buffer; that way we keep a few
        // pages of history (so a small backtrack doesn't re-RPC) while still extending the
        // forward runway.
        let shift = state
            .picker
            .limit
            .saturating_sub(state.picker.pane_rows)
            .max(state.picker.pane_rows);
        let max_offset = state
            .picker
            .total_matches
            .saturating_sub(state.picker.limit);
        let target = (state.picker.offset + shift).min(max_offset);
        if target != state.picker.offset {
            state.picker.pending_offset = Some(target);
        }
    } else if delta < 0 && near_backward_edge && server_more_backward {
        let shift = state
            .picker
            .limit
            .saturating_sub(state.picker.pane_rows)
            .max(state.picker.pane_rows);
        let target = state.picker.offset.saturating_sub(shift);
        if target != state.picker.offset {
            state.picker.pending_offset = Some(target);
        }
    }
    Ok(())
}

/// Adjust `visible_start` so `selected` is within the rendered window. Cheap and runs on every
/// move — keeps the highlight on-screen as the user navigates the local cache, without any RPC.
fn ensure_picker_selected_visible(state: &mut AppState) {
    let pane_rows = state.picker.pane_rows.max(1) as usize;
    let items = &state.picker.items;
    let kind = state.picker.kind;

    // Selected scrolled above the visible window — snap visible_start to it.
    if state.picker.selected < state.picker.visible_start {
        state.picker.visible_start = state.picker.selected;
        return;
    }
    // Selected scrolled past the visible window — advance visible_start one row at a time until
    // selected is the last visible item. For grep the visible count depends on the slice's
    // header pattern, so we recompute each step rather than doing the math analytically.
    loop {
        let count = crate::ui::picker_visible_item_count_from(
            items,
            state.picker.visible_start,
            pane_rows,
            kind,
        );
        let end = state.picker.visible_start + count;
        if state.picker.selected < end {
            break;
        }
        if state.picker.visible_start + 1 >= items.len() {
            break;
        }
        state.picker.visible_start += 1;
    }
}

/// Send the latest pending picker refetch, if any. Called from the main loop after each
/// event-drain batch so trackpad / held-arrow bursts collapse into one RPC. We keep
/// `pending_offset` set across the await — `apply_update` clears it when the matching push
/// lands and shifts `visible_start` / `selected` so the user's spot in the result set is
/// preserved across the cache swap.
async fn flush_pending_picker_scroll(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(target) = state.picker.pending_offset else {
        return Ok(());
    };
    if target == state.picker.offset {
        state.picker.pending_offset = None;
        return Ok(());
    }
    let Some(kind) = state.picker.kind else {
        state.picker.pending_offset = None;
        return Ok(());
    };
    let limit = state.picker.limit;
    let view = client
        .rpc::<PickerView>(PickerViewParams {
            kind,
            reset: false,
            offset: target,
            limit,
            center_on: None,
            center_on_cursor_grep_hit: None,
            directory_path: None,
            buffer_id: None,
            explorer_roots: false,
        })
        .await?;
    // The server may have clamped `target` (e.g. past EOF). Update `pending_offset` so the
    // arriving push (which carries `effective_offset`) is recognized by `apply_update`.
    state.picker.pending_offset = Some(view.effective_offset);
    Ok(())
}

async fn send_picker_query(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(kind) = state.picker.kind else {
        return Ok(());
    };
    state.picker.generation = state.picker.generation.wrapping_add(1);
    state.picker.offset = 0;
    state.picker.selected = 0;
    state.picker.visible_start = 0;
    state.picker.pending_offset = None;
    state.picker.ticking = true;
    // Query changes invalidate the resume anchor — the user is steering somewhere new.
    state.picker.resume_target = None;
    state.picker.resume_row_offset = None;
    // Recompute the synthetic "create" row immediately so the user gets feedback before the
    // server's response arrives. `apply_update` will reconcile it once the push lands.
    state.picker.recompute_synthetic_create_row();
    client
        .rpc::<PickerQuery>(PickerQueryParams {
            kind,
            query: state.picker.query.text.clone(),
            generation: state.picker.generation,
        })
        .await?;
    Ok(())
}

async fn select_picker_item(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(kind) = state.picker.kind else {
        return Ok(());
    };
    // Synthetic "create new project" row in the Projects picker: route to project/create with
    // the picker's current query, then auto-open the settings overlay so the user can add roots
    // immediately. Bypasses picker/select entirely (the server doesn't know about this row).
    if kind == PickerKind::Projects && state.picker.highlighted_is_synthetic_create() {
        let name = state.picker.query.text.trim().to_string();
        if name.is_empty() {
            return Ok(());
        }
        let _ = client.rpc::<PickerHide>(PickerHideParams { kind }).await;
        state.picker.open = false;
        create_project_and_open_settings(client, state, &name).await?;
        return Ok(());
    }
    // Synthetic "+ create" row in the Explorer picker. A trailing `/` on the typed query
    // switches the action from "create file" to "create directory". File creation routes
    // through `buffer/open { create_if_missing }`; directory creation routes through
    // `directory/create` and then navigates the picker into the new dir so the user can
    // immediately start typing a filename inside it.
    if kind == PickerKind::Explorer && state.picker.highlighted_is_synthetic_create() {
        let raw = state.picker.query.text.trim().to_string();
        if raw.is_empty() {
            return Ok(());
        }
        if let Some(name) = raw.strip_suffix('/') {
            return create_directory_in_explorer_dir(client, state, name).await;
        }
        return create_file_in_explorer_dir(client, state, &raw).await;
    }
    let Some(item) = state.picker.highlighted().cloned() else {
        return Ok(());
    };
    // Explorer + a directory (subdir or root): Enter "enters" it rather than selecting — the same
    // navigation `Alt-l` performs. File entries return `false` and fall through to the normal
    // selection path (server returns `File { path }`, we open it).
    if enter_highlighted_dir(client, state).await? {
        return Ok(());
    }
    if kind.preserves_state() {
        let row_offset = state
            .picker
            .selected
            .saturating_sub(state.picker.visible_start);
        state
            .picker
            .last_selected
            .insert(kind, (item.clone(), row_offset));
    }

    // Snapshot the picker query before hide/clear — we'll forward it to the opened buffer's
    // search state when selecting a grep hit.
    let grep_query: Option<String> = if kind == PickerKind::Grep {
        let q = state.picker.query.text.clone();
        if q.is_empty() {
            None
        } else {
            Some(q)
        }
    } else {
        None
    };

    let result = client
        .rpc::<PickerSelect>(PickerSelectParams {
            kind,
            item: item.clone(),
        })
        .await?;
    // Implicit hide: server keeps state alive for resume, just stops pushing.
    let _ = client.rpc::<PickerHide>(PickerHideParams { kind }).await;
    state.picker.open = false;

    match result {
        PickerSelectResult::File { path } => {
            open_file_at_path(client, state, path, false, None).await?;
        }
        PickerSelectResult::Buffer { buffer_id } => {
            // Switching to a different buffer is a jump; record the origin first (skip if it's the
            // buffer we're already on — `attach_buffer` no-ops there).
            if state.editor.as_ref().map(|e| e.buffer_id) != Some(buffer_id) {
                record_nav(client, state).await;
            }
            attach_buffer(client, state, buffer_id).await?;
        }
        PickerSelectResult::FileAt { path, position } => {
            open_file_at_path(client, state, path, false, Some(position)).await?;
            // For grep selects, prime the new buffer's search with the picker query so the
            // matches are highlighted and `n` / `Alt-n` step through them. Anchor at the grep
            // hit's position so the server resolves `current_index` to this match.
            if let Some(query) = grep_query {
                let buffer_id = state.ed_mut().buffer_id;
                let r = client
                    .rpc::<SearchSet>(SearchSetParams {
                        buffer_id,
                        query: query.clone(),
                        anchor: Some(position),
                        extend: false,
                    })
                    .await?;
                let ed = state.ed_mut();
                ed.cursor = r.cursor;
                ed.search.summary = Some(r.summary);
                ed.search.query.set(query.clone());
                ed.search.active = true;
                push_history(state, query);
            }
        }
        PickerSelectResult::Project { name } => {
            // Project selection: switch active project + bootstrap a fresh editor. Returns
            // early so the post-select tail (which assumes an active editor for mode/cursor
            // adjustments) doesn't run on the not-yet-replaced state.
            activate_project_and_rebuild_editor(client, state, &name).await?;
            return Ok(());
        }
    }
    // Whatever the selection did (file open / buffer switch), we land in Normal mode. Guarded
    // because PickerSelectResult::Project early-returned above, but defensive in case future
    // result variants rebuild the editor inline.
    if state.has_editor() {
        state.ed_mut().mode = EditorMode::Normal;
    }
    apply_cursor_style(state);
    Ok(())
}

/// Snapshot the current location onto the server-side jump list before a user-initiated
/// cross-file navigation. Best-effort — a failure shouldn't abort the navigation itself.
async fn record_nav(client: &mut Client, state: &AppState) {
    if let Some(ed) = state.editor.as_ref() {
        let _ = client.rpc::<NavRecord>(NavRecordParams { buffer_id: ed.buffer_id }).await;
    }
}

/// `Alt-Left` / `Alt-Right`: step the jump list (`forward=false` is back). The server restores the
/// target buffer's cursor/selection without feeding the `z` motion history; we re-subscribe to
/// whatever buffer it returns. A `None` target means we're already at the end of the stack.
async fn nav_step(client: &mut Client, state: &mut AppState, forward: bool) -> Result<()> {
    let Some(buffer_id) = state.editor.as_ref().map(|e| e.buffer_id) else {
        return Ok(());
    };
    let res: NavStepResult = if forward {
        client.rpc::<NavForward>(NavStepParams { buffer_id }).await?
    } else {
        client.rpc::<NavBack>(NavStepParams { buffer_id }).await?
    };
    match res.target {
        Some(open) => {
            state.ed_mut().mode = EditorMode::Normal;
            subscribe_to_buffer(client, state, open).await?;
            apply_cursor_style(state);
        }
        None => {
            state.status = StatusMessage::info(if forward {
                "no later location in history"
            } else {
                "no earlier location in history"
            });
        }
    }
    Ok(())
}

/// Switch to an already-open buffer by id (no path lookup; works for scratch buffers too).
/// Subscribes a fresh viewport and restores per-buffer cursor + scroll from the server. No-op
/// in the sense that the buffer's contents and per-client state already exist server-side —
/// we're just rebinding the client to it. Never discards the scratch we're leaving: this is
/// navigation among the already-open buffers (the scratch is one of them), so dropping it as a
/// side effect would be surprising — unlike opening a *file*, which replaces the placeholder.
async fn attach_buffer(
    client: &mut Client,
    state: &mut AppState,
    buffer_id: BufferId,
) -> Result<()> {
    if state.ed_mut().buffer_id == buffer_id {
        // Already attached. Skip the round-trip; the picker's "current at position 0" feature
        // makes selecting the current buffer a frequent no-op.
        return Ok(());
    }
    let open: BufferOpenResult = client
        .rpc::<BufferOpen>(BufferOpenParams {
            buffer_id: Some(buffer_id),
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        })
        .await?;
    subscribe_to_buffer(client, state, open).await
}

/// Create a fresh scratch buffer and switch to it. Empty buffer with no path; saved as a new
/// file when the user runs Save-As. MRU bumps server-side so it shows up at position 0 in the
/// buffer picker.
async fn new_scratch(client: &mut Client, state: &mut AppState) -> Result<()> {
    let open: BufferOpenResult = client
        .rpc::<BufferOpen>(BufferOpenParams {
            buffer_id: None,
            path_index: None,
            relative_path: None,
            language: None,
            create_if_missing: false,
            jump_to: None,
        })
        .await?;
    // New Scratch is an explicit request for a fresh placeholder — don't discard the one we're on.
    subscribe_to_buffer(client, state, open).await
}

/// Close the active buffer. If it's dirty, opens a confirm prompt first; the user's `y`
/// answer routes through `handle_confirm_prompt_key` back to `finalize_close_buffer`.
async fn close_buffer(client: &mut Client, state: &mut AppState) -> Result<()> {
    let ed = state.ed();
    let buffer_id = ed.buffer_id;
    let dirty = ed.revision != ed.saved_revision;
    let label = ed.file_label.clone();
    if dirty {
        state.confirm_prompt = Some(ConfirmPrompt {
            message: format!("discard unsaved changes in {label}"),
            action: ConfirmAction::CloseBuffer { buffer_id },
        });
        apply_cursor_style(state);
        return Ok(());
    }
    finalize_close_buffer(client, state, buffer_id).await
}

/// Actually close `buffer_id`. The server returns the next-MRU buffer to switch to; if
/// `None`, no buffers remain and we open a fresh scratch. Called either directly (clean
/// buffer) or via the confirm-prompt flow (dirty).
async fn finalize_close_buffer(
    client: &mut Client,
    state: &mut AppState,
    buffer_id: BufferId,
) -> Result<()> {
    // Capture the display name before the buffer's gone so the status message can refer to it.
    let closed_label = if state.ed_mut().buffer_id == buffer_id {
        state.ed_mut().file_label.clone()
    } else {
        format!("buffer {buffer_id}")
    };
    let result: aether_protocol::buffer::BufferCloseResult = client
        .rpc::<BufferClose>(BufferCloseParams { buffer_id })
        .await?;
    state.status = StatusMessage::success(format!("closed {closed_label}"));
    if let Some(next) = result.next_buffer_id {
        attach_buffer(client, state, next).await?;
    } else {
        new_scratch(client, state).await?;
    }
    Ok(())
}

/// The current buffer's id when it's an empty, unmodified scratch — the placeholder the editor
/// spawns so you always land *somewhere*. `None` otherwise. A scratch has no path to save in
/// place, so it only becomes dirty once typed into; "no path and not dirty" therefore already
/// implies empty, and the line-count check is a cheap guard.
fn empty_scratch_id(state: &AppState) -> Option<BufferId> {
    let ed = state.editor.as_ref()?;
    (ed.file_path.is_none() && ed.revision == ed.saved_revision && ed.line_count <= 1)
        .then_some(ed.buffer_id)
}

/// Shared post-`buffer/open` plumbing for runtime buffer switches: build the new `EditorState`
/// (via the shared core, inheriting the previous editor's wrap), replace `state.editor`, and
/// ensure the cursor is in view. `attach_buffer`, `new_scratch`, and `subscribe_replacing_scratch`
/// route through this.
async fn subscribe_to_buffer(
    client: &mut Client,
    state: &mut AppState,
    open: BufferOpenResult,
) -> Result<()> {
    // Inherit wrap from the current editor so switching buffers keeps the user's wrap setting.
    let wrap = state.ed_mut().wrap;
    // Snapshot labels before passing into the builder so we can hand the builder pure slices —
    // it doesn't need (and shouldn't borrow) the full AppState.
    let project_paths = state.project_paths.clone();
    let root_labels = state.root_labels.clone();
    state.editor = Some(
        build_editor_state_from_open(
            client,
            state.viewport_cols,
            state.viewport_rows,
            &project_paths,
            &root_labels,
            open,
            wrap,
        )
        .await?,
    );
    apply_cursor_style(state);
    // Cover the case where the restored scroll disagrees with the cursor (e.g. a `jump_to`
    // override on a buffer we've already opened before, so the stored scroll wasn't computed
    // around the new cursor). Cheap when the cursor is already visible — no RPC.
    ensure_cursor_in_window(client, state).await
}

/// Switch to a freshly opened *file* buffer, then discard the empty placeholder scratch we were on
/// (if any). Opening a file replaces the placeholder; the buffer-switch path (`attach_buffer`)
/// deliberately leaves the scratch alone — it's navigation among the already-open buffers.
async fn subscribe_replacing_scratch(
    client: &mut Client,
    state: &mut AppState,
    open: BufferOpenResult,
) -> Result<()> {
    // Capture the scratch we're leaving before `subscribe_to_buffer` replaces the editor. Skip it
    // if the buffer we're opening somehow *is* it (defensive — a file buffer has a path).
    let leaving_scratch = empty_scratch_id(state).filter(|&id| id != open.buffer_id);
    subscribe_to_buffer(client, state, open).await?;
    // Drop the leftover scratch now that the editor has moved to the file; the server prunes it
    // from the project MRU. Best-effort — a failure (e.g. it raced closed) is harmless.
    if let Some(scratch_id) = leaving_scratch {
        let _ = client
            .rpc::<BufferClose>(BufferCloseParams {
                buffer_id: scratch_id,
            })
            .await;
    }
    Ok(())
}

/// Format `abs` as `"{root_label}: {relative}"` against the longest-matching project root, or
/// fall back to the raw absolute path when nothing matches. `root_labels` must be aligned by
/// index with `project_paths`. The label is always included (even single-root) so the format is
/// consistent across surfaces. Use this for display — see `project_relative_path` for the
/// typeable-path variant that the save-as prefill needs.
fn project_relative_label(abs: &str, project_paths: &[String], root_labels: &[String]) -> String {
    match strip_longest_root(abs, project_paths) {
        Some((i, rel)) => {
            let label = root_labels.get(i).map(String::as_str).unwrap_or("");
            if rel.is_empty() {
                label.to_string()
            } else if label.is_empty() {
                rel
            } else {
                format!("{label}: {rel}")
            }
        }
        None => abs.to_string(),
    }
}

/// Resolve a CLI-supplied file/dir argument to an absolute, canonical path. Relative args are
/// resolved against the *current working directory* (shell convention), then canonicalized so
/// any `..` / symlinks line up with the project's canonical roots — without that, prefix
/// matching against the roots in `strip_longest_root` would miss when the user CDs through a
/// symlink. Errors surface verbatim (e.g. "No such file or directory") with the original arg
/// for context — keeps the CLI's failure mode readable.
fn resolve_cli_path(arg: &str) -> Result<std::path::PathBuf> {
    let raw = std::path::Path::new(arg);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        let cwd = std::env::current_dir()
            .context("could not read current directory to resolve a relative CLI path")?;
        cwd.join(raw)
    };
    std::fs::canonicalize(&joined).with_context(|| format!("could not resolve {arg}"))
}

/// Strip the longest matching project root off `abs`. Returns `(root_index, relative_path)`,
/// where `relative_path` is empty if `abs` *is* the root itself.
pub(crate) fn strip_longest_root(abs: &str, project_paths: &[String]) -> Option<(usize, String)> {
    let abs_path = std::path::Path::new(abs);
    project_paths
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let root = std::path::Path::new(p);
            abs_path
                .strip_prefix(root)
                .ok()
                .map(|rel| (i, root.as_os_str().len(), rel.display().to_string()))
        })
        .max_by_key(|(_, root_len, _)| *root_len)
        .map(|(i, _, rel)| (i, rel))
}

/// Drill into the highlighted LSP server's status/error detail (`Enter` in the LSP-servers picker).
/// A snapshot from the row, which already carries the server's `status` (incl. any crash message).
fn open_lsp_detail(state: &mut AppState) {
    if let Some(PickerItem::LspServer { name, language, workspace_root, status, .. }) =
        state.picker.highlighted()
    {
        state.picker.lsp_detail = Some(crate::picker::LspServerDetail {
            name: name.clone(),
            language: language.clone(),
            workspace_root: workspace_root.clone(),
            status: status.clone(),
            scroll: crate::scroll::ScrollState::default(),
        });
    }
}

fn detail_scroll(state: &mut AppState, delta: i32) {
    if let Some(d) = state.picker.lsp_detail.as_mut() {
        d.scroll.scroll_by(delta);
    }
}

fn detail_page(state: &mut AppState, down: bool) {
    if let Some(d) = state.picker.lsp_detail.as_mut() {
        d.scroll.page(down);
    }
}

/// Restart the language server highlighted in the LSP-servers picker (`Ctrl-r`). Fire-and-forget
/// against `lsp/restart_server`; the dialog stays open and the server's `lsp/status_changed`
/// pushes drive the live re-render of its health glyph. A no-op when the highlighted row isn't a
/// server (wrong picker kind / empty list).
async fn lsp_picker_restart(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(PickerItem::LspServer { language, name, .. }) = state.picker.highlighted() else {
        return Ok(());
    };
    let language = language.clone();
    let name = name.clone();
    client
        .rpc::<LspRestartServer>(LspRestartServerParams { language })
        .await?;
    state.status = StatusMessage::info(format!("restarting {name}"));
    Ok(())
}

async fn hide_picker(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(kind) = state.picker.kind else {
        return Ok(());
    };
    // Persist the highlight so the next open resumes here — only for kinds that preserve
    // state. The server's own per-picker state is independent: we always send `picker/hide`
    // so it stops pushing; whether to reset on next open is decided in `open_picker`.
    if kind.preserves_state() {
        if let Some(item) = state.picker.highlighted().cloned() {
            let row_offset = state
                .picker
                .selected
                .saturating_sub(state.picker.visible_start);
            state.picker.last_selected.insert(kind, (item, row_offset));
        }
    }
    let _ = client.rpc::<PickerHide>(PickerHideParams { kind }).await;
    state.picker.open = false;
    state.picker.pending_offset = None;
    state.picker.pending_delete = None;
    state.picker.lsp_detail = None;
    apply_cursor_style(state);
    Ok(())
}

async fn handle_insert_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    // Pending `Ctrl-s`: the next keystroke names the surround delimiter (line-scoped in Insert).
    // Use the raw key so the delimiter isn't consumed as text. Non-`Char` keys cancel.
    if let Some(target) = state.ed_mut().pending_surround.take() {
        if let KeyCode::Char(delimiter) = k.code {
            surround(client, state, delimiter, target).await?;
        }
        return Ok(());
    }

    let (code, mods) = normalize_key(k);
    // Shared Ctrl editing shortcuts (`Global` table) first — mode-specific divergences live inside
    // the wrappers (handle_copy / handle_cut / etc.). Count is hardcoded to 1 in Insert (no
    // pending_count accumulator). Then the Insert-specific table (Esc, arrows, Enter, …).
    if let Some(b) = keymap::lookup(keymap::KeyContext::Global, code, mods) {
        return run_action(client, state, b.action, 1, false).await;
    }
    if let Some(b) = keymap::lookup(keymap::KeyContext::Insert, code, mods) {
        return run_action(client, state, b.action, 1, false).await;
    }
    // Anything else that's a bare printable char is literal text.
    if let KeyCode::Char(c) = code {
        if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT) {
            // `normalize_key` lowercased the char and synthesised SHIFT so the Ctrl-* bindings
            // above can match consistently. Reverse that for actual text insertion.
            let c = if mods.contains(KeyModifiers::SHIFT) {
                c.to_ascii_uppercase()
            } else {
                c
            };
            insert_text(client, state, &c.to_string()).await?;
        }
    }
    Ok(())
}

/// Switch the active buffer to the file at `abs_path`. Resolves the path against the
/// configured project roots, subscribes a fresh viewport, and replaces `state.editor` with
/// the new editor. The previous buffer stays loaded server-side so the user can switch back
/// via `Space b`. Called from picker selections (Files + Explorer + Grep).
async fn open_file_at_path(
    client: &mut Client,
    state: &mut AppState,
    abs_path: String,
    create_if_missing: bool,
    jump_to: Option<aether_protocol::LogicalPosition>,
) -> Result<()> {
    // Opening a file is a jump (every caller here is a user navigation: goto-def, grep nav, file/
    // grep/explorer/diagnostics picker). Record where we're leaving onto the jump list first.
    record_nav(client, state).await;
    // Find a `path_index` + `relative_path` pair the server will accept. Each project path is
    // either a file or a directory; we want the directory that contains the target.
    let target = std::path::PathBuf::from(&abs_path);
    let (path_index, relative) = state
        .project_paths
        .iter()
        .enumerate()
        .find_map(|(i, p)| {
            let project_root = std::path::PathBuf::from(p);
            target
                .strip_prefix(&project_root)
                .ok()
                .map(|rel| (i as u32, rel.display().to_string()))
        })
        .ok_or_else(|| anyhow::anyhow!("file {} is outside any project path", abs_path))?;

    let open: BufferOpenResult = client
        .rpc::<BufferOpen>(BufferOpenParams {
            buffer_id: None,
            path_index: Some(path_index),
            relative_path: Some(relative),
            language: None,
            create_if_missing,
            jump_to,
        })
        .await?;
    subscribe_replacing_scratch(client, state, open).await
}

/// `Space k` — request hover info for the symbol at the cursor and show it in a popup. Empty
/// result → a transient status note rather than an empty box.
async fn lsp_hover(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(buffer_id) = state.editor.as_ref().map(|e| e.buffer_id) else {
        return Ok(());
    };
    match client
        .rpc::<aether_protocol::lsp::LspHover>(aether_protocol::lsp::LspBufferParams { buffer_id })
        .await
    {
        Ok(r) => match r.contents {
            Some(text) => state.hover = Some(HoverPopup::plain(text)),
            None => {
                state.hover = None;
                state.status = StatusMessage::info("No hover info");
            }
        },
        Err(e) => state.status = StatusMessage::error(format!("hover failed: {e}")),
    }
    Ok(())
}

/// `Space d` — resolve the definition of the symbol at the cursor and jump to it (opening the file
/// if needed). Out-of-project targets (e.g. dependencies) can't be opened yet — surfaced as a note.
async fn lsp_goto_definition(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(buffer_id) = state.editor.as_ref().map(|e| e.buffer_id) else {
        return Ok(());
    };
    match client
        .rpc::<aether_protocol::lsp::LspGotoDefinition>(aether_protocol::lsp::LspBufferParams {
            buffer_id,
        })
        .await
    {
        Ok(r) => match r.location {
            Some(loc) => {
                if let Err(e) =
                    open_file_at_path(client, state, loc.path.clone(), false, Some(loc.position)).await
                {
                    state.status =
                        StatusMessage::warning(format!("definition at {}: {e}", loc.path));
                }
            }
            None => state.status = StatusMessage::info("No definition found"),
        },
        Err(e) => state.status = StatusMessage::error(format!("goto definition failed: {e}")),
    }
    Ok(())
}

/// `Space j` — show the diagnostic(s) at the cursor in the hover box (reusing the hover rendering).
/// Prefers diagnostics under the cursor column, falling back to all on the cursor's line. Reads the
/// cached window render, so it needs no server round-trip.
fn show_diagnostic(state: &mut AppState) {
    let Some(ed) = state.editor.as_ref() else {
        return;
    };
    let cursor = ed.cursor.position;
    let local = (cursor.line as i64) - (ed.window_first_logical_line as i64);
    let diags: Vec<(DiagnosticSeverity, String)> = if local >= 0 && (local as usize) < ed.lines.len()
    {
        let line = &ed.lines[local as usize];
        let under: Vec<(DiagnosticSeverity, String)> = line
            .diagnostics
            .iter()
            .filter(|d| cursor.col >= d.start && cursor.col <= d.end)
            .map(|d| (d.severity, d.message.clone()))
            .collect();
        if under.is_empty() {
            line.diagnostics
                .iter()
                .map(|d| (d.severity, d.message.clone()))
                .collect()
        } else {
            under
        }
    } else {
        Vec::new()
    };
    if diags.is_empty() {
        state.status = StatusMessage::info("No diagnostics on this line");
        return;
    }
    let blocks = diags
        .into_iter()
        .map(|(severity, msg)| HoverBlock {
            text: format!("{}: {msg}", severity_label(severity)),
            severity: Some(severity),
        })
        .collect();
    state.hover = Some(HoverPopup::from_blocks(blocks));
}

fn severity_label(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "Error",
        DiagnosticSeverity::Warning => "Warning",
        DiagnosticSeverity::Information => "Info",
        DiagnosticSeverity::Hint => "Hint",
    }
}

async fn handle_search_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    // Don't `normalize_key` here — that lowercases uppercase chars and synthesises SHIFT, which
    // is what Normal-mode keymaps want but would strip case from the literal search query. The
    // `Search` table only holds the editing/control keys; printable chars fall through to the
    // query-insert tail below.
    if let Some(b) = keymap::lookup(keymap::KeyContext::Search, k.code, k.modifiers) {
        return run_action(client, state, b.action, 1, false).await;
    }
    if let KeyCode::Char(c) = k.code {
        if !k.modifiers.contains(KeyModifiers::CONTROL) && !k.modifiers.contains(KeyModifiers::ALT)
        {
            state.ed_mut().search.query.insert_char(c);
            state.ed_mut().search.history_cursor = None;
            run_incremental_search(client, state).await?;
        }
    }
    Ok(())
}

async fn enter_search_mode(client: &mut Client, state: &mut AppState) -> Result<()> {
    state.ed_mut().search.snapshot = Some(SearchSnapshot {
        cursor: state.ed_mut().cursor,
        scroll_logical_line: state.ed_mut().scroll_logical_line,
        query: state.ed_mut().search.query.take_text(),
        active: state.ed_mut().search.active,
    });
    state.ed_mut().search.active = false;
    state.ed_mut().search.summary = None;
    {
        let ed = state.ed_mut();
        ed.search.history_cursor = None;
        ed.search.history_draft.clear();
        ed.search.extend_to_cursor = false;
        ed.mode = EditorMode::Search;
    }
    apply_cursor_style(state);
    // Clear the server-side search so highlights disappear immediately. Restored on Esc.
    let buffer_id = state.ed_mut().buffer_id;
    let _ = client
        .rpc::<SearchClear>(SearchClearParams { buffer_id })
        .await;
    Ok(())
}

fn commit_search(state: &mut AppState) {
    let committed_query = {
        let ed = state.ed_mut();
        ed.search.snapshot = None;
        if !ed.search.query.is_empty() {
            ed.search.active = true;
            Some(ed.search.query.text.clone())
        } else {
            ed.search.active = false;
            ed.search.summary = None;
            None
        }
    };
    if let Some(q) = committed_query {
        push_history(state, q);
    }
    let ed = state.ed_mut();
    ed.search.history_cursor = None;
    ed.search.history_draft.clear();
    ed.search.extend_to_cursor = false;
    ed.mode = EditorMode::Normal;
    apply_cursor_style(state);
}

const SEARCH_HISTORY_MAX: usize = 100;

fn push_history(state: &mut AppState, query: String) {
    if query.is_empty() {
        return;
    }
    let ed = state.ed_mut();
    if ed.search.history.last() == Some(&query) {
        return; // dedup consecutive duplicates
    }
    ed.search.history.push(query);
    let overflow = ed.search.history.len().saturating_sub(SEARCH_HISTORY_MAX);
    if overflow > 0 {
        ed.search.history.drain(..overflow);
    }
}

fn history_up(state: &mut AppState) {
    let ed = state.ed_mut();
    if ed.search.history.is_empty() {
        return;
    }
    let new_idx = match ed.search.history_cursor {
        None => {
            ed.search.history_draft = ed.search.query.text.clone();
            ed.search.history.len() - 1
        }
        Some(0) => 0,
        Some(i) => i - 1,
    };
    ed.search.history_cursor = Some(new_idx);
    let entry = ed.search.history[new_idx].clone();
    ed.search.query.set(entry);
}

fn history_down(state: &mut AppState) {
    let ed = state.ed_mut();
    match ed.search.history_cursor {
        None => {} // already past the newest entry
        Some(i) if i + 1 < ed.search.history.len() => {
            ed.search.history_cursor = Some(i + 1);
            let entry = ed.search.history[i + 1].clone();
            ed.search.query.set(entry);
        }
        Some(_) => {
            ed.search.history_cursor = None;
            let draft = std::mem::take(&mut ed.search.history_draft);
            ed.search.query.set(draft);
        }
    }
}

async fn abort_search(client: &mut Client, state: &mut AppState) -> Result<()> {
    let Some(snap) = state.ed_mut().search.snapshot.take() else {
        state.ed_mut().mode = EditorMode::Normal;
        apply_cursor_style(state);
        return Ok(());
    };
    // Restore the prior server-side search query (if any). Done before cursor restoration so the
    // server's view of "current_index" matches once we move the cursor back.
    if snap.active && !snap.query.is_empty() {
        let r = client
            .rpc::<SearchSet>(SearchSetParams {
                buffer_id: state.ed_mut().buffer_id,
                query: snap.query.clone(),
                anchor: None,
                extend: false,
            })
            .await?;
        state.ed_mut().search.summary = Some(r.summary);
    } else {
        let _ = client
            .rpc::<SearchClear>(SearchClearParams {
                buffer_id: state.ed_mut().buffer_id,
            })
            .await;
        state.ed_mut().search.summary = None;
    }
    state.ed_mut().search.query.set(snap.query);
    state.ed_mut().search.active = snap.active;
    // Restore cursor + selection.
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.ed_mut().buffer_id,
            position: snap.cursor.position,
            anchor: snap.cursor.anchor,
        })
        .await?;
    state.ed_mut().cursor = new;
    // Restore scroll if it moved during incremental search.
    if snap.scroll_logical_line != state.ed_mut().scroll_logical_line {
        scroll_to(client, state, snap.scroll_logical_line).await?;
    }
    state.ed_mut().search.extend_to_cursor = false;
    state.ed_mut().mode = EditorMode::Normal;
    apply_cursor_style(state);
    Ok(())
}

/// Incremental-search step: tell the server the latest query and let it jump the cursor onto
/// the first match at-or-after where `/` was pressed. The server's response carries the new
/// cursor + summary; per-viewport highlight notifications follow asynchronously.
async fn run_incremental_search(client: &mut Client, state: &mut AppState) -> Result<()> {
    if state.ed_mut().search.query.is_empty() {
        let _ = client
            .rpc::<SearchClear>(SearchClearParams {
                buffer_id: state.ed_mut().buffer_id,
            })
            .await;
        state.ed_mut().search.summary = None;
        // No matches — revert the cursor to the pre-search position so the user sees where
        // they started rather than wherever the previous query stranded them.
        if let Some(snap_cursor) = state.ed_mut().search.snapshot.as_ref().map(|s| s.cursor) {
            if state.ed_mut().cursor.position != snap_cursor.position
                || state.ed_mut().cursor.anchor != snap_cursor.anchor
            {
                let new = client
                    .rpc::<CursorSet>(CursorSetParams {
                        buffer_id: state.ed_mut().buffer_id,
                        position: snap_cursor.position,
                        anchor: snap_cursor.anchor,
                    })
                    .await?;
                state.ed_mut().cursor = new;
            }
        }
        return Ok(());
    }
    let anchor = state
        .ed()
        .search
        .snapshot
        .as_ref()
        .map(|s| selection_start(&s.cursor));
    let (buffer_id, query, extend) = {
        let ed = state.ed_mut();
        (
            ed.buffer_id,
            ed.search.query.text.clone(),
            ed.search.extend_to_cursor,
        )
    };
    let result = client
        .rpc::<SearchSet>(SearchSetParams {
            buffer_id,
            query,
            anchor,
            extend,
        })
        .await;
    let revert_needed = match result {
        Ok(r) => {
            state.ed_mut().cursor = r.cursor;
            state.ed_mut().search.summary = Some(r.summary.clone());
            // Zero matches: revert below so a failed keystroke doesn't strand the user.
            r.summary.total == 0
        }
        Err(_) => {
            // Most commonly an invalid regex while the user is mid-type (e.g. a trailing `\`).
            // Treat it as a transient "no matches" state — empty highlights, cursor reverted,
            // a short note in the status so the user knows why their search isn't matching.
            state.ed_mut().search.summary = Some(SearchSummary {
                buffer_id: state.ed_mut().buffer_id,
                total: 0,
                truncated: false,
                current_index: 0,
            });
            state.status = StatusMessage::warning("invalid regex");
            true
        }
    };
    if revert_needed {
        if let Some(snap_cursor) = state.ed_mut().search.snapshot.as_ref().map(|s| s.cursor) {
            if state.ed_mut().cursor.position != snap_cursor.position
                || state.ed_mut().cursor.anchor != snap_cursor.anchor
            {
                let new = client
                    .rpc::<CursorSet>(CursorSetParams {
                        buffer_id: state.ed_mut().buffer_id,
                        position: snap_cursor.position,
                        anchor: snap_cursor.anchor,
                    })
                    .await?;
                state.ed_mut().cursor = new;
            }
        }
    }
    Ok(())
}

fn selection_start(c: &CursorState) -> LogicalPosition {
    if pos_tuple(c.anchor) < pos_tuple(c.position) {
        c.anchor
    } else {
        c.position
    }
}

fn pos_tuple(p: LogicalPosition) -> (u32, u32) {
    (p.line, p.col)
}

/// `Some("3/47")` when a search is active and the server says the cursor is currently on a match
/// (i.e., `current_index != 0`). The status bar only shows the counter when the cursor is
/// meaningfully "on" a result. The total gets a trailing `+` if the server truncated.
pub fn search_counter_label(state: &AppState) -> Option<String> {
    let ed = state.ed();
    if !ed.search.active {
        return None;
    }
    let summary = ed.search.summary.as_ref()?;
    if summary.current_index == 0 || summary.total == 0 {
        return None;
    }
    Some(format!(
        "{}/{}",
        summary.current_index,
        format_total(summary)
    ))
}

fn format_total(s: &SearchSummary) -> String {
    if s.truncated {
        format!("{}+", s.total)
    } else {
        s.total.to_string()
    }
}

/// `Some("(3/12)")` when the server reports the cursor is currently on a cached grep hit, paired
/// with the total hit count across the workspace. `None` whenever there's no cached grep or the
/// cursor isn't on a hit — the status bar then renders nothing for the grep slot, matching the
/// "hide when not on a match" treatment for the in-buffer search counter.
pub fn grep_counter_label(state: &AppState) -> Option<String> {
    let gp = state.ed().cursor.grep_position?;
    Some(format!("({}/{})", gp.current, gp.total))
}

/// Summary line for the search prompt: "3/47", "3/10000+", or "no matches". `None` when the
/// query is empty (the bare `/` already conveys "no search yet").
pub fn search_match_count_label(state: &AppState) -> Option<String> {
    let ed = state.ed();
    if ed.search.query.is_empty() {
        return None;
    }
    let summary = ed.search.summary.as_ref()?;
    if summary.total == 0 {
        return Some(String::from("no matches"));
    }
    let total = format_total(summary);
    Some(if summary.current_index == 0 {
        total
    } else {
        format!("{}/{total}", summary.current_index)
    })
}

/// Take the current selection's text, escape its regex metacharacters, and use it as the active
/// search term. The cursor stays on the original selection — `n` / `Alt-n` then cycle from there.
async fn search_from_selection(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: BufferCopyResult = client
        .rpc::<BufferCopy>(BufferCopyParams {
            buffer_id: state.ed_mut().buffer_id,
            scope: CopyScope::Selection,
        })
        .await?;
    if r.text.is_empty() {
        return Ok(());
    }
    let query = {
        let ed = state.ed_mut();
        ed.search.query.set(regex_escape(&r.text));
        ed.search.active = true;
        ed.search.query.text.clone()
    };
    push_history(state, query.clone());
    let buffer_id = state.ed_mut().buffer_id;
    let result = client
        .rpc::<SearchSet>(SearchSetParams {
            buffer_id,
            query: query.clone(),
            anchor: None,
            extend: false,
        })
        .await?;
    state.ed_mut().search.summary = Some(result.summary);
    // search/set with anchor=None doesn't move the cursor server-side, so state.ed_mut().cursor is still
    // valid (mirrors the selection that prompted the search).
    Ok(())
}

/// Escape regex metacharacters so a literal string can be embedded in the search regex. Mirrors
/// `regex::escape` (we don't pull `regex` into the TUI just for this one call).
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
                | '&'
                | '-'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn search_cycle(
    client: &mut Client,
    state: &mut AppState,
    direction: Direction,
    count: u32,
    extend: bool,
) -> Result<()> {
    if !state.ed_mut().search.active {
        // No active search: revive the most recent history entry server-side, then cycle.
        let Some(last) = state.ed_mut().search.history.last().cloned() else {
            return Ok(());
        };
        state.ed_mut().search.query.set(last.clone());
        let r = client
            .rpc::<SearchSet>(SearchSetParams {
                buffer_id: state.ed_mut().buffer_id,
                query: last,
                anchor: None,
                extend: false,
            })
            .await?;
        state.ed_mut().cursor = r.cursor;
        state.ed_mut().search.summary = Some(r.summary);
        state.ed_mut().search.active = true;
    }
    let summary_total = state
        .ed()
        .search
        .summary
        .as_ref()
        .map(|s| s.total)
        .unwrap_or(0);
    if summary_total == 0 {
        return Ok(());
    }
    for _ in 0..count.max(1) {
        let params = SearchNavParams {
            buffer_id: state.ed_mut().buffer_id,
            extend,
        };
        let result = match direction {
            Direction::Forward => client.rpc::<SearchNext>(params).await?,
            Direction::Backward => client.rpc::<SearchPrev>(params).await?,
        };
        state.ed_mut().cursor = result.cursor;
        state.ed_mut().search.summary = Some(result.summary);
    }
    Ok(())
}

async fn handle_resize(
    client: &mut Client,
    state: &mut AppState,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let viewport_rows = rows.saturating_sub(1) as u32;
    state.viewport_cols = (cols as u32).saturating_sub(crate::ui::GUTTER_WIDTH as u32);
    state.viewport_rows = viewport_rows;
    let viewport_id = state.ed_mut().viewport_id;
    let r = client
        .rpc::<ViewportResize>(ViewportResizeParams {
            viewport_id,
            cols: state.viewport_cols,
            rows: viewport_rows,
        })
        .await?;
    let ed = state.ed_mut();
    ed.window_first_logical_line = r.window.first_logical_line;
    ed.line_count = r.window.line_count;
    ed.max_scroll_logical_line = r.window.max_scroll_logical_line;
    ed.lines = r.window.lines;
    ed.scroll_skip_rows = 0; // visual heights changed; re-align the top line

    // If the picker is open, the resize changed how many result rows fit. Re-subscribe with the
    // new `limit`, keeping the current `offset`. The server's next push uses the new window.
    if state.picker.open {
        if let Some(kind) = state.picker.kind {
            let new_pane_rows = picker_pane_rows(state);
            let new_limit = picker_fetch_limit(state);
            let view = client
                .rpc::<PickerView>(PickerViewParams {
                    kind,
                    reset: false,
                    offset: state.picker.offset,
                    limit: new_limit,
                    center_on: None,
                    center_on_cursor_grep_hit: None,
                    directory_path: None,
                    buffer_id: None,
                    explorer_roots: false,
                })
                .await?;
            state.picker.limit = new_limit;
            state.picker.pane_rows = new_pane_rows;
            state.picker.offset = view.effective_offset;
        }
    }
    Ok(())
}

async fn move_motion(
    client: &mut Client,
    state: &mut AppState,
    motion: Motion,
    extend: bool,
) -> Result<()> {
    let new: CursorState = client
        .rpc::<CursorMove>(CursorMoveParams {
            buffer_id: state.ed_mut().buffer_id,
            motion,
            extend_selection: extend,
        })
        .await?;
    state.ed_mut().cursor = new;
    // Repeat (`r`) is recorded by the caller at the `Action` layer (see `run_action`) and at the
    // find-char capture site — not here — so this funnel stays a pure cursor move.
    Ok(())
}

async fn select_line(
    client: &mut Client,
    state: &mut AppState,
    direction: Direction,
    extend: bool,
    count: u32,
) -> Result<()> {
    for _ in 0..count.max(1) {
        let new = client
            .rpc::<CursorSelectLine>(CursorSelectLineParams {
                buffer_id: state.ed_mut().buffer_id,
                direction,
                extend,
            })
            .await?;
        state.ed_mut().cursor = new;
    }
    Ok(())
}

async fn tree_expand(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let new = client
            .rpc::<CursorExpand>(CursorBufferOnlyParams {
                buffer_id: state.ed_mut().buffer_id,
            })
            .await?;
        if new == state.ed_mut().cursor {
            break; // already at root
        }
        state.ed_mut().cursor = new;
    }
    Ok(())
}

async fn tree_contract(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let new = client
            .rpc::<CursorContract>(CursorBufferOnlyParams {
                buffer_id: state.ed_mut().buffer_id,
            })
            .await?;
        if new == state.ed_mut().cursor {
            break; // history empty
        }
        state.ed_mut().cursor = new;
    }
    Ok(())
}

async fn swap_anchor(client: &mut Client, state: &mut AppState) -> Result<()> {
    let new = client
        .rpc::<CursorSwapAnchor>(CursorSwapAnchorParams {
            buffer_id: state.ed_mut().buffer_id,
        })
        .await?;
    state.ed_mut().cursor = new;
    Ok(())
}

async fn motion_undo(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: CursorUndoResult = client
            .rpc::<CursorUndo>(CursorUndoParams {
                buffer_id: state.ed_mut().buffer_id,
            })
            .await?;
        let applied = r.applied;
        apply_motion_undo_result(state, r, "motion undo");
        if !applied {
            break;
        }
    }
    Ok(())
}

async fn motion_redo(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: CursorUndoResult = client
            .rpc::<CursorRedo>(CursorUndoParams {
                buffer_id: state.ed_mut().buffer_id,
            })
            .await?;
        let applied = r.applied;
        apply_motion_undo_result(state, r, "motion redo");
        if !applied {
            break;
        }
    }
    Ok(())
}

fn apply_motion_undo_result(state: &mut AppState, r: CursorUndoResult, label: &str) {
    if r.applied {
        state.ed_mut().cursor = r.cursor;
    } else {
        state.status = StatusMessage::info(format!("nothing to {label}"));
    }
}

async fn clear_selection(client: &mut Client, state: &mut AppState) -> Result<()> {
    // "Clear selection" now means "collapse to a 1-char point at the current position" since
    // the data model always has an anchor. Visually unchanged: the block cursor stays put.
    let pos = state.ed_mut().cursor.position;
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.ed_mut().buffer_id,
            position: pos,
            anchor: pos,
        })
        .await?;
    state.ed_mut().cursor = new;
    Ok(())
}

async fn enter_insert_at(
    client: &mut Client,
    state: &mut AppState,
    where_: InsertWhere,
) -> Result<()> {
    // Entering Insert mode always collapses to a 1-char point at the chosen position. The
    // model invariant is that Insert mode never has a multi-char selection — typing inserts at
    // `position`, motion-based deletes (Backspace/Delete) ignore the anchor.
    let (pos, cursor, buffer_id) = {
        let ed = state.ed_mut();
        (ed.cursor.position, ed.cursor, ed.buffer_id)
    };
    let target = match where_ {
        InsertWhere::SelectionStart => min_pos(pos, cursor.anchor),
        InsertWhere::SelectionEnd => {
            // Just past the (possibly multi-char) selection — set to the max position, then
            // step one char forward server-side to handle multi-byte chars / end-of-line.
            let max = max_pos(pos, cursor.anchor);
            let _ = client
                .rpc::<CursorSet>(CursorSetParams {
                    buffer_id,
                    position: max,
                    anchor: max,
                })
                .await?;
            let new = client
                .rpc::<CursorMove>(CursorMoveParams {
                    buffer_id,
                    motion: Motion::Char {
                        direction: Direction::Forward,
                        count: 1,
                    },
                    extend_selection: false,
                })
                .await?;
            state.ed_mut().cursor = new;
            enter_insert_mode(state);
            return Ok(());
        }
        InsertWhere::FirstLineStart => {
            let line = pos.line.min(cursor.anchor.line);
            LogicalPosition { line, col: 0 }
        }
        InsertWhere::LastLineEnd => {
            let line = pos.line.max(cursor.anchor.line);
            LogicalPosition {
                line,
                col: u32::MAX,
            }
        }
    };
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id,
            position: target,
            anchor: target,
        })
        .await?;
    state.ed_mut().cursor = new;
    enter_insert_mode(state);
    Ok(())
}

fn enter_insert_mode(state: &mut AppState) {
    state.ed_mut().mode = EditorMode::Insert;
    apply_cursor_style(state);
}

fn leave_insert(state: &mut AppState) {
    state.ed_mut().mode = EditorMode::Normal;
    apply_cursor_style(state);
}

fn min_pos(a: LogicalPosition, b: LogicalPosition) -> LogicalPosition {
    if (a.line, a.col) <= (b.line, b.col) {
        a
    } else {
        b
    }
}

fn max_pos(a: LogicalPosition, b: LogicalPosition) -> LogicalPosition {
    if (a.line, a.col) >= (b.line, b.col) {
        a
    } else {
        b
    }
}

async fn insert_text(client: &mut Client, state: &mut AppState, text: &str) -> Result<()> {
    insert_text_inner(client, state, text, false).await
}

/// Server-side smart indent: insert `\n` + indent computed from the cursor's context (current
/// line's leading whitespace, plus one level if the cursor sits right after an opening bracket
/// outside a string/comment).
async fn newline_and_indent(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputNewlineAndIndent>(BufferOnlyParams {
            buffer_id: state.ed_mut().buffer_id,
        })
        .await?;
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    Ok(())
}

async fn insert_text_inner(
    client: &mut Client,
    state: &mut AppState,
    text: &str,
    select_pasted: bool,
) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputText>(InputTextParams {
            buffer_id: state.ed_mut().buffer_id,
            text: text.into(),
            select_pasted,
        })
        .await?;
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    Ok(())
}

/// Delete the current selection (or the 1-char range at the cursor when there's no anchor) and
/// enter Insert mode — the "change" operator. Server-side `apply_edit` treats the selection as
async fn change_selection(client: &mut Client, state: &mut AppState) -> Result<()> {
    delete_selection(client, state).await?;
    enter_insert_mode(state);
    Ok(())
}

async fn delete_selection(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputDelete>(BufferOnlyParams {
            buffer_id: state.ed_mut().buffer_id,
        })
        .await?;
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    Ok(())
}

async fn backspace(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputBackspace>(BufferOnlyParams {
            buffer_id: state.ed_mut().buffer_id,
        })
        .await?;
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    Ok(())
}

async fn surround(
    client: &mut Client,
    state: &mut AppState,
    delimiter: char,
    target: SurroundTarget,
) -> Result<()> {
    let r: EditResult = client
        .rpc::<aether_protocol::input::InputSurround>(InputSurroundParams {
            buffer_id: state.ed_mut().buffer_id,
            delimiter,
            target,
        })
        .await?;
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    Ok(())
}

async fn unsurround(
    client: &mut Client,
    state: &mut AppState,
    target: SurroundTarget,
) -> Result<()> {
    let r: EditResult = client
        .rpc::<aether_protocol::input::InputUnsurround>(InputUnsurroundParams {
            buffer_id: state.ed_mut().buffer_id,
            target,
        })
        .await?;
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    Ok(())
}

async fn delete_line(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<aether_protocol::input::InputDeleteLine>(BufferOnlyParams {
            buffer_id: state.ed_mut().buffer_id,
        })
        .await?;
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    Ok(())
}

async fn change_line(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<aether_protocol::input::InputChangeLine>(BufferOnlyParams {
            buffer_id: state.ed_mut().buffer_id,
        })
        .await?;
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    Ok(())
}

async fn replace_line_with_clipboard(client: &mut Client, state: &mut AppState) -> Result<()> {
    let text = match clipboard::paste(&mut state.clipboard) {
        Ok(t) => t,
        Err(e) => {
            state.status = StatusMessage::error(format!("clipboard read failed: {e}"));
            return Ok(());
        }
    };
    let r: EditResult = client
        .rpc::<aether_protocol::input::InputReplaceLine>(
            aether_protocol::input::InputReplaceLineParams {
                buffer_id: state.ed_mut().buffer_id,
                text,
            },
        )
        .await?;
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    Ok(())
}

// The selection/clipboard Ctrl shortcuts no longer branch on mode here: each mode binds its own
// action (`Copy`/`DeleteSelection`/… in Normal, `CopyLine`/`DeleteLine`/… in Insert), so
// `run_action` dispatches straight to `copy_to_clipboard(Selection)` / `delete_line` / etc.

async fn join_lines(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: EditResult = client
            .rpc::<InputJoinLines>(BufferOnlyParams {
                buffer_id: state.ed_mut().buffer_id,
            })
            .await?;
        state.ed_mut().revision = r.revision;
        state.ed_mut().cursor = r.cursor;
    }
    Ok(())
}

async fn indent(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: EditResult = client
            .rpc::<InputIndent>(BufferOnlyParams {
                buffer_id: state.ed_mut().buffer_id,
            })
            .await?;
        state.ed_mut().revision = r.revision;
        state.ed_mut().cursor = r.cursor;
    }
    Ok(())
}

async fn dedent(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: EditResult = client
            .rpc::<InputDedent>(BufferOnlyParams {
                buffer_id: state.ed_mut().buffer_id,
            })
            .await?;
        state.ed_mut().revision = r.revision;
        state.ed_mut().cursor = r.cursor;
    }
    Ok(())
}

/// Toggle line-comment status on the cursor's line (or all selected lines). Server picks the
/// prefix from the buffer language's `line_comment` and no-ops for languages without one.
async fn toggle_comment(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputToggleComment>(BufferOnlyParams {
            buffer_id: state.ed_mut().buffer_id,
        })
        .await?;
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    Ok(())
}

/// Add a blank line after the cursor's current line and drop into Insert mode at its start.
/// Implemented as: park cursor at end of current line, then `newline_and_indent` (which copies
/// the line's leading whitespace and adds one level if the line ends in an opener). The newline
/// pushes the cursor onto the new line at the indent column.
async fn open_line_below(client: &mut Client, state: &mut AppState) -> Result<()> {
    let line = state.ed_mut().cursor.position.line;
    let target = LogicalPosition {
        line,
        col: u32::MAX,
    };
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.ed_mut().buffer_id,
            position: target,
            anchor: target,
        })
        .await?;
    state.ed_mut().cursor = new;
    newline_and_indent(client, state).await?;
    enter_insert_mode(state);
    Ok(())
}

/// Insert a blank line *above* the cursor's current line and drop into Insert mode on it.
/// Park at col 0 of the current line, insert "\n" (which pushes the original line down a row
/// and lands the cursor at its new start), then step back up onto the freshly-blank line.
async fn open_line_above(client: &mut Client, state: &mut AppState) -> Result<()> {
    let line = state.ed_mut().cursor.position.line;
    let target = LogicalPosition { line, col: 0 };
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.ed_mut().buffer_id,
            position: target,
            anchor: target,
        })
        .await?;
    state.ed_mut().cursor = new;
    insert_text(client, state, "\n").await?;
    move_motion(
        client,
        state,
        Motion::LogicalLine {
            direction: Direction::Backward,
            count: 1,
            preserve_col: false,
        },
        false,
    )
    .await?;
    enter_insert_mode(state);
    Ok(())
}

async fn move_lines(
    client: &mut Client,
    state: &mut AppState,
    direction: VerticalDirection,
    count: u32,
) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: EditResult = client
            .rpc::<InputMoveLines>(InputMoveLinesParams {
                buffer_id: state.ed_mut().buffer_id,
                direction,
            })
            .await?;
        state.ed_mut().revision = r.revision;
        state.ed_mut().cursor = r.cursor;
    }
    Ok(())
}

async fn copy_to_clipboard(
    client: &mut Client,
    state: &mut AppState,
    scope: CopyScope,
) -> Result<()> {
    let r: BufferCopyResult = client
        .rpc::<BufferCopy>(BufferCopyParams {
            buffer_id: state.ed_mut().buffer_id,
            scope,
        })
        .await?;
    let len = r.text.len();
    match clipboard::copy(&mut state.clipboard, r.text) {
        Ok(()) => state.status = StatusMessage::success(format!("copied {len} bytes")),
        Err(e) => state.status = StatusMessage::error(format!("copy failed: {e}")),
    }
    Ok(())
}

async fn cut_to_clipboard(
    client: &mut Client,
    state: &mut AppState,
    scope: CopyScope,
) -> Result<()> {
    let r: BufferCutResult = client
        .rpc::<BufferCut>(BufferCopyParams {
            buffer_id: state.ed_mut().buffer_id,
            scope,
        })
        .await?;
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    let len = r.text.len();
    match clipboard::copy(&mut state.clipboard, r.text) {
        Ok(()) => state.status = StatusMessage::success(format!("cut {len} bytes")),
        Err(e) => state.status = StatusMessage::error(format!("cut to clipboard failed: {e}")),
    }
    Ok(())
}

/// Normal-mode paste: insert clipboard content *before* the selection's start and select the
/// pasted text. `count` repeats the clipboard contents, so `3p` pastes three copies in a row.
async fn paste_before(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    let text = match clipboard::paste(&mut state.clipboard) {
        Ok(t) => t,
        Err(e) => {
            state.status = StatusMessage::error(format!("paste failed: {e}"));
            return Ok(());
        }
    };
    let text = text.repeat(count.max(1) as usize);
    // Collapse to the start of the selection (no-op for a point cursor).
    let start = min_pos(state.ed_mut().cursor.position, state.ed_mut().cursor.anchor);
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.ed_mut().buffer_id,
            position: start,
            anchor: start,
        })
        .await?;
    state.ed_mut().cursor = new;
    insert_text_inner(client, state, &text, true).await
}

/// Normal-mode paste-replace: replace the current selection (or the cursor char) with the
/// clipboard content and select what was pasted. `count` repeats the clipboard contents.
async fn paste_replace(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    let text = match clipboard::paste(&mut state.clipboard) {
        Ok(t) => t,
        Err(e) => {
            state.status = StatusMessage::error(format!("paste failed: {e}"));
            return Ok(());
        }
    };
    let text = text.repeat(count.max(1) as usize);
    insert_text_inner(client, state, &text, true).await
}

/// Insert-mode paste: just insert at the cursor, no selection of the inserted text.
async fn paste_at_cursor(client: &mut Client, state: &mut AppState) -> Result<()> {
    let text = match clipboard::paste(&mut state.clipboard) {
        Ok(t) => t,
        Err(e) => {
            state.status = StatusMessage::error(format!("paste failed: {e}"));
            return Ok(());
        }
    };
    insert_text_inner(client, state, &text, false).await
}

async fn undo(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: UndoResult = client
            .rpc::<InputUndo>(BufferOnlyParams {
                buffer_id: state.ed_mut().buffer_id,
            })
            .await?;
        let applied = r.applied;
        apply_undo_result(state, r, "undo");
        if !applied {
            break;
        }
    }
    Ok(())
}

async fn redo(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: UndoResult = client
            .rpc::<InputRedo>(BufferOnlyParams {
                buffer_id: state.ed_mut().buffer_id,
            })
            .await?;
        let applied = r.applied;
        apply_undo_result(state, r, "redo");
        if !applied {
            break;
        }
    }
    Ok(())
}

fn apply_undo_result(state: &mut AppState, r: UndoResult, label: &str) {
    if !r.applied {
        state.status = StatusMessage::info(format!("nothing to {label}"));
        return;
    }
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    state.status = StatusMessage::success(format!("{label} (rev {})", r.revision));
}

async fn save_buffer(client: &mut Client, state: &mut AppState) -> Result<()> {
    save_buffer_with(client, state, false).await
}

/// In-place save (`Ctrl-s`) with explicit overwrite flag. `overwrite: false` is the default
/// path; `overwrite: true` is the retry after a user-confirmed `EXTERNALLY_MODIFIED` /
/// `EXTERNALLY_DELETED` conflict. (Save-as has its own flow — see `send_save_prompt`.)
async fn save_buffer_with(
    client: &mut Client,
    state: &mut AppState,
    overwrite: bool,
) -> Result<()> {
    if state.ed_mut().file_path.is_none() {
        // Scratch buffer — no path to save to. Don't auto-prompt: the user has to be explicit
        // about using save-as. Keeps `Ctrl-s` semantics uniform: it only ever writes to an
        // already-known path. We don't echo the specific save-as chord because it keeps
        // drifting; the user already knows their own keymap.
        state.status = StatusMessage::warning("scratch buffer has no path — use save-as");
        return Ok(());
    }
    let result = client
        .rpc::<BufferSave>(BufferSaveParams {
            buffer_id: state.ed_mut().buffer_id,
            path_index: None,
            relative_path: None,
            overwrite,
        })
        .await;
    match result {
        Ok(r) => {
            state.ed_mut().revision = r.revision;
            state.ed_mut().saved_revision = r.revision;
            state.ed_mut().externally_modified = false;
            state.ed_mut().externally_deleted = false;
            state.status = StatusMessage::success(format!("saved (rev {})", r.revision));
        }
        Err(e) if is_externally_modified(&e) => {
            state.confirm_prompt = Some(ConfirmPrompt {
                message: "file changed on disk — overwrite".into(),
                action: ConfirmAction::OverwriteExternalChange,
            });
        }
        Err(e) if is_externally_deleted(&e) => {
            state.confirm_prompt = Some(ConfirmPrompt {
                message: "file removed on disk — recreate".into(),
                action: ConfirmAction::OverwriteExternalChange,
            });
        }
        Err(e) => {
            state.status = StatusMessage::error(format!("save failed: {e}"));
        }
    }
    Ok(())
}

async fn reload_buffer(client: &mut Client, state: &mut AppState) -> Result<()> {
    reload_buffer_with(client, state, false).await
}

/// Send `buffer/reload` with explicit `force`. `force: false` is the default `Space r` path;
/// `force: true` is the retry after a user-confirmed `WOULD_DISCARD_CHANGES`.
async fn reload_buffer_with(client: &mut Client, state: &mut AppState, force: bool) -> Result<()> {
    if state.ed_mut().file_path.is_none() {
        state.status = StatusMessage::warning("scratch buffer has no path to reload");
        return Ok(());
    }
    let result = client
        .rpc::<BufferReload>(BufferReloadParams {
            buffer_id: state.ed_mut().buffer_id,
            force,
        })
        .await;
    match result {
        Ok(r) => {
            state.ed_mut().revision = r.revision;
            state.ed_mut().saved_revision = r.revision;
            state.ed_mut().externally_modified = false;
            state.ed_mut().externally_deleted = false;
            state.status = StatusMessage::success(format!("reloaded (rev {})", r.revision));
        }
        Err(e) if is_would_discard_changes(&e) => {
            state.confirm_prompt = Some(ConfirmPrompt {
                message: "discard local changes and reload".into(),
                action: ConfirmAction::ReloadDiscardChanges,
            });
        }
        Err(e) => {
            state.status = StatusMessage::error(format!("reload failed: {e}"));
        }
    }
    Ok(())
}

/// Open the status-bar save-as prompt. Pre-filled with the current file's parent directory as
/// the immutable prefix and the leaf filename as the editable input, so a small rename is one
/// or two keystrokes. Scratch buffers land at the first project root with an empty input. Kicks
/// off a `directory/list` to populate the cycle cache; until that response lands, Alt-j/k cycle
/// over an empty listing (i.e. no-ops).
async fn begin_save_prompt(client: &mut Client, state: &mut AppState) -> Result<()> {
    let (prompt, hint) = match state.ed().file_path.clone() {
        Some(abs) => SavePromptState::open_for_existing(&abs, &state.project_paths),
        None => SavePromptState::open_for_scratch(&state.project_paths),
    };
    state.save_prompt = Some(prompt);
    apply_cursor_style(state);
    if matches!(hint, crate::save_prompt::TransitionHint::RefreshListing) {
        refresh_save_prompt_listing(client, state).await?;
    }
    Ok(())
}

/// Fire `directory/list` for the save-prompt's current prefix and stash the response. No-op in
/// SelectingRoot mode (no committed dir to list). Errors are swallowed onto the status line —
/// the prompt stays usable, just without cycle-suggestions until the next prefix change.
async fn refresh_save_prompt_listing(client: &mut Client, state: &mut AppState) -> Result<()> {
    use aether_protocol::directory::{DirectoryList, DirectoryListParams};
    let Some(prompt) = state.save_prompt.as_ref() else {
        return Ok(());
    };
    let Some(path) = prompt.listing_path(&state.project_paths) else {
        return Ok(());
    };
    let result = client
        .rpc::<DirectoryList>(DirectoryListParams { path })
        .await;
    match result {
        Ok(r) => {
            if let Some(prompt) = state.save_prompt.as_mut() {
                prompt.set_listing(r.entries);
            }
        }
        Err(e) => {
            tracing::warn!("directory/list for save prompt failed: {e:#}");
        }
    }
    Ok(())
}

/// Create a new project (server writes its TOML, activates it for this client), tear down any
/// existing editor, and open the project-settings overlay so the user can add roots straight
/// away. Used by the "+ create" row in the Projects picker.
async fn create_project_and_open_settings(
    client: &mut Client,
    state: &mut AppState,
    name: &str,
) -> Result<()> {
    use aether_protocol::project::{ProjectCreate, ProjectCreateParams};
    let activated = client
        .rpc::<ProjectCreate>(ProjectCreateParams {
            name: name.to_string(),
        })
        .await?;
    state.editor = None;
    state.project_name = activated.project.name.clone();
    state.project_paths = activated.project.paths.clone();
    refresh_root_labels(state);
    state.status = StatusMessage::success(format!("created project {}", state.project_name));
    open_project_settings(state);
    Ok(())
}

/// Handle the Explorer's "+ create" synthetic row: open a fresh buffer at
/// `<explorer_dir>/<name>` with `create_if_missing: true`. Closes the picker, attaches the new
/// buffer in the editor. The server allocates the buffer without writing to disk; the file
/// hits the filesystem on the user's next save.
async fn create_file_in_explorer_dir(
    client: &mut Client,
    state: &mut AppState,
    name: &str,
) -> Result<()> {
    let Some(dir_abs) = state.picker.explorer_dir.clone() else {
        // Roots mode (or no explorer state) — nothing meaningful to create against.
        return Ok(());
    };
    let Some((path_index, rel_dir)) = strip_longest_root(&dir_abs, &state.project_paths) else {
        state.status = StatusMessage::error(format!(
            "can't create file in {dir_abs}: outside the project"
        ));
        return Ok(());
    };
    let relative_path = if rel_dir.is_empty() {
        name.to_string()
    } else {
        format!("{rel_dir}/{name}")
    };
    // Hide the picker before the RPC so the new buffer view replaces it cleanly.
    let kind = PickerKind::Explorer;
    let _ = client.rpc::<PickerHide>(PickerHideParams { kind }).await;
    state.picker.open = false;
    let open: BufferOpenResult = client
        .rpc::<BufferOpen>(BufferOpenParams {
            buffer_id: None,
            path_index: Some(path_index as u32),
            relative_path: Some(relative_path),
            language: None,
            create_if_missing: true,
            jump_to: None,
        })
        .await?;
    subscribe_replacing_scratch(client, state, open).await
}

/// Handle the Explorer's "+ create directory" synthetic row: create the directory via
/// `directory/create`, then navigate the picker into the new (empty) dir so the user can
/// immediately type a filename and create their first file inside it. The synthetic-row
/// recompute already enforces single-segment names and refuses to overwrite an existing
/// entry, so we don't need to defend here.
async fn create_directory_in_explorer_dir(
    client: &mut Client,
    state: &mut AppState,
    name: &str,
) -> Result<()> {
    use aether_protocol::directory::{DirectoryCreate, DirectoryCreateParams};
    let Some(dir_abs) = state.picker.explorer_dir.clone() else {
        return Ok(());
    };
    let target = std::path::Path::new(&dir_abs)
        .join(name)
        .display()
        .to_string();
    let result = match client
        .rpc::<DirectoryCreate>(DirectoryCreateParams { path: target })
        .await
    {
        Ok(r) => r,
        Err(e) => {
            state.status = StatusMessage::error(format!("create directory failed: {e}"));
            return Ok(());
        }
    };
    state.status = StatusMessage::success(format!("created directory {}", result.path));
    picker_navigate_to_dir(client, state, result.path, None).await
}

// ---- project settings -------------------------------------------------------------------------

/// Hydrate the project-settings overlay from the currently-active project's name + roots and open
/// it. Cheap (just clones); no RPC. Focus lands on the always-present input row at the bottom —
/// most overlay opens (especially the post-create flow) are to add a root, and this avoids an
/// extra keypress for that case. The name field sits above the roots and is reached with Alt-k.
fn open_project_settings(state: &mut AppState) {
    let roots = state.project_paths.clone();
    // Input row index = roots.len() + 1 (name field is 0, roots are 1..=len).
    let selected = roots.len() + 1;
    state.project_settings = Some(ProjectSettingsState {
        project_name: state.project_name.clone(),
        name_input: crate::text_input::TextInput::new(state.project_name.clone()),
        roots,
        selected,
        add_input: crate::text_input::TextInput::default(),
        error: None,
        pending_delete: false,
    });
    apply_cursor_style(state);
}

/// Key handling for the keyboard-shortcut help overlay (`Space ?`). Read-only: Esc / `?` / `q`
/// close it; the arrows and PageUp/Down scroll. The render clamps `scroll` to the content height,
/// so over-scrolling here is harmless.
fn handle_help_key(state: &mut AppState, k: KeyEvent) -> Result<()> {
    // Scroll math (and clamping to the real bottom) lives in `ScrollState`, which the renderer
    // feeds the box geometry. Here we just translate keys to intents. `h`/`l` (and the arrows /
    // Tab) move *between* tabs; `j`/`k` (and ↑/↓) scroll *within* one. Switching tabs resets the
    // scroll to the top, since each tab is its own (usually short) page.
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => state.help.open = false,
        KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
            state.help.tab = state.help.tab.prev();
            state.help.scroll.scroll_to_top();
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
            state.help.tab = state.help.tab.next();
            state.help.scroll.scroll_to_top();
        }
        KeyCode::Up | KeyCode::Char('k') => state.help.scroll.scroll_by(-1),
        KeyCode::Down | KeyCode::Char('j') => state.help.scroll.scroll_by(1),
        KeyCode::PageUp => state.help.scroll.page(false),
        KeyCode::PageDown | KeyCode::Char(' ') => state.help.scroll.page(true),
        KeyCode::Home | KeyCode::Char('g') => state.help.scroll.scroll_to_top(),
        KeyCode::End | KeyCode::Char('G') => state.help.scroll.scroll_to_bottom(),
        _ => {}
    }
    Ok(())
}

/// Mouse handling for the help overlay — the wheel scrolls, everything else is ignored.
fn handle_help_mouse(state: &mut AppState, m: MouseEvent) {
    match m.kind {
        MouseEventKind::ScrollUp => state.help.scroll.scroll_by(-3),
        MouseEventKind::ScrollDown => state.help.scroll.scroll_by(3),
        _ => {}
    }
}

/// Selection model: `selected == 0` is the name field, `1..=roots.len()` are root rows (root `i`
/// at index `i + 1`), and `roots.len() + 1` is the add-root input row. Alt-j/k move between
/// fields (mirroring the picker's chord, so Alt-j/k means "navigate" everywhere in the app);
/// Left/Right stay free to move the caret inside a text field. Leaving the name field downward
/// (Alt-j or Enter) or closing the overlay (Esc) commits a pending rename via `project/rename`;
/// if the server rejects it the overlay stays open with the error. Delete or Ctrl-d on a root row
/// stages a remove (which `y`/Enter/Delete/Ctrl-d then confirm); Enter on the input row commits
/// the add.
async fn handle_project_settings_key(
    client: &mut Client,
    state: &mut AppState,
    k: KeyEvent,
) -> Result<()> {
    let code = k.code;
    let mods = k.modifiers;
    // Ctrl-d is accepted alongside the Delete key for both staging and confirming a removal —
    // easier to reach on keyboards where Delete is awkward (or absent on small layouts).
    let is_delete_chord =
        code == KeyCode::Delete || (code == KeyCode::Char('d') && mods == KeyModifiers::CONTROL);

    // Pending-delete confirmation takes precedence over every other key. While set, only
    // y/Y/Enter/Delete/Ctrl-d commit and n/N/Esc cancel; everything else is swallowed so the
    // user can't silently drop the pending state. Esc cancels the pending *first*; a subsequent
    // Esc then closes the overlay.
    if state
        .project_settings
        .as_ref()
        .is_some_and(|s| s.pending_delete)
    {
        if is_delete_chord
            || code == KeyCode::Enter
            || matches!(code, KeyCode::Char('y') | KeyCode::Char('Y'))
        {
            let Some(s) = state.project_settings.as_mut() else {
                return Ok(());
            };
            // Root `i` lives at selection index `i + 1`; map back before indexing.
            let Some(path) = s
                .selected
                .checked_sub(1)
                .and_then(|i| s.roots.get(i))
                .cloned()
            else {
                s.pending_delete = false;
                return Ok(());
            };
            let project_name = s.project_name.clone();
            s.pending_delete = false;
            s.error = None;
            remove_root(client, state, &project_name, &path).await?;
        } else if matches!(code, KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc) {
            if let Some(s) = state.project_settings.as_mut() {
                s.pending_delete = false;
            }
        }
        return Ok(());
    }

    // Read the focus position via a short borrow — the rename commit below needs `&mut state`,
    // so we can't hold a borrow of `project_settings` across it.
    let Some((selected, roots_len)) = state
        .project_settings
        .as_ref()
        .map(|s| (s.selected, s.roots.len()))
    else {
        return Ok(());
    };
    let on_name = selected == 0;
    let on_input = selected == roots_len + 1;

    if code == KeyCode::Esc {
        // Closing blurs the name field — commit any pending rename. A rejected rename keeps the
        // overlay open so its error stays visible; otherwise close.
        if on_name && !commit_rename_if_changed(client, state).await? {
            return Ok(());
        }
        state.project_settings = None;
        apply_cursor_style(state);
        return Ok(());
    }

    // Alt-j / Alt-k navigation. Moving *down* off the name field blurs it → commit the rename.
    if mods == KeyModifiers::ALT {
        match code {
            KeyCode::Char('k') => {
                if let Some(s) = state.project_settings.as_mut() {
                    s.selected = s.selected.saturating_sub(1);
                }
                return Ok(());
            }
            KeyCode::Char('j') => {
                if on_name && !commit_rename_if_changed(client, state).await? {
                    return Ok(()); // rename rejected — stay on the name field to fix it
                }
                if let Some(s) = state.project_settings.as_mut() {
                    s.selected = (s.selected + 1).min(s.roots.len() + 1);
                }
                return Ok(());
            }
            _ => {}
        }
    }

    if is_delete_chord && !on_name && !on_input {
        // Stage the confirm — actual removal happens in the pending-delete branch above.
        if let Some(s) = state.project_settings.as_mut() {
            s.pending_delete = true;
            s.error = None;
        }
        return Ok(());
    }

    if code == KeyCode::Enter {
        if on_name {
            // Enter commits the rename and, on success, advances to the next field (index 1 is
            // the first root, or the input row when there are none).
            if commit_rename_if_changed(client, state).await? {
                if let Some(s) = state.project_settings.as_mut() {
                    s.selected = 1;
                }
            }
        } else if on_input {
            commit_add_root(client, state).await?;
        }
        return Ok(());
    }

    // Text editing for whichever text field is focused (root rows have none). apply_prompt_key
    // returns Cancel/Commit only for Esc/Enter — both intercepted above — so we only see Edited.
    if let Some(s) = state.project_settings.as_mut() {
        let input = if on_name {
            Some(&mut s.name_input)
        } else if on_input {
            Some(&mut s.add_input)
        } else {
            None
        };
        if let Some(input) = input {
            if let PromptKeyOutcome::Edited = crate::text_input::apply_prompt_key(input, k) {
                s.error = None;
            }
        }
    }
    Ok(())
}

/// Commit a pending project rename if the name field differs from the committed name. Returns
/// `Ok(true)` when it's safe to leave the field: the rename succeeded, or there was nothing to do
/// (the edit was empty or unchanged, in which case the field is normalized back to the current
/// name). Returns `Ok(false)` only when the server *rejected* the rename (e.g. a name collision)
/// — the error is stored on the overlay and the typed text is left in place so the user can fix
/// it without losing what they wrote.
async fn commit_rename_if_changed(client: &mut Client, state: &mut AppState) -> Result<bool> {
    use aether_protocol::project::{ProjectRename, ProjectRenameParams};
    let Some((old_name, new_name)) = state
        .project_settings
        .as_ref()
        .map(|s| (s.project_name.clone(), s.name_input.text.trim().to_string()))
    else {
        return Ok(true);
    };
    if new_name.is_empty() || new_name == old_name {
        // Nothing to commit — normalize the field back to the committed name so a stray-whitespace
        // or abandoned-empty edit doesn't linger.
        if let Some(s) = state.project_settings.as_mut() {
            s.name_input.set(old_name);
        }
        return Ok(true);
    }
    let result = client
        .rpc::<ProjectRename>(ProjectRenameParams {
            project: old_name.clone(),
            new_name: new_name.clone(),
        })
        .await;
    match result {
        Ok(info) => {
            if state.project_name == old_name {
                state.project_name = info.name.clone();
            }
            if let Some(s) = state.project_settings.as_mut() {
                s.project_name = info.name.clone();
                s.name_input.set(info.name);
                s.error = None;
            }
            state.status = StatusMessage::success(format!("renamed project to {new_name}"));
            apply_cursor_style(state);
            Ok(true)
        }
        Err(e) => {
            let msg = if let Some(rpc_err) = e.downcast_ref::<crate::client::RpcError>() {
                rpc_err.message.clone()
            } else {
                e.to_string()
            };
            if let Some(s) = state.project_settings.as_mut() {
                s.error = Some(msg);
            }
            Ok(false)
        }
    }
}

async fn commit_add_root(client: &mut Client, state: &mut AppState) -> Result<()> {
    use aether_protocol::project::{ProjectAddRoot, ProjectAddRootParams};
    let Some(settings) = state.project_settings.as_mut() else {
        return Ok(());
    };
    let path = settings.add_input.text.trim().to_string();
    if path.is_empty() {
        return Ok(());
    }
    let project_name = settings.project_name.clone();
    settings.error = None;
    let result = client
        .rpc::<ProjectAddRoot>(ProjectAddRootParams {
            project: project_name.clone(),
            path,
        })
        .await;
    match result {
        Ok(info) => {
            sync_project_paths(state, info);
            if let Some(s) = state.project_settings.as_mut() {
                s.add_input.clear();
                s.selected = s.roots.len() + 1;
            }
            state.status = StatusMessage::success(format!("added root to {project_name}"));
        }
        Err(e) => {
            if let Some(s) = state.project_settings.as_mut() {
                s.error = Some(
                    if let Some(rpc_err) = e.downcast_ref::<crate::client::RpcError>() {
                        rpc_err.message.clone()
                    } else {
                        e.to_string()
                    },
                );
            }
        }
    }
    apply_cursor_style(state);
    Ok(())
}

async fn remove_root(
    client: &mut Client,
    state: &mut AppState,
    project_name: &str,
    path: &str,
) -> Result<()> {
    use aether_protocol::project::{ProjectRemoveRoot, ProjectRemoveRootParams};
    let result = client
        .rpc::<ProjectRemoveRoot>(ProjectRemoveRootParams {
            project: project_name.to_string(),
            path: path.to_string(),
        })
        .await;
    match result {
        Ok(r) => {
            let closed = r.closed_buffer_ids.clone();
            sync_project_paths(state, r.project);
            // If the active editor's buffer just got closed, attach to next_buffer_id or spawn
            // a scratch so the user lands on something usable.
            let current_buffer = state.editor.as_ref().map(|e| e.buffer_id);
            if let Some(cur) = current_buffer {
                if closed.contains(&cur) {
                    match r.next_buffer_id {
                        Some(next) => attach_buffer(client, state, next).await?,
                        None => new_scratch(client, state).await?,
                    }
                }
            }
            state.status = StatusMessage::success(if closed.is_empty() {
                format!("removed root from {project_name}")
            } else {
                format!(
                    "removed root from {project_name}; closed {} buffer(s)",
                    closed.len()
                )
            });
        }
        Err(e) => {
            // Surface the failure inside the overlay when it's open (the common case — `remove_root`
            // is only invoked from there today). Fall back to the status line otherwise.
            let msg = if let Some(rpc_err) = e.downcast_ref::<crate::client::RpcError>() {
                if rpc_err.code
                    == aether_protocol::error::ErrorCode::DIRTY_BUFFERS_PREVENT_REMOVE.code()
                {
                    rpc_err.message.clone()
                } else {
                    format!("remove root failed: {}", rpc_err.message)
                }
            } else {
                format!("remove root failed: {e}")
            };
            if let Some(s) = state.project_settings.as_mut() {
                s.error = Some(msg);
            } else {
                state.status = StatusMessage::error(msg);
            }
        }
    }
    apply_cursor_style(state);
    Ok(())
}

/// Recompute `root_labels` from the current `project_paths`, then refresh any cached display
/// strings derived from them. Called from every site that mutates `project_paths`, so the
/// cached labels never drift out of sync after add/remove root.
fn refresh_root_labels(state: &mut AppState) {
    state.root_labels = crate::labels::root_labels(&state.project_paths);
    // Re-derive the active editor's `file_label` since it embeds the root label.
    if let Some(ed) = state.editor.as_mut() {
        if let Some(path) = ed.file_path.clone() {
            ed.file_label = project_relative_label(&path, &state.project_paths, &state.root_labels);
        }
    }
}

/// Reflect a server-returned `ProjectInfo` into `AppState`. Updates `project_paths`, and — if
/// the settings overlay is open for this project — refreshes its visible root list. After a
/// successful remove the selection can land past the new end of `roots`; snap it to the input
/// row (`roots.len()`) so focus lands somewhere usable.
fn sync_project_paths(state: &mut AppState, info: aether_protocol::project::ProjectInfo) {
    if state.project_name == info.name {
        state.project_paths = info.paths.clone();
        refresh_root_labels(state);
    }
    if let Some(settings) = state.project_settings.as_mut() {
        if settings.project_name == info.name {
            settings.roots = info.paths;
            // Input row is the last index (`roots.len() + 1`); snap focus there if a remove left
            // `selected` pointing past the new end.
            if settings.selected > settings.roots.len() + 1 {
                settings.selected = settings.roots.len() + 1;
            }
        }
    }
}

async fn handle_save_prompt_key(
    client: &mut Client,
    state: &mut AppState,
    k: KeyEvent,
) -> Result<()> {
    use crate::save_prompt::{EnterAction, TransitionHint};
    let code = k.code;
    let mods = k.modifiers;
    // Esc cancels; Enter routes through `enter_action` so it never silently closes the prompt —
    // see EnterAction for the per-state breakdown (Tab semantics in SelectingRoot and on
    // trailing-slash input; no-op on empty input; save otherwise).
    match code {
        KeyCode::Esc => {
            abort_save_prompt(state);
            return Ok(());
        }
        KeyCode::Enter => {
            let action = state
                .save_prompt
                .as_ref()
                .map(|p| p.enter_action())
                .unwrap_or(EnterAction::Nothing);
            match action {
                EnterAction::Save => return send_save_prompt(client, state, false).await,
                EnterAction::Tab => {
                    let project_paths = state.project_paths.clone();
                    let hint = state
                        .save_prompt
                        .as_mut()
                        .map(|p| p.tab(&project_paths))
                        .unwrap_or(TransitionHint::None);
                    if matches!(hint, TransitionHint::RefreshListing) {
                        refresh_save_prompt_listing(client, state).await?;
                    }
                    return Ok(());
                }
                EnterAction::Nothing => return Ok(()),
            }
        }
        _ => {}
    }
    let Some(prompt) = state.save_prompt.as_mut() else {
        return Ok(());
    };
    let project_paths = state.project_paths.clone();
    let hint = match (code, mods) {
        (KeyCode::Char('j'), m) if m == KeyModifiers::ALT => {
            prompt.alt_j(&project_paths);
            TransitionHint::None
        }
        (KeyCode::Char('k'), m) if m == KeyModifiers::ALT => {
            prompt.alt_k(&project_paths);
            TransitionHint::None
        }
        (KeyCode::Char('l'), m) if m == KeyModifiers::ALT => prompt.tab(&project_paths),
        (KeyCode::Backspace, m) if m == KeyModifiers::ALT => prompt.alt_backspace(&project_paths),
        (KeyCode::Backspace, _) => prompt.backspace(&project_paths),
        (KeyCode::Left, _) => {
            prompt.input.move_left();
            TransitionHint::None
        }
        (KeyCode::Right, _) => {
            prompt.input.move_right();
            TransitionHint::None
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            prompt.type_char(c, &project_paths)
        }
        _ => TransitionHint::None,
    };
    if matches!(hint, TransitionHint::RefreshListing) {
        refresh_save_prompt_listing(client, state).await?;
    }
    Ok(())
}

fn abort_save_prompt(state: &mut AppState) {
    state.save_prompt = None;
    apply_cursor_style(state);
}

/// Send the save-as RPC. With `overwrite = false` we may get a `WOULD_OVERWRITE` back, in
/// which case we keep the prompt open and switch it to the y/N confirmation; with `true` we
/// resend after confirmation. On success we close the prompt and refresh local state; on any
/// other error we just stash the message in the status bar and close.
async fn send_save_prompt(
    client: &mut Client,
    state: &mut AppState,
    overwrite: bool,
) -> Result<()> {
    let (path_index, path) = match state
        .save_prompt
        .as_ref()
        .and_then(crate::save_prompt::SavePromptState::save_target)
    {
        Some((idx, p)) => (idx, p),
        None => {
            // Empty input or root-selection mode without a committed name — treat as cancel.
            state.save_prompt = None;
            apply_cursor_style(state);
            return Ok(());
        }
    };

    let buffer_id = state.ed_mut().buffer_id;
    let result = client
        .rpc::<BufferSave>(BufferSaveParams {
            buffer_id,
            path_index: Some(path_index),
            relative_path: Some(path.clone()),
            overwrite,
        })
        .await;
    match result {
        Ok(r) => {
            state.save_prompt = None;
            apply_cursor_style(state);
            let project_paths = state.project_paths.clone();
            let root_labels = state.root_labels.clone();
            let new_abs = project_paths
                .get(path_index as usize)
                .map(|root| std::path::Path::new(root).join(&path).display().to_string());
            let ed = state.ed_mut();
            ed.revision = r.revision;
            ed.saved_revision = r.revision;
            if let Some(abs) = new_abs {
                ed.file_label = project_relative_label(&abs, &project_paths, &root_labels);
                ed.file_path = Some(abs);
            } else {
                // No project root configured — fall back to the typed path verbatim.
                ed.file_label = path.clone();
            }
            state.status =
                StatusMessage::success(format!("saved as {} (rev {})", path, r.revision));
        }
        Err(e) if is_would_overwrite(&e) => {
            // Keep the save-prompt open and overlay a confirm prompt on top. If the user
            // declines we drop the confirm and they're back editing the path; if they accept
            // we re-send with `overwrite: true`.
            state.confirm_prompt = Some(ConfirmPrompt {
                message: format!("overwrite {path}"),
                action: ConfirmAction::OverwriteSaveAs,
            });
        }
        Err(e) => {
            state.save_prompt = None;
            apply_cursor_style(state);
            state.status = StatusMessage::error(format!("save failed: {e}"));
        }
    }
    Ok(())
}

/// True iff `e` is the server's `WOULD_OVERWRITE` JSON-RPC error. `Client::rpc` wraps server
/// errors as `RpcError` inside an anyhow chain; we downcast rather than match on the message.
fn is_would_overwrite(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::client::RpcError>()
        .is_some_and(|r| r.code == ErrorCode::WOULD_OVERWRITE.code())
}

fn is_externally_modified(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::client::RpcError>()
        .is_some_and(|r| r.code == ErrorCode::EXTERNALLY_MODIFIED.code())
}

fn is_externally_deleted(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::client::RpcError>()
        .is_some_and(|r| r.code == ErrorCode::EXTERNALLY_DELETED.code())
}

fn is_would_discard_changes(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::client::RpcError>()
        .is_some_and(|r| r.code == ErrorCode::WOULD_DISCARD_CHANGES.code())
}

async fn handle_confirm_prompt_key(
    client: &mut Client,
    state: &mut AppState,
    k: KeyEvent,
) -> Result<()> {
    match (k.code, k.modifiers) {
        // Default (Enter / Esc / n) declines — matches the uppercase `N` in `[y/N]`. Drops the
        // confirm prompt; whatever it was layered on top of stays put.
        (KeyCode::Esc, _) | (KeyCode::Enter, _) | (KeyCode::Char('n' | 'N'), _) => {
            state.confirm_prompt = None;
            apply_cursor_style(state);
        }
        (KeyCode::Char('y' | 'Y'), _) => {
            let Some(prompt) = state.confirm_prompt.take() else {
                return Ok(());
            };
            match prompt.action {
                ConfirmAction::OverwriteSaveAs => {
                    send_save_prompt(client, state, true).await?;
                }
                ConfirmAction::CloseBuffer { buffer_id } => {
                    finalize_close_buffer(client, state, buffer_id).await?;
                }
                ConfirmAction::OverwriteExternalChange => {
                    save_buffer_with(client, state, true).await?;
                }
                ConfirmAction::ReloadDiscardChanges => {
                    reload_buffer_with(client, state, true).await?;
                }
            }
            apply_cursor_style(state);
        }
        _ => {}
    }
    Ok(())
}

async fn ensure_cursor_in_window(client: &mut Client, state: &mut AppState) -> Result<()> {
    // Commit any pending scroll first so the visibility check below sees the user's intended
    // scroll position; otherwise we'd snap against stale state and possibly miss the snap-back.
    flush_pending_scroll(client, state).await?;

    // Horizontal dimension first — only matters when wrap is off. Adjust `scroll_col` so the
    // cursor's column is within `[scroll_col, scroll_col + viewport_cols)`. Pure client-side.
    if matches!(state.ed_mut().wrap, WrapMode::None) && state.viewport_cols > 0 {
        let col = state.ed_mut().cursor.position.col;
        if col < state.ed_mut().scroll_col {
            state.ed_mut().scroll_col = col;
        } else if col
            >= state
                .ed_mut()
                .scroll_col
                .saturating_add(state.viewport_cols)
        {
            state.ed_mut().scroll_col = col.saturating_sub(state.viewport_cols.saturating_sub(1));
        }
    }

    let cursor_line = state.ed_mut().cursor.position.line;
    let top = state.ed_mut().scroll_logical_line;

    // Above the top: scroll up so the cursor's line is the new top.
    if cursor_line < top {
        scroll_to(client, state, cursor_line).await?;
        return Ok(());
    }

    // Below the bottom (counting *visual* rows, not logical lines): scroll the cursor's line to
    // the top. Clamp the target to `max_scroll_logical_line` so a jump to (or near) the last
    // line doesn't overscroll — `Alt-g` would otherwise put the last line at the very top of
    // an otherwise-empty viewport.
    let cursor_visible = ui::cursor_visual_position(state, state.viewport_rows).is_some();
    if !cursor_visible {
        let target = cursor_line.min(state.ed_mut().max_scroll_logical_line);
        scroll_to(client, state, target).await?;
    }
    Ok(())
}

/// Scroll the viewport so the cursor's logical line sits at the vertical center. Clamped to
/// `max_scroll_logical_line` so jumps near EOF don't overscroll. Approximate under soft wrap —
/// the line's first visual row lands near center, which is close enough for a quick `zz`.
async fn center_cursor(client: &mut Client, state: &mut AppState) -> Result<()> {
    let half = state.viewport_rows / 2;
    let target = state.ed_mut().cursor.position.line.saturating_sub(half);
    let target = target.min(state.ed_mut().max_scroll_logical_line);
    if target != state.ed_mut().scroll_logical_line {
        scroll_to(client, state, target).await?;
    }
    Ok(())
}

async fn toggle_wrap(client: &mut Client, state: &mut AppState) -> Result<()> {
    let new_wrap = match state.ed_mut().wrap {
        WrapMode::Soft => WrapMode::None,
        WrapMode::None => WrapMode::Soft,
    };
    let r = client
        .rpc::<ViewportSetWrap>(ViewportSetWrapParams {
            viewport_id: state.ed_mut().viewport_id,
            wrap: new_wrap,
        })
        .await?;
    state.ed_mut().wrap = new_wrap;
    state.ed_mut().window_first_logical_line = r.window.first_logical_line;
    state.ed_mut().lines = r.window.lines;
    state.ed_mut().scroll_skip_rows = 0; // wrap changed visual heights; re-align the top line
                                         // Horizontal scroll is meaningless under soft wrap — content never overflows right.
    if matches!(new_wrap, WrapMode::Soft) {
        state.ed_mut().scroll_col = 0;
    }
    state.status = StatusMessage::info(format!(
        "wrap: {}",
        match new_wrap {
            WrapMode::Soft => "on",
            WrapMode::None => "off",
        }
    ));
    Ok(())
}

/// Toggle the server-side inline diff view for the active viewport and adopt the re-rendered
/// window. Like `toggle_wrap`, the whole window is resent because the phantom rows change the
/// visual layout and `max_scroll`.
async fn toggle_diff_view(client: &mut Client, state: &mut AppState) -> Result<()> {
    let enabled = !state.ed_mut().diff_view;
    let r = client
        .rpc::<GitSetDiffView>(GitSetDiffViewParams {
            viewport_id: state.ed_mut().viewport_id,
            enabled,
        })
        .await?;
    let ed = state.ed_mut();
    ed.diff_view = enabled;
    ed.window_first_logical_line = r.window.first_logical_line;
    ed.line_count = r.window.line_count;
    ed.max_scroll_logical_line = r.window.max_scroll_logical_line;
    ed.lines = r.window.lines;
    ed.scroll_skip_rows = 0; // phantom rows appeared/vanished; re-align the top line
    state.status = StatusMessage::info(format!("diff: {}", if enabled { "on" } else { "off" }));
    Ok(())
}

/// Jump the cursor to the next/previous changed region. The server recomputes hunks and moves the
/// cursor authoritatively; the dispatch funnel scrolls it into view afterward (cursor changed).
/// A status note when there's nothing further in that direction.
async fn navigate_hunk(
    client: &mut Client,
    state: &mut AppState,
    direction: HunkDirection,
) -> Result<()> {
    let r = client
        .rpc::<GitNavigateHunk>(GitNavigateHunkParams {
            buffer_id: state.ed_mut().buffer_id,
            from_line: state.ed_mut().cursor.position.line,
            direction,
        })
        .await?;
    state.ed_mut().cursor = r.cursor;
    if !r.moved {
        state.status = StatusMessage::info("no more changes".to_string());
    }
    Ok(())
}

/// Format the whole buffer via the language server. The server applies the edits authoritatively
/// (one undo step) and pushes the re-rendered viewports; we just adopt the returned cursor and note
/// when nothing changed (no server, no edits, or already-formatted).
async fn lsp_format(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r = client
        .rpc::<LspFormat>(aether_protocol::lsp::LspBufferParams {
            buffer_id: state.ed_mut().buffer_id,
        })
        .await?;
    state.ed_mut().cursor = r.cursor;
    // Specific feedback per outcome — "nothing happened" has several distinct causes.
    let note = match r.status {
        FormatStatus::Applied => None,
        FormatStatus::NoChange => Some("already formatted".to_string()),
        FormatStatus::NotReady => Some("language server still starting".to_string()),
        FormatStatus::Unavailable => Some("language server unavailable".to_string()),
        FormatStatus::Unsupported => Some(match state.ed().language.as_deref() {
            Some(lang) => format!("no formatter for {lang}"),
            None => "no formatter for this file".to_string(),
        }),
    };
    if let Some(note) = note {
        state.status = StatusMessage::info(note);
    }
    Ok(())
}

/// Jump the cursor to the next/previous diagnostic. The server holds the diagnostics and moves the
/// cursor authoritatively; the dispatch funnel scrolls it into view afterward. A status note when
/// there's nothing further in that direction. (Sibling of [`navigate_hunk`].)
async fn navigate_diagnostic(
    client: &mut Client,
    state: &mut AppState,
    direction: DiagnosticDirection,
) -> Result<()> {
    let r = client
        .rpc::<LspNavigateDiagnostic>(LspNavigateDiagnosticParams {
            buffer_id: state.ed_mut().buffer_id,
            from_line: state.ed_mut().cursor.position.line,
            direction,
        })
        .await?;
    state.ed_mut().cursor = r.cursor;
    if !r.moved {
        state.status = StatusMessage::info("no more diagnostics".to_string());
    }
    Ok(())
}

/// Accumulate a vertical-scroll delta. Doesn't touch the cursor and doesn't issue an RPC — the
/// actual `viewport/scroll` is sent when `flush_pending_scroll` runs (before the next draw, or
/// at the start of `ensure_cursor_in_window`). This lets a trackpad burst of N scroll events
/// collapse into one server round-trip.
fn scroll_lines(state: &mut AppState, delta: i64) {
    state.ed_mut().pending_scroll_lines = state.ed_mut().pending_scroll_lines.saturating_add(delta);
}

/// The number of visual rows a logical line occupies in the current window: its phantom deleted
/// rows plus its (possibly wrapped) content rows. Out-of-window lines fall back to 1 — accurate
/// enough for the rare scroll that walks past the overscanned window before the next refetch.
fn line_visual_height(state: &AppState, logical_line: u32) -> u32 {
    let local = (logical_line as i64) - (state.ed().window_first_logical_line as i64);
    if local < 0 || local >= state.ed().lines.len() as i64 {
        return 1;
    }
    let r = &state.ed().lines[local as usize];
    (r.virtual_rows_above.len() + r.visual_rows.len().max(1)) as u32
}

/// Advance a `(logical_line, skip)` scroll position by `delta` *visual* rows (negative = up),
/// stepping across logical-line boundaries using each line's visual height. This is what makes
/// scrolling move one visual row at a time instead of jumping a whole (wrapped / phantom-row-
/// padded) logical line.
fn scroll_advance(state: &AppState, line: &mut u32, skip: &mut u32, delta: i64) {
    if delta >= 0 {
        let mut remaining = delta as u32;
        while remaining > 0 {
            let rows_below = line_visual_height(state, *line)
                .saturating_sub(1)
                .saturating_sub(*skip);
            if remaining <= rows_below {
                *skip += remaining;
                return;
            }
            remaining -= rows_below + 1; // +1 to cross into the next line's first row
            *line += 1;
            *skip = 0;
        }
    } else {
        let mut remaining = (-delta) as u32;
        while remaining > 0 {
            if remaining <= *skip {
                *skip -= remaining;
                return;
            }
            remaining -= *skip + 1; // +1 to cross into the previous line's last row
            if *line == 0 {
                *skip = 0;
                return;
            }
            *line -= 1;
            *skip = line_visual_height(state, *line).saturating_sub(1);
        }
    }
}

/// Apply any accumulated `pending_scroll_lines` (now in *visual rows*) to the scroll position.
/// A pure within-line move just updates `scroll_skip_rows` (no RPC — same window); crossing into a
/// new logical line issues one `viewport/scroll`. Called before every draw and from inside
/// `ensure_cursor_in_window`.
async fn flush_pending_scroll(client: &mut Client, state: &mut AppState) -> Result<()> {
    if !state.has_editor() {
        return Ok(());
    }
    let delta = state.ed().pending_scroll_lines;
    if delta == 0 {
        return Ok(());
    }
    state.ed_mut().pending_scroll_lines = 0;

    let mut line = state.ed().scroll_logical_line;
    let mut skip = state.ed().scroll_skip_rows;
    scroll_advance(state, &mut line, &mut skip, delta);

    // Clamp to the server-computed last scroll line (it accounts for wrap). At the very bottom we
    // align to the logical line — fine-positioning into the final screenful isn't worth the math.
    let max = state.ed().max_scroll_logical_line;
    if line >= max {
        line = max;
        skip = 0;
    }

    if line == state.ed().scroll_logical_line && skip == state.ed().scroll_skip_rows {
        return Ok(()); // no movement
    }
    if line != state.ed().scroll_logical_line {
        scroll_to(client, state, line).await?; // resets skip to 0 + refetches the window
    }
    state.ed_mut().scroll_skip_rows = skip;
    Ok(())
}

/// Scroll the viewport horizontally by `delta` columns. Only meaningful under `WrapMode::None`;
/// no-op when soft wrap is on (wrapped content never overflows right).
fn scroll_cols(state: &mut AppState, delta: i64) {
    if !matches!(state.ed_mut().wrap, WrapMode::None) {
        return;
    }
    state.ed_mut().scroll_col = if delta >= 0 {
        state.ed_mut().scroll_col.saturating_add(delta as u32)
    } else {
        state.ed_mut().scroll_col.saturating_sub((-delta) as u32)
    };
}

async fn scroll_to(client: &mut Client, state: &mut AppState, target_line: u32) -> Result<()> {
    let r = client
        .rpc::<ViewportScroll>(ViewportScrollParams {
            viewport_id: state.ed_mut().viewport_id,
            scroll: ScrollPosition {
                logical_line: target_line,
                sub_row: 0.0,
            },
        })
        .await?;
    state.ed_mut().scroll_logical_line = target_line;
    // A logical-line-aligned scroll: the target line sits flush at the top, no hidden rows.
    state.ed_mut().scroll_skip_rows = 0;
    state.ed_mut().window_first_logical_line = r.window.first_logical_line;
    state.ed_mut().line_count = r.window.line_count;
    state.ed_mut().max_scroll_logical_line = r.window.max_scroll_logical_line;
    state.ed_mut().lines = r.window.lines;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_status_changed_updates_status_map() {
        let mut state = empty_app_state(80, 24);
        let params = serde_json::to_value(LspServerStatus {
            name: "rust-analyzer".into(),
            language: "rust".into(),
            workspace_root: "/p".into(),
            status: aether_protocol::lsp::LspStatus::Ready,
        })
        .unwrap();
        apply_notification(
            &mut state,
            aether_protocol::envelope::Notification {
                jsonrpc: aether_protocol::envelope::JsonRpc,
                method: LspStatusChanged::NAME.into(),
                params,
            },
        );
        let s = state
            .lsp_status
            .get(&("rust".to_string(), "/p".to_string()))
            .expect("status recorded by (language, root)");
        assert_eq!(s.name, "rust-analyzer");
        assert!(matches!(s.status, aether_protocol::lsp::LspStatus::Ready));
    }

    /// `empty_scratch_id` flags only an empty, unmodified scratch (the discardable placeholder):
    /// not when there's no editor, not once it's been typed into (dirty), and not a file-backed
    /// buffer.
    #[test]
    fn empty_scratch_id_detects_only_unmodified_scratch() {
        let mut state = empty_app_state(80, 24);
        assert_eq!(
            empty_scratch_id(&state),
            None,
            "no editor → nothing to discard"
        );

        // Fresh scratch: no path, not dirty, empty.
        state.editor = Some(stub_editor_state("(scratch 1)"));
        assert_eq!(empty_scratch_id(&state), Some(state.ed().buffer_id));

        // Typed into → dirty → keep it.
        state.ed_mut().revision = 5; // saved_revision stays 0
        assert_eq!(empty_scratch_id(&state), None);

        // File-backed buffer is never a scratch.
        state.editor = Some(stub_editor_state("src/main.rs"));
        state.ed_mut().file_path = Some("/proj/src/main.rs".to_string());
        assert_eq!(empty_scratch_id(&state), None);
    }

    /// The ephemeral status line is dismissed only by events the user actually drives. Passive
    /// events the handler ignores must not dismiss it, or one arriving just after a slow save would
    /// wipe the "saved" message: key *releases* (kitty keyboard protocol), focus changes, and mouse
    /// *motion* (streamed continuously under mouse capture). Real actions still dismiss it.
    #[test]
    fn event_dismisses_status_only_for_actionable_events() {
        use crossterm::event::{
            KeyCode, KeyEvent, KeyEventState, MouseButton, MouseEvent, MouseEventKind,
        };
        let key = |kind| {
            Event::Key(KeyEvent {
                code: KeyCode::Char('s'),
                modifiers: KeyModifiers::CONTROL,
                kind,
                state: KeyEventState::NONE,
            })
        };
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert!(event_dismisses_status(&key(KeyEventKind::Press)));
        assert!(event_dismisses_status(&key(KeyEventKind::Repeat)));
        assert!(!event_dismisses_status(&key(KeyEventKind::Release)));
        assert!(!event_dismisses_status(&Event::FocusGained));
        assert!(!event_dismisses_status(&Event::FocusLost));
        // Mouse motion (hover) is passive and must not dismiss; clicks/scroll are deliberate.
        assert!(!event_dismisses_status(&mouse(MouseEventKind::Moved)));
        assert!(event_dismisses_status(&mouse(MouseEventKind::Down(
            MouseButton::Left
        ))));
        assert!(event_dismisses_status(&mouse(MouseEventKind::ScrollDown)));
        // Deliberate non-key events still dismiss it (matches the prior clear-on-any behaviour).
        assert!(event_dismisses_status(&Event::Resize(80, 24)));
    }

    /// `resolve_cli_path` resolves a relative arg against CWD, not against an arbitrary base.
    /// Tested here because the old (buggy) behaviour joined relative args with `project_paths[0]`
    /// — the regression we're guarding against is "user is CD'd into root B but `ae` resolves
    /// their relative arg under root A".
    #[test]
    fn resolve_cli_path_resolves_relative_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("hello.txt");
        std::fs::write(&file_path, "hi").unwrap();
        let prior_cwd = std::env::current_dir().ok();
        std::env::set_current_dir(dir.path()).unwrap();
        let resolved = resolve_cli_path("hello.txt").unwrap();
        // Restore CWD before any asserts (so a failure doesn't strand later tests).
        if let Some(cwd) = prior_cwd {
            let _ = std::env::set_current_dir(cwd);
        }
        assert_eq!(
            resolved,
            std::fs::canonicalize(&file_path).unwrap(),
            "relative CLI path should resolve under the process CWD"
        );
    }

    #[test]
    fn resolve_cli_path_canonicalizes_absolute_input() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("abs.txt");
        std::fs::write(&file_path, "hi").unwrap();
        let resolved = resolve_cli_path(file_path.to_str().unwrap()).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&file_path).unwrap());
    }

    #[test]
    fn resolve_cli_path_errors_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-file.txt");
        let err = resolve_cli_path(missing.to_str().unwrap()).unwrap_err();
        // The chained context should name the original arg so the CLI error is useful.
        assert!(
            format!("{err:#}").contains(missing.to_str().unwrap()),
            "error chain should mention the requested path: {err:#}"
        );
    }

    /// The actual bug-fix property: a file living under a *non-zero* root must classify to that
    /// root, not to root 0. Pairs `resolve_cli_path` with `strip_longest_root` the way
    /// `bootstrap` does.
    #[test]
    fn cli_path_under_non_zero_root_classifies_to_that_root() {
        let outer = tempfile::tempdir().unwrap();
        let root_a = outer.path().join("a");
        let root_b = outer.path().join("b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let file_in_b = root_b.join("sub/file.rs");
        std::fs::create_dir_all(file_in_b.parent().unwrap()).unwrap();
        std::fs::write(&file_in_b, "in b").unwrap();

        let project_paths = vec![
            std::fs::canonicalize(&root_a)
                .unwrap()
                .display()
                .to_string(),
            std::fs::canonicalize(&root_b)
                .unwrap()
                .display()
                .to_string(),
        ];
        let abs = resolve_cli_path(file_in_b.to_str().unwrap()).unwrap();
        let (idx, rel) = strip_longest_root(&abs.display().to_string(), &project_paths)
            .expect("file must classify under one of the roots");
        assert_eq!(idx, 1, "should classify under root B (index 1), not root 0");
        assert_eq!(rel, "sub/file.rs");
    }

    // ---- terminal_title ----

    #[test]
    fn terminal_title_falls_back_to_aether_before_project_activation() {
        let state = AppState {
            project_name: String::new(),
            project_paths: Vec::new(),
            root_labels: Vec::new(),
            viewport_cols: 80,
            viewport_rows: 24,
            should_quit: false,
            status: StatusMessage::default(),
            last_terminal_title: String::new(),
            clipboard: None,
            pending_leader: None,
            picker: crate::picker::PickerState::default(),
            save_prompt: None,
            confirm_prompt: None,
            editor: None,
            project_settings: None,
            help: HelpState::default(),
            lsp_status: std::collections::HashMap::new(),
            hover: None,
            diagnostic_counts: std::collections::HashMap::new(),
        pending_external_close: None,
        };
        assert_eq!(terminal_title(&state), "Aether");
    }

    #[test]
    fn terminal_title_shows_project_only_when_no_editor() {
        let mut state = AppState {
            project_name: "demo".into(),
            project_paths: vec!["/tmp/demo".into()],
            root_labels: vec![String::new()],
            viewport_cols: 80,
            viewport_rows: 24,
            should_quit: false,
            status: StatusMessage::default(),
            last_terminal_title: String::new(),
            clipboard: None,
            pending_leader: None,
            picker: crate::picker::PickerState::default(),
            save_prompt: None,
            confirm_prompt: None,
            editor: None,
            project_settings: None,
            help: HelpState::default(),
            lsp_status: std::collections::HashMap::new(),
            hover: None,
            diagnostic_counts: std::collections::HashMap::new(),
        pending_external_close: None,
        };
        assert_eq!(terminal_title(&state), "[demo]");
        // Once a buffer exists, the title grows to include the file label.
        state.editor = Some(stub_editor_state("(scratch 0)"));
        assert_eq!(terminal_title(&state), "[demo] (scratch 0)");
    }

    #[test]
    fn terminal_title_appends_dirty_marker() {
        let mut state = AppState {
            project_name: "demo".into(),
            project_paths: vec!["/tmp/demo".into()],
            root_labels: vec![String::new()],
            viewport_cols: 80,
            viewport_rows: 24,
            should_quit: false,
            status: StatusMessage::default(),
            last_terminal_title: String::new(),
            clipboard: None,
            pending_leader: None,
            picker: crate::picker::PickerState::default(),
            save_prompt: None,
            confirm_prompt: None,
            editor: Some(stub_editor_state("src/main.rs")),
            project_settings: None,
            help: HelpState::default(),
            lsp_status: std::collections::HashMap::new(),
            hover: None,
            diagnostic_counts: std::collections::HashMap::new(),
        pending_external_close: None,
        };
        // Clean buffer → no marker.
        assert_eq!(terminal_title(&state), "[demo] src/main.rs");
        // Local edits → `[+]`.
        if let Some(ed) = state.editor.as_mut() {
            ed.revision = 5;
        }
        assert_eq!(terminal_title(&state), "[demo] src/main.rs [+]");
        // External delete trumps `[+]`.
        if let Some(ed) = state.editor.as_mut() {
            ed.externally_deleted = true;
        }
        assert_eq!(terminal_title(&state), "[demo] src/main.rs [x]");
    }

    /// Minimal `EditorState` for title tests — only the fields the title code reads matter
    /// (`file_label`, `revision`, `saved_revision`, `externally_modified`, `externally_deleted`).
    /// The rest is filled with sensible defaults.
    fn stub_editor_state(label: &str) -> EditorState {
        EditorState {
            mode: EditorMode::Normal,
            buffer_id: 1,
            viewport_id: 1,
            cursor: Default::default(),
            scroll_logical_line: 0,
            scroll_skip_rows: 0,
            window_first_logical_line: 0,
            lines: Vec::new(),
            line_count: 0,
            max_scroll_logical_line: 0,
            wrap: aether_protocol::viewport::WrapMode::None,
            diff_view: false,
            scroll_col: 0,
            pending_scroll_lines: 0,
            drag_anchor: None,
            revision: 0,
            saved_revision: 0,
            externally_modified: false,
            externally_deleted: false,
            pending_count: 0,
            pending_find: None,
            pending_surround: None,
            last_repeat: None,
            search: Default::default(),
            blame: Default::default(),
            file_path: None,
            file_label: label.into(),
            language: None,
            lsp_server: None,
        }
    }

    #[test]
    fn scroll_advance_steps_one_visual_row_across_tall_lines() {
        use aether_protocol::viewport::{Segment, VirtualRow, VirtualRowKind, VisualRow};

        // Heights: line0=1, line1=3 (2 phantom deleted rows + 1 content), line2=1, line3=1.
        // Global visual rows from the top: 0=(0,0) 1=(1,0) 2=(1,1) 3=(1,2) 4=(2,0) 5=(3,0).
        let line = |phantom: usize, content: usize| LogicalLineRender {
            logical_line: 0,
            visual_rows: (0..content.max(1))
                .map(|_| VisualRow {
                    byte_offset: 0,
                    continuation_indent: 0,
                    segments: vec![Segment {
                        text: String::new(),
                        highlights: vec![],
                    }],
                })
                .collect(),
            search_matches: vec![],
            virtual_rows_above: (0..phantom)
                .map(|_| VirtualRow {
                    text: String::new(),
                    kind: VirtualRowKind::Deleted,
                })
                .collect(),
            diff_marker: None,
            diagnostics: vec![],
        };

        let mut state = empty_app_state(80, 24);
        state.editor = Some(stub_editor_state("buf"));
        state.ed_mut().window_first_logical_line = 0;
        state.ed_mut().lines = vec![line(0, 1), line(2, 1), line(0, 1), line(0, 1)];

        let walk = |delta: i64, from: (u32, u32)| {
            let (mut l, mut s) = from;
            scroll_advance(&state, &mut l, &mut s, delta);
            (l, s)
        };

        // Down: one visual row at a time, stopping *on* the phantom rows of line 1.
        assert_eq!(walk(1, (0, 0)), (1, 0), "into line 1's first phantom row");
        assert_eq!(walk(2, (0, 0)), (1, 1), "second phantom row");
        assert_eq!(walk(3, (0, 0)), (1, 2), "line 1's content row");
        assert_eq!(walk(4, (0, 0)), (2, 0), "across the tall line to line 2");

        // Up: mirror image.
        assert_eq!(walk(-1, (2, 0)), (1, 2));
        assert_eq!(walk(-3, (2, 0)), (1, 0));
        assert_eq!(walk(-4, (2, 0)), (0, 0));
        assert_eq!(walk(-10, (1, 1)), (0, 0), "clamps at the top");
    }

    #[test]
    fn awaiting_key_tracks_pending_captures() {
        let mut state = empty_app_state(80, 24);
        state.editor = Some(stub_editor_state("buf"));

        // Nothing armed → not awaiting.
        assert!(!awaiting_key(&state));

        // Leader (`Space`) armed.
        state.pending_leader = Some(PendingLeader::Space);
        assert!(awaiting_key(&state));
        state.pending_leader = None;

        // Find (`f`/`t`) armed.
        state.ed_mut().pending_find = Some(PendingFind {
            direction: Direction::Forward,
            till: false,
            extend: false,
            count: 1,
        });
        assert!(awaiting_key(&state));
        state.ed_mut().pending_find = None;

        // Surround (`Ctrl-s`) armed.
        state.ed_mut().pending_surround = Some(SurroundTarget::Selection);
        assert!(awaiting_key(&state));
        state.ed_mut().pending_surround = None;

        // Count prefix mid-entry (`2`… awaiting a motion).
        state.ed_mut().pending_count = 2;
        assert!(awaiting_key(&state));
        state.ed_mut().pending_count = 0;

        assert!(!awaiting_key(&state));
    }

    #[test]
    fn awaiting_key_does_not_panic_without_editor() {
        // No editor: the editor-scoped captures are simply unreachable, but leader still counts.
        let mut state = empty_app_state(80, 24);
        assert!(!awaiting_key(&state));
        state.pending_leader = Some(PendingLeader::Space);
        assert!(awaiting_key(&state));
    }

    #[test]
    fn help_overlay_key_handling() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut state = empty_app_state(80, 24);
        state.help.open = true;
        // Simulate a render recording the geometry (100 lines, 10 visible → max offset 90), which
        // is what lets the key handler clamp correctly.
        state.help.scroll.record(100, 10);
        // Down scrolls, Up scrolls back and saturates at the top.
        handle_help_key(&mut state, press(KeyCode::Down)).unwrap();
        assert_eq!(state.help.scroll.offset(), 1);
        handle_help_key(&mut state, press(KeyCode::Up)).unwrap();
        assert_eq!(state.help.scroll.offset(), 0);
        handle_help_key(&mut state, press(KeyCode::Up)).unwrap();
        assert_eq!(state.help.scroll.offset(), 0, "scroll saturates at the top");
        // End jumps to the bottom (clamped to max offset), Home returns to the top.
        handle_help_key(&mut state, press(KeyCode::End)).unwrap();
        assert_eq!(state.help.scroll.offset(), 90);
        handle_help_key(&mut state, press(KeyCode::Home)).unwrap();
        assert_eq!(state.help.scroll.offset(), 0);
        // h/l (and ←/→) switch tabs, wrapping at both ends, and reset the scroll to the top.
        state.help.scroll.record(100, 10);
        handle_help_key(&mut state, press(KeyCode::End)).unwrap();
        assert_eq!(state.help.scroll.offset(), 90);
        assert_eq!(state.help.tab, HelpTab::Normal);
        handle_help_key(&mut state, press(KeyCode::Char('l'))).unwrap();
        assert_eq!(state.help.tab, HelpTab::Insert, "l moves to the next tab");
        assert_eq!(
            state.help.scroll.offset(),
            0,
            "switching tabs resets the scroll"
        );
        handle_help_key(&mut state, press(KeyCode::Char('h'))).unwrap();
        assert_eq!(state.help.tab, HelpTab::Normal, "h moves back");
        handle_help_key(&mut state, press(KeyCode::Char('h'))).unwrap();
        assert_eq!(
            state.help.tab,
            HelpTab::Application,
            "h wraps to the last tab"
        );
        handle_help_key(&mut state, press(KeyCode::Right)).unwrap();
        assert_eq!(state.help.tab, HelpTab::Normal, "→ wraps to the first tab");

        // Esc closes the overlay; other keys are swallowed while it's open.
        assert!(state.help.open);
        handle_help_key(&mut state, press(KeyCode::Esc)).unwrap();
        assert!(!state.help.open);
    }
}
