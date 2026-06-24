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
use aether_protocol::picker::{CaseMode, MatchOptions};
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
    /// How the query matches: case mode, whole-word, and regex-vs-literal. Sticky across `/`
    /// presses within a session (like the grep picker's filters); toggled in the search prompt
    /// (`Alt-c` / `Alt-w` / `Alt-e`) and adopted from a grep result that primed the search.
    pub options: MatchOptions,
    /// Which option chip is "selected" for keyboard editing, mirroring the grep picker's
    /// `chip_selected`. `Some(i)` indexes [`SearchState::option_chips`]; Left/Right walk the row,
    /// Backspace/Delete remove, Enter cycles, Esc/typing deselect. `None` while the query input
    /// owns the keyboard. Reset on every prompt open / commit / abort.
    pub chip_selected: Option<usize>,
    /// State to restore on Esc, snapshotted when the prompt opens.
    pub snapshot: Option<SearchSnapshot>,
}

impl SearchState {
    /// The active match options as filter chips, rendered exactly like the grep picker's
    /// (`Aa`/`aa` for a forced case mode, `wd` for whole-word, `lit` for a literal query). Empty
    /// when every option is at its default (regex, smartcase). Each shell renders these with its
    /// own picker-chip styling, before the query — so search options read the same as grep's.
    pub fn option_chips(&self) -> Vec<crate::chips::Chip> {
        let mut values: Vec<crate::chips::ChipValue> = Vec::new();
        if self.options.case != CaseMode::Smart {
            values.push(crate::chips::ChipValue::Case(self.options.case));
        }
        if self.options.whole_word {
            values.push(crate::chips::ChipValue::Word);
        }
        if self.options.regex {
            values.push(crate::chips::ChipValue::Regex);
        }
        // No Dir chips here, so `project_paths` is irrelevant.
        crate::chips::derive_chips(&values, &[])
    }
}

pub struct SearchSnapshot {
    pub cursor: CursorState,
    pub query: String,
    pub active: bool,
    /// Options at prompt-open time, restored on Esc so a cancelled search reverts any toggles too.
    pub options: MatchOptions,
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
    /// The save-as path editor (`Alt-s`): a project-relative path field with the picker dir-chip
    /// editor's directory-completion UX (ghost suggestions, `Tab`/`Alt-l` accept, multi-root inline
    /// root field). Text editing is owned by each shell's input, which syncs the value via
    /// [`super::update`]'s `save_as_set_input` / `save_as_set_root_filter`; the core keeps the value
    /// and the command keys. See [`crate::save_as::SaveAsEditor`].
    SaveAs(Box<crate::save_as::SaveAsEditor>),
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

/// The application-settings overlay (`Space .`): global preferences (not per-project),
/// rendered by every shell from `session.app_settings`. Distinct from [`ProjectSettings`], which
/// edits the active project's name and roots.
///
/// The setting *values* live on the session (soft wrap is [`Session::wrap`], persisted server-side
/// via `settings/set`); this overlay holds only the open state and the focused-row cursor. Settings
/// are presented as labelled checkboxes arranged into [`AppSettingGroup`]s; `selected` indexes the
/// flat row list ([`Session::app_setting_rows`]) for keyboard navigation.
#[derive(Debug, Clone, Default)]
pub struct AppSettingsOverlay {
    /// Focused row index into [`Session::app_setting_rows`] (the groups flattened in order).
    pub selected: usize,
}

/// Stable identity of a setting, so toggling is keyed by *which* setting rather than a flat index
/// that shifts as groups/rows are reordered. The shells never see this — they toggle by row index,
/// which [`Session::toggle_app_setting`] resolves to an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSettingId {
    SoftWrap,
    Ligatures,
    FontSize,
}

/// Editor font-size presets the [`AppSettingId::FontSize`] row steps through (px). The default
/// ([`aether_protocol::settings::default_font_size`]) is one of them, so the stored value always
/// lands on a preset and the row's "current" maps cleanly to an index.
pub const FONT_SIZE_PRESETS: &[u32] = &[10, 11, 12, 13, 14, 16, 18, 20, 24];

/// Step `current` to an adjacent font-size preset. `up` picks the larger neighbour. With `wrap`,
/// stepping past an end wraps around (Enter/Space cycle the row); without, it clamps (the Left/Right
/// stepper). A `current` that isn't a preset (e.g. an older hand-edited `settings.toml`) snaps to
/// the nearest one first.
pub fn step_font_size(current: u32, up: bool, wrap: bool) -> u32 {
    let presets = FONT_SIZE_PRESETS;
    let idx = presets
        .iter()
        .position(|&v| v == current)
        .or_else(|| {
            presets
                .iter()
                .enumerate()
                .min_by_key(|(_, &v)| v.abs_diff(current))
                .map(|(i, _)| i)
        })
        .unwrap_or(0);
    let n = presets.len();
    let next = if up {
        if idx + 1 < n {
            idx + 1
        } else if wrap {
            0
        } else {
            idx
        }
    } else if idx > 0 {
        idx - 1
    } else if wrap {
        n - 1
    } else {
        idx
    };
    presets[next]
}

/// The control a settings row presents: an on/off checkbox, or a stepped numeric value (font size).
/// The shells render each kind; activating a row (Enter / Space / click) advances it — flips a
/// toggle, or steps a value to the next preset (wrapping) — via [`Session::activate_app_setting`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSettingControl {
    /// `true` is "on" / checked.
    Toggle(bool),
    /// The current value (px, for font size); presets + stepping live in the core.
    Value(u32),
}

/// One row of the application-settings overlay: its identity, label, current control state, and a
/// hint describing what it does. Built by [`Session::app_setting_groups`] so every shell shows the
/// same rows in the same order.
#[derive(Debug, Clone)]
pub struct AppSettingRow {
    pub id: AppSettingId,
    pub label: &'static str,
    pub control: AppSettingControl,
    pub hint: &'static str,
}

/// A titled group of related settings, for display. Groups are purely presentational — keyboard
/// navigation and toggling run over the flattened row list ([`Session::app_setting_rows`]).
#[derive(Debug, Clone)]
pub struct AppSettingGroup {
    pub title: &'static str,
    pub rows: Vec<AppSettingRow>,
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
    /// Close a specific (unsaved) buffer picked from the Buffers picker, despite its changes. Unlike
    /// [`CloseDiscard`] (which targets the active buffer), this carries the picked buffer's id — the
    /// picker selection may have moved by the time the confirm resolves. The picker stays open and
    /// re-lists from the server's `picker/update` push.
    ClosePickerBuffer { buffer_id: BufferId },
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
    /// `Ctrl-r` armed: the next keystroke names the case transform (`CaseKind::from_char`).
    Transform,
}

/// What `.` replays: the binding intent for table actions, the resolved motion (with its target
/// char) for find.
#[derive(Debug, Clone)]
pub enum RepeatTarget {
    Action { action: Action, count: u32 },
    Find(Motion),
}

#[derive(Debug, Clone, Copy)]
pub enum PasteKind {
    /// Normal-mode `Ctrl-v`: collapse to selection start, insert, select pasted.
    Before { count: u32 },
    /// Normal-mode `Ctrl-Alt-v`: insert over the selection (the server replaces it), select pasted.
    Replace { count: u32 },
    /// Insert-mode `Ctrl-v`: plain insert at the caret.
    AtCursor,
    /// Insert-mode `Ctrl-Alt-v`: replace the whole line.
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
    /// Coding ligatures in the editor font — an app-wide setting (`Space .`), seeded from
    /// `settings/get` at boot. The shells read it each render to pick their text shaping
    /// (native) / font feature (web); the core just holds the value.
    pub ligatures: bool,
    /// Editor font size in px — an app-wide setting (`Space .`), seeded from `settings/get` at
    /// boot and synced via `settings/changed`. The GUI/web shells read it each render to size the
    /// buffer text (and reflow); the terminal client ignores it. The core just holds the value.
    pub font_size: u32,
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
    /// The application-settings overlay (`Space .`); owns the keyboard while open.
    pub app_settings: Option<AppSettingsOverlay>,
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
            ligatures: true,
            font_size: aether_protocol::settings::default_font_size(),
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
            app_settings: None,
            conn: ConnState::Connected,
            relayout_anchor: None,
        }
    }

    /// The application-settings groups for the overlay, in display order. Built against the live
    /// session so every shell renders identical groups/labels/states. Adding a setting means adding a
    /// row here (and a toggle arm in [`crate::update`]'s `toggle_app_setting`, keyed by
    /// [`AppSettingId`]).
    pub fn app_setting_groups(&self) -> Vec<AppSettingGroup> {
        vec![AppSettingGroup {
            title: "View",
            rows: vec![
                AppSettingRow {
                    id: AppSettingId::SoftWrap,
                    label: "Soft wrap",
                    control: AppSettingControl::Toggle(self.wrap == WrapMode::Soft),
                    hint: "Wrap long lines to the viewport width",
                },
                AppSettingRow {
                    id: AppSettingId::Ligatures,
                    label: "Ligatures",
                    control: AppSettingControl::Toggle(self.ligatures),
                    hint: "Coding ligatures in the editor font (→, ≠, ⇒, …)",
                },
                AppSettingRow {
                    id: AppSettingId::FontSize,
                    label: "Font size",
                    control: AppSettingControl::Value(self.font_size),
                    hint: "Editor text size in pixels (GUI/web; the terminal uses its own font)",
                },
            ],
        }]
    }

    /// The settings rows flattened across all groups, in display order — the index space keyboard
    /// navigation and toggling run over (group headers aren't selectable).
    pub fn app_setting_rows(&self) -> Vec<AppSettingRow> {
        self.app_setting_groups()
            .into_iter()
            .flat_map(|g| g.rows)
            .collect()
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
/// The display label for a saved buffer at `path`: its project-relative path, falling back to the
/// absolute path when it sits outside every root. Shared by buffer-open and the save-as rename
/// adoption so both relabel identically.
pub fn label_for_path(path: &str, roots: &[String]) -> String {
    strip_longest_root(path, roots)
        .map(|(_, rel)| rel)
        .unwrap_or_else(|| path.to_string())
}

pub fn buffer_info(open: BufferOpenResult, roots: &[String]) -> BufferInfo {
    let label = match (&open.path, open.scratch_number) {
        (Some(path), _) => label_for_path(path, roots),
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

/// What `Space y`'s blame → commit-info chain resolved to.
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
