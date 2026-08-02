//! Session state — the platform-free heart of a window's editing context
//! (docs/client-core.md): connection lifecycle, buffer identity, modal state, search,
//! prompts. The shell keeps the presentation companions (pixel scroll, animation, parsed
//! hover markdown) on its own struct.

use super::keymap::Action;
use super::picker::PickerState;
use aether_protocol::buffer::{BufferOpenResult, BufferReloadResult, BufferSaveResult};
use aether_protocol::cursor::{CursorState, Direction, Granularity, Motion};
use aether_protocol::git::CommitInfo;
use aether_protocol::history::{HistoryEntry, HistoryKind, HistoryLists};
use aether_protocol::input::SurroundTarget;
use aether_protocol::lsp::{DiagnosticCounts, LspServerRef, LspServerStatus};
use aether_protocol::picker::{CaseMode, MatchOptions};
use aether_protocol::search::SearchSummary;
use aether_protocol::viewport::{DiagnosticSeverity, ScrollPosition, Window, WrapMode};
use aether_protocol::workspace::{WorkspaceInfo, WorkspaceProject};
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
    /// A live server answered but the session couldn't be re-established (the workspace is
    /// gone). Terminal — the window stays frozen.
    Failed,
}

/// Backoff before reconnect attempt `attempt`: 250ms doubling to a 5s ceiling, retrying
/// indefinitely — a failed localhost dial is instant and free, and the daemon coming back is
/// the expected outcome, not the exception.
pub fn reconnect_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis((250u64 << attempt.min(5)).min(5000))
}

/// Backoff before *boot* dial attempt `attempt`. Boot is the one place the client is usually
/// racing a server it just spawned itself (`ensure_server_running`): the first dial fires before
/// the daemon binds, and the reconnect curve would then quantize a ~50ms server start into a
/// 500ms+ wait. So poll fast — a refused localhost connect is instant and free — for a ~1s
/// window that comfortably covers a normal daemon start, then fall back to the reconnect curve
/// for the abnormal case (spawn failed, no server coming).
pub fn boot_backoff(attempt: u32) -> std::time::Duration {
    const FAST_TRIES: u32 = 20; // 20 × 50ms — the fast window
    if attempt <= FAST_TRIES {
        std::time::Duration::from_millis(50)
    } else {
        reconnect_backoff(attempt - FAST_TRIES)
    }
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
    /// The `?` variant: grow the selection from the entry point to each incremental match.
    pub extend_to_cursor: bool,
    /// How the query matches: case mode, whole-word, and regex-vs-literal. Toggled in the search
    /// prompt (`Alt-c` / `Alt-w` / `Alt-e`) and reset to the defaults on every `/` — options are
    /// part of the search you're running, not standing configuration, exactly as every picker's
    /// chips are (`PickerReset::All` on open). `Up` recalls a past query with the options it ran
    /// under (docs/input-history.md §4a); Esc restores the pre-prompt search and its options.
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
        // No Dir chips here, so `workspace_paths` is irrelevant.
        crate::chips::derive_chips(&values, &[])
    }
}

/// `Up`/`Down` recall for the overlay text inputs (docs/input-history.md): the buffer-search
/// prompt, the grep query, and the glob / path chip editors.
///
/// Each entry carries the *configuration* it ran under as well as the text, so a recall reproduces
/// the search rather than just its words — see [`HistoryEntry`].
///
/// The *lists* are server-owned and workspace-scoped — fetched on connect and after every
/// workspace switch, appended to on commit — but the *walk* is entirely local, so a recall is a
/// keystroke, not a round-trip. One nav cursor for the session, not one per list: only a single
/// input has the keyboard at a time, and moving between them (or editing the value) abandons the
/// walk, which is what [`Self::reset`] does.
#[derive(Default)]
pub struct InputHistory {
    lists: HistoryLists,
    nav: Option<HistoryNav>,
}

/// A recall in progress: where in the list we are, and what the user had before starting —
/// text *and* configuration, both restored when they step back past the newest entry. Stashing the
/// configuration is what makes the walk non-destructive: `Up` may replace your chip row, `Down`
/// puts it back.
struct HistoryNav {
    kind: HistoryKind,
    index: usize,
    draft: HistoryEntry,
}

impl InputHistory {
    /// Adopt a `history/state` snapshot, discarding any walk in progress. Replaces rather than
    /// merges: the server's copy already includes everything this client recorded.
    pub fn adopt(&mut self, lists: HistoryLists) {
        self.lists = lists;
        self.nav = None;
    }

    /// The list for `kind`, oldest first. Exposed for the shells' status/help surfaces and tests.
    pub fn list(&self, kind: HistoryKind) -> &[HistoryEntry] {
        self.lists.get(kind)
    }

    /// Abandon any walk in progress. Called whenever the field's value changes underneath us
    /// (typing) or the focused input opens/closes — the stashed draft would be stale either way.
    pub fn reset(&mut self) {
        self.nav = None;
    }

    /// Append a committed entry locally, applying the shared dedupe/cap rule. Returns whether the
    /// list changed — `false` means the caller can skip the `history/record` round-trip.
    pub fn record(&mut self, kind: HistoryKind, entry: HistoryEntry) -> bool {
        self.lists.record(kind, entry)
    }

    /// Step one entry towards *older* (`Up`). Returns the entry to install — text and the
    /// configuration it ran under — or `None` when there's nothing to recall (empty list) or we're
    /// already at the oldest. `current` is what the field holds right now, stashed as the draft
    /// when a walk starts so `Down` can restore it.
    pub fn prev(&mut self, kind: HistoryKind, current: HistoryEntry) -> Option<HistoryEntry> {
        let len = self.lists.get(kind).len();
        if len == 0 {
            return None;
        }
        // A walk on a *different* field's list starts over — the stale draft belongs to an input
        // that no longer has the keyboard.
        let index = match &self.nav {
            Some(nav) if nav.kind == kind => {
                if nav.index == 0 {
                    return None; // oldest entry — stay put rather than wrap
                }
                nav.index - 1
            }
            _ => len - 1,
        };
        let draft = match self.nav.take() {
            Some(nav) if nav.kind == kind => nav.draft,
            _ => current,
        };
        self.nav = Some(HistoryNav { kind, index, draft });
        Some(self.lists.get(kind)[index].clone())
    }

    /// Step one entry towards *newer* (`Down`), ending by restoring the stashed draft — its
    /// configuration included, so a walk that replaced the chip row leaves it as it found it.
    /// Returns `None` when no walk is in progress (nothing newer to go to).
    pub fn next(&mut self, kind: HistoryKind) -> Option<HistoryEntry> {
        let nav = self.nav.take()?;
        if nav.kind != kind {
            return None;
        }
        let len = self.lists.get(kind).len();
        if nav.index + 1 < len {
            let index = nav.index + 1;
            let entry = self.lists.get(kind)[index].clone();
            self.nav = Some(HistoryNav { index, ..nav });
            Some(entry)
        } else {
            // Past the newest entry: back to what the user had, walk over.
            Some(nav.draft)
        }
    }
}

pub struct SearchSnapshot {
    pub cursor: CursorState,
    pub query: String,
    pub active: bool,
    /// Options at prompt-open time, restored on Esc so a cancelled search reverts any toggles too.
    pub options: MatchOptions,
}

/// Client-side state for an active sneak (`s`/`S`) word-jump. Present only while sneaking; the
/// candidate list + labels themselves are server-owned and ride the viewport render. The core keeps
/// just enough to classify each keystroke: the query typed so far, the live label set returned by
/// the last `sneak/update`, and whether this is the extend (`S`) variant.
#[derive(Default)]
pub struct SneakState {
    pub query: String,
    /// Labels currently painted on screen (from the last `sneak/update`). A keystroke in this set
    /// is a jump; anything else extends the query. Empty until the first result arrives or while
    /// the match count exceeds the available labels.
    pub labels: Vec<char>,
    pub extend: bool,
    /// Target "big" words (`Alt-s`) rather than normal word-starts (`s`). Fixed for the session.
    pub big: bool,
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
    /// The save-as path editor (`Alt-s`): a workspace-relative path field with the picker dir-chip
    /// editor's directory-completion UX (ghost suggestions, `Tab`/`Alt-l` accept, multi-root inline
    /// root field). Text editing is owned by each shell's input, which syncs the value via
    /// [`super::update`]'s `save_as_set_input` / `save_as_set_root_filter`; the core keeps the value
    /// and the command keys. See [`crate::path_editor::PathEditor`].
    SaveAs(Box<crate::path_editor::PathEditor>),
    /// LSP server detail (from the LspServers picker): info rows + `r` to restart.
    LspInfo(Box<LspServerStatus>),
    /// Application info & diagnostics (`Space ?`): build identity, live instance, on-disk paths.
    /// Rendered from [`crate::app_info::sections`] so every shell shows the same rows; `Ctrl-c`
    /// copies the lot as text, any other key closes. Re-fetched on each open — the counts inside go
    /// stale immediately, so there's nothing worth caching.
    AppInfo(Box<aether_protocol::app::AppInfo>),
    /// The open-from-path overlay (`Space Alt-w`): a single, workspace-agnostic path field. Unlike
    /// [`Self::SaveAs`] (a root-relative chip editor), this is a plain absolute/relative path —
    /// `Enter` opens it via `workspace/open_path` (external buffer outside the roots, or a fresh
    /// ephemeral context with no workspace active), `Esc` cancels. Text editing is shell-owned and
    /// synced via [`super::update`]'s `open_path_set_input`; the core keeps the value.
    OpenPath(TextField),
}

/// A single editable text field. The workspace-settings overlay holds two (name + add-root). Text
/// editing (caret, insert, delete) is owned by each shell's input — native `text_input`/`<input>`
/// in the rich clients, a shell-local editor in the TUI — which syncs the whole value via
/// [`super::update`]'s `workspace_settings_set_name` / `_set_add`. The core keeps only the value.
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

/// The workspace-settings overlay state (`Space ,`), migrated from the TUI's shell-local
/// `WorkspaceSettingsState` into the core so every shell renders it. Shows an editable
/// workspace-name field, then the active workspace's roots, then an always-present "add root" input
/// row; `selected` is the focused field.
///
/// Row layout, top to bottom: the name field, the roots, the add-root input, the projects, the
/// add-project input. [`WorkspaceSettings::row_at`] owns the mapping from `selected` to a
/// [`SettingsRow`] — with two lists each carrying a trailing input, open-coded index arithmetic
/// ("`selected - 1` is a root") stopped being tractable.
///
/// Both input rows are always reachable, which is why the overlay focuses one on open — most opens
/// are to add something.
// Neither `Clone` nor `Default` is derived: the overlay owns a `PathEditor` (which holds a cached
// directory listing), and there is exactly one of these at a time — nothing needs to copy it or
// conjure an empty one.
#[derive(Debug)]
pub struct WorkspaceSettings {
    /// The workspace's *committed* name — the key used for root/project RPCs and the rename source.
    /// Updated only when a rename succeeds; `name` holds the in-progress edit.
    pub workspace_name: String,
    /// Editable buffer for the name field (index 0). Seeded from `workspace_name` on open;
    /// committed on blur (focus leaving the field) via `workspace/rename`.
    pub name: TextField,
    pub roots: Vec<String>,
    /// Declared projects (`docs/projects.md`), server-resolved. Each carries the language its
    /// pinned server speaks, or the reason it can't be used.
    pub projects: Vec<WorkspaceProject>,
    pub selected: usize,
    /// Text being typed into the add-root input row.
    pub add: TextField,
    /// The add-project row's path editor (`docs/projects.md`) — the same component the save-as
    /// prompt uses, so declaring a project reuses the muscle memory of saving somewhere: a root
    /// typeahead segment in multi-root workspaces, then a directory-completing path field. A
    /// project is stored relative to its root, so the root genuinely isn't in the path and has to
    /// be chosen; that's exactly what this editor's root field is for.
    pub add_project: Box<crate::path_editor::PathEditor>,
    /// The add-project row's optional language segment — a typeahead over
    /// [`aether_protocol::lsp::SERVER_LANGUAGES`], the languages a server exists for. Left empty the
    /// server infers from the directory's build manifests; typed, it overrides — which is the only
    /// way to declare a tree that has no manifest (a Python package with no `pyproject.toml`) or
    /// several.
    ///
    /// It's a *separate* field rather than a third [`crate::path_editor::PathEditor`] segment
    /// because that editor is shared with the save-as prompt, which has no language to pick.
    pub add_project_language: crate::chips::Input,
    /// Highlight within the language candidates for the current filter (Alt-j/k cycles it).
    pub add_project_language_selected: usize,
    /// The language segment has focus, rather than the path editor's own two.
    pub on_add_project_language: bool,
    /// The language segment's current text was filled in by `workspace/infer_language` rather than
    /// typed — so a fresh inference may replace or clear it as the path moves. Any user edit drops
    /// the flag and the field is theirs from then on (inference stops touching it).
    pub language_inferred: bool,
    /// The `(path_index, relative_path)` the last `workspace/infer_language` request asked about:
    /// the staleness key its result is checked against, and the dedupe that makes re-syncing after
    /// any key free. `None` while the path is empty (nothing to ask about).
    pub inference_key: Option<(u32, String)>,
    /// In-dialog error from the last add/remove/rename attempt. Rendered as the bottom line of
    /// the overlay. Cleared when the user edits a field or initiates another action.
    pub error: Option<String>,
}

/// A focusable row of the workspace-settings overlay. See [`WorkspaceSettings`] for the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRow {
    /// The workspace-name field — always index 0.
    Name,
    /// The root at this index within [`WorkspaceSettings::roots`].
    Root(usize),
    /// The add-root input.
    AddRoot,
    /// The project at this index within [`WorkspaceSettings::projects`].
    Project(usize),
    /// The add-project input.
    AddProject,
}

impl WorkspaceSettings {
    /// The row at selection index `index`, clamping anything past the end onto the last row.
    pub fn row_at(&self, index: usize) -> SettingsRow {
        let add_root = self.roots.len() + 1;
        let first_project = add_root + 1;
        let add_project = first_project + self.projects.len();
        match index {
            0 => SettingsRow::Name,
            i if i < add_root => SettingsRow::Root(i - 1),
            i if i == add_root => SettingsRow::AddRoot,
            i if i < add_project => SettingsRow::Project(i - first_project),
            _ => SettingsRow::AddProject,
        }
    }

    /// The focused row.
    pub fn row(&self) -> SettingsRow {
        self.row_at(self.selected)
    }

    /// Total focusable rows: the name field, both lists, and both input rows.
    pub fn row_count(&self) -> usize {
        self.roots.len() + self.projects.len() + 3
    }

    /// Selection index of the add-root input row.
    pub fn input_index(&self) -> usize {
        self.roots.len() + 1
    }

    /// Selection index of the add-project input row — the last row.
    pub fn add_project_index(&self) -> usize {
        self.row_count() - 1
    }

    pub fn on_name(&self) -> bool {
        self.row() == SettingsRow::Name
    }

    /// Whether an input row (either one) has focus — the shells use this to decide when to hand
    /// keystrokes to a text field rather than treat them as commands.
    pub fn on_input(&self) -> bool {
        matches!(self.row(), SettingsRow::AddRoot | SettingsRow::AddProject)
    }

    /// The root under the current selection, when a root row is focused.
    pub fn selected_root(&self) -> Option<&String> {
        match self.row() {
            SettingsRow::Root(i) => self.roots.get(i),
            _ => None,
        }
    }

    /// Languages matching the add-project row's typed language filter, as indices into
    /// [`aether_protocol::lsp::SERVER_LANGUAGES`]. Prefix-matched exactly like the root typeahead —
    /// the whole list on an empty filter.
    pub fn language_candidates(&self) -> Vec<usize> {
        let all: Vec<String> = aether_protocol::lsp::SERVER_LANGUAGES
            .iter()
            .map(|s| s.to_string())
            .collect();
        crate::chips::root_candidates(&all, &self.add_project_language.text)
    }

    /// The highlighted language, if the filter matches anything.
    pub fn highlighted_language(&self) -> Option<&'static str> {
        let candidates = self.language_candidates();
        let i = *candidates.get(
            self.add_project_language_selected
                .min(candidates.len().saturating_sub(1)),
        )?;
        aether_protocol::lsp::SERVER_LANGUAGES.get(i).copied()
    }

    /// The inline completion beyond what's typed, for the ghost. `None` on an empty field: nothing
    /// is being completed there, and ghosting the first language would read as a default.
    pub fn language_ghost(&self) -> Option<String> {
        if self.add_project_language.text.is_empty() {
            return None;
        }
        let full = self.highlighted_language()?;
        full.len()
            .checked_sub(self.add_project_language.text.len())
            .map(|_| full[self.add_project_language.text.len()..].to_string())
    }

    /// The typed filter matches no supported language — rendered red, and refused on commit. Empty
    /// is fine: that's "infer it".
    pub fn language_invalid(&self) -> bool {
        !self.add_project_language.text.is_empty() && self.highlighted_language().is_none()
    }

    /// The language to send with `workspace/add_project`: `None` when the field is empty (infer
    /// server-side). Always a real supported language, never the raw typed text — the field only
    /// accepts one of ours.
    pub fn chosen_language(&self) -> Option<String> {
        if self.add_project_language.text.is_empty() {
            return None;
        }
        self.highlighted_language().map(str::to_string)
    }

    /// The project under the current selection, when a project row is focused.
    pub fn selected_project(&self) -> Option<&WorkspaceProject> {
        match self.row() {
            SettingsRow::Project(i) => self.projects.get(i),
            _ => None,
        }
    }
}

/// The application-settings overlay (`Space .`): global preferences (not per-workspace),
/// rendered by every shell from `session.app_settings`. Distinct from [`WorkspaceSettings`], which
/// edits the active workspace's name and roots.
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
    /// Size of the file text itself.
    BufferFontSize,
    /// Size of everything around it — status bar, pickers, dialogs.
    UiFontSize,
    Hints,
}

/// Font-size presets the two font-size rows step through (px). Both defaults
/// ([`aether_protocol::settings::default_buffer_font_size`] and `default_ui_font_size`) are in the
/// list, so a stored value always lands on a preset and the row's "current" maps cleanly to an
/// index.
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
    /// Removing a root from the workspace-settings overlay.
    RemoveRoot { path: String },
    /// Undeclaring a project from the workspace-settings overlay. Only the declaration goes — the
    /// directory and everything in it are untouched.
    RemoveProject { path: String },
    /// Deleting a workspace (its config) from the workspace switcher. Forgets the definition, not the
    /// files under its roots.
    DeleteWorkspace { name: String },
}

/// What accepting a confirmation does.
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    /// Retry `buffer/save` with `overwrite: true`; `target` carries the save-as path (None for
    /// the in-place save). `quit_after` threads a pending save-and-quit (`Space Alt-q`) through the
    /// confirm, so the retry still quits on success.
    Save {
        target: Option<(u32, String)>,
        quit_after: bool,
    },
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
    /// Remove a root from the workspace-settings overlay (`workspace/remove_root`). Carries the
    /// committed workspace name and the root path so the request is self-contained — the overlay's
    /// selection may have moved (or the overlay closed) by the time the confirm resolves.
    RemoveWorkspaceRoot { workspace: String, path: String },
    /// Undeclare a project (`workspace/remove_project`), unpinning its language server. Carries the
    /// workspace and the project's (root, path) pair so the request is self-contained, like
    /// [`Self::RemoveWorkspaceRoot`].
    RemoveWorkspaceProject {
        workspace: String,
        path_index: u32,
        relative_path: String,
    },
    /// Delete a workspace (`workspace/delete`) from the switcher. The server refuses if it's active
    /// anywhere or has dirty buffers; the refreshed picker list rides a `picker/update` push.
    DeleteWorkspace { name: String },
}

/// Outcome of a `buffer/save` attempt: saved, or refused pending user confirmation.
#[derive(Debug)]
pub enum SaveTry {
    Saved {
        result: BufferSaveResult,
        target: Option<(u32, String)>,
        /// Quit once the save lands (`Space Alt-q`); threaded through any overwrite confirm.
        quit_after: bool,
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

    pub workspace: String,
    pub workspace_paths: Vec<String>,
    /// The active workspace's declared projects (`docs/projects.md`), mirrored from every
    /// `WorkspaceInfo` the server sends. Read by the settings overlay when it opens; kept on the
    /// session rather than fetched on demand so opening the overlay stays a keystroke, like roots.
    pub workspace_projects: Vec<WorkspaceProject>,
    /// True when this session was launched directly to view a file outside any workspace (`ae
    /// /path`), landing it in an ephemeral context, and it hasn't switched workspaces since. It's
    /// the signal for what to do when the last buffer of an ephemeral context closes: a
    /// launched-for-a-file session has nothing left to show, so native clients quit (vim-like);
    /// a session that merely *navigated into* an ephemeral context (via the switcher) returns to
    /// the chooser instead. Set by the shells at a file-launch bootstrap; cleared on any workspace
    /// switch. See [`crate::update`]'s `leave_ephemeral_workspace`.
    pub launched_with_file: bool,
    pub buffer: BufferInfo,
    pub mode: Mode,
    pub pending: Pending,
    pub count: Option<u32>,
    pub last_repeat: Option<RepeatTarget>,
    pub search: SearchState,
    /// `Up`/`Down` recall for every overlay text input (docs/input-history.md). Session-wide, not
    /// per-overlay: the lists are workspace-scoped and only one input has the keyboard at a time.
    pub history: InputHistory,
    /// Active sneak word-jump session (`s`/`S`), or `None` when not sneaking. While `Some`, the key
    /// handler interprets keystrokes as query/label input rather than normal-mode bindings.
    pub sneak: Option<SneakState>,
    /// The logical-line range actually on screen (`first`..`last`, last exclusive), kept current by
    /// the shells (which own the pixel scroll) via [`Session::set_visible_lines`]. Used to scope
    /// sneak candidates to what's truly visible — the server's window carries a screen of overscan,
    /// so it can't tell. `None` until a window is loaded.
    pub visible_lines: Option<(u32, u32)>,

    pub viewport_id: Option<ViewportId>,
    pub window: Option<Window>,
    pub wrap: WrapMode,
    /// Coding ligatures in the editor font — an app-wide setting (`Space .`), seeded from
    /// `settings/get` at boot. The shells read it each render to pick their text shaping
    /// (native) / font feature (web); the core just holds the value.
    pub ligatures: bool,
    /// Buffer text size in px — an app-wide setting (`Space .`), seeded from `settings/get` at
    /// boot and synced via `settings/changed`. The GUI/web shells read it each render to size the
    /// buffer text (and reflow); the terminal client ignores it. The core just holds the value.
    pub buffer_font_size: u32,
    /// UI text size in px — the same deal for everything *around* the buffer (status bar, pickers,
    /// dialogs, hover, toasts, hints), which the GUI/web shells scale from this one number. Sized
    /// separately from [`Self::buffer_font_size`]: chrome density and code size are different
    /// preferences.
    pub ui_font_size: u32,
    /// Hints on/off — an app-wide setting (`Space .`), seeded from `settings/get` at
    /// boot and synced via `settings/changed`. Gates the hint engine (docs/hints.md); the corner
    /// hint disappears (and observation stops) when off.
    pub hints_enabled: bool,
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
    /// The workspace-settings overlay (`Space ,`); owns the keyboard while open.
    pub workspace_settings: Option<WorkspaceSettings>,
    /// The application-settings overlay (`Space .`); owns the keyboard while open.
    pub app_settings: Option<AppSettingsOverlay>,
    pub conn: ConnState,
    /// A content scroll anchor captured before a re-layout (wrap / diff toggle), so the view can be
    /// restored to the same content afterwards. Set by [`Session::capture_scroll_anchor`] and
    /// consumed by [`Session::resolve_scroll_anchor`]. See [`crate::grid::ScrollAnchor`].
    relayout_anchor: Option<crate::grid::ScrollAnchor>,
    /// LSP servers (by [`lsp_toast_group`] key) we've asked to restart and are awaiting the
    /// `Ready`/`Crashed` outcome for. Gates the in-place "restarting → ready" toast so an ordinary
    /// busy→idle `lsp/status_changed` blip doesn't spuriously toast. See the `LspStatusChanged`
    /// handler in [`crate::update`].
    pub(crate) lsp_restart_pending: std::collections::HashSet<String>,
    /// The hint engine (docs/hints.md): curriculum progress, per-context display
    /// slots, sampling. Dormant until the `hints/state` snapshot adopts, and gated by
    /// [`Self::hints_enabled`]. Shells read [`crate::update`]'s `hint_view()` and drive
    /// `on_hint_tick()`.
    pub hints: crate::hints::HintEngine,
}

/// The toast group key identifying one LSP *server instance* — `language` + its `workspace_root`,
/// the same identity halves the picker and status pushes carry. Keeps each server's lifecycle toast
/// separate (restarting two servers shows two toasts, each updating in place).
pub fn lsp_toast_group(language: &str, workspace_root: &str) -> String {
    format!("lsp:{language}:{workspace_root}")
}

/// Tab stop width used for all cell math (mirrors the value the shells pass to the server on
/// subscribe). Single-sourced here so the anchor math agrees with the rendered layout.
pub const TAB_WIDTH: u32 = 4;

impl Session {
    /// Build a session for a workspace the shell has just activated.
    ///
    /// Takes the whole [`WorkspaceInfo`] rather than picking fields out of it: boot is the one
    /// place a session is seeded *without* going through `sync_workspace_info`, so anything the
    /// shell forgets to carry across is simply missing until the next workspace event — which for a
    /// freshly booted client is never. (That's exactly how `projects` came to be empty in the
    /// settings overlay after a restart.) Passing the struct makes new fields flow automatically.
    pub fn new(workspace: WorkspaceInfo, buffer: BufferInfo) -> Self {
        Session {
            pending_rpcs: std::collections::HashMap::new(),
            next_token: 0,
            workspace: workspace.name,
            workspace_paths: workspace.paths,
            workspace_projects: workspace.projects,
            launched_with_file: false,
            buffer,
            mode: Mode::Normal,
            pending: Pending::None,
            count: None,
            last_repeat: None,
            search: SearchState::default(),
            history: InputHistory::default(),
            sneak: None,
            visible_lines: None,
            viewport_id: None,
            window: None,
            wrap: WrapMode::Soft,
            ligatures: true,
            buffer_font_size: aether_protocol::settings::default_buffer_font_size(),
            ui_font_size: aether_protocol::settings::default_ui_font_size(),
            hints_enabled: true,
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
            workspace_settings: None,
            app_settings: None,
            conn: ConnState::Connected,
            relayout_anchor: None,
            lsp_restart_pending: std::collections::HashSet::new(),
            hints: crate::hints::HintEngine::default(),
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
                    id: AppSettingId::BufferFontSize,
                    label: "Buffer font size",
                    control: AppSettingControl::Value(self.buffer_font_size),
                    hint: "File text size in pixels (GUI/web; the terminal uses its own font)",
                },
                AppSettingRow {
                    id: AppSettingId::UiFontSize,
                    label: "UI font size",
                    control: AppSettingControl::Value(self.ui_font_size),
                    hint: "Status bar, picker and dialog text size in pixels (GUI/web)",
                },
                AppSettingRow {
                    id: AppSettingId::Hints,
                    label: "Hints",
                    control: AppSettingControl::Toggle(self.hints_enabled),
                    hint: "Suggest things to try in the corner (Space h dismisses one, Space Alt-h toggles)",
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

    /// The boot chooser's session (no workspace picked yet): every shell raises the Workspaces
    /// picker over one of these and renders its no-workspace view (no editor) behind it. Picking
    /// a workspace activates it and the session adopts in place.
    pub fn placeholder() -> Self {
        Session::new(
            WorkspaceInfo {
                name: String::new(),
                paths: Vec::new(),
                projects: Vec::new(),
            },
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

    /// A boot placeholder ([`Session::placeholder`]): no workspace activated and no real buffer
    /// (the sentinel `buffer_id == 0`, which the server never assigns). Shells render their
    /// no-workspace view — no editor, no viewport subscribe — until a workspace is picked and
    /// [`Session::adopt_switch`](crate::update) lands the first real buffer.
    pub fn is_placeholder(&self) -> bool {
        self.buffer.buffer_id == 0
    }
}

/// Build the client-side buffer record from a `buffer/open` result.
/// The display label for a saved buffer at `path`: its workspace-relative location in the canonical
/// `"[root]: [path]"` form (bare path for single-root workspaces), falling back to the absolute path
/// when it sits outside every root. This is what the status bar and window title render, so it must
/// match the buffers picker — both route through [`labels::root_relative_display`]. Shared by
/// buffer-open and the save-as rename adoption so both relabel identically.
pub fn label_for_path(path: &str, roots: &[String]) -> String {
    match strip_longest_root(path, roots) {
        Some((idx, rel)) => crate::labels::root_relative_display(roots, idx, &rel),
        None => path.to_string(),
    }
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

/// Find the workspace root that contains `abs` (longest match wins, for nested roots) and return
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

/// What `Space m`'s blame → commit-info chain resolved to.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn label_for_path_single_root_is_bare_relative() {
        // Single-root workspace: no root prefix, just the workspace-relative path — what the status
        // bar and title show.
        let r = roots(&["/home/joe/work/repo"]);
        assert_eq!(
            label_for_path("/home/joe/work/repo/src/main.rs", &r),
            "src/main.rs"
        );
    }

    #[test]
    fn label_for_path_multi_root_prefixes_disambiguated_label() {
        // Two roots sharing a basename: the status bar / title (and picker) prefix the disambiguated
        // root label as "[root]: [path]". This is the regression this whole change guards against.
        let r = roots(&["/home/joe/work/api", "/home/joe/personal/api"]);
        assert_eq!(
            label_for_path("/home/joe/work/api/src/main.rs", &r),
            "api (work): src/main.rs"
        );
        assert_eq!(
            label_for_path("/home/joe/personal/api/lib.rs", &r),
            "api (personal): lib.rs"
        );
    }

    #[test]
    fn label_for_path_outside_all_roots_falls_back_to_absolute() {
        let r = roots(&["/home/joe/work/api", "/home/joe/personal/api"]);
        assert_eq!(label_for_path("/etc/hosts", &r), "/etc/hosts");
    }

    #[test]
    fn boot_backoff_polls_fast_then_falls_back_to_reconnect_curve() {
        use std::time::Duration;
        // Fast window: a just-spawned daemon binds in tens of ms; each retry stays at 50ms so
        // connecting tracks actual readiness instead of quantizing to the reconnect curve.
        assert_eq!(boot_backoff(1), Duration::from_millis(50));
        assert_eq!(boot_backoff(20), Duration::from_millis(50));
        // Past the window (~1s of polling), no server is coming imminently — hand over to the
        // reconnect curve rather than hammering forever.
        assert_eq!(boot_backoff(21), reconnect_backoff(1));
        assert_eq!(boot_backoff(30), reconnect_backoff(10));
    }

    #[test]
    fn reconnect_backoff_doubles_to_a_ceiling() {
        use std::time::Duration;
        assert_eq!(reconnect_backoff(0), Duration::from_millis(250));
        assert_eq!(reconnect_backoff(1), Duration::from_millis(500));
        assert_eq!(reconnect_backoff(4), Duration::from_millis(4000));
        // Capped: attempts 5+ all wait 5s.
        assert_eq!(reconnect_backoff(5), Duration::from_millis(5000));
        assert_eq!(reconnect_backoff(50), Duration::from_millis(5000));
    }
}
