//! Application state and event loop. Modal editing (Normal vs Insert) lives entirely here; the
//! server has no notion of mode.

use crate::client::Client;
use crate::clipboard;
use crate::text_input::PromptKeyOutcome;
use crate::ui;
use aether_protocol::buffer::{
    BufferClose, BufferCloseParams, BufferCopy, BufferCopyParams, BufferCopyResult, BufferCut,
    BufferCutResult, BufferOpen, BufferOpenParams, BufferOpenResult, BufferReload,
    BufferReloadParams, BufferSave, BufferSaveParams, BufferState, BufferStateParams, CopyScope,
};
use aether_protocol::cursor::{
    CursorBufferOnlyParams, CursorContract, CursorExpand, CursorMove, CursorMoveParams, CursorRedo,
    CursorSelectLine, CursorSelectLineParams, CursorSet, CursorSetParams, CursorState,
    CursorSwapAnchor, CursorSwapAnchorParams, CursorUndo, CursorUndoParams, CursorUndoResult,
    Direction, Motion, VerticalDirection, WordBoundary,
};
use aether_protocol::envelope::{ClientInbound, NotificationMethod};
use aether_protocol::error::ErrorCode;
use aether_protocol::project::{ProjectActivate, ProjectActivateParams, ProjectActivateResult};
use aether_protocol::input::{
    BufferOnlyParams, EditResult, InputBackspace, InputDedent, InputDelete, InputIndent,
    InputJoinLines, InputMoveLines, InputMoveLinesParams, InputNewlineAndIndent, InputRedo,
    InputText, InputTextParams, InputToggleComment, InputUndo, UndoResult,
};
use aether_protocol::picker::{
    PickerGrepNavigate, PickerGrepNavigateParams, PickerHide, PickerHideParams, PickerItem,
    PickerKind, PickerQuery, PickerQueryParams, PickerSelect, PickerSelectParams,
    PickerSelectResult, PickerUpdate, PickerUpdateParams, PickerView, PickerViewParams,
};
use aether_protocol::search::{
    SearchClear, SearchClearParams, SearchNavParams, SearchNext, SearchPrev, SearchSet,
    SearchSetParams, SearchStateChanged, SearchSummary,
};
use aether_protocol::viewport::{
    LogicalLineRender, ScrollPosition, ViewportLinesChanged, ViewportLinesChangedParams,
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
}

#[derive(Debug)]
pub struct SearchSnapshot {
    pub cursor: CursorState,
    pub scroll_logical_line: u32,
    pub query: String,
    pub active: bool,
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
    pub status: String,
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
}

/// Project-settings overlay. Shows the active project's roots plus an always-present "add root"
/// input row at the bottom; `selected` is the row highlight. Source of truth for `roots` is the
/// server (synced via `sync_project_paths`).
///
/// Selection model: `selected` indexes `roots` when `< roots.len()`; the special value
/// `roots.len()` selects the input row. The input row is always reachable, which is why we focus
/// it on open — most overlay opens are to add a root.
#[derive(Debug, Clone, Default)]
pub struct ProjectSettingsState {
    pub project_name: String,
    pub roots: Vec<String>,
    pub selected: usize,
    /// Text being typed into the add-root input row.
    pub add_input: crate::text_input::TextInput,
    /// In-dialog error from the last add or remove attempt. Rendered as the bottom line of the
    /// overlay. Cleared when the user edits `add_input` or initiates another action.
    pub error: Option<String>,
    /// `true` when a delete is awaiting confirmation on the currently-selected root row. The row
    /// renders as `remove "<path>"? [y/N]`; key handling is restricted to confirm/cancel until
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
    pub window_first_logical_line: u32,
    pub lines: Vec<LogicalLineRender>,
    /// Total logical lines in the buffer, kept fresh from every viewport response /
    /// `viewport/lines_changed` notification.
    pub line_count: u32,
    /// Highest legal `scroll_logical_line` — server-computed so it accounts for wrap, putting
    /// the buffer's last visual row at the bottom of the viewport.
    pub max_scroll_logical_line: u32,
    pub wrap: WrapMode,
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
    /// The most recent repeatable motion, replayed by `r` (cursor move) or `Shift-r` (cursor
    /// move + extend selection).
    pub last_motion: Option<Motion>,
    pub search: SearchState,
    /// Canonical absolute path of this buffer's file on disk, if any.
    pub file_path: Option<String>,
    pub file_label: String,
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

    pub fn dirty(&self) -> bool {
        self.ed().revision != self.ed().saved_revision
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
    let viewport_cols = cols as u32;

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
                        anyhow::anyhow!(
                            "{} is outside the project's roots",
                            abs.display()
                        )
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
        status: String::new(),
        clipboard: clipboard::new_handle(),
        pending_leader: None,
        picker: crate::picker::PickerState::default(),
        save_prompt: None,
        confirm_prompt: None,
        editor: Some(editor),
        project_settings: None,
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
        status: String::new(),
        clipboard: clipboard::new_handle(),
        pending_leader: None,
        picker: crate::picker::PickerState::default(),
        save_prompt: None,
        confirm_prompt: None,
        editor: None,
        project_settings: None,
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
        state.status = format!("already in project {project_name}");
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
    state.status = format!("activated project {}", state.project_name);
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
        None => format!("[scratch {}]", open.buffer_id),
    };
    Ok(EditorState {
        mode: EditorMode::Normal,
        buffer_id: open.buffer_id,
        viewport_id: sub.viewport_id,
        cursor: open.cursor,
        scroll_logical_line: initial_scroll.logical_line,
        window_first_logical_line: sub.window.first_logical_line,
        lines: sub.window.lines,
        line_count: sub.window.line_count,
        max_scroll_logical_line: sub.window.max_scroll_logical_line,
        wrap,
        scroll_col: 0,
        pending_scroll_lines: 0,
        drag_anchor: None,
        revision: open.revision,
        saved_revision: open.saved_revision,
        externally_modified: false,
        externally_deleted: false,
        pending_count: 0,
        pending_find: None,
        last_motion: None,
        search: SearchState::default(),
        file_path: open.path,
        file_label,
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
        flush_pending_scroll(client, state).await?;
        flush_pending_picker_scroll(client, state).await?;
        terminal.draw(|f| ui::draw(f, state))?;
    }
    Ok(())
}

async fn dispatch_terminal_event(
    client: &mut Client,
    state: &mut AppState,
    ev: Event,
) -> Result<()> {
    // Each user-driven event clears the ephemeral status line before being processed. Anything
    // the event itself sets (save/copy feedback, search truncation, etc.) stays visible until
    // the *next* event.
    state.status.clear();
    if let Event::Resize(cols, rows) = &ev {
        handle_resize(client, state, *cols, *rows).await
    } else {
        handle_event(client, state, ev).await
    }
}

fn apply_cursor_style(state: &AppState) {
    // Overlays always use the bar cursor (they're text-prompt UIs). With no editor and no
    // overlay, fall back to the bar — there's nothing for the block cursor to sit on.
    let style = if state.picker.open
        || state.save_prompt.is_some()
        || state.confirm_prompt.is_some()
        || state.project_settings.is_some()
        || !state.has_editor()
    {
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

fn apply_notification(state: &mut AppState, n: aether_protocol::envelope::Notification) {
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
            Err(e) => state.status = format!("bad notif params: {e}"),
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
                    state.status = "file removed on disk — save to recreate, or close buffer".into();
                } else if p.externally_modified {
                    state.status = "file changed on disk — Ctrl-s to overwrite, or reload".into();
                } else if !was_synced && ed.revision == ed.saved_revision {
                    state.status = format!("saved (rev {})", ed.saved_revision);
                }
            }
            Ok(_) => {}
            Err(e) => state.status = format!("bad buffer/state params: {e}"),
        }
    } else if n.method == SearchStateChanged::NAME {
        match serde_json::from_value::<SearchSummary>(n.params) {
            Ok(s) if state.ed_mut().buffer_id == s.buffer_id => {
                state.ed_mut().search.summary = Some(s);
            }
            Ok(_) => {}
            Err(e) => state.status = format!("bad search/state_changed params: {e}"),
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
            Err(e) => state.status = format!("bad picker/update params: {e}"),
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
    if let Event::Key(k) = &ev {
        if k.kind == KeyEventKind::Press || k.kind == KeyEventKind::Repeat {
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
            // Pending leader chord (e.g. `Space f`): the next key resolves the binding.
            if let Some(leader) = state.pending_leader.take() {
                return handle_leader_key(client, state, leader, k).await;
            }
            // Overlays first — they sit on top of whichever screen is underneath. The
            // confirm prompt takes priority over everything else (it can layer on top of the
            // save prompt for the overwrite case).
            if state.confirm_prompt.is_some() {
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
    if state.ed_mut().cursor.position != cursor_before {
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

const SHIFT_ONLY: KeyModifiers = KeyModifiers::SHIFT;
const ALT_ONLY: KeyModifiers = KeyModifiers::ALT;
const CTRL_ONLY: KeyModifiers = KeyModifiers::CONTROL;

async fn handle_normal_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    // Pending `f`/`t`: the next keystroke names the target character. Use the raw key (skipping
    // `normalize_key`) so `f X` is case-sensitive. Any non-`Char` key (Esc, arrow, etc.) cancels.
    if let Some(pending) = state.ed_mut().pending_find.take() {
        if let KeyCode::Char(ch) = k.code {
            move_motion(
                client,
                state,
                Motion::FindChar {
                    ch,
                    direction: pending.direction,
                    count: pending.count,
                    till: pending.till,
                },
                pending.extend,
            )
            .await?;
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

    // Ctrl-modified shortcuts that Normal and Insert share live in `handle_ctrl_binding`.
    // Mode-specific divergences (e.g. clipboard scope) are handled inside that dispatcher's
    // per-binding wrappers.
    if handle_ctrl_binding(client, state, code, mods, count).await? {
        return Ok(());
    }

    match (code, mods) {
        // ---- meta ----
        (KeyCode::Esc, _) => {
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
        // `c` collapses any multi-char selection to a 1-char point at the cursor. No-op if
        // already a point. Visually unchanged: the block cursor stays where it was.
        (KeyCode::Char('c'), m) if m == KeyModifiers::NONE => {
            if !state.ed_mut().cursor.is_point() {
                clear_selection(client, state).await?;
            }
        }

        // ---- non-letter motions and scroll ----
        // Home/End map to logical-line start/end; arrows scroll the viewport without moving the
        // cursor (so the cursor can drift off-screen until a motion snaps it back). Alt-arrow
        // scrolls by a half-viewport. PageUp/Down are full-viewport scrolls.
        (KeyCode::Home, _) => move_motion(client, state, Motion::LineStart, extend).await?,
        (KeyCode::End, _) => move_motion(client, state, Motion::LineEnd, extend).await?,
        (KeyCode::PageDown, _) => scroll_lines(state, state.viewport_rows as i64),
        (KeyCode::PageUp, _) => scroll_lines(state, -(state.viewport_rows as i64)),
        (KeyCode::Up, m) if m.contains(KeyModifiers::ALT) => {
            scroll_lines(state, -((state.viewport_rows / 2) as i64))
        }
        (KeyCode::Down, m) if m.contains(KeyModifiers::ALT) => {
            scroll_lines(state, (state.viewport_rows / 2) as i64)
        }
        (KeyCode::Up, _) => scroll_lines(state, -1),
        (KeyCode::Down, _) => scroll_lines(state, 1),
        (KeyCode::Left, m) if m.contains(KeyModifiers::ALT) => {
            scroll_cols(state, -((state.viewport_cols / 2) as i64))
        }
        (KeyCode::Right, m) if m.contains(KeyModifiers::ALT) => {
            scroll_cols(state, (state.viewport_cols / 2) as i64)
        }
        (KeyCode::Left, _) => scroll_cols(state, -1),
        (KeyCode::Right, _) => scroll_cols(state, 1),

        // ---- motions: hjkl (logical) and Alt-hjkl (line jumps + visual rows) ----
        // `h/l` move by char; `Alt-h/l` jump to the first non-whitespace / end of the logical
        // line. `j/k` move by logical line; `Alt-j/k` move by one visual row (the only "visual"
        // motion now — used to step inside wrapped content). `0` (below) goes to literal col 0
        // for cases where you want column zero, not first non-blank.
        (KeyCode::Char('h'), m) if m.contains(KeyModifiers::ALT) => {
            move_motion(client, state, Motion::LineFirstNonblank, extend).await?
        }
        (KeyCode::Char('h'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            move_motion(
                client,
                state,
                Motion::Char {
                    direction: Direction::Backward,
                    count,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('l'), m) if m.contains(KeyModifiers::ALT) => {
            move_motion(client, state, Motion::LineEnd, extend).await?
        }
        (KeyCode::Char('l'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            move_motion(
                client,
                state,
                Motion::Char {
                    direction: Direction::Forward,
                    count,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('k'), m) if m.contains(KeyModifiers::ALT) => {
            let viewport_id = state.ed_mut().viewport_id;
            move_motion(
                client,
                state,
                Motion::VisualLine {
                    viewport_id,
                    direction: VerticalDirection::Up,
                    count,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('k'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            move_motion(
                client,
                state,
                Motion::LogicalLine {
                    direction: Direction::Backward,
                    count,
                    preserve_col: true,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('j'), m) if m.contains(KeyModifiers::ALT) => {
            let viewport_id = state.ed_mut().viewport_id;
            move_motion(
                client,
                state,
                Motion::VisualLine {
                    viewport_id,
                    direction: VerticalDirection::Down,
                    count,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('j'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            move_motion(
                client,
                state,
                Motion::LogicalLine {
                    direction: Direction::Forward,
                    count,
                    preserve_col: true,
                },
                extend,
            )
            .await?
        }

        // ---- motions: page / half-page ----
        // `u`/`d` move the cursor by a full viewport's worth of visual rows; `Alt-u`/`Alt-d` move
        // by half. Measured in visual rows so wrapped content steps consistently. Count prefix
        // multiplies (e.g. `3d` = three pages). The viewport follows the cursor on re-render.
        (KeyCode::Char('d'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            let viewport_id = state.ed_mut().viewport_id;
            let lines = count.saturating_mul(state.viewport_rows.max(1));
            move_motion(
                client,
                state,
                Motion::VisualLine {
                    viewport_id,
                    direction: VerticalDirection::Down,
                    count: lines,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('u'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            let viewport_id = state.ed_mut().viewport_id;
            let lines = count.saturating_mul(state.viewport_rows.max(1));
            move_motion(
                client,
                state,
                Motion::VisualLine {
                    viewport_id,
                    direction: VerticalDirection::Up,
                    count: lines,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('d'), m) if m.contains(KeyModifiers::ALT) => {
            let viewport_id = state.ed_mut().viewport_id;
            let lines = count.saturating_mul((state.viewport_rows / 2).max(1));
            move_motion(
                client,
                state,
                Motion::VisualLine {
                    viewport_id,
                    direction: VerticalDirection::Down,
                    count: lines,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('u'), m) if m.contains(KeyModifiers::ALT) => {
            let viewport_id = state.ed_mut().viewport_id;
            let lines = count.saturating_mul((state.viewport_rows / 2).max(1));
            move_motion(
                client,
                state,
                Motion::VisualLine {
                    viewport_id,
                    direction: VerticalDirection::Up,
                    count: lines,
                },
                extend,
            )
            .await?
        }

        // ---- motions: WORD (w/b/e) and Alt for word ----
        // Plain `w/b/e` use big WORDs (whitespace-delimited); `Alt-w/b/e` use small words
        // (alphanumeric/symbol category transitions). Forward `w` is exclusive when extending —
        // Shift-w selects up to (but not including) the start of the next WORD, matching the
        // vim/helix convention that operator-style selections don't bleed into the next word.
        (KeyCode::Char('w'), m) if m.contains(KeyModifiers::ALT) => {
            move_motion(
                client,
                state,
                Motion::Word {
                    direction: Direction::Forward,
                    count,
                    boundary: WordBoundary::Word,
                    exclusive: extend,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('w'), m) if !m.contains(KeyModifiers::CONTROL) => {
            move_motion(
                client,
                state,
                Motion::Word {
                    direction: Direction::Forward,
                    count,
                    boundary: WordBoundary::BigWord,
                    exclusive: extend,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('b'), m) if m.contains(KeyModifiers::ALT) => {
            move_motion(
                client,
                state,
                Motion::Word {
                    direction: Direction::Backward,
                    count,
                    boundary: WordBoundary::Word,
                    exclusive: false,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('b'), m) if !m.contains(KeyModifiers::CONTROL) => {
            move_motion(
                client,
                state,
                Motion::Word {
                    direction: Direction::Backward,
                    count,
                    boundary: WordBoundary::BigWord,
                    exclusive: false,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('e'), m) if m.contains(KeyModifiers::ALT) => {
            move_motion(
                client,
                state,
                Motion::WordEnd {
                    direction: Direction::Forward,
                    count,
                    boundary: WordBoundary::Word,
                },
                extend,
            )
            .await?
        }
        (KeyCode::Char('e'), _) => {
            move_motion(
                client,
                state,
                Motion::WordEnd {
                    direction: Direction::Forward,
                    count,
                    boundary: WordBoundary::BigWord,
                },
                extend,
            )
            .await?
        }

        // ---- motions: line start ----
        (KeyCode::Char('0'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            move_motion(client, state, Motion::LineStart, extend).await?
        }

        // ---- motions: find char (`f`/`t` + Alt for backward, Shift to extend) ----
        // After pressing one of these, the *next* keystroke is interpreted as the target
        // character (see the `pending_find` block at the top of this handler).
        (KeyCode::Char('f'), m) if m.contains(KeyModifiers::ALT) => {
            state.ed_mut().pending_find = Some(PendingFind {
                direction: Direction::Backward,
                till: false,
                extend,
                count,
            })
        }
        (KeyCode::Char('f'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            state.ed_mut().pending_find = Some(PendingFind {
                direction: Direction::Forward,
                till: false,
                extend,
                count,
            })
        }
        (KeyCode::Char('t'), m) if m.contains(KeyModifiers::ALT) => {
            state.ed_mut().pending_find = Some(PendingFind {
                direction: Direction::Backward,
                till: true,
                extend,
                count,
            })
        }
        (KeyCode::Char('t'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            state.ed_mut().pending_find = Some(PendingFind {
                direction: Direction::Forward,
                till: true,
                extend,
                count,
            })
        }

        // ---- motion: matching bracket ----
        // `m` jumps to the bracket that matches the one under (or enclosing) the cursor.
        // `Shift-m` does the same with `extend=true`, producing a selection from the original
        // position to the match — a natural "select around brackets" gesture (Vim's `v%`).
        // `Alt-m` is the "inner" counterpart: it jumps one char inside the matching bracket
        // and toggles sides on repeat, so `Alt-m Shift-Alt-m` selects everything *inside*
        // the bracket pair (excluding the brackets themselves).
        (KeyCode::Char('m'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            move_motion(client, state, Motion::MatchBracket { inner: false }, extend).await?
        }
        (KeyCode::Char('m'), m) if m == ALT_ONLY || m == (ALT_ONLY | SHIFT_ONLY) => {
            move_motion(client, state, Motion::MatchBracket { inner: true }, extend).await?
        }

        // ---- motions: navigation units ----
        // `]` / `[` *navigate between* per-language navigation units (function, struct, HTML
        // element, CSS rule set, etc. — see `LanguageConfig::navigation_kinds`). The cursor's
        // position implicitly determines the level: inside a method the next hit is the next
        // method in the same class; on the class header the next hit is the next top-level
        // item. Scope boundaries are *not* crossed.
        // `}` / `{` *jump to the end / start* of the enclosing navigation unit, extending the
        // selection. Use `}` on a function header to select the whole function; use `{` to
        // select from the cursor back to the function's start. Unlike `]`/`[` they don't have
        // a "next" hop, so they work on the last unit in a container too.
        (KeyCode::Char(']'), m) if m == KeyModifiers::NONE => {
            move_motion(client, state, Motion::NextNavigationUnit, false).await?
        }
        (KeyCode::Char('['), m) if m == KeyModifiers::NONE => {
            move_motion(client, state, Motion::PrevNavigationUnit, false).await?
        }
        (KeyCode::Char('}'), _) => {
            move_motion(client, state, Motion::EndOfNavigationUnit, true).await?
        }
        (KeyCode::Char('{'), _) => {
            move_motion(client, state, Motion::StartOfNavigationUnit, true).await?
        }

        // ---- motions: goto line ----
        // `g` jumps to line N (1-indexed; no prefix = line 1). `Alt-g` jumps to the last line.
        // Shift extends the selection. The server clamps line numbers past EOF.
        (KeyCode::Char('g'), m) if m.contains(KeyModifiers::ALT) => {
            let target = LogicalPosition {
                line: state.ed_mut().line_count.saturating_sub(1),
                col: 0,
            };
            move_motion(client, state, Motion::Goto { position: target }, extend).await?
        }
        (KeyCode::Char('g'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            let target = LogicalPosition {
                line: count.saturating_sub(1),
                col: 0,
            };
            move_motion(client, state, Motion::Goto { position: target }, extend).await?
        }

        // ---- line selection ----
        // `x` always grows the selection's bottom edge downward; `Alt-x` always grows the top
        // edge upward. With no selection: `x` picks the current line (or the next at end-of-line)
        // and `Alt-x` picks the previous (or the current at end-of-line). The `Shift` variants
        // keep the other edge in place (extending); the non-shift variants collapse onto a single
        // line at the moved edge. The cursor stays on whichever end (top/bottom) it was on, so
        // the bindings behave the same after `o` flips the selection direction.
        (KeyCode::Char('x'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            select_line(client, state, Direction::Forward, extend, count).await?
        }
        (KeyCode::Char('x'), m) if m.contains(KeyModifiers::ALT) => {
            select_line(client, state, Direction::Backward, extend, count).await?
        }

        // ---- selection manipulation ----
        // `o` swaps the cursor and anchor — flips which end of the selection is the "leading"
        // edge, so a subsequent `Shift-*` motion extends from the other side.
        (KeyCode::Char('o'), m) if m == KeyModifiers::NONE => swap_anchor(client, state).await?,

        // Tree-sitter selection expansion / contraction. `y` grows the selection to the smallest
        // enclosing syntax node; `Alt-y` reverses one step. With `N` prefix, applied N times.
        (KeyCode::Char('y'), m) if m == KeyModifiers::NONE => {
            tree_expand(client, state, count).await?
        }
        (KeyCode::Char('y'), m) if m == ALT_ONLY => {
            tree_contract(client, state, count).await?
        }

        // Motion undo / redo — per-client history of cursor/selection changes, capped at the
        // last buffer mutation. Distinct from `Ctrl-z`/`Ctrl-Alt-z` which rewind buffer edits.
        (KeyCode::Char('z'), m) if m == ALT_ONLY => motion_redo(client, state, count).await?,
        (KeyCode::Char('z'), m) if m == KeyModifiers::NONE => {
            motion_undo(client, state, count).await?
        }

        // Repeat the last *repeatable* motion (see `is_repeatable_motion`). `r` runs it as a
        // plain cursor move; `Shift-r` runs it extending the current selection. `Nr` loops the
        // motion N times — so e.g. after `f x`, `5r` jumps to the 5th next `x`.
        (KeyCode::Char('r'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            if let Some(motion) = state.ed_mut().last_motion.clone() {
                for _ in 0..count.max(1) {
                    move_motion(client, state, motion.clone(), extend).await?;
                }
            }
        }

        // ---- mode transitions ----
        (KeyCode::Char('i'), m) if m == KeyModifiers::NONE => {
            enter_insert_at(client, state, InsertWhere::SelectionStart).await?
        }
        (KeyCode::Char('a'), m) if m == KeyModifiers::NONE => {
            enter_insert_at(client, state, InsertWhere::SelectionEnd).await?
        }
        (KeyCode::Char('i'), m) if m == ALT_ONLY => {
            enter_insert_at(client, state, InsertWhere::FirstLineStart).await?
        }
        (KeyCode::Char('a'), m) if m == ALT_ONLY => {
            enter_insert_at(client, state, InsertWhere::LastLineEnd).await?
        }

        // ---- viewport ----
        // `Ctrl-p` (toggle wrap) goes through the shared Ctrl handler below; only the
        // non-Ctrl viewport bindings (centre-cursor) live here.
        (KeyCode::Char('-'), m) if m == KeyModifiers::NONE => center_cursor(client, state).await?,

        // ---- delete (also bound to Delete key) ----
        // The plain `Delete` key shares semantics with `Ctrl-d` in Normal mode (delete-
        // selection, repeated `count` times). `Ctrl-d` itself is in `handle_ctrl_binding`.
        (KeyCode::Delete, _) => handle_delete(client, state, count).await?,

        // ---- leader (Space) ----
        // `Space` starts a multi-key chord; the next keystroke selects the action. See
        // `handle_leader_key`.
        (KeyCode::Char(' '), m) if m == KeyModifiers::NONE => {
            state.pending_leader = Some(PendingLeader::Space)
        }

        // ---- search ----
        (KeyCode::Char('/'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            enter_search_mode(client, state).await?
        }
        (KeyCode::Char('/'), m) if m == ALT_ONLY => search_from_selection(client, state).await?,
        (KeyCode::Char('n'), m) if m.contains(KeyModifiers::ALT) => {
            search_cycle(client, state, Direction::Backward, count).await?
        }
        (KeyCode::Char('n'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            search_cycle(client, state, Direction::Forward, count).await?
        }

        // ---- grep navigation ----
        // `>` / `<` step through the last grep query's hits without re-opening the picker. The
        // server resolves the next/previous hit against the cursor's position (within-file
        // first, falling through to the next/previous file in path order). Silently no-op when
        // there are no cached hits or we're past the list ends.
        (KeyCode::Char('>'), _) => grep_navigate(client, state, Direction::Forward).await?,
        (KeyCode::Char('<'), _) => grep_navigate(client, state, Direction::Backward).await?,

        _ => {}
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
    let (code, mods) = normalize_key(k);
    let alt_only: KeyModifiers = KeyModifiers::ALT;
    // Bindings that act on the current buffer (or the project's file index) need both an active
    // editor and an active project. Without those, fall through to the chord-cancel branch — the
    // pre-activation event loop only surfaces Space p / Space q to the user. We could also error,
    // but silently dropping matches the existing "Esc cancels" cancellation behaviour for chords
    // the user composed and changed their mind on.
    let needs_editor = matches!(
        (leader, code, mods),
        (PendingLeader::Space, KeyCode::Char('f'), m) if m == KeyModifiers::NONE
    ) || matches!(
        (leader, code, mods),
        (PendingLeader::Space, KeyCode::Char('b'), m) if m == KeyModifiers::NONE
    ) || matches!(
        (leader, code, mods),
        (PendingLeader::Space, KeyCode::Char('g'), m) if m == KeyModifiers::NONE
    ) || matches!(
        (leader, code, mods),
        (PendingLeader::Space, KeyCode::Char('e'), m) if m == KeyModifiers::NONE
    ) || matches!(
        (leader, code, mods),
        (PendingLeader::Space, KeyCode::Char('w'), m) if m == KeyModifiers::NONE
    ) || matches!(
        (leader, code, mods),
        (PendingLeader::Space, KeyCode::Char('s'), _)
    ) || matches!(
        (leader, code, mods),
        (PendingLeader::Space, KeyCode::Char('r'), m) if m == KeyModifiers::NONE
    ) || matches!(
        (leader, code, mods),
        (PendingLeader::Space, KeyCode::Char('n'), _)
    );
    if needs_editor && !state.has_editor() {
        return Ok(());
    }
    match (leader, code, mods) {
        // `Space f` — open the file picker. Resumes the prior query + highlight + scroll
        // position; first-ever open is empty.
        (PendingLeader::Space, KeyCode::Char('f'), m) if m == KeyModifiers::NONE => {
            open_picker(client, state, PickerKind::Files).await?;
        }
        // `Space b` — open the buffer picker. MRU-ordered with the current buffer at the top;
        // selecting it is a no-op switch. Useful for quickly cycling back to a recent buffer
        // without going through the file browser.
        (PendingLeader::Space, KeyCode::Char('b'), m) if m == KeyModifiers::NONE => {
            open_picker(client, state, PickerKind::Buffers).await?;
        }
        // `Space g` — open the grep picker. Pre-fills the input from the active buffer's search
        // query (so workspace-search continues the in-buffer search), falling back to the last
        // grep query, then empty. Same-query reopens hit the server-side cache and appear
        // instantly; a different query reruns the search.
        (PendingLeader::Space, KeyCode::Char('g'), m) if m == KeyModifiers::NONE => {
            open_picker(client, state, PickerKind::Grep).await?;
        }
        // `Space e` — open the filesystem explorer. Resumes the prior directory + highlight on
        // reopen; first-ever open lands in the parent of the current file (or the first project
        // root). Filter via the query input; `Alt-Backspace` clears the filter and (when the
        // filter is already empty) steps up to the parent directory; `Enter` opens the
        // highlighted file or descends into the highlighted directory.
        (PendingLeader::Space, KeyCode::Char('e'), m) if m == KeyModifiers::NONE => {
            open_picker(client, state, PickerKind::Explorer).await?;
        }
        // `Space p` — open the project picker overlay. Lists every project under
        // `$XDG_CONFIG_HOME/aether/projects/`; selecting one calls `project/activate` and
        // rebuilds the editor on that project.
        (PendingLeader::Space, KeyCode::Char('p'), m) if m == KeyModifiers::NONE => {
            open_picker(client, state, PickerKind::Projects).await?;
        }
        // `Space ,` — open the project settings overlay. Lists the active project's roots;
        // `a` adds, `d` removes. Only meaningful when a project is active.
        (PendingLeader::Space, KeyCode::Char(','), m) if m == KeyModifiers::NONE => {
            if !state.project_name.is_empty() {
                open_project_settings(state);
            }
        }
        // ---- app-level meta actions ----
        // These used to live under Ctrl-, but `Ctrl` is reserved for buffer-content edits.
        // Quit / close buffer / save / save-as / new file / new scratch all sit under `Space`.
        (PendingLeader::Space, KeyCode::Char('q'), m) if m == KeyModifiers::NONE => {
            state.should_quit = true;
        }
        (PendingLeader::Space, KeyCode::Char('w'), m) if m == KeyModifiers::NONE => {
            close_buffer(client, state).await?;
        }
        (PendingLeader::Space, KeyCode::Char('s'), m) if m == KeyModifiers::NONE => {
            save_buffer(client, state).await?;
        }
        (PendingLeader::Space, KeyCode::Char('s'), m) if m == alt_only => {
            begin_save_prompt(client, state).await?;
        }
        // `Space r` — reload the current buffer from disk. Discards local changes; used to
        // pick up an external modification (paired with the `[!]` indicator and the save
        // conflict prompt).
        (PendingLeader::Space, KeyCode::Char('r'), m) if m == KeyModifiers::NONE => {
            reload_buffer(client, state).await?;
        }
        // `Space n` — spawn a fresh scratch buffer. The path is chosen at save time via the
        // save-as prompt (which handles roots / parent dirs / mkdir -p), so we don't ask up
        // front. Replaces the older two-chord setup (`Space n` opening a new-file prompt and
        // `Space Alt-n` for scratch) — the prompt's pre-fill never really earned its keep over
        // "just save-as when you're ready".
        (PendingLeader::Space, KeyCode::Char('n'), m) if m == KeyModifiers::NONE => {
            new_scratch(client, state).await?;
        }
        // Esc or any other key cancels the chord without further action.
        _ => {}
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
    let view = client
        .rpc::<PickerView>(PickerViewParams {
            kind,
            reset: !kind.preserves_state(),
            offset: 0,
            limit,
            center_on: center_on.clone(),
            center_on_cursor_grep_hit,
            directory_path: explorer_path_for_view,
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
    state.picker.selected = 0;
    // Prefer the server-resolved centre item (set when `center_on_cursor_grep_hit` resolved)
    // so `apply_update` snaps the highlight to the same row the server framed.
    state.picker.resume_target = view.effective_center_on.clone().or(center_on);
    state.picker.resume_row_offset = resume_row_offset;
    state.picker.pending_offset = None;
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
/// file, or the first project root for scratch buffers.
fn default_explorer_dir(state: &AppState) -> Option<String> {
    if let Some(p) = state.ed().file_path.as_deref() {
        if let Some(parent) = std::path::Path::new(p).parent() {
            return Some(parent.display().to_string());
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
    // Keep query input case-sensitive (so smartcase works), so skip `normalize_key`.
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => hide_picker(client, state).await?,
        (KeyCode::Enter, _) => select_picker_item(client, state).await?,
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
        // `Alt-Backspace` — multi-step "back" inside the Explorer picker:
        //   1. Non-empty filter → clear the filter (preserving the highlight as a resume anchor).
        //   2. Filter empty + inside a subdirectory → step up to the parent directory.
        //   3. Filter empty + at the top of a root → switch to Roots mode (list all project roots).
        //   4. Filter empty + already in Roots mode → no-op (no further "back" to take).
        // We deliberately leave `resume_row_offset` as `None` on the clear path — a filtered
        // listing usually has the highlight near the top of the visible window, and pinning
        // that offset onto the larger unfiltered listing scrolls items off the top, making it
        // look like the filter is still active.
        (KeyCode::Backspace, m) if m == KeyModifiers::ALT => {
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
                    // At the top of a root (no parent inside the project) — escape into the
                    // Roots view. Single-root projects skip this: there's only one root, so
                    // there's nothing to pick between. Already-in-Roots case falls through
                    // (the `explorer_dir.is_none()` arm).
                    picker_enter_roots_mode(client, state).await?;
                }
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
        let _ = client
            .rpc::<PickerHide>(PickerHideParams { kind })
            .await;
        state.picker.open = false;
        create_project_and_open_settings(client, state, &name).await?;
        return Ok(());
    }
    // Synthetic "+ create" row in the Explorer picker: create a new file at the picker's
    // current directory using the typed name. Routes through `buffer/open { create_if_missing }`
    // so the server allocates a fresh buffer pre-bound to the not-yet-existing path; the user's
    // first save writes it to disk (with mkdir-p of any missing intermediate dirs).
    if kind == PickerKind::Explorer && state.picker.highlighted_is_synthetic_create() {
        let name = state.picker.query.text.trim().to_string();
        if name.is_empty() {
            return Ok(());
        }
        return create_file_in_explorer_dir(client, state, &name).await;
    }
    let Some(item) = state.picker.highlighted().cloned() else {
        return Ok(());
    };
    // Explorer + directory entry: Enter is "enter this directory" rather than "select" — same
    // semantics as Alt-l, kept here for muscle memory. File entries fall through to the normal
    // selection path (server returns `File { path }`, we open it).
    if kind == PickerKind::Explorer {
        if let aether_protocol::picker::PickerItem::DirEntry {
            name, is_dir: true, ..
        } = &item
        {
            let target = std::path::Path::new(state.picker.explorer_dir.as_deref().unwrap_or(""))
                .join(name)
                .display()
                .to_string();
            picker_navigate_to_dir(client, state, target, None).await?;
            return Ok(());
        }
        // Roots mode: Enter on a Root row navigates to that root's top. The client looks up the
        // absolute path from project_paths — the server stays out of presentation.
        if let aether_protocol::picker::PickerItem::Root { path_index, .. } = &item {
            if let Some(target) = state.project_paths.get(*path_index as usize).cloned() {
                picker_navigate_to_dir(client, state, target, None).await?;
            }
            return Ok(());
        }
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

/// Switch to an already-open buffer by id (no path lookup; works for scratch buffers too).
/// Subscribes a fresh viewport and restores per-buffer cursor + scroll from the server. No-op
/// in the sense that the buffer's contents and per-client state already exist server-side —
/// we're just rebinding the client to it.
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
    state.status = format!("closed {closed_label}");
    if let Some(next) = result.next_buffer_id {
        attach_buffer(client, state, next).await?;
    } else {
        new_scratch(client, state).await?;
    }
    Ok(())
}

/// Shared post-`buffer/open` plumbing for runtime buffer switches: build the new `EditorState`
/// (via the shared core, inheriting the previous editor's wrap), replace `state.editor`, and
/// ensure the cursor is in view. `attach_buffer` and `new_scratch` route through this.
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
    ensure_cursor_in_window(client, state).await?;
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
    std::fs::canonicalize(&joined)
        .with_context(|| format!("could not resolve {arg}"))
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
    apply_cursor_style(state);
    Ok(())
}

async fn handle_insert_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    let (code, mods) = normalize_key(k);
    // Try shared Ctrl shortcuts first; mode-specific divergences live inside the wrappers
    // (handle_copy / handle_cut / etc.). Count is hardcoded to 1 in Insert — no pending_count
    // accumulator here.
    if handle_ctrl_binding(client, state, code, mods, 1).await? {
        return Ok(());
    }
    match (code, mods) {
        (KeyCode::Esc, _) => leave_insert(state),
        // Insert-mode Backspace deletes the char before the cursor; Delete deletes the 1-char
        // point at the cursor (the always-have-selection invariant means the point IS the
        // char to delete).
        (KeyCode::Backspace, _) => backspace(client, state).await?,
        (KeyCode::Delete, _) => delete_selection(client, state).await?,
        (KeyCode::Enter, _) => newline_and_indent(client, state).await?,
        (KeyCode::Tab, _) => insert_text(client, state, "\t").await?,
        (KeyCode::Left, _) => {
            move_motion(
                client,
                state,
                Motion::Char {
                    direction: Direction::Backward,
                    count: 1,
                },
                false,
            )
            .await?
        }
        (KeyCode::Right, _) => {
            move_motion(
                client,
                state,
                Motion::Char {
                    direction: Direction::Forward,
                    count: 1,
                },
                false,
            )
            .await?
        }
        (KeyCode::Up, _) => {
            let viewport_id = state.ed_mut().viewport_id;
            move_motion(
                client,
                state,
                Motion::VisualLine {
                    viewport_id,
                    direction: VerticalDirection::Up,
                    count: 1,
                },
                false,
            )
            .await?
        }
        (KeyCode::Down, _) => {
            let viewport_id = state.ed_mut().viewport_id;
            move_motion(
                client,
                state,
                Motion::VisualLine {
                    viewport_id,
                    direction: VerticalDirection::Down,
                    count: 1,
                },
                false,
            )
            .await?
        }

        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            // `normalize_key` lowercased the char and synthesised SHIFT so the Ctrl-* bindings
            // above can match consistently. Reverse that for actual text insertion.
            let c = if m.contains(KeyModifiers::SHIFT) {
                c.to_ascii_uppercase()
            } else {
                c
            };
            insert_text(client, state, &c.to_string()).await?;
        }

        _ => {}
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
    subscribe_to_buffer(client, state, open).await
}

async fn handle_search_key(client: &mut Client, state: &mut AppState, k: KeyEvent) -> Result<()> {
    // Don't `normalize_key` here — that lowercases uppercase chars and synthesises SHIFT, which
    // is what Normal-mode keymaps want but would strip case from the literal search query.
    match (k.code, k.modifiers) {
        (KeyCode::Esc, _) => abort_search(client, state).await?,
        (KeyCode::Enter, _) => commit_search(state),
        (KeyCode::Up, _) => {
            history_up(state);
            run_incremental_search(client, state).await?;
        }
        (KeyCode::Down, _) => {
            history_down(state);
            run_incremental_search(client, state).await?;
        }
        (KeyCode::Left, _) => state.ed_mut().search.query.move_left(),
        (KeyCode::Right, _) => state.ed_mut().search.query.move_right(),
        (KeyCode::Backspace, _) => {
            state.ed_mut().search.query.backspace();
            state.ed_mut().search.history_cursor = None;
            run_incremental_search(client, state).await?;
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            state.ed_mut().search.query.insert_char(c);
            state.ed_mut().search.history_cursor = None;
            run_incremental_search(client, state).await?;
        }
        _ => {}
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
    let (buffer_id, query) = {
        let ed = state.ed_mut();
        (ed.buffer_id, ed.search.query.text.clone())
    };
    let result = client
        .rpc::<SearchSet>(SearchSetParams {
            buffer_id,
            query,
            anchor,
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
            state.status = "invalid regex".into();
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
    state.viewport_cols = cols as u32;
    state.viewport_rows = viewport_rows;
    let viewport_id = state.ed_mut().viewport_id;
    let r = client
        .rpc::<ViewportResize>(ViewportResizeParams {
            viewport_id,
            cols: cols as u32,
            rows: viewport_rows,
        })
        .await?;
    let ed = state.ed_mut();
    ed.window_first_logical_line = r.window.first_logical_line;
    ed.line_count = r.window.line_count;
    ed.max_scroll_logical_line = r.window.max_scroll_logical_line;
    ed.lines = r.window.lines;

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
            motion: motion.clone(),
            extend_selection: extend,
        })
        .await?;
    state.ed_mut().cursor = new;
    if is_repeatable_motion(&motion) {
        state.ed_mut().last_motion = Some(motion);
    }
    Ok(())
}

/// Motions worth remembering for `r`/`Shift-r` repeat: those where each press makes incremental
/// progress. Absolute positions (line endpoints, buffer endpoints, goto) are excluded because
/// repeating them is a no-op.
fn is_repeatable_motion(motion: &Motion) -> bool {
    match motion {
        Motion::Char { .. }
        | Motion::Word { .. }
        | Motion::WordEnd { .. }
        | Motion::LogicalLine { .. }
        | Motion::VisualLine { .. }
        | Motion::FindChar { .. }
        | Motion::NextNavigationUnit
        | Motion::PrevNavigationUnit => true,
        Motion::LineStart
        | Motion::LineEnd
        | Motion::LineFirstNonblank
        | Motion::BufferStart
        | Motion::BufferEnd
        | Motion::Goto { .. }
        | Motion::VisualLineStart { .. }
        | Motion::VisualLineEnd { .. }
        | Motion::MatchBracket { .. }
        | Motion::EndOfNavigationUnit
        | Motion::StartOfNavigationUnit => false,
    }
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
        state.status = format!("nothing to {label}");
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

enum InsertWhere {
    /// `i` — at the cursor (or at the lower end of the selection).
    SelectionStart,
    /// `a` — after the cursor (or at the upper end of the selection).
    SelectionEnd,
    /// `Alt-i` — column 0 of the first line of the selection (or the cursor's line).
    FirstLineStart,
    /// `Alt-a` — end of the last line of the selection (or the cursor's line).
    LastLineEnd,
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
            state.status = format!("clipboard read failed: {e}");
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

// ---- shared Ctrl-binding dispatch -------------------------------------------------------------
//
// `handle_ctrl_binding` covers every Ctrl-modified shortcut that Normal and Insert mode share.
// Mode-dependent commands (copy/cut/paste/change/delete/replace) get thin wrappers below that
// branch on `state.ed_mut().mode` to pick the right scope/behavior. This lets both mode
// handlers delegate to one dispatcher instead of carrying ~22 duplicated arms each.

/// Ctrl-y in Normal: copy selection. In Insert: copy current line.
async fn handle_copy(client: &mut Client, state: &mut AppState) -> Result<()> {
    let scope = scope_for_mode(state);
    copy_to_clipboard(client, state, scope).await
}

/// Ctrl-x in Normal: cut selection. In Insert: cut current line.
async fn handle_cut(client: &mut Client, state: &mut AppState) -> Result<()> {
    let scope = scope_for_mode(state);
    cut_to_clipboard(client, state, scope).await
}

/// Ctrl-v in Normal: paste-before (collapses to selection start + inserts with select_pasted).
/// In Insert: paste at cursor (no selection of the inserted text — the bar cursor sits past it
/// ready to keep typing).
async fn handle_paste(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    match state.ed_mut().mode {
        EditorMode::Insert => paste_at_cursor(client, state).await,
        _ => paste_before(client, state, count).await,
    }
}

/// Ctrl-c. In Normal: delete the selection and enter Insert. In Insert: blank the current line
/// (we're already in Insert).
async fn handle_change(client: &mut Client, state: &mut AppState) -> Result<()> {
    match state.ed_mut().mode {
        EditorMode::Insert => change_line(client, state).await,
        _ => change_selection(client, state).await,
    }
}

/// Ctrl-d. In Normal: delete the selection (looped `count` times). In Insert: delete the
/// current line (count ignored — Insert has no count accumulator).
async fn handle_delete(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    match state.ed_mut().mode {
        EditorMode::Insert => delete_line(client, state).await,
        _ => {
            for _ in 0..count.max(1) {
                delete_selection(client, state).await?;
            }
            Ok(())
        }
    }
}

/// Ctrl-r. In Normal: paste-replace selection (paste + select-pasted, looped). In Insert:
/// replace the current line with the clipboard.
async fn handle_replace_with_clipboard(
    client: &mut Client,
    state: &mut AppState,
    count: u32,
) -> Result<()> {
    match state.ed_mut().mode {
        EditorMode::Insert => replace_line_with_clipboard(client, state).await,
        _ => paste_replace(client, state, count).await,
    }
}

fn scope_for_mode(state: &AppState) -> CopyScope {
    match state.ed().mode {
        EditorMode::Insert => CopyScope::Line,
        _ => CopyScope::Selection,
    }
}

/// Dispatch every Ctrl-modified binding shared between Normal and Insert. Returns `Ok(true)`
/// when a binding matched (the caller short-circuits); `Ok(false)` when nothing matched and
/// the caller should try mode-specific bindings. `count` is `pending_count` (Normal) or `1`
/// (Insert).
async fn handle_ctrl_binding(
    client: &mut Client,
    state: &mut AppState,
    code: KeyCode,
    mods: KeyModifiers,
    count: u32,
) -> Result<bool> {
    let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
    // The Ctrl shortcuts here are limited to things that *change the buffer's contents*
    // (edits, clipboard, undo/redo, indent, etc.). App-level meta actions — quit, save,
    // close/open buffers — live under the `Space` leader instead (`handle_leader_key`).
    match (code, mods) {
        // ---- viewport ----
        (KeyCode::Char('p'), CTRL_ONLY) => toggle_wrap(client, state).await?,
        // ---- undo / redo ----
        (KeyCode::Char('z'), CTRL_ONLY) => undo(client, state, count).await?,
        (KeyCode::Char('z'), m) if m == ctrl_alt => redo(client, state, count).await?,
        // ---- line manipulation (count-taking in Normal; count=1 in Insert) ----
        (KeyCode::Char('j'), CTRL_ONLY) => {
            move_lines(client, state, VerticalDirection::Down, count).await?
        }
        (KeyCode::Char('k'), CTRL_ONLY) => {
            move_lines(client, state, VerticalDirection::Up, count).await?
        }
        (KeyCode::Char('g'), CTRL_ONLY) => join_lines(client, state, count).await?,
        (KeyCode::Char('l'), CTRL_ONLY) => indent(client, state, count).await?,
        (KeyCode::Char('h'), CTRL_ONLY) => dedent(client, state, count).await?,
        (KeyCode::Char('t'), CTRL_ONLY) => toggle_comment(client, state).await?,
        (KeyCode::Char('o'), CTRL_ONLY) => open_line_below(client, state).await?,
        (KeyCode::Char('o'), m) if m == ctrl_alt => open_line_above(client, state).await?,
        // ---- mode-dependent: clipboard, change, delete, replace ----
        (KeyCode::Char('y'), CTRL_ONLY) => handle_copy(client, state).await?,
        (KeyCode::Char('x'), CTRL_ONLY) => handle_cut(client, state).await?,
        (KeyCode::Char('v'), CTRL_ONLY) => handle_paste(client, state, count).await?,
        (KeyCode::Char('c'), CTRL_ONLY) => handle_change(client, state).await?,
        (KeyCode::Char('d'), CTRL_ONLY) => handle_delete(client, state, count).await?,
        (KeyCode::Char('r'), CTRL_ONLY) => {
            handle_replace_with_clipboard(client, state, count).await?
        }
        _ => return Ok(false),
    }
    Ok(true)
}

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
        Ok(()) => state.status = format!("copied {len} bytes"),
        Err(e) => state.status = format!("copy failed: {e}"),
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
        Ok(()) => state.status = format!("cut {len} bytes"),
        Err(e) => state.status = format!("cut to clipboard failed: {e}"),
    }
    Ok(())
}

/// Normal-mode paste: insert clipboard content *before* the selection's start and select the
/// pasted text. `count` repeats the clipboard contents, so `3p` pastes three copies in a row.
async fn paste_before(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    let text = match clipboard::paste(&mut state.clipboard) {
        Ok(t) => t,
        Err(e) => {
            state.status = format!("paste failed: {e}");
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
            state.status = format!("paste failed: {e}");
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
            state.status = format!("paste failed: {e}");
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
        state.status = format!("nothing to {label}");
        return;
    }
    state.ed_mut().revision = r.revision;
    state.ed_mut().cursor = r.cursor;
    state.status = format!("{label} (rev {})", r.revision);
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
        state.status = "scratch buffer has no path — use save-as".into();
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
            state.status = format!("saved (rev {})", r.revision);
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
            state.status = format!("save failed: {e}");
        }
    }
    Ok(())
}

async fn reload_buffer(client: &mut Client, state: &mut AppState) -> Result<()> {
    reload_buffer_with(client, state, false).await
}

/// Send `buffer/reload` with explicit `force`. `force: false` is the default `Space r` path;
/// `force: true` is the retry after a user-confirmed `WOULD_DISCARD_CHANGES`.
async fn reload_buffer_with(
    client: &mut Client,
    state: &mut AppState,
    force: bool,
) -> Result<()> {
    if state.ed_mut().file_path.is_none() {
        state.status = "scratch buffer has no path to reload".into();
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
            state.status = format!("reloaded (rev {})", r.revision);
        }
        Err(e) if is_would_discard_changes(&e) => {
            state.confirm_prompt = Some(ConfirmPrompt {
                message: "discard local changes and reload".into(),
                action: ConfirmAction::ReloadDiscardChanges,
            });
        }
        Err(e) => {
            state.status = format!("reload failed: {e}");
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
    state.status = format!("created project {}", state.project_name);
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
        state.status = format!("can't create file in {dir_abs}: outside the project");
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
    subscribe_to_buffer(client, state, open).await
}

// ---- project settings -------------------------------------------------------------------------

/// Hydrate the project-settings overlay from the currently-active project's roots and open it.
/// Cheap (just clones the roots vec); no RPC. Focus lands on the always-present input row at the
/// bottom — most overlay opens (especially the post-create flow) are to add a root, and this
/// avoids an extra keypress for that case.
fn open_project_settings(state: &mut AppState) {
    let roots = state.project_paths.clone();
    let selected = roots.len();
    state.project_settings = Some(ProjectSettingsState {
        project_name: state.project_name.clone(),
        roots,
        selected,
        add_input: crate::text_input::TextInput::default(),
        error: None,
        pending_delete: false,
    });
    apply_cursor_style(state);
}

/// Selection model: `selected ∈ 0..=roots.len()`, where `roots.len()` is the input row. Alt-j/k
/// move between fields (mirroring the picker's chord, so Alt-j/k means "navigate" everywhere in
/// the app). Left/Right stay free to move the caret inside the input. Delete or Ctrl-d on a root
/// row stages a remove (which `y`/Enter/Delete/Ctrl-d then confirm); Enter on the input row
/// commits the add. Esc always closes (or first cancels a pending delete).
async fn handle_project_settings_key(
    client: &mut Client,
    state: &mut AppState,
    k: KeyEvent,
) -> Result<()> {
    let code = k.code;
    let mods = k.modifiers;
    // Ctrl-d is accepted alongside the Delete key for both staging and confirming a removal —
    // easier to reach on keyboards where Delete is awkward (or absent on small layouts).
    let is_delete_chord = code == KeyCode::Delete
        || (code == KeyCode::Char('d') && mods == KeyModifiers::CONTROL);

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
            let Some(path) = s.roots.get(s.selected).cloned() else {
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

    if code == KeyCode::Esc {
        state.project_settings = None;
        apply_cursor_style(state);
        return Ok(());
    }

    let Some(settings) = state.project_settings.as_mut() else {
        return Ok(());
    };
    let on_input = settings.selected == settings.roots.len();

    // Alt-j / Alt-k navigation. Check before the input-row text-routing block so the chord
    // works whether the caret is in the input or on a root row.
    if mods == KeyModifiers::ALT {
        match code {
            KeyCode::Char('k') => {
                settings.selected = settings.selected.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Char('j') => {
                settings.selected = (settings.selected + 1).min(settings.roots.len());
                return Ok(());
            }
            _ => {}
        }
    }

    if is_delete_chord && !on_input {
        // Stage the confirm — actual removal happens in the pending-delete branch above.
        settings.pending_delete = true;
        settings.error = None;
        return Ok(());
    }

    match code {
        KeyCode::Enter if on_input => {
            commit_add_root(client, state).await?;
            return Ok(());
        }
        _ if on_input => {
            // Esc/Enter already handled above; everything else (chars, Backspace, Left/Right)
            // edits the input. apply_prompt_key returns Cancel/Commit only for Esc/Enter — which
            // we've intercepted — so we only ever see Edited here.
            let outcome = crate::text_input::apply_prompt_key(&mut settings.add_input, k);
            if let PromptKeyOutcome::Edited = outcome {
                settings.error = None;
            }
        }
        _ => {}
    }
    Ok(())
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
                s.selected = s.roots.len();
            }
            state.status = format!("added root to {project_name}");
        }
        Err(e) => {
            if let Some(s) = state.project_settings.as_mut() {
                s.error = Some(if let Some(rpc_err) = e.downcast_ref::<crate::client::RpcError>() {
                    rpc_err.message.clone()
                } else {
                    e.to_string()
                });
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
            state.status = if closed.is_empty() {
                format!("removed root from {project_name}")
            } else {
                format!(
                    "removed root from {project_name}; closed {} buffer(s)",
                    closed.len()
                )
            };
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
                state.status = msg;
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
            if settings.selected > settings.roots.len() {
                settings.selected = settings.roots.len();
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
            state.status = format!("saved as {} (rev {})", path, r.revision);
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
            state.status = format!("save failed: {e}");
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
        } else if col >= state.ed_mut().scroll_col.saturating_add(state.viewport_cols) {
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
    // Horizontal scroll is meaningless under soft wrap — content never overflows right.
    if matches!(new_wrap, WrapMode::Soft) {
        state.ed_mut().scroll_col = 0;
    }
    state.status = format!(
        "wrap: {}",
        match new_wrap {
            WrapMode::Soft => "on",
            WrapMode::None => "off",
        }
    );
    Ok(())
}

/// Accumulate a vertical-scroll delta. Doesn't touch the cursor and doesn't issue an RPC — the
/// actual `viewport/scroll` is sent when `flush_pending_scroll` runs (before the next draw, or
/// at the start of `ensure_cursor_in_window`). This lets a trackpad burst of N scroll events
/// collapse into one server round-trip.
fn scroll_lines(state: &mut AppState, delta: i64) {
    state.ed_mut().pending_scroll_lines = state.ed_mut().pending_scroll_lines.saturating_add(delta);
}

/// Apply any accumulated `pending_scroll_lines` to the server via one `viewport/scroll` call.
/// No-op if zero. Called before every draw and from inside `ensure_cursor_in_window` so the
/// cursor-visibility check sees the user's intended scroll position.
async fn flush_pending_scroll(client: &mut Client, state: &mut AppState) -> Result<()> {
    if !state.has_editor() {
        return Ok(());
    }
    let ed = state.ed_mut();
    if ed.pending_scroll_lines == 0 {
        return Ok(());
    }
    let delta = ed.pending_scroll_lines;
    ed.pending_scroll_lines = 0;
    let raw = if delta >= 0 {
        ed.scroll_logical_line.saturating_add(delta as u32)
    } else {
        ed.scroll_logical_line.saturating_sub((-delta) as u32)
    };
    // Server-computed: highest scroll position that still puts the buffer's last visual row at
    // the bottom of the viewport. Accounts for wrap (where one logical line can be multiple
    // visual rows).
    let target = raw.min(ed.max_scroll_logical_line);
    if target == ed.scroll_logical_line {
        return Ok(()); // no movement after clamping; skip the RPC
    }
    scroll_to(client, state, target).await
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
    state.ed_mut().window_first_logical_line = r.window.first_logical_line;
    state.ed_mut().line_count = r.window.line_count;
    state.ed_mut().max_scroll_logical_line = r.window.max_scroll_logical_line;
    state.ed_mut().lines = r.window.lines;
    Ok(())
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
}
