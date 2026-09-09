//! Authoritative in-memory state owned by the server.

use crate::error::RpcError;
use crate::indent::{self, IndentStyle};
use crate::picker::{self as picker_state, PickerState};
use crate::syntax::{self, InjectionLayer, LanguageConfig};
use crate::workspace_index::WorkspaceIndex;
use aether_protocol::coords::ViewLine;
use aether_protocol::cursor::CursorState;
use aether_protocol::envelope::Notification;
use aether_protocol::lsp::SymbolCrumb;
use aether_protocol::picker::{MatchOptions, PickerKind};
use aether_protocol::viewport::{ScrollPosition, WrapMode};
use aether_protocol::{BufferId, ClientId, LogicalPosition, Revision, ViewId, ViewportId};
use std::time::{Duration, Instant};
use tree_sitter::{InputEdit, Parser, Point, Tree};

/// Edits within this window join the active undo group.
const GROUP_TIME_WINDOW: Duration = Duration::from_millis(500);
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

pub type SharedState = Arc<Mutex<ServerState>>;

pub struct ServerState {
    /// Loaded workspaces, keyed by workspace name. Populated lazily by `workspace/activate` — a workspace
    /// the user has configured but never activated is *not* here. Each entry owns the workspace's
    /// canonical paths and workspace index. Never removed at runtime (no workspace/unload concept);
    /// dropped only with the server.
    pub workspaces: HashMap<String, WorkspaceEntry>,
    /// File-system watcher for this server. `None` until [`crate::watcher::spawn`] runs — that
    /// happens during `run_with_listener`. `workspace/activate` reaches in to add new workspace roots
    /// once a workspace gets loaded. Per-server (not a global) so tests can spin up multiple servers
    /// in the same process without sharing watcher state.
    pub watcher: Option<Arc<crate::watcher::WatcherHandle>>,
    /// Repo working directories whose file events the watcher must ignore right now, because
    /// *we* are rewriting the tree there (a checkout, a stash pop, a pull).
    ///
    /// Two reasons, and the second is the load-bearing one. The obvious one: a checkout rewrites
    /// hundreds of files at once, and letting the per-file external-change path fire for each
    /// would reload and re-diff them one by one. The subtle one: `git/refresh` reports back which
    /// buffers reloaded, which diverged, and which vanished, so the caller can tell the user in
    /// one message — and if the watcher has already quietly done that work, the report comes back
    /// empty and the user is told nothing happened. Suppression is what makes the reconciliation
    /// pass the authoritative account of a tree move rather than a race against the watcher.
    ///
    /// Keyed by canonicalized workdir (a [`aether_protocol::git::RepoId`]); a path is suppressed
    /// if it sits under any entry. Held only for the duration of one operation.
    pub git_suppressed: std::collections::HashSet<PathBuf>,
    /// Repos whose open working-changes view the watcher has queued a rebuild for, and whether a
    /// drain task is already running.
    ///
    /// The view follows the working tree live, and the watcher speaks in single files: one save is
    /// several inotify events and a tool that rewrites the tree is hundreds, each of which would
    /// otherwise run its own `git diff` of everything. Events accumulate here instead and one
    /// debounced pass answers them all. Only ever non-empty while a view is actually open.
    pub working_changes_pending: std::collections::HashSet<PathBuf>,
    /// Whether [`crate::watcher::schedule_working_changes_refresh`]'s drain task is running; it
    /// owns [`Self::working_changes_pending`] until it finds it empty.
    pub working_changes_draining: bool,
    /// Long-running git operations currently in flight, keyed by canonicalized workdir — at most
    /// one per repo, since they all contend for the same refs anyway.
    ///
    /// Holds the cancel handle so `git/cancel` can reach a `git push` that is sitting in a TCP
    /// timeout. Only **user-initiated** operations are registered: the periodic fetcher runs
    /// unannounced and uncancellable, which is what keeps it out of the way.
    pub git_operations: HashMap<PathBuf, crate::git_cli::CancelHandle>,
    /// Serialises worktree creation and removal per **repo family**, keyed by canonicalized
    /// common dir. See [`Self::worktree_lock`].
    pub worktree_locks: HashMap<PathBuf, Arc<Mutex<()>>>,
    /// Repos currently diffed against a revision other than HEAD (`git/set_baseline`), keyed by
    /// canonicalized workdir. In memory only: this is an inspection mode ("what have I changed
    /// since I branched?"), not a preference — coming back to a restored session still diffing
    /// against a commit you set last week would be a surprise, not a convenience.
    pub git_baseline_choices: crate::git::BaselineChoices,
    pub buffers: HashMap<BufferId, Buffer>,
    /// Document content, shared by every buffer attached to the same file. A buffer is a
    /// workspace's *view* of a document (`Buffer::document`); the rope, undo history, dirty
    /// state, and parse live here so two workspaces holding the same path see the same pending
    /// changes. Keyed by [`DocumentId`] — server-internal, never on the wire. A document is
    /// dropped when its last buffer closes ([`Self::close_buffer`]).
    pub documents: HashMap<DocumentId, Document>,
    /// Which workspace each open buffer belongs to. Populated when a buffer is created
    /// (`buffer/open`) and looked up when scoping per-buffer state to a workspace (e.g. on
    /// `workspace/activate`, when tearing down a client's state for the previously active workspace).
    pub buffer_workspaces: HashMap<BufferId, String>,
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
    /// `(client, buffer)` pairs whose symbol highlights follow the cursor: while present, every
    /// cursor change re-arms the debounced refresh server-side (`lsp/document_highlight
    /// {active: true}` subscribes; `false` unsubscribes and clears). The client only speaks on
    /// mode/search transitions — never per move.
    pub symbol_highlight_follow: HashSet<(ClientId, BufferId)>,
    /// `(client, buffer)` pairs whose cursor-line blame follows the cursor (`git/set_blame_follow`).
    /// While present, cursor changes arm a debounced [`crate::handlers::spawn_blame_refresh`] that
    /// pushes `git/blame_changed` when the settled line's blame differs from the last push.
    pub blame_follow: HashSet<(ClientId, BufferId)>,
    /// Debounce generation for blame follow — same supersession scheme as
    /// [`Self::symbol_highlight_gen`].
    pub blame_follow_gen: HashMap<(ClientId, BufferId), u64>,
    /// The `(line, revision)` of the last `git/blame_changed` push per follower, so a settle that
    /// resolves to the same pair (e.g. the cursor moved within a line) pushes nothing. Cleared
    /// when the underlying blame is invalidated externally (HEAD change) so the label refreshes.
    pub blame_last_pushed: HashMap<(ClientId, BufferId), (u32, Revision)>,
    /// The status-bar breadcrumb last pushed to each `(client, buffer)`: the names+kinds of the
    /// outline symbols enclosing that client's cursor, outermost first. Purely a change-detector —
    /// the follow loop recomputes the chain on every cursor move but pushes only when it differs
    /// from this, so moving *within* a function is silent. Seeded by `viewport/subscribe` (which
    /// answers with the same value) and cleared on close/disconnect.
    pub symbol_path_sent: HashMap<(ClientId, BufferId), Vec<SymbolCrumb>>,
    /// Fan-in for cursor changes: [`crate::handlers::set_cursor`] (the single cursor write path)
    /// sends the key here whenever a *followed* cursor changes; the server's follow loop debounces
    /// and refreshes the cursor-tracking decorations. `None` only in unit tests that construct a
    /// bare state.
    pub cursor_moved_tx:
        Option<tokio::sync::mpsc::UnboundedSender<(ClientId, BufferId, DeferredToken)>>,
    /// Outstanding deferred work, so a caller can wait for the server to go quiet rather than
    /// guess at how long its debounces take. See [`Deferred`].
    pub deferred: Arc<Deferred>,
    /// Per-`(client, buffer)` last-known scroll position. Written whenever the client subscribes
    /// or scrolls a viewport on the buffer, and surfaced on `buffer/open` so the client can
    /// restore the view when it reopens the buffer (e.g. navigating away and back via the file
    /// browser). Cleared on disconnect.
    pub last_scroll: HashMap<(ClientId, BufferId), ScrollPosition>,
    /// Per-`(client, kind)` picker state. Survives `picker/hide` (so resume restores query +
    /// ranking); cleared on disconnect.
    pub pickers: HashMap<(ClientId, PickerKind), PickerState>,
    /// Per-client navigation history: back/forward across files, browser-style. (Distinct from
    /// the *jumplist* — the captured picker-results list, which is per *context* rather than per
    /// client and lives on [`WorkspaceEntry::jumplist`].)
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
    /// Repo-level Git status for a buffer that is *of* a repo without being a file in it: the
    /// generated patches and revision views `git/show` mints. There is no baseline to hang it off
    /// (no path, no blob, no index entry), but the status bar still has to say which checkout is
    /// in front of you — the branch indicator is how the editor says a repo is active at all.
    ///
    /// Resolved once, when the buffer is minted or regenerated, rather than per viewport window:
    /// these reads open the repo, and the window is built on the keystroke path.
    pub virtual_git_status: HashMap<BufferId, aether_protocol::git::GitBufferStatus>,
    /// Driver-built element layouts, keyed by the **view's** buffer. Present for a patch, whose
    /// elements window the real files it describes; absent for every view that is simply its own
    /// document. Dropped with the view.
    pub view_layouts: HashMap<BufferId, Vec<ElementLayout>>,
    /// Per-buffer conflict blocks, for the files a stopped merge or rebase left conflicted.
    /// Rescanned from the buffer's markers on the same triggers as the hunk caches, and **only
    /// while the baseline says the file is conflicted** — so an ordinary buffer never pays for the
    /// scan, and a file resolved outside the editor stops being decorated as soon as the watcher
    /// reloads its baseline. Absent means "not conflicted"; present-but-empty means "conflicted in
    /// the index, but every marker block has been resolved" — the state mark-resolved is waiting
    /// for.
    pub git_conflicts: HashMap<BufferId, Vec<crate::git::ConflictRegion>>,
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
    /// source for the workspace-wide `Space Alt-d` picker — independent of the buffer-keyed
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
    /// `workspace/activate` so they can detect a daemon restart across a reconnect. Set once at
    /// construction; the same value is written to the runtime file.
    pub started_at_unix_ms: u64,
    /// Where to read/write the persisted workspace-session file
    /// ([`crate::config::WorkspaceSessions`]). `Some` in the real server (set at boot in
    /// `server::run`); `None` everywhere else — in-process tests and embeddings leave it unset so
    /// they never touch the developer's real `~/.config/aether/sessions.json`. When `None`, session
    /// recency/restore is simply disabled (all logic short-circuits).
    pub sessions_path: Option<PathBuf>,
    /// Root directory for unsaved-buffer backups ([`crate::backup`]). `Some` in the real server
    /// (set at boot in `server::run`); `None` everywhere else — in-process tests and embeddings
    /// leave it unset so they never write backups to disk, and the idle reaper keeps its
    /// dirty-buffer guard (with backups off, reaping a dirty buffer would lose work).
    pub backups_path: Option<PathBuf>,
    /// Where app-managed git worktrees are created ([`crate::worktree::store_root`]). Same
    /// convention as [`Self::sessions_path`]: `Some` in the real server, `None` in tests and
    /// embeddings — except that here `None` means *fall back to the default location* rather than
    /// disable the feature, because a worktree with nowhere to go is not a worktree.
    ///
    /// It is a field rather than a plain call to the default so tests can point it at a tempdir.
    /// The alternative — an environment variable set per test — races: `set_var` is process-global
    /// and the suite runs in parallel.
    pub worktree_store: Option<PathBuf>,
    /// Where the workspace definitions (`<name>.toml`) live. Same convention as
    /// [`Self::settings_path`]: `None` means the profile's real workspaces directory, not
    /// "disabled" — workspace CRUD has to work in production.
    ///
    /// It exists so `workspace/list`, `/create`, `/rename`, `/add_project` and `/remove_project`
    /// can be tested at all. Without it a test would list, litter, or clobber the developer's own
    /// configured workspaces, which is why those handlers went untested.
    pub workspaces_dir: Option<PathBuf>,
    /// Where `settings/get` and `settings/set` read and write the app settings
    /// ([`aether_protocol::settings::AppSettings`]). Follows [`Self::worktree_store`]'s convention
    /// rather than [`Self::sessions_path`]'s: `None` means *the profile's real `settings.toml`*,
    /// not "disabled" — the settings RPCs have to work in production, and a client with no settings
    /// is not a thing.
    ///
    /// It exists so those two handlers can be tested at all. Every other persisted file already had
    /// an injectable path and this one did not, which is why it went untested: a test calling
    /// `settings/set` would have overwritten the developer's own editor settings.
    pub settings_path: Option<PathBuf>,
    /// Where to read/write the hint learning state ([`crate::config::HintsState`]). Same convention
    /// as [`Self::sessions_path`]: `Some` in the real server (and in tests that point it at a
    /// tempfile); `None` disables persistence — `hints/record` still aggregates in memory so a
    /// snapshot within one run stays coherent.
    pub hints_path: Option<PathBuf>,
    /// In-memory hint learning state, loaded from [`Self::hints_path`] at boot and
    /// flushed back by the periodic hints flush (dirty-flag debounced) plus a final flush on
    /// graceful shutdown.
    pub hints: crate::config::HintsState,
    /// Set by `hints/record` when [`Self::hints`] mutated; cleared by the flush that writes it.
    pub hints_dirty: bool,
    /// Where to read/write the input-history lists ([`crate::config::HistoryFile`]). Same
    /// convention as [`Self::hints_path`]: `Some` in the real server (and in tests that point it at
    /// a tempfile); `None` disables persistence — `history/record` still accumulates in memory so
    /// recall works within one run.
    pub history_path: Option<PathBuf>,
    /// In-memory input-history lists keyed by workspace, loaded from [`Self::history_path`] at
    /// boot and flushed back by the periodic flush (dirty-flag debounced) plus a final flush on
    /// graceful shutdown.
    pub history: crate::config::HistoryFile,
    /// Set when [`Self::history`] mutated; cleared by the flush that writes it.
    pub history_dirty: bool,
    /// The idle-reaper setting this instance was started with: `Some(d)` is a client-conjured
    /// server that self-reaps after `d` idle (see [`crate::server::idle_reaper`]); `None` is the
    /// persistent `ae server` daemon. Set once by `run_with_listener`; `None` until then. Read only
    /// to report it in the `/status` snapshot — the reaper itself owns the timeout separately.
    pub idle_timeout: Option<Duration>,
    /// The loopback port this instance actually bound, recorded by `run_with_listener` from the
    /// listener itself rather than from `profile.toml` — so it's the port clients are really talking
    /// to, including the ephemeral one an in-process test server gets. `None` until then.
    /// Diagnostic only (the `app/info` snapshot); nothing routes on it.
    pub port: Option<u16>,
    next_buffer_id: u64,
    next_viewport_id: u64,
    next_document_id: u64,
}

/// Server-internal identity of a [`Document`]. Never leaves the process — the protocol speaks
/// only [`BufferId`]s. A newtype (not a bare `u64`) so a document id can't be passed where a
/// buffer id belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DocumentId(pub u64);

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

/// One location in the navigation history: a buffer plus the cursor/selection to
/// restore. The path fields let a closed file be reopened; `buffer_id` is preferred while the
/// buffer is still open (and is the only handle a scratch buffer has).
#[derive(Clone, Debug, PartialEq)]
pub struct NavEntry {
    pub buffer_id: BufferId,
    pub path_index: Option<u32>,
    pub relative_path: Option<String>,
    /// [`VirtualSource::key`] when the entry is a materialised revision — a commit's patch or a
    /// file at a revision. The reopen handle for buffers that have no path: they were *generated*,
    /// not loaded, so without this a virtual buffer would die with its id and stepping back to a
    /// diff you followed a line out of would find nothing to return to.
    pub virtual_key: Option<String>,
    pub cursor: CursorState,
}

/// A client's back/forward navigation history. Browser semantics: a jump pushes onto `back` and clears
/// `forward`; stepping back/forward moves entries between the two and across the "current" cursor.
#[derive(Default)]
pub struct NavHistory {
    pub back: Vec<NavEntry>,
    pub forward: Vec<NavEntry>,
}

/// Cap on each direction of the nav history, mirroring `MOTION_HISTORY_CAP`'s "transient, not an
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

/// One workspace, loaded and ready to serve. Owns its canonical roots and workspace index (the
/// picker file cache). One per active workspace; lives in `ServerState::workspaces`, keyed by `id`.
///
/// Identity (`id`) is separate from the human name (`name`). A persisted workspace — one backed by a
/// `<name>.toml` on disk — has `name: Some(..)`, and its `id` equals that name. An *ephemeral*
/// workspace — synthesized to host files opened outside any configured workspace (`ae /path/to/file`,
/// open-from-path, goto-def into the stdlib) — has `name: None`, a generated reserved `id`, no
/// config on disk, and is auto-removed once its last buffer closes. So `name.is_some()` *is* the
/// persistence signal: there's no separate "ephemeral" flag that could fall out of sync.
pub struct WorkspaceEntry {
    /// Stable identity and `workspaces` map key: [`crate::worktree::context_id`] of this entry's
    /// name and bindings. For an *unbound* persisted workspace that is simply its name; for a bound
    /// one it is `<name>/<bindings>`; for an ephemeral workspace it's a generated token (see
    /// [`ServerState::ephemeral_workspace_id`]). None of the three can collide, because every form
    /// but the first contains a path separator, which `validate_workspace_name` forbids.
    ///
    /// Deriving identity from the bindings is what makes two clients with the same worktrees *join*
    /// one entry instead of racing to create two — and what makes "does this worktree already belong
    /// to another context?" a lookup rather than a case to handle.
    pub id: String,
    /// The persisted workspace name, or `None` for an ephemeral workspace. `Some` ⇔ a `<name>.toml`
    /// exists on disk ⇔ this workspace survives losing its last buffer.
    pub name: Option<String>,
    /// The workspace's **configured** roots — what [`Self::paths`] is a worktree remapping *of*.
    /// `None` when no repo is bound, in which case `paths` already are them.
    ///
    /// Kept here rather than re-read from the TOML on every rebind for two reasons: rebinding is
    /// interactive and a config read is disk I/O in the middle of it, and an in-memory workspace
    /// (tests, embeddings) has no TOML to read at all. It is also the only copy of the pre-remap
    /// shape once `paths` has been materialised.
    pub base_paths: Option<Vec<PathBuf>>,
    /// The worktree bindings this context is resolved against: repo family (common dir) → admin
    /// name. Empty for the base, which is not a different kind of thing — just the empty set.
    ///
    /// Held on the entry because it is what [`Self::paths`] was materialised *from*, and because the
    /// entry's own id is derived from it. Persisted separately, per context, under the workspace's
    /// session entry.
    pub worktrees: std::collections::BTreeMap<PathBuf, String>,
    /// Canonicalized workspace paths. Each is either a file or a directory. Read from the config for
    /// a persisted workspace; for an ephemeral one they're synthesized from the files it hosts (one
    /// per directory — see [`ServerState::adopt_ephemeral_root`]), which is bookkeeping for the
    /// pickers rather than a statement of trust.
    pub paths: Vec<PathBuf>,
    /// Workspace-wide candidate cache for this workspace. Walked lazily on first picker access;
    /// survives picker hide/show.
    pub workspace_index: Arc<WorkspaceIndex>,
    /// Most-recently-used buffers in this workspace, front = most-recent. Bumped on every
    /// `buffer/open` (fresh open, reopen, or attach-by-id) — so this is *focus* recency, not edit
    /// recency. Drives the buffer picker's empty-query ordering, and the `last_buffer_id` returned
    /// by `workspace/activate` (so re-attaching to a workspace drops the user on the buffer they
    /// last had, scratch or file alike).
    ///
    /// Lives on the workspace — not on the client — so it persists across client disconnects.
    /// A new TUI invocation gets a fresh `ClientId` but inherits the workspace's MRU.
    pub mru_buffers: VecDeque<BufferId>,
    /// Buffers restored from the persisted session ([`crate::config::WorkspaceSession`]) on
    /// activation but not yet loaded into memory — most-recently-used first, mirroring the order
    /// they were saved in. Each holds a reserved [`BufferId`] (its picker identity) and the file's
    /// canonical path; it carries no rope/syntax/LSP. The buffer picker lists them after the live
    /// buffers, rendered identically to them; opening one materializes a real buffer (see
    /// `buffer_open`'s by-id path) and drops it from here. Never contains a path that's also a
    /// live buffer in this workspace — promotion removes it.
    pub dormant_buffers: Vec<DormantBuffer>,
    /// This context's jumplist: the quickfix-style snapshot `jumplist/capture` takes of a picker's
    /// filtered results, stepped cursor-relative by `jumplist/step` (`]` / `[`). `None` until
    /// something is captured; replaced wholesale by the next capture.
    ///
    /// Lives on the entry — not on the client — for the same reason [`Self::mru_buffers`] does, and
    /// for one more. The reason it shares: a client has no durable identity, so a list hung off a
    /// `ClientId` dies with the window that captured it, and two shells attached to the same context
    /// each step a list the other can't see. The reason it doesn't: entries carry absolute paths, so
    /// a list that followed a worktree rebind would step you into the tree you just left. Because
    /// the `workspaces` map is keyed by [`crate::worktree::context_id`] — name *plus* bindings —
    /// storing it here makes both right at once: one list per tree, each surviving the client.
    pub jumplist: Option<crate::jumplist::Jumplist>,
    /// Projects declared by this workspace's config, whose language servers are pinned open while
    /// it's active. Flattened out of the config's nested `[[roots]]` form, so each carries the
    /// index of the root it was declared under.
    ///
    /// Held in memory — not re-read from disk when needed — because `workspace/add_root` and
    /// `remove_root` rewrite the config file wholesale from this entry. Anything they don't carry
    /// is silently erased on the next root edit.
    pub projects: Vec<crate::config::ProjectRef>,
}

/// A session-restored buffer that hasn't been loaded yet. See [`WorkspaceEntry::dormant_buffers`].
#[derive(Debug, Clone)]
pub struct DormantBuffer {
    /// Reserved id — the buffer's identity in the picker, so selecting it can route back through
    /// `buffer/open { buffer_id }`. Not present in `ServerState::buffers` until materialized.
    pub id: BufferId,
    /// What to materialize: a file (by path) or a scratch (by per-workspace number, whose unsaved
    /// content is restored from its backup).
    pub source: DormantSource,
}

/// The thing a [`DormantBuffer`] materializes into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DormantSource {
    /// A file-backed buffer; the canonical path to load when materialized.
    File(PathBuf),
    /// A scratch buffer that had unsaved content; its per-workspace display number (and backup key).
    Scratch { number: u32 },
    /// A materialised revision — a commit's diff, or a file at one — by its [`VirtualSource::key`].
    /// Nothing is stored: the content regenerates from the repo when the buffer is first viewed,
    /// which is also why a revision that has since been rewritten away simply doesn't come back.
    Virtual { key: String },
}

impl DormantBuffer {
    /// The canonical path, for a file dormant buffer; `None` for a scratch.
    pub fn path(&self) -> Option<&Path> {
        match &self.source {
            DormantSource::File(p) => Some(p.as_path()),
            DormantSource::Scratch { .. } | DormantSource::Virtual { .. } => None,
        }
    }
}

impl WorkspaceEntry {
    /// True iff the given canonical path falls under one of this workspace's roots. A file that
    /// isn't contained is a *guest* — no git baseline, no language server (`buffer_open`). Note this
    /// is containment, not trust: an ephemeral workspace's own files are contained (it roots itself
    /// at their directories) and still get no language server.
    pub fn contains(&self, canonical: &Path) -> bool {
        self.paths
            .iter()
            .any(|p| canonical == p || canonical.starts_with(p))
    }

    /// True iff `canonical` is eligible for Git integration (baseline, gutter, hunk staging):
    /// contained by a root, **or** inside the working tree of a repo one of those roots reaches.
    ///
    /// The second clause matters when a root is a *subdirectory* of its repo. Its siblings are
    /// still the same working tree: open one — by absolute path, or by following a definition —
    /// and a gutter reading "no history" would simply be wrong, and `ApplyHunkStatus::Unavailable`
    /// wronger. Eligibility follows the repo, not the root list. (The workspace changes picker
    /// deliberately doesn't *list* those files — that's a git question, not a workspace one — so
    /// this is about what a buffer can do once you have it, not about what the picker offers.)
    ///
    /// Deliberately *not* "discover a repo from the file's own path": that walks upward out of the
    /// workspace and would make every dependency checkout (`~/.cargo/git/checkouts/…`, reached by
    /// goto-definition) a git target. Discovery only ever runs from a root, so an unreachable repo
    /// stays unreachable. The containment test runs first, so the ordinary in-root open costs no
    /// git work at all.
    pub fn git_eligible(&self, canonical: &Path) -> bool {
        self.contains(canonical)
            || self.paths.iter().any(|root| {
                crate::git::discover_repo(root)
                    .is_some_and(|identity| canonical.starts_with(&identity.workdir))
            })
    }

    /// Ephemeral ⇔ not persisted ⇔ no on-disk config. The single source of truth is `name.is_none()`.
    pub fn is_ephemeral(&self) -> bool {
        self.name.is_none()
    }

    /// The workspace's **configured** roots: [`Self::base_paths`] when a worktree binding has
    /// remapped [`Self::paths`], else `paths` themselves.
    ///
    /// **This — never `paths` — is what gets written to the workspace TOML.** `paths` is a
    /// materialisation: for a bound workspace it holds paths inside the app-managed worktree store,
    /// which are machine state and must never reach the hand-editable config. Writing them there
    /// replaces the workspace's own definition with a checkout that `git worktree remove` can
    /// delete, and unbinding then has nothing to fall back to.
    ///
    /// The two lists are positionally identical by construction — materialisation preserves root
    /// count and order (`crate::worktree::materialise_roots`) — so an index into one indexes the
    /// other, which is what lets `ProjectRef::root_index` survive a binding.
    pub fn configured_paths(&self) -> &[PathBuf] {
        self.base_paths.as_deref().unwrap_or(&self.paths)
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
            workspaces: HashMap::new(),
            watcher: None,
            git_suppressed: std::collections::HashSet::new(),
            working_changes_pending: std::collections::HashSet::new(),
            working_changes_draining: false,
            git_operations: HashMap::new(),
            worktree_locks: HashMap::new(),
            git_baseline_choices: crate::git::BaselineChoices::new(),
            buffers: HashMap::new(),
            documents: HashMap::new(),
            buffer_workspaces: HashMap::new(),
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
            symbol_highlight_follow: HashSet::new(),
            blame_follow: HashSet::new(),
            blame_follow_gen: HashMap::new(),
            blame_last_pushed: HashMap::new(),
            symbol_path_sent: HashMap::new(),
            cursor_moved_tx: None,
            deferred: Arc::new(Deferred::default()),
            last_scroll: HashMap::new(),
            pickers: HashMap::new(),
            nav_history: HashMap::new(),
            git_unstaged_hunks: HashMap::new(),
            git_both_hunks: HashMap::new(),
            git_baseline: HashMap::new(),
            virtual_git_status: HashMap::new(),
            view_layouts: HashMap::new(),
            git_conflicts: HashMap::new(),
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
            backups_path: None,
            worktree_store: None,
            workspaces_dir: None,
            settings_path: None,
            hints_path: None,
            hints: crate::config::HintsState::default(),
            hints_dirty: false,
            history_path: None,
            history: crate::config::HistoryFile::default(),
            history_dirty: false,
            idle_timeout: None,
            port: None,
            next_buffer_id: 1,
            next_viewport_id: 1,
            next_document_id: 1,
        }
    }

    /// The document a buffer views. Panics on an unknown buffer or a dangling document reference,
    /// mirroring the `self.buffers[&id]` indexing convention at existing call sites — both are
    /// server-internal invariant violations, not recoverable conditions.
    pub fn doc_of(&self, buffer_id: BufferId) -> &Document {
        &self.documents[&self.buffers[&buffer_id].document]
    }

    /// Mutable [`Self::doc_of`].
    pub fn doc_of_mut(&mut self, buffer_id: BufferId) -> &mut Document {
        let doc_id = self.buffers[&buffer_id].document;
        self.documents
            .get_mut(&doc_id)
            .expect("buffer references a live document")
    }

    /// A driver-built layout for this view, if one exists — elements bound to real buffers rather
    /// than to slices of the view's own document. Set when a patch is generated; consulted first by
    /// [`Self::element_layout_of`].
    pub fn set_view_layout(&mut self, buffer_id: BufferId, layout: Vec<ElementLayout>) {
        self.view_layouts.insert(buffer_id, layout);
    }

    /// The regions a document divides into, with the chrome that introduces each.
    ///
    /// A generated patch carries its own table — the chrome between files and hunks is what splits
    /// it — and every other document is one region spanning the whole buffer with no chrome. One
    /// function for both, so callers never branch on "is this a patch", and one place where a
    /// region's chrome is paired with its extent so the two cannot drift.
    pub fn element_layout_of(&self, buffer_id: BufferId) -> Vec<ElementLayout> {
        if let Some(layout) = self.view_layouts.get(&buffer_id) {
            return layout
                .iter()
                .map(|l| ElementLayout {
                    extent: l.extent.clone(),
                    chrome_above: l.chrome_above.clone(),
                    decorations: l.decorations.clone(),
                })
                .collect();
        }
        let doc = self.doc_of(buffer_id);
        match doc.generated.as_ref() {
            Some(g) if !g.decorations.elements.is_empty() => g
                .decorations
                .elements
                .iter()
                .map(|e| ElementLayout {
                    extent: ElementExtent::OwnDocument {
                        lines: e.start_line..e.end_line,
                    },
                    decorations: None,
                    chrome_above: std::sync::Arc::new(
                        g.decorations
                            .chrome
                            .get(e.start_line as usize)
                            .cloned()
                            .unwrap_or_default(),
                    ),
                })
                .collect(),
            _ => vec![ElementLayout {
                extent: ElementExtent::OwnDocument {
                    lines: 0..doc.text.len_lines() as u32,
                },
                decorations: None,
                chrome_above: std::sync::Arc::new(Vec::new()),
            }],
        }
    }

    /// Move every element windowing `buffer_id` to account for an edit that changed its line count.
    ///
    /// An element's extent comes from a diff — "this hunk is lines 16..23 of a.rs" — but the buffer
    /// it indexes is live, and editing a hunk is the whole point of the working-changes view. Left
    /// alone, the extent describes the file as it *was*: type a line into a hunk and its last line
    /// scrolls out of a window that still claims the old height, and every later hunk of that file
    /// is off by one.
    ///
    /// Three cases, and they are the same three a diff would produce:
    /// - the edit lands **inside** an element → that element grows or shrinks,
    /// - the edit lands **above** it → the whole element slides,
    /// - the edit lands **below** it → nothing changes.
    ///
    /// Only for edits. Undo, redo and reload replace the rope wholesale, where no single position
    /// describes what moved; those rely on [`ViewLayout::of`]'s clamp against the live buffer and on
    /// the rebuild a save/stage/commit performs.
    pub fn shift_element_extents(&mut self, buffer_id: BufferId, shift: LineShift) {
        let siblings = self.doc_siblings(buffer_id);
        for layout in self.view_layouts.values_mut() {
            for element in layout {
                // `bound_to`, not "whichever buffer this resolves to": an `OwnDocument` extent
                // indexes the view's *generated* document, which no edit reaches — a patch is
                // read-only — so its lines never move even when the file it was rendered from does.
                // Asking the extent rather than a nullable id is what says so out loud.
                if element.extent.bound_to().is_some_and(|id| siblings.contains(&id)) {
                    element.extent.shift(shift.at, shift.delta);
                }
            }
        }
        for vp in self.viewports.values_mut() {
            for element in &mut vp.elements {
                if siblings.contains(&element.buffer_id) {
                    if shift.at < element.start_line {
                        element.start_line = element.start_line.saturating_add_signed(shift.delta);
                        element.end_line_exclusive =
                            element.end_line_exclusive.saturating_add_signed(shift.delta);
                    } else if shift.at < element.end_line_exclusive {
                        element.end_line_exclusive =
                            element.end_line_exclusive.saturating_add_signed(shift.delta);
                    }
                }
            }
        }
    }

    /// Rewrite a generated document in place, re-deriving the element bindings of every viewport
    /// showing it.
    ///
    /// The two belong together: a rebuilt patch has different regions — files appear and vanish as
    /// you stage — so bindings minted at subscribe would describe a document that no longer exists.
    pub fn replace_generated(
        &mut self,
        buffer_id: BufferId,
        text: &str,
        generated: Option<crate::patch::GeneratedPatch>,
    ) {
        self.doc_of_mut(buffer_id)
            .replace_generated(text, generated);
        // A rebuilt patch has different regions *and* different stages — staging is the only thing
        // this view shows changing — so a driver-built layout is rebuilt from the new data, or the
        // view keeps rendering the stages it was opened with. Resolution is by path against buffers
        // already open: this runs on the save/stage/commit refresh, where opening files would be
        // both surprising and asynchronous.
        let rebuilt = self.view_layouts.contains_key(&buffer_id).then(|| {
            let workspace = self.buffer_workspaces.get(&buffer_id).cloned();
            let doc = self.doc_of(buffer_id);
            let repo_id = doc
                .virtual_source
                .as_ref()
                .map(|v| v.target.repo_id.clone())
                .unwrap_or_default();
            doc.generated.as_ref().map(|generated| {
                crate::patch::layout_over_files(generated, |path| {
                    let workspace = workspace.as_deref()?;
                    let canonical = std::path::Path::new(&repo_id).join(path);
                    self.buffer_for_path_in_workspace(workspace, &canonical)
                })
            })
        });
        if let Some(Some(rebuilt)) = rebuilt {
            self.view_layouts.insert(buffer_id, rebuilt);
        }
        let layout = self.element_layout_of(buffer_id);
        for vp in self.viewports.values_mut() {
            if !vp.binds(buffer_id) {
                continue;
            }
            let (cols, marker) = (vp.focus().cols, vp.focus().continuation_marker_width);
            vp.elements = layout
                .iter()
                .map(|l| l.bind(buffer_id, cols, marker))
                .collect();
        }
    }

    /// Non-panicking [`Self::doc_of`], for paths where the buffer may already be gone.
    pub fn try_doc_of(&self, buffer_id: BufferId) -> Option<&Document> {
        self.documents.get(&self.buffers.get(&buffer_id)?.document)
    }

    /// Non-panicking [`Self::doc_of_mut`].
    pub fn try_doc_of_mut(&mut self, buffer_id: BufferId) -> Option<&mut Document> {
        let doc_id = self.buffers.get(&buffer_id)?.document;
        self.documents.get_mut(&doc_id)
    }

    /// The document a buffer views, bounded to the window the client's focused element shows of it
    /// — the only way to reach [`crate::cursor::Scope`], and so the only way to resolve a motion.
    ///
    /// Motions are element-local (see [`crate::cursor::Scope`] for why), and this is where that is
    /// decided, once, for every one of them. The handlers that used to clamp a motion's result by
    /// hand — and the many that never did — now all ask the same question here.
    ///
    /// The extent applies only when the focused element windows the buffer being addressed. A client
    /// acting on a view while focus sits on an element windowing a *file* is naming two different
    /// line spaces, and bounding one by the other yields a line belonging to neither; the honest
    /// answer there is the whole document.
    pub fn motion_scope(
        &self,
        client_id: ClientId,
        buffer_id: BufferId,
    ) -> Result<crate::cursor::Scope<'_>, RpcError> {
        let doc = self
            .try_doc_of(buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
        let element = self
            .viewports
            .values()
            .find(|v| v.client_id == client_id && v.binds(buffer_id))
            .map(|v| v.focus())
            .filter(|e| e.buffer_id == buffer_id);
        Ok(match element {
            Some(e) => crate::cursor::Scope::windowed(doc, e.start_line, e.end_line_exclusive),
            None => crate::cursor::Scope::whole(doc),
        })
    }

    /// The document a buffer views, as something that may be **mutated** — the only way to reach
    /// [`Editable`], and so the only way to change a document's text. Refuses a buffer that isn't
    /// there and a document that doesn't accept edits, which folds the two checks every edit
    /// handler opens with into one call.
    pub fn editable_doc(&mut self, buffer_id: BufferId) -> Result<Editable<'_>, RpcError> {
        let doc = self
            .try_doc_of_mut(buffer_id)
            .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
        if doc.read_only() {
            return Err(RpcError::read_only_buffer(buffer_id));
        }
        Ok(Editable(doc))
    }

    /// The live document loaded from `canonical`, if any — the cross-workspace sharing lookup.
    pub fn document_for_path(&self, canonical: &Path) -> Option<DocumentId> {
        self.documents
            .values()
            .find(|d| d.canonical_path.as_deref() == Some(canonical))
            .map(|d| d.id)
    }

    /// Every buffer attached to `doc` — the edit fan-out audience. One entry per workspace
    /// holding the document open.
    pub fn buffers_of_document(&self, doc: DocumentId) -> Vec<BufferId> {
        self.buffers
            .iter()
            .filter(|(_, b)| b.document == doc)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Buffer ids attached to the same document as `buffer_id`, including itself — the audience
    /// a mutation through any one buffer must fan out to. Just `[buffer_id]` for an unknown
    /// buffer, so per-buffer teardown paths stay well-defined mid-close.
    pub fn doc_siblings(&self, buffer_id: BufferId) -> Vec<BufferId> {
        match self.buffers.get(&buffer_id) {
            Some(b) => self.buffers_of_document(b.document),
            None => vec![buffer_id],
        }
    }

    pub fn allocate_document_id(&mut self) -> DocumentId {
        let id = DocumentId(self.next_document_id);
        self.next_document_id += 1;
        id
    }

    /// Insert a fresh buffer over its own new document (the 1:1 case): allocates the
    /// [`DocumentId`], builds the document via `make_doc`, and stores both halves. Returns the
    /// document id so callers can reach the content for follow-up tweaks.
    pub fn insert_buffer_with_document(
        &mut self,
        buffer_id: BufferId,
        scratch_number: Option<u32>,
        transient: bool,
        make_doc: impl FnOnce(DocumentId) -> Document,
    ) -> DocumentId {
        let doc_id = self.allocate_document_id();
        self.documents.insert(doc_id, make_doc(doc_id));
        self.buffers.insert(
            buffer_id,
            Buffer {
                id: buffer_id,
                document: doc_id,
                scratch_number,
                transient,
            },
        );
        doc_id
    }

    /// Look up a loaded workspace by name. Returns `None` if the workspace hasn't been activated by
    /// any client yet (or doesn't exist).
    pub fn workspace(&self, name: &str) -> Option<&WorkspaceEntry> {
        self.workspaces.get(name)
    }

    /// The workspace the given client currently has activated, if any.
    pub fn active_workspace(&self, client_id: ClientId) -> Option<&WorkspaceEntry> {
        let session = self.clients.get(&client_id)?;
        let name = session.active_workspace.as_deref()?;
        self.workspaces.get(name)
    }

    /// Same as [`Self::active_workspace`] but surfaces a `NO_ACTIVE_WORKSPACE` `RpcError` for the
    /// common handler pattern of "require an active workspace or bail." Most non-`workspace/*`
    /// handlers want this.
    pub fn active_workspace_or_err(
        &self,
        client_id: ClientId,
    ) -> Result<&WorkspaceEntry, crate::error::RpcError> {
        self.active_workspace(client_id)
            .ok_or_else(crate::error::RpcError::no_active_workspace)
    }

    /// The workspace the given client currently has activated, mutably.
    pub fn active_workspace_mut(&mut self, client_id: ClientId) -> Option<&mut WorkspaceEntry> {
        let id = self.clients.get(&client_id)?.active_workspace.clone()?;
        self.workspaces.get_mut(&id)
    }

    /// The jumplist the given client steps: the one belonging to the context it is standing in.
    /// See [`WorkspaceEntry::jumplist`] for why the client is a lens onto the context's list rather
    /// than the owner of a list of its own. `None` with no active workspace, or nothing captured
    /// there yet.
    pub fn jumplist(&self, client_id: ClientId) -> Option<&crate::jumplist::Jumplist> {
        self.active_workspace(client_id)?.jumplist.as_ref()
    }

    /// Replace the active context's jumplist. Silently drops the list when the client has no active
    /// workspace — a capture can only come from a picker, which needs one.
    pub fn set_jumplist(&mut self, client_id: ClientId, list: crate::jumplist::Jumplist) {
        if let Some(entry) = self.active_workspace_mut(client_id) {
            entry.jumplist = Some(list);
        }
    }

    /// Id of the workspace a buffer belongs to. `None` if the buffer is unknown or somehow
    /// untagged (shouldn't happen for live buffers but the lookup is defensive).
    pub fn workspace_for_buffer(&self, buffer_id: BufferId) -> Option<&str> {
        self.buffer_workspaces.get(&buffer_id).map(|s| s.as_str())
    }

    /// The mutual-exclusion lock for worktree creation and removal in one repo **family**.
    ///
    /// `git worktree add` rewrites `.git/config`, which lives in the *common* dir and is guarded by
    /// git's own lockfile: two concurrent adds in one family leave one dead with `could not lock
    /// config file`. [`ServerState::git_operations`] is the wrong tool — it is a *cancellation
    /// registry*, not a lock, and it is keyed by workdir, which every worktree of a family has a
    /// different one of while sharing the config being written.
    ///
    /// The value is a mutex rather than a busy flag so a second caller **waits** instead of being
    /// refused. Two agents asking for a worktree at the same moment is the expected case, not an
    /// error the user should have to understand and retry.
    ///
    /// Take the `Arc` under the state lock, drop the state guard, *then* await the mutex — holding
    /// the whole server's state across a checkout would stall every other client:
    ///
    /// ```ignore
    /// let lock = state.lock().await.worktree_lock(&common_dir);
    /// let _guard = lock.lock().await;
    /// ```
    ///
    /// Entries are never pruned: one `PathBuf` plus an `Arc` per repo family the process has
    /// touched, in a daemon that idle-reaps anyway.
    pub fn worktree_lock(&mut self, common_dir: &Path) -> Arc<Mutex<()>> {
        self.worktree_locks
            .entry(common_dir.to_path_buf())
            .or_default()
            .clone()
    }

    /// The display number for a *new* ephemeral workspace: the lowest positive integer not in use by
    /// another live ephemeral workspace. Mirrors [`Self::next_scratch_number`] — numbers stay small
    /// and a freed one is reused once its workspace is pruned — so the picker shows `(workspace 1)`,
    /// `(workspace 2)`, … rather than an ever-climbing counter. Because ephemeral workspaces are pruned
    /// the moment they empty, the lowest-free number is always unique among the live set, so it
    /// doubles as the id suffix.
    fn next_ephemeral_number(&self) -> u32 {
        let used: std::collections::HashSet<u32> = self
            .workspaces
            .values()
            .filter_map(|p| {
                p.id.strip_prefix(aether_protocol::EPHEMERAL_WORKSPACE_PREFIX)
                    .and_then(|n| n.parse().ok())
            })
            .collect();
        (1..)
            .find(|n| !used.contains(n))
            .expect("u32 range is non-empty")
    }

    /// Mint a fresh ephemeral-workspace id, `ephemeral/<n>`. The `/` can never appear in a valid
    /// workspace name (`validate_workspace_name` rejects separators), so the id never collides with a
    /// persisted workspace or resolves to an on-disk config path; `<n>` is the small reusable display
    /// number (see [`Self::next_ephemeral_number`]).
    pub fn ephemeral_workspace_id(&mut self) -> String {
        let n = self.next_ephemeral_number();
        format!("{}{n}", aether_protocol::EPHEMERAL_WORKSPACE_PREFIX)
    }

    /// Register a fresh, rootless, nameless workspace and return its id. The caller activates it for
    /// a client and opens a buffer in it; it is auto-removed when that last buffer closes (see
    /// [`Self::prune_ephemeral_if_empty`]).
    pub fn register_ephemeral_workspace(&mut self) -> String {
        let id = self.ephemeral_workspace_id();
        let workspace_index = Arc::new(WorkspaceIndex::new(Vec::new()));
        self.workspaces.insert(
            id.clone(),
            WorkspaceEntry {
                worktrees: Default::default(),
                id: id.clone(),
                name: None,
                base_paths: None,
                paths: Vec::new(),
                // No config file, so no configured roots to remap — an ephemeral context can bind
                // nothing, which `workspace/bind_worktree` refuses outright rather than silently
                // recording something that could never resolve.
                workspace_index,
                mru_buffers: VecDeque::new(),
                dormant_buffers: Vec::new(),
                jumplist: None,
                // An ephemeral workspace has no config file, so nothing can declare a project in it.
                projects: Vec::new(),
            },
        );
        id
    }

    /// Drop an ephemeral workspace once it holds no buffers and no client still has it active. A
    /// no-op for persisted workspaces and for ephemeral ones that still host a buffer. Call after any
    /// buffer close so an ephemeral workspace's lifetime is exactly "while it has a buffer".
    /// Returns `true` if the workspace was removed.
    pub fn prune_ephemeral_if_empty(&mut self, workspace_id: &str) -> bool {
        let is_ephemeral = self
            .workspaces
            .get(workspace_id)
            .is_some_and(|p| p.is_ephemeral());
        if !is_ephemeral {
            return false;
        }
        if self.buffers_in_workspace(workspace_id).is_empty()
            && !self.workspace_active_anywhere(workspace_id)
        {
            self.workspaces.remove(workspace_id);
            return true;
        }
        false
    }

    /// Retire an ephemeral workspace the moment it loses its *last buffer*: remove it and clear it
    /// from any client still parked in it. Unlike [`Self::prune_ephemeral_if_empty`] — which keeps
    /// an empty context alive while a client has it active — this *evicts* those clients (their
    /// `active_workspace` becomes `None`), because an ephemeral context with no files has no reason
    /// to linger in the switcher even if a second client had selected it. Call after a user-driven
    /// `buffer/close`; the evicted clients are being told the buffer closed (`buffer/closed`) and
    /// drop to the chooser. Returns whether the workspace was removed.
    pub fn retire_ephemeral_if_empty(&mut self, workspace_id: &str) -> bool {
        let is_ephemeral = self
            .workspaces
            .get(workspace_id)
            .is_some_and(|p| p.is_ephemeral());
        if !is_ephemeral || !self.buffers_in_workspace(workspace_id).is_empty() {
            return false;
        }
        for s in self.clients.values_mut() {
            if s.active_workspace.as_deref() == Some(workspace_id) {
                s.active_workspace = None;
            }
        }
        self.workspaces.remove(workspace_id);
        true
    }

    /// Give an ephemeral workspace a root for what it's about to host — the **parent directory** of
    /// a file it opens, or a directory opened as a context in its own right (`ae ~/notes`, where the
    /// caller passes the directory itself). Returns whether a root was added.
    ///
    /// A rootless workspace can't answer any file-oriented question — the Files index walks roots,
    /// grep walks roots, the explorer refuses to open without one, and every picker row is addressed
    /// as `(path_index, relative_path)`, which needs a root to be relative *to*. So a temporary
    /// context used to have dead pickers. The parent directory is the smallest root that contains
    /// the file you opened: bounded, predictable, and already the directory the explorer should land
    /// in. Opening a second file from elsewhere into the same context appends its parent (a temp
    /// context is multi-root like any other); one already under an existing root adds nothing.
    ///
    /// Deliberately *not* the enclosing project root (an upward walk for `.git` / `Cargo.toml`): a
    /// dotfiles repo in `$HOME` would silently root the context at the home directory and hand the
    /// Files picker the whole tree. Widening to the project is a decision to make explicitly.
    ///
    /// The filesystem root is refused rather than rooting a workspace at `/`. No-op for a persisted
    /// workspace — those own their roots, and an open must never edit them.
    pub fn adopt_ephemeral_root(&mut self, workspace_id: &str, dir: &Path) -> bool {
        let Some(workspace) = self.workspaces.get_mut(workspace_id) else {
            return false;
        };
        if !workspace.is_ephemeral() || workspace.contains(dir) {
            return false;
        }
        // `dir.parent()` is `None` only at the filesystem root.
        if dir.parent().is_none() {
            return false;
        }
        workspace.paths.push(dir.to_path_buf());
        workspace.workspace_index = Arc::new(WorkspaceIndex::new(workspace.paths.clone()));
        true
    }

    /// The live temporary workspace an open of `canonical` should **join** rather than mint a rival
    /// to: one that already has the path open as a buffer, or — for a `directory`, which has no
    /// buffer to match on — one whose roots already contain it. `None` when no temporary context
    /// claims the path, which is the caller's cue to mint one.
    ///
    /// Without this, two clients opening the same external path (`ae /etc/hosts` in two terminals;
    /// the `ae --web` launcher and the browser tab it opens) land in two rival contexts holding two
    /// buffers over one shared document — where two clients in a *named* workspace simply share the
    /// buffer. Joining makes a temporary context behave like any other one, and it is what lets the
    /// web tether wait on the very buffer the browser will close.
    ///
    /// Ties break on the lowest context number, so the answer doesn't depend on map iteration order
    /// when several contexts hold the same path (the state this rule stops accumulating).
    pub fn ephemeral_workspace_for(&self, canonical: &Path, directory: bool) -> Option<String> {
        let mut claiming: Vec<(u32, &str)> = self
            .workspaces
            .values()
            .filter(|w| w.is_ephemeral())
            .filter(|w| {
                if directory {
                    w.contains(canonical)
                } else {
                    self.buffer_for_path_in_workspace(&w.id, canonical)
                        .is_some()
                }
            })
            .map(|w| {
                let n =
                    w.id.strip_prefix(aether_protocol::EPHEMERAL_WORKSPACE_PREFIX)
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(u32::MAX);
                (n, w.id.as_str())
            })
            .collect();
        claiming.sort_unstable();
        claiming.first().map(|(_, id)| id.to_string())
    }

    /// Ephemeral workspaces a *new* one supersedes: every throwaway context nothing is using any
    /// more. See [`Self::supersede_ephemeral_workspaces`] for the rule and why it exists.
    fn superseded_ephemeral_workspaces(&self) -> Vec<String> {
        self.workspaces
            .values()
            .filter(|w| w.is_ephemeral())
            .filter(|w| !self.workspace_active_anywhere(&w.id))
            .filter(|w| {
                self.buffers_in_workspace(&w.id).into_iter().all(|id| {
                    !self.try_doc_of(id).is_some_and(|d| d.dirty)
                        && !self.viewports.values().any(|v| v.binds(id))
                })
            })
            .map(|w| w.id.clone())
            .collect()
    }

    /// Retire every ephemeral workspace a newly-created one supersedes, closing their buffers.
    /// Returns the workspace ids removed, the buffer ids closed, and the keys of any language
    /// servers torn down with them (so the caller can refresh the affected pickers).
    ///
    /// Temporary workspaces are meant to behave like transient buffers: one at a time, replaced
    /// rather than accumulated. An ephemeral context normally dies with its last buffer, but a
    /// client that quits (`Space q`) without closing that buffer leaves it behind — the buffers live
    /// on in the daemon — so every subsequent "open a file with no workspace" used to mint another
    /// one and the switcher filled up with `(workspace N)` rows. Opening a new one now sweeps the
    /// stale ones away.
    ///
    /// A temporary workspace is spared when anything still holds it: a client parked in it, a
    /// viewport showing one of its buffers, or an **unsaved** buffer — nothing here may discard
    /// work. (Those exemptions are also why this can't simply be done on disconnect.)
    pub fn supersede_ephemeral_workspaces(
        &mut self,
    ) -> (
        Vec<String>,
        Vec<BufferId>,
        Vec<crate::lsp::manager::LspServerKey>,
    ) {
        let ids = self.superseded_ephemeral_workspaces();
        let mut closed = Vec::new();
        let mut stopped = Vec::new();
        for id in &ids {
            for buffer_id in self.buffers_in_workspace(id) {
                closed.push(buffer_id);
                if let Some(key) = self.close_buffer(buffer_id) {
                    stopped.push(key);
                }
            }
            self.workspaces.remove(id);
        }
        (ids, closed, stopped)
    }

    /// Rename a loaded workspace in place: move its entry to the new key (updating `entry.name`),
    /// then re-point every buffer association and client active-workspace that referenced the old
    /// name. Open buffers are otherwise untouched — only the name key changes, nothing is closed —
    /// so this is safe even with dirty buffers in the workspace. Returns the workspace's root paths
    /// (display form) on success, or `None` if no workspace was loaded under `old`. The caller is
    /// responsible for renaming the on-disk config and for rejecting name collisions first.
    pub fn rename_workspace(&mut self, old: &str, new: &str) -> Option<Vec<String>> {
        let mut entry = self.workspaces.remove(old)?;
        // Rename only applies to persisted workspaces, whose id tracks their name.
        entry.id = new.to_string();
        entry.name = Some(new.to_string());
        let paths: Vec<String> = entry
            .paths
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        self.workspaces.insert(new.to_string(), entry);
        for workspace in self.buffer_workspaces.values_mut() {
            if workspace == old {
                *workspace = new.to_string();
            }
        }
        for session in self.clients.values_mut() {
            if session.active_workspace.as_deref() == Some(old) {
                session.active_workspace = Some(new.to_string());
            }
        }
        Some(paths)
    }

    /// True if any connected client currently has `name` as its active workspace. `workspace/delete`
    /// refuses in that case so deletion can't pull the rug out from under an open session.
    pub fn workspace_active_anywhere(&self, name: &str) -> bool {
        self.clients
            .values()
            .any(|c| c.active_workspace.as_deref() == Some(name))
    }

    /// Buffer ids belonging to `name`. Used by `workspace/delete` to find what it would close (and
    /// to screen them for unsaved changes first).
    pub fn buffers_in_workspace(&self, name: &str) -> Vec<BufferId> {
        self.buffer_workspaces
            .iter()
            .filter(|(_, p)| p.as_str() == name)
            .map(|(id, _)| *id)
            .collect()
    }

    /// How many buffers in `workspace` have unsaved edits — **loaded or not**. Drives the
    /// unsaved-count shown on each row of the workspace picker.
    ///
    /// Live dirty buffers (`Buffer::dirty`) plus every on-disk backup of the workspace that no live
    /// buffer accounts for. The disk half is what makes the indicator honest for a workspace nobody
    /// has activated since the server started: the daemon idle-reaps, so most unsaved work is
    /// sitting in `backups/<workspace>/` rather than in memory, and counting only loaded buffers
    /// showed every such workspace as clean. Backups belonging to a buffer that *is* loaded are
    /// excluded rather than added — the live buffer already counted, and double-counting it would
    /// be worse than the gap it fixes.
    ///
    /// `0` for a workspace with neither (the common case for a configured-but-unvisited workspace),
    /// and for an ephemeral one with no dirty buffers — those are never backed up.
    ///
    /// Costs two directory listings per workspace (of dirs holding one small file per unsaved
    /// buffer), on a path that is already a disk read — the picker's candidate list enumerates the
    /// workspaces directory to find its rows in the first place.
    pub fn unsaved_buffer_count(&self, workspace: &str) -> u32 {
        let live: Vec<(&Buffer, &Document)> = self
            .buffer_workspaces
            .iter()
            .filter(|(_, p)| p.as_str() == workspace)
            .filter_map(|(id, _)| {
                let buf = self.buffers.get(id)?;
                Some((buf, self.documents.get(&buf.document)?))
            })
            .collect();
        let dirty = live.iter().filter(|(_, d)| d.dirty).count() as u32;
        let Some(root) = self.backups_path.as_deref() else {
            return dirty;
        };
        // Scratch half: list the workspace's scratch-backup dir, minus numbers a live buffer
        // already accounts for.
        let mut scratches = crate::backup::scratch_keys(root, workspace);
        for (buf, _) in &live {
            if let Some(number) = buf.scratch_number {
                scratches.remove(&number);
            }
        }
        // Files half: file backups are document-level (`files/<hash>`, no workspace in the key)
        // and the hash is one-way, so the shared directory can't be attributed by listing. Go the
        // other way: this workspace's *session entries* name its files — count each whose backup
        // exists and that no live buffer here accounts for. A file backup recorded in no session
        // was never restorable by activation anyway (only by recover-on-open), so not counting it
        // against any workspace matches what activation can actually bring back.
        let live_paths: std::collections::HashSet<&Path> = live
            .iter()
            .filter_map(|(_, d)| d.canonical_path.as_deref())
            .collect();
        let mut file_count = 0u32;
        if let Some(sessions_path) = self.sessions_path.as_deref() {
            if let Ok(sessions) = crate::config::load_workspace_sessions_at(sessions_path) {
                if let Some(session) = sessions.workspaces.get(workspace) {
                    for entry in &session.buffers {
                        if let crate::config::SessionBuffer::File { path } = entry {
                            if !live_paths.contains(path.as_path())
                                && crate::backup::exists(&crate::backup::file_backup_path(
                                    root, path,
                                ))
                            {
                                file_count += 1;
                            }
                        }
                    }
                }
            }
        }
        dirty + file_count + scratches.len() as u32
    }

    /// Whether any dirty document's content exists **only** in memory — no on-disk backup would
    /// survive this process. True when backups are disabled entirely (in-process tests and
    /// embeddings), and for a dirty *scratch* whose every attachment is ephemeral — its backup
    /// key (`scratch/<workspace>/<number>`) dies with the context, so the flush skips it. A
    /// dirty *file-backed* document is always protected when backups are on: its backup is
    /// path-keyed and recover-on-open is workspace-agnostic, so even a tether context's edits
    /// come back the next time the path is opened from anywhere. The idle reaper consults this
    /// so an auto-started server never reaps unsaved work it can't restore.
    pub fn has_unprotected_unsaved_buffers(&self) -> bool {
        let backups_enabled = self.backups_path.is_some();
        self.documents.values().any(|d| {
            d.dirty
                && !(backups_enabled
                    && (d.canonical_path.is_some()
                        || self.buffers.iter().any(|(id, b)| {
                            b.document == d.id
                                && self
                                    .buffer_workspaces
                                    .get(id)
                                    .and_then(|w| self.workspaces.get(w))
                                    .is_some_and(|w| !w.is_ephemeral())
                        })))
        })
    }

    /// The display number to assign a *new* scratch buffer in `workspace`: the lowest positive
    /// integer not already in use by another scratch there. Keeps `(scratch N)` numbers small and
    /// stable, reusing one once its buffer closes. Call before inserting the new buffer.
    pub fn next_scratch_number(&self, workspace: &str) -> u32 {
        let mut used: std::collections::HashSet<u32> = self
            .buffer_workspaces
            .iter()
            .filter(|(_, p)| p.as_str() == workspace)
            .filter_map(|(id, _)| self.buffers.get(id))
            .filter_map(|b| b.scratch_number)
            .collect();
        // Also reserve numbers held by *dormant* scratch buffers (restored from the session but not
        // yet materialized) so a fresh scratch can't grab one out from under a pending restore.
        if let Some(w) = self.workspaces.get(workspace) {
            for d in &w.dormant_buffers {
                if let DormantSource::Scratch { number } = d.source {
                    used.insert(number);
                }
            }
        }
        (1..)
            .find(|n| !used.contains(n))
            .expect("u32 range is non-empty")
    }

    /// Buffer ids in `workspace` whose backing file is at or under `canonical` — an exact match for
    /// a file, or a path-prefix match for a directory. Used by `path/delete` to find the buffers a
    /// deletion would close (and to screen them for unsaved changes first).
    pub fn buffers_under_path(&self, workspace: &str, canonical: &Path) -> Vec<BufferId> {
        self.buffers
            .iter()
            .filter(|(id, b)| {
                self.buffer_workspaces.get(id).map(|s| s.as_str()) == Some(workspace)
                    && self
                        .documents
                        .get(&b.document)
                        .and_then(|d| d.canonical_path.as_deref())
                        .is_some_and(|p| p == canonical || p.starts_with(canonical))
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Close one buffer: drop it and every per-`(client, buffer)` slice keyed to it. This is the
    /// canonical `buffer/close` teardown, shared by root removal, workspace deletion, and path
    /// deletion so they can't drift out of sync.
    /// Returns the key of a language server that was torn down because this was its last buffer
    /// (so the caller can refresh open status views), or `None`.
    pub fn close_buffer(&mut self, id: BufferId) -> Option<crate::lsp::manager::LspServerKey> {
        // Notify any language server before we drop the buffer (needs its path).
        let lsp_uri = self
            .try_doc_of(id)
            .and_then(|d| d.canonical_path.as_deref())
            .map(crate::lsp::uri::path_to_uri);
        let doc_id = self.buffers.get(&id).map(|b| b.document);
        // The document goes with the uri: `didClose` fires only when this was its last holder on
        // that server, since another workspace may still have the same file open against it.
        let stopped_server = match (lsp_uri, doc_id) {
            (Some(uri), Some(doc)) => self.lsp.notify_close(id, doc, &uri),
            _ => None,
        };
        self.buffers.remove(&id);
        // Drop the document once its last buffer is gone — content lives exactly as long as some
        // workspace still holds it open.
        if let Some(doc_id) = doc_id {
            if !self.buffers.values().any(|b| b.document == doc_id) {
                self.documents.remove(&doc_id);
            }
        }
        self.buffer_workspaces.remove(&id);
        // A view's layout dies with the view, and no *surviving* layout may keep naming this
        // buffer: `element_bindings` feeds every element's id to `ViewLayout::of`, which resolves
        // it through `doc_of` — documented to panic on an unknown buffer. Closing a file a patch
        // view windows also drops that view's viewport (just below), so re-subscribing is the very
        // next thing that happens.
        //
        // The whole layout goes, not the one dead element. An element's extent is in *file* lines
        // while it is bound and in the generated patch's lines once it is not
        // (`patch::layout_over_files`), so unbinding in place would reinterpret one space as the
        // other — a silently wrong window in place of a panic, which is the worse trade. Without a
        // layout the view falls back to one region over its own document, and the next
        // `git/show` rebuilds the bindings.
        self.view_layouts.remove(&id);
        self.view_layouts
            .retain(|_, layout| !layout.iter().any(|e| e.extent.bound_to() == Some(id)));
        // `shows`: a viewport *presenting* a patch binds no element of it, so a `binds` test left
        // it alive with a `view_id` pointing at a buffer that no longer exists.
        self.viewports.retain(|_, v| !v.shows(id));
        self.cursors.retain(|(_, b), _| *b != id);
        self.motion_history.retain(|(_, b), _| *b != id);
        self.virtual_col.retain(|(_, b), _| *b != id);
        self.tree_selection_history.retain(|(_, b), _| *b != id);
        self.searches.retain(|(_, b), _| *b != id);
        self.sneaks.retain(|(_, b), _| *b != id);
        self.symbol_highlights.retain(|(_, b), _| *b != id);
        self.symbol_highlight_gen.retain(|(_, b), _| *b != id);
        self.symbol_highlight_follow.retain(|(_, b)| *b != id);
        self.blame_follow.retain(|(_, b)| *b != id);
        self.blame_follow_gen.retain(|(_, b), _| *b != id);
        self.blame_last_pushed.retain(|(_, b), _| *b != id);
        self.symbol_path_sent.retain(|(_, b), _| *b != id);
        self.last_scroll.retain(|(_, b), _| *b != id);
        self.git_unstaged_hunks.remove(&id);
        self.git_both_hunks.remove(&id);
        self.git_baseline.remove(&id);
        self.virtual_git_status.remove(&id);
        self.git_conflicts.remove(&id);
        self.git_blame.remove(&id);
        self.diagnostics.remove(&id);
        self.document_symbols.remove(&id);
        self.drop_buffer_from_mru(id);
        stopped_server
    }

    /// Close every buffer in `candidates` that is transient and no longer shown by any viewport.
    /// This is the "hidden ⇒ close" half of transient buffers; callers pass the buffers a client
    /// just stopped viewing (viewport switch, workspace switch, disconnect) *after* dropping the
    /// stale viewports. A dirty document blocks the close as a guard — the first edit promotes,
    /// so a dirty transient shouldn't exist — *unless* a sibling buffer in another workspace
    /// still holds the document, in which case closing this attachment loses nothing. Returns
    /// the ids closed and the keys of language servers torn down with them (so callers can
    /// refresh picker views).
    /// Whether closing `id` would drop the **last in-memory copy of unsaved work**.
    ///
    /// The question every *automatic* close has to ask, given a name and one definition. It was
    /// written out inline in the collector as `has_sibling || !dirty`, which is the same rule
    /// stated as a coincidence of two locals — and a rule stated inline is a rule the next
    /// automatic close path will restate slightly differently, or not at all.
    ///
    /// A sibling buffer on the same document keeps the content reachable, which is why this asks
    /// about the *document* rather than the buffer: two views of one file are two buffers and one
    /// piece of unsaved work.
    ///
    /// Not consulted by `buffer/close`: closing on purpose with unsaved changes is the user's call
    /// to make (they are prompted), and this is about closes nobody asked for.
    pub fn close_would_orphan_unsaved(&self, id: BufferId) -> bool {
        let Some(buffer) = self.buffers.get(&id) else {
            return false;
        };
        if !self.try_doc_of(id).is_some_and(|d| d.dirty) {
            return false;
        }
        !self
            .buffers
            .values()
            .any(|o| o.document == buffer.document && o.id != id)
    }

    pub fn close_orphaned_transients(
        &mut self,
        candidates: impl IntoIterator<Item = BufferId>,
    ) -> (Vec<BufferId>, Vec<crate::lsp::manager::LspServerKey>) {
        let mut closed = Vec::new();
        let mut stopped = Vec::new();
        for id in candidates {
            let eligible = self.buffers.get(&id).is_some_and(|b| b.transient)
                && !self.close_would_orphan_unsaved(id)
                // `shows`, not `binds`: a patch's viewers are watching the *view*, and no element
                // windows it, so asking only about bindings said "nothing is showing this" about
                // the document on screen. The GC and the push fan-out now ask the same question —
                // a buffer that is live enough to receive notifications is live enough to keep.
                && !self.viewports.values().any(|v| v.shows(id));
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

    /// Delete a loaded workspace's in-memory state: drop the workspace entry and close every buffer
    /// that belonged to it, tearing down all per-buffer state (same teardown as `buffer/close` /
    /// `workspace/remove_root`). Returns the closed buffer ids. The caller is responsible for the
    /// refusal checks ([`Self::workspace_active_anywhere`], dirty buffers) and for removing the
    /// on-disk config. A no-op for the workspace entry when it was never loaded; still closes any
    /// of its buffers that exist.
    pub fn delete_workspace(&mut self, name: &str) -> Vec<BufferId> {
        let mut closed = Vec::new();
        for buffer in self.buffers_in_workspace(name) {
            self.close_buffer(buffer);
            closed.push(buffer);
        }
        self.workspaces.remove(name);
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

    /// Clear motion history for every client on the given buffer — and on every sibling buffer
    /// sharing its document, since a mutation through one buffer invalidates remembered positions
    /// on all of them. Called on any buffer mutation (text, delete, cut, join, undo, redo) — the
    /// user contract is "motion undo only goes back to the last edit".
    pub fn clear_motion_history_for_buffer(&mut self, buffer_id: BufferId) {
        let attached = self.doc_siblings(buffer_id);
        for ((_, b), h) in self.motion_history.iter_mut() {
            if attached.contains(b) {
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
        self.symbol_highlight_gen
            .retain(|(c, _), _| *c != client_id);
        self.symbol_highlight_follow
            .retain(|(c, _)| *c != client_id);
        self.blame_follow.retain(|(c, _)| *c != client_id);
        self.blame_follow_gen.retain(|(c, _), _| *c != client_id);
        self.blame_last_pushed.retain(|(c, _), _| *c != client_id);
        self.symbol_path_sent.retain(|(c, _), _| *c != client_id);
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
    /// fresh session, so the nav history — like cursor/selection state — is not recovered).
    pub fn drop_nav_history_for_client(&mut self, client_id: ClientId) {
        self.nav_history.remove(&client_id);
    }

    /// Bump `buffer_id` to the front of its workspace's MRU. Called from `buffer/open` every time
    /// any client lands on a buffer — fresh open, reopen, or attach-by-id. No-op if the buffer
    /// has no recorded workspace (shouldn't happen for live buffers but the lookup is defensive).
    pub fn touch_mru(&mut self, buffer_id: BufferId) {
        let Some(workspace_name) = self.buffer_workspaces.get(&buffer_id).cloned() else {
            return;
        };
        let Some(workspace) = self.workspaces.get_mut(&workspace_name) else {
            return;
        };
        workspace.mru_buffers.retain(|&b| b != buffer_id);
        workspace.mru_buffers.push_front(buffer_id);
    }

    /// Drop `buffer_id` from every workspace's MRU. Called from `buffer/close` so a closed buffer
    /// doesn't reappear at the top of the picker on the next open.
    pub fn drop_buffer_from_mru(&mut self, buffer_id: BufferId) {
        for workspace in self.workspaces.values_mut() {
            workspace.mru_buffers.retain(|&b| b != buffer_id);
        }
    }

    /// The buffers to persist for `workspace_name`, most-recently-used first: the workspace's live
    /// MRU buffers, then its still-dormant buffers (already in MRU order). Files are keyed by path,
    /// scratches by number; deduplicated by each. This is exactly what a future activation should
    /// restore, so it's what gets written to the session file.
    ///
    /// What's excluded: **transient** buffers — preview opens that auto-close once you navigate away
    /// (grep/file-picker peeks, goto-def, nav revisits) — and **clean scratch** buffers. Transient
    /// means "ephemeral, don't accumulate me"; persisting previews would reintroduce exactly the
    /// buffer-list clutter the transient mechanism exists to avoid. A scratch is only worth restoring
    /// if it has unsaved content (it's dirty, hence has a backup); an empty scratch is dropped.
    ///
    /// **Virtual** buffers ([`VirtualSource`]) are included only once *kept*, which the transient
    /// rule above already enforces: a revision opens transient, so it takes a deliberate `Space k`
    /// to persist one. That's the whole gate — a diff you glanced at from the log picker is a
    /// preview and stays out, a diff you pinned is somewhere you were working. Keyed by
    /// `VirtualSource::key`, which is stable across restarts (a repo id is its canonical workdir).
    pub fn session_buffers(&self, workspace_name: &str) -> Vec<crate::config::SessionBuffer> {
        use crate::config::SessionBuffer;
        let Some(workspace) = self.workspaces.get(workspace_name) else {
            return Vec::new();
        };
        let mut out: Vec<SessionBuffer> = Vec::new();
        let mut seen_paths: std::collections::HashSet<&Path> = std::collections::HashSet::new();
        let mut seen_scratch: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut seen_virtual: std::collections::HashSet<String> = std::collections::HashSet::new();
        for id in &workspace.mru_buffers {
            let Some(buf) = self.buffers.get(id) else {
                continue;
            };
            if buf.transient {
                continue;
            }
            let Some(doc) = self.documents.get(&buf.document) else {
                continue;
            };
            if let Some(path) = doc.canonical_path.as_deref() {
                if seen_paths.insert(path) {
                    out.push(SessionBuffer::File {
                        path: path.to_path_buf(),
                    });
                }
            } else if let Some(source) = doc.virtual_source.as_ref() {
                let key = source.target.key();
                if seen_virtual.insert(key.clone()) {
                    out.push(SessionBuffer::Virtual { key });
                }
            } else if let Some(number) = buf.scratch_number {
                // Only dirty scratches carry content worth restoring (and therefore a backup).
                if doc.dirty && seen_scratch.insert(number) {
                    out.push(SessionBuffer::Scratch { number });
                }
            }
        }
        for d in &workspace.dormant_buffers {
            match &d.source {
                DormantSource::File(path) => {
                    if seen_paths.insert(path.as_path()) {
                        out.push(SessionBuffer::File { path: path.clone() });
                    }
                }
                DormantSource::Scratch { number } => {
                    if seen_scratch.insert(*number) {
                        out.push(SessionBuffer::Scratch { number: *number });
                    }
                }
                DormantSource::Virtual { key } => {
                    if seen_virtual.insert(key.clone()) {
                        out.push(SessionBuffer::Virtual { key: key.clone() });
                    }
                }
            }
        }
        out
    }

    /// Remove the dormant entry for `canonical` from `workspace_name`, if present. Called when a live
    /// buffer for that path opens, so the now-loaded file doesn't also show as a dormant row.
    pub fn promote_dormant(&mut self, workspace_name: &str, canonical: &Path) {
        if let Some(workspace) = self.workspaces.get_mut(workspace_name) {
            workspace
                .dormant_buffers
                .retain(|d| d.path() != Some(canonical));
        }
    }

    /// Restore the dormant list's invariant in `workspace_name`: **at most one entry per path, and
    /// none for a path that already has a live buffer.**
    ///
    /// `promote_dormant` keeps this at the one moment a path is materialised, which is enough while
    /// entries only ever arrive one at a time. A worktree rebind adds a whole set at once *and*
    /// rewrites the paths of the entries already there, so two of them can land on the same file —
    /// and a live buffer opened by another client can appear beside one. Both show up as a
    /// duplicated row in the buffers picker.
    ///
    /// Enforced here rather than trusted at each call site: the rebind has several routes in
    /// (remap, extend, another client's open racing the landing), and an invariant that has to hold
    /// after all of them is cheaper to restore once than to prove at each.
    pub fn dedupe_dormant(&mut self, workspace_name: &str) {
        let live: std::collections::HashSet<PathBuf> = self
            .buffers
            .keys()
            .filter(|id| self.buffer_workspaces.get(id).map(String::as_str) == Some(workspace_name))
            .filter_map(|id| self.try_doc_of(*id).and_then(|d| d.canonical_path.clone()))
            .collect();
        let Some(workspace) = self.workspaces.get_mut(workspace_name) else {
            return;
        };
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        workspace.dormant_buffers.retain(|d| match d.path() {
            // A scratch has no path to collide on; it is identified by its number.
            None => true,
            Some(path) => !live.contains(path) && seen.insert(path.to_path_buf()),
        });
    }

    /// Remove and return the dormant buffer with `id` in `workspace_name`, if any. Used by
    /// `buffer/open`'s by-id path to materialize a dormant buffer the picker selected — the caller
    /// inspects [`DormantBuffer::source`] to decide whether to load a file or rebuild a scratch.
    pub fn take_dormant(&mut self, workspace_name: &str, id: BufferId) -> Option<DormantBuffer> {
        let workspace = self.workspaces.get_mut(workspace_name)?;
        let pos = workspace.dormant_buffers.iter().position(|d| d.id == id)?;
        Some(workspace.dormant_buffers.remove(pos))
    }

    /// The id of `workspace_name`'s most-recently-used dormant buffer (front of the list), if any.
    /// Used as the activation landing target when the workspace has no live MRU buffer yet (a cold
    /// restore after a restart).
    pub fn first_dormant_id(&self, workspace_name: &str) -> Option<BufferId> {
        self.workspaces
            .get(workspace_name)?
            .dormant_buffers
            .first()
            .map(|d| d.id)
    }

    /// Drop the selection-expansion history for one client+buffer. Called from every cursor RPC
    /// except `expand` / `contract` (and from every buffer mutation) so the contract chain only
    /// follows a contiguous run of expands.
    /// Where this server's workspace definitions live: the tempdir a test pointed us at, or the
    /// profile's real workspaces directory.
    pub fn workspaces_dir(&self) -> anyhow::Result<PathBuf> {
        match &self.workspaces_dir {
            Some(dir) => Ok(dir.clone()),
            None => crate::config::workspaces_dir(),
        }
    }

    /// Where the app settings live for this server: the tempfile a test pointed us at, or the
    /// profile's real `settings.toml`.
    pub fn app_settings_path(&self) -> anyhow::Result<PathBuf> {
        match &self.settings_path {
            Some(path) => Ok(path.clone()),
            None => crate::config::app_settings_path(),
        }
    }

    pub fn clear_tree_selection_history(&mut self, client_id: ClientId, buffer_id: BufferId) {
        self.tree_selection_history.remove(&(client_id, buffer_id));
    }

    /// Clear selection-expansion history for every client on the given buffer and its document
    /// siblings. Called from buffer mutation paths so a post-edit contract doesn't pop a stale
    /// (pre-edit) selection.
    pub fn clear_tree_selection_history_for_buffer(&mut self, buffer_id: BufferId) {
        let attached = self.doc_siblings(buffer_id);
        self.tree_selection_history
            .retain(|(_, b), _| !attached.contains(b));
    }

    /// Remove all selection-expansion records for the given client. Used on disconnect.
    pub fn drop_tree_selection_history_for_client(&mut self, client_id: ClientId) {
        self.tree_selection_history
            .retain(|(c, _), _| *c != client_id);
    }

    /// Clear virtual column for every client on the given buffer and its document siblings.
    /// Called on any buffer mutation.
    pub fn clear_virtual_col_for_buffer(&mut self, buffer_id: BufferId) {
        let attached = self.doc_siblings(buffer_id);
        self.virtual_col.retain(|(_, b), _| !attached.contains(b));
    }

    /// Locate an already-open buffer for the given canonical path, if any. Scoped to a workspace —
    /// two workspaces can independently open the same file as separate buffers, and a path lookup
    /// during `buffer/open` for workspace A shouldn't latch onto workspace B's existing buffer.
    pub fn buffer_for_path_in_workspace(
        &self,
        workspace_name: &str,
        canonical: &Path,
    ) -> Option<BufferId> {
        self.buffers.iter().find_map(|(id, b)| {
            if self
                .documents
                .get(&b.document)
                .and_then(|d| d.canonical_path.as_deref())
                == Some(canonical)
                && self.buffer_workspaces.get(id).map(|s| s.as_str()) == Some(workspace_name)
            {
                Some(*id)
            } else {
                None
            }
        })
    }

    /// Locate every open buffer for the given canonical path, across all workspaces. Used by the
    /// file watcher, which has a path but not a workspace context. Plural because workspaces with
    /// overlapping roots can each hold their own buffer for the same file — a disk change must
    /// reach all of them, not whichever one iteration order yields first.
    pub fn buffers_for_path(&self, canonical: &Path) -> Vec<BufferId> {
        self.buffers
            .iter()
            .filter(|(_, b)| {
                self.documents
                    .get(&b.document)
                    .and_then(|d| d.canonical_path.as_deref())
                    == Some(canonical)
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Tear down all per-`(client, buffer)` state for buffers that belong to `workspace_name`,
    /// limited to one client. Used when the client switches its active workspace: the buffers
    /// themselves stay alive (other clients may have them open), but this client's viewports,
    /// cursors, history, searches, scroll, and pickers/mru are reset.
    pub fn teardown_client_state_for_workspace(
        &mut self,
        client_id: ClientId,
        workspace_name: &str,
    ) {
        // Snapshot the buffer ids belonging to the workspace; we'll filter all the per-(client,
        // buffer) maps against this set. Avoids borrowing `buffers` while mutating the maps.
        let workspace_buffers: std::collections::HashSet<BufferId> = self
            .buffer_workspaces
            .iter()
            .filter_map(|(id, name)| (name == workspace_name).then_some(*id))
            .collect();

        // Every buffer the departing viewports were showing, filtered to this workspace's. A
        // composed view holds several, and only naming the focused one left the rest behind.
        let viewed: Vec<BufferId> = self
            .viewports
            .values()
            .filter(|v| v.client_id == client_id && workspace_buffers.contains(&v.buffer_id()))
            .flat_map(|v| v.shown_buffers())
            .filter(|b| workspace_buffers.contains(b))
            .collect();
        self.viewports.retain(|_, v| {
            !(v.client_id == client_id && workspace_buffers.contains(&v.buffer_id()))
        });
        // A transient buffer the client was previewing doesn't survive leaving the workspace —
        // it's hidden now, same as switching buffers. (Permanent buffers stay alive for
        // re-entry, per the comment below.)
        let _ = self.close_orphaned_transients(viewed);
        let in_proj = |c: &ClientId, b: &BufferId| *c == client_id && workspace_buffers.contains(b);
        // Viewports + the live search state get torn down (they're transient view-layer
        // bookkeeping). Cursors / motion history / tree-selection / virtual-col / scroll are
        // *preserved* — they're the user's place-in-the-buffer memory, and re-attaching to a
        // buffer on workspace re-entry should restore them. The MRU is preserved for the same
        // reason: the buffer picker filters by active workspace, so cross-workspace MRU entries
        // don't leak into the UI, but they still let us reattach to "the buffer you last had"
        // when you come back.
        self.searches.retain(|(c, b), _| !in_proj(c, b));
        self.sneaks.retain(|(c, b), _| !in_proj(c, b));
        self.symbol_highlights.retain(|(c, b), _| !in_proj(c, b));
        self.symbol_highlight_gen.retain(|(c, b), _| !in_proj(c, b));
        self.symbol_highlight_follow
            .retain(|&(c, b)| !in_proj(&c, &b));
        self.blame_follow.retain(|&(c, b)| !in_proj(&c, &b));
        self.blame_follow_gen.retain(|(c, b), _| !in_proj(c, b));
        self.blame_last_pushed.retain(|(c, b), _| !in_proj(c, b));
        self.symbol_path_sent.retain(|(c, b), _| !in_proj(c, b));

        // Pickers are per-session UI state — their candidate sets/queries reference the prior
        // workspace so wipe them all on switch. The jumplist used to go with them, back when it
        // hung off the client: it stays now, on the entry being left, because the entry is what its
        // paths point into — so coming back finds it, and stepping in the workspace you switched to
        // steps *its* list. Same rule, arrived at by ownership rather than by a wipe.
        self.pickers.retain(|(c, _), _| *c != client_id);
    }
}

/// One workspace's handle on a [`Document`]: the unit the protocol addresses (`BufferId`), carrying
/// only the state that describes this workspace's *relationship* to the content — the content
/// itself (rope, undo, dirty, parse) lives on the shared document. Two workspaces holding the same
/// file are two `Buffer`s over one `Document`.
pub struct Buffer {
    pub id: BufferId,
    /// The shared document this buffer views. Multiple buffers (one per workspace) may reference
    /// the same document; the document is dropped when the last one closes.
    pub document: DocumentId,
    /// Small per-workspace display number for a scratch buffer (`(scratch N)`), assigned at creation
    /// as the lowest positive integer not in use by another scratch in the workspace — so the numbers
    /// stay small, stay stable for the buffer's life, and a freed number gets reused. `None` for
    /// file-backed buffers (which display their path instead).
    pub scratch_number: Option<u32>,
    /// Transient buffers auto-close once no viewport shows them anymore (see
    /// `viewport_subscribe` / `close_orphaned_transients`). Set at creation when the opening
    /// client asked for it (picker/goto-def navigation, the bootstrap scratch); cleared —
    /// "promoted" — by the buffer's first edit, a save, or a user-initiated reload. Never set
    /// again after creation. Per-buffer, not per-document: an edit arriving through a *sibling*
    /// buffer doesn't promote this one — if it goes hidden it just detaches, and the content
    /// survives on the document.
    pub transient: bool,
}

/// The shared content of one open file (or scratch): everything derived from *what the text is*
/// rather than from any workspace's relationship to it. Owned by [`ServerState::documents`] and
/// referenced by one or more [`Buffer`]s — see `Buffer` for the split. What a **virtual** document
/// was materialised from: content the server produced from an immutable source rather than loading
/// from disk. `git/show` is the only producer today — a commit's patch, or a file as of a commit.
///
/// Its presence is what makes a document read-only: there is no file to save to and no meaning to
/// an edit against a revision that has already happened. Deriving read-only from this rather than
/// carrying a separate flag keeps the two from disagreeing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualSource {
    /// What it was materialised *from*, and the identity for reuse: re-showing the same target
    /// attaches to the existing document instead of stacking duplicates — the pathless equivalent
    /// of the canonical-path sharing key.
    pub target: VirtualTarget,
    /// Display name (`abc1234 — subject`, `abc1234:src/main.rs`, `Working changes`), shipped as
    /// `BufferOpenResult::title`.
    pub title: String,
}

/// What a virtual document was generated from: a repo, plus which of its states.
///
/// **Structured, not a string.** It was a string once, and every consumer that wanted one field out
/// of it — the repo, the rev, the path — re-split it by hand, each slightly differently
/// (`rsplit_once('@')`, `split_once(':')`). Adding a fourth shape quietly broke one of those
/// splits, which is the failure mode this exists to remove: ask for the field you want and a shape
/// that hasn't got one answers `None`.
///
/// The string form survives only as an *encoding*, for the one place that needs to write a target
/// down and read it back: the session file. See [`Self::key`] and [`Self::parse_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualTarget {
    /// Canonical workdir of the repo — which is what makes a key stable across restarts.
    pub repo_id: String,
    pub what: aether_protocol::git::ShowTarget,
}

impl VirtualTarget {
    pub fn new(repo_id: impl Into<String>, what: aether_protocol::git::ShowTarget) -> Self {
        Self {
            repo_id: repo_id.into(),
            what,
        }
    }

    pub fn rev(&self) -> Option<&str> {
        self.what.rev()
    }

    pub fn path(&self) -> Option<&str> {
        self.what.path()
    }

    /// Whether re-showing this can attach to a buffer already holding it. A revision can't change
    /// under us; the working tree can, and has to be rebuilt.
    pub fn is_immutable(&self) -> bool {
        self.rev().is_some()
    }

    /// Stable string encoding, for writing a target into the session file.
    ///
    /// `#` separates the working tree rather than `@`, so a round-trip can never mistake it for a
    /// revision named `worktree`.
    pub fn key(&self) -> String {
        use aether_protocol::git::ShowTarget;
        match &self.what {
            ShowTarget::Commit { rev } => format!("{}@{rev}", self.repo_id),
            ShowTarget::File { rev, path } => format!("{}@{rev}:{path}", self.repo_id),
            ShowTarget::WorkingChanges => format!("{}#worktree", self.repo_id),
        }
    }

    /// Inverse of [`Self::key`]. Split from the right: a repo id is a filesystem path and may
    /// itself contain `@`; the remainder can't, so the `:` split after it is unambiguous.
    pub fn parse_key(key: &str) -> Option<Self> {
        use aether_protocol::git::ShowTarget;
        if let Some(repo_id) = key.strip_suffix("#worktree") {
            return Some(Self::new(repo_id, ShowTarget::WorkingChanges));
        }
        let (repo_id, rest) = key.rsplit_once('@')?;
        Some(Self::new(
            repo_id,
            match rest.split_once(':') {
                Some((rev, path)) => ShowTarget::File {
                    rev: rev.to_string(),
                    path: path.to_string(),
                },
                None => ShowTarget::Commit {
                    rev: rest.to_string(),
                },
            },
        ))
    }
}

pub struct Document {
    pub id: DocumentId,
    /// The canonical path this document is loaded from (and the sharing key — one live document
    /// per canonical path). `None` for a scratch *and* for a virtual document (`virtual_source`).
    pub canonical_path: Option<PathBuf>,
    /// Set when this document's content was materialised from a revision rather than a file — see
    /// [`VirtualSource`]. Read-only, never backed up, never session-restored.
    pub virtual_source: Option<VirtualSource>,
    pub text: ropey::Rope,
    pub revision: Revision,
    pub language: Option<String>,
    /// Derived: `revision != saved_revision`. Kept as a field for cheap reads.
    pub dirty: bool,
    pub line_ending: LineEnding,
    pub last_modified_unix_ms: Option<u64>,
    pub syntax: Option<BufferSyntax>,
    /// A background parse is producing this buffer's `syntax` (see `handlers::finish_pending_parse`).
    /// Set by `load_from_file` when the file is too large to parse on the open round-trip; cleared
    /// when the parse lands (or when there's nothing to parse). While pending, everything
    /// tree-dependent degrades exactly as for a language without a grammar: `syntax` is `None`,
    /// renders are unhighlighted, and `apply_edit` skips tree maintenance — the background task
    /// re-checks `revision` and reparses until it catches up.
    pub syntax_pending: bool,
    /// Where the most recent [`Self::apply_edit`] landed, or `None` when it changed no line count
    /// (the overwhelmingly common case — typing within a line). Consumed once, by the post-edit
    /// refresh that shifts a view's element extents; see [`LineShift`].
    ///
    /// Only `apply_edit` sets it. Undo, redo and reload replace the rope wholesale, where a *shift*
    /// is not a meaningful description of what happened — those rely on the layout's clamp against
    /// the live buffer and on the rebuild that a save/stage/commit performs.
    pub last_shift: Option<LineShift>,
    /// Decorations and structure computed once when the content was *generated*, for documents no
    /// grammar spans — the commit patch behind `git/show`. See [`crate::patch::GeneratedPatch`].
    pub generated: Option<crate::patch::GeneratedPatch>,
    /// Detected (or defaulted) once on load; stable for the buffer's lifetime so further edits
    /// don't make the unit drift.
    pub indent_style: IndentStyle,
    /// Disk diverged while the buffer was dirty — the watcher couldn't silently reload. Set by
    /// the watcher, cleared by a successful save or a `buffer/reload`.
    pub externally_modified: bool,
    /// Buffer's on-disk file was removed externally. Set by the watcher, cleared by a save
    /// (which recreates the file) or by the file being recreated externally.
    pub externally_deleted: bool,
    /// The `revision` last written to an on-disk backup ([`crate::backup`]), or `None` if no backup
    /// is currently on disk for this buffer. The flush task uses it to skip rewriting unchanged
    /// content and to delete a stale backup once the buffer goes clean. Purely server-internal —
    /// never sent to clients, not part of `dirty`.
    pub backed_up_revision: Option<Revision>,
    /// The document's text as it was at the last point it matched disk, LF-normalised — the
    /// baseline for the saved-file diff. `Some` exactly while the document is dirty; see
    /// [`Document::snapshot_disk_text`] for why it is captured rather than read back.
    ///
    /// Lives on the document rather than beside the Git baselines (which are per-`BufferId`,
    /// because git is workspace-scoped) because being unsaved is a property of the content: two
    /// workspaces viewing one dirty document are looking at the same unsaved edits.
    pub disk_blob: Option<Vec<u8>>,

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
    /// Taking a side in a merge conflict (`git/resolve_conflict`). Its own tag for the same reason
    /// as `Revert`: one take, one undo step — which is what makes trying a side and changing your
    /// mind cost a single `Ctrl-z`.
    Resolve,
}

struct UndoEntry {
    rope: ropey::Rope,
    revision: Revision,
    /// Cursor snapshot at the start of the group, across every buffer attached to the document —
    /// keyed by `(client, buffer)` because one client can hold cursors on the same document
    /// through two workspaces' buffers.
    cursors: std::collections::HashMap<(ClientId, BufferId), CursorState>,
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
    pub restored_cursors: std::collections::HashMap<(ClientId, BufferId), CursorState>,
}

/// Where an edit landed and how many lines it added or removed.
///
/// Recorded by [`Document::apply_edit`] because that is the one place both facts are known, and
/// consumed by the element layouts a *view* is built from: a patch's hunk windows `start..end` of a
/// file, and typing inside it has to grow that window or the hunk's last line scrolls out of a view
/// that still claims the old height. Threading this through the dozen post-edit call sites instead
/// would have meant a dozen chances to forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineShift {
    /// First line the edit touched.
    pub at: u32,
    /// Lines gained (positive) or lost (negative).
    pub delta: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl Document {
    /// Load a document from disk. Detects line endings, normalizes to LF in-memory.
    pub fn load_from_file(id: DocumentId, canonical: PathBuf) -> std::io::Result<Self> {
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
        // Large files defer the parse to a background task (`handlers::finish_pending_parse`) so
        // the open round-trip returns in milliseconds; the first frame renders unhighlighted and a
        // `viewport/lines_changed` push restyles it when the tree lands. Small files parse inline —
        // the cost is a few ms and it keeps the first frame highlighted with no restyle flash.
        let defer_parse = language
            .as_deref()
            .is_some_and(|name| !sync_parse_affordable(name, text.len_bytes()));
        let syntax = if defer_parse {
            None
        } else {
            language
                .as_deref()
                .and_then(|name| make_syntax(&text, name))
        };
        let indent_style = resolve_indent_style(&text, language.as_deref());
        Ok(Document {
            id,
            canonical_path: Some(canonical),
            text,
            revision: 0,
            language,
            dirty: false,
            line_ending,
            last_modified_unix_ms,
            syntax,
            syntax_pending: defer_parse,
            generated: None,
            indent_style,
            saved_revision: Some(0),
            next_revision_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_group: None,
            externally_modified: false,
            externally_deleted: false,
            backed_up_revision: None,
            disk_blob: None,
            last_shift: None,
            virtual_source: None,
        })
    }

    /// Empty document with a target file path attached but no file on disk yet. Used by
    /// `buffer/open` with `create_if_missing: true` — the file is created by `save_to_disk`
    /// on the first save. Language is auto-detected from the extension if not provided.
    pub fn new_at_path(id: DocumentId, canonical: PathBuf, language: Option<String>) -> Self {
        let text = ropey::Rope::new();
        let language = language.or_else(|| detect_language(&canonical));
        let syntax = language
            .as_deref()
            .and_then(|name| make_syntax(&text, name));
        let indent_style = resolve_indent_style(&text, language.as_deref());
        Document {
            id,
            canonical_path: Some(canonical),
            text,
            revision: 0,
            language,
            dirty: false,
            line_ending: LineEnding::Lf,
            last_modified_unix_ms: None,
            syntax,
            syntax_pending: false,
            last_shift: None,
            generated: None,
            indent_style,
            saved_revision: Some(0),
            next_revision_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_group: None,
            externally_modified: false,
            externally_deleted: false,
            backed_up_revision: None,
            disk_blob: None,
            virtual_source: None,
        }
    }

    /// A **virtual** document: pathless, read-only, content materialised from a revision
    /// ([`VirtualSource`]). Structurally a scratch with content and a title — the difference that
    /// matters is that nothing may write to it, and nothing should try to persist it.
    ///
    /// `language` and `generated` are alternatives, not companions: a file at a revision has a
    /// grammar and gets a live tree, a generated patch has neither and carries
    /// [`crate::patch::GeneratedPatch`] instead.
    /// `force_defer` skips the inline parse whatever the size. A buffer opened because a *view*
    /// needs it is the case: any one file in a patch may be comfortably parseable, but forty of
    /// them are not, and the view has to appear now. Deferring is otherwise decided exactly as it
    /// is for file-backed documents — by size — so an ordinary `git/show` still arrives
    /// highlighted with no restyle flash.
    pub fn virtual_content(
        id: DocumentId,
        source: VirtualSource,
        text: String,
        language: Option<String>,
        generated: Option<crate::patch::GeneratedPatch>,
        force_defer: bool,
    ) -> Self {
        let text = ropey::Rope::from_str(&text);
        let defer_parse = force_defer
            || language
                .as_deref()
                .is_some_and(|name| !sync_parse_affordable(name, text.len_bytes()));
        let syntax = if defer_parse {
            None
        } else {
            language
                .as_deref()
                .and_then(|name| make_syntax(&text, name))
        };
        let indent_style = resolve_indent_style(&text, language.as_deref());
        Document {
            id,
            canonical_path: None,
            virtual_source: Some(source),
            text,
            revision: 0,
            language,
            dirty: false,
            line_ending: LineEnding::Lf,
            last_modified_unix_ms: None,
            syntax,
            syntax_pending: defer_parse,
            generated,
            indent_style,
            saved_revision: Some(0),
            next_revision_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_group: None,
            externally_modified: false,
            externally_deleted: false,
            backed_up_revision: None,
            disk_blob: None,
            last_shift: None,
        }
    }

    /// Whether this document refuses edits, saves and reloads — true exactly for the virtual ones.
    pub fn read_only(&self) -> bool {
        self.virtual_source.is_some()
    }

    /// Content for a scratch buffer: empty, pathless. The scratch's per-workspace display number
    /// lives on its [`Buffer`] — a scratch document is never shared.
    pub fn scratch(id: DocumentId, language: Option<String>) -> Self {
        let text = ropey::Rope::new();
        let syntax = language
            .as_deref()
            .and_then(|name| make_syntax(&text, name));
        let indent_style = resolve_indent_style(&text, language.as_deref());
        Document {
            id,
            canonical_path: None,
            text,
            revision: 0,
            language,
            dirty: false,
            line_ending: LineEnding::Lf,
            last_modified_unix_ms: None,
            syntax,
            syntax_pending: false,
            last_shift: None,
            generated: None,
            indent_style,
            // Treat empty scratch as "clean"; first edit makes it dirty.
            saved_revision: Some(0),
            next_revision_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_group: None,
            externally_modified: false,
            externally_deleted: false,
            backed_up_revision: None,
            disk_blob: None,
            virtual_source: None,
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
    /// `cursors_before_edit` is the `(client, buffer)` cursor map captured before this edit —
    /// across every buffer attached to this document; it's stored in the undo entry when a new
    /// group opens, so `Document::undo` can restore cursors.
    ///
    /// Private: reached only through [`Editable::apply_edit`], which is what makes the read-only
    /// check unskippable.
    fn apply_edit(
        &mut self,
        start_char: usize,
        end_char: usize,
        insert_text: &str,
        kind: EditKindTag,
        cursors_before_edit: std::collections::HashMap<(ClientId, BufferId), CursorState>,
    ) -> Revision {
        self.snapshot_disk_text();
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

        // Where this edit lands, in lines, before the rope moves under us. Removing a range that
        // spans N line breaks and inserting text containing M of them is a delta of M − N.
        let shift = {
            let at = self.text.char_to_line(start_char) as u32;
            let removed =
                (self.text.char_to_line(end_char) - self.text.char_to_line(start_char)) as i32;
            let added = insert_text.matches('\n').count() as i32;
            LineShift {
                at,
                delta: added - removed,
            }
        };
        if start_char < end_char {
            self.text.remove(start_char..end_char);
        }
        if !insert_text.is_empty() {
            self.text.insert(start_char, insert_text);
        }
        self.last_shift = (shift.delta != 0).then_some(shift);
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
            // layers were touched) would need its own diff bookkeeping. Gated on the language
            // actually declaring injections — the contiguous source copy is O(buffer) per edit.
            if syntax.config.injection_query.is_some() {
                let source: String = text.chunks().collect();
                syntax.injections =
                    syntax::compute_injections(syntax.config, &syntax.tree, &source);
            }
        }

        self.revision
    }

    /// Write the buffer to disk atomically: write to `<dir>/.aether-tmp-<pid>-<name>`,
    /// fsync, rename onto `target`, fsync the parent directory. Restores CRLF if the buffer
    /// was loaded with CRLF endings. Updates `canonical_path`, `dirty`, `last_modified_unix_ms`.
    ///
    /// Returns the post-save mtime in unix milliseconds.
    ///
    /// Not behind [`Editable`]: this doesn't touch the text, and a read-only document has to be
    /// refused by `buffer/save` before it resolves a save-as path, not here at the end of it.
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
    ///
    /// Private: reached only through [`Editable::reload_from_disk`].
    fn reload_from_disk(&mut self) -> std::io::Result<u64> {
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

    /// Overlay restored unsaved content (from a backup) onto a freshly-constructed buffer. The
    /// buffer's current text/revision — set by `load_from_file` (the on-disk baseline), `new_at_path`
    /// (empty), or `scratch` (empty) — stays as the **saved** baseline (`saved_revision`); `content`
    /// becomes the live text at a fresh revision, so the buffer comes back **dirty** with exactly the
    /// unsaved edits that were in flight. Undo history isn't reconstructed (hot-exit restores content
    /// and dirty state only). Re-parses syntax from scratch since the whole rope is replaced.
    ///
    /// Not behind [`Editable`]: this hydrates a document still under construction, before it's
    /// inserted into [`ServerState`] and before any buffer views it, so there's no `BufferId` to
    /// gate on — and the callers built it from a file or a scratch, never from a revision.
    pub fn restore_unsaved(&mut self, content: &str) {
        // The caller loaded the file first, so `text` is still the disk content the backup is
        // about to diverge from — exactly what the saved-file baseline wants.
        self.snapshot_disk_text();
        self.text = ropey::Rope::from_str(content);
        self.revision = self.next_revision_id;
        self.next_revision_id += 1;
        self.active_group = None;
        self.recompute_dirty();
        self.reparse_full();
    }

    /// Private: reached only through [`Editable::undo`].
    fn undo(
        &mut self,
        current_cursors: std::collections::HashMap<(ClientId, BufferId), CursorState>,
    ) -> Option<UndoOutcome> {
        // Undoing away from the saved point dirties a clean document, so the snapshot has to be
        // taken here too — `text` is still the on-disk content until the entry is swapped in.
        self.snapshot_disk_text();
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

    /// Private: reached only through [`Editable::redo`].
    fn redo(
        &mut self,
        current_cursors: std::collections::HashMap<(ClientId, BufferId), CursorState>,
    ) -> Option<UndoOutcome> {
        // Symmetric with `undo`: redoing forward off the saved point dirties a clean document.
        self.snapshot_disk_text();
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
        if !self.dirty {
            // Back in step with disk, so the snapshot has nothing left to describe. Dropping it
            // here rather than only on save is what makes undoing back to the saved point clear
            // the saved-file diff too.
            self.disk_blob = None;
        }
    }

    /// Snapshot the on-disk text, for the saved-file diff baseline (`git/set_baseline` with
    /// [`aether_protocol::git::GitBaselineChoice::Saved`]).
    ///
    /// **Must be the first statement of any mutator that replaces or edits `text`.** It reads
    /// `self.text`, which is the on-disk content only while the document is still clean — once
    /// the mutation lands, the saved bytes are gone. Reading the file back at diff time instead
    /// would put I/O on the keystroke path and race the watcher; this is exact and free of both.
    ///
    /// Idempotent and self-limiting: it captures at most once per clean→dirty transition, an
    /// already-dirty document falls straight through, and [`Self::recompute_dirty`] drops the
    /// snapshot the moment the document is clean again. So the cost is one materialisation of a
    /// document you have actually started editing, and clean documents pay nothing.
    fn snapshot_disk_text(&mut self) {
        if self.disk_blob.is_none() && self.saved_revision == Some(self.revision) {
            self.disk_blob = Some(self.text.chunks().collect::<String>().into_bytes());
        }
    }

    /// Swap a generated document's content in place — rebuilding the working-changes patch after
    /// the working tree, or the index, moved under it.
    ///
    /// Bumps the revision, because viewport pushes are revision-guarded and would otherwise be
    /// discarded as stale. Moves `saved_revision` with it, because the document stays **clean**: a
    /// read-only buffer has nothing to save, and leaving the two apart would show a dirty marker
    /// for content the user never edited.
    /// Restricted to this module on purpose: rebuilding a patch changes which regions it has, so
    /// every viewport showing it needs its element bindings re-derived. Going through
    /// [`ServerState::replace_generated`] is what makes the pair unskippable rather than something
    /// a third rebuild path has to remember.
    pub(in crate::state) fn replace_generated(
        &mut self,
        text: &str,
        generated: Option<crate::patch::GeneratedPatch>,
    ) {
        self.text = ropey::Rope::from_str(text);
        self.generated = generated;
        self.revision += 1;
        self.saved_revision = Some(self.revision);
        self.recompute_dirty();
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

/// A document that has been checked to accept mutation — the only handle through which a
/// document's text can change.
///
/// Every text-changing operation lives here rather than on [`Document`], and the only constructor
/// is [`ServerState::editable_doc`], which refuses a read-only document. That makes the refusal a
/// property of the type rather than a rule each handler has to remember: a new edit handler
/// inherits it by construction (there is nothing else to call), and a new *source* of
/// read-only-ness — see [`Document::read_only`] — applies to the whole edit surface at once.
///
/// Reads go through `Deref`, so the usual "edit, then measure the result" call sites keep working
/// off the same binding. Field-level bookkeeping that isn't an edit (`dirty`, `backed_up_revision`,
/// the external-change flags) still goes through [`ServerState::doc_of_mut`].
pub struct Editable<'a>(&'a mut Document);

impl std::ops::Deref for Editable<'_> {
    type Target = Document;

    fn deref(&self) -> &Document {
        self.0
    }
}

impl Editable<'_> {
    /// See [`Document::apply_edit`] — the one text replacement primitive.
    pub fn apply_edit(
        &mut self,
        start_char: usize,
        end_char: usize,
        insert_text: &str,
        kind: EditKindTag,
        cursors_before_edit: std::collections::HashMap<(ClientId, BufferId), CursorState>,
    ) -> Revision {
        self.0
            .apply_edit(start_char, end_char, insert_text, kind, cursors_before_edit)
    }

    /// See [`Document::undo`].
    pub fn undo(
        &mut self,
        current_cursors: std::collections::HashMap<(ClientId, BufferId), CursorState>,
    ) -> Option<UndoOutcome> {
        self.0.undo(current_cursors)
    }

    /// See [`Document::redo`].
    pub fn redo(
        &mut self,
        current_cursors: std::collections::HashMap<(ClientId, BufferId), CursorState>,
    ) -> Option<UndoOutcome> {
        self.0.redo(current_cursors)
    }

    /// See [`Document::reload_from_disk`].
    pub fn reload_from_disk(&mut self) -> std::io::Result<u64> {
        self.0.reload_from_disk()
    }
}

/// The language name for a path, or `None` if we have no grammar for it. Detection itself lives
/// in [`syntax::config_for_path`] — file-name rules plus the registry's alias table — so there's
/// no second extension list here to drift out of step with the one languages are registered in.
fn detect_language(path: &Path) -> Option<String> {
    Some(syntax::config_for_path(path)?.name.to_string())
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

/// Whether a full parse of `bytes` in `language` is cheap enough to run inline on the open
/// round-trip. Plain grammars parse at roughly 10ms per 100 KB in release builds, so files up to
/// [`SYNC_PARSE_LIMIT_BYTES`] stay synchronous — the first frame arrives highlighted and nothing
/// restyles. Grammars with injections (markdown) additionally sub-parse every injection region,
/// measured at ~2.5ms per KB on fence-dense documents, so their limit is far lower.
fn sync_parse_affordable(language: &str, bytes: usize) -> bool {
    const SYNC_PARSE_LIMIT_BYTES: usize = 128 * 1024;
    const SYNC_PARSE_LIMIT_INJECTION_BYTES: usize = 8 * 1024;
    let limit = match syntax::get_config(language) {
        None => return true, // no grammar — nothing to parse, nothing to defer
        Some(cfg) if cfg.injection_query.is_some() => SYNC_PARSE_LIMIT_INJECTION_BYTES,
        Some(_) => SYNC_PARSE_LIMIT_BYTES,
    };
    bytes <= limit
}

pub(crate) fn make_syntax(text: &ropey::Rope, language: &str) -> Option<BufferSyntax> {
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
    /// Notifications actually written to this client's socket. Observability, and the only way to
    /// tell "the server pushed nothing" apart from "the writer kept up" when a test is trying to
    /// establish backpressure — the channel's own capacity reads the same either way.
    pub pushes_written: Arc<std::sync::atomic::AtomicU64>,
    /// The workspace this client is currently working in. `None` between connect and the first
    /// successful `workspace/activate`. Updated on every `workspace/activate`.
    pub active_workspace: Option<String>,
}

/// One client's presentation of one view.
///
/// The fields split two ways, and the split is the point of the struct rather than an accident:
/// **per-view** state (scroll, wrap, the pane's height) belongs to the presentation as a whole,
/// while **per-element** state (which buffer, at what width, showing which lines) belongs to each
/// editor the view is composed of. A view holds its editors inside one scroller, so there is one
/// scroll position and N windows into N buffers — not N viewports.
///
/// A patch view already holds several elements — one per region its chrome divides it into — and
/// the renderer draws each from its own binding. What no *produced* view does yet is bind those
/// elements to **different buffers**; that arrives with the patch driver, and the machinery for it
/// is already exercised by `handlers::viewport::tests::elements_render_from_their_own_buffers`.
pub struct Viewport {
    pub id: ViewportId,
    pub client_id: ClientId,
    /// The **view** this presents — what `viewport/subscribe` named.
    ///
    /// Distinct from the buffers its elements window, and not always one of them: a working-changes
    /// patch's elements are all real files, so nothing here binds the patch itself. Without this a
    /// viewport could not answer "are you showing this view?", and pushes about the view — its
    /// dirty state, its rebuild — would reach nobody.
    pub view_id: ViewId,

    // ---- per-view ----
    pub rows: u32,
    pub overscan_rows: u32,
    pub scroll_view_line: ViewLine,
    pub scroll_sub_row: f32,
    pub wrap: WrapMode,
    pub tab_width: u32,
    /// Inline diff view: when on, rendered windows interleave phantom baseline rows from the
    /// buffer's Git hunks and the hunks are recomputed on every edit. Per-viewport so two views of
    /// the same buffer can differ. Toggled by `git/set_diff_view`.
    pub diff_view: bool,
    /// First logical line currently pushed to the client (inclusive).
    ///
    /// Per-**view**, not per-element: a view scrolls as one, so there is a single visible range
    /// across the whole tree. It lived on the element binding until multi-element views needed the
    /// binding to mean something stable, and a field rewritten on every scroll can't also be an
    /// identity — see [`ElementBinding`].
    pub first_view_line: ViewLine,
    /// Last logical line currently pushed to the client (exclusive).
    pub last_view_line_exclusive: ViewLine,

    /// Which element holds the live cursor.
    ///
    /// Per-view, because a view has one cursor and it is in exactly one element. It stays 0 while
    /// every element of a view windows the same buffer — which is why storing it earlier would have
    /// been inert — and becomes load-bearing the moment they don't: it is what decides which
    /// *buffer* an edit, a search, a motion or an undo acts on.
    pub focused: aether_protocol::viewport::FieldId,

    // ---- per-element ----
    pub elements: Vec<ElementBinding>,
}

/// What a *view* says about the lines of one of its elements, overriding what the element's own
/// buffer would say.
///
/// A patch's hunk windows a real file, but what it shows about that file is the **diff's** view of
/// it — which lines this commit added, and what it removed — not the file's current working-tree
/// state. Without this an element would render its buffer's own diff against HEAD, which for a
/// historical commit answers a completely different question, and for the working tree answers the
/// right one only by coincidence.
///
/// Absent for an ordinary editor view, where a buffer describing itself is exactly right.
#[derive(Debug, Default)]
pub struct ElementDecorations {
    /// Per line: what happened to it, and which layer it sits in.
    pub markers: std::collections::HashMap<
        u32,
        (
            aether_protocol::viewport::DiffMarker,
            aether_protocol::viewport::DiffStage,
        ),
    >,
    /// Per line: the removed lines that sat above it, as phantom rows. This is where a patch's
    /// `-` lines go once they stop being buffer lines of a generated document.
    ///
    /// Rendered only while the viewport's inline diff is on — the same toggle an ordinary editor's
    /// phantoms answer to. With it off, a patch's hunk reads as its file's changed region and the
    /// removed content collapses onto [`Self::markers`], which are ungated. What a *view* says
    /// still wins over what a buffer would say about itself; the toggle decides whether either gets
    /// asked.
    pub baseline_above: std::collections::HashMap<u32, Vec<aether_protocol::viewport::BaselineRow>>,
    /// Per line: the byte ranges that differ from the line it replaced. Gated with
    /// [`Self::baseline_above`]: it describes a comparison the collapsed view isn't drawing.
    pub emphasis: std::collections::HashMap<u32, Vec<aether_protocol::viewport::EmphasisRange>>,
}

/// Which buffer an element's lines are lines *of*, and which lines they are.
///
/// The two variants were once one nullable id beside a bare `u32` pair, and the pair meant different
/// things depending on the id: **file** lines while it was `Some`, lines of the **generated patch**
/// once it was `None`. Nothing named that, and the one loop walking both kinds
/// ([`ServerState::shift_element_extents`]) was correct only because its filter happened to skip the
/// unbound ones. Naming it is what stops the next reader — or the next repair — reinterpreting one
/// as the other, which would be a silently wrong window rather than a crash.
#[derive(Debug, Clone)]
pub enum ElementExtent {
    /// Lines of `buffer`: a hunk's window onto the real file it came from.
    Bound {
        buffer: BufferId,
        lines: std::ops::Range<u32>,
    },
    /// Lines of the **view's own** document, for content with no file to point at — a deleted
    /// file's text, a binary swap, a mode change — and for every element of a view no driver has
    /// built.
    ///
    /// The buffer is unnamed rather than unknown: a layout is built *before* the view's own buffer
    /// is opened, so there is no id to record yet. [`Self::buffer`] is the one place it is supplied.
    OwnDocument { lines: std::ops::Range<u32> },
}

impl ElementExtent {
    /// The lines themselves, whichever buffer they belong to.
    pub fn lines(&self) -> std::ops::Range<u32> {
        match self {
            Self::Bound { lines, .. } | Self::OwnDocument { lines } => lines.clone(),
        }
    }

    /// The buffer these lines index, given the view they belong to. **The only place an
    /// `OwnDocument` extent acquires an id**, which is what keeps the two kinds from being confused
    /// at any of the sites that merely want "which buffer".
    pub fn buffer(&self, view_buffer: BufferId) -> BufferId {
        match self {
            Self::Bound { buffer, .. } => *buffer,
            Self::OwnDocument { .. } => view_buffer,
        }
    }

    /// The buffer this element is bound to, if it is bound to one — for the callers that must treat
    /// "windows a real file" differently from "windows the view's own text", rather than merely
    /// needing an id.
    pub fn bound_to(&self) -> Option<BufferId> {
        match self {
            Self::Bound { buffer, .. } => Some(*buffer),
            Self::OwnDocument { .. } => None,
        }
    }

    /// Slide or stretch the extent for an edit that changed a line count. See
    /// [`ServerState::shift_element_extents`] for the three cases.
    fn shift(&mut self, at: u32, delta: i32) {
        let lines = match self {
            Self::Bound { lines, .. } | Self::OwnDocument { lines } => lines,
        };
        if at < lines.start {
            lines.start = lines.start.saturating_add_signed(delta);
            lines.end = lines.end.saturating_add_signed(delta);
        } else if at < lines.end {
            lines.end = lines.end.saturating_add_signed(delta);
        }
    }
}

/// How a view divides into elements: each one's extent, the chrome introducing it, and — once a
/// driver builds them — what the view says about its lines.
pub struct ElementLayout {
    pub extent: ElementExtent,
    pub chrome_above: std::sync::Arc<Vec<aether_protocol::viewport::Element>>,
    pub decorations: Option<std::sync::Arc<ElementDecorations>>,
}

impl ElementLayout {
    /// Bind this element against the view it belongs to — the point at which an `OwnDocument`
    /// extent is told which buffer it is a slice of.
    pub fn bind(
        &self,
        view_buffer: BufferId,
        cols: u32,
        continuation_marker_width: u32,
    ) -> ElementBinding {
        let lines = self.extent.lines();
        ElementBinding {
            buffer_id: self.extent.buffer(view_buffer),
            cols,
            continuation_marker_width,
            start_line: lines.start,
            end_line_exclusive: lines.end,
            decorations: self.decorations.clone(),
            chrome_above: self.chrome_above.clone(),
        }
    }
}

/// One editor element of a view: a window onto a buffer, at its own width.
///
/// **Identity, not geometry.** Everything here is stable while the view is open — which buffer the
/// element shows and which slice of it — so an [`aether_protocol::viewport::FieldId`] indexing
/// into `elements` keeps naming the same region across a scroll. Where the *viewport* is currently
/// scrolled to is [`Viewport::first_view_line`], one level up.
#[derive(Debug, Clone)]
pub struct ElementBinding {
    pub buffer_id: BufferId,
    pub cols: u32,
    pub continuation_marker_width: u32,
    /// The element's extent in its buffer: `start_line..end_line_exclusive`. A whole-buffer element
    /// — every view but a patch, today — spans `0..line_count`.
    pub start_line: u32,
    pub end_line_exclusive: u32,
    /// What the view says about these lines, if it has an opinion. `Arc` because bindings are
    /// cloned per render and this is the one field with any size to it.
    pub decorations: Option<std::sync::Arc<ElementDecorations>>,
    /// Chrome drawn above this element — a file separator, a hunk heading — as sibling nodes.
    ///
    /// Held by the *element* rather than looked up by line in a generated document, because that
    /// lookup was the last thing tying a view's structure to a document's line space. An element
    /// whose content comes from a real file has no line in the patch to anchor its heading to.
    pub chrome_above: std::sync::Arc<Vec<aether_protocol::viewport::Element>>,
}

/// Where each of a view's elements sits in the view's own line space — **the one place the two
/// spaces meet.**
///
/// A view line is an index into the concatenation of its elements' extents; a buffer line is a line
/// of some file. They coincide exactly when a view is a single whole-buffer element, which is every
/// view but a patch — which is why passing one where the other was meant stayed invisible for so
/// long, and then produced blank viewports, unreachable last lines and two `index past end of Rope`
/// panics. Crossing between them now means calling something here. See [`aether_protocol::coords`].
///
/// Built against the buffers' **live** line counts, so an extent that has gone stale (the diff said
/// a hunk was seven lines; the file has since been edited) is clamped at construction rather than
/// indexing past the end of a rope somewhere downstream. A short window is a far better answer than
/// a crash, and doing it once here beats doing it at each use.
pub struct ViewLayout {
    spans: Vec<ElementSpan>,
}

/// One element's place in a view: where it starts in each space, and how many lines it contributes.
#[derive(Debug, Clone, Copy)]
struct ElementSpan {
    view_start: ViewLine,
    buffer_start: u32,
    lines: u32,
}

/// A range of **buffer** lines that is already bounded by its buffer's live length.
///
/// Only [`ViewLayout`] can make one, and that is the entire point. The render helpers that take it
/// used to take a bare `(start, end_exclusive)` pair and re-clamp defensively, because nothing in
/// the type said whether the caller had already done it — and one caller genuinely had not, passing
/// an element's raw extents straight from the diff. The clamp now happens once, at
/// [`ViewLayout::of`], and a helper that receives this needs no opinion about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferRange {
    start: u32,
    end_exclusive: u32,
}

impl BufferRange {
    pub fn start(&self) -> u32 {
        self.start
    }

    pub fn end_exclusive(&self) -> u32 {
        self.end_exclusive
    }

    pub fn len(&self) -> u32 {
        self.end_exclusive.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.end_exclusive <= self.start
    }

    /// The lines themselves, for walking one at a time.
    pub fn lines(&self) -> std::ops::Range<u32> {
        self.start..self.end_exclusive
    }
}

impl ViewLayout {
    /// Lay out `elements` in view order. `doc_lines` gives a buffer's current line count — the
    /// clamp that keeps a stale extent from outliving the lines it described.
    pub fn of(elements: &[ElementBinding], mut doc_lines: impl FnMut(BufferId) -> u32) -> Self {
        let mut spans = Vec::with_capacity(elements.len());
        let mut view_start = ViewLine::ZERO;
        for binding in elements {
            let available = doc_lines(binding.buffer_id);
            let buffer_start = binding.start_line.min(available);
            let lines = binding
                .end_line_exclusive
                .saturating_sub(binding.start_line)
                .min(available.saturating_sub(buffer_start));
            spans.push(ElementSpan {
                view_start,
                buffer_start,
                lines,
            });
            view_start = view_start.saturating_add(lines);
        }
        Self { spans }
    }

    /// How many lines the **view** has: its elements' extents, summed.
    ///
    /// Not any document's line count. A bound patch's generated text is far longer than the view
    /// built over it — only new-side lines are windowed, removals became phantoms — so ranging
    /// against the document lets the scroll run past the view's real end, where every element clips
    /// to nothing and the screen goes blank.
    pub fn line_count(&self) -> u32 {
        self.spans
            .iter()
            .map(|s| s.lines)
            .fold(0u32, u32::saturating_add)
    }

    /// The view-line extent of one element: `start..end_exclusive`.
    pub fn span_of(
        &self,
        element: aether_protocol::viewport::FieldId,
    ) -> Option<(ViewLine, ViewLine)> {
        let s = self.spans.get(element as usize)?;
        Some((s.view_start, s.view_start.saturating_add(s.lines)))
    }

    /// The buffer-line range of the part of `element` lying inside the view-line window
    /// `first..last_excl`, or `None` when the element is entirely outside it.
    ///
    /// This is what a renderer wants: intersect in *view* coordinates, then offset into the
    /// element's own buffer. For a single whole-buffer element the two coincide, which is exactly
    /// why doing it by hand went unnoticed.
    pub fn intersect(
        &self,
        element: aether_protocol::viewport::FieldId,
        first: ViewLine,
        last_excl: ViewLine,
    ) -> Option<BufferRange> {
        let s = self.spans.get(element as usize)?;
        let view_end = s.view_start.saturating_add(s.lines);
        let lo = first.max(s.view_start);
        let hi = last_excl.min(view_end);
        if lo >= hi {
            return None;
        }
        Some(BufferRange {
            start: s.buffer_start + s.view_start.distance_to(lo),
            end_exclusive: s.buffer_start + s.view_start.distance_to(hi),
        })
    }

    /// The whole of `element`'s window into its buffer, in view order or not — what its *height* is
    /// measured over, as distinct from [`Self::intersect`]'s "the part currently on screen".
    ///
    /// Empty for an element that has fallen off the end of a buffer that shrank under it.
    pub fn element_range(&self, element: aether_protocol::viewport::FieldId) -> BufferRange {
        self.spans
            .get(element as usize)
            .map(|s| BufferRange {
                start: s.buffer_start,
                end_exclusive: s.buffer_start.saturating_add(s.lines),
            })
            .unwrap_or(BufferRange {
                start: 0,
                end_exclusive: 0,
            })
    }

    /// The view line at which `element`'s buffer line `line` sits — the inverse of
    /// [`Self::intersect`]. `None` when the line is outside the element's extent.
    pub fn to_view(
        &self,
        element: aether_protocol::viewport::FieldId,
        line: u32,
    ) -> Option<ViewLine> {
        let s = self.spans.get(element as usize)?;
        let offset = line.checked_sub(s.buffer_start)?;
        (offset < s.lines).then(|| s.view_start.saturating_add(offset))
    }

    /// Which element a **view** line falls in — the inverse of [`Self::span_of`].
    ///
    /// What "you are looking at this line" means in element terms, and so how a subscribe decides
    /// which element holds the cursor: a view is scrolled somewhere on purpose, and the element
    /// covering that line is the one being read. `None` when the line is past the view's end.
    pub fn element_at(&self, line: ViewLine) -> Option<aether_protocol::viewport::FieldId> {
        self.spans
            .iter()
            .position(|s| line >= s.view_start && line < s.view_start.saturating_add(s.lines))
            .map(|i| i as aether_protocol::viewport::FieldId)
    }

    /// The buffer line an element's window starts at, whether or not any of it is in view — what a
    /// rendered node reports as its `first_buffer_line`.
    pub fn buffer_start(&self, element: aether_protocol::viewport::FieldId) -> u32 {
        self.spans
            .get(element as usize)
            .map(|s| s.buffer_start)
            .unwrap_or(0)
    }
}

impl Viewport {
    /// The element holding the live cursor.
    ///
    /// This replaced a `sole()` accessor that returned element 0 and was named for the assumption
    /// it encoded, so that the audit would be a grep rather than a guess. Working through those
    /// call sites is what showed almost all of them wanted *the view's* buffer — the one being
    /// edited and searched — rather than the first element's, and those two only diverge once
    /// focus exists. Falls back to the first element if `focused` is somehow out of range, since a
    /// view always has at least one and a panic here would be a strange way to report a stale id.
    pub fn focus(&self) -> &ElementBinding {
        self.elements
            .get(self.focused as usize)
            .unwrap_or(&self.elements[0])
    }

    /// The buffer this view is currently acting on: the focused element's.
    pub fn buffer_id(&self) -> BufferId {
        self.focus().buffer_id
    }

    /// Whether any of this viewport's elements windows `buffer_id` — "is this buffer on screen?".
    /// The one question about a viewport that stays meaningful once views bind several buffers.
    pub fn binds(&self, buffer_id: BufferId) -> bool {
        self.elements.iter().any(|e| e.buffer_id == buffer_id)
    }

    /// Whether this viewport is showing `id` at all — as the view it presents, or as one of the
    /// buffers its elements window. The right question for anything fanning out *to viewers*,
    /// because a patch's viewers are watching the view even though no element windows it.
    pub fn shows(&self, id: BufferId) -> bool {
        self.view_id.presenting_buffer() == id || self.binds(id)
    }

    /// Every buffer this viewport shows, each named once — [`Self::shows`] enumerated rather than
    /// asked.
    ///
    /// What a viewport being torn down was keeping alive, and so what the transient GC must
    /// consider. Its callers used to build that list from [`Self::buffer_id`], the *focused
    /// element's* buffer — which for a composed view is one of the files and never the patch, so
    /// navigating away from working changes left the patch document and every unfocused element's
    /// buffer behind with nothing showing them and nothing looking for them.
    pub fn shown_buffers(&self) -> Vec<BufferId> {
        let mut out = vec![self.view_id.presenting_buffer()];
        for e in &self.elements {
            if !out.contains(&e.buffer_id) {
                out.push(e.buffer_id);
            }
        }
        out
    }

    /// This viewport's wrap-layout inputs for its focused element, bundled for the motion/render
    /// paths.
    pub fn wrap_geometry(&self) -> crate::wrap::WrapGeometry {
        crate::wrap::WrapGeometry {
            wrap: self.wrap,
            cols: self.focus().cols,
            marker_width: self.focus().continuation_marker_width,
            tab_width: self.tab_width,
        }
    }
}

#[cfg(test)]
mod view_layout_tests {
    use super::*;

    /// Two elements over two different files, each windowing lines 10..13 of its own — the shape
    /// that makes view lines and buffer lines diverge, and the one every coordinate bug was hiding
    /// behind. A whole-buffer element would make the two spaces coincide and prove nothing.
    fn two_hunks() -> Vec<ElementBinding> {
        let binding = |buffer_id| ElementBinding {
            buffer_id,
            cols: 80,
            continuation_marker_width: 0,
            start_line: 10,
            end_line_exclusive: 13,
            decorations: None,
            chrome_above: Default::default(),
        };
        vec![binding(1), binding(2)]
    }

    #[test]
    fn a_view_is_as_long_as_its_elements_together() {
        let layout = ViewLayout::of(&two_hunks(), |_| 100);
        assert_eq!(layout.line_count(), 6);
        assert_eq!(layout.span_of(0), Some((ViewLine(0), ViewLine(3))));
        assert_eq!(layout.span_of(1), Some((ViewLine(3), ViewLine(6))));
    }

    /// The crossing, both ways. View line 4 is the second element's *second* line, which that
    /// element's own file calls line 11 — and both files have a line 11, which is exactly why the
    /// element has to be part of the question.
    #[test]
    fn a_view_line_resolves_into_the_element_that_owns_it() {
        let layout = ViewLayout::of(&two_hunks(), |_| 100);
        assert_eq!(
            layout
                .intersect(1, ViewLine(4), ViewLine(5))
                .map(|r| (r.start(), r.end_exclusive())),
            Some((11, 12))
        );
        assert_eq!(layout.to_view(1, 11), Some(ViewLine(4)));
        // Element 0 has the same buffer line, and it is a different view line.
        assert_eq!(layout.to_view(0, 11), Some(ViewLine(1)));
    }

    /// An edit inside a hunk grows it; one above slides it; one below leaves it alone. The same
    /// three answers a re-diff would give, without re-diffing on every keystroke.
    #[test]
    fn an_edit_moves_the_elements_it_lands_in_and_the_ones_below_it() {
        let extents = |elements: &[ElementBinding]| -> Vec<(u32, u32)> {
            elements
                .iter()
                .map(|e| (e.start_line, e.end_line_exclusive))
                .collect()
        };
        let mut s = ServerState::new();
        let mut file = |id: BufferId, text: &str| {
            let text = text.to_string();
            s.insert_buffer_with_document(id, None, false, |d| {
                let mut doc = Document::scratch(d, None);
                doc.text = ropey::Rope::from_str(&text);
                doc
            });
            id
        };
        let (a, b) = (
            file(1, &"fn one() {}\n".repeat(40)),
            file(2, &"fn two() {}\n".repeat(40)),
        );
        // Two hunks of file `a` plus one of file `b` — the third must not move when `a` is edited.
        let binding = |buffer_id, start, end| ElementBinding {
            buffer_id,
            cols: 80,
            continuation_marker_width: 0,
            start_line: start,
            end_line_exclusive: end,
            decorations: None,
            chrome_above: Default::default(),
        };
        let bound = |buffer, lines: std::ops::Range<u32>| ElementLayout {
            extent: ElementExtent::Bound { buffer, lines },
            chrome_above: Default::default(),
            decorations: None,
        };
        let view = file(3, "patch\n");
        s.set_view_layout(
            view,
            vec![
                bound(a, 10..14),
                bound(a, 30..34),
                bound(b, 10..14),
            ],
        );
        let vp = Viewport {
            id: 1,
            client_id: uuid::Uuid::new_v4(),
            view_id: ViewId(view),
            rows: 10,
            overscan_rows: 0,
            scroll_view_line: ViewLine::ZERO,
            scroll_sub_row: 0.0,
            wrap: WrapMode::None,
            tab_width: 4,
            diff_view: false,
            first_view_line: ViewLine::ZERO,
            last_view_line_exclusive: ViewLine(12),
            focused: 0,
            elements: vec![binding(a, 10, 14), binding(a, 30, 34), binding(b, 10, 14)],
        };
        s.viewports.insert(1, vp);

        // Two lines typed into the first hunk of `a`.
        s.shift_element_extents(a, LineShift { at: 12, delta: 2 });
        assert_eq!(
            extents(&s.viewports[&1].elements),
            vec![(10, 16), (32, 36), (10, 14)],
            "the hunk grew, the one below it slid, the other file's did not move"
        );
        // The stored layout moves with it, or a rebuild would resurrect the old extents.
        let layout = s.element_layout_of(view);
        assert_eq!(
            layout
                .iter()
                .map(|l| { let r = l.extent.lines(); (r.start, r.end) })
                .collect::<Vec<_>>(),
            vec![(10, 16), (32, 36), (10, 14)]
        );

        // A deletion below every element of `a` changes nothing.
        s.shift_element_extents(a, LineShift { at: 38, delta: -1 });
        assert_eq!(
            extents(&s.viewports[&1].elements),
            vec![(10, 16), (32, 36), (10, 14)]
        );
    }

    /// An element scrolled out of the window contributes nothing rather than a bogus range.
    #[test]
    fn an_element_outside_the_window_intersects_nothing() {
        let layout = ViewLayout::of(&two_hunks(), |_| 100);
        assert_eq!(layout.intersect(1, ViewLine(0), ViewLine(3)), None);
        assert_eq!(layout.to_view(1, 99), None);
    }

    /// An extent comes from a diff; the buffer it indexes is live. Once the file shrinks under it,
    /// trusting the extent is what indexed past the end of a rope and panicked the server — so the
    /// layout clamps at construction and the view simply gets shorter.
    #[test]
    fn a_stale_extent_is_clamped_to_the_buffer_that_is_actually_there() {
        // The second file has been cut down to 11 lines, so its 10..13 window holds only line 10.
        let layout = ViewLayout::of(&two_hunks(), |id| if id == 2 { 11 } else { 100 });
        assert_eq!(
            layout.line_count(),
            4,
            "3 from the first, 1 from the second"
        );
        assert_eq!(
            layout
                .intersect(1, ViewLine(3), ViewLine(6))
                .map(|r| (r.start(), r.end_exclusive())),
            Some((10, 11))
        );
        // And a file gone entirely from under an element contributes no lines at all.
        let gone = ViewLayout::of(&two_hunks(), |id| if id == 2 { 0 } else { 100 });
        assert_eq!(gone.line_count(), 3);
        assert_eq!(gone.intersect(1, ViewLine(0), ViewLine(9)), None);
    }
}

#[cfg(test)]
mod virtual_target_tests {
    use super::*;
    use aether_protocol::git::ShowTarget;

    /// The string form is an *encoding*, used only where a target has to be written down and read
    /// back (the session file). Every shape must survive the round trip, including the awkward
    /// ones: a repo path containing `@`, and the working tree — which is not a revision and must
    /// never decode as one.
    #[test]
    fn every_target_shape_survives_its_key() {
        let cases = [
            VirtualTarget::new(
                "/home/joe/proj",
                ShowTarget::Commit {
                    rev: "abc1234".into(),
                },
            ),
            VirtualTarget::new(
                "/home/joe/proj",
                ShowTarget::File {
                    rev: "abc1234".into(),
                    path: "src/a.rs".into(),
                },
            ),
            VirtualTarget::new("/home/joe/proj", ShowTarget::WorkingChanges),
            // A repo path with an `@` in it — why the key splits from the right.
            VirtualTarget::new(
                "/home/joe/w@rk/proj",
                ShowTarget::File {
                    rev: "abc1234".into(),
                    path: "src/a.rs".into(),
                },
            ),
        ];
        for target in cases {
            let key = target.key();
            assert_eq!(
                VirtualTarget::parse_key(&key),
                Some(target.clone()),
                "round trip through {key:?}"
            );
        }
    }

    /// Asking a shape for a field it hasn't got answers `None` — the whole point of the type. When
    /// this was a string every consumer re-split it by hand, and the working tree (which has no
    /// revision) came out looking like a commit named `worktree`.
    ///
    /// `is_immutable` is the one that earns its keep at runtime: it decides whether re-showing a
    /// target can attach to the buffer already holding it, or has to rebuild it.
    #[test]
    fn a_shape_without_a_field_answers_none() {
        let working = VirtualTarget::new("/proj", ShowTarget::WorkingChanges);
        assert_eq!(working.repo_id, "/proj");
        assert_eq!(working.rev(), None, "never hand this to rev-parse");
        assert_eq!(working.path(), None);
        assert!(!working.is_immutable(), "the worktree moves; rebuild it");

        let commit = VirtualTarget::new("/proj", ShowTarget::Commit { rev: "abc".into() });
        assert_eq!(commit.rev(), Some("abc"));
        assert_eq!(commit.path(), None, "a commit's diff spans many files");
        assert!(commit.is_immutable(), "a commit can't change; attach to it");

        let file = VirtualTarget::new(
            "/proj",
            ShowTarget::File {
                rev: "abc".into(),
                path: "a.rs".into(),
            },
        );
        assert_eq!(file.path(), Some("a.rs"));
        assert!(file.is_immutable());
    }
}

#[cfg(test)]
mod deferred_tests {
    use super::*;

    /// The barrier's whole point: a quiet server answers instantly, so waiting for quiescence
    /// costs nothing when there is nothing to wait for.
    #[tokio::test]
    async fn quiet_by_default_and_waiting_is_free() {
        let d = Arc::new(Deferred::default());
        assert!(d.is_quiet());
        tokio::time::timeout(std::time::Duration::from_millis(50), d.wait_quiet())
            .await
            .expect("an idle server resolves immediately");
    }

    #[tokio::test]
    async fn outstanding_work_holds_the_barrier_until_it_drops() {
        let d = Arc::new(Deferred::default());
        let token = d.start();
        assert!(!d.is_quiet());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), d.wait_quiet())
                .await
                .is_err(),
            "outstanding work must hold the barrier"
        );
        drop(token);
        assert!(d.is_quiet());
    }

    /// One provoking event can arm several follow-ups, so the token clones — and the work is only
    /// done when the last of them is.
    #[tokio::test]
    async fn clones_all_have_to_finish() {
        let d = Arc::new(Deferred::default());
        let a = d.start();
        let b = a.clone();
        drop(a);
        assert!(!d.is_quiet(), "one clone still outstanding");
        drop(b);
        assert!(d.is_quiet());
    }

    /// A waiter parked before the work finishes is woken by it — the case the `notified()`-before-
    /// check ordering in `wait_quiet` exists to protect.
    #[tokio::test]
    async fn a_waiter_is_woken_when_the_last_token_drops() {
        let d = Arc::new(Deferred::default());
        let token = d.start();
        let waiter = {
            let d = d.clone();
            tokio::spawn(async move { d.wait_quiet().await })
        };
        tokio::task::yield_now().await;
        drop(token);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("the waiter is woken, not left parked")
            .unwrap();
    }

    /// Independent units are counted, not collapsed: two armed refreshes both have to finish.
    #[tokio::test]
    async fn separate_units_are_counted_separately() {
        let d = Arc::new(Deferred::default());
        let one = d.start();
        let two = d.start();
        drop(one);
        assert!(!d.is_quiet());
        drop(two);
        assert!(d.is_quiet());
    }
}

#[cfg(test)]
mod workspace_state_tests {
    use super::*;

    /// Deleting a workspace must not take a *same-prefixed* one with it: `p` and `printer` are
    /// unrelated workspaces, and a prefix match would have destroyed the second. (Worth keeping
    /// after the variant ids that motivated the prefix check were removed — the hazard was real.)
    #[test]
    fn deleting_a_workspace_leaves_a_same_prefixed_one_alone() {
        let mut s = ServerState::new();
        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));
        s.workspaces.insert(
            "printer".into(),
            workspace_entry("printer", vec![PathBuf::from("/printer")]),
        );

        s.delete_workspace("p");
        assert!(!s.workspaces.contains_key("p"));
        assert!(s.workspaces.contains_key("printer"));
    }

    #[test]
    fn renaming_a_workspace_carries_its_buffers_and_bindings() {
        let mut s = ServerState::new();
        let mut bound = workspace_entry("p", vec![PathBuf::from("/store/p/feature")]);
        bound.base_paths = Some(vec![PathBuf::from("/p")]);
        s.workspaces.insert("p".into(), bound);
        s.buffer_workspaces.insert(7, "p".into());

        s.rename_workspace("p", "renamed").expect("renamed");
        let moved = s.workspaces.get("renamed").expect("the entry moved");
        assert_eq!(moved.name.as_deref(), Some("renamed"));
        // The remapped shape rides on the entry, so a rename can't strand it on the old key —
        // which is the whole reason bindings no longer need a cascade of their own.
        assert_eq!(
            moved.base_paths.as_deref(),
            Some(&[PathBuf::from("/p")][..])
        );
        assert!(!s.workspaces.contains_key("p"));
        assert_eq!(
            s.buffer_workspaces.get(&7).map(String::as_str),
            Some("renamed")
        );
    }

    fn workspace_entry(name: &str, paths: Vec<PathBuf>) -> WorkspaceEntry {
        WorkspaceEntry {
            worktrees: Default::default(),
            id: name.to_string(),
            name: Some(name.to_string()),
            base_paths: None,
            paths: paths.clone(),
            workspace_index: Arc::new(WorkspaceIndex::new(paths)),
            mru_buffers: VecDeque::new(),
            dormant_buffers: Vec::new(),
            jumplist: None,
            projects: Vec::new(),
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
                pushes_written: Default::default(),
                active_workspace: Some(active.to_string()),
            },
        )
    }

    /// The "rename while the workspace and its buffers are open" path: re-keys the workspace map (and
    /// updates `entry.name`), every buffer association, and every client's active-workspace pointer
    /// — while leaving buffers and unrelated workspaces untouched.
    #[test]
    fn rename_workspace_rekeys_buffers_and_clients() {
        let mut s = ServerState::new();
        s.workspaces.insert(
            "old".to_string(),
            workspace_entry("old", vec![PathBuf::from("/tmp/x")]),
        );
        s.workspaces
            .insert("other".to_string(), workspace_entry("other", vec![]));

        // A buffer in the renamed workspace, plus one in an unrelated workspace.
        let buf = s.allocate_buffer_id();
        s.buffer_workspaces.insert(buf, "old".to_string());
        let other_buf = s.allocate_buffer_id();
        s.buffer_workspaces.insert(other_buf, "other".to_string());

        let (c1, sess1) = session("old");
        s.clients.insert(c1, sess1);
        let (c2, sess2) = session("other");
        s.clients.insert(c2, sess2);

        let paths = s
            .rename_workspace("old", "new")
            .expect("workspace was loaded");
        assert_eq!(paths, vec!["/tmp/x".to_string()]);

        // Workspace map re-keyed; the entry's own name field follows.
        assert!(!s.workspaces.contains_key("old"));
        assert_eq!(s.workspaces.get("new").map(|p| p.id.as_str()), Some("new"));
        assert_eq!(
            s.workspaces.get("new").and_then(|p| p.name.as_deref()),
            Some("new")
        );
        assert!(s.workspaces.contains_key("other"));

        // The buffer is re-pointed but still present (nothing closed); the unrelated one is left.
        assert_eq!(
            s.buffer_workspaces.get(&buf).map(String::as_str),
            Some("new")
        );
        assert_eq!(
            s.buffer_workspaces.get(&other_buf).map(String::as_str),
            Some("other")
        );

        // Only the matching client's active-workspace pointer follows the rename.
        assert_eq!(
            s.clients.get(&c1).unwrap().active_workspace.as_deref(),
            Some("new")
        );
        assert_eq!(
            s.clients.get(&c2).unwrap().active_workspace.as_deref(),
            Some("other")
        );
    }

    /// Renaming a workspace that isn't loaded returns `None` (the handler maps this to an internal
    /// error; it can't happen in practice since workspaces are never unloaded at runtime).
    #[test]
    fn rename_workspace_unknown_returns_none() {
        let mut s = ServerState::new();
        assert!(s.rename_workspace("nope", "new").is_none());
    }

    /// The paths persisted for a workspace's session: live MRU buffers first (most-recent-first),
    /// then dormant ones, deduplicated by path so a dormant entry that's since been loaded doesn't
    /// double-show.
    #[test]
    fn session_buffer_paths_merges_live_mru_then_dormant_deduped() {
        let mut s = ServerState::new();
        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));

        // Two live file-backed buffers; touch so the MRU front is b2.
        let b1 = s.allocate_buffer_id();
        s.insert_buffer_with_document(b1, None, false, |d| {
            Document::new_at_path(d, PathBuf::from("/p/a.rs"), None)
        });
        s.buffer_workspaces.insert(b1, "p".into());
        let b2 = s.allocate_buffer_id();
        s.insert_buffer_with_document(b2, None, false, |d| {
            Document::new_at_path(d, PathBuf::from("/p/b.rs"), None)
        });
        s.buffer_workspaces.insert(b2, "p".into());
        s.touch_mru(b1);
        s.touch_mru(b2);

        // A transient preview at the MRU front: must be excluded (previews don't persist).
        let bt = s.allocate_buffer_id();
        s.insert_buffer_with_document(bt, None, true, |d| {
            Document::new_at_path(d, PathBuf::from("/p/preview.rs"), None)
        });
        s.buffer_workspaces.insert(bt, "p".into());
        s.touch_mru(bt);

        // A dirty scratch (must persist as a Scratch entry) and a clean scratch (must be dropped).
        let sc_dirty = s.allocate_buffer_id();
        s.insert_buffer_with_document(sc_dirty, Some(1), false, |d| {
            let mut doc = Document::scratch(d, None);
            doc.restore_unsaved("unsaved scratch text"); // makes it dirty
            doc
        });
        s.buffer_workspaces.insert(sc_dirty, "p".into());
        s.touch_mru(sc_dirty);
        let sc_clean = s.allocate_buffer_id();
        s.insert_buffer_with_document(sc_clean, Some(2), false, |d| Document::scratch(d, None));
        s.buffer_workspaces.insert(sc_clean, "p".into());
        s.touch_mru(sc_clean);

        // Dormant: a fresh path, plus one that duplicates a live buffer's path (must be dropped).
        let d1 = s.allocate_buffer_id();
        let d_dup = s.allocate_buffer_id();
        s.workspaces.get_mut("p").unwrap().dormant_buffers = vec![
            DormantBuffer {
                id: d1,
                source: DormantSource::File(PathBuf::from("/p/c.rs")),
            },
            DormantBuffer {
                id: d_dup,
                source: DormantSource::File(PathBuf::from("/p/a.rs")),
            },
        ];

        use crate::config::SessionBuffer;
        assert_eq!(
            s.session_buffers("p"),
            vec![
                // clean scratch (2) and preview.rs (transient) are both excluded; the dirty
                // scratch (MRU front) is kept.
                SessionBuffer::Scratch { number: 1 },
                SessionBuffer::File {
                    path: PathBuf::from("/p/b.rs")
                }, // most-recent non-transient file
                SessionBuffer::File {
                    path: PathBuf::from("/p/a.rs")
                },
                SessionBuffer::File {
                    path: PathBuf::from("/p/c.rs")
                }, // dormant; /p/a.rs dropped as a dup of the live buffer
            ]
        );
    }

    /// The dormant-registry helpers: `first_dormant_id` is the landing target (front of the list),
    /// `take_dormant` removes and returns the entry by id (materialization), and `promote_dormant`
    /// drops a file path once it's loaded.
    #[test]
    fn dormant_registry_take_promote_and_first() {
        let mut s = ServerState::new();
        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));
        let d1 = s.allocate_buffer_id();
        let d2 = s.allocate_buffer_id();
        s.workspaces.get_mut("p").unwrap().dormant_buffers = vec![
            DormantBuffer {
                id: d1,
                source: DormantSource::File(PathBuf::from("/p/a.rs")),
            },
            DormantBuffer {
                id: d2,
                source: DormantSource::File(PathBuf::from("/p/b.rs")),
            },
        ];

        assert_eq!(s.first_dormant_id("p"), Some(d1), "front of the list lands");
        assert_eq!(
            s.take_dormant("p", d1).map(|d| d.source),
            Some(DormantSource::File(PathBuf::from("/p/a.rs")))
        );
        assert!(
            s.take_dormant("p", d1).is_none(),
            "removed; a second take is empty"
        );
        assert_eq!(s.first_dormant_id("p"), Some(d2));
        s.promote_dormant("p", Path::new("/p/b.rs"));
        assert_eq!(
            s.first_dormant_id("p"),
            None,
            "promotion empties the registry"
        );
    }

    /// A restored *scratch* is a landing target like any other dormant buffer: it sits at the front
    /// of the session's buffer list when it's what you were last editing, and that's what a restart
    /// lands you on.
    #[test]
    fn first_dormant_id_includes_restored_scratches() {
        let mut s = ServerState::new();
        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));
        let scratch = s.allocate_buffer_id();
        let file = s.allocate_buffer_id();
        s.workspaces.get_mut("p").unwrap().dormant_buffers = vec![
            DormantBuffer {
                id: scratch,
                source: DormantSource::Scratch { number: 1 },
            },
            DormantBuffer {
                id: file,
                source: DormantSource::File(PathBuf::from("/p/a.rs")),
            },
        ];

        assert_eq!(
            s.first_dormant_id("p"),
            Some(scratch),
            "the scratch you were last editing lands, not the file behind it"
        );
    }

    /// `workspace_active_anywhere` is the delete guard: true iff *some* client has the workspace
    /// active, so deleting it would pull the rug.
    #[test]
    fn workspace_active_anywhere_tracks_any_client() {
        let mut s = ServerState::new();
        let (c1, sess1) = session("alpha");
        s.clients.insert(c1, sess1);
        assert!(s.workspace_active_anywhere("alpha"));
        assert!(!s.workspace_active_anywhere("beta"));
    }

    /// An ephemeral workspace is retired only once it has no buffers *and* no client still has it
    /// active. This is the multi-client safety property: if a second client joined the ephemeral
    /// context (via the switcher), closing the first client's buffer must not delete the workspace
    /// out from under it.
    #[test]
    fn ephemeral_pruned_only_when_empty_and_inactive() {
        let mut s = ServerState::new();
        let id = s.register_ephemeral_workspace();
        assert!(s.workspaces.contains_key(&id));
        assert!(s.workspaces[&id].is_ephemeral());

        // A client is active in the context and it holds a buffer.
        let (client, sess) = session(&id);
        s.clients.insert(client, sess);
        let buf = s.allocate_buffer_id();
        s.buffer_workspaces.insert(buf, id.clone());
        assert!(!s.prune_ephemeral_if_empty(&id), "active + non-empty stays");

        // The buffer closes, but the client is still parked here → still not pruned (no rug-pull).
        s.buffer_workspaces.remove(&buf);
        assert!(
            !s.prune_ephemeral_if_empty(&id),
            "an active client keeps the context even with no buffers"
        );
        assert!(s.workspaces.contains_key(&id));

        // The client switches away → now it's both empty and inactive, so it's retired.
        s.clients.get_mut(&client).unwrap().active_workspace = Some("other".to_string());
        assert!(s.prune_ephemeral_if_empty(&id));
        assert!(!s.workspaces.contains_key(&id));
    }

    /// Ephemeral display numbers reuse the lowest free slot (like scratch numbers), so the picker
    /// shows small, stable `(workspace N)` labels rather than an ever-climbing counter.
    #[test]
    fn ephemeral_ids_reuse_the_lowest_free_number() {
        let mut s = ServerState::new();
        let a = s.register_ephemeral_workspace();
        let b = s.register_ephemeral_workspace();
        assert_eq!(a, "ephemeral/1");
        assert_eq!(b, "ephemeral/2");
        // Retire #1; the next mint reuses its number rather than climbing to 3.
        s.workspaces.remove(&a);
        let c = s.register_ephemeral_workspace();
        assert_eq!(c, "ephemeral/1");
    }

    /// Deleting a workspace drops its entry and closes exactly its buffers (tearing down their
    /// per-buffer state), leaving unrelated workspaces and their buffers intact.
    #[test]
    fn delete_workspace_closes_only_its_buffers() {
        let mut s = ServerState::new();
        s.workspaces.insert(
            "doomed".to_string(),
            workspace_entry("doomed", vec![PathBuf::from("/tmp/d")]),
        );
        s.workspaces
            .insert("keep".to_string(), workspace_entry("keep", vec![]));

        let buf_a = s.allocate_buffer_id();
        s.buffer_workspaces.insert(buf_a, "doomed".to_string());
        let buf_b = s.allocate_buffer_id();
        s.buffer_workspaces.insert(buf_b, "doomed".to_string());
        let survivor = s.allocate_buffer_id();
        s.buffer_workspaces.insert(survivor, "keep".to_string());
        // Per-buffer state that teardown must also clear (cursors keyed by (client, buffer)).
        let (client, sess) = session("keep");
        s.cursors.insert((client, buf_a), CursorState::default());
        s.cursors.insert((client, survivor), CursorState::default());
        s.clients.insert(client, sess);

        let mut closed = s.delete_workspace("doomed");
        closed.sort();
        let mut expected = vec![buf_a, buf_b];
        expected.sort();
        assert_eq!(closed, expected);

        // Entry gone; its buffers and their per-buffer state are gone; the unrelated ones remain.
        assert!(!s.workspaces.contains_key("doomed"));
        assert!(s.workspaces.contains_key("keep"));
        assert!(!s.buffer_workspaces.contains_key(&buf_a));
        assert!(!s.buffer_workspaces.contains_key(&buf_b));
        assert!(!s.cursors.contains_key(&(client, buf_a)));
        assert_eq!(
            s.buffer_workspaces.get(&survivor).map(String::as_str),
            Some("keep")
        );
        assert!(s.cursors.contains_key(&(client, survivor)));
    }

    /// `buffers_under_path` (the `path/delete` screen) matches a file exactly and matches a
    /// directory by path-prefix — component-wise, so `/ws/src` doesn't catch `/ws/srcfoo` — and is
    /// scoped to the named workspace.
    #[test]
    fn buffers_under_path_matches_file_and_dir_prefix() {
        let mut s = ServerState::new();
        s.workspaces.insert(
            "proj".to_string(),
            workspace_entry("proj", vec![PathBuf::from("/ws")]),
        );

        let add = |s: &mut ServerState, path: &str| -> BufferId {
            let id = s.allocate_buffer_id();
            let path = PathBuf::from(path);
            s.insert_buffer_with_document(id, None, false, |d| {
                Document::new_at_path(d, path, None)
            });
            s.buffer_workspaces.insert(id, "proj".to_string());
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
            "scoped to the named workspace"
        );
    }

    /// `next_scratch_number` returns the lowest positive integer not used by another scratch in the
    /// workspace: small, reuses freed numbers, ignores file buffers, and numbers workspaces apart.
    #[test]
    fn next_scratch_number_picks_lowest_unused_per_workspace() {
        let mut s = ServerState::new();
        assert_eq!(s.next_scratch_number("proj"), 1, "empty workspace → 1");

        let add_scratch = |s: &mut ServerState, n: u32| {
            let id = s.allocate_buffer_id();
            s.insert_buffer_with_document(id, Some(n), false, |d| Document::scratch(d, None));
            s.buffer_workspaces.insert(id, "proj".to_string());
            id
        };
        let s1 = add_scratch(&mut s, 1);
        add_scratch(&mut s, 2);
        // A file buffer (no scratch number) doesn't occupy a slot.
        let file = s.allocate_buffer_id();
        s.insert_buffer_with_document(file, None, false, |d| {
            Document::new_at_path(d, PathBuf::from("/p/a.rs"), None)
        });
        s.buffer_workspaces.insert(file, "proj".to_string());
        assert_eq!(s.next_scratch_number("proj"), 3, "1 and 2 used → 3");

        // Free #1 → it's reused rather than handing out 3.
        s.buffers.remove(&s1);
        s.buffer_workspaces.remove(&s1);
        assert_eq!(s.next_scratch_number("proj"), 1);

        // A different workspace numbers independently.
        assert_eq!(s.next_scratch_number("other"), 1);
    }

    /// `unsaved_buffer_count` counts only the dirty buffers belonging to the named workspace — the
    /// number the workspace picker shows. Clean buffers, buffers in other workspaces, and dangling
    /// associations (no buffer entry) don't count.
    #[test]
    fn unsaved_buffer_count_counts_dirty_buffers_per_workspace() {
        let mut s = ServerState::new();

        let add = |s: &mut ServerState, workspace: &str, dirty: bool| -> BufferId {
            let id = s.allocate_buffer_id();
            s.insert_buffer_with_document(id, Some(1), false, |d| {
                let mut doc = Document::scratch(d, None);
                doc.dirty = dirty;
                doc
            });
            s.buffer_workspaces.insert(id, workspace.to_string());
            id
        };

        add(&mut s, "alpha", true);
        add(&mut s, "alpha", true);
        add(&mut s, "alpha", false); // clean — not counted
        add(&mut s, "beta", true); // other workspace — not counted for alpha

        // A buffer_workspaces association with no live buffer (defensive: shouldn't panic / count).
        let dangling = s.allocate_buffer_id();
        s.buffer_workspaces.insert(dangling, "alpha".to_string());

        assert_eq!(s.unsaved_buffer_count("alpha"), 2);
        assert_eq!(s.unsaved_buffer_count("beta"), 1);
        assert_eq!(
            s.unsaved_buffer_count("never-loaded"),
            0,
            "a workspace with no buffers reports zero"
        );
    }

    /// With backups enabled the count also sees unsaved work that *isn't loaded* — the state a
    /// workspace is in after the daemon idle-reaped and nobody has activated it again. File
    /// backups are document-level (`files/<hash>`) so they're attributed to workspaces through
    /// session entries; scratch backups come from the workspace's own `scratch/<workspace>/`
    /// listing. A backup whose buffer is live counts once (via the buffer), not twice.
    #[test]
    fn unsaved_buffer_count_includes_backups_for_unloaded_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let sessions_file = dir.path().join("sessions.json");
        let mut s = ServerState::new();
        s.backups_path = Some(dir.path().to_path_buf());
        s.sessions_path = Some(sessions_file.clone());
        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));
        // The session names /p/a.rs for workspace p — that's what attributes its shared backup.
        let mut sessions = crate::config::WorkspaceSessions::default();
        sessions.workspaces.insert(
            "p".into(),
            crate::config::WorkspaceSession {
                contexts: Vec::new(),
                last_activated_at: 1,
                buffers: vec![crate::config::SessionBuffer::File {
                    path: PathBuf::from("/p/a.rs"),
                }],
            },
        );
        crate::config::write_workspace_sessions_at(&sessions_file, &sessions).unwrap();
        crate::backup::write(
            &crate::backup::file_backup_path(dir.path(), Path::new("/p/a.rs")),
            "unsaved a",
        )
        .unwrap();
        crate::backup::write(&crate::backup::scratch_backup_path(dir.path(), "p", 3), "s").unwrap();
        assert_eq!(
            s.unsaved_buffer_count("p"),
            2,
            "nothing loaded — the backups *are* the unsaved buffers"
        );
        assert_eq!(
            s.unsaved_buffer_count("other"),
            0,
            "another workspace's backups don't leak in"
        );

        // Load one of them (what activation's eager restore does): still one unsaved buffer, not two.
        let id = s.allocate_buffer_id();
        s.insert_buffer_with_document(id, None, false, |d| {
            let mut doc = Document::new_at_path(d, PathBuf::from("/p/a.rs"), None);
            doc.dirty = true;
            doc
        });
        s.buffer_workspaces.insert(id, "p".to_string());
        assert_eq!(s.unsaved_buffer_count("p"), 2, "1 live + 1 still on disk");

        // Saving it deletes the backup and clears the flag — the workspace reads clean again.
        crate::backup::delete(&crate::backup::file_backup_path(
            dir.path(),
            Path::new("/p/a.rs"),
        ));
        crate::backup::delete(&crate::backup::scratch_backup_path(dir.path(), "p", 3));
        s.doc_of_mut(id).dirty = false;
        assert_eq!(s.unsaved_buffer_count("p"), 0);
    }

    /// The idle reaper's guard: a dirty buffer pins the server open only when its content would
    /// die with the process — backups disabled, or an ephemeral-only dirty *scratch* (its backup
    /// key dies with the context). A dirty buffer in a named workspace with backups on is safe on
    /// disk, so it doesn't block the reap — and so is a dirty *file* in an ephemeral workspace,
    /// whose backup is path-keyed and recovered on the next open of that path from anywhere.
    #[test]
    fn unprotected_unsaved_buffers_are_the_ones_no_backup_covers() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = ServerState::new();
        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));
        let named = s.allocate_buffer_id();
        s.insert_buffer_with_document(named, Some(1), false, |d| {
            let mut doc = Document::scratch(d, None);
            doc.dirty = true;
            doc
        });
        s.buffer_workspaces.insert(named, "p".to_string());

        assert!(
            s.has_unprotected_unsaved_buffers(),
            "backups disabled — any dirty buffer pins the server"
        );
        s.backups_path = Some(dir.path().to_path_buf());
        assert!(
            !s.has_unprotected_unsaved_buffers(),
            "a named workspace's dirty buffer is on disk; the reaper may proceed"
        );

        // A *scratch* edit in a temporary workspace has nowhere to be flushed to: its backup key
        // is scratch/<workspace>/<n> and the ephemeral workspace id is never looked up again.
        let ephemeral = s.register_ephemeral_workspace();
        let temp = s.allocate_buffer_id();
        s.insert_buffer_with_document(temp, Some(1), false, |d| {
            let mut doc = Document::scratch(d, None);
            doc.dirty = true;
            doc
        });
        s.buffer_workspaces.insert(temp, ephemeral.clone());
        assert!(s.has_unprotected_unsaved_buffers());
        s.doc_of_mut(temp).dirty = false;
        assert!(!s.has_unprotected_unsaved_buffers(), "clean again");

        // A *file* edit in a temporary workspace IS protected: the backup keys on the path alone
        // (files/<hash>), so recover-on-open restores it after the context is gone.
        let tether = s.allocate_buffer_id();
        s.insert_buffer_with_document(tether, None, false, |d| {
            let mut doc = Document::new_at_path(d, PathBuf::from("/tmp/tethered.txt"), None);
            doc.dirty = true;
            doc
        });
        s.buffer_workspaces.insert(tether, ephemeral);
        assert!(
            !s.has_unprotected_unsaved_buffers(),
            "an ephemeral file-backed document is covered by its path-keyed backup"
        );
    }

    /// A temporary workspace takes the directory the caller hands it — the opened file's parent, or
    /// a directory opened as a context of its own. A second directory appends a root; one already
    /// covered adds nothing; a persisted workspace is never touched; and the filesystem root is
    /// refused rather than rooting a workspace at `/`.
    #[test]
    fn adopt_ephemeral_root_takes_the_directory_it_is_given() {
        let mut s = ServerState::new();
        let id = s.register_ephemeral_workspace();
        assert!(s.adopt_ephemeral_root(&id, Path::new("/home/joe/notes")));
        assert_eq!(
            s.workspaces[&id].paths,
            vec![PathBuf::from("/home/joe/notes")]
        );

        assert!(
            !s.adopt_ephemeral_root(&id, Path::new("/home/joe/notes/sub")),
            "already under an existing root"
        );
        assert!(s.adopt_ephemeral_root(&id, Path::new("/etc")));
        assert_eq!(
            s.workspaces[&id].paths,
            vec![PathBuf::from("/home/joe/notes"), PathBuf::from("/etc")],
            "a second directory becomes a second root"
        );

        assert!(
            !s.adopt_ephemeral_root(&id, Path::new("/")),
            "a file directly under the filesystem root would root the workspace at /"
        );
        assert!(!s.adopt_ephemeral_root("nonexistent", Path::new("/a")));

        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));
        assert!(
            !s.adopt_ephemeral_root("p", Path::new("/elsewhere")),
            "a persisted workspace owns its roots; an open must not edit them"
        );
        assert_eq!(s.workspaces["p"].paths, vec![PathBuf::from("/p")]);
    }

    /// Opening a new temporary workspace supersedes the idle ones before it — the contexts a client
    /// left behind by quitting without closing its buffer. Anything still in use is spared: a client
    /// parked in it, a viewport showing one of its buffers, or an unsaved buffer. Persisted
    /// workspaces are never touched.
    #[test]
    fn supersede_retires_only_idle_clean_ephemeral_workspaces() {
        let mut s = ServerState::new();
        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));
        let add_buffer = |s: &mut ServerState, workspace: &str, dirty: bool| -> BufferId {
            let id = s.allocate_buffer_id();
            s.insert_buffer_with_document(id, None, false, |d| {
                let mut doc = Document::new_at_path(d, PathBuf::from("/outside/f.rs"), None);
                doc.dirty = dirty;
                doc
            });
            s.buffer_workspaces.insert(id, workspace.to_string());
            id
        };
        let persisted = add_buffer(&mut s, "p", false);

        let idle = s.register_ephemeral_workspace();
        let idle_buffer = add_buffer(&mut s, &idle, false);
        let dirty = s.register_ephemeral_workspace();
        add_buffer(&mut s, &dirty, true);
        let occupied = s.register_ephemeral_workspace();
        add_buffer(&mut s, &occupied, false);
        let (client, sess) = session(&occupied);
        s.clients.insert(client, sess);
        let viewed = s.register_ephemeral_workspace();
        let viewed_buffer = add_buffer(&mut s, &viewed, false);
        let viewport_id = s.allocate_viewport_id();
        s.viewports.insert(
            viewport_id,
            Viewport {
                id: viewport_id,
                view_id: ViewId(viewed_buffer),
                focused: 0,
                client_id: uuid::Uuid::new_v4(),
                rows: 24,
                overscan_rows: 0,
                scroll_view_line: ViewLine::ZERO,
                scroll_sub_row: 0.0,
                wrap: WrapMode::None,
                tab_width: 4,
                diff_view: false,
                first_view_line: ViewLine::ZERO,
                last_view_line_exclusive: ViewLine(1),
                elements: vec![ElementBinding {
                    buffer_id: viewed_buffer,
                    cols: 80,
                    continuation_marker_width: 0,
                    start_line: 0,
                    end_line_exclusive: 1,
                    decorations: None,
                    chrome_above: Default::default(),
                }],
            },
        );

        let (retired, closed, _stopped) = s.supersede_ephemeral_workspaces();
        assert_eq!(retired, vec![idle.clone()]);
        assert_eq!(closed, vec![idle_buffer]);
        assert!(!s.workspaces.contains_key(&idle));
        assert!(
            !s.buffers.contains_key(&idle_buffer),
            "its buffer is closed"
        );
        assert!(!s.buffer_workspaces.contains_key(&idle_buffer));
        for kept in [&dirty, &occupied, &viewed, &"p".to_string()] {
            assert!(s.workspaces.contains_key(kept), "{kept} should survive");
        }
        assert!(s.buffers.contains_key(&persisted));

        // The freed number is reused, so the replacement is `(workspace 1)` again rather than 5.
        assert_eq!(s.register_ephemeral_workspace(), idle);
    }
}

/// A count of the server's outstanding **deferred work** — the debounced, spawned follow-ups that
/// finish after the RPC that provoked them has already replied.
///
/// It exists so that "has the server finished reacting?" can be *asked* rather than waited out.
/// Debounced work is otherwise unobservable until it produces a side effect, which makes the
/// absence of a side effect — "this settle was correctly deduped and pushed nothing" — impossible
/// to assert except by sleeping longer than the debounce and hoping. That is a guess, and it is
/// wrong on a loaded machine.
#[derive(Default)]
pub struct Deferred {
    outstanding: std::sync::atomic::AtomicUsize,
    quiet: tokio::sync::Notify,
}

impl Deferred {
    /// Register one unit of deferred work, counted until every clone of the returned token is
    /// dropped.
    ///
    /// The token must be taken at the point the work becomes *inevitable* — while the provoking
    /// RPC still holds the state lock — not when the task is eventually spawned. Otherwise there
    /// is a window where the RPC has replied, the work is coming, and the server looks quiet.
    pub fn start(self: &Arc<Self>) -> DeferredToken {
        self.outstanding
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        DeferredToken(Arc::new(TokenInner(self.clone())))
    }

    pub fn is_quiet(&self) -> bool {
        self.outstanding.load(std::sync::atomic::Ordering::SeqCst) == 0
    }

    /// Resolve once no deferred work is outstanding. Returns immediately when already quiet.
    pub async fn wait_quiet(&self) {
        loop {
            // Register for the wakeup *before* testing, so a completion landing between the two
            // can't be missed.
            let waiting = self.quiet.notified();
            if self.is_quiet() {
                return;
            }
            waiting.await;
        }
    }
}

/// Keeps a unit of deferred work counted. Cloneable: one provoking event can arm several
/// follow-ups, and the work is done when the last of them is.
///
/// Release is by `Drop` rather than an explicit call because most of these tasks return *early* —
/// superseded by a newer cursor move — and a barrier that only counts down on the happy path
/// wedges the moment anything is superseded.
#[derive(Clone)]
pub struct DeferredToken(#[allow(dead_code)] Arc<TokenInner>);

struct TokenInner(Arc<Deferred>);

impl Drop for TokenInner {
    fn drop(&mut self) {
        if self
            .0
            .outstanding
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
            == 1
        {
            self.0.quiet.notify_waiters();
        }
    }
}
