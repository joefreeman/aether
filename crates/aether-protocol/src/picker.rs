//! Pickers — fuzzy-matched selection overlays (files, buffers, grep hits, ...). Server owns
//! the candidate cache, query, and ranked snapshot per `(client_id, kind)`; client owns the
//! highlighted row and the scroll window. Items, not indices, are the stable handle: the client
//! persists the last-highlighted item locally and asks the server to scroll to include it on
//! resume.
//!
//! Lifecycle: `picker/view` attaches/subscribes (with `reset` to wipe persisted state or
//! `center_on` to frame around a remembered item), `picker/query` updates the query, `picker/select`
//! confirms a choice, `picker/hide` unsubscribes. The server pushes `picker/update` whenever the
//! subscribed window's contents change or the matcher snapshot ticks.

use crate::cursor::Direction;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::lsp::LspStatus;
use crate::viewport::DiagnosticSeverity;
use crate::{BufferId, LogicalPosition};
use serde::{Deserialize, Serialize};

/// Which picker the client is talking about. Keyed `(client_id, kind)` server-side; only one
/// instance per kind per client lives at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickerKind {
    /// Project files, fuzzy-matched on path.
    Files,
    /// Open buffers, ordered by most-recently-used. The current buffer sits at position 0 and
    /// selecting it is a no-op switch.
    Buffers,
    /// Workspace-wide content search. Each candidate is a single match on a single line; the
    /// query *is* the search (no fuzzy filtering on a pre-built candidate set), so query changes
    /// throw out the prior candidates and start a fresh scan. Persisted hits stay around across
    /// `hide`/`view` so the user can step through results — they may be stale relative to the
    /// file on disk after editing, and that's accepted (jumps clamp to the current line bounds).
    Grep,
    /// Filesystem explorer. Entries are the children of one directory (re-listed on each
    /// `picker/view`). The query fuzzy-matches entry names within that directory. Navigation
    /// (parent / enter subdirectory) is driven by the client sending `picker/view` with a new
    /// `directory_path`; the result + push carry the canonical path the listing is for.
    Explorer,
    /// Configured projects under `$XDG_CONFIG_HOME/aether/projects/`. Fuzzy-matched on name.
    /// Selecting one triggers the client to send `project/activate`. Distinct from the other
    /// kinds in that this picker is usable *before* a project is active (it's how the user
    /// gets one active in the first place) — every other picker requires `active_project`.
    Projects,
    /// The current buffer's LSP diagnostics, fuzzy-matched on the message. Scoped to one buffer
    /// (`PickerViewParams::buffer_id`). Selecting one jumps to its position (via `FileAt`).
    Diagnostics,
    /// The language servers for the active project, fuzzy-matched on server name. Unlike the
    /// other kinds this isn't a jump target: the client restarts the highlighted server in place
    /// (`Ctrl-r` → `lsp/restart_server`) and the list live-updates as statuses change.
    LspServers,
}

impl PickerKind {
    /// Whether this picker saves its highlight + query on hide/select so the next open
    /// resumes the prior state. Only Grep does — its candidate set is the result of a
    /// (potentially slow) workspace scan and dropping it on every reopen would be wasteful.
    /// The others reset on each open so the picker stays contextual: Files and Buffers reset
    /// the query so each open is a fresh search; Explorer resets back to the active buffer's
    /// directory so it acts like "show me where I am" rather than a persistent file-manager
    /// session.
    pub fn preserves_state(self) -> bool {
        matches!(self, PickerKind::Grep)
    }
}

/// A pickable item. Tagged enum so different pickers can carry the data they need; match-index
/// highlighting rides in `match_indices` (char positions within the display string).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PickerItem {
    /// A file from the workspace walk. `relative_path` is path-relative to the root at
    /// `path_index` in the project's root list. The client formats the row by joining its own
    /// disambiguated root label with the relative path; the server stays out of presentation.
    File {
        path_index: u32,
        relative_path: String,
        /// Indices into `relative_path` (char offsets) covered by fuzzy matches. Empty on empty
        /// query. Note that the matcher haystack is `relative_path` alone — root labels are not
        /// part of the fuzzy match.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// An open buffer. Identity is `buffer_id` — stable across rename / Save-As, where the
    /// `display` string would change. `dirty` is captured at row-build time and may go stale
    /// between pushes (an active picker re-pushes on dirty transitions).
    Buffer {
        buffer_id: BufferId,
        /// What the row renders: project-relative path for file-backed buffers, `(scratch N)`
        /// for scratch buffers. Also the haystack the matcher scores against.
        display: String,
        dirty: bool,
        /// Project-relative location (root index + path) for a file-backed buffer that lives inside
        /// a project root — mirrors `File`'s fields so the client can build an opener URL. Both are
        /// `None` for scratch buffers and for files outside every root (no `?file=` URL possible).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path_index: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relative_path: Option<String>,
        /// Indices into `display` (char offsets) covered by fuzzy matches.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One match found by the grep picker. Identity is `(path_index, relative_path, line, col)`.
    /// One row per match (a line with N matches produces N hits) — keeps `match_indices` a flat
    /// list within the preview, same as the other variants.
    GrepHit {
        /// Index into the project's root list — pairs with `relative_path` to recover the
        /// absolute path.
        path_index: u32,
        /// Path relative to root `path_index` (forward-slash separated).
        relative_path: String,
        /// 0-based line number within the file.
        line: u32,
        /// 0-based byte offset of the match's first byte within the line.
        col: u32,
        /// The full text of the matching line, trimmed of its trailing newline. May be truncated
        /// at the client side to fit the picker pane.
        preview: String,
        /// Char offsets into `preview` covered by the match.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One diagnostic in the current buffer. Identity is `(line, col, message)`. The matcher
    /// haystack is `message`; `match_indices` are char offsets into it. Selecting jumps to
    /// `(line, col)`.
    Diagnostic {
        line: u32,
        col: u32,
        severity: DiagnosticSeverity,
        message: String,
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One configured project. Identity is `name` (the file stem of the project's TOML config).
    /// Selecting a `Project` returns a `PickerSelectResult::Project` and the client follows up
    /// with `project/activate`.
    Project {
        name: String,
        /// Char offsets into `name` covered by fuzzy matches.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One entry (file or directory) inside the explorer picker's current directory. Identity
    /// is `name` within the active listing; the absolute path lives only on the server.
    DirEntry {
        /// Leaf name (no path separators).
        name: String,
        /// True for subdirectories, false for files. The client uses this to gate the
        /// "Enter / Alt-l enters directory" vs. "Enter opens file" routing.
        is_dir: bool,
        /// Char offsets into `name` covered by fuzzy matches.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One of the project's roots, shown in the Explorer's Roots mode (entered by `Alt-Backspace`
    /// at the top of a root). Identity is `path_index`; the client knows the absolute path via
    /// its own copy of `project_paths`. Match indices index into the root's basename — the
    /// disambiguator is client-derived and not part of the haystack.
    Root {
        path_index: u32,
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One language server for the active project. Identity is `(language, workspace_root)` — the
    /// server key. Carries `status` so the client renders the health glyph; the matcher haystack
    /// is `name`. Not a jump target: the client acts on it via `lsp/restart_server`, so there's
    /// no corresponding `PickerSelectResult` variant.
    LspServer {
        name: String,
        language: String,
        /// Absolute workspace root — the stable identity half (with `language`).
        workspace_root: String,
        /// Display-only: `workspace_root` relative to its project root, or empty when the server
        /// is rooted *at* a project root (so single-root projects show no redundant path; only
        /// monorepo sub-roots get a disambiguating label). Server-computed.
        #[serde(default)]
        root_label: String,
        status: LspStatus,
        #[serde(default)]
        match_indices: Vec<u32>,
    },
}

// ---- picker/view --------------------------------------------------------------------------------

/// Attach to a picker, declare the scroll window to be pushed, and start receiving updates. If
/// `reset` is true, any persisted state (query, selection) is wiped first; otherwise the picker
/// resumes from whatever the prior `view`/`query`/`hide` cycle left behind. If `center_on` is
/// provided, the server picks an offset that frames the named item — this is how the client
/// restores its highlight on resume. `offset` and `center_on` are mutually exclusive —
/// `center_on` wins if both are sent.
pub struct PickerView;
impl RpcMethod for PickerView {
    const NAME: &'static str = "picker/view";
    type Params = PickerViewParams;
    type Result = PickerViewResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerViewParams {
    pub kind: PickerKind,
    /// Wipe persisted query and matcher state before attaching.
    #[serde(default)]
    pub reset: bool,
    /// First row of the window the client wants pushed. Ignored when `center_on` is set.
    #[serde(default)]
    pub offset: u32,
    pub limit: u32,
    /// If set, the server picks an `effective_offset` such that this item is inside the returned
    /// window (used on resume to restore the client's prior highlight). If the item is no longer
    /// in the results, the server falls back to `offset: 0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_on: Option<PickerItem>,
    /// Grep-only convenience: when set, the server resolves the buffer's cursor to the
    /// nearest cached hit (at-or-after the cursor's leading selection edge, walker order,
    /// wrapping to the first hit) and uses that as the effective `center_on` — overriding any
    /// explicit `center_on` the client passed. The resolved item is echoed back in
    /// `effective_center_on` so the client can use it as its resume highlight. This is what
    /// makes `Space g` open with the picker landing on "where you are" in the result list
    /// even when the cursor isn't sitting on a match exactly. No-op when there are no cached
    /// hits or `kind != Grep`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_on_cursor_grep_hit: Option<BufferId>,
    /// Explorer only: absolute path of the directory to list. `None` means "keep whatever
    /// directory the picker last listed; default to the first project root on first open".
    /// Ignored when `explorer_roots` is set, and for other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_path: Option<String>,
    /// Explorer only: when true, list the project's roots instead of a filesystem directory.
    /// Wins over `directory_path` when both are set. The client uses this to enter "Roots
    /// mode" by pressing `Alt-Backspace` at the top of a root.
    #[serde(default, skip_serializing_if = "is_false")]
    pub explorer_roots: bool,
    /// Diagnostics only: the buffer to list diagnostics for. Required when opening the Diagnostics
    /// picker (`reset: true`); `None` on resume/scroll re-views (the candidate snapshot is kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerViewResult {
    /// The current query (may be empty on first open or after `reset`).
    pub query: String,
    /// Server's view of "what query generation is current." On `reset` this resets to 0; otherwise
    /// it's the generation that was active when the persisted state was saved. The client should
    /// adopt this as its `generation` baseline.
    pub generation: u64,
    /// Total candidates in the cache. May still be growing if the walker isn't done.
    pub total_candidates: u32,
    /// The offset the server actually used (matters when the client passed `center_on`). The
    /// follow-up `picker/update` push carries the same offset.
    pub effective_offset: u32,
    /// The item the server framed `effective_offset` around. Equals what the client passed in
    /// `center_on` unless `center_on_cursor_grep_hit` resolved (and overrode it) — in which
    /// case this is the resolved hit, so the client can set its local highlight to match.
    /// `None` when no centering happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_center_on: Option<PickerItem>,
    /// Explorer only: the canonical absolute path of the directory the picker is listing. `None`
    /// for the other picker kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_path: Option<String>,
    /// Explorer only: the canonical absolute path of the parent directory, if it's still inside
    /// the project's access boundary. `None` when at (or above) a project root, and `None` for
    /// the other picker kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_parent: Option<String>,
}

// ---- picker/query -------------------------------------------------------------------------------

/// Update the active query. The client mints `generation` (monotonic per query change); the
/// server tags subsequent `picker/update` pushes with the same generation so the client can
/// discard updates from earlier queries.
pub struct PickerQuery;
impl RpcMethod for PickerQuery {
    const NAME: &'static str = "picker/query";
    type Params = PickerQueryParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerQueryParams {
    pub kind: PickerKind,
    pub query: String,
    pub generation: u64,
}

// ---- picker/select ------------------------------------------------------------------------------

/// Confirm a choice. The client sends the actual item, not an index — so there's no risk of
/// drift if results re-ranked between the user moving the highlight and pressing Enter. The
/// server acts on it (e.g. opens a buffer) and returns whatever the kind's action produces.
pub struct PickerSelect;
impl RpcMethod for PickerSelect {
    const NAME: &'static str = "picker/select";
    type Params = PickerSelectParams;
    type Result = PickerSelectResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerSelectParams {
    pub kind: PickerKind,
    pub item: PickerItem,
}

/// Per-kind action result. For `Files`, the canonical absolute path the client should open
/// (via `buffer/open`). For `Buffers`, the `buffer_id` the client should attach to (via
/// `buffer/open { buffer_id }`). For `Grep`, the canonical absolute path plus the position to
/// jump to (client opens via `buffer/open { jump_to }`). The picker handler doesn't perform the
/// switch itself — that's the client's job, same as the file browser flow.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PickerSelectResult {
    File {
        /// Absolute canonical path on disk.
        path: String,
    },
    Buffer {
        buffer_id: BufferId,
    },
    FileAt {
        /// Absolute canonical path on disk.
        path: String,
        /// Position to land the cursor on. Coordinates may be stale if the file changed since the
        /// hit was recorded; the server clamps in `buffer/open` when applying.
        position: LogicalPosition,
    },
    /// A project was selected. The client follows up with `project/activate` to switch.
    Project { name: String },
}

// ---- picker/hide --------------------------------------------------------------------------------

/// Stop pushing updates for this picker. The underlying walker/matcher state stays alive so the
/// next `view` with `reset: false` resumes from where it left off. No payload — the client owns
/// the highlight and persists it locally.
pub struct PickerHide;
impl RpcMethod for PickerHide {
    const NAME: &'static str = "picker/hide";
    type Params = PickerHideParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerHideParams {
    pub kind: PickerKind,
}

// ---- picker/grep_navigate -----------------------------------------------------------------------

/// Step through the cached grep hits from the cursor's current location without re-opening the
/// picker. Bound to `<` / `>` in Normal mode.
///
/// The server looks up the cursor's selection from its own state and uses the selection's
/// *outer* edges to skip past any match the cursor currently overlaps: Backward compares
/// against `min(anchor, position)` (so a hit at exactly the selection's start is skipped),
/// Forward against `max(anchor, position)`. This is what makes `<` go back a *real* step when
/// the cursor was just placed on a match (e.g. via `>` or via picker selection, where the
/// cursor's selection covers the entire match).
///
/// Direction: Forward = next hit, Backward = previous hit. Resolved against the cached
/// `PickerKind::Grep` candidates:
///
/// - If the current buffer's project-relative path is in the hits, find the next/previous match
///   *after* / *before* the cursor within the file. When the cursor is past the last (or before
///   the first) hit in the file, fall through to the first / last hit of the next / previous
///   file in walker order.
/// - If the current buffer's path is *not* in the hits (or the buffer has no path), virtually
///   insert it by path comparison and jump to the first / last hit of the file that would sit
///   immediately after / before it in walker order. For a buffer with no path (scratch), the
///   fallback is the first / last hit overall.
///
/// Returns `None` when there are no cached grep hits at all, or when navigation would walk past
/// the end of the list (no wraparound).
pub struct PickerGrepNavigate;
impl RpcMethod for PickerGrepNavigate {
    const NAME: &'static str = "picker/grep_navigate";
    type Params = PickerGrepNavigateParams;
    type Result = Option<PickerGrepNavigateTarget>;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerGrepNavigateParams {
    pub direction: Direction,
    pub buffer_id: BufferId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerGrepNavigateTarget {
    /// Absolute canonical path of the target file — feed into `buffer/open`.
    pub path: String,
    /// Position to jump to in the target file.
    pub position: LogicalPosition,
    /// The grep query the cached hits came from. Echoed so the client can prime the opened
    /// buffer's search state for `n` / `Alt-n` follow-on, the same way picker selection does.
    pub query: String,
}

/// Move the open grep picker's selection to the first hit of the next or previous *file* (grep
/// hits are grouped into contiguous per-file runs). Computed server-side against the full result
/// list, so it works even when the target file is past the client's over-fetched window — the
/// client then frames the returned hit via `picker/view { center_on }`.
///
/// `Forward` → first hit of the next file. `Backward` → first hit of the *current* file, or, if
/// the selection is already on it, the first hit of the previous file (vim-`{` feel). Returns
/// `None` when there's no further file in that direction (already at the first / last file).
pub struct PickerGrepFileJump;
impl RpcMethod for PickerGrepFileJump {
    const NAME: &'static str = "picker/grep_file_jump";
    type Params = PickerGrepFileJumpParams;
    type Result = Option<PickerItem>;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerGrepFileJumpParams {
    /// The selection's current absolute index in the result list (`offset + selected`).
    pub from_index: u32,
    pub direction: Direction,
}

// ---- picker/update (notification) ---------------------------------------------------------------

/// Server-pushed window contents. Sent whenever the subscribed window's items change (matcher
/// tick, query update applied, walker progress) or `total_matches` / `total_candidates` move.
///
/// The client discards updates whose `generation` doesn't match its latest query, and whose
/// `offset` doesn't match its current subscribed window — that handles in-flight crossover when
/// query or window changes hit the wire just before a push.
pub struct PickerUpdate;
impl NotificationMethod for PickerUpdate {
    const NAME: &'static str = "picker/update";
    type Params = PickerUpdateParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerUpdateParams {
    pub kind: PickerKind,
    pub generation: u64,
    pub offset: u32,
    pub items: Vec<PickerItem>,
    pub total_matches: u32,
    pub total_candidates: u32,
    /// True while the matcher is still consuming candidates (walk in progress, or matcher hasn't
    /// quiesced after a query change). The client may use this to show a spinner.
    pub ticking: bool,
    /// Grep only: the display-row index (hits interleaved with one section header per file group) of
    /// this window's first item, accounting for the headers above it. Lets a client virtual-scroll a
    /// list that renders per-file headers without its spacer under-counting those header rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grep_display_offset: Option<u32>,
    /// Grep only: total display rows in the whole result set (`total_matches` + number of file
    /// groups). Sizes the client's virtual-scroll spacer so every hit (incl. the last file's) is
    /// reachable. `None` for non-grep kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grep_total_display_rows: Option<u32>,
}
