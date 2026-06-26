//! Authoritative in-memory state owned by the server.

use crate::indent::{self, IndentStyle};
use crate::picker::{self as picker_state, PickerState};
use crate::syntax::{self, InjectionLayer, LanguageConfig};
use crate::workspace_index::WorkspaceIndex;
use aether_protocol::cursor::CursorState;
use aether_protocol::envelope::Notification;
use aether_protocol::picker::{MatchOptions, PickerKind};
use aether_protocol::viewport::{ScrollPosition, WrapMode};
use aether_protocol::{BufferId, ClientId, LogicalPosition, Revision, ViewportId};
use std::time::{Duration, Instant};
use tree_sitter::{InputEdit, Parser, Point, Tree};

/// Edits within this window join the active undo group.
const GROUP_TIME_WINDOW: Duration = Duration::from_millis(500);
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

pub type SharedState = Arc<Mutex<ServerState>>;

pub struct ServerState {
    /// Loaded projects, keyed by project name. Populated lazily by `project/activate` — a project
    /// the user has configured but never activated is *not* here. Each entry owns the project's
    /// canonical paths and workspace index. Never removed at runtime (no project/unload concept);
    /// dropped only with the server.
    pub projects: HashMap<String, ProjectEntry>,
    /// File-system watcher for this server. `None` until [`crate::watcher::spawn`] runs — that
    /// happens during `run_with_listener`. `project/activate` reaches in to add new project roots
    /// once a project gets loaded. Per-server (not a global) so tests can spin up multiple servers
    /// in the same process without sharing watcher state.
    pub watcher: Option<Arc<std::sync::Mutex<notify::RecommendedWatcher>>>,
    pub buffers: HashMap<BufferId, Buffer>,
    /// Which project each open buffer belongs to. Populated when a buffer is created
    /// (`buffer/open`) and looked up when scoping per-buffer state to a project (e.g. on
    /// `project/activate`, when tearing down a client's state for the previously active project).
    pub buffer_projects: HashMap<BufferId, String>,
    pub clients: HashMap<ClientId, ClientSession>,
    pub viewports: HashMap<ViewportId, Viewport>,
    pub cursors: HashMap<(ClientId, BufferId), CursorState>,
    /// Per-`(client, buffer)` history of cursor states for motion undo/redo. Distinct from the
    /// buffer's own undo stack: this rewinds *only* the client's own cursor moves and is cleared
    /// by any buffer mutation (since prior positions may no longer be valid).
    pub motion_history: HashMap<(ClientId, BufferId), MotionHistory>,
    /// The cursor's "intended" *visual* column for vertical motions — preserved across repeated
    /// `Motion::VisualLine` presses so that landing on rows with different prefixes (continuation
    /// marker + indent) doesn't cause the visual column to drift. Cleared by any non-vertical
    /// motion, explicit cursor set, or buffer mutation. Only meaningful for `VisualLine`; logical
    /// `j/k` clears it (mixing motion kinds resets intent).
    pub virtual_col: HashMap<(ClientId, BufferId), u32>,
    /// Per-`(client, buffer)` selection-expansion history. Each entry is a prior cursor state
    /// that `cursor/contract` will restore. Pushed by `cursor/expand`; cleared by any other
    /// cursor RPC (or buffer mutation) to keep contraction well-defined.
    pub tree_selection_history: HashMap<(ClientId, BufferId), Vec<CursorState>>,
    /// Per-`(client, buffer)` active search. Set by `search/set`, cleared by `search/clear` or
    /// when the client disconnects / the buffer closes. Re-run whenever the buffer mutates.
    pub searches: HashMap<(ClientId, BufferId), SearchEntry>,
    /// Per-`(client, buffer)` active sneak (`s`/`S`) word-jump session. Set/refined by
    /// `sneak/update`, cleared by `sneak/select` / `sneak/cancel` or when the client disconnects /
    /// the buffer closes. Purely transient view-layer state — no buffer mutation happens during a
    /// sneak, so unlike searches it never needs an after-edit recompute.
    pub sneaks: HashMap<(ClientId, BufferId), SneakEntry>,
    /// Per-`(client, buffer)` LSP document-highlight set: the occurrences of the symbol under the
    /// cursor, painted with the same styling as search matches when no search is active. Stored as a
    /// [`SearchEntry`] so it renders through the very same path (`render_matches` → `matches_on_line`);
    /// only `matches` is meaningful (the other fields go unused). Refreshed — debounced — as the
    /// cursor settles, and cleared when a search is set, the cursor leaves a symbol, the buffer
    /// mutates, or the client disconnects / the buffer closes.
    pub symbol_highlights: HashMap<(ClientId, BufferId), SearchEntry>,
    /// Debounce generation for [`symbol_highlights`], bumped per cursor-settle request. A spawned
    /// refresh applies its result only while the generation still matches — a newer cursor move (or a
    /// buffer mutation) supersedes any in-flight round-trip. Mirrors the picker async-load epoch.
    pub symbol_highlight_gen: HashMap<(ClientId, BufferId), u64>,
    /// Per-`(client, buffer)` last-known scroll position. Written whenever the client subscribes
    /// or scrolls a viewport on the buffer, and surfaced on `buffer/open` so the client can
    /// restore the view when it reopens the buffer (e.g. navigating away and back via the file
    /// browser). Cleared on disconnect.
    pub last_scroll: HashMap<(ClientId, BufferId), ScrollPosition>,
    /// Per-`(client, kind)` picker state. Survives `picker/hide` (so resume restores query +
    /// ranking); cleared on disconnect.
    pub pickers: HashMap<(ClientId, PickerKind), PickerState>,
    /// Per-client navigation history (the jump list): back/forward across files, browser-style.
    /// Distinct from `motion_history` (per-buffer cursor undo via `z`): coarse, cross-buffer, and
    /// untouched by edits or `z`. Recorded on qualifying jumps (the navigating `buffer/open`'s
    /// `record_nav_from`); driven by the TUI's `nav/back`/`nav/forward`. The web client rides
    /// native browser history instead, so its
    /// entries here go unused — but recording stays uniform across clients. Cleared on disconnect.
    pub nav_history: HashMap<ClientId, NavHistory>,
    /// Per-buffer *unstaged* diff hunks: the live buffer against its **index** content
    /// (`git diff`). Populated on `buffer/open` for file-backed buffers; recomputed as the buffer
    /// changes. Empty / absent for scratch buffers and files outside a repo. Shared by all clients
    /// viewing the buffer (the baseline is a property of the file, not the viewer). Drives the
    /// unstaged half of the status-bar counts and one side of the combined view.
    pub git_unstaged_hunks: HashMap<BufferId, Vec<crate::git::DiffHunk>>,
    /// Per-buffer **combined** view hunks (what the gutter / inline diff renders): the unstaged
    /// hunks plus the staged (HEAD→index) hunks carried into buffer coordinates, each tagged with
    /// its `DiffStage`. Composed by `git::compose_both` on the same triggers as the unstaged set.
    pub git_both_hunks: HashMap<BufferId, Vec<crate::git::DiffHunk>>,
    /// Per-buffer cached Git baseline: resolved repo location + the committed (HEAD) content,
    /// LF-normalized. Populated on open, refreshed when HEAD changes (the watcher), and read by
    /// the per-edit `diff_hunks` so editing never re-runs repo discovery or re-reads the blob.
    pub git_baseline: HashMap<BufferId, crate::git::GitBaseline>,
    /// Per-buffer cached whole-file blame, tagged with the revision it was computed at. Lazily
    /// (re)computed on `git/blame_line` when stale, so moving the cursor around a buffer at one
    /// revision never recomputes. Cleared on close.
    pub git_blame: HashMap<BufferId, BlameCache>,
    /// Single shared fuzzy matcher. `nucleo_matcher::Matcher` reuses scratch buffers across
    /// calls, so it's cheaper to share one than construct per RPC. Not `Sync`, so the global
    /// lock around `ServerState` is what serializes access.
    pub matcher: nucleo_matcher::Matcher,
    /// Language-server sessions (one per workspace-root × language) and the buffers synced against
    /// them. See [`crate::lsp::manager`].
    pub lsp: crate::lsp::manager::LspManager,
    /// Latest diagnostics per buffer, in buffer coordinates (byte columns). Replaced wholesale on
    /// each `publishDiagnostics`; cleared on close. Empty/absent when a buffer has none. Drives the
    /// open-buffer surfaces: squiggles, gutter counts, and the buffer-scoped `Space d` picker.
    pub diagnostics: HashMap<BufferId, Vec<crate::lsp::diagnostics::BufferDiagnostic>>,
    /// Latest diagnostics per file **path**, line-granular (no byte column). Every `publishDiagnostics`
    /// updates this keyed by the file's canonical path — for *every* file a server reports, open or
    /// not (rust-analyzer's `cargo check` / flycheck pushes cover the whole build). This is the sole
    /// source for the project-wide `Space Alt-d` picker — independent of the buffer-keyed
    /// [`Self::diagnostics`], not merged with it — and it retains a file's last-known set after its
    /// buffer closes. An empty push removes the entry.
    pub path_diagnostics: HashMap<std::path::PathBuf, Vec<crate::lsp::diagnostics::RawDiagnostic>>,
    /// Latest LSP `textDocument/documentSymbol` outline per buffer, flattened depth-first (the
    /// same shape the `Space o` picker shows). Refreshed asynchronously after the language server
    /// re-analyzes (on `publishDiagnostics`) and on buffer open. Drives the `o` symbol-navigation
    /// motion so it walks the same items as the outline. Absent until the first fetch lands; the
    /// `o` motion no-ops (rather than falling back to tree-sitter) while it's absent for a buffer
    /// whose language has a server — see `cursor::resolve_navigation_motion`.
    pub document_symbols: HashMap<BufferId, Vec<crate::picker::SymbolCandidate>>,
    /// This server instance's start time (unix ms) — its identity, reported to clients on
    /// `project/activate` so they can detect a daemon restart across a reconnect. Set once at
    /// construction; the same value is written to the runtime file.
    pub started_at_unix_ms: u64,
    /// Where to read/write the persisted project-session file
    /// ([`crate::config::ProjectSessions`]). `Some` in the real server (set at boot in
    /// `server::run`); `None` everywhere else — in-process tests and embeddings leave it unset so
    /// they never touch the developer's real `~/.config/aether/sessions.json`. When `None`, session
    /// recency/restore is simply disabled (all logic short-circuits).
    pub sessions_path: Option<PathBuf>,
    next_buffer_id: u64,
    next_viewport_id: u64,
}

/// Cached whole-file blame for a buffer, valid only while `revision` matches the buffer's. One
/// entry per 0-based buffer line; `None` for lines with no blame (e.g. the trailing empty line).
pub struct BlameCache {
    pub revision: Revision,
    pub lines: Vec<Option<aether_protocol::git::BlameInfo>>,
}

/// Server-side state for one client's active search on a specific buffer.
#[derive(Debug, Clone)]
pub struct SearchEntry {
    pub query: String,
    /// How the query matches (case / whole-word / literal). Recorded so an after-edit recompute
    /// ([`crate::handlers::refresh_searches_for_buffer`]) re-runs with the same options the
    /// search was set with.
    pub options: MatchOptions,
    /// Sorted by start position. Each match is `(start_inclusive, end_exclusive)` in
    /// buffer-line / byte-col coords.
    pub matches: Vec<(LogicalPosition, LogicalPosition)>,
    /// `true` when the server hit its match cap (`SEARCH_MAX_MATCHES`) and the real count is
    /// higher. `matches.len()` is then a prefix.
    pub truncated: bool,
    /// 1-based match index most recently sent in a `search/state_changed` notification for this
    /// client+buffer. Used to dedup cursor-move-driven pushes so we only fire when the cursor
    /// actually crosses a match boundary.
    pub last_pushed_index: u32,
}

/// Server-side state for one client's active sneak word-jump on a specific buffer. The candidate
/// list is recomputed on every `sneak/update`; it's kept between updates so label assignment can
/// stay stable across refinement and so `sneak/select` can resolve a label to its word.
#[derive(Debug, Clone)]
pub struct SneakEntry {
    /// The query typed so far (the word prefix). Empty right after `s`, before any char.
    pub query: String,
    /// The viewport whose visible range scoped the candidate search — re-derived each update.
    pub viewport_id: ViewportId,
    pub candidates: Vec<SneakCandidate>,
}

/// One matched word-start in a sneak session.
#[derive(Debug, Clone, Copy)]
pub struct SneakCandidate {
    /// Absolute char index of the word's first char — the stable key used to preserve a word's
    /// label across refinement (refining only removes candidates, so survivors keep their label).
    pub start_char: usize,
    /// Inclusive word start (the cell the label is painted over).
    pub start: LogicalPosition,
    /// Exclusive word end — `start`..`end_excl` is the byte range highlighted, and the inclusive
    /// last char (`end_excl` minus one char) is where a non-extending jump puts the cursor head.
    pub end_excl: LogicalPosition,
    /// Position just past the typed query prefix within the word (`start` plus the query's char
    /// count). `start`..`prefix_end` is the chip the client brightens — one cell per typed char.
    pub prefix_end: LogicalPosition,
    /// The assigned label char, or `None` while deferring (more matches than available labels).
    pub label: Option<char>,
}

/// Cap on each direction's stack. Bounds memory in pathological cases (e.g. holding down a
/// motion key), and matches the "cursor undo is per-client transient state, not an audit log"
/// framing.
pub const MOTION_HISTORY_CAP: usize = 100;

#[derive(Default)]
pub struct MotionHistory {
    pub undo: VecDeque<CursorState>,
    pub redo: Vec<CursorState>,
}

impl MotionHistory {
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

/// One location in the navigation history (jump list): a buffer plus the cursor/selection to
/// restore. The path fields let a closed file be reopened; `buffer_id` is preferred while the
/// buffer is still open (and is the only handle a scratch buffer has).
#[derive(Clone, Debug, PartialEq)]
pub struct NavEntry {
    pub buffer_id: BufferId,
    pub path_index: Option<u32>,
    pub relative_path: Option<String>,
    pub cursor: CursorState,
}

/// A client's back/forward jump list. Browser semantics: a jump pushes onto `back` and clears
/// `forward`; stepping back/forward moves entries between the two and across the "current" cursor.
#[derive(Default)]
pub struct NavHistory {
    pub back: Vec<NavEntry>,
    pub forward: Vec<NavEntry>,
}

/// Cap on each direction of the jump list, mirroring `MOTION_HISTORY_CAP`'s "transient, not an
/// audit log" framing — old jumps fall off the bottom.
pub const NAV_HISTORY_CAP: usize = 100;

impl NavHistory {
    /// Push `entry` onto the back stack (dropping the oldest past the cap) and clear forward.
    /// Collapses an exact duplicate of the current top so re-recording the same spot is a no-op.
    /// Returns whether anything was pushed.
    pub fn record(&mut self, entry: NavEntry) -> bool {
        if self.back.last() == Some(&entry) {
            self.forward.clear();
            return false;
        }
        self.back.push(entry);
        if self.back.len() > NAV_HISTORY_CAP {
            self.back.remove(0);
        }
        self.forward.clear();
        true
    }
}

/// One project, loaded and ready to serve. Owns its canonical roots and workspace index (the
/// picker file cache). One per active project; lives in `ServerState::projects`, keyed by `id`.
///
/// Identity (`id`) is separate from the human name (`name`). A persisted project — one backed by a
/// `<name>.toml` on disk — has `name: Some(..)`, and its `id` equals that name. An *ephemeral*
/// project — synthesized to host files opened outside any configured project (`ae /path/to/file`,
/// open-from-path, goto-def into the stdlib) — has `name: None`, a generated reserved `id`, no
/// config on disk, and is auto-removed once its last buffer closes. So `name.is_some()` *is* the
/// persistence signal: there's no separate "ephemeral" flag that could fall out of sync.
pub struct ProjectEntry {
    /// Stable identity and `projects` map key. For a persisted project this equals its `name`; for
    /// an ephemeral project it's a generated token (see [`ServerState::ephemeral_project_id`]) that
    /// can never collide with a valid project name — it contains a path separator, which
    /// `validate_project_name` forbids.
    pub id: String,
    /// The persisted project name, or `None` for an ephemeral project. `Some` ⇔ a `<name>.toml`
    /// exists on disk ⇔ this project survives losing its last buffer.
    pub name: Option<String>,
    /// Canonicalized project paths. Each is either a file or a directory. Empty for an ephemeral
    /// project (so every buffer in it is "external" — see [`ProjectEntry::contains`]).
    pub paths: Vec<PathBuf>,
    /// Workspace-wide candidate cache for this project. Walked lazily on first picker access;
    /// survives picker hide/show.
    pub workspace_index: Arc<WorkspaceIndex>,
    /// Most-recently-used buffers in this project, front = most-recent. Bumped on every
    /// `buffer/open` (fresh open, reopen, or attach-by-id). Drives the buffer picker's empty-
    /// query ordering, and the `last_buffer_id` returned by `project/activate` (so re-attaching
    /// to a project drops the user on the buffer they last had).
    ///
    /// Lives on the project — not on the client — so it persists across client disconnects.
    /// A new TUI invocation gets a fresh `ClientId` but inherits the project's MRU.
    pub mru_buffers: VecDeque<BufferId>,
    /// Buffers restored from the persisted session ([`crate::config::ProjectSession`]) on
    /// activation but not yet loaded into memory — most-recently-used first, mirroring the order
    /// they were saved in. Each holds a reserved [`BufferId`] (its picker identity) and the file's
    /// canonical path; it carries no rope/syntax/LSP. The buffer picker lists them greyed out
    /// after the live buffers; opening one materializes a real buffer (see
    /// `buffer_open`'s by-id path) and drops it from here. Never contains a path that's also a
    /// live buffer in this project — promotion removes it.
    pub dormant_buffers: Vec<DormantBuffer>,
}

/// A session-restored buffer that hasn't been loaded yet. See [`ProjectEntry::dormant_buffers`].
#[derive(Debug, Clone)]
pub struct DormantBuffer {
    /// Reserved id — the buffer's identity in the picker, so selecting it can route back through
    /// `buffer/open { buffer_id }`. Not present in `ServerState::buffers` until materialized.
    pub id: BufferId,
    /// Canonical path of the file to load when the buffer is materialized.
    pub path: PathBuf,
}

impl ProjectEntry {
    /// True iff the given canonical path falls under one of this project's roots. Always `false`
    /// for an ephemeral project (no roots), which is exactly what makes every buffer in it
    /// "external" — see [`ServerState::buffer_is_external`].
    pub fn contains(&self, canonical: &Path) -> bool {
        self.paths
            .iter()
            .any(|p| canonical == p || canonical.starts_with(p))
    }

    /// Ephemeral ⇔ not persisted ⇔ no on-disk config. The single source of truth is `name.is_none()`.
    pub fn is_ephemeral(&self) -> bool {
        self.name.is_none()
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            projects: HashMap::new(),
            watcher: None,
            buffers: HashMap::new(),
            buffer_projects: HashMap::new(),
            clients: HashMap::new(),
            viewports: HashMap::new(),
            cursors: HashMap::new(),
            motion_history: HashMap::new(),
            virtual_col: HashMap::new(),
            tree_selection_history: HashMap::new(),
            searches: HashMap::new(),
            sneaks: HashMap::new(),
            symbol_highlights: HashMap::new(),
            symbol_highlight_gen: HashMap::new(),
            last_scroll: HashMap::new(),
            pickers: HashMap::new(),
            nav_history: HashMap::new(),
            git_unstaged_hunks: HashMap::new(),
            git_both_hunks: HashMap::new(),
            git_baseline: HashMap::new(),
            git_blame: HashMap::new(),
            matcher: picker_state::make_matcher(),
            lsp: crate::lsp::manager::LspManager::default(),
            diagnostics: HashMap::new(),
            path_diagnostics: HashMap::new(),
            document_symbols: HashMap::new(),
            started_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            sessions_path: None,
            next_buffer_id: 1,
            next_viewport_id: 1,
        }
    }

    /// Look up a loaded project by name. Returns `None` if the project hasn't been activated by
    /// any client yet (or doesn't exist).
    pub fn project(&self, name: &str) -> Option<&ProjectEntry> {
        self.projects.get(name)
    }

    /// The project the given client currently has activated, if any.
    pub fn active_project(&self, client_id: ClientId) -> Option<&ProjectEntry> {
        let session = self.clients.get(&client_id)?;
        let name = session.active_project.as_deref()?;
        self.projects.get(name)
    }

    /// Same as [`Self::active_project`] but surfaces a `NO_ACTIVE_PROJECT` `RpcError` for the
    /// common handler pattern of "require an active project or bail." Most non-`project/*`
    /// handlers want this.
    pub fn active_project_or_err(
        &self,
        client_id: ClientId,
    ) -> Result<&ProjectEntry, crate::error::RpcError> {
        self.active_project(client_id)
            .ok_or_else(crate::error::RpcError::no_active_project)
    }

    /// Id of the project a buffer belongs to. `None` if the buffer is unknown or somehow
    /// untagged (shouldn't happen for live buffers but the lookup is defensive).
    pub fn project_for_buffer(&self, buffer_id: BufferId) -> Option<&str> {
        self.buffer_projects.get(&buffer_id).map(|s| s.as_str())
    }

    /// Whether a buffer is *external* to its owning project: a file-backed buffer whose path falls
    /// under none of the project's roots. Computed live from the roots (never stored), so adding or
    /// removing a root reclassifies open buffers automatically. Scratch buffers (no path) and
    /// buffers whose project can't be found are not external. Every file-backed buffer in an
    /// ephemeral project is external (it has no roots). External buffers get no git baseline and a
    /// trust-restricted LSP path (see the LSP manager); the client shows an "external" marker.
    pub fn buffer_is_external(&self, buffer_id: BufferId) -> bool {
        let Some(path) = self
            .buffers
            .get(&buffer_id)
            .and_then(|b| b.canonical_path.as_deref())
        else {
            return false;
        };
        match self
            .buffer_projects
            .get(&buffer_id)
            .and_then(|id| self.projects.get(id))
        {
            Some(project) => !project.contains(path),
            None => false,
        }
    }

    /// The display number for a *new* ephemeral project: the lowest positive integer not in use by
    /// another live ephemeral project. Mirrors [`Self::next_scratch_number`] — numbers stay small
    /// and a freed one is reused once its project is pruned — so the picker shows `(project 1)`,
    /// `(project 2)`, … rather than an ever-climbing counter. Because ephemeral projects are pruned
    /// the moment they empty, the lowest-free number is always unique among the live set, so it
    /// doubles as the id suffix.
    fn next_ephemeral_number(&self) -> u32 {
        let used: std::collections::HashSet<u32> = self
            .projects
            .values()
            .filter_map(|p| {
                p.id.strip_prefix(aether_protocol::EPHEMERAL_PROJECT_PREFIX)
                    .and_then(|n| n.parse().ok())
            })
            .collect();
        (1..)
            .find(|n| !used.contains(n))
            .expect("u32 range is non-empty")
    }

    /// Mint a fresh ephemeral-project id, `ephemeral/<n>`. The `/` can never appear in a valid
    /// project name (`validate_project_name` rejects separators), so the id never collides with a
    /// persisted project or resolves to an on-disk config path; `<n>` is the small reusable display
    /// number (see [`Self::next_ephemeral_number`]).
    pub fn ephemeral_project_id(&mut self) -> String {
        let n = self.next_ephemeral_number();
        format!("{}{n}", aether_protocol::EPHEMERAL_PROJECT_PREFIX)
    }

    /// Register a fresh, rootless, nameless project and return its id. The caller activates it for
    /// a client and opens a buffer in it; it is auto-removed when that last buffer closes (see
    /// [`Self::prune_ephemeral_if_empty`]).
    pub fn register_ephemeral_project(&mut self) -> String {
        let id = self.ephemeral_project_id();
        let workspace_index = Arc::new(WorkspaceIndex::new(Vec::new()));
        self.projects.insert(
            id.clone(),
            ProjectEntry {
                id: id.clone(),
                name: None,
                paths: Vec::new(),
                workspace_index,
                mru_buffers: VecDeque::new(),
                dormant_buffers: Vec::new(),
            },
        );
        id
    }

    /// Drop an ephemeral project once it holds no buffers and no client still has it active. A
    /// no-op for persisted projects and for ephemeral ones that still host a buffer. Call after any
    /// buffer close so an ephemeral project's lifetime is exactly "while it has a buffer".
    /// Returns `true` if the project was removed.
    pub fn prune_ephemeral_if_empty(&mut self, project_id: &str) -> bool {
        let is_ephemeral = self
            .projects
            .get(project_id)
            .is_some_and(|p| p.is_ephemeral());
        if !is_ephemeral {
            return false;
        }
        if self.buffers_in_project(project_id).is_empty()
            && !self.project_active_anywhere(project_id)
        {
            self.projects.remove(project_id);
            return true;
        }
        false
    }

    /// Retire an ephemeral project the moment it loses its *last buffer*: remove it and clear it
    /// from any client still parked in it. Unlike [`Self::prune_ephemeral_if_empty`] — which keeps
    /// an empty context alive while a client has it active — this *evicts* those clients (their
    /// `active_project` becomes `None`), because an ephemeral context with no files has no reason
    /// to linger in the switcher even if a second client had selected it. Call after a user-driven
    /// `buffer/close`; the evicted clients are being told the buffer closed (`buffer/closed`) and
    /// drop to the chooser. Returns whether the project was removed.
    pub fn retire_ephemeral_if_empty(&mut self, project_id: &str) -> bool {
        let is_ephemeral = self
            .projects
            .get(project_id)
            .is_some_and(|p| p.is_ephemeral());
        if !is_ephemeral || !self.buffers_in_project(project_id).is_empty() {
            return false;
        }
        for s in self.clients.values_mut() {
            if s.active_project.as_deref() == Some(project_id) {
                s.active_project = None;
            }
        }
        self.projects.remove(project_id);
        true
    }

    /// Rename a loaded project in place: move its entry to the new key (updating `entry.name`),
    /// then re-point every buffer association and client active-project that referenced the old
    /// name. Open buffers are otherwise untouched — only the name key changes, nothing is closed —
    /// so this is safe even with dirty buffers in the project. Returns the project's root paths
    /// (display form) on success, or `None` if no project was loaded under `old`. The caller is
    /// responsible for renaming the on-disk config and for rejecting name collisions first.
    pub fn rename_project(&mut self, old: &str, new: &str) -> Option<Vec<String>> {
        let mut entry = self.projects.remove(old)?;
        // Rename only applies to persisted projects, whose id tracks their name.
        entry.id = new.to_string();
        entry.name = Some(new.to_string());
        let paths: Vec<String> = entry
            .paths
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        self.projects.insert(new.to_string(), entry);
        for project in self.buffer_projects.values_mut() {
            if project == old {
                *project = new.to_string();
            }
        }
        for session in self.clients.values_mut() {
            if session.active_project.as_deref() == Some(old) {
                session.active_project = Some(new.to_string());
            }
        }
        Some(paths)
    }

    /// True if any connected client currently has `name` as its active project. `project/delete`
    /// refuses in that case so deletion can't pull the rug out from under an open session.
    pub fn project_active_anywhere(&self, name: &str) -> bool {
        self.clients
            .values()
            .any(|c| c.active_project.as_deref() == Some(name))
    }

    /// Buffer ids belonging to `name`. Used by `project/delete` to find what it would close (and
    /// to screen them for unsaved changes first).
    pub fn buffers_in_project(&self, name: &str) -> Vec<BufferId> {
        self.buffer_projects
            .iter()
            .filter(|(_, p)| p.as_str() == name)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Whether any open buffer (in any project) has unsaved edits. The idle reaper consults this so
    /// an auto-started server never shuts itself down while work is in flight.
    pub fn has_unsaved_buffers(&self) -> bool {
        self.buffers.values().any(|b| b.dirty)
    }

    /// How many open buffers in `project` have unsaved edits (`Buffer::dirty`). Drives the
    /// unsaved-count shown on each row of the project picker. `0` for a project with no loaded
    /// buffers (the common case for a configured-but-unvisited project).
    pub fn unsaved_buffer_count(&self, project: &str) -> u32 {
        self.buffer_projects
            .iter()
            .filter(|(id, p)| {
                p.as_str() == project && self.buffers.get(id).map(|b| b.dirty).unwrap_or(false)
            })
            .count() as u32
    }

    /// The display number to assign a *new* scratch buffer in `project`: the lowest positive
    /// integer not already in use by another scratch there. Keeps `(scratch N)` numbers small and
    /// stable, reusing one once its buffer closes. Call before inserting the new buffer.
    pub fn next_scratch_number(&self, project: &str) -> u32 {
        let used: std::collections::HashSet<u32> = self
            .buffer_projects
            .iter()
            .filter(|(_, p)| p.as_str() == project)
            .filter_map(|(id, _)| self.buffers.get(id))
            .filter_map(|b| b.scratch_number)
            .collect();
        (1..)
            .find(|n| !used.contains(n))
            .expect("u32 range is non-empty")
    }

    /// Buffer ids in `project` whose backing file is at or under `canonical` — an exact match for
    /// a file, or a path-prefix match for a directory. Used by `path/delete` to find the buffers a
    /// deletion would close (and to screen them for unsaved changes first).
    pub fn buffers_under_path(&self, project: &str, canonical: &Path) -> Vec<BufferId> {
        self.buffers
            .iter()
            .filter(|(id, b)| {
                self.buffer_projects.get(id).map(|s| s.as_str()) == Some(project)
                    && b.canonical_path
                        .as_deref()
                        .is_some_and(|p| p == canonical || p.starts_with(canonical))
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Close one buffer: drop it and every per-`(client, buffer)` slice keyed to it. This is the
    /// canonical `buffer/close` teardown, shared by root removal, project deletion, and path
    /// deletion so they can't drift out of sync.
    /// Returns the key of a language server that was torn down because this was its last buffer
    /// (so the caller can refresh open status views), or `None`.
    pub fn close_buffer(&mut self, id: BufferId) -> Option<crate::lsp::manager::LspServerKey> {
        // Notify any language server before we drop the buffer (needs its path).
        let lsp_uri = self
            .buffers
            .get(&id)
            .and_then(|b| b.canonical_path.as_deref())
            .map(crate::lsp::uri::path_to_uri);
        let stopped_server = lsp_uri.and_then(|uri| self.lsp.notify_close(id, &uri));
        self.buffers.remove(&id);
        self.buffer_projects.remove(&id);
        self.viewports.retain(|_, v| v.buffer_id != id);
        self.cursors.retain(|(_, b), _| *b != id);
        self.motion_history.retain(|(_, b), _| *b != id);
        self.virtual_col.retain(|(_, b), _| *b != id);
        self.tree_selection_history.retain(|(_, b), _| *b != id);
        self.searches.retain(|(_, b), _| *b != id);
        self.sneaks.retain(|(_, b), _| *b != id);
        self.symbol_highlights.retain(|(_, b), _| *b != id);
        self.symbol_highlight_gen.retain(|(_, b), _| *b != id);
        self.last_scroll.retain(|(_, b), _| *b != id);
        self.git_unstaged_hunks.remove(&id);
        self.git_both_hunks.remove(&id);
        self.git_baseline.remove(&id);
        self.git_blame.remove(&id);
        self.diagnostics.remove(&id);
        self.document_symbols.remove(&id);
        self.drop_buffer_from_mru(id);
        stopped_server
    }

    /// Close every buffer in `candidates` that is transient and no longer shown by any viewport.
    /// This is the "hidden ⇒ close" half of transient buffers; callers pass the buffers a client
    /// just stopped viewing (viewport switch, project switch, disconnect) *after* dropping the
    /// stale viewports. Dirty buffers are skipped as a guard — the first edit promotes, so a
    /// dirty transient shouldn't exist. Returns the ids closed and the keys of language servers
    /// torn down with them (so callers can refresh picker views).
    pub fn close_orphaned_transients(
        &mut self,
        candidates: impl IntoIterator<Item = BufferId>,
    ) -> (Vec<BufferId>, Vec<crate::lsp::manager::LspServerKey>) {
        let mut closed = Vec::new();
        let mut stopped = Vec::new();
        for id in candidates {
            let eligible = self
                .buffers
                .get(&id)
                .is_some_and(|b| b.transient && !b.dirty)
                && !self.viewports.values().any(|v| v.buffer_id == id);
            if !eligible {
                continue;
            }
            closed.push(id);
            if let Some(key) = self.close_buffer(id) {
                stopped.push(key);
            }
        }
        (closed, stopped)
    }

    /// Delete a loaded project's in-memory state: drop the project entry and close every buffer
    /// that belonged to it, tearing down all per-buffer state (same teardown as `buffer/close` /
    /// `project/remove_root`). Returns the closed buffer ids. The caller is responsible for the
    /// refusal checks ([`Self::project_active_anywhere`], dirty buffers) and for removing the
    /// on-disk config. A no-op for the project entry when it was never loaded; still closes any
    /// of its buffers that exist.
    pub fn delete_project(&mut self, name: &str) -> Vec<BufferId> {
        let closed = self.buffers_in_project(name);
        for &id in &closed {
            self.close_buffer(id);
        }
        self.projects.remove(name);
        closed
    }

    pub fn allocate_buffer_id(&mut self) -> BufferId {
        let id = self.next_buffer_id;
        self.next_buffer_id += 1;
        id
    }

    pub fn allocate_viewport_id(&mut self) -> ViewportId {
        let id = self.next_viewport_id;
        self.next_viewport_id += 1;
        id
    }

    /// Remove all viewports owned by the given client. Used on disconnect.
    pub fn drop_viewports_for_client(&mut self, client_id: ClientId) {
        self.viewports.retain(|_, v| v.client_id != client_id);
    }

    /// Remove all cursor records for the given client. Used on disconnect.
    pub fn drop_cursors_for_client(&mut self, client_id: ClientId) {
        self.cursors.retain(|(c, _), _| *c != client_id);
    }

    /// Remove all motion-history records for the given client. Used on disconnect.
    pub fn drop_motion_history_for_client(&mut self, client_id: ClientId) {
        self.motion_history.retain(|(c, _), _| *c != client_id);
    }

    /// Record a user-initiated cursor state transition. The previous state goes on the undo
    /// stack and the redo stack is cleared. No-op if the state didn't change. Called by every
    /// `cursor/*` handler.
    pub fn record_motion(
        &mut self,
        key: (ClientId, BufferId),
        prev: CursorState,
        next: CursorState,
    ) {
        if prev == next {
            return;
        }
        let history = self.motion_history.entry(key).or_default();
        // Skip duplicate top — defensive against compound client ops that touch the cursor more
        // than once via the same intermediate state.
        if history.undo.back() != Some(&prev) {
            history.undo.push_back(prev);
            while history.undo.len() > MOTION_HISTORY_CAP {
                history.undo.pop_front();
            }
        }
        history.redo.clear();
    }

    /// Clear motion history for every client on the given buffer. Called on any buffer mutation
    /// (text, delete, cut, join, undo, redo) — remembered positions could be invalid after the
    /// buffer changes, and the user contract is "motion undo only goes back to the last edit".
    pub fn clear_motion_history_for_buffer(&mut self, buffer_id: BufferId) {
        for ((_, b), h) in self.motion_history.iter_mut() {
            if *b == buffer_id {
                h.clear();
            }
        }
    }

    pub fn drop_virtual_col_for_client(&mut self, client_id: ClientId) {
        self.virtual_col.retain(|(c, _), _| *c != client_id);
    }

    /// Remove all search records for the given client. Used on disconnect.
    pub fn drop_searches_for_client(&mut self, client_id: ClientId) {
        self.searches.retain(|(c, _), _| *c != client_id);
        self.symbol_highlights.retain(|(c, _), _| *c != client_id);
        self.symbol_highlight_gen.retain(|(c, _), _| *c != client_id);
    }

    /// Remove all sneak sessions for the given client. Used on disconnect.
    pub fn drop_sneaks_for_client(&mut self, client_id: ClientId) {
        self.sneaks.retain(|(c, _), _| *c != client_id);
    }

    /// Remove all last-scroll records for the given client. Used on disconnect.
    pub fn drop_last_scroll_for_client(&mut self, client_id: ClientId) {
        self.last_scroll.retain(|(c, _), _| *c != client_id);
    }

    /// Remove all picker state for the given client. Used on disconnect.
    pub fn drop_pickers_for_client(&mut self, client_id: ClientId) {
        self.pickers.retain(|(c, _), _| *c != client_id);
    }

    /// Remove the navigation history for the given client. Used on disconnect (a reconnect is a
    /// fresh session, so the jump list — like cursor/selection state — is not recovered).
    pub fn drop_nav_history_for_client(&mut self, client_id: ClientId) {
        self.nav_history.remove(&client_id);
    }

    /// Bump `buffer_id` to the front of its project's MRU. Called from `buffer/open` every time
    /// any client lands on a buffer — fresh open, reopen, or attach-by-id. No-op if the buffer
    /// has no recorded project (shouldn't happen for live buffers but the lookup is defensive).
    pub fn touch_mru(&mut self, buffer_id: BufferId) {
        let Some(project_name) = self.buffer_projects.get(&buffer_id).cloned() else {
            return;
        };
        let Some(project) = self.projects.get_mut(&project_name) else {
            return;
        };
        project.mru_buffers.retain(|&b| b != buffer_id);
        project.mru_buffers.push_front(buffer_id);
    }

    /// Drop `buffer_id` from every project's MRU. Called from `buffer/close` so a closed buffer
    /// doesn't reappear at the top of the picker on the next open.
    pub fn drop_buffer_from_mru(&mut self, buffer_id: BufferId) {
        for project in self.projects.values_mut() {
            project.mru_buffers.retain(|&b| b != buffer_id);
        }
    }

    /// The set of file-backed buffers to persist for `project_name`, most-recently-used first:
    /// the project's live MRU buffers that have a path, then its still-dormant buffers (already in
    /// MRU order). Deduplicated by path. This is exactly what a future activation should restore,
    /// so it's what gets written to the session file.
    ///
    /// Two kinds are excluded: scratch buffers (no path), and **transient** buffers — preview
    /// opens that auto-close once you navigate away (grep/file-picker peeks, goto-def, nav
    /// revisits). Transient means "ephemeral, don't accumulate me"; persisting previews would
    /// reintroduce exactly the buffer-list clutter the transient mechanism exists to avoid, so the
    /// restored set is just the working buffers you edited or explicitly kept (which clears the
    /// transient flag — promotion — and brings them back in here). It also means a preview's
    /// open/auto-close never churns the file.
    pub fn session_buffer_paths(&self, project_name: &str) -> Vec<PathBuf> {
        let Some(project) = self.projects.get(project_name) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = Vec::new();
        let mut seen: std::collections::HashSet<&Path> = std::collections::HashSet::new();
        for id in &project.mru_buffers {
            let Some(buf) = self.buffers.get(id) else {
                continue;
            };
            if buf.transient {
                continue;
            }
            if let Some(path) = buf.canonical_path.as_deref() {
                if seen.insert(path) {
                    out.push(path.to_path_buf());
                }
            }
        }
        for d in &project.dormant_buffers {
            if seen.insert(d.path.as_path()) {
                out.push(d.path.clone());
            }
        }
        out
    }

    /// Remove the dormant entry for `canonical` from `project_name`, if present. Called when a live
    /// buffer for that path opens, so the now-loaded file stops showing as a greyed dormant row.
    pub fn promote_dormant(&mut self, project_name: &str, canonical: &Path) {
        if let Some(project) = self.projects.get_mut(project_name) {
            project.dormant_buffers.retain(|d| d.path.as_path() != canonical);
        }
    }

    /// Remove and return the canonical path of the dormant buffer with `id` in `project_name`, if
    /// any. Used by `buffer/open`'s by-id path to materialize a dormant buffer the picker selected.
    pub fn take_dormant(&mut self, project_name: &str, id: BufferId) -> Option<PathBuf> {
        let project = self.projects.get_mut(project_name)?;
        let pos = project.dormant_buffers.iter().position(|d| d.id == id)?;
        Some(project.dormant_buffers.remove(pos).path)
    }

    /// The id of `project_name`'s most-recently-used dormant buffer (front of the list), if any.
    /// Used as the activation landing target when the project has no live MRU buffer yet (a cold
    /// restore after a restart).
    pub fn first_dormant_id(&self, project_name: &str) -> Option<BufferId> {
        self.projects
            .get(project_name)?
            .dormant_buffers
            .first()
            .map(|d| d.id)
    }

    /// Drop the selection-expansion history for one client+buffer. Called from every cursor RPC
    /// except `expand` / `contract` (and from every buffer mutation) so the contract chain only
    /// follows a contiguous run of expands.
    pub fn clear_tree_selection_history(&mut self, client_id: ClientId, buffer_id: BufferId) {
        self.tree_selection_history.remove(&(client_id, buffer_id));
    }

    /// Clear selection-expansion history for every client on the given buffer. Called from
    /// buffer mutation paths so a post-edit contract doesn't pop a stale (pre-edit) selection.
    pub fn clear_tree_selection_history_for_buffer(&mut self, buffer_id: BufferId) {
        self.tree_selection_history
            .retain(|(_, b), _| *b != buffer_id);
    }

    /// Remove all selection-expansion records for the given client. Used on disconnect.
    pub fn drop_tree_selection_history_for_client(&mut self, client_id: ClientId) {
        self.tree_selection_history
            .retain(|(c, _), _| *c != client_id);
    }

    /// Clear virtual column for every client on the given buffer. Called on any buffer mutation.
    pub fn clear_virtual_col_for_buffer(&mut self, buffer_id: BufferId) {
        self.virtual_col.retain(|(_, b), _| *b != buffer_id);
    }

    /// Locate an already-open buffer for the given canonical path, if any. Scoped to a project —
    /// two projects can independently open the same file as separate buffers, and a path lookup
    /// during `buffer/open` for project A shouldn't latch onto project B's existing buffer.
    pub fn buffer_for_path_in_project(
        &self,
        project_name: &str,
        canonical: &Path,
    ) -> Option<BufferId> {
        self.buffers.iter().find_map(|(id, b)| {
            if b.canonical_path.as_deref() == Some(canonical)
                && self.buffer_projects.get(id).map(|s| s.as_str()) == Some(project_name)
            {
                Some(*id)
            } else {
                None
            }
        })
    }

    /// Locate every open buffer for the given canonical path, across all projects. Used by the
    /// file watcher, which has a path but not a project context. Plural because projects with
    /// overlapping roots can each hold their own buffer for the same file — a disk change must
    /// reach all of them, not whichever one iteration order yields first.
    pub fn buffers_for_path(&self, canonical: &Path) -> Vec<BufferId> {
        self.buffers
            .iter()
            .filter(|(_, b)| b.canonical_path.as_deref() == Some(canonical))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Tear down all per-`(client, buffer)` state for buffers that belong to `project_name`,
    /// limited to one client. Used when the client switches its active project: the buffers
    /// themselves stay alive (other clients may have them open), but this client's viewports,
    /// cursors, history, searches, scroll, and pickers/mru are reset.
    pub fn teardown_client_state_for_project(&mut self, client_id: ClientId, project_name: &str) {
        // Snapshot the buffer ids belonging to the project; we'll filter all the per-(client,
        // buffer) maps against this set. Avoids borrowing `buffers` while mutating the maps.
        let project_buffers: std::collections::HashSet<BufferId> = self
            .buffer_projects
            .iter()
            .filter_map(|(id, name)| (name == project_name).then_some(*id))
            .collect();

        let viewed: Vec<BufferId> = self
            .viewports
            .values()
            .filter(|v| v.client_id == client_id && project_buffers.contains(&v.buffer_id))
            .map(|v| v.buffer_id)
            .collect();
        self.viewports
            .retain(|_, v| !(v.client_id == client_id && project_buffers.contains(&v.buffer_id)));
        // A transient buffer the client was previewing doesn't survive leaving the project —
        // it's hidden now, same as switching buffers. (Permanent buffers stay alive for
        // re-entry, per the comment below.)
        let _ = self.close_orphaned_transients(viewed);
        let in_proj = |c: &ClientId, b: &BufferId| *c == client_id && project_buffers.contains(b);
        // Viewports + the live search state get torn down (they're transient view-layer
        // bookkeeping). Cursors / motion history / tree-selection / virtual-col / scroll are
        // *preserved* — they're the user's place-in-the-buffer memory, and re-attaching to a
        // buffer on project re-entry should restore them. The MRU is preserved for the same
        // reason: the buffer picker filters by active project, so cross-project MRU entries
        // don't leak into the UI, but they still let us reattach to "the buffer you last had"
        // when you come back.
        self.searches.retain(|(c, b), _| !in_proj(c, b));
        self.sneaks.retain(|(c, b), _| !in_proj(c, b));
        self.symbol_highlights.retain(|(c, b), _| !in_proj(c, b));
        self.symbol_highlight_gen.retain(|(c, b), _| !in_proj(c, b));

        // Pickers are per-session UI state — their candidate sets/queries reference the prior
        // project so wipe them all on switch.
        self.pickers.retain(|(c, _), _| *c != client_id);
    }
}

pub struct Buffer {
    pub id: BufferId,
    pub canonical_path: Option<PathBuf>,
    /// Small per-project display number for a scratch buffer (`(scratch N)`), assigned at creation
    /// as the lowest positive integer not in use by another scratch in the project — so the numbers
    /// stay small, stay stable for the buffer's life, and a freed number gets reused. `None` for
    /// file-backed buffers (which display their path instead).
    pub scratch_number: Option<u32>,
    pub text: ropey::Rope,
    pub revision: Revision,
    pub language: Option<String>,
    /// Derived: `revision != saved_revision`. Kept as a field for cheap reads.
    pub dirty: bool,
    pub line_ending: LineEnding,
    pub last_modified_unix_ms: Option<u64>,
    pub syntax: Option<BufferSyntax>,
    /// Detected (or defaulted) once on load; stable for the buffer's lifetime so further edits
    /// don't make the unit drift.
    pub indent_style: IndentStyle,
    /// Disk diverged while the buffer was dirty — the watcher couldn't silently reload. Set by
    /// the watcher, cleared by a successful save or a `buffer/reload`.
    pub externally_modified: bool,
    /// Buffer's on-disk file was removed externally. Set by the watcher, cleared by a save
    /// (which recreates the file) or by the file being recreated externally.
    pub externally_deleted: bool,
    /// Transient buffers auto-close once no viewport shows them anymore (see
    /// `viewport_subscribe` / `close_orphaned_transients`). Set at creation when the opening
    /// client asked for it (picker/goto-def navigation, the bootstrap scratch); cleared —
    /// "promoted" — by the buffer's first edit, a save, or a user-initiated reload. Never set
    /// again after creation.
    pub transient: bool,

    /// Revision at the most recent successful save. `None` only for a never-saved scratch
    /// buffer in its initial empty state — see `Buffer::scratch`.
    saved_revision: Option<Revision>,
    /// Source of fresh revision ids. Always strictly greater than any revision ever assigned.
    next_revision_id: u64,

    undo_stack: Vec<UndoEntry>,
    redo_stack: Vec<UndoEntry>,
    active_group: Option<ActiveGroup>,
}

pub struct BufferSyntax {
    pub config: &'static LanguageConfig,
    pub parser: Parser,
    pub tree: Tree,
    /// Embedded sub-language layers (e.g. fenced code blocks in markdown). Recomputed from
    /// scratch after every reparse — cheap for the number of fences in a typical file, and
    /// keeps the byte ranges synced with the parent tree without diff bookkeeping.
    pub injections: Vec<InjectionLayer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKindTag {
    Text,
    Delete,
    /// Surround/unsurround edits. Tagged distinctly so they never coalesce with an adjacent typing
    /// or delete burst into one undo group — each surround toggle is its own undo step.
    Surround,
    /// Whole-buffer formatting (`lsp/format`). Its own tag so a format is always a single undo
    /// step and never coalesces into an adjacent typing/delete burst.
    Format,
    /// Hunk revert (`git/apply_hunk`). Like `Format`: one revert, one undo step.
    Revert,
    /// Case transform (`input/transform_case`). Distinct so a recase is its own undo step and
    /// never folds into adjacent typing.
    Transform,
}

struct UndoEntry {
    rope: ropey::Rope,
    revision: Revision,
    cursors: std::collections::HashMap<ClientId, CursorState>,
}

struct ActiveGroup {
    last_edit_at: Instant,
    kind: EditKindTag,
}

pub struct UndoOutcome {
    pub new_revision: Revision,
    /// Cursor positions captured at the start of the rewound group. The undoing client uses
    /// theirs as the post-undo cursor; other clients clamp these or their existing positions
    /// to valid buffer offsets.
    pub restored_cursors: std::collections::HashMap<ClientId, CursorState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl Buffer {
    /// Load a buffer from disk. Detects line endings, normalizes to LF in-memory.
    pub fn load_from_file(id: BufferId, canonical: PathBuf) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(&canonical)?;
        let line_ending = if content.contains("\r\n") {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        };
        let normalized = if line_ending == LineEnding::Crlf {
            content.replace("\r\n", "\n")
        } else {
            content
        };
        let text = ropey::Rope::from_str(&normalized);
        let metadata = std::fs::metadata(&canonical).ok();
        let last_modified_unix_ms = metadata.and_then(|m| {
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
        });
        let language = detect_language(&canonical);
        let syntax = language
            .as_deref()
            .and_then(|name| make_syntax(&text, name));
        let indent_style = resolve_indent_style(&text, language.as_deref());
        Ok(Buffer {
            id,
            canonical_path: Some(canonical),
            scratch_number: None,
            text,
            revision: 0,
            language,
            dirty: false,
            line_ending,
            last_modified_unix_ms,
            syntax,
            indent_style,
            saved_revision: Some(0),
            next_revision_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_group: None,
            externally_modified: false,
            transient: false,
            externally_deleted: false,
        })
    }

    /// Empty buffer with a target file path attached but no file on disk yet. Used by
    /// `buffer/open` with `create_if_missing: true` — the file is created by `save_to_disk`
    /// on the first save. Language is auto-detected from the extension if not provided.
    pub fn new_at_path(id: BufferId, canonical: PathBuf, language: Option<String>) -> Self {
        let text = ropey::Rope::new();
        let language = language.or_else(|| detect_language(&canonical));
        let syntax = language
            .as_deref()
            .and_then(|name| make_syntax(&text, name));
        let indent_style = resolve_indent_style(&text, language.as_deref());
        Buffer {
            id,
            canonical_path: Some(canonical),
            scratch_number: None,
            text,
            revision: 0,
            language,
            dirty: false,
            line_ending: LineEnding::Lf,
            last_modified_unix_ms: None,
            syntax,
            indent_style,
            saved_revision: Some(0),
            next_revision_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_group: None,
            externally_modified: false,
            transient: false,
            externally_deleted: false,
        }
    }

    pub fn scratch(id: BufferId, language: Option<String>, scratch_number: u32) -> Self {
        let text = ropey::Rope::new();
        let syntax = language
            .as_deref()
            .and_then(|name| make_syntax(&text, name));
        let indent_style = resolve_indent_style(&text, language.as_deref());
        Buffer {
            id,
            canonical_path: None,
            scratch_number: Some(scratch_number),
            text,
            revision: 0,
            language,
            dirty: false,
            line_ending: LineEnding::Lf,
            last_modified_unix_ms: None,
            syntax,
            indent_style,
            // Treat empty scratch as "clean"; first edit makes it dirty.
            saved_revision: Some(0),
            next_revision_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_group: None,
            externally_modified: false,
            transient: false,
            externally_deleted: false,
        }
    }

    pub fn line_count(&self) -> u32 {
        // ropey counts lines as separated by \n; a trailing empty "line" after a final \n is
        // included. For protocol purposes we report ropey's count directly — clients see what
        // ropey sees.
        self.text.len_lines() as u32
    }

    /// Revision at the last successful save (or `0` for a fresh scratch buffer that's never been
    /// saved). The client uses this together with `revision` to derive `dirty`.
    pub fn saved_revision(&self) -> Revision {
        self.saved_revision.unwrap_or(0)
    }

    pub fn byte_count(&self) -> u64 {
        self.text.len_bytes() as u64
    }

    /// Apply a text replacement: remove `start_char..end_char`, insert `insert_text` at
    /// `start_char`. Bumps `revision`, marks dirty, updates the parse tree incrementally, and
    /// manages the undo group (opening a new entry if grouping conditions broke).
    ///
    /// `cursors_before_edit` is the per-client cursor map captured before this edit; it's
    /// stored in the undo entry when a new group opens, so `Buffer::undo` can restore cursors.
    pub fn apply_edit(
        &mut self,
        start_char: usize,
        end_char: usize,
        insert_text: &str,
        kind: EditKindTag,
        cursors_before_edit: std::collections::HashMap<ClientId, CursorState>,
    ) -> Revision {
        let now = Instant::now();

        // Decide whether to start a new undo group.
        let start_new_group = match &self.active_group {
            None => true,
            Some(g) => now.duration_since(g.last_edit_at) > GROUP_TIME_WINDOW || g.kind != kind,
        };
        if start_new_group {
            self.undo_stack.push(UndoEntry {
                rope: self.text.clone(),
                revision: self.revision,
                cursors: cursors_before_edit,
            });
            self.redo_stack.clear();
        }

        // Capture old byte positions for tree-sitter's InputEdit *before* mutating the rope.
        let edit_info = if self.syntax.is_some() {
            let start_byte = self.text.char_to_byte(start_char);
            let old_end_byte = self.text.char_to_byte(end_char);
            let start_position = rope_byte_to_point(&self.text, start_byte);
            let old_end_position = rope_byte_to_point(&self.text, old_end_byte);
            Some((start_byte, old_end_byte, start_position, old_end_position))
        } else {
            None
        };

        if start_char < end_char {
            self.text.remove(start_char..end_char);
        }
        if !insert_text.is_empty() {
            self.text.insert(start_char, insert_text);
        }
        self.revision = self.next_revision_id;
        self.next_revision_id += 1;
        self.active_group = Some(ActiveGroup {
            last_edit_at: now,
            kind,
        });
        self.recompute_dirty();

        if let Some((start_byte, old_end_byte, start_position, old_end_position)) = edit_info {
            let new_end_byte = start_byte + insert_text.len();
            let new_end_position = rope_byte_to_point(&self.text, new_end_byte);

            let text = &self.text;
            let syntax = self.syntax.as_mut().expect("just checked");
            syntax.tree.edit(&InputEdit {
                start_byte,
                old_end_byte,
                new_end_byte,
                start_position,
                old_end_position,
                new_end_position,
            });
            let parser = &mut syntax.parser;
            let tree = &mut syntax.tree;
            let new_tree = parser.parse_with_options(
                &mut |byte_idx: usize, _: Point| -> &[u8] {
                    if byte_idx >= text.len_bytes() {
                        return &[];
                    }
                    let (chunk, chunk_byte_start, _, _) = text.chunk_at_byte(byte_idx);
                    let bytes = chunk.as_bytes();
                    &bytes[byte_idx - chunk_byte_start..]
                },
                Some(&*tree),
                None,
            );
            if let Some(t) = new_tree {
                *tree = t;
            }
            // Injection layers are recomputed from scratch after every edit. Cheap relative to
            // the parse itself for typical fence counts, and the alternative (tracking which
            // layers were touched) would need its own diff bookkeeping.
            let source: String = text.chunks().collect();
            syntax.injections = syntax::compute_injections(syntax.config, &syntax.tree, &source);
        }

        self.revision
    }

    /// Write the buffer to disk atomically: write to `<dir>/.aether-tmp-<pid>-<name>`,
    /// fsync, rename onto `target`, fsync the parent directory. Restores CRLF if the buffer
    /// was loaded with CRLF endings. Updates `canonical_path`, `dirty`, `last_modified_unix_ms`.
    ///
    /// Returns the post-save mtime in unix milliseconds.
    pub fn save_to_disk(&mut self, target: PathBuf) -> std::io::Result<u64> {
        use std::io::Write;

        let mut text: String = self.text.chunks().collect();
        if self.line_ending == LineEnding::Crlf {
            text = text.replace('\n', "\r\n");
        }

        let parent = target.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "save target has no parent dir",
            )
        })?;
        let file_name = target
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("aether");
        let tmp_path = parent.join(format!(".aether-tmp-{}-{file_name}", std::process::id()));

        // Write to tmp.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);

        // Atomic rename.
        if let Err(e) = std::fs::rename(&tmp_path, &target) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }

        // Best-effort: fsync the parent directory so the rename is durable.
        #[cfg(unix)]
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }

        let canonical = std::fs::canonicalize(&target).unwrap_or(target);
        let mtime_ms = std::fs::metadata(&canonical)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        self.canonical_path = Some(canonical);
        self.last_modified_unix_ms = Some(mtime_ms);
        self.saved_revision = Some(self.revision);
        self.active_group = None;
        self.externally_modified = false;
        self.externally_deleted = false;
        self.recompute_dirty();
        Ok(mtime_ms)
    }

    /// Re-read this buffer's `canonical_path` from disk, replacing the rope, bumping the
    /// revision, and clearing undo/redo + external-change flags. The buffer comes back clean
    /// (saved_revision == revision). Indent style is preserved (stable for buffer lifetime).
    ///
    /// Errors if the buffer has no path or the file is unreadable.
    pub fn reload_from_disk(&mut self) -> std::io::Result<u64> {
        let path = self.canonical_path.clone().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "buffer has no path")
        })?;
        let content = std::fs::read_to_string(&path)?;
        let line_ending = if content.contains("\r\n") {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        };
        let normalized = if line_ending == LineEnding::Crlf {
            content.replace("\r\n", "\n")
        } else {
            content
        };
        let mtime_ms = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        self.text = ropey::Rope::from_str(&normalized);
        self.line_ending = line_ending;
        self.revision = self.next_revision_id;
        self.next_revision_id += 1;
        self.saved_revision = Some(self.revision);
        self.last_modified_unix_ms = Some(mtime_ms);
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.active_group = None;
        self.externally_modified = false;
        self.externally_deleted = false;
        self.recompute_dirty();
        // Re-parse from scratch — the incremental InputEdit path can't help when the whole rope
        // is replaced. Matches what undo/redo do.
        self.reparse_full();
        Ok(mtime_ms)
    }

    pub fn undo(
        &mut self,
        current_cursors: std::collections::HashMap<ClientId, CursorState>,
    ) -> Option<UndoOutcome> {
        let entry = self.undo_stack.pop()?;
        self.redo_stack.push(UndoEntry {
            rope: self.text.clone(),
            revision: self.revision,
            cursors: current_cursors,
        });
        self.text = entry.rope;
        self.revision = entry.revision;
        self.active_group = None;
        self.recompute_dirty();
        self.reparse_full();
        Some(UndoOutcome {
            new_revision: self.revision,
            restored_cursors: entry.cursors,
        })
    }

    pub fn redo(
        &mut self,
        current_cursors: std::collections::HashMap<ClientId, CursorState>,
    ) -> Option<UndoOutcome> {
        let entry = self.redo_stack.pop()?;
        self.undo_stack.push(UndoEntry {
            rope: self.text.clone(),
            revision: self.revision,
            cursors: current_cursors,
        });
        self.text = entry.rope;
        self.revision = entry.revision;
        self.active_group = None;
        self.recompute_dirty();
        self.reparse_full();
        Some(UndoOutcome {
            new_revision: self.revision,
            restored_cursors: entry.cursors,
        })
    }

    fn recompute_dirty(&mut self) {
        self.dirty = self.saved_revision != Some(self.revision);
    }

    /// Re-parse the entire buffer from scratch. Used after operations (undo/redo) that swap the
    /// whole rope — the incremental InputEdit pathway can't help when the buffer is replaced.
    fn reparse_full(&mut self) {
        if let Some(syntax) = self.syntax.as_mut() {
            let source: String = self.text.chunks().collect();
            if let Some(tree) = syntax.parser.parse(&source, None) {
                syntax.tree = tree;
                syntax.injections =
                    syntax::compute_injections(syntax.config, &syntax.tree, &source);
            }
        }
    }
}

fn detect_language(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    Some(
        match ext.to_ascii_lowercase().as_str() {
            "rs" => "rust",
            "toml" => "toml",
            "md" | "markdown" => "markdown",
            "json" => "json",
            "py" => "python",
            "js" | "mjs" | "cjs" | "jsx" => "javascript",
            "ts" => "typescript",
            "tsx" => "tsx",
            "go" => "go",
            "ex" | "exs" => "elixir",
            "erl" | "hrl" => "erlang",
            "yaml" | "yml" => "yaml",
            "html" | "htm" => "html",
            "css" => "css",
            "sh" | "bash" | "zsh" => "bash",
            _ => return None,
        }
        .to_string(),
    )
}

/// Pick the buffer's indent unit: detect from the text first, fall back to the language's
/// configured default, and to 2-space if even the language is unknown. Called once per buffer
/// load so subsequent edits don't shift the unit out from under the user.
fn resolve_indent_style(text: &ropey::Rope, language: Option<&str>) -> IndentStyle {
    if let Some(detected) = indent::detect_indent_style(text) {
        return detected;
    }
    if let Some(cfg) = language.and_then(syntax::get_config) {
        return cfg.default_indent;
    }
    IndentStyle::Spaces(2)
}

fn make_syntax(text: &ropey::Rope, language: &str) -> Option<BufferSyntax> {
    let config = syntax::get_config(language)?;
    let mut parser = syntax::make_parser(config);
    let source: String = text.chunks().collect();
    let tree = parser.parse(&source, None)?;
    let injections = syntax::compute_injections(config, &tree, &source);
    Some(BufferSyntax {
        config,
        parser,
        tree,
        injections,
    })
}

fn rope_byte_to_point(rope: &ropey::Rope, byte_idx: usize) -> Point {
    let char_idx = rope.byte_to_char(byte_idx);
    let line = rope.char_to_line(char_idx);
    let line_start_char = rope.line_to_char(line);
    let col_chars = char_idx - line_start_char;
    let line_slice = rope.line(line);
    let col_bytes = line_slice.char_to_byte(col_chars);
    Point {
        row: line,
        column: col_bytes,
    }
}

pub struct ClientSession {
    #[allow(dead_code)]
    pub client_id: ClientId,
    /// Channel for sending notifications to this client's connection task.
    pub outbound: mpsc::Sender<Notification>,
    /// The project this client is currently working in. `None` between connect and the first
    /// successful `project/activate`. Updated on every `project/activate`.
    pub active_project: Option<String>,
}

pub struct Viewport {
    pub id: ViewportId,
    pub buffer_id: BufferId,
    pub client_id: ClientId,
    pub cols: u32,
    pub rows: u32,
    pub overscan_rows: u32,
    pub scroll_logical_line: u32,
    pub scroll_sub_row: f32,
    pub wrap: WrapMode,
    pub continuation_marker_width: u32,
    pub tab_width: u32,
    /// First logical line currently pushed to the client (inclusive).
    pub first_logical_line: u32,
    /// Last logical line currently pushed to the client (exclusive).
    pub last_logical_line_exclusive: u32,
    /// Inline diff view: when on, rendered windows interleave phantom "deleted" rows from the
    /// buffer's Git hunks and the buffer's hunks are recomputed on every edit. Per-viewport so
    /// two views of the same buffer can differ. Toggled by `git/set_diff_view`.
    pub diff_view: bool,
}

impl Viewport {
    /// This viewport's wrap-layout inputs, bundled for the motion/render paths.
    pub fn wrap_geometry(&self) -> crate::wrap::WrapGeometry {
        crate::wrap::WrapGeometry {
            wrap: self.wrap,
            cols: self.cols,
            marker_width: self.continuation_marker_width,
            tab_width: self.tab_width,
        }
    }
}

#[cfg(test)]
mod project_state_tests {
    use super::*;

    fn project_entry(name: &str, paths: Vec<PathBuf>) -> ProjectEntry {
        ProjectEntry {
            id: name.to_string(),
            name: Some(name.to_string()),
            paths: paths.clone(),
            workspace_index: Arc::new(WorkspaceIndex::new(paths)),
            mru_buffers: VecDeque::new(),
            dormant_buffers: Vec::new(),
        }
    }

    fn session(active: &str) -> (ClientId, ClientSession) {
        let id = uuid::Uuid::new_v4();
        let (tx, _rx) = mpsc::channel::<Notification>(1);
        // Leak the receiver so the channel stays open for the test's lifetime.
        std::mem::forget(_rx);
        (
            id,
            ClientSession {
                client_id: id,
                outbound: tx,
                active_project: Some(active.to_string()),
            },
        )
    }

    /// The "rename while the project and its buffers are open" path: re-keys the project map (and
    /// updates `entry.name`), every buffer association, and every client's active-project pointer
    /// — while leaving buffers and unrelated projects untouched.
    #[test]
    fn rename_project_rekeys_buffers_and_clients() {
        let mut s = ServerState::new();
        s.projects.insert(
            "old".to_string(),
            project_entry("old", vec![PathBuf::from("/tmp/x")]),
        );
        s.projects
            .insert("other".to_string(), project_entry("other", vec![]));

        // A buffer in the renamed project, plus one in an unrelated project.
        let buf = s.allocate_buffer_id();
        s.buffer_projects.insert(buf, "old".to_string());
        let other_buf = s.allocate_buffer_id();
        s.buffer_projects.insert(other_buf, "other".to_string());

        let (c1, sess1) = session("old");
        s.clients.insert(c1, sess1);
        let (c2, sess2) = session("other");
        s.clients.insert(c2, sess2);

        let paths = s.rename_project("old", "new").expect("project was loaded");
        assert_eq!(paths, vec!["/tmp/x".to_string()]);

        // Project map re-keyed; the entry's own name field follows.
        assert!(!s.projects.contains_key("old"));
        assert_eq!(s.projects.get("new").map(|p| p.id.as_str()), Some("new"));
        assert_eq!(
            s.projects.get("new").and_then(|p| p.name.as_deref()),
            Some("new")
        );
        assert!(s.projects.contains_key("other"));

        // The buffer is re-pointed but still present (nothing closed); the unrelated one is left.
        assert_eq!(s.buffer_projects.get(&buf).map(String::as_str), Some("new"));
        assert_eq!(
            s.buffer_projects.get(&other_buf).map(String::as_str),
            Some("other")
        );

        // Only the matching client's active-project pointer follows the rename.
        assert_eq!(
            s.clients.get(&c1).unwrap().active_project.as_deref(),
            Some("new")
        );
        assert_eq!(
            s.clients.get(&c2).unwrap().active_project.as_deref(),
            Some("other")
        );
    }

    /// Renaming a project that isn't loaded returns `None` (the handler maps this to an internal
    /// error; it can't happen in practice since projects are never unloaded at runtime).
    #[test]
    fn rename_project_unknown_returns_none() {
        let mut s = ServerState::new();
        assert!(s.rename_project("nope", "new").is_none());
    }

    /// The paths persisted for a project's session: live MRU buffers first (most-recent-first),
    /// then dormant ones, deduplicated by path so a dormant entry that's since been loaded doesn't
    /// double-show.
    #[test]
    fn session_buffer_paths_merges_live_mru_then_dormant_deduped() {
        let mut s = ServerState::new();
        s.projects
            .insert("p".into(), project_entry("p", vec![PathBuf::from("/p")]));

        // Two live file-backed buffers; touch so the MRU front is b2.
        let b1 = s.allocate_buffer_id();
        s.buffers
            .insert(b1, Buffer::new_at_path(b1, PathBuf::from("/p/a.rs"), None));
        s.buffer_projects.insert(b1, "p".into());
        let b2 = s.allocate_buffer_id();
        s.buffers
            .insert(b2, Buffer::new_at_path(b2, PathBuf::from("/p/b.rs"), None));
        s.buffer_projects.insert(b2, "p".into());
        s.touch_mru(b1);
        s.touch_mru(b2);

        // A transient preview at the MRU front: must be excluded (previews don't persist).
        let bt = s.allocate_buffer_id();
        let mut tbuf = Buffer::new_at_path(bt, PathBuf::from("/p/preview.rs"), None);
        tbuf.transient = true;
        s.buffers.insert(bt, tbuf);
        s.buffer_projects.insert(bt, "p".into());
        s.touch_mru(bt);

        // Dormant: a fresh path, plus one that duplicates a live buffer's path (must be dropped).
        let d1 = s.allocate_buffer_id();
        let d_dup = s.allocate_buffer_id();
        s.projects.get_mut("p").unwrap().dormant_buffers = vec![
            DormantBuffer {
                id: d1,
                path: PathBuf::from("/p/c.rs"),
            },
            DormantBuffer {
                id: d_dup,
                path: PathBuf::from("/p/a.rs"),
            },
        ];

        assert_eq!(
            s.session_buffer_paths("p"),
            vec![
                // preview.rs (transient, MRU front) is excluded.
                PathBuf::from("/p/b.rs"), // most-recent non-transient
                PathBuf::from("/p/a.rs"),
                PathBuf::from("/p/c.rs"), // dormant; /p/a.rs dropped as a dup of the live buffer
            ]
        );
    }

    /// The dormant-registry helpers: `first_dormant_id` is the landing target (front of the list),
    /// `take_dormant` removes and returns a path by id (materialization), and `promote_dormant`
    /// drops a path once it's loaded.
    #[test]
    fn dormant_registry_take_promote_and_first() {
        let mut s = ServerState::new();
        s.projects
            .insert("p".into(), project_entry("p", vec![PathBuf::from("/p")]));
        let d1 = s.allocate_buffer_id();
        let d2 = s.allocate_buffer_id();
        s.projects.get_mut("p").unwrap().dormant_buffers = vec![
            DormantBuffer {
                id: d1,
                path: PathBuf::from("/p/a.rs"),
            },
            DormantBuffer {
                id: d2,
                path: PathBuf::from("/p/b.rs"),
            },
        ];

        assert_eq!(s.first_dormant_id("p"), Some(d1), "front of the list lands");
        assert_eq!(s.take_dormant("p", d1), Some(PathBuf::from("/p/a.rs")));
        assert_eq!(s.take_dormant("p", d1), None, "removed; a second take is empty");
        assert_eq!(s.first_dormant_id("p"), Some(d2));
        s.promote_dormant("p", Path::new("/p/b.rs"));
        assert_eq!(s.first_dormant_id("p"), None, "promotion empties the registry");
    }

    /// `project_active_anywhere` is the delete guard: true iff *some* client has the project
    /// active, so deleting it would pull the rug.
    #[test]
    fn project_active_anywhere_tracks_any_client() {
        let mut s = ServerState::new();
        let (c1, sess1) = session("alpha");
        s.clients.insert(c1, sess1);
        assert!(s.project_active_anywhere("alpha"));
        assert!(!s.project_active_anywhere("beta"));
    }

    /// An ephemeral project is retired only once it has no buffers *and* no client still has it
    /// active. This is the multi-client safety property: if a second client joined the ephemeral
    /// context (via the switcher), closing the first client's buffer must not delete the project
    /// out from under it.
    #[test]
    fn ephemeral_pruned_only_when_empty_and_inactive() {
        let mut s = ServerState::new();
        let id = s.register_ephemeral_project();
        assert!(s.projects.contains_key(&id));
        assert!(s.projects[&id].is_ephemeral());

        // A client is active in the context and it holds a buffer.
        let (client, sess) = session(&id);
        s.clients.insert(client, sess);
        let buf = s.allocate_buffer_id();
        s.buffer_projects.insert(buf, id.clone());
        assert!(!s.prune_ephemeral_if_empty(&id), "active + non-empty stays");

        // The buffer closes, but the client is still parked here → still not pruned (no rug-pull).
        s.buffer_projects.remove(&buf);
        assert!(
            !s.prune_ephemeral_if_empty(&id),
            "an active client keeps the context even with no buffers"
        );
        assert!(s.projects.contains_key(&id));

        // The client switches away → now it's both empty and inactive, so it's retired.
        s.clients.get_mut(&client).unwrap().active_project = Some("other".to_string());
        assert!(s.prune_ephemeral_if_empty(&id));
        assert!(!s.projects.contains_key(&id));
    }

    /// Ephemeral display numbers reuse the lowest free slot (like scratch numbers), so the picker
    /// shows small, stable `(project N)` labels rather than an ever-climbing counter.
    #[test]
    fn ephemeral_ids_reuse_the_lowest_free_number() {
        let mut s = ServerState::new();
        let a = s.register_ephemeral_project();
        let b = s.register_ephemeral_project();
        assert_eq!(a, "ephemeral/1");
        assert_eq!(b, "ephemeral/2");
        // Retire #1; the next mint reuses its number rather than climbing to 3.
        s.projects.remove(&a);
        let c = s.register_ephemeral_project();
        assert_eq!(c, "ephemeral/1");
    }

    /// Deleting a project drops its entry and closes exactly its buffers (tearing down their
    /// per-buffer state), leaving unrelated projects and their buffers intact.
    #[test]
    fn delete_project_closes_only_its_buffers() {
        let mut s = ServerState::new();
        s.projects.insert(
            "doomed".to_string(),
            project_entry("doomed", vec![PathBuf::from("/tmp/d")]),
        );
        s.projects
            .insert("keep".to_string(), project_entry("keep", vec![]));

        let buf_a = s.allocate_buffer_id();
        s.buffer_projects.insert(buf_a, "doomed".to_string());
        let buf_b = s.allocate_buffer_id();
        s.buffer_projects.insert(buf_b, "doomed".to_string());
        let survivor = s.allocate_buffer_id();
        s.buffer_projects.insert(survivor, "keep".to_string());
        // Per-buffer state that teardown must also clear (cursors keyed by (client, buffer)).
        let (client, sess) = session("keep");
        s.cursors.insert((client, buf_a), CursorState::default());
        s.cursors.insert((client, survivor), CursorState::default());
        s.clients.insert(client, sess);

        let mut closed = s.delete_project("doomed");
        closed.sort();
        let mut expected = vec![buf_a, buf_b];
        expected.sort();
        assert_eq!(closed, expected);

        // Entry gone; its buffers and their per-buffer state are gone; the unrelated ones remain.
        assert!(!s.projects.contains_key("doomed"));
        assert!(s.projects.contains_key("keep"));
        assert!(!s.buffer_projects.contains_key(&buf_a));
        assert!(!s.buffer_projects.contains_key(&buf_b));
        assert!(!s.cursors.contains_key(&(client, buf_a)));
        assert_eq!(
            s.buffer_projects.get(&survivor).map(String::as_str),
            Some("keep")
        );
        assert!(s.cursors.contains_key(&(client, survivor)));
    }

    /// `buffers_under_path` (the `path/delete` screen) matches a file exactly and matches a
    /// directory by path-prefix — component-wise, so `/ws/src` doesn't catch `/ws/srcfoo` — and is
    /// scoped to the named project.
    #[test]
    fn buffers_under_path_matches_file_and_dir_prefix() {
        let mut s = ServerState::new();
        s.projects.insert(
            "proj".to_string(),
            project_entry("proj", vec![PathBuf::from("/ws")]),
        );

        let add = |s: &mut ServerState, path: &str| -> BufferId {
            let id = s.allocate_buffer_id();
            s.buffers
                .insert(id, Buffer::new_at_path(id, PathBuf::from(path), None));
            s.buffer_projects.insert(id, "proj".to_string());
            id
        };
        let a = add(&mut s, "/ws/src/a.rs");
        let b = add(&mut s, "/ws/src/sub/b.rs");
        let _sibling = add(&mut s, "/ws/srcfoo/c.rs"); // not under /ws/src
        let _lib = add(&mut s, "/ws/lib/d.rs");

        let mut under = s.buffers_under_path("proj", Path::new("/ws/src"));
        under.sort();
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(
            under, expected,
            "directory prefix should match a.rs and sub/b.rs only"
        );

        assert_eq!(
            s.buffers_under_path("proj", Path::new("/ws/src/a.rs")),
            vec![a],
            "exact file path matches just that buffer"
        );
        assert!(
            s.buffers_under_path("other-proj", Path::new("/ws/src"))
                .is_empty(),
            "scoped to the named project"
        );
    }

    /// `next_scratch_number` returns the lowest positive integer not used by another scratch in the
    /// project: small, reuses freed numbers, ignores file buffers, and numbers projects apart.
    #[test]
    fn next_scratch_number_picks_lowest_unused_per_project() {
        let mut s = ServerState::new();
        assert_eq!(s.next_scratch_number("proj"), 1, "empty project → 1");

        let add_scratch = |s: &mut ServerState, n: u32| {
            let id = s.allocate_buffer_id();
            s.buffers.insert(id, Buffer::scratch(id, None, n));
            s.buffer_projects.insert(id, "proj".to_string());
            id
        };
        let s1 = add_scratch(&mut s, 1);
        add_scratch(&mut s, 2);
        // A file buffer (no scratch number) doesn't occupy a slot.
        let file = s.allocate_buffer_id();
        s.buffers.insert(
            file,
            Buffer::new_at_path(file, PathBuf::from("/p/a.rs"), None),
        );
        s.buffer_projects.insert(file, "proj".to_string());
        assert_eq!(s.next_scratch_number("proj"), 3, "1 and 2 used → 3");

        // Free #1 → it's reused rather than handing out 3.
        s.buffers.remove(&s1);
        s.buffer_projects.remove(&s1);
        assert_eq!(s.next_scratch_number("proj"), 1);

        // A different project numbers independently.
        assert_eq!(s.next_scratch_number("other"), 1);
    }

    /// `unsaved_buffer_count` counts only the dirty buffers belonging to the named project — the
    /// number the project picker shows. Clean buffers, buffers in other projects, and dangling
    /// associations (no buffer entry) don't count.
    #[test]
    fn unsaved_buffer_count_counts_dirty_buffers_per_project() {
        let mut s = ServerState::new();

        let add = |s: &mut ServerState, project: &str, dirty: bool| -> BufferId {
            let id = s.allocate_buffer_id();
            let mut buf = Buffer::scratch(id, None, 1);
            buf.dirty = dirty;
            s.buffers.insert(id, buf);
            s.buffer_projects.insert(id, project.to_string());
            id
        };

        add(&mut s, "alpha", true);
        add(&mut s, "alpha", true);
        add(&mut s, "alpha", false); // clean — not counted
        add(&mut s, "beta", true); // other project — not counted for alpha

        // A buffer_projects association with no live buffer (defensive: shouldn't panic / count).
        let dangling = s.allocate_buffer_id();
        s.buffer_projects.insert(dangling, "alpha".to_string());

        assert_eq!(s.unsaved_buffer_count("alpha"), 2);
        assert_eq!(s.unsaved_buffer_count("beta"), 1);
        assert_eq!(
            s.unsaved_buffer_count("never-loaded"),
            0,
            "a project with no buffers reports zero"
        );
    }
}
