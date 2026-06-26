//! Application state and event loop. Modal editing (Normal vs Insert) lives entirely here; the
//! server has no notion of mode.

use aether_client::session::ConnState;
use aether_protocol::cursor::{CursorState, Direction, Granularity};
use aether_protocol::git::GitBufferStatus;
use aether_protocol::input::SurroundTarget;
use aether_protocol::lsp::{DiagnosticCounts, LspServerRef, LspServerStatus};
use aether_protocol::search::SearchSummary;
use aether_protocol::viewport::{DiagnosticSeverity, LogicalLineRender, WrapMode};
use aether_protocol::{BufferId, LogicalPosition, ViewportId};
use anyhow::{Context, Result};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
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

/// Multi-key prefix the next keystroke completes: `Space` (the picker / app chords). Drives the
/// underline cursor that signals "waiting for one more key"; the actual second-key dispatch lives
/// in the core.
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
    /// Active match-option chips synced from the core, as `(label, underline)` pairs — `label` is
    /// the abbreviation (`Aa`/`aa`/`wd`/`lit`), `underline` marks the whole-word chip. Rendered
    /// before the query with the same styling as the grep picker's filter chips. Empty when all
    /// options are at their default (regex, smartcase).
    pub option_chips: Vec<(String, bool)>,
    /// The keyboard-selected option chip (index into `option_chips`), or `None` while the query
    /// owns the keyboard. The selected chip renders inverted and the caret is hidden.
    pub chip_selected: Option<usize>,
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
    /// Replacement key (see [`aether_client::effect::Effect::Toast`]). A new grouped toast evicts
    /// any existing toast sharing this key instead of stacking. `None` toasts always stack.
    pub group: Option<String>,
}

/// Top-level UI state. Anything that exists regardless of whether a buffer is open lives on
/// `AppState`; anything that's per-screen (editor vs file browser) lives inside `Screen`.
///
/// Overlays — the picker and the save-as prompt — sit on top of either screen as `Option`s on
/// `AppState`. They don't change which screen is underneath, so opening/closing them needs no
/// "return mode" bookkeeping.
#[allow(dead_code)] // view-model surface synced from the core; ui matches on it
pub struct AppState {
    /// Active workspace name. Empty string before a workspace is activated — the no-workspace view
    /// shows the workspace picker instead of the editor in that state.
    pub workspace_name: String,
    /// Active workspace's root paths (absolute, server-canonical). Empty before activation.
    pub workspace_paths: Vec<String>,
    /// One disambiguated label per entry in `workspace_paths`, aligned by index. Computed by
    /// `labels::root_labels` and refreshed via `refresh_root_labels` whenever `workspace_paths`
    /// changes. Used for UI rendering (status bar, picker prefixes, explorer breadcrumb) — the
    /// protocol is unaware.
    pub root_labels: Vec<String>,
    pub viewport_cols: u32,
    pub viewport_rows: u32,
    pub should_quit: bool,
    /// Transient feedback. Carries a `StatusKind` so the renderer can colour the message — "saved"
    /// reads as success, "save failed" reads as error, etc. Constructed via `StatusMessage::info` /
    /// `::success` / `::warning` / `::error`. This is now a *handoff* slot: the shell drains it into
    /// a [`Toast`] (see [`AppState::toasts`]); only code without shell access (workspace-settings
    /// handlers) writes it directly.
    pub status: StatusMessage,
    /// Active floating toasts, stacked bottom-right (newest at the bottom). Each is expired on a
    /// timer by the shell, keyed by its `id`. Fed from the same stream as `status`.
    pub toasts: Vec<Toast>,
    /// Connection state, mirrored from the session. The disconnect toast auto-expires, so the
    /// status bar shows a persistent `reconnecting…` / `disconnected` indicator while it's down.
    pub conn: ConnState,
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
    /// Active open-from-path prompt (`Space Alt-w`): a single-line path input shown in the status
    /// row. `Some` holds the field's text + caret; text entry is shell-owned (synced into the
    /// core's `Prompt::OpenPath`), `Enter` opens via `workspace/open_path`, `Esc` cancels.
    pub open_path_prompt: Option<crate::text_input::TextInput>,
    /// Active binary y/N confirmation prompt. Layers on top of any other overlay (including
    /// `save_prompt`, e.g. for the save-as overwrite confirm). Holds the question text and the
    /// action to run on `y`.
    pub confirm_prompt: Option<ConfirmPrompt>,
    /// `None` before a workspace is activated, or transiently while switching. Most key handlers
    /// early-return without touching state in that case; the no-workspace view (workspace picker)
    /// is rendered instead by `ui::draw`.
    pub editor: Option<EditorState>,
    /// Active workspace-settings overlay (`Space ,`). When `Some`, draws a centered modal listing
    /// the workspace's roots, with a permanent add-root input row at the bottom. Closed by Esc.
    pub workspace_settings: Option<WorkspaceSettingsState>,
    /// Active application-settings overlay (`Space .`). When `Some`, draws a centered modal
    /// listing the global settings (e.g. soft wrap). Closed by Esc.
    pub app_settings: Option<AppSettingsState>,
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

/// Workspace-settings overlay view model. A render-only projection of the core's
/// `Session::workspace_settings` (which owns the state + key handling): shows an editable
/// workspace-name field, then the active workspace's roots, then an always-present "add root" input
/// row; `selected` is the focused field. Populated each frame by `Shell::sync_workspace_settings`.
///
/// Selection model: `selected == 0` is the name field; `1..=roots.len()` are the root rows (root
/// `i` at index `i + 1`); `roots.len() + 1` is the add-root input row. The input row is always
/// reachable, which is why we focus it on open — most overlay opens are to add a root.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSettingsState {
    /// Editable buffer for the name field (index 0), mirrored from the core's in-progress edit.
    pub name_input: crate::text_input::TextInput,
    pub roots: Vec<String>,
    pub selected: usize,
    /// Text being typed into the add-root input row.
    pub add_input: crate::text_input::TextInput,
    /// In-dialog error from the last add or remove attempt. Rendered as the bottom line of the
    /// overlay. Cleared when the user edits `add_input` or initiates another action.
    pub error: Option<String>,
}

/// Application-settings overlay view model. A render-only projection of the core's
/// `Session::app_settings` + `Session::app_setting_groups`: the grouped checkbox settings and the
/// focused *flat* row index (across all groups). Populated each frame by `Shell::sync_app_settings`.
#[derive(Debug, Clone, Default)]
pub struct AppSettingsState {
    pub groups: Vec<aether_client::session::AppSettingGroup>,
    pub selected: usize,
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
    /// True while a sneak (`s`/`S`/`Alt-s`) session is active — the next keystrokes are query/label
    /// input, so the cursor renders as the "awaiting key" underscore.
    pub sneak_active: bool,
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
    /// status bar shows (language alone is ambiguous when a workspace runs several same-language
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
    /// Buffer line the cached blame was fetched for, so a stale entry is never shown for the
    /// wrong line.
    pub line: Option<u32>,
    /// Pre-formatted end-of-line label (e.g. `author · 3 days ago`), supplied by the core.
    pub text: Option<String>,
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
    /// `true` while a buffer is open. False during the no-workspace view and the brief window
    /// between switching workspaces (old editor torn down, new one not yet built).
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
/// status row — `[{workspace}] {file_label}` — with a leading dot when the buffer is dirty or
/// changed on disk, so the title answers "what am I editing, and is it saved?" at a glance. The
/// dot leads (not trails) to match the favicon's position in the web client's tab; a terminal
/// title can't carry colour, so every non-clean state shows the same plain dot (the status bar
/// colour-codes it). Before any workspace is active we fall back to a bare `Aether` placeholder;
/// without a buffer (transient workspace-switch window) we just show the workspace name.
fn terminal_title(state: &AppState) -> String {
    // The label is only meaningful with an open editor (the transient workspace-switch window has
    // none). `title_body` yields `None` before a workspace is active → the title is just `Aether`.
    let label = if state.has_editor() {
        state.ed().file_label.as_str()
    } else {
        ""
    };
    let Some(body) = aether_client::labels::title_body(&state.workspace_name, label) else {
        return "Aether".to_string();
    };
    let dot = if state.has_editor() && state.buffer_status().is_some() {
        format!("{BUFFER_STATUS_DOT} ")
    } else {
        String::new()
    };
    format!("{dot}{body} - Aether")
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
        ed.pending_find.is_some()
            || ed.pending_surround.is_some()
            || ed.pending_count > 0
            || ed.sneak_active
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
        || state.workspace_settings.is_some()
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

/// Resolve a CLI-supplied file/dir argument to an absolute, canonical path. Relative args are
/// resolved against the *current working directory* (shell convention), then canonicalized so
/// any `..` / symlinks line up with the workspace's canonical roots — without that, prefix
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

/// Strip the longest matching workspace root off `abs`. Returns `(root_index, relative_path)`,
/// where `relative_path` is empty if `abs` *is* the root itself.
pub(crate) fn strip_longest_root(abs: &str, workspace_paths: &[String]) -> Option<(usize, String)> {
    let abs_path = std::path::Path::new(abs);
    workspace_paths
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `resolve_cli_path` resolves a relative arg against CWD, not against an arbitrary base.
    /// Tested here because the old (buggy) behaviour joined relative args with `workspace_paths[0]`
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

        let workspace_paths = vec![
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
        let (idx, rel) = strip_longest_root(&abs.display().to_string(), &workspace_paths)
            .expect("file must classify under one of the roots");
        assert_eq!(idx, 1, "should classify under root B (index 1), not root 0");
        assert_eq!(rel, "sub/file.rs");
    }

    // ---- terminal_title ----

    #[test]
    fn terminal_title_falls_back_to_aether_before_workspace_activation() {
        let state = AppState {
            workspace_name: String::new(),
            workspace_paths: Vec::new(),
            root_labels: Vec::new(),
            viewport_cols: 80,
            viewport_rows: 24,
            should_quit: false,
            status: StatusMessage::default(),
            toasts: Vec::new(),
            conn: ConnState::Connected,
            last_terminal_title: String::new(),
            clipboard: None,
            pending_leader: None,
            picker: crate::picker::PickerState::default(),
            save_prompt: None,
            open_path_prompt: None,
            confirm_prompt: None,
            editor: None,
            workspace_settings: None,
            app_settings: None,
            help: HelpState::default(),
            lsp_status: std::collections::HashMap::new(),
            hover: None,
            diagnostic_counts: std::collections::HashMap::new(),
        };
        assert_eq!(terminal_title(&state), "Aether");
    }

    #[test]
    fn terminal_title_shows_workspace_only_when_no_editor() {
        let mut state = AppState {
            workspace_name: "demo".into(),
            workspace_paths: vec!["/tmp/demo".into()],
            root_labels: vec![String::new()],
            viewport_cols: 80,
            viewport_rows: 24,
            should_quit: false,
            status: StatusMessage::default(),
            toasts: Vec::new(),
            conn: ConnState::Connected,
            last_terminal_title: String::new(),
            clipboard: None,
            pending_leader: None,
            picker: crate::picker::PickerState::default(),
            save_prompt: None,
            open_path_prompt: None,
            confirm_prompt: None,
            editor: None,
            workspace_settings: None,
            app_settings: None,
            help: HelpState::default(),
            lsp_status: std::collections::HashMap::new(),
            hover: None,
            diagnostic_counts: std::collections::HashMap::new(),
        };
        assert_eq!(terminal_title(&state), "[demo] - Aether");
        // Once a buffer exists, the title grows to include the file label.
        state.editor = Some(stub_editor_state("(scratch 0)"));
        assert_eq!(terminal_title(&state), "[demo] (scratch 0) - Aether");
    }

    #[test]
    fn terminal_title_prepends_status_dot() {
        let mut state = AppState {
            workspace_name: "demo".into(),
            workspace_paths: vec!["/tmp/demo".into()],
            root_labels: vec![String::new()],
            viewport_cols: 80,
            viewport_rows: 24,
            should_quit: false,
            status: StatusMessage::default(),
            toasts: Vec::new(),
            conn: ConnState::Connected,
            last_terminal_title: String::new(),
            clipboard: None,
            pending_leader: None,
            picker: crate::picker::PickerState::default(),
            save_prompt: None,
            open_path_prompt: None,
            confirm_prompt: None,
            editor: Some(stub_editor_state("src/main.rs")),
            workspace_settings: None,
            app_settings: None,
            help: HelpState::default(),
            lsp_status: std::collections::HashMap::new(),
            hover: None,
            diagnostic_counts: std::collections::HashMap::new(),
        };
        // Clean buffer → no dot.
        assert_eq!(terminal_title(&state), "[demo] src/main.rs - Aether");
        // Local edits → leading dot.
        if let Some(ed) = state.editor.as_mut() {
            ed.revision = 5;
        }
        assert_eq!(terminal_title(&state), "● [demo] src/main.rs - Aether");
        // External delete is still a (single, plain) leading dot — the title can't colour-code it.
        if let Some(ed) = state.editor.as_mut() {
            ed.externally_deleted = true;
        }
        assert_eq!(terminal_title(&state), "● [demo] src/main.rs - Aether");
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
            sneak_active: false,
            search: Default::default(),
            blame: Default::default(),
            file_path: None,
            file_label: label.into(),
            language: None,
            lsp_server: None,
        }
    }
}
