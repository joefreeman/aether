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
use aether_protocol::handshake::ClientHelloResult;
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
use anyhow::Result;
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
    pub project_name: String,
    pub project_paths: Vec<String>,
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
    /// Active "new file" prompt opened by `Space n`. Pre-filled with the current directory so
    /// the user just types the filename to append. Commits via `create_if_missing`, leaving the
    /// new buffer attached.
    pub new_file_prompt: Option<NewFilePromptState>,
    /// Active binary y/N confirmation prompt. Layers on top of any other overlay (including
    /// `save_prompt`, e.g. for the save-as overwrite confirm). Holds the question text and the
    /// action to run on `y`.
    pub confirm_prompt: Option<ConfirmPrompt>,
    pub editor: EditorState,
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
    /// Retry the save-as RPC with `overwrite: true`. The path is read from
    /// `state.save_prompt.input.text` (the save-prompt stays open beneath the confirm), so
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

#[derive(Debug, Clone)]
pub struct SavePromptState {
    pub input: crate::text_input::TextInput,
}

#[derive(Debug, Clone)]
pub struct NewFilePromptState {
    pub input: crate::text_input::TextInput,
    /// The project root the typed path is relative to. Captured at prompt-open from the current
    /// file's containing root (or first root, when nothing is open). On commit we send this to
    /// the server alongside the typed relative path.
    pub path_index: u32,
}

impl AppState {
    pub fn dirty(&self) -> bool {
        self.editor.revision != self.editor.saved_revision
    }
}

pub async fn bootstrap(
    client: &mut Client,
    token: String,
    file: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<AppState> {
    let viewport_rows = rows.saturating_sub(1) as u32;
    let viewport_cols = cols as u32;

    let hello: ClientHelloResult = client
        .rpc::<aether_protocol::handshake::ClientHello>(
            aether_protocol::handshake::ClientHelloParams {
                token,
                client_version: env!("CARGO_PKG_VERSION").into(),
            },
        )
        .await?;
    let project_paths = hello.project.paths.clone();

    // Classify the file arg: file → open it; directory → open scratch + auto-show the Explorer
    // popup pointed at that directory; missing → open scratch + auto-show the Explorer popup
    // at the first project root.
    let (open_file, explorer_dir): (Option<String>, Option<String>) = match file {
        None => (None, project_paths.first().cloned()),
        Some(f) => {
            let raw = std::path::Path::new(f);
            let abs = if raw.is_absolute() {
                raw.to_path_buf()
            } else {
                project_paths
                    .first()
                    .map(|root| std::path::Path::new(root).join(raw))
                    .unwrap_or_else(|| raw.to_path_buf())
            };
            if abs.is_dir() {
                (None, Some(abs.display().to_string()))
            } else {
                (Some(f.to_string()), None)
            }
        }
    };

    let editor = match open_file.as_deref() {
        Some(f) => {
            open_buffer_and_subscribe(
                client,
                viewport_cols,
                viewport_rows,
                &project_paths,
                aether_protocol::buffer::BufferOpenParams {
                    buffer_id: None,
                    path_index: Some(0),
                    relative_path: Some(f.into()),
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
                aether_protocol::buffer::BufferOpenParams {
                    buffer_id: None,
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
        project_name: hello.project.name,
        project_paths,
        viewport_cols,
        viewport_rows,
        should_quit: false,
        status: String::new(),
        clipboard: clipboard::new_handle(),
        pending_leader: None,
        picker: crate::picker::PickerState::default(),
        save_prompt: None,
        new_file_prompt: None,
        confirm_prompt: None,
        editor,
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

/// Construct an `EditorState` by running `buffer/open` followed by `viewport/subscribe`. Used
/// by bootstrap only — runtime buffer switches start from a pre-resolved `BufferOpenResult`
/// (held by the caller for status reporting) and go through `subscribe_to_buffer`.
async fn open_buffer_and_subscribe(
    client: &mut Client,
    viewport_cols: u32,
    viewport_rows: u32,
    project_paths: &[String],
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
        Some(p) => project_relative_label(p, project_paths),
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
    // Overlays always use the bar cursor (they're text-prompt UIs). Otherwise editor mode
    // decides: block in Normal, bar in Insert / Search.
    let style = if state.picker.open
        || state.save_prompt.is_some()
        || state.new_file_prompt.is_some()
        || state.confirm_prompt.is_some()
    {
        SetCursorStyle::SteadyBar
    } else {
        match state.editor.mode {
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
    // Editor-bound notifications: ignore unless the ids match.
    if n.method == ViewportLinesChanged::NAME {
        match serde_json::from_value::<ViewportLinesChangedParams>(n.params) {
            Ok(p) if state.editor.viewport_id == p.viewport_id => {
                splice_lines(state, p);
            }
            Ok(_) => {}
            Err(e) => state.status = format!("bad notif params: {e}"),
        }
    } else if n.method == BufferState::NAME {
        match serde_json::from_value::<BufferStateParams>(n.params) {
            Ok(p) if state.editor.buffer_id == p.buffer_id => {
                let ed = &mut state.editor;
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
            Ok(s) if state.editor.buffer_id == s.buffer_id => {
                state.editor.search.summary = Some(s);
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
    state.editor.revision = p.revision;
    state.editor.line_count = p.line_count;
    state.editor.max_scroll_logical_line = p.max_scroll_logical_line;
    let local_start =
        (p.range.start_logical_line as i64) - (state.editor.window_first_logical_line as i64);
    let local_end = (p.range.end_logical_line_exclusive as i64)
        - (state.editor.window_first_logical_line as i64);
    if local_end < 0 || local_start > state.editor.lines.len() as i64 {
        return;
    }
    let lo = local_start.max(0) as usize;
    let hi = (local_end as usize).min(state.editor.lines.len());
    let replacement_len = p.replacement_lines.len();
    state.editor.lines.splice(lo..hi, p.replacement_lines);
    // The server's notification covers the *current* (post-edit) viewport range. If the edit
    // shrank the buffer, the OLD `state.editor.lines` could extend past the new range — truncate any
    // stale tail so subsequent draws never read a line that no longer exists.
    state.editor.lines.truncate(lo + replacement_len);
}

async fn handle_event(client: &mut Client, state: &mut AppState, ev: Event) -> Result<()> {
    // Track whether the cursor moved during this event. Pure-scroll bindings leave it alone, so
    // the viewport stays where the user scrolled; any binding that actually moves the cursor
    // triggers `ensure_cursor_in_window` to snap the view back to it.
    let cursor_before = state.editor.cursor.position;
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
            } else if state.new_file_prompt.is_some() {
                handle_new_file_prompt_key(client, state, k).await?;
            } else if state.picker.open {
                handle_picker_key(client, state, k).await?;
            } else {
                match state.editor.mode {
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
    if state.editor.cursor.position != cursor_before {
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
                        buffer_id: state.editor.buffer_id,
                        position: pos,
                        anchor: pos,
                    })
                    .await?;
                state.editor.cursor = new;
                state.editor.drag_anchor = Some(new.position);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(anchor) = state.editor.drag_anchor {
                if let Some(pos) = ui::screen_to_logical(state, m.row, m.column) {
                    let new = client
                        .rpc::<CursorSet>(CursorSetParams {
                            buffer_id: state.editor.buffer_id,
                            position: pos,
                            anchor: anchor,
                        })
                        .await?;
                    state.editor.cursor = new;
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            state.editor.drag_anchor = None;
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
    if let Some(pending) = state.editor.pending_find.take() {
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
            let ed = &mut state.editor;
            ed.pending_count = ed
                .pending_count
                .saturating_mul(10)
                .saturating_add(c.to_digit(10).unwrap_or(0));
            return Ok(());
        }
    }
    if let KeyCode::Char('0') = code {
        if mods == KeyModifiers::NONE && state.editor.pending_count > 0 {
            state.editor.pending_count = state.editor.pending_count.saturating_mul(10);
            return Ok(());
        }
    }

    // Whatever this command consumes for `count`, reset after.
    let count = if state.editor.pending_count == 0 {
        1
    } else {
        state.editor.pending_count
    };
    state.editor.pending_count = 0;

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
            if state.editor.search.active || state.editor.search.summary.is_some() {
                let _ = client
                    .rpc::<SearchClear>(SearchClearParams {
                        buffer_id: state.editor.buffer_id,
                    })
                    .await;
            }
            state.editor.search.active = false;
            state.editor.search.summary = None;
        }
        // `c` collapses any multi-char selection to a 1-char point at the cursor. No-op if
        // already a point. Visually unchanged: the block cursor stays where it was.
        (KeyCode::Char('c'), m) if m == KeyModifiers::NONE => {
            if !state.editor.cursor.is_point() {
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
            let viewport_id = state.editor.viewport_id;
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
            let viewport_id = state.editor.viewport_id;
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
            let viewport_id = state.editor.viewport_id;
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
            let viewport_id = state.editor.viewport_id;
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
            let viewport_id = state.editor.viewport_id;
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
            let viewport_id = state.editor.viewport_id;
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
            state.editor.pending_find = Some(PendingFind {
                direction: Direction::Backward,
                till: false,
                extend,
                count,
            })
        }
        (KeyCode::Char('f'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            state.editor.pending_find = Some(PendingFind {
                direction: Direction::Forward,
                till: false,
                extend,
                count,
            })
        }
        (KeyCode::Char('t'), m) if m.contains(KeyModifiers::ALT) => {
            state.editor.pending_find = Some(PendingFind {
                direction: Direction::Backward,
                till: true,
                extend,
                count,
            })
        }
        (KeyCode::Char('t'), m) if m == KeyModifiers::NONE || m == SHIFT_ONLY => {
            state.editor.pending_find = Some(PendingFind {
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
                line: state.editor.line_count.saturating_sub(1),
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
            if let Some(motion) = state.editor.last_motion.clone() {
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
            buffer_id: state.editor.buffer_id,
        })
        .await?;
    let Some(target) = target else {
        return Ok(());
    };
    open_file_at_path(client, state, target.path, false, Some(target.position)).await?;
    if !target.query.is_empty() {
        let buffer_id = state.editor.buffer_id;
        let r = client
            .rpc::<SearchSet>(SearchSetParams {
                buffer_id,
                query: target.query.clone(),
                anchor: Some(target.position),
            })
            .await?;
        let ed = &mut state.editor;
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
            begin_save_prompt(state);
        }
        // `Space r` — reload the current buffer from disk. Discards local changes; used to
        // pick up an external modification (paired with the `[!]` indicator and the save
        // conflict prompt).
        (PendingLeader::Space, KeyCode::Char('r'), m) if m == KeyModifiers::NONE => {
            reload_buffer(client, state).await?;
        }
        // `Space n` — open a "new file" prompt pre-filled with the current directory. Same
        // current-directory rule as `-` (file browser entry): parent of the current file, or
        // the first project root when there's no current file.
        (PendingLeader::Space, KeyCode::Char('n'), m) if m == KeyModifiers::NONE => {
            begin_new_file_prompt(state);
        }
        // `Space Alt-n` — fresh scratch buffer.
        (PendingLeader::Space, KeyCode::Char('n'), m) if m == alt_only => {
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
    // alone only covers the strict-on-a-match case.
    let center_on_cursor_grep_hit = (kind == PickerKind::Grep).then_some(state.editor.buffer_id);
    let view = client
        .rpc::<PickerView>(PickerViewParams {
            kind,
            reset: !kind.preserves_state(),
            offset: 0,
            limit,
            center_on: center_on.clone(),
            center_on_cursor_grep_hit,
            directory_path: explorer_path_for_view,
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
    let ed = &state.editor;
    if !ed.search.active || ed.search.query.is_empty() {
        return None;
    }
    Some(ed.search.query.text.clone())
}

/// Initial directory for a freshly-opened Explorer picker: parent of the active buffer's
/// file, or the first project root for scratch buffers.
fn default_explorer_dir(state: &AppState) -> Option<String> {
    if let Some(p) = state.editor.file_path.as_deref() {
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
    let p = state.editor.file_path.as_deref()?;
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
        // `Alt-Backspace` — two-step "back". With text in the filter it wipes the filter
        // (preserving the highlighted item as a resume anchor so the cursor stays on it once
        // the broader empty-query listing re-pushes); with the filter empty *and* the picker
        // in Explorer mode, it steps up to the parent directory (no-op at the project root).
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
                let buffer_id = state.editor.buffer_id;
                let r = client
                    .rpc::<SearchSet>(SearchSetParams {
                        buffer_id,
                        query: query.clone(),
                        anchor: Some(position),
                    })
                    .await?;
                let ed = &mut state.editor;
                ed.cursor = r.cursor;
                ed.search.summary = Some(r.summary);
                ed.search.query.set(query.clone());
                ed.search.active = true;
                push_history(state, query);
            }
        }
    }
    // Whatever the selection did (file open / buffer switch), we land in Normal mode.
    state.editor.mode = EditorMode::Normal;
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
    if state.editor.buffer_id == buffer_id {
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
    let ed = &state.editor;
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
    let closed_label = if state.editor.buffer_id == buffer_id {
        state.editor.file_label.clone()
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
    let wrap = state.editor.wrap;
    state.editor = build_editor_state_from_open(
        client,
        state.viewport_cols,
        state.viewport_rows,
        &state.project_paths,
        open,
        wrap,
    )
    .await?;
    apply_cursor_style(state);
    // Cover the case where the restored scroll disagrees with the cursor (e.g. a `jump_to`
    // override on a buffer we've already opened before, so the stored scroll wasn't computed
    // around the new cursor). Cheap when the cursor is already visible — no RPC.
    ensure_cursor_in_window(client, state).await?;
    Ok(())
}

/// Strip the longest matching project path off `abs`, or fall back to the raw absolute path.
/// Mirrors the server's display rule for buffer-picker items.
fn project_relative_label(abs: &str, project_paths: &[String]) -> String {
    let abs_path = std::path::Path::new(abs);
    let best = project_paths
        .iter()
        .filter_map(|p| {
            let root = std::path::Path::new(p);
            abs_path.strip_prefix(root).ok().map(|rel| (root, rel))
        })
        .max_by_key(|(root, _)| root.as_os_str().len());
    match best {
        Some((_, rel)) => rel.display().to_string(),
        None => abs.to_string(),
    }
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
            let viewport_id = state.editor.viewport_id;
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
            let viewport_id = state.editor.viewport_id;
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
        (KeyCode::Left, _) => state.editor.search.query.move_left(),
        (KeyCode::Right, _) => state.editor.search.query.move_right(),
        (KeyCode::Backspace, _) => {
            state.editor.search.query.backspace();
            state.editor.search.history_cursor = None;
            run_incremental_search(client, state).await?;
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            state.editor.search.query.insert_char(c);
            state.editor.search.history_cursor = None;
            run_incremental_search(client, state).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn enter_search_mode(client: &mut Client, state: &mut AppState) -> Result<()> {
    state.editor.search.snapshot = Some(SearchSnapshot {
        cursor: state.editor.cursor,
        scroll_logical_line: state.editor.scroll_logical_line,
        query: state.editor.search.query.take_text(),
        active: state.editor.search.active,
    });
    state.editor.search.active = false;
    state.editor.search.summary = None;
    {
        let ed = &mut state.editor;
        ed.search.history_cursor = None;
        ed.search.history_draft.clear();
        ed.mode = EditorMode::Search;
    }
    apply_cursor_style(state);
    // Clear the server-side search so highlights disappear immediately. Restored on Esc.
    let buffer_id = state.editor.buffer_id;
    let _ = client
        .rpc::<SearchClear>(SearchClearParams { buffer_id })
        .await;
    Ok(())
}

fn commit_search(state: &mut AppState) {
    let committed_query = {
        let ed = &mut state.editor;
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
    let ed = &mut state.editor;
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
    let ed = &mut state.editor;
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
    let ed = &mut state.editor;
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
    let ed = &mut state.editor;
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
    let Some(snap) = state.editor.search.snapshot.take() else {
        state.editor.mode = EditorMode::Normal;
        apply_cursor_style(state);
        return Ok(());
    };
    // Restore the prior server-side search query (if any). Done before cursor restoration so the
    // server's view of "current_index" matches once we move the cursor back.
    if snap.active && !snap.query.is_empty() {
        let r = client
            .rpc::<SearchSet>(SearchSetParams {
                buffer_id: state.editor.buffer_id,
                query: snap.query.clone(),
                anchor: None,
            })
            .await?;
        state.editor.search.summary = Some(r.summary);
    } else {
        let _ = client
            .rpc::<SearchClear>(SearchClearParams {
                buffer_id: state.editor.buffer_id,
            })
            .await;
        state.editor.search.summary = None;
    }
    state.editor.search.query.set(snap.query);
    state.editor.search.active = snap.active;
    // Restore cursor + selection.
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.editor.buffer_id,
            position: snap.cursor.position,
            anchor: snap.cursor.anchor,
        })
        .await?;
    state.editor.cursor = new;
    // Restore scroll if it moved during incremental search.
    if snap.scroll_logical_line != state.editor.scroll_logical_line {
        scroll_to(client, state, snap.scroll_logical_line).await?;
    }
    state.editor.mode = EditorMode::Normal;
    apply_cursor_style(state);
    Ok(())
}

/// Incremental-search step: tell the server the latest query and let it jump the cursor onto
/// the first match at-or-after where `/` was pressed. The server's response carries the new
/// cursor + summary; per-viewport highlight notifications follow asynchronously.
async fn run_incremental_search(client: &mut Client, state: &mut AppState) -> Result<()> {
    if state.editor.search.query.is_empty() {
        let _ = client
            .rpc::<SearchClear>(SearchClearParams {
                buffer_id: state.editor.buffer_id,
            })
            .await;
        state.editor.search.summary = None;
        // No matches — revert the cursor to the pre-search position so the user sees where
        // they started rather than wherever the previous query stranded them.
        if let Some(snap_cursor) = state.editor.search.snapshot.as_ref().map(|s| s.cursor) {
            if state.editor.cursor.position != snap_cursor.position
                || state.editor.cursor.anchor != snap_cursor.anchor
            {
                let new = client
                    .rpc::<CursorSet>(CursorSetParams {
                        buffer_id: state.editor.buffer_id,
                        position: snap_cursor.position,
                        anchor: snap_cursor.anchor,
                    })
                    .await?;
                state.editor.cursor = new;
            }
        }
        return Ok(());
    }
    let anchor = state
        .editor
        .search
        .snapshot
        .as_ref()
        .map(|s| selection_start(&s.cursor));
    let (buffer_id, query) = {
        let ed = &mut state.editor;
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
            state.editor.cursor = r.cursor;
            state.editor.search.summary = Some(r.summary.clone());
            // Zero matches: revert below so a failed keystroke doesn't strand the user.
            r.summary.total == 0
        }
        Err(_) => {
            // Most commonly an invalid regex while the user is mid-type (e.g. a trailing `\`).
            // Treat it as a transient "no matches" state — empty highlights, cursor reverted,
            // a short note in the status so the user knows why their search isn't matching.
            state.editor.search.summary = Some(SearchSummary {
                buffer_id: state.editor.buffer_id,
                total: 0,
                truncated: false,
                current_index: 0,
            });
            state.status = "invalid regex".into();
            true
        }
    };
    if revert_needed {
        if let Some(snap_cursor) = state.editor.search.snapshot.as_ref().map(|s| s.cursor) {
            if state.editor.cursor.position != snap_cursor.position
                || state.editor.cursor.anchor != snap_cursor.anchor
            {
                let new = client
                    .rpc::<CursorSet>(CursorSetParams {
                        buffer_id: state.editor.buffer_id,
                        position: snap_cursor.position,
                        anchor: snap_cursor.anchor,
                    })
                    .await?;
                state.editor.cursor = new;
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
    let ed = &state.editor;
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
    let gp = state.editor.cursor.grep_position?;
    Some(format!("({}/{})", gp.current, gp.total))
}

/// Summary line for the search prompt: "3/47", "3/10000+", or "no matches". `None` when the
/// query is empty (the bare `/` already conveys "no search yet").
pub fn search_match_count_label(state: &AppState) -> Option<String> {
    let ed = &state.editor;
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
            buffer_id: state.editor.buffer_id,
            scope: CopyScope::Selection,
        })
        .await?;
    if r.text.is_empty() {
        return Ok(());
    }
    let query = {
        let ed = &mut state.editor;
        ed.search.query.set(regex_escape(&r.text));
        ed.search.active = true;
        ed.search.query.text.clone()
    };
    push_history(state, query.clone());
    let buffer_id = state.editor.buffer_id;
    let result = client
        .rpc::<SearchSet>(SearchSetParams {
            buffer_id,
            query: query.clone(),
            anchor: None,
        })
        .await?;
    state.editor.search.summary = Some(result.summary);
    // search/set with anchor=None doesn't move the cursor server-side, so state.editor.cursor is still
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
    if !state.editor.search.active {
        // No active search: revive the most recent history entry server-side, then cycle.
        let Some(last) = state.editor.search.history.last().cloned() else {
            return Ok(());
        };
        state.editor.search.query.set(last.clone());
        let r = client
            .rpc::<SearchSet>(SearchSetParams {
                buffer_id: state.editor.buffer_id,
                query: last,
                anchor: None,
            })
            .await?;
        state.editor.cursor = r.cursor;
        state.editor.search.summary = Some(r.summary);
        state.editor.search.active = true;
    }
    let summary_total = state
        .editor
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
            buffer_id: state.editor.buffer_id,
        };
        let result = match direction {
            Direction::Forward => client.rpc::<SearchNext>(params).await?,
            Direction::Backward => client.rpc::<SearchPrev>(params).await?,
        };
        state.editor.cursor = result.cursor;
        state.editor.search.summary = Some(result.summary);
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
    let viewport_id = state.editor.viewport_id;
    let r = client
        .rpc::<ViewportResize>(ViewportResizeParams {
            viewport_id,
            cols: cols as u32,
            rows: viewport_rows,
        })
        .await?;
    let ed = &mut state.editor;
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
            buffer_id: state.editor.buffer_id,
            motion: motion.clone(),
            extend_selection: extend,
        })
        .await?;
    state.editor.cursor = new;
    if is_repeatable_motion(&motion) {
        state.editor.last_motion = Some(motion);
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
                buffer_id: state.editor.buffer_id,
                direction,
                extend,
            })
            .await?;
        state.editor.cursor = new;
    }
    Ok(())
}

async fn tree_expand(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let new = client
            .rpc::<CursorExpand>(CursorBufferOnlyParams {
                buffer_id: state.editor.buffer_id,
            })
            .await?;
        if new == state.editor.cursor {
            break; // already at root
        }
        state.editor.cursor = new;
    }
    Ok(())
}

async fn tree_contract(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let new = client
            .rpc::<CursorContract>(CursorBufferOnlyParams {
                buffer_id: state.editor.buffer_id,
            })
            .await?;
        if new == state.editor.cursor {
            break; // history empty
        }
        state.editor.cursor = new;
    }
    Ok(())
}

async fn swap_anchor(client: &mut Client, state: &mut AppState) -> Result<()> {
    let new = client
        .rpc::<CursorSwapAnchor>(CursorSwapAnchorParams {
            buffer_id: state.editor.buffer_id,
        })
        .await?;
    state.editor.cursor = new;
    Ok(())
}

async fn motion_undo(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: CursorUndoResult = client
            .rpc::<CursorUndo>(CursorUndoParams {
                buffer_id: state.editor.buffer_id,
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
                buffer_id: state.editor.buffer_id,
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
        state.editor.cursor = r.cursor;
    } else {
        state.status = format!("nothing to {label}");
    }
}

async fn clear_selection(client: &mut Client, state: &mut AppState) -> Result<()> {
    // "Clear selection" now means "collapse to a 1-char point at the current position" since
    // the data model always has an anchor. Visually unchanged: the block cursor stays put.
    let pos = state.editor.cursor.position;
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.editor.buffer_id,
            position: pos,
            anchor: pos,
        })
        .await?;
    state.editor.cursor = new;
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
        let ed = &mut state.editor;
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
            state.editor.cursor = new;
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
    state.editor.cursor = new;
    enter_insert_mode(state);
    Ok(())
}

fn enter_insert_mode(state: &mut AppState) {
    state.editor.mode = EditorMode::Insert;
    apply_cursor_style(state);
}

fn leave_insert(state: &mut AppState) {
    state.editor.mode = EditorMode::Normal;
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
            buffer_id: state.editor.buffer_id,
        })
        .await?;
    state.editor.revision = r.revision;
    state.editor.cursor = r.cursor;
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
            buffer_id: state.editor.buffer_id,
            text: text.into(),
            select_pasted,
        })
        .await?;
    state.editor.revision = r.revision;
    state.editor.cursor = r.cursor;
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
            buffer_id: state.editor.buffer_id,
        })
        .await?;
    state.editor.revision = r.revision;
    state.editor.cursor = r.cursor;
    Ok(())
}

async fn backspace(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputBackspace>(BufferOnlyParams {
            buffer_id: state.editor.buffer_id,
        })
        .await?;
    state.editor.revision = r.revision;
    state.editor.cursor = r.cursor;
    Ok(())
}

async fn delete_line(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<aether_protocol::input::InputDeleteLine>(BufferOnlyParams {
            buffer_id: state.editor.buffer_id,
        })
        .await?;
    state.editor.revision = r.revision;
    state.editor.cursor = r.cursor;
    Ok(())
}

async fn change_line(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<aether_protocol::input::InputChangeLine>(BufferOnlyParams {
            buffer_id: state.editor.buffer_id,
        })
        .await?;
    state.editor.revision = r.revision;
    state.editor.cursor = r.cursor;
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
                buffer_id: state.editor.buffer_id,
                text,
            },
        )
        .await?;
    state.editor.revision = r.revision;
    state.editor.cursor = r.cursor;
    Ok(())
}

// ---- shared Ctrl-binding dispatch -------------------------------------------------------------
//
// `handle_ctrl_binding` covers every Ctrl-modified shortcut that Normal and Insert mode share.
// Mode-dependent commands (copy/cut/paste/change/delete/replace) get thin wrappers below that
// branch on `state.editor.mode` to pick the right scope/behavior. This lets both mode
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
    match state.editor.mode {
        EditorMode::Insert => paste_at_cursor(client, state).await,
        _ => paste_before(client, state, count).await,
    }
}

/// Ctrl-c. In Normal: delete the selection and enter Insert. In Insert: blank the current line
/// (we're already in Insert).
async fn handle_change(client: &mut Client, state: &mut AppState) -> Result<()> {
    match state.editor.mode {
        EditorMode::Insert => change_line(client, state).await,
        _ => change_selection(client, state).await,
    }
}

/// Ctrl-d. In Normal: delete the selection (looped `count` times). In Insert: delete the
/// current line (count ignored — Insert has no count accumulator).
async fn handle_delete(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    match state.editor.mode {
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
    match state.editor.mode {
        EditorMode::Insert => replace_line_with_clipboard(client, state).await,
        _ => paste_replace(client, state, count).await,
    }
}

fn scope_for_mode(state: &AppState) -> CopyScope {
    match state.editor.mode {
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
                buffer_id: state.editor.buffer_id,
            })
            .await?;
        state.editor.revision = r.revision;
        state.editor.cursor = r.cursor;
    }
    Ok(())
}

async fn indent(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: EditResult = client
            .rpc::<InputIndent>(BufferOnlyParams {
                buffer_id: state.editor.buffer_id,
            })
            .await?;
        state.editor.revision = r.revision;
        state.editor.cursor = r.cursor;
    }
    Ok(())
}

async fn dedent(client: &mut Client, state: &mut AppState, count: u32) -> Result<()> {
    for _ in 0..count.max(1) {
        let r: EditResult = client
            .rpc::<InputDedent>(BufferOnlyParams {
                buffer_id: state.editor.buffer_id,
            })
            .await?;
        state.editor.revision = r.revision;
        state.editor.cursor = r.cursor;
    }
    Ok(())
}

/// Toggle line-comment status on the cursor's line (or all selected lines). Server picks the
/// prefix from the buffer language's `line_comment` and no-ops for languages without one.
async fn toggle_comment(client: &mut Client, state: &mut AppState) -> Result<()> {
    let r: EditResult = client
        .rpc::<InputToggleComment>(BufferOnlyParams {
            buffer_id: state.editor.buffer_id,
        })
        .await?;
    state.editor.revision = r.revision;
    state.editor.cursor = r.cursor;
    Ok(())
}

/// Add a blank line after the cursor's current line and drop into Insert mode at its start.
/// Implemented as: park cursor at end of current line, then `newline_and_indent` (which copies
/// the line's leading whitespace and adds one level if the line ends in an opener). The newline
/// pushes the cursor onto the new line at the indent column.
async fn open_line_below(client: &mut Client, state: &mut AppState) -> Result<()> {
    let line = state.editor.cursor.position.line;
    let target = LogicalPosition {
        line,
        col: u32::MAX,
    };
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.editor.buffer_id,
            position: target,
            anchor: target,
        })
        .await?;
    state.editor.cursor = new;
    newline_and_indent(client, state).await?;
    enter_insert_mode(state);
    Ok(())
}

/// Insert a blank line *above* the cursor's current line and drop into Insert mode on it.
/// Park at col 0 of the current line, insert "\n" (which pushes the original line down a row
/// and lands the cursor at its new start), then step back up onto the freshly-blank line.
async fn open_line_above(client: &mut Client, state: &mut AppState) -> Result<()> {
    let line = state.editor.cursor.position.line;
    let target = LogicalPosition { line, col: 0 };
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.editor.buffer_id,
            position: target,
            anchor: target,
        })
        .await?;
    state.editor.cursor = new;
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
                buffer_id: state.editor.buffer_id,
                direction,
            })
            .await?;
        state.editor.revision = r.revision;
        state.editor.cursor = r.cursor;
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
            buffer_id: state.editor.buffer_id,
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
            buffer_id: state.editor.buffer_id,
            scope,
        })
        .await?;
    state.editor.revision = r.revision;
    state.editor.cursor = r.cursor;
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
    let start = min_pos(state.editor.cursor.position, state.editor.cursor.anchor);
    let new = client
        .rpc::<CursorSet>(CursorSetParams {
            buffer_id: state.editor.buffer_id,
            position: start,
            anchor: start,
        })
        .await?;
    state.editor.cursor = new;
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
                buffer_id: state.editor.buffer_id,
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
                buffer_id: state.editor.buffer_id,
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
    state.editor.revision = r.revision;
    state.editor.cursor = r.cursor;
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
    if state.editor.file_path.is_none() {
        // Scratch buffer — no path to save to. Don't auto-prompt: the user has to be explicit
        // about creating a file with Ctrl-Alt-s. This keeps `Ctrl-s` semantics uniform: it only
        // ever writes to an already-known path.
        state.status = "scratch buffer has no path — use Ctrl-Alt-s to save as".into();
        return Ok(());
    }
    let result = client
        .rpc::<BufferSave>(BufferSaveParams {
            buffer_id: state.editor.buffer_id,
            path_index: None,
            relative_path: None,
            overwrite,
        })
        .await;
    match result {
        Ok(r) => {
            state.editor.revision = r.revision;
            state.editor.saved_revision = r.revision;
            state.editor.externally_modified = false;
            state.editor.externally_deleted = false;
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
    if state.editor.file_path.is_none() {
        state.status = "scratch buffer has no path to reload".into();
        return Ok(());
    }
    let result = client
        .rpc::<BufferReload>(BufferReloadParams {
            buffer_id: state.editor.buffer_id,
            force,
        })
        .await;
    match result {
        Ok(r) => {
            state.editor.revision = r.revision;
            state.editor.saved_revision = r.revision;
            state.editor.externally_modified = false;
            state.editor.externally_deleted = false;
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

/// Open the status-bar save-as prompt. Pre-filled with the current file's project-relative
/// path so a small rename is one Backspace + a few keys; empty for scratch buffers. Cursor
/// lands at the end of the pre-fill.
fn begin_save_prompt(state: &mut AppState) {
    let initial = state
        .editor
        .file_path
        .as_deref()
        .map(|p| project_relative_label(p, &state.project_paths))
        .unwrap_or_default();
    state.save_prompt = Some(SavePromptState {
        input: crate::text_input::TextInput::new(initial),
    });
    apply_cursor_style(state);
}

/// Open the `Space n` "new file" prompt. Pre-filled with the project-relative directory the
/// user is currently working in (parent of the current file, or the explorer's last dir),
/// plus a trailing `/` so the user just types the filename and Enter. The relative path is
/// resolved against `path_index` on commit.
fn begin_new_file_prompt(state: &mut AppState) {
    let (path_index, mut initial) = current_directory_for_new_file(state);
    if !initial.is_empty() && !initial.ends_with('/') {
        initial.push('/');
    }
    state.new_file_prompt = Some(NewFilePromptState {
        input: crate::text_input::TextInput::new(initial),
        path_index,
    });
    apply_cursor_style(state);
}

/// Best-effort `(path_index, dir_relative_to_root)` for the new-file prompt. Prefers the
/// current buffer's parent directory; falls back to the Explorer picker's current directory if
/// it's open (the user was probably about to drop a new file in there); otherwise project root.
/// The returned dir is always project-relative — empty string means "at the project root".
fn current_directory_for_new_file(state: &AppState) -> (u32, String) {
    if let Some(p) = state.editor.file_path.as_deref() {
        if let Some(parent) = std::path::Path::new(p).parent() {
            if let Some(found) = relative_to_project(parent, &state.project_paths) {
                return found;
            }
        }
    }
    if let Some(dir) = state.picker.explorer_dir.as_deref() {
        if let Some(found) = relative_to_project(std::path::Path::new(dir), &state.project_paths) {
            return found;
        }
    }
    (0, String::new())
}

/// Find the longest project root containing `abs` and return `(path_index, relative)`. `None`
/// when `abs` lies outside every root.
fn relative_to_project(abs: &std::path::Path, project_paths: &[String]) -> Option<(u32, String)> {
    project_paths
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let root = std::path::Path::new(p);
            abs.strip_prefix(root)
                .ok()
                .map(|rel| (i as u32, root, rel.display().to_string()))
        })
        .max_by_key(|(_, root, _)| root.as_os_str().len())
        .map(|(i, _, rel)| (i, rel))
}

async fn handle_new_file_prompt_key(
    client: &mut Client,
    state: &mut AppState,
    k: KeyEvent,
) -> Result<()> {
    let Some(p) = state.new_file_prompt.as_mut() else {
        return Ok(());
    };
    match crate::text_input::apply_prompt_key(&mut p.input, k) {
        PromptKeyOutcome::Cancel => {
            state.new_file_prompt = None;
            apply_cursor_style(state);
        }
        PromptKeyOutcome::Commit => commit_new_file_prompt(client, state).await?,
        PromptKeyOutcome::Edited => {}
    }
    Ok(())
}

async fn commit_new_file_prompt(client: &mut Client, state: &mut AppState) -> Result<()> {
    let (path_index, relative) = match state.new_file_prompt.as_ref() {
        Some(p) if !p.input.trim().is_empty() => (p.path_index, p.input.text.clone()),
        _ => {
            state.new_file_prompt = None;
            apply_cursor_style(state);
            return Ok(());
        }
    };
    state.new_file_prompt = None;
    apply_cursor_style(state);
    let open: BufferOpenResult = client
        .rpc::<BufferOpen>(BufferOpenParams {
            buffer_id: None,
            path_index: Some(path_index),
            relative_path: Some(relative),
            language: None,
            create_if_missing: true,
            jump_to: None,
        })
        .await?;
    subscribe_to_buffer(client, state, open).await
}

async fn handle_save_prompt_key(
    client: &mut Client,
    state: &mut AppState,
    k: KeyEvent,
) -> Result<()> {
    let Some(p) = state.save_prompt.as_mut() else {
        return Ok(());
    };
    match crate::text_input::apply_prompt_key(&mut p.input, k) {
        PromptKeyOutcome::Cancel => abort_save_prompt(state),
        PromptKeyOutcome::Commit => send_save_prompt(client, state, false).await?,
        PromptKeyOutcome::Edited => {}
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
    let path = match state.save_prompt.as_ref() {
        Some(p) if !p.input.trim().is_empty() => p.input.text.clone(),
        Some(_) => {
            // Empty input — treat as cancel.
            state.save_prompt = None;
            apply_cursor_style(state);
            return Ok(());
        }
        None => return Ok(()),
    };

    let buffer_id = state.editor.buffer_id;

    // TODO: multi-root support — when `project_paths.len() > 1` we should let the user pick a
    // root (or accept absolute paths in the prompt and infer the root) rather than silently
    // saving under the first project root.
    let result = client
        .rpc::<BufferSave>(BufferSaveParams {
            buffer_id,
            path_index: Some(0),
            relative_path: Some(path.clone()),
            overwrite,
        })
        .await;
    match result {
        Ok(r) => {
            state.save_prompt = None;
            apply_cursor_style(state);
            let project_paths = state.project_paths.clone();
            let ed = &mut state.editor;
            ed.revision = r.revision;
            ed.saved_revision = r.revision;
            ed.file_label = path.clone();
            if let Some(root) = project_paths.first() {
                ed.file_path = Some(std::path::Path::new(root).join(&path).display().to_string());
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
    if matches!(state.editor.wrap, WrapMode::None) && state.viewport_cols > 0 {
        let col = state.editor.cursor.position.col;
        if col < state.editor.scroll_col {
            state.editor.scroll_col = col;
        } else if col >= state.editor.scroll_col.saturating_add(state.viewport_cols) {
            state.editor.scroll_col = col.saturating_sub(state.viewport_cols.saturating_sub(1));
        }
    }

    let cursor_line = state.editor.cursor.position.line;
    let top = state.editor.scroll_logical_line;

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
        let target = cursor_line.min(state.editor.max_scroll_logical_line);
        scroll_to(client, state, target).await?;
    }
    Ok(())
}

/// Scroll the viewport so the cursor's logical line sits at the vertical center. Clamped to
/// `max_scroll_logical_line` so jumps near EOF don't overscroll. Approximate under soft wrap —
/// the line's first visual row lands near center, which is close enough for a quick `zz`.
async fn center_cursor(client: &mut Client, state: &mut AppState) -> Result<()> {
    let half = state.viewport_rows / 2;
    let target = state.editor.cursor.position.line.saturating_sub(half);
    let target = target.min(state.editor.max_scroll_logical_line);
    if target != state.editor.scroll_logical_line {
        scroll_to(client, state, target).await?;
    }
    Ok(())
}

async fn toggle_wrap(client: &mut Client, state: &mut AppState) -> Result<()> {
    let new_wrap = match state.editor.wrap {
        WrapMode::Soft => WrapMode::None,
        WrapMode::None => WrapMode::Soft,
    };
    let r = client
        .rpc::<ViewportSetWrap>(ViewportSetWrapParams {
            viewport_id: state.editor.viewport_id,
            wrap: new_wrap,
        })
        .await?;
    state.editor.wrap = new_wrap;
    state.editor.window_first_logical_line = r.window.first_logical_line;
    state.editor.lines = r.window.lines;
    // Horizontal scroll is meaningless under soft wrap — content never overflows right.
    if matches!(new_wrap, WrapMode::Soft) {
        state.editor.scroll_col = 0;
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
    state.editor.pending_scroll_lines = state.editor.pending_scroll_lines.saturating_add(delta);
}

/// Apply any accumulated `pending_scroll_lines` to the server via one `viewport/scroll` call.
/// No-op if zero. Called before every draw and from inside `ensure_cursor_in_window` so the
/// cursor-visibility check sees the user's intended scroll position.
async fn flush_pending_scroll(client: &mut Client, state: &mut AppState) -> Result<()> {
    let ed = &mut state.editor;
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
    if !matches!(state.editor.wrap, WrapMode::None) {
        return;
    }
    state.editor.scroll_col = if delta >= 0 {
        state.editor.scroll_col.saturating_add(delta as u32)
    } else {
        state.editor.scroll_col.saturating_sub((-delta) as u32)
    };
}

async fn scroll_to(client: &mut Client, state: &mut AppState, target_line: u32) -> Result<()> {
    let r = client
        .rpc::<ViewportScroll>(ViewportScrollParams {
            viewport_id: state.editor.viewport_id,
            scroll: ScrollPosition {
                logical_line: target_line,
                sub_row: 0.0,
            },
        })
        .await?;
    state.editor.scroll_logical_line = target_line;
    state.editor.window_first_logical_line = r.window.first_logical_line;
    state.editor.line_count = r.window.line_count;
    state.editor.max_scroll_logical_line = r.window.max_scroll_logical_line;
    state.editor.lines = r.window.lines;
    Ok(())
}
