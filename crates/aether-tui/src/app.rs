//! Application state and event loop. Modal editing (Normal vs Insert) lives entirely here; the
//! server has no notion of mode.

use crate::text_input::PromptKeyOutcome;
use aether_client::keymap::Action;
use aether_protocol::buffer::BufferClosedParams;
use aether_protocol::cursor::{CursorState, Direction, Granularity, Motion};
use aether_protocol::git::{BlameInfo, GitBufferStatus};
use aether_protocol::input::SurroundTarget;
use aether_protocol::lsp::{DiagnosticCounts, LspServerRef, LspServerStatus};
use aether_protocol::search::SearchSummary;
use aether_protocol::viewport::{DiagnosticSeverity, LogicalLineRender, WrapMode};
use aether_protocol::{BufferId, LogicalPosition, ViewportId};
use anyhow::{Context, Result};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use crossterm::execute;
use std::io::stdout;
use std::time::Instant;

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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Success,
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

/// A floating toast: a transient notification, stacked in the bottom-right (newest at the bottom).
/// Fed from the same `StatusMessage` stream as before; the shell assigns the `id` and expires it on
/// a timer. Mirrors the web/native clients' toasts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub id: u64,
    pub text: String,
    pub kind: StatusKind,
}

/// Top-level UI state. Anything that exists regardless of whether a buffer is open lives on
/// `AppState`; anything that's per-screen (editor vs file browser) lives inside `Screen`.
///
/// Overlays — the picker and the save-as prompt — sit on top of either screen as `Option`s on
/// `AppState`. They don't change which screen is underneath, so opening/closing them needs no
/// "return mode" bookkeeping.
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    /// Transient feedback. Carries a `StatusKind` so the renderer can colour the message — "saved"
    /// reads as success, "save failed" reads as error, etc. Constructed via `StatusMessage::info` /
    /// `::success` / `::warning` / `::error`. This is now a *handoff* slot: the shell drains it into
    /// a [`Toast`] (see [`AppState::toasts`]); only code without shell access (project-settings
    /// handlers) writes it directly.
    pub status: StatusMessage,
    /// Active floating toasts, stacked bottom-right (newest at the bottom). Each is expired on a
    /// timer by the shell, keyed by its `id`. Fed from the same stream as `status`.
    pub toasts: Vec<Toast>,
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
/// Hover popover content: severity-coloured paragraphs (diagnostics / commit details), or the
/// shared Markdown AST (LSP hover) which the UI renders to styled lines.
pub enum HoverBody {
    Blocks(Vec<HoverBlock>),
    Markdown(Vec<aether_client::markdown::Block>),
}

impl HoverBody {
    /// The whole popover as plain text, for "copy popover content" (`Ctrl-y`). Diagnostic blocks are
    /// joined by blank lines; Markdown is flattened via the shared AST serializer.
    pub fn to_plain_text(&self) -> String {
        match self {
            HoverBody::Blocks(blocks) => blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
            HoverBody::Markdown(blocks) => aether_client::markdown::to_plain(blocks),
        }
    }
}

pub struct HoverPopup {
    pub body: HoverBody,
    pub scroll: crate::scroll::ScrollState,
}

impl HoverPopup {
    pub fn new(body: HoverBody) -> Self {
        Self {
            body,
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
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
pub struct ConfirmPrompt {
    /// What gets shown on the status row, formatted as `" {message}? [y/N]"`.
    pub message: String,
    pub action: ConfirmAction,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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

#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
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
    /// Buffer-wide Git change summary (added/modified/deleted line counts vs HEAD) for the status
    /// bar, refreshed alongside `line_count` from every window the server sends.

    /// Buffer-level Git status (branch + staged/unstaged counts) for the status bar; `None` outside
    /// a repo. Refreshed from every window, like `git_changes`.
    pub git_status: Option<GitBufferStatus>,
    /// Highest legal `scroll_logical_line` — server-computed so it accounts for wrap, putting
    /// the buffer's last visual row at the bottom of the viewport.
    pub max_scroll_logical_line: u32,
    /// Total visual rows in the whole buffer (wrapped rows + diff phantoms), from the window.
    /// Drives the editor scrollbar's thumb size; `0` means unknown (no window yet).
    pub total_visual_rows: u32,
    /// Absolute visual row at the top of the viewport (accounts for wrap and `scroll_skip_rows`).
    /// The editor scrollbar's thumb position.
    pub top_visual_row: u32,
    pub wrap: WrapMode,
    /// Inline diff view toggle. Server-authoritative (per-viewport); mirrored here so the
    /// keybinding can flip it. When on, the server interleaves phantom "deleted" rows into the
    /// pushed window.
    pub diff_view: bool,
    /// Diff baseline the gutter compares against: `Head` (all uncommitted) or `Index` (unstaged
    /// only). Server-authoritative (per-viewport); mirrored here so the keybinding can flip it and
    /// so it can be re-applied (sticky) on the next buffer's subscribe.
    /// Horizontal scroll, in bytes. Only meaningful when `wrap == WrapMode::None`; reset to 0
    /// when soft wrap is on (wrapped content never overflows). Client-only.
    pub scroll_col: u32,
    /// Accumulated vertical-scroll delta from arrow-key / PageUp-PageDown bursts. Deferred
    /// to a coalesced `viewport/scroll` RPC at draw time.
    pub pending_scroll_lines: i64,
    /// Anchor position set by a left-mouse-button down. Subsequent drags use it as the
    /// selection anchor; cleared on mouse-up. This is the *raw clicked* position, not the
    /// server's snapped cursor, so word/line drags keep re-snapping from the original cell.
    pub drag_anchor: Option<LogicalPosition>,
    /// Snapping granularity of the drag gesture in progress — set on mouse-down from the click
    /// streak (single/double/triple → char/word/line) and applied to every drag update.
    pub drag_granularity: Granularity,
    /// `(when, row, col)` of the most recent left-button press. The terminal reports plain
    /// presses only, so multi-clicks are synthesized: a press on the same cell within
    /// [`MULTI_CLICK_WINDOW`] extends `click_streak`, anything else resets it to 1.
    pub last_click: Option<(Instant, u16, u16)>,
    /// Length of the current same-cell click chain (1 = single, 2 = double, 3+ = triple).
    pub click_streak: u32,
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
    /// The buffer auto-closes once hidden (server-side flag, from `buffer/open` and `buffer/state`
    /// pushes). Shown by italicising the status-bar file label; promoted to permanent by the
    /// first edit, a save, or a reload (`Space r`).
    pub transient: bool,
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

/// Dot used as the buffer-state indicator (status bar + terminal title). The heavier `●` (same
/// glyph as the LSP status indicator), which reads more clearly than the lighter bullet.
pub const BUFFER_STATUS_DOT: &str = "●";

/// Which dirty / external-change condition applies to the active buffer, in precedence order.
/// Rendered as a colour-coded dot in the status bar; the colours match the web client's favicon.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BufferStatusKind {
    /// Unsaved local edits (`revision != saved_revision`).
    Unsaved,
    /// The file changed on disk underneath us.
    ExternallyModified,
    /// The file was removed on disk.
    ExternallyDeleted,
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

    /// Buffer-state for the status indicator, highest-precedence first so the user always sees
    /// the most urgent flag: removed on disk → changed on disk → unsaved local edits → `None`
    /// when clean (or no editor is attached). The status bar renders this as a colour-coded dot
    /// (see `buffer_status_color`); the terminal title shows a leading plain dot.
    pub fn buffer_status(&self) -> Option<BufferStatusKind> {
        if !self.has_editor() {
            return None;
        }
        let ed = self.ed();
        if ed.externally_deleted {
            Some(BufferStatusKind::ExternallyDeleted)
        } else if ed.externally_modified {
            Some(BufferStatusKind::ExternallyModified)
        } else if ed.revision != ed.saved_revision {
            Some(BufferStatusKind::Unsaved)
        } else {
            None
        }
    }
}

/// Re-emit the terminal title via OSC if the derived title has changed since the last frame.
/// Cheap when state is unchanged (just a string compare); a single OSC write when it does
/// change. Failures are swallowed — the title is cosmetic and we'd rather have the editor
/// keep running than crash on a quirky terminal that doesn't accept the sequence.
pub fn refresh_terminal_title(state: &mut AppState) {
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
/// status row — `[{project}] {file_label}` — with a leading dot when the buffer is dirty or
/// changed on disk, so the title answers "what am I editing, and is it saved?" at a glance. The
/// dot leads (not trails) to match the favicon's position in the web client's tab; a terminal
/// title can't carry colour, so every non-clean state shows the same plain dot (the status bar
/// colour-codes it). Before any project is active we fall back to a bare `Aether` placeholder;
/// without a buffer (transient project-switch window) we just show the project name.
fn terminal_title(state: &AppState) -> String {
    if state.project_name.is_empty() {
        return "Aether".to_string();
    }
    if !state.has_editor() {
        return format!("[{}]", state.project_name);
    }
    let prefix = if state.buffer_status().is_some() {
        format!("{BUFFER_STATUS_DOT} ")
    } else {
        String::new()
    };
    format!(
        "{}[{}] {}",
        prefix,
        state.project_name,
        state.ed().file_label
    )
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

pub fn apply_cursor_style(state: &AppState) {
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

/// Format `abs` as `"{root_label}: {relative}"` against the longest-matching project root, or
/// fall back to the raw absolute path when nothing matches. `root_labels` must be aligned by
/// index with `project_paths`. Single-root projects get a bare relative path — their label is
/// `""` (see `labels::root_labels`), so the prefix only appears when it disambiguates. Use this
/// for display — see `project_relative_path` for the typeable-path variant that the save-as
/// prefill needs.
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
pub fn resolve_cli_path(arg: &str) -> Result<std::path::PathBuf> {
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

// The selection/clipboard Ctrl shortcuts no longer branch on mode here: each mode binds its own
// action (`Copy`/`DeleteSelection`/… in Normal, `CopyLine`/`DeleteLine`/… in Insert), so
// `run_action` dispatches straight to `copy_to_clipboard(Selection)` / `delete_line` / etc.

// ---- project settings -------------------------------------------------------------------------

/// Hydrate the project-settings overlay from the currently-active project's name + roots and open
/// it. Cheap (just clones); no RPC. Focus lands on the always-present input row at the bottom —
/// most overlay opens (especially the post-create flow) are to add a root, and this avoids an
/// extra keypress for that case. The name field sits above the roots and is reached with Alt-k.
pub fn open_project_settings(state: &mut AppState) {
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
pub fn handle_help_key(state: &mut AppState, k: KeyEvent) -> Result<()> {
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
pub fn handle_help_mouse(state: &mut AppState, m: MouseEvent) {
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
pub async fn handle_project_settings_key(
    client: &crate::connection::Handle,
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
async fn commit_rename_if_changed(
    client: &crate::connection::Handle,
    state: &mut AppState,
) -> Result<bool> {
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
            let msg = e.message.clone();
            if let Some(s) = state.project_settings.as_mut() {
                s.error = Some(msg);
            }
            Ok(false)
        }
    }
}

async fn commit_add_root(client: &crate::connection::Handle, state: &mut AppState) -> Result<()> {
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
                s.error = Some(e.message.clone());
            }
        }
    }
    apply_cursor_style(state);
    Ok(())
}

async fn remove_root(
    client: &crate::connection::Handle,
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
                    // The shell drains this and feeds the core the same `buffer/closed`
                    // shape another client's close would push — one switching path.
                    state.pending_external_close =
                        Some(aether_protocol::buffer::BufferClosedParams {
                            buffer_id: cur,
                            next_buffer_id: r.next_buffer_id,
                        });
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
            let msg = if e.code
                == aether_protocol::error::ErrorCode::DIRTY_BUFFERS_PREVENT_REMOVE.code()
            {
                e.message.clone()
            } else {
                format!("remove root failed: {}", e.message)
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

#[cfg(test)]
mod tests {
    use super::*;

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
            toasts: Vec::new(),
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
            toasts: Vec::new(),
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
    fn terminal_title_prepends_status_dot() {
        let mut state = AppState {
            project_name: "demo".into(),
            project_paths: vec!["/tmp/demo".into()],
            root_labels: vec![String::new()],
            viewport_cols: 80,
            viewport_rows: 24,
            should_quit: false,
            status: StatusMessage::default(),
            toasts: Vec::new(),
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
        // Clean buffer → no dot.
        assert_eq!(terminal_title(&state), "[demo] src/main.rs");
        // Local edits → leading dot.
        if let Some(ed) = state.editor.as_mut() {
            ed.revision = 5;
        }
        assert_eq!(terminal_title(&state), "● [demo] src/main.rs");
        // External delete is still a (single, plain) leading dot — the title can't colour-code it.
        if let Some(ed) = state.editor.as_mut() {
            ed.externally_deleted = true;
        }
        assert_eq!(terminal_title(&state), "● [demo] src/main.rs");
    }

    /// Minimal `EditorState` for title tests — only the fields the title code reads matter
    /// (`file_label`, `revision`, `saved_revision`, `externally_modified`, `externally_deleted`).
    /// The rest is filled with sensible defaults.
    fn stub_editor_state(label: &str) -> EditorState {
        EditorState {
            transient: false,
            mode: EditorMode::Normal,
            buffer_id: 1,
            viewport_id: 1,
            cursor: Default::default(),
            scroll_logical_line: 0,
            scroll_skip_rows: 0,
            window_first_logical_line: 0,
            lines: Vec::new(),
            line_count: 0,
            git_status: None,
            max_scroll_logical_line: 0,
            total_visual_rows: 0,
            top_visual_row: 0,
            wrap: aether_protocol::viewport::WrapMode::None,
            diff_view: false,
            scroll_col: 0,
            pending_scroll_lines: 0,
            drag_anchor: None,
            drag_granularity: Granularity::Char,
            last_click: None,
            click_streak: 0,
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
}
