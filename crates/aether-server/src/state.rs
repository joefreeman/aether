//! Authoritative in-memory state owned by the server.

use crate::error::RpcError;
use crate::indent::{self, IndentStyle};
use crate::picker::{self as picker_state, PickerState};
use crate::syntax::{self, InjectionLayer, LanguageConfig};
use crate::workspace_index::WorkspaceIndex;
use aether_protocol::cursor::CursorState;
use aether_protocol::envelope::Notification;
use aether_protocol::lsp::SymbolCrumb;
use aether_protocol::picker::{MatchOptions, PickerKind};
use aether_protocol::ui::LayoutOwner;
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
    /// (`view/open`) and looked up when scoping per-buffer state to a workspace (e.g. on
    /// `workspace/activate`, when tearing down a client's state for the previously active workspace).
    pub buffer_workspaces: HashMap<BufferId, String>,
    pub clients: HashMap<ClientId, ClientSession>,
    pub viewports: HashMap<ViewportId, Viewport>,
    pub cursors: HashMap<(ClientId, BufferId), CursorState>,
    /// Whether a client is **reading** a markdown buffer — seeing the rendered document as one
    /// prose element — rather than editing its source. Keyed like the cursor, because it is the
    /// same kind of fact: this client's relationship to this buffer. Not a field of the viewport,
    /// which is superseded on every switch and would forget; not a field of the view, which every
    /// client shares. Materialised at a client's first landing on the buffer
    /// ([`Self::land_read`]) from the document's memory of how it was last shown, so a later
    /// toggle elsewhere never flips a screen that was already showing the file.
    pub read: HashMap<(ClientId, BufferId), bool>,
    /// Per-`(client, buffer)` history of cursor states for motion undo/redo. Distinct from the
    /// buffer's own undo stack: this rewinds *only* the client's own cursor moves and is cleared
    /// by any buffer mutation (since prior positions may no longer be valid).
    pub motion_history: HashMap<(ClientId, BufferId), MotionHistory>,
    /// The cursor's "intended" *visual* column for vertical motions — preserved across repeated
    /// vertical presses so that landing on rows with different prefixes (continuation marker +
    /// indent) doesn't cause the visual column to drift. Cleared by any non-vertical motion,
    /// explicit cursor set, or buffer mutation.
    ///
    /// Every vertical motion keeps it: `Motion::VisualLine` (`Alt-j`/`Alt-k`), `Motion::Page`
    /// (`v`/`Alt-v`), and `Motion::LogicalLine` (`j`/`k`) when it preserves the column — which is
    /// why that resolver takes a `tab_width` at all.
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
    /// Per-`(client, view)` last-known scroll position, as content, and the cursor it framed.
    /// Written whenever the client subscribes to or loads a window of the view, stamped with the
    /// cursor again as the view is hidden, and surfaced on `view/open` so the client can restore
    /// the view where it left it — while the cursor is still where it was ([`ScrollMemory`]).
    /// Keyed by the **view**: keying by the focused element's buffer recorded a patch's position
    /// against one of its files, which a plain open of that file then restored as if it were the
    /// file's own. Cleared on disconnect.
    pub last_scroll: HashMap<(ClientId, ViewId), ScrollMemory>,
    /// Ticks once per use of a view (an open, a subscribe), stamping [`View::last_used`] — the
    /// order [`Self::view_presenting`] answers a buffer's most recently used view by.
    pub view_clock: u64,
    /// The app settings as this server knows them: loaded from the profile at boot, replaced by
    /// every `settings/set`. Tests never touch the profile's file, and start from the defaults.
    pub app_settings: aether_protocol::settings::AppSettings,
    /// Per-`(client, kind)` picker state. Survives `picker/hide` (so resume restores query +
    /// ranking); cleared on disconnect.
    pub pickers: HashMap<(ClientId, PickerKind), PickerState>,
    /// Per-buffer *unstaged* diff hunks: the live buffer against its **index** content
    /// (`git diff`). Populated on `view/open` for file-backed buffers; recomputed as the buffer
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
    /// Every view a client can be showing, keyed by the buffer presenting it — the **one owner** of
    /// what a view is composed of. A viewport references its view here rather than carrying a copy
    /// of the elements: a copy was a second thing to shift on edit, to rebuild after a stage, and
    /// to drop on close, and each of those was forgotten at least once.
    ///
    /// An ordinary buffer's view is one whole-buffer element; a patch's is what its driver built.
    /// Created when a view is first presented (`ensure_view`) or built by a driver
    /// (`set_view_layout`), dropped with its buffer.
    pub views: HashMap<ViewId, View>,
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
    /// Where `agent/open` gets an agent from. See [`AgentLauncher`] — the point of it being an
    /// enum rather than an optional dummy is that a test server cannot fall through to launching
    /// a real one.
    pub agent_launcher: AgentLauncher,
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
    /// Workspaces whose session file is out of date — keys of [`Self::workspaces`], so a bound
    /// context is its own entry. Drained at the end of every request dispatch
    /// ([`crate::handlers::flush_dirty_sessions`]), which writes each named workspace and clears
    /// it; [`crate::handlers::persist_workspace_session`] clears whatever it has just written.
    ///
    /// **Set where a user action changed what the session should say** — the MRU, a view's
    /// transience or reading mode, a close, the dormant list. Deliberately *not* set by
    /// [`Self::drop_view_from_mru`] / [`Self::drop_buffer_from_mru`]: the hide collector
    /// ([`Self::close_orphaned_transients`]) reaches both, and it runs on disconnect and on
    /// workspace-leave with no user action behind it — a write there would erase the preview the
    /// session exists to bring back. In the ordinary "move on" case the next open's
    /// [`Self::touch_mru_view`] dirties, and *that* write correctly omits the collected preview.
    pub sessions_dirty: std::collections::HashSet<String>,
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
    /// One id space for buffers and views: see [`Self::allocate_view_id`].
    next_id: u64,
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

/// Where a client last had a view scrolled to, and the cursor that scroll framed.
///
/// A remembered scroll is worth restoring only while the cursor is where it was when the view was
/// last shown. A buffer's cursor can move while its own view is hidden: the cursor is per
/// `(client, buffer)`, and a review's element windows the same buffer, so stepping through the
/// hunks moves it; `cursor/set` moves it outright. Restoring the old scroll then frames the wrong
/// region and strands the cursor off screen — `Enter` out of the working changes, `Backspace`
/// back, a few lines down, `Enter` again, and the file opened where it had been rather than where
/// the cursor was. So the memory says which cursor it framed, and an open checks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollMemory {
    pub anchor: ScrollPosition,
    /// The focused element's buffer and the cursor in it, as of the last write or the moment the
    /// view was hidden. `None` only for a memory written with no viewport to ask.
    pub cursor: Option<(BufferId, aether_protocol::LogicalPosition)>,
}

/// One location in the navigation history: the view the client was in, its buffer, and the
/// cursor/selection to restore there. The view is preferred while it is still open — it says
/// *which* view of the file, the reader or the editor, and is the only handle a scratch has; the
/// path fields let a closed file be reopened.
#[derive(Clone, Debug, PartialEq)]
pub struct NavEntry {
    pub view_id: ViewId,
    /// The buffer the cursor is in — what the cursor is keyed by, and the file the path fields
    /// name.
    pub buffer_id: BufferId,
    pub path_index: Option<u32>,
    pub relative_path: Option<String>,
    /// [`VirtualSource::key`] when the entry is a materialised revision — a commit's patch or a
    /// file at a revision. The reopen handle for buffers that have no path: they were *generated*,
    /// not loaded, so without this a virtual buffer would die with its id and stepping back to a
    /// diff you followed a line out of would find nothing to return to.
    pub virtual_key: Option<String>,
    /// The element the cursor was in, for a **composed** view — one whose elements window buffers
    /// other than the view's own. `None` for an ordinary view, where the view's buffer *is* the
    /// element's and there is nothing to disambiguate.
    ///
    /// A composed view's location is not a position in the buffer it was opened as. A
    /// working-changes patch's elements window the real files, so the cursor lives in one of
    /// *those*, and the patch's own document holds a cursor nothing ever moves. Recorded without
    /// this, every step back into such a view restored that one — line 0 — and landed at the top
    /// of the patch however far down you had been.
    pub element: Option<aether_protocol::viewport::FieldId>,
    /// The cursor to restore, in [`Self::element`]'s buffer when there is one and in
    /// [`Self::buffer_id`] otherwise.
    pub cursor: CursorState,
    /// Whether the client was **reading** the file here — the mode a step back lands in, since
    /// the file may have closed and forgotten how it was shown. `None` for an entry that recorded
    /// nothing (the web's own stacks), which leaves the mode to the server's memory.
    pub read: Option<bool>,
}

/// One back/forward navigation trail. Browser semantics: a jump pushes onto `back` and clears
/// `forward`; stepping back/forward moves entries between the two and across the "current" cursor.
///
/// A trail belongs to a **(context, client)** pair — it lives in
/// [`WorkspaceEntry::nav_history`], keyed by the client standing there. Its entries name files as
/// `(path_index, relative_path)` *relative to the context's roots*, so a trail is only meaningful
/// inside the context that recorded it; one shared across workspaces resolved its entries against
/// the wrong roots. Cloned when a context hands its trail on to an arriving client
/// ([`WorkspaceEntry::last_nav`]) — a copy, never a share, so two windows never step each other.
#[derive(Default, Clone, Debug)]
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

    /// Nothing in either direction — a trail with nothing to hand on.
    pub fn is_empty(&self) -> bool {
        self.back.is_empty() && self.forward.is_empty()
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
    /// `view/open` (fresh open, reopen, or attach-by-id) — so this is *focus* recency, not edit
    /// recency. Drives the picker's empty-query ordering, the successor a close lands on, and the
    /// `last_view_id` returned by `workspace/activate` (so re-attaching to a workspace drops the
    /// user on the view they last had, scratch or file alike).
    ///
    /// **Views**, not buffers: a file's editor and its reader are two entries, and the one you
    /// used last is the one you come back to. Lives on the workspace — not on the client — so it
    /// persists across client disconnects. A new TUI invocation gets a fresh `ClientId` but
    /// inherits the workspace's MRU.
    pub mru_views: VecDeque<ViewId>,
    /// Views restored from the persisted session ([`crate::config::WorkspaceSession`]) on
    /// activation but not yet loaded into memory — most-recently-used first, mirroring the order
    /// they were saved in. Each holds a reserved [`ViewId`] (its picker identity) and enough to
    /// rebuild what it shows; it carries no rope/syntax/LSP. The view picker lists them after the
    /// live views, rendered identically to them; opening one materializes a real view (see
    /// `view_open`'s by-id path) and drops it from here. Never contains a path that's also a
    /// live buffer in this workspace — promotion removes it.
    pub dormant_views: Vec<DormantView>,
    /// This context's jumplist: the quickfix-style snapshot `jumplist/capture` takes of a picker's
    /// filtered results, stepped cursor-relative by `jumplist/step` (`]` / `[`). `None` until
    /// something is captured; replaced wholesale by the next capture.
    ///
    /// Lives on the entry — not on the client — for the same reason [`Self::mru_views`] does, and
    /// for one more. The reason it shares: a client has no durable identity, so a list hung off a
    /// `ClientId` dies with the window that captured it, and two shells attached to the same context
    /// each step a list the other can't see. The reason it doesn't: entries carry absolute paths, so
    /// a list that followed a worktree rebind would step you into the tree you just left. Because
    /// the `workspaces` map is keyed by [`crate::worktree::context_id`] — name *plus* bindings —
    /// storing it here makes both right at once: one list per tree, each surviving the client.
    pub jumplist: Option<crate::jumplist::Jumplist>,
    /// The back/forward navigation trails of the clients standing in this context — one each,
    /// keyed by client, browser-style ([`NavHistory`]). Recorded on qualifying jumps (the
    /// navigating `view/open`'s `record_nav_from`) and stepped by `nav/step`. The web client rides
    /// native browser history instead, so its entries here go unused — but recording stays uniform
    /// across clients.
    ///
    /// Per **(context, client)** for the two halves of the reason [`Self::jumplist`] is per
    /// context: an entry's `(path_index, relative_path)` is relative to the roots of the workspace
    /// that recorded it, so a trail stepped from another context resolves against the wrong ones —
    /// and yet two windows on the same context must not step one another's trail, since back and
    /// forward are where *this* window has been. Reached only through
    /// [`ServerState::nav_history`] / [`ServerState::nav_history_mut`], which resolve the client's
    /// active context, so no caller can pick the wrong trail.
    pub nav_history: HashMap<ClientId, NavHistory>,
    /// The trail the most recent client to leave this context left behind — by switching away or
    /// by disconnecting. A client that activates this context with no trail of its own starts from
    /// a **clone** of it, so closing a window and opening another keeps `Alt-Left` working where
    /// you left off. A clone and not a share: concurrent windows each step their own.
    ///
    /// In memory only, like [`Self::jumplist`] — nothing about it is written to the session file.
    pub last_nav: Option<NavHistory>,
    /// Projects declared by this workspace's config, whose language servers are pinned open while
    /// it's active. Flattened out of the config's nested `[[roots]]` form, so each carries the
    /// index of the root it was declared under.
    ///
    /// Held in memory — not re-read from disk when needed — because `workspace/add_root` and
    /// `remove_root` rewrite the config file wholesale from this entry. Anything they don't carry
    /// is silently erased on the next root edit.
    pub projects: Vec<crate::config::ProjectRef>,
}

/// A dormant *view*: a session-restored row that hasn't been loaded yet. It is what the picker
/// lists and what a session records — the buffer behind it does not exist until the row is opened.
/// See [`WorkspaceEntry::dormant_views`].
#[derive(Debug, Clone)]
pub struct DormantView {
    /// Reserved id — the buffer the row's view will present. Not present in `ServerState::buffers`
    /// until materialized; nothing on the wire names it.
    pub id: BufferId,
    /// Reserved view id — what the picker row names, so closing the row (`view/close`) and
    /// selecting it (`view/open { view_id }`) address it as they would a live view. Discarded at
    /// materialisation like `id`: the real buffer gets a real view.
    pub view: ViewId,
    /// Whether the file was being **read** when it was recorded — the mode its row materialises
    /// in. `false` for a scratch, a revision, a shell or an agent, which have no such mode.
    pub read: bool,
    /// Whether the view was a **preview** when it was recorded, so materialising the row brings
    /// back a preview rather than a kept view. Honoured only for the landing: every other
    /// transient row is dropped once activation has decided where to land
    /// ([`ServerState::drop_transient_dormant`]), because an unopened one is re-written on every
    /// persist and would otherwise never die.
    pub transient: bool,
    /// What to materialize: a file (by path) or a scratch (by per-workspace number, whose unsaved
    /// content is restored from its backup).
    pub source: DormantSource,
}

/// The thing a [`DormantView`] materializes into.
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
    /// A shell, restored from its snapshot in the backups directory by its per-workspace number.
    Shell { number: u32 },
    /// An agent conversation, restored from its snapshot. Opening one shows what was on screen and
    /// launches **nothing**: an agent is a subprocess that costs money to run, so it starts when
    /// you prompt it, not when you glance at yesterday's conversation.
    Agent { number: u32 },
}

/// How a dormant row was presented when the session recorded it — what materialising the row
/// restores. A pair rather than two loose booleans because it is passed as one: the dormant list
/// hands it to the open, and the open hands it to the view it mints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DormantPresentation {
    /// The file was being **read** — shown as the rendered document rather than its source.
    pub read: bool,
    /// The view was a **preview**, and comes back as one.
    pub transient: bool,
}

impl DormantView {
    /// How this row was presented — what materialising it restores.
    pub fn presentation(&self) -> DormantPresentation {
        DormantPresentation {
            read: self.read,
            transient: self.transient,
        }
    }

    /// The canonical path, for a file-backed dormant view; `None` for a scratch.
    pub fn path(&self) -> Option<&Path> {
        match &self.source {
            DormantSource::File(p) => Some(p.as_path()),
            DormantSource::Scratch { .. }
            | DormantSource::Virtual { .. }
            | DormantSource::Shell { .. }
            | DormantSource::Agent { .. } => None,
        }
    }
}

impl WorkspaceEntry {
    /// A client arrives in this context: give it the hand-over trail
    /// ([`Self::last_nav`]) when it has none of its own here. A client that has stood here before
    /// keeps what it had — returning from a switch finds back and forward exactly as it left them.
    ///
    /// "None of its own" counts an *empty* trail, which is the same judgement [`Self::leave_nav`]
    /// makes in the other direction: a client can have an empty one recorded merely by having
    /// pressed `Alt-Left` here once, and that is not a reason to refuse the hand-over.
    fn join_nav(&mut self, client_id: ClientId) {
        if self
            .nav_history
            .get(&client_id)
            .is_some_and(|h| !h.is_empty())
        {
            return;
        }
        if let Some(handed_on) = self.last_nav.clone() {
            self.nav_history.insert(client_id, handed_on);
        }
    }

    /// A client leaves this context: leave its trail behind as the hand-over copy for whoever
    /// arrives next. `take` also removes the client's own trail — a disconnect, where no window is
    /// coming back to it; a switch leaves it in place, since the same client returning must find
    /// it intact.
    ///
    /// An empty trail is not a hand-over: a window that passed through here without navigating has
    /// nothing to pass on, and erasing the last real trail with it would lose the one thing this
    /// field is for.
    fn leave_nav(&mut self, client_id: ClientId, take: bool) {
        let trail = if take {
            self.nav_history.remove(&client_id)
        } else {
            self.nav_history.get(&client_id).cloned()
        };
        match trail {
            Some(trail) if !trail.is_empty() => self.last_nav = Some(trail),
            _ => {}
        }
    }

    /// True iff the given canonical path falls under one of this workspace's roots. A file that
    /// isn't contained is a *guest* — no git baseline, no language server (`view_open`). Note this
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
            read: HashMap::new(),
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
            view_clock: 0,
            app_settings: aether_protocol::settings::AppSettings::default(),
            pickers: HashMap::new(),
            git_unstaged_hunks: HashMap::new(),
            git_both_hunks: HashMap::new(),
            git_baseline: HashMap::new(),
            virtual_git_status: HashMap::new(),
            views: HashMap::new(),
            git_conflicts: HashMap::new(),
            git_blame: HashMap::new(),
            matcher: picker_state::make_matcher(),
            lsp: crate::lsp::manager::LspManager::default(),
            agent_launcher: AgentLauncher::Subprocess,
            diagnostics: HashMap::new(),
            path_diagnostics: HashMap::new(),
            document_symbols: HashMap::new(),
            started_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            sessions_path: None,
            sessions_dirty: std::collections::HashSet::new(),
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
            next_id: 1,
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

    /// Install the layout a driver built for the view `buffer_id` presents: elements bound to real
    /// buffers rather than to slices of the view's own document. Set when a patch is generated,
    /// replacing whatever view the buffer's creation gave it.
    pub fn set_view_layout(&mut self, buffer_id: BufferId, layout: Vec<ElementLayout>) {
        let id = self
            .view_presenting(buffer_id)
            .unwrap_or_else(|| self.allocate_view_id());
        let (last_used, transient) = self
            .views
            .get(&id)
            .map_or((0, false), |v| (v.last_used, v.transient));
        self.views.insert(
            id,
            View {
                last_used,
                transient,
                ..View::from_layout(buffer_id, layout)
            },
        );
    }

    /// The buffer presenting view `id`. Panics like [`Self::view`] on a view nothing has created.
    pub fn presenting_buffer(&self, id: ViewId) -> BufferId {
        self.view(id).presenting
    }

    /// Non-panicking [`Self::presenting_buffer`].
    pub fn try_presenting_buffer(&self, id: ViewId) -> Option<BufferId> {
        self.views.get(&id).map(|v| v.presenting)
    }

    /// Every view `buffer_id` presents — at most one. A `Vec` because the teardown paths walk it
    /// the same way whether it holds one view or none; [`Self::open_view`] is what keeps it from
    /// ever holding two.
    pub fn views_presenting(&self, buffer_id: BufferId) -> Vec<ViewId> {
        let mut ids: Vec<ViewId> = self
            .views
            .iter()
            .filter(|(_, v)| v.presenting == buffer_id)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// The view `buffer_id` presents — its **most recently used** one, when it has several: what an
    /// open with no opinion presents, what a picker row names, what a close of the buffer's last
    /// viewport tears down. `None` for a buffer with no view (a dormant one).
    pub fn view_presenting(&self, buffer_id: BufferId) -> Option<ViewId> {
        self.views_presenting(buffer_id)
            .into_iter()
            .max_by_key(|id| self.views[id].last_used)
    }

    /// Mint the id of a new view.
    ///
    /// Buffers and views draw on one id space ([`Self::next_id`]), so no view id is ever a
    /// buffer's and no buffer id ever a view's: nothing can pass one off as the other, and no
    /// arithmetic produces a view from a buffer. `view/open` reports the view it presented, and
    /// that is the only way a client learns a view's id.
    pub(crate) fn allocate_view_id(&mut self) -> ViewId {
        let id = self.next_id;
        self.next_id += 1;
        ViewId(id)
    }

    /// Stamp `id` as used now — an open, a subscribe — so it is the buffer's most recently used
    /// view until another is.
    pub fn touch_view(&mut self, id: ViewId) {
        self.view_clock += 1;
        let clock = self.view_clock;
        if let Some(view) = self.views.get_mut(&id) {
            view.last_used = clock;
        }
    }

    /// The view presented by `id`. Panics like [`Self::doc_of`] on a view nothing has presented:
    /// every viewport's view exists (subscribing ensures it) and dies with its buffer, so a miss is
    /// a broken invariant rather than a case.
    pub fn view(&self, id: ViewId) -> &View {
        self.views
            .get(&id)
            .unwrap_or_else(|| panic!("view {id} has not been presented"))
    }

    /// Non-panicking [`Self::view`].
    pub fn try_view(&self, id: ViewId) -> Option<&View> {
        self.views.get(&id)
    }

    /// The view a viewport presents.
    pub fn view_of(&self, vp: &Viewport) -> &View {
        self.view(vp.view_id)
    }

    /// The buffer a viewport is acting on: its focused element's.
    pub fn focused_buffer(&self, vp: &Viewport) -> BufferId {
        vp.buffer_id(self.view_of(vp))
    }

    /// The elements of the view `buffer_id` presents, whether or not one exists yet — the view's
    /// own if it does, otherwise what creating it *would* build: the regions a generated patch
    /// divides into, or one element over the whole buffer.
    ///
    /// For the callers that resolve a place in a view nobody has opened: a jumplist entry
    /// reopening the view it was captured from lands before the client's subscribe arrives.
    pub fn view_elements_of(&self, buffer_id: BufferId) -> std::borrow::Cow<'_, [ElementBinding]> {
        match self
            .view_presenting(buffer_id)
            .and_then(|id| self.views.get(&id))
        {
            Some(view) => std::borrow::Cow::Borrowed(&view.elements),
            None => std::borrow::Cow::Owned(self.default_view(buffer_id).elements),
        }
    }

    /// The view `buffer_id` presents, created if it has none — what a buffer's creation does, so
    /// every live buffer has a view from its first moment, and **the one place a buffer's view is
    /// made**: a buffer has exactly one, and an open of a buffer that has it is that view again.
    ///
    /// **A view created with no opinion is a preview.** `transient` is the open's intent, and a
    /// new view is transient unless the open says `Some(false)`: an open that says nothing is a
    /// glance, and the view closes itself once nothing shows it. A view is *kept* only because the
    /// user did something to it — an edit, a save, a user-initiated reload, `Space k`, a tethered
    /// launch (`ae file`), or a session row that recorded it kept — and an open never demotes a
    /// view that already is. `Some(true)` therefore only restates the default for a creating open,
    /// and does nothing to an existing view.
    ///
    /// A view made here that nothing has ever shown (its creation's placeholder, before the open
    /// that created the buffer said what it wanted) takes the open's intent as its own.
    ///
    /// How a client sees the view — a markdown file read as a document or edited as source — is
    /// not the view's to know: see [`Self::read_mode`].
    pub fn open_view(&mut self, buffer_id: BufferId, transient: Option<bool>) -> ViewId {
        if let Some(id) = self.view_presenting(buffer_id) {
            if self.views.get(&id).expect("listed view").last_used == 0 {
                // This open's own placeholder, made with the buffer: the intent is its own. Its
                // *creation*, so it writes the flag directly — the composed-view refusal in
                // `set_view_transient` is about changing a view's transience afterwards.
                self.write_view_transient(id, transient != Some(false));
            } else if transient == Some(false) {
                self.set_view_transient(id, false);
            }
            self.touch_view(id);
            return id;
        }
        let mut view = self.default_view(buffer_id);
        view.transient = transient != Some(false);
        let id = self.allocate_view_id();
        self.views.insert(id, view);
        self.touch_view(id);
        debug_assert_eq!(
            self.views_presenting(buffer_id).len(),
            1,
            "a buffer presents exactly one view"
        );
        id
    }

    /// Whether `buffer_id` can be **read** — shown as the rendered document rather than its
    /// source: a markdown file presented on its own. A driver's view (a patch, a shell, an agent)
    /// has no such mode, and neither has any other language.
    pub fn readable(&self, buffer_id: BufferId) -> bool {
        self.try_doc_of(buffer_id)
            .is_some_and(|d| d.language.as_deref() == Some("markdown"))
            && self
                .view_presenting(buffer_id)
                .is_none_or(|id| !self.views[&id].is_composed())
    }

    /// How `client` sees `buffer_id`: reading, or editing. The client's own entry when it has
    /// one; else what the file was last shown as by anyone; else the app setting. Always `false`
    /// for a buffer that is not [`Self::readable`].
    pub fn read_mode(&self, client: ClientId, buffer_id: BufferId) -> bool {
        if !self.readable(buffer_id) {
            return false;
        }
        self.read
            .get(&(client, buffer_id))
            .copied()
            .unwrap_or_else(|| self.read_seed(buffer_id))
    }

    /// The mode a client's first landing on `buffer_id` starts in: what the file was last shown
    /// as, else the app setting.
    fn read_seed(&self, buffer_id: BufferId) -> bool {
        self.doc_of(buffer_id)
            .read_last
            .unwrap_or(self.app_settings.markdown_read)
    }

    /// Whether `client` reads the view `view`: a plain markdown view whose buffer the client is
    /// reading. What the render step asks to decide whether the view's one element goes out as
    /// prose or as lines.
    pub fn reads(&self, client: ClientId, view: ViewId) -> bool {
        self.try_presenting_buffer(view)
            .is_some_and(|buffer_id| self.read_mode(client, buffer_id))
    }

    /// Materialise `client`'s mode for `buffer_id` — a landing. The entry is fixed at what the
    /// file was last shown as (or the setting), so another client's later toggle leaves this
    /// screen alone; and the file now remembers that as its last showing, so the session records
    /// a mode for every file someone has looked at. No-op for a buffer that cannot be read.
    /// Returns the mode.
    pub fn land_read(&mut self, client: ClientId, buffer_id: BufferId) -> bool {
        if !self.readable(buffer_id) {
            return false;
        }
        let mode = self.read_mode(client, buffer_id);
        self.read.insert((client, buffer_id), mode);
        self.doc_of_mut(buffer_id).read_last.get_or_insert(mode);
        mode
    }

    /// Set how `client` sees `buffer_id`, and remember it as how the file was last shown — the
    /// seed of every later first landing, and what the session records. `false` (and nothing
    /// changed) for a buffer that cannot be read.
    pub fn set_read_mode(&mut self, client: ClientId, buffer_id: BufferId, read: bool) -> bool {
        if !self.readable(buffer_id) {
            return false;
        }
        self.read.insert((client, buffer_id), read);
        self.doc_of_mut(buffer_id).read_last = Some(read);
        // The session records how each file was last shown, so a toggle is a change to it.
        self.dirty_session_for_buffer(buffer_id);
        true
    }

    /// Forget a client's reading modes. Used on disconnect.
    pub fn drop_read_for_client(&mut self, client_id: ClientId) {
        self.read.retain(|(c, _), _| *c != client_id);
    }

    /// Whether every view of `buffer_id` is a preview — what "the buffer is transient" means now
    /// that transience is the view's: a buffer with a kept view is kept. A buffer with no view is
    /// nobody's, and counts as transient.
    pub fn buffer_is_transient(&self, buffer_id: BufferId) -> bool {
        self.views_presenting(buffer_id)
            .iter()
            .all(|id| self.views[id].transient)
    }

    /// Promote the views of `buffer_id` an edit, a save or a reload is a keep-signal for: the ones
    /// on screen — presented by a viewport whose focus is in the buffer, since that is where the
    /// edit came from — or, when none is, all of them (an edit with no viewport is a script's or a
    /// test's, and means the buffer). A reader you glanced at and left hidden stays a preview.
    /// Returns the views promoted; empty when none was transient.
    pub fn promote_views_of(&mut self, buffer_id: BufferId) -> Vec<ViewId> {
        let shown: Vec<ViewId> = self
            .viewports
            .values()
            .filter(|vp| {
                let view = self.view_of(vp);
                view.presenting == buffer_id && self.focused_buffer(vp) == buffer_id
            })
            .map(|vp| vp.view_id)
            .collect();
        let targets = if shown.is_empty() {
            self.views_presenting(buffer_id)
        } else {
            shown
        };
        let mut promoted = Vec::new();
        for id in targets {
            if self.set_view_transient(id, false) {
                promoted.push(id);
            }
        }
        promoted
    }

    /// Set whether `view` is a preview, and mark its workspace's session as needing a write when
    /// the answer changes. Returns whether it changed — `false` for a view that is not allowed to
    /// change, so a caller echoing the outcome must read the flag back rather than its request.
    ///
    /// **The one place transience is changed**, so that every path that flips it — the `Space k`
    /// toggle, the promotion an edit or a save applies, the pin a `transient: Some(false)` open
    /// asks for, the demotion a close applies to a view a composed view still holds — dirties the
    /// session without having to remember to, and meets the rule below without having to know it.
    /// Transience is part of what a session records now (a preview you were in is where you left
    /// off), so a flip nobody persisted would be lost.
    ///
    /// Only a document's own view — one that is **not composed** — can have its transience
    /// changed after it is created.
    ///
    /// A composed view — a commit's patch, the working changes, a shell, a conversation — is the
    /// presentation of something the user reached by a command, not a document they opened by
    /// name: it is not a buffers-picker row, so keeping one would strand it in a limbo nothing
    /// lists and nothing collects. A shell and a conversation are the mirror case: created kept,
    /// they must stay kept, or `Space k` would arm a transcript to close itself the next time it
    /// is hidden. Both are the same rule — a composed view keeps the flag it was created with —
    /// and [`View::is_composed`] is the test, exactly as it is for "can this be read".
    pub fn set_view_transient(&mut self, view: ViewId, transient: bool) -> bool {
        if self.views.get(&view).is_none_or(View::is_composed) {
            return false;
        }
        self.write_view_transient(view, transient)
    }

    /// [`Self::set_view_transient`] without the composed-view refusal: what a view's **creation**
    /// uses, which is the one moment a composed view's flag is set at all.
    fn write_view_transient(&mut self, view: ViewId, transient: bool) -> bool {
        let Some(v) = self.views.get_mut(&view) else {
            return false;
        };
        if v.transient == transient {
            return false;
        }
        v.transient = transient;
        self.dirty_session_for_view(view);
        true
    }

    /// Mark `workspace`'s session as needing a write. The name is a key of [`Self::workspaces`] —
    /// a bound context is its own entry — which is exactly what the flush hands to
    /// [`crate::handlers::persist_workspace_session`]. See [`Self::sessions_dirty`] for what may
    /// and may not call this.
    pub fn dirty_session(&mut self, workspace: &str) {
        self.sessions_dirty.insert(workspace.to_string());
    }

    /// [`Self::dirty_session`] for the workspace `buffer_id` belongs to. A buffer with no recorded
    /// workspace (a dormant reservation, a buffer mid-teardown) dirties nothing.
    pub fn dirty_session_for_buffer(&mut self, buffer_id: BufferId) {
        if let Some(name) = self.buffer_workspaces.get(&buffer_id).cloned() {
            self.dirty_session(&name);
        }
    }

    /// [`Self::dirty_session`] for the workspace of the buffer `view` presents.
    pub fn dirty_session_for_view(&mut self, view: ViewId) {
        if let Some(buffer_id) = self.try_presenting_buffer(view) {
            self.dirty_session_for_buffer(buffer_id);
        }
    }

    /// The view a buffer presents when no driver built one — matched per kind of generated
    /// content, so a shell cannot fall through to the patch's region-splitting and come out as one
    /// undivided block with no input.
    fn default_view(&self, buffer_id: BufferId) -> View {
        match self.doc_of(buffer_id).generated.as_ref() {
            // A generated patch's own regions: chrome between files and hunks is what splits it.
            Some(Generated::Patch(g)) if !g.decorations.elements.is_empty() => {
                View::over_generated(buffer_id, g)
            }
            Some(Generated::Shell(t)) => View::over_transcript(buffer_id, t),
            Some(Generated::Agent(c)) => {
                View::over_conversation(buffer_id, c, |id| self.doc_of(id).content_lines())
            }
            Some(Generated::Patch(_)) | None => View::whole(buffer_id),
        }
    }

    /// Append to one conversation block's document, and rebuild the view that shows it.
    ///
    /// The agent view's counterpart to [`Self::extend_transcript`], and the reason the two are
    /// separate: a shell writes into the *view's own* document and has to carry the active run's
    /// extent with the write, while a block **is** a document and binds `ElementLines::Whole`, so
    /// there is no extent to move — the element's length is the document's length by construction.
    /// That is what makes appending to a block that is no longer the last one safe, which is
    /// exactly what ACP does every time it updates a tool call.
    ///
    /// Always paired with the layout rebuild, for the same reason `extend_transcript` is: a write
    /// that lands without one leaves a shell painting the block at its old height.
    pub fn extend_block(&mut self, view_buffer: BufferId, block: BufferId, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let Some(doc) = self.try_doc_of_mut(block) else {
            return false;
        };
        // Only ever a block's own document: `write_tail` is generation, not editing, and letting
        // it reach an ordinary document would put un-undoable text into a file.
        if doc.virtual_source.is_none() {
            return false;
        }
        let from = doc.text.len_chars();
        doc.write_tail(from, text);
        if let Some(c) = self
            .try_doc_of_mut(view_buffer)
            .and_then(|d| d.generated.as_mut())
            .and_then(Generated::conversation_mut)
        {
            c.generation += 1;
        }
        self.rebuild_view_layout(view_buffer);
        self.rebind_viewports_of(view_buffer);
        true
    }

    /// Replace one conversation block's document with `text`, and rebuild the view.
    ///
    /// The other half of [`Self::extend_block`], for the blocks whose content arrives whole rather
    /// than in pieces: a diff the agent re-sent, and the plan, which is replaced every time it
    /// changes. Same discipline — through `write_tail` from the start of the document, so the wrap
    /// cache splices, the revision moves, and undo is never touched.
    pub fn set_block_text(&mut self, view_buffer: BufferId, block: BufferId, text: &str) -> bool {
        let Some(doc) = self.try_doc_of_mut(block) else {
            return false;
        };
        if doc.virtual_source.is_none() {
            return false;
        }
        doc.write_tail(0, text);
        if let Some(c) = self
            .try_doc_of_mut(view_buffer)
            .and_then(|d| d.generated.as_mut())
            .and_then(Generated::conversation_mut)
        {
            c.generation += 1;
        }
        self.rebuild_view_layout(view_buffer);
        self.rebind_viewports_of(view_buffer);
        true
    }

    /// Lay `elements` out in view order against their buffers' **live** line counts — the one way
    /// to get a [`ViewLayout`], so no site can build one against a stale or a wrong count. Cheap:
    /// one map lookup per element.
    pub fn layout_of(&self, elements: &[ElementBinding]) -> ViewLayout {
        ViewLayout::of(elements, |id| self.doc_of(id).line_count())
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
        // One place, because the views are the one owner. An element windowing the view's own
        // generated text is never among `siblings` — a patch is read-only, so no edit reaches it —
        // and a whole-buffer element has no extent to move.
        for view in self.views.values_mut() {
            for element in &mut view.elements {
                if siblings.contains(&element.buffer_id) {
                    element.lines.shift(shift.at, shift.delta);
                }
            }
        }
        // The slices each viewport has loaded are lines of the same buffers, and follow the text
        // the same way: type a line into a loaded slice and the slice holds one more line, so the
        // push that follows re-renders it rather than a slice one line short.
        let views = &self.views;
        for vp in self.viewports.values_mut() {
            let Some(view) = views.get(&vp.view_id) else {
                continue;
            };
            for (element, slice) in view.elements.iter().zip(vp.loaded.iter_mut()) {
                if let Some(range) = slice {
                    if siblings.contains(&element.buffer_id) {
                        shift_range(range, shift.at, shift.delta);
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
        generated: Option<Generated>,
    ) {
        self.doc_of_mut(buffer_id)
            .replace_generated(text, generated);
        // A rebuilt patch has different regions *and* different stages — staging is the only thing
        // this view shows changing — so a driver-built layout is rebuilt from the new data, or the
        // view keeps rendering the stages it was opened with.
        self.rebuild_view_layout(buffer_id);
        self.rebind_viewports_of(buffer_id);
    }

    /// Re-derive a view's driver-built layout from its generated document, resolving each file
    /// against the buffers already open.
    ///
    /// By path, not by the ids the old layout held: this runs on the save/stage/commit refresh and
    /// on a buffer's teardown, where opening files would be both surprising and asynchronous, and
    /// where an id may name nothing any more. A file with no open buffer keeps its generated text,
    /// exactly as the open path leaves it (see [`crate::patch::layout_over_files`]).
    ///
    /// `false` when the view has no driver layout to rebuild — an ordinary buffer, or a patch no
    /// driver built — in which case nothing changes.
    pub fn rebuild_view_layout(&mut self, view_buffer: BufferId) -> bool {
        let Some(view_id) = self.view_presenting(view_buffer) else {
            return false;
        };
        let workspace = self.buffer_workspaces.get(&view_buffer).cloned();
        let Some(doc) = self.try_doc_of(view_buffer) else {
            return false;
        };
        let repo_id = doc
            .virtual_source
            .as_ref()
            .and_then(|v| v.target.repo_id())
            .unwrap_or_default()
            .to_string();
        // Per kind, exhaustively: a shell's elements come from its runs and its input, and there
        // is no path resolution to do — treating one as a patch would rebuild it as a single
        // fileless region and lose the input element with it.
        let rebuilt = match doc.generated.as_ref() {
            None => return false,
            Some(Generated::Shell(t)) => {
                let view = View::over_transcript(view_buffer, t);
                self.reinstate(view_id, view);
                return true;
            }
            Some(Generated::Agent(c)) => {
                let view = View::over_conversation(view_buffer, c, |id| {
                    self.try_doc_of(id).map_or(0, |d| d.content_lines())
                });
                self.reinstate(view_id, view);
                return true;
            }
            Some(Generated::Patch(generated)) => {
                crate::patch::layout_over_files(generated, |path| {
                    let workspace = workspace.as_deref()?;
                    let canonical = std::path::Path::new(&repo_id).join(path);
                    self.buffer_for_path_in_workspace(workspace, &canonical)
                })
            }
        };
        let view = View::from_layout(view_buffer, rebuilt);
        self.reinstate(view_id, view);
        true
    }

    /// Replace a view's composition while keeping what makes it *the same view* — its place in the
    /// recency order and whether it is a preview. Both would otherwise reset on every rebuild, and
    /// a shell rebuilds on every flush of output.
    fn reinstate(&mut self, view_id: ViewId, view: View) {
        let (last_used, transient, old_input) =
            self.views.get(&view_id).map_or((0, false, None), |v| {
                (v.last_used, v.transient, v.input_element())
            });
        let new_input = view.input_element();
        let last = view.elements.len().saturating_sub(1) as aether_protocol::viewport::FieldId;
        self.views.insert(
            view_id,
            View {
                last_used,
                transient,
                ..view
            },
        );
        // A viewport's focus is an index into the elements, and a rebuild renumbers them: a shell
        // appends every run *above* its input, so the number that named the input a moment ago
        // names the new run now. Focus follows the element, not the number — a caret in the input
        // stays in the input — and any other focus is clamped as before, since a rebuild can also
        // have fewer elements than the one the focus was in.
        for vp in self
            .viewports
            .values_mut()
            .filter(|vp| vp.view_id == view_id)
        {
            vp.focused = match (old_input, new_input) {
                (Some(old), Some(new)) if vp.focused == old => new,
                _ => vp.focused.min(last),
            };
        }
    }

    /// Change a shell's transcript and rebuild its view from the result — **the pair**, because a
    /// transcript that has grown a run while the view still lists the old ones is a view with an
    /// element missing, and a status that changed with no rebuild is a header still saying
    /// "running".
    ///
    /// `None` when the buffer is not a shell (or has gone), which is how the handlers treat
    /// `shell/*` addressed at something else.
    pub fn with_transcript<R>(
        &mut self,
        view_buffer: BufferId,
        f: impl FnOnce(&mut crate::shell::Transcript) -> R,
    ) -> Option<R> {
        let doc = self.try_doc_of_mut(view_buffer)?;
        let t = doc.generated.as_mut()?.transcript_mut()?;
        let out = f(t);
        t.generation += 1;
        self.rebuild_view_layout(view_buffer);
        self.rebind_viewports_of(view_buffer);
        Some(out)
    }

    /// Append to a shell's transcript: replace the document from char `from` to its end with
    /// `text`, carry the active run's extent to the new length, and rebuild the view.
    ///
    /// The one way to write output into a transcript. [`Document::write_tail`] is module-private
    /// precisely so it cannot be called without this: a tail write that skipped the extent update
    /// would leave the last run's element as long as it was before the output arrived, and the
    /// lines you are watching arrive would render nowhere.
    ///
    /// `from` must be at or after the start of the active run's last line — see
    /// [`crate::process::OutputText::line_start_at`], which is what computes it.
    pub fn extend_transcript(&mut self, view_buffer: BufferId, from: usize, text: &str) -> bool {
        let Some(doc) = self.try_doc_of_mut(view_buffer) else {
            return false;
        };
        if doc
            .generated
            .as_ref()
            .and_then(Generated::transcript)
            .is_none()
        {
            return false;
        }
        doc.write_tail(from, text);
        let end = doc.content_lines();
        // The active run is the last one, and it is the only one whose extent can move: every
        // earlier run's output is final, which is what makes a run's element stable for good.
        if let Some(t) = doc.generated.as_mut().and_then(Generated::transcript_mut) {
            if let Some(run) = t.runs.last_mut() {
                run.end_line_exclusive = end.max(run.start_line);
            }
            t.generation += 1;
        }
        self.rebuild_view_layout(view_buffer);
        self.rebind_viewports_of(view_buffer);
        true
    }

    /// After a view's elements were rebuilt: a rebuild can have fewer elements than the one a
    /// viewport's focus was in, and a focus index past the end would fall back to element 0
    /// silently. The viewports reference the view, so this is all a rebuild has to tell them.
    pub fn rebind_viewports_of(&mut self, view_buffer: BufferId) {
        for view_id in self.views_presenting(view_buffer) {
            let last = self
                .views
                .get(&view_id)
                .map_or(0, |v| v.elements.len().saturating_sub(1))
                as aether_protocol::viewport::FieldId;
            for vp in self.viewports.values_mut() {
                if vp.view_id == view_id {
                    vp.focused = vp.focused.min(last);
                }
            }
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
            .find(|v| v.client_id == client_id && self.view_of(v).binds(buffer_id))
            .map(|v| v.focus(self.view_of(v)))
            .filter(|e| e.buffer_id == buffer_id);
        Ok(match element {
            Some(e) => {
                let lines = e.lines_in(doc.line_count());
                crate::cursor::Scope::windowed(doc, lines.start, lines.end)
            }
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
            },
        );
        self.open_view(buffer_id, Some(transient));
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

    /// The navigation trail `client_id` steps: its own, in the context it is standing in
    /// ([`WorkspaceEntry::nav_history`]). `None` with no active workspace — a client standing
    /// nowhere has no trail — or with nothing recorded there yet.
    ///
    /// This and [`Self::nav_history_mut`] are the only ways in, so every read and write lands on
    /// the trail of the context the client is actually in.
    pub fn nav_history(&self, client_id: ClientId) -> Option<&NavHistory> {
        self.active_workspace(client_id)?
            .nav_history
            .get(&client_id)
    }

    /// [`Self::nav_history`] for writing, creating the client's trail in its active context on
    /// first use. `None` with no active workspace, which records nothing.
    pub fn nav_history_mut(&mut self, client_id: ClientId) -> Option<&mut NavHistory> {
        Some(
            self.active_workspace_mut(client_id)?
                .nav_history
                .entry(client_id)
                .or_default(),
        )
    }

    /// Put `client_id` in `workspace_id`: the one place a client's active context is set, so the
    /// nav-trail hand-over ([`WorkspaceEntry::last_nav`]) can't be skipped by a new caller. A
    /// client arriving without a trail of its own here starts from a clone of the one the last
    /// window to leave left behind.
    pub fn activate_workspace_for_client(&mut self, client_id: ClientId, workspace_id: &str) {
        if let Some(session) = self.clients.get_mut(&client_id) {
            session.active_workspace = Some(workspace_id.to_string());
        }
        if let Some(entry) = self.workspaces.get_mut(workspace_id) {
            entry.join_nav(client_id);
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
                mru_views: VecDeque::new(),
                dormant_views: Vec::new(),
                jumplist: None,
                nav_history: Default::default(),
                last_nav: None,
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
    /// `view/close`; the evicted clients are being told the buffer closed (`view/closed`) and
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
                        && !self.viewports.values().any(|v| self.view_of(v).binds(id))
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
                    for entry in &session.views {
                        if let Some((path, _)) = entry.file_view() {
                            if !live_paths.contains(path)
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
        // An internal document is never dirty (see `Document::internal`), so it cannot pin the
        // reaper — a half-typed shell command is not work to rescue. What *does* pin it is a
        // running command; that lives with the reaper, beside this.
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
            for d in &w.dormant_views {
                if let DormantSource::Scratch { number } = d.source {
                    used.insert(number);
                }
            }
        }
        (1..)
            .find(|n| !used.contains(n))
            .expect("u32 range is non-empty")
    }

    /// Whether any shell is running a command right now.
    ///
    /// Pins the idle reaper open beside the unsaved-work check: an auto-started server that reaped
    /// itself mid-`cargo build` would kill the build and lose its output, which is the same class
    /// of harm as dropping unsaved text. The shell's *input* deliberately does not pin it — a
    /// half-typed command is not work in progress (see `Document::internal`).
    pub fn has_running_shell(&self) -> bool {
        self.documents
            .values()
            .filter_map(|d| d.transcript())
            .any(|t| t.active().is_some())
    }

    /// Stop every shell's running command — the server is going away, and a process group that
    /// outlives the editor that started it is an orphan nobody can see or stop.
    pub fn cancel_all_shell_runs(&mut self) {
        for doc in self.documents.values_mut() {
            if let Some(t) = doc.generated.as_mut().and_then(Generated::transcript_mut) {
                t.cancel_all();
            }
        }
    }

    /// The display number to give a new shell in `workspace`: the lowest positive integer no live
    /// shell there is using. Same rule as [`Self::next_scratch_number`], and for the same reasons —
    /// small numbers, stable for the shell's life, reused once it closes.
    ///
    /// Dormant shells — restored from the session but not yet opened — hold their numbers too, so
    /// a fresh shell cannot take one out from under a pending restore.
    pub fn next_shell_number(&self, workspace: &str) -> u32 {
        let mut used: std::collections::HashSet<u32> = self
            .buffer_workspaces
            .iter()
            .filter(|(_, p)| p.as_str() == workspace)
            .filter_map(|(id, _)| self.try_doc_of(*id))
            .filter_map(|d| match d.virtual_source.as_ref().map(|v| &v.target) {
                Some(VirtualTarget::Shell { number, .. }) => Some(*number),
                _ => None,
            })
            .collect();
        if let Some(w) = self.workspaces.get(workspace) {
            for d in &w.dormant_views {
                if let DormantSource::Shell { number } = d.source {
                    used.insert(number);
                }
            }
        }
        (1..)
            .find(|n| !used.contains(n))
            .expect("u32 range is non-empty")
    }

    /// Write `id`'s shell to its snapshot file now, if backups are on and the shell belongs to a
    /// named workspace. What the flush does on its interval, done at once — for a teardown that
    /// is about to drop the shell, so the last moments of its state are not lost with it.
    pub fn snapshot_shell(&self, id: BufferId) {
        let Some(root) = self.backups_path.as_deref() else {
            return;
        };
        let Some(workspace) = self.buffer_workspaces.get(&id) else {
            return;
        };
        if self
            .workspaces
            .get(workspace)
            .is_none_or(|w| w.name.is_none())
        {
            return;
        }
        let Some(doc) = self.try_doc_of(id) else {
            return;
        };
        let Some(t) = doc.transcript() else {
            return;
        };
        let Some(VirtualTarget::Shell { number, .. }) =
            doc.virtual_source.as_ref().map(|v| &v.target)
        else {
            return;
        };
        let text: String = doc.text.chunks().collect();
        let input: String = self
            .try_doc_of(t.input)
            .map(|d| d.text.chunks().collect())
            .unwrap_or_default();
        let snap = t
            .snapshot(&text, &input)
            .trimmed(crate::shell::SNAPSHOT_BUDGET);
        if let Ok(json) = serde_json::to_string(&snap) {
            if let Err(e) = crate::backup::write(
                &crate::backup::shell_backup_path(root, workspace, *number),
                &json,
            ) {
                tracing::warn!(error = %e, "failed to write shell snapshot");
            }
        }
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
    /// canonical `view/close` teardown, shared by root removal, workspace deletion, and path
    /// deletion so they can't drift out of sync.
    /// Returns the key of a language server that was torn down because this was its last buffer
    /// (so the caller can refresh open status views), or `None`.
    /// Write a conversation down, if `id` is one.
    ///
    /// The blocks' text lives in their own documents, so unlike a shell's this has to gather it —
    /// which is also why the snapshot is the *display* and not the agent's memory: those are the
    /// blocks we watched arrive, including the tool calls and diffs an agent is under no obligation
    /// to replay when a session is loaded back.
    ///
    /// A session id is recorded only once the agent has **confirmed** it (`Conversation::session`,
    /// set from `AgentEvent::Ready`), never one we were merely trying to resume: a snapshot naming
    /// a session that failed to load would claim a context nobody holds.
    pub fn snapshot_agent(&self, id: BufferId) {
        let Some(root) = self.backups_path.as_deref() else {
            return;
        };
        let Some(workspace) = self.buffer_workspaces.get(&id) else {
            return;
        };
        if self
            .workspaces
            .get(workspace)
            .is_none_or(|w| w.name.is_none())
        {
            return;
        }
        let Some(doc) = self.try_doc_of(id) else {
            return;
        };
        let Some(c) = doc.conversation() else {
            return;
        };
        let Some(VirtualTarget::Agent { number, .. }) =
            doc.virtual_source.as_ref().map(|v| &v.target)
        else {
            return;
        };
        let input: String = self
            .try_doc_of(c.input)
            .map(|d| d.text.chunks().collect())
            .unwrap_or_default();
        let snap = c
            .snapshot(&input, |b| {
                self.try_doc_of(b)
                    .map(|d| d.text.chunks().collect())
                    .unwrap_or_default()
            })
            .trimmed(crate::agent::SNAPSHOT_BUDGET);
        if let Ok(json) = serde_json::to_string(&snap) {
            if let Err(e) = crate::backup::write(
                &crate::backup::agent_backup_path(root, workspace, *number),
                &json,
            ) {
                tracing::warn!(error = %e, "failed to write agent snapshot");
            }
        }
    }

    pub fn close_buffer(&mut self, id: BufferId) -> Option<crate::lsp::manager::LspServerKey> {
        // A shell owns two documents and a process group. Stopping the runs here — rather than in
        // the `shell/*` handlers — is what makes it unconditional: a view closed by a workspace
        // switch, a root removal or a disconnect kills its `cargo build` exactly as `Space x`
        // does. The input goes with it: nothing else can reach it, so leaving it behind would be a
        // buffer nobody can open and nobody can close.
        // Written down before it goes: a workspace switch or a disconnect tears the shell down,
        // and what it held since the last flush must not go with it. An explicit close deletes
        // the file again afterwards, which is what makes closing a discard.
        self.snapshot_shell(id);
        // Same reason, same moment: a conversation torn down by a switch or a disconnect keeps
        // what it held.
        self.snapshot_agent(id);
        let input = match self
            .try_doc_of_mut(id)
            .and_then(|d| d.generated.as_mut())
            .and_then(Generated::transcript_mut)
        {
            Some(t) => {
                t.cancel_all();
                Some(t.input)
            }
            None => None,
        };
        if let Some(input) = input.filter(|input| *input != id) {
            self.close_buffer(input);
        }
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
        // A view's own layout dies with the view. A *surviving* layout that windowed this buffer
        // is re-derived without it — the element falls back to the view's generated text, exactly
        // as it would have opened had the file been unavailable — because no layout may keep naming
        // a buffer that is gone: `element_bindings` feeds every element's id to `ViewLayout::of`,
        // which resolves it through `doc_of`, documented to panic on an unknown buffer.
        //
        // Re-derived rather than unbound in place: an element's extent is in *file* lines while it
        // is bound and in the generated patch's lines once it is not, so editing the one element
        // would reinterpret one space as the other — a silently wrong window in place of a panic.
        // And re-derived rather than dropped: dropping the layout dropped the review's viewports
        // with it, and the client whose review it was simply stopped receiving updates.
        let gone = self.views_presenting(id);
        self.drop_buffer_from_mru(id);
        self.views.retain(|view_id, _| !gone.contains(view_id));
        let windowed_by: Vec<BufferId> = self
            .views
            .values()
            .filter(|view| view.binds(id))
            .map(|view| view.presenting)
            .collect();
        for view in windowed_by {
            if self.rebuild_view_layout(view) {
                self.rebind_viewports_of(view);
            } else {
                let ids = self.views_presenting(view);
                self.views.retain(|view_id, _| !ids.contains(view_id));
            }
        }
        // Only the viewports *presenting* this buffer go with it; one that merely windowed it has
        // just been rebound. The views presenting, not `binds`: a viewport presenting a patch binds
        // no element of it, so a `binds` test left it alive with a `view_id` pointing at a buffer
        // that no longer existed.
        self.viewports.retain(|_, v| !gone.contains(&v.view_id));
        self.cursors.retain(|(_, b), _| *b != id);
        self.read.retain(|(_, b), _| *b != id);
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
        self.last_scroll.retain(|(_, v), _| !gone.contains(v));
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
    /// Not consulted by `view/close`: closing on purpose with unsaved changes is the user's call
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

    /// Collect what hiding left behind: among `candidates` — the buffers a torn-down viewport was
    /// showing — a buffer whose view is a preview and which nothing shows goes. A buffer lives
    /// exactly as long as some view uses it: presented by a viewport, kept, or bound into a
    /// composed view someone is looking at.
    ///
    /// Returns the buffers closed and the language servers stopped with them — the pickers
    /// re-list on either.
    pub fn close_orphaned_transients(
        &mut self,
        candidates: impl IntoIterator<Item = BufferId>,
    ) -> (Vec<BufferId>, Vec<crate::lsp::manager::LspServerKey>) {
        let mut closed = Vec::new();
        let mut stopped = Vec::new();
        for id in candidates {
            if !self.buffers.contains_key(&id) {
                continue;
            }
            // A field of a view is not a buffer anyone opened, so it is not one the GC may close:
            // it has no view of its own to be "hidden", and closing it would take a shell's input
            // line away the moment you looked at something else. It dies with the view that owns
            // it — see `ServerState::close_buffer`.
            if self.try_doc_of(id).is_some_and(|d| d.internal) {
                continue;
            }
            let eligible = self.buffer_is_transient(id)
                && !self.close_would_orphan_unsaved(id)
                // `shows`, not `binds`: a patch's viewers are watching the *view*, and no element
                // windows it, so asking only about bindings said "nothing is showing this" about
                // the document on screen. The GC and the push fan-out now ask the same question —
                // a buffer that is live enough to receive notifications is live enough to keep.
                && !self.viewports.values().any(|v| v.shows(self.view_of(v), id));
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
    /// that belonged to it, tearing down all per-buffer state (same teardown as `view/close` /
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
        let id = self.next_id;
        self.next_id += 1;
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

    /// Release every undo-group hold (`element/undo_group`) the given client owns. Used on
    /// disconnect: a bracket the holder can no longer close would otherwise fold every later edit
    /// on the document, anyone's, into one undo step forever.
    pub fn drop_undo_group_holds_for_client(&mut self, client_id: ClientId) {
        for doc in self.documents.values_mut() {
            if doc.undo_group_holder() == Some(client_id) {
                doc.close_undo_group();
            }
        }
    }

    /// Remove all last-scroll records for the given client. Used on disconnect.
    pub fn drop_last_scroll_for_client(&mut self, client_id: ClientId) {
        self.last_scroll.retain(|(c, _), _| *c != client_id);
    }

    /// Record where `view_id` is scrolled to for `client_id`, with the cursor that scroll frames —
    /// the focused element's, read through the viewport. See [`ScrollMemory`].
    pub fn remember_scroll(
        &mut self,
        client_id: ClientId,
        view_id: ViewId,
        viewport_id: ViewportId,
        anchor: ScrollPosition,
    ) {
        let cursor = self
            .viewports
            .get(&viewport_id)
            .map(|vp| self.focused_cursor(client_id, vp));
        self.last_scroll
            .insert((client_id, view_id), ScrollMemory { anchor, cursor });
    }

    /// A viewport is going away: stamp its view's scroll memory with the cursor as it stands, so
    /// the memory says where the cursor was when the view was last *seen*. The cursor moves within
    /// the loaded window without a window being asked for, so the last write is not enough.
    pub fn stamp_hidden_cursor(&mut self, vp: &Viewport) {
        let cursor = self.focused_cursor(vp.client_id, vp);
        if let Some(memory) = self.last_scroll.get_mut(&(vp.client_id, vp.view_id)) {
            memory.cursor = Some(cursor);
        }
    }

    /// The focused element's buffer of `vp`, and `client_id`'s cursor in it.
    fn focused_cursor(
        &self,
        client_id: ClientId,
        vp: &Viewport,
    ) -> (BufferId, aether_protocol::LogicalPosition) {
        let buffer = self.focused_buffer(vp);
        (buffer, self.cursor_position(client_id, buffer))
    }

    fn cursor_position(
        &self,
        client_id: ClientId,
        buffer: BufferId,
    ) -> aether_protocol::LogicalPosition {
        self.cursors
            .get(&(client_id, buffer))
            .map_or_else(Default::default, |c| c.position)
    }

    /// The scroll to restore on an open of `view_id` for `client_id`: the remembered one while the
    /// cursor it framed is still where it was, else `None`, so the client frames the cursor
    /// instead ([`ScrollMemory`]).
    pub fn restorable_scroll(
        &self,
        client_id: ClientId,
        view_id: ViewId,
    ) -> Option<ScrollPosition> {
        let memory = self.last_scroll.get(&(client_id, view_id))?;
        let unmoved = memory
            .cursor
            .is_none_or(|(buffer, position)| self.cursor_position(client_id, buffer) == position);
        unmoved.then_some(memory.anchor)
    }

    /// Remove all picker state for the given client. Used on disconnect.
    pub fn drop_pickers_for_client(&mut self, client_id: ClientId) {
        self.pickers.retain(|(c, _), _| *c != client_id);
    }

    /// Take the disconnecting client's navigation trails out of every context it holds one in,
    /// leaving each behind as that context's hand-over copy ([`WorkspaceEntry::last_nav`]). The
    /// window is gone, so its live trails go with it; the next client to activate the context
    /// picks up where it left off, which is what makes closing and reopening a window keep
    /// `Alt-Left` meaningful.
    pub fn drop_nav_history_for_client(&mut self, client_id: ClientId) {
        for entry in self.workspaces.values_mut() {
            entry.leave_nav(client_id, true);
        }
    }

    /// Bump the view `buffer_id` presents — its most recently used one, which is the one an open
    /// of it just presented — to the front of its workspace's MRU. Called from `view/open` every
    /// time any client lands on a buffer: fresh open, reopen, or attach-by-id. No-op if the
    /// buffer has no recorded workspace (shouldn't happen for live buffers but the lookup is
    /// defensive).
    pub fn touch_mru(&mut self, buffer_id: BufferId) {
        if let Some(view) = self.view_presenting(buffer_id) {
            self.touch_mru_view(view);
        }
    }

    /// Bump `view` to the front of its workspace's MRU.
    pub fn touch_mru_view(&mut self, view: ViewId) {
        let Some(buffer_id) = self.try_presenting_buffer(view) else {
            return;
        };
        let Some(workspace_name) = self.buffer_workspaces.get(&buffer_id).cloned() else {
            return;
        };
        let Some(workspace) = self.workspaces.get_mut(&workspace_name) else {
            return;
        };
        workspace.mru_views.retain(|&v| v != view);
        workspace.mru_views.push_front(view);
        // The MRU *is* the session's list, so landing anywhere changes what it should say. This is
        // also the write that covers the ordinary "move on" case: the hide collector drops the
        // preview you left, and this dirty makes the next write omit it.
        self.dirty_session(&workspace_name);
    }

    /// Drop every view of `buffer_id` from every workspace's MRU. Called from `view/close` so a
    /// closed buffer doesn't reappear at the top of the picker on the next open.
    ///
    /// **Does not dirty the session** — see [`Self::sessions_dirty`]. The `view/close` handler
    /// dirties for itself, after the close; the hide collector must not, and it reaches here.
    pub fn drop_buffer_from_mru(&mut self, buffer_id: BufferId) {
        let views = self.views_presenting(buffer_id);
        for workspace in self.workspaces.values_mut() {
            workspace.mru_views.retain(|v| !views.contains(v));
        }
    }

    /// Drop one view from every workspace's MRU — a sibling closed on its own. Dirties nothing,
    /// for [`Self::drop_buffer_from_mru`]'s reason.
    pub fn drop_view_from_mru(&mut self, view: ViewId) {
        for workspace in self.workspaces.values_mut() {
            workspace.mru_views.retain(|&v| v != view);
        }
    }

    /// `workspace_name`'s most recently used **live** view, if any — where a close lands and where
    /// activation lands.
    pub fn mru_view(&self, workspace_name: &str) -> Option<ViewId> {
        self.workspaces
            .get(workspace_name)?
            .mru_views
            .iter()
            .copied()
            .find(|v| self.views.contains_key(v))
    }

    /// The buffer of [`Self::mru_view`].
    pub fn mru_buffer(&self, workspace_name: &str) -> Option<BufferId> {
        self.try_presenting_buffer(self.mru_view(workspace_name)?)
    }

    /// The buffers to persist for `workspace_name`, most-recently-used first: the workspace's live
    /// MRU buffers, then its still-dormant buffers (already in MRU order). Files are keyed by path,
    /// scratches by number; deduplicated by each. This is exactly what a future activation should
    /// restore, so it's what gets written to the session file.
    ///
    /// What's excluded: **clean scratch** buffers, and nothing else. A scratch is only worth
    /// restoring if it has unsaved content (it's dirty, hence has a backup); an empty scratch is
    /// dropped.
    ///
    /// **Previews are included, marked as previews.** The hide collector
    /// ([`Self::close_orphaned_transients`]) has already closed every transient view nothing
    /// shows and dropped it from the MRU, so the transient views here are exactly the ones a
    /// connected window is looking at — one per window. Recording them verbatim adds "where you
    /// left off" (exit in a diff, come back to it) and nothing else: transience is honoured only
    /// for the landing view, and activation drops every other transient row it restored.
    ///
    /// **Virtual** buffers ([`VirtualSource`]) therefore reach the file whether kept or previewed,
    /// keyed by `VirtualSource::key`, which is stable across restarts (a repo id is its canonical
    /// workdir). A revision you pinned comes back as a listed row; one you were merely looking at
    /// comes back as the landing and closes as soon as you open something else.
    pub fn session_views(&self, workspace_name: &str) -> Vec<crate::config::SessionView> {
        use crate::config::SessionView;
        let Some(workspace) = self.workspaces.get(workspace_name) else {
            return Vec::new();
        };
        let mut out: Vec<SessionView> = Vec::new();
        // A file is one entry, carrying how it was last shown — read as a document, or edited.
        let mut seen_files: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut seen_scratch: std::collections::HashSet<u32> = std::collections::HashSet::new();

        let mut seen_agent: std::collections::HashSet<u32> = Default::default();
        let mut seen_shell: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut seen_virtual: std::collections::HashSet<String> = std::collections::HashSet::new();
        for view_id in &workspace.mru_views {
            let Some(view) = self.views.get(view_id) else {
                continue;
            };
            let transient = view.transient;
            let id = view.presenting;
            let Some(buf) = self.buffers.get(&id) else {
                continue;
            };
            let Some(doc) = self.documents.get(&buf.document) else {
                continue;
            };
            if let Some(path) = doc.canonical_path.as_deref() {
                // As it was last shown. A file nobody has landed on through a client — one bound
                // into a review and kept — has no showing to remember, and comes back as source.
                if seen_files.insert(path.to_path_buf()) {
                    out.push(SessionView::file(
                        path.to_path_buf(),
                        doc.read_last.unwrap_or(false),
                        transient,
                    ));
                }
            } else if let Some(source) = doc.virtual_source.as_ref() {
                // A shell is recorded by its number; its content survives as a snapshot in the
                // backups directory, keyed the same way, and an entry with no snapshot is dropped
                // at activation exactly as a scratch's is.
                if let VirtualTarget::Shell { number, .. } = source.target {
                    if seen_shell.insert(number) {
                        out.push(SessionView::Shell { number, transient });
                    }
                    continue;
                }
                // A conversation is recorded the same way, and for the same reason: it comes back
                // from its snapshot, not by re-materialising a target. Recorded as a *`Virtual`*
                // entry it came back as a dormant row nothing could open — `virtual_buffer_for`
                // only finds buffers that are already live.
                if let VirtualTarget::Agent { number, .. } = source.target {
                    if seen_agent.insert(number) {
                        out.push(SessionView::Agent { number, transient });
                    }
                    continue;
                }
                let key = source.target.key();
                if seen_virtual.insert(key.clone()) {
                    out.push(SessionView::Virtual { key, transient });
                }
            } else if let Some(number) = buf.scratch_number {
                // Only dirty scratches carry content worth restoring (and therefore a backup).
                if doc.dirty && seen_scratch.insert(number) {
                    out.push(SessionView::Scratch { number, transient });
                }
            }
        }
        // A path with a live buffer supersedes its dormant row: the live buffer is what is
        // persisted for it, and a row it did not absorb is stale.
        for d in &workspace.dormant_views {
            match &d.source {
                DormantSource::File(path) => {
                    if seen_files.insert(path.clone()) {
                        out.push(SessionView::file(path.clone(), d.read, d.transient));
                    }
                }
                DormantSource::Scratch { number } => {
                    if seen_scratch.insert(*number) {
                        out.push(SessionView::Scratch {
                            number: *number,
                            transient: d.transient,
                        });
                    }
                }
                DormantSource::Virtual { key } => {
                    if seen_virtual.insert(key.clone()) {
                        out.push(SessionView::Virtual {
                            key: key.clone(),
                            transient: d.transient,
                        });
                    }
                }
                DormantSource::Shell { number } => {
                    if seen_shell.insert(*number) {
                        out.push(SessionView::Shell {
                            number: *number,
                            transient: d.transient,
                        });
                    }
                }
                DormantSource::Agent { number } => {
                    if seen_agent.insert(*number) {
                        out.push(SessionView::Agent {
                            number: *number,
                            transient: d.transient,
                        });
                    }
                }
            }
        }
        out
    }

    /// Remove the dormant entries for `canonical` from `workspace_name`, returning how they were
    /// presented, most recently used first. Called when a live buffer for that path opens, so the
    /// now-loaded file doesn't also show as a dormant row — and so the caller can give the buffer
    /// what the entry stood for: as it was last shown, kept or a preview.
    pub fn promote_dormant(
        &mut self,
        workspace_name: &str,
        canonical: &Path,
    ) -> Vec<DormantPresentation> {
        let mut found = Vec::new();
        if let Some(workspace) = self.workspaces.get_mut(workspace_name) {
            workspace.dormant_views.retain(|d| {
                if d.path() == Some(canonical) {
                    found.push(d.presentation());
                    false
                } else {
                    true
                }
            });
        }
        if !found.is_empty() {
            self.dirty_session(workspace_name);
        }
        found
    }

    /// Give `buffer_id` what its dormant entries stood for — the presentations
    /// [`Self::promote_dormant`] returned. The view comes back as the first (most recent) entry
    /// recorded it, kept or a preview, whichever open materialised the file — a preview bound by a
    /// review included — and the file remembers that entry's mode as its last showing, unless it
    /// already remembers one. Nothing happens for an empty list.
    pub fn restore_dormant_views(
        &mut self,
        buffer_id: BufferId,
        entries: Vec<DormantPresentation>,
    ) {
        let Some(&first) = entries.first() else {
            return;
        };
        self.open_view(buffer_id, Some(first.transient));
        if self.readable(buffer_id) {
            self.doc_of_mut(buffer_id)
                .read_last
                .get_or_insert(first.read);
        }
    }

    /// Restore the dormant list's invariant in `workspace_name`: **at most one entry per path, and
    /// none for a path that already has a live buffer.**
    ///
    /// `promote_dormant` keeps this at the one moment a path is materialised, which is enough while
    /// entries only ever arrive one at a time. A worktree rebind adds a whole set at once *and*
    /// rewrites the paths of the entries already there, so two of them can land on the same file —
    /// and a live buffer opened by another client can appear beside one. Both show up as a
    /// duplicated row in the view picker.
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
        let before = workspace.dormant_views.len();
        workspace.dormant_views.retain(|d| match d.path() {
            // A scratch has no path to collide on; it is identified by its number.
            None => true,
            Some(path) => !live.contains(path) && seen.insert(path.to_path_buf()),
        });
        if workspace.dormant_views.len() != before {
            self.dirty_session(workspace_name);
        }
    }

    /// Drop every dormant row in `workspace_name` still marked transient, except `keep`.
    ///
    /// **Transience is honoured only for the landing.** Two windows leave two previews in the
    /// session, and only the front one is where you left off; a tethered launch (`ae file.rs`)
    /// lands elsewhere entirely and must not leave the preview behind as a listed row. A dormant
    /// row is re-written on every persist, so an unopened transient one would otherwise never
    /// die. Called once per activation, after the landing view has been decided — `keep` is that
    /// view when this activation is about to open it.
    pub fn drop_transient_dormant(&mut self, workspace_name: &str, keep: Option<ViewId>) {
        let Some(workspace) = self.workspaces.get_mut(workspace_name) else {
            return;
        };
        let before = workspace.dormant_views.len();
        workspace
            .dormant_views
            .retain(|d| !d.transient || Some(d.view) == keep);
        if workspace.dormant_views.len() != before {
            self.dirty_session(workspace_name);
        }
    }

    /// Remove and return the dormant buffer with `id` in `workspace_name`, if any. Used by
    /// `view/open`'s by-id path to materialize a dormant buffer the picker selected — the caller
    /// inspects [`DormantView::source`] to decide whether to load a file or rebuild a scratch.
    /// The reserved buffer id of `workspace_name`'s dormant row whose reserved view is `view`.
    pub fn dormant_buffer_of_view(&self, workspace_name: &str, view: ViewId) -> Option<BufferId> {
        self.workspaces
            .get(workspace_name)?
            .dormant_views
            .iter()
            .find(|d| d.view == view)
            .map(|d| d.id)
    }

    pub fn take_dormant(&mut self, workspace_name: &str, id: BufferId) -> Option<DormantView> {
        let workspace = self.workspaces.get_mut(workspace_name)?;
        let pos = workspace.dormant_views.iter().position(|d| d.id == id)?;
        let taken = workspace.dormant_views.remove(pos);
        self.dirty_session(workspace_name);
        Some(taken)
    }

    /// The id of `workspace_name`'s most-recently-used dormant buffer (front of the list), if any.
    /// Used as the activation landing target when the workspace has no live MRU buffer yet (a cold
    /// restore after a restart).
    pub fn first_dormant_buffer(&self, workspace_name: &str) -> Option<BufferId> {
        self.workspaces
            .get(workspace_name)?
            .dormant_views
            .first()
            .map(|d| d.id)
    }

    /// The reserved view of `workspace_name`'s most recently used dormant row — what a client
    /// lands on after a cold restore, when nothing is live yet.
    pub fn first_dormant_view(&self, workspace_name: &str) -> Option<ViewId> {
        self.workspaces
            .get(workspace_name)?
            .dormant_views
            .first()
            .map(|d| d.view)
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
    /// during `view/open` for workspace A shouldn't latch onto workspace B's existing buffer.
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
            .filter(|v| {
                v.client_id == client_id && workspace_buffers.contains(&self.focused_buffer(v))
            })
            .flat_map(|v| v.shown_buffers(self.view_of(v)))
            .filter(|b| workspace_buffers.contains(b))
            .collect();
        // Hidden, not closed: what these viewports were showing comes back on re-entry, so their
        // scroll memories take the cursor as it stands now.
        let leaving: Vec<Viewport> = self
            .viewports
            .values()
            .filter(|v| {
                v.client_id == client_id
                    && workspace_buffers.contains(&v.buffer_id(&self.views[&v.view_id]))
            })
            .cloned()
            .collect();
        for vp in &leaving {
            self.stamp_hidden_cursor(vp);
        }
        let views = &self.views;
        self.viewports.retain(|_, v| {
            !(v.client_id == client_id
                && workspace_buffers.contains(&v.buffer_id(&views[&v.view_id])))
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
        // reason: the view picker filters by active workspace, so cross-workspace MRU entries
        // don't leak into the UI, but they still let us reattach to "the buffer you last had"
        // when you come back. The nav trail is preserved too, and for a stronger reason: it
        // belongs to the context, not to the switch (see the hand-over below).
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

        // The nav trail stays where it is — on the context being left, under this client — so
        // coming back finds back and forward as they were, and stepping in the context switched
        // *to* steps that context's own trail. A copy stays behind as its hand-over
        // (`WorkspaceEntry::last_nav`) for the next window to arrive here without one.
        if let Some(entry) = self.workspaces.get_mut(workspace_name) {
            entry.leave_nav(client_id, false);
        }
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
    /// Display name (`abc1234 — subject`, `src/main.rs`, `Working changes`, `Shell 2`), shipped as
    /// `ViewOpenResult::title`.
    pub title: String,
    /// The revision the content is *as of*, abbreviated — `Some` only for a file at a revision,
    /// whose [`Self::title`] is then the bare path. Shipped as `ViewOpenResult::commit`, which
    /// every shell paints muted after the name.
    ///
    /// Carried rather than derived from [`Self::target`]: the target holds the revision as the
    /// caller *named* it (`HEAD`, `HEAD~1`, a full hash), and what a buffer is labelled with is
    /// the commit that resolved to.
    pub commit: Option<String>,
}

/// What a virtual document was generated from — content the server produced rather than loaded.
///
/// **Structured, not a string.** It was a string once, and every consumer that wanted one field out
/// of it — the repo, the rev, the path — re-split it by hand, each slightly differently
/// (`rsplit_once('@')`, `split_once(':')`). Adding a fourth shape quietly broke one of those
/// splits, which is the failure mode this exists to remove: ask for the field you want and a shape
/// that hasn't got one answers `None`.
///
/// A **shell** is the second producer, and it is not a repo at all: it has no revision, no path
/// and no repo id, which is exactly why the repo fields moved inside a variant instead of staying
/// at the top with a shell obliged to invent values for them.
///
/// The string form survives only as an *encoding*, for the one place that needs to write a target
/// down and read it back: the session file. See [`Self::key`] and [`Self::parse_key`]. A shell is
/// never written there — its content does not survive a restart — but the encoding still round
/// trips, because a key that only half works is a key that fails somewhere else later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualTarget {
    /// A repo, plus which of its states: a commit's diff, a file at a revision, the working tree.
    Git {
        /// Canonical workdir of the repo — which is what makes a key stable across restarts.
        repo_id: String,
        what: aether_protocol::git::ShowTarget,
    },
    /// One shell view's transcript, numbered per workspace like a scratch buffer.
    Shell { workspace: String, number: u32 },
    /// One agent view's conversation, numbered per workspace like a shell. The document this
    /// targets holds no text of its own — every block has a document — but it is still the thing
    /// the view presents, and so still needs a target.
    Agent { workspace: String, number: u32 },
}

impl VirtualTarget {
    pub fn new(repo_id: impl Into<String>, what: aether_protocol::git::ShowTarget) -> Self {
        Self::Git {
            repo_id: repo_id.into(),
            what,
        }
    }

    pub fn shell(workspace: impl Into<String>, number: u32) -> Self {
        Self::Shell {
            workspace: workspace.into(),
            number,
        }
    }

    pub fn agent(workspace: impl Into<String>, number: u32) -> Self {
        Self::Agent {
            workspace: workspace.into(),
            number,
        }
    }

    /// The repo this is of, for the git-shaped targets. `None` for a shell, which has none —
    /// which is the whole point of asking rather than reading a field.
    pub fn repo_id(&self) -> Option<&str> {
        match self {
            Self::Git { repo_id, .. } => Some(repo_id),
            Self::Shell { .. } | Self::Agent { .. } => None,
        }
    }

    /// Which state of the repo this shows. `None` for a shell.
    pub fn what(&self) -> Option<&aether_protocol::git::ShowTarget> {
        match self {
            Self::Git { what, .. } => Some(what),
            Self::Shell { .. } | Self::Agent { .. } => None,
        }
    }

    pub fn rev(&self) -> Option<&str> {
        self.what()?.rev()
    }

    pub fn path(&self) -> Option<&str> {
        self.what()?.path()
    }

    /// Whether re-showing this can attach to a buffer already holding it. A revision can't change
    /// under us; the working tree can, and has to be rebuilt. A shell is neither: it is addressed
    /// by its own view, never re-materialised from a target.
    pub fn is_immutable(&self) -> bool {
        self.rev().is_some()
    }

    /// Whether this materialises a **composed** view — a generated presentation built over other
    /// buffers — rather than a document: a read-only but genuine file, its content as of a
    /// revision.
    ///
    /// A commit's patch and the working changes are composed: their text exists to *be* the
    /// view, and what you actually read lives in the buffers their elements window. A shell's
    /// transcript and an agent's conversation are composed too. Only a document is something the
    /// user opened by name and can name again, which is what the buffers picker lists — the
    /// live-view form of the same question is [`View::is_composed`].
    pub fn is_composed(&self) -> bool {
        !matches!(
            self.what(),
            Some(aether_protocol::git::ShowTarget::File { .. })
        )
    }

    /// [`Self::is_composed`] for a target written down as a [`Self::key`] — a session entry, or a
    /// dormant row. A key that no longer parses names nothing a picker could list, so it counts
    /// as composed.
    pub fn key_is_composed(key: &str) -> bool {
        Self::parse_key(key).is_none_or(|t| t.is_composed())
    }

    /// The buffer's display name — what the picker rows and the status bar show.
    pub fn title(&self) -> Option<String> {
        match self {
            Self::Git { .. } => None, // generated with the content; see `VirtualSource::title`
            Self::Shell { number, .. } => Some(format!("Shell {number}")),
            Self::Agent { number, .. } => Some(format!("Agent {number}")),
        }
    }

    /// Stable string encoding, for writing a target into the session file.
    ///
    /// `#` separates the working tree rather than `@`, so a round-trip can never mistake it for a
    /// revision named `worktree`. A shell leads with `shell:`, which no canonical workdir can:
    /// a repo id is an absolute path.
    pub fn key(&self) -> String {
        use aether_protocol::git::ShowTarget;
        match self {
            Self::Git { repo_id, what } => match what {
                ShowTarget::Commit { rev } => format!("{repo_id}@{rev}"),
                ShowTarget::File { rev, path } => format!("{repo_id}@{rev}:{path}"),
                ShowTarget::WorkingChanges => format!("{repo_id}#worktree"),
            },
            Self::Shell { workspace, number } => format!("shell:{number}:{workspace}"),
            Self::Agent { workspace, number } => format!("agent:{number}:{workspace}"),
        }
    }

    /// Inverse of [`Self::key`]. Split from the right: a repo id is a filesystem path and may
    /// itself contain `@`; the remainder can't, so the `:` split after it is unambiguous.
    pub fn parse_key(key: &str) -> Option<Self> {
        use aether_protocol::git::ShowTarget;
        if let Some(rest) = key.strip_prefix("shell:") {
            let (number, workspace) = rest.split_once(':')?;
            return Some(Self::shell(workspace, number.parse().ok()?));
        }
        if let Some(rest) = key.strip_prefix("agent:") {
            let (number, workspace) = rest.split_once(':')?;
            return Some(Self::agent(workspace, number.parse().ok()?));
        }
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

/// Where an agent view's agent comes from.
///
/// A three-way enum rather than an optional dummy, because the third state is the one that
/// matters: a **test server refuses to launch anything**. The seam that installs a dummy is opt-in
/// per test, and an `Option` would have meant that a test which forgot to install one silently ran
/// `npx @agentclientprotocol/claude-agent-acp` — spawning a real coding agent, with real
/// credentials, doing real work, per test. That is not a mistake to leave one forgotten line away;
/// [`Self::Refuse`] makes it unreachable by construction instead, and the test that forgot gets a
/// clear error rather than a subprocess.
pub enum AgentLauncher {
    /// Production: launch the subprocess the agent table names.
    Subprocess,
    /// A test with an in-process dummy agent installed. See [`crate::agent::dummy`].
    Dummy(std::sync::Arc<dyn Fn() -> agent_client_protocol::Channel + Send + Sync>),
    /// A test server that has not installed one. Launches nothing, ever.
    Refuse,
}

impl std::fmt::Debug for AgentLauncher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Subprocess => f.write_str("Subprocess"),
            Self::Dummy(_) => f.write_str("Dummy"),
            Self::Refuse => f.write_str("Refuse"),
        }
    }
}

/// Decorations and structure computed once when a document's content was **generated** — for the
/// documents no grammar spans and no file backs.
///
/// A closed enum rather than one nullable patch, because there are two kinds now and the
/// difference matters everywhere the content is read: a patch divides into files and hunks, a
/// shell into runs. Every reader that cares matches exhaustively (`ServerState::default_view`,
/// `rebuild_view_layout`, `view_outline_of`, `change_anchors`), so a third kind cannot be silently
/// rendered as a patch — which is exactly what a bare `Option<GeneratedPatch>` would have allowed.
/// Readers that genuinely only want a patch ask for one ([`Document::patch`]) and get `None`.
#[derive(Debug)]
pub enum Generated {
    /// `git/show`: a commit's diff, or the working tree's.
    Patch(crate::patch::GeneratedPatch),
    /// A shell view's transcript — its runs, its input, and where it runs.
    Shell(crate::shell::Transcript),
    /// An agent view's conversation — its blocks, its input, and the agent behind it.
    Agent(crate::agent::Conversation),
}

impl Generated {
    pub fn patch(&self) -> Option<&crate::patch::GeneratedPatch> {
        match self {
            Generated::Patch(p) => Some(p),
            Generated::Shell(_) | Generated::Agent(_) => None,
        }
    }

    pub fn transcript(&self) -> Option<&crate::shell::Transcript> {
        match self {
            Generated::Shell(t) => Some(t),
            Generated::Patch(_) | Generated::Agent(_) => None,
        }
    }

    pub fn transcript_mut(&mut self) -> Option<&mut crate::shell::Transcript> {
        match self {
            Generated::Shell(t) => Some(t),
            Generated::Patch(_) | Generated::Agent(_) => None,
        }
    }

    pub fn conversation(&self) -> Option<&crate::agent::Conversation> {
        match self {
            Generated::Agent(c) => Some(c),
            Generated::Patch(_) | Generated::Shell(_) => None,
        }
    }

    pub fn conversation_mut(&mut self) -> Option<&mut crate::agent::Conversation> {
        match self {
            Generated::Agent(c) => Some(c),
            Generated::Patch(_) | Generated::Shell(_) => None,
        }
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
    /// Per-line layout facts derived from the text — see [`LayoutCache`]. Interior mutability
    /// because a render holds the state immutably and the cache fills on first read. A `Mutex`
    /// rather than a `RefCell` only so the state stays `Sync` — handlers hold `&ServerState` across
    /// awaits — and it is never contended: the state is behind one lock already.
    layout: std::sync::Mutex<LayoutCache>,
    /// Decorations and structure computed once when the content was *generated*, for documents no
    /// grammar spans — the commit patch behind `git/show`, a shell's transcript. See [`Generated`].
    pub generated: Option<Generated>,
    /// This document is a **field of a view** rather than a document of the user's: a shell's
    /// input line. It is never listed in a picker, never written to the session file, never backed
    /// up, never attached to a language server, and — see [`Self::saved_revision`] — never dirty.
    ///
    /// One flag consulted by the places that enumerate the user's work, rather than an exclusion
    /// remembered at each of them: an internal document that leaked into the buffers picker would
    /// be a row nobody can open, and one that leaked into the dirty aggregate would put a modified
    /// marker on a shell for the crime of having a half-typed command in it.
    pub internal: bool,
    /// How this document was last **shown**, when it is markdown: read as the rendered document
    /// (`true`) or edited as source (`false`). Presentation memory, not content — the one thing on
    /// a document that is not derived from its text — kept here because it has the document's
    /// lifetime exactly: the seed of every client's first landing while the file is open
    /// ([`ServerState::land_read`]), written by every toggle, recorded by the session, and gone
    /// with the last buffer. `None` until someone has looked at it through a client.
    pub read_last: Option<bool>,
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
    /// A client is holding the running undo group open (`element/undo_group`): while set, every
    /// edit joins `active_group` whatever its kind or timing, so a replayed gesture lands as one
    /// undo step. Document-level on purpose — one hold, not one per client — because the group
    /// itself is one per document, and a second client's edit landing mid-bracket has nowhere else
    /// to go anyway. Released by the closing bracket or by the holder's disconnect.
    undo_group_held_by: Option<ClientId>,
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
    /// A write the **agent** made (`fs/write_text_file`). Its own tag so one agent write is one
    /// undo step: it must never coalesce with the user's own typing burst, or a single `u` would
    /// take back some of each and the user could not cleanly reject what the agent did.
    Agent,
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

/// Lines an edit replaced: `removed` lines starting at `at` became `inserted` lines. What a
/// per-line cache needs to splice itself rather than start over. Several edits merge into one:
/// `at..at+removed` is then a range of the text *before* any of them and `at..at+inserted` the
/// same region of the text after all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSplice {
    pub at: u32,
    pub removed: u32,
    pub inserted: u32,
}

impl LineSplice {
    /// Fold a later edit — given in the coordinates of the text as it was when that edit ran —
    /// into this one, so the pair describes one region of the original text replaced by one
    /// region of the current text.
    fn merge(self, next: LineSplice) -> LineSplice {
        let lo = self.at.min(next.at);
        // The union's end in current coordinates, and the same line in the original text: lines
        // below the region so far replaced sit `inserted - removed` further along now.
        let cur_hi = (self.at + self.inserted).max(next.at + next.removed);
        let old_hi = cur_hi - self.inserted + self.removed;
        LineSplice {
            at: lo,
            removed: old_hi - lo,
            inserted: (cur_hi - lo) + next.inserted - next.removed,
        }
    }
}

/// Per-line layout facts about a document's text, kept with the text so a render reads them
/// instead of re-deriving them: how many rows each line wraps to at a given width, and the widest
/// line. Both used to be recomputed over the whole document on every scroll step — every line of
/// a view wrapped again each time, which on a working-changes view of this repository is thirty
/// thousand lines per keystroke.
///
/// Maintained across edits by splicing the lines an edit touched ([`LineSplice`]) and thrown away
/// on a wholesale replacement (undo, reload), where no single splice describes what moved. Filled
/// lazily, on the first read after a change, so a document nobody wraps costs nothing.
#[derive(Default)]
pub struct LayoutCache {
    /// Wrapped-row count per line, for the last two wrap geometries asked about. Two, not one: one
    /// document can be on screen in two clients at different widths, and a single slot would
    /// recompute the whole document on every alternating render.
    wrapped: Vec<(WrapKey, std::sync::Arc<Vec<u32>>)>,
    /// `(tab_width, widest line in cols)`, for the no-wrap horizontal scroller.
    max_width: Option<(u32, u32)>,
    /// The edits applied to the text since the wrapped counts last matched it, merged into one.
    pending: Option<LineSplice>,
}

/// The inputs a line's wrapped row count depends on — [`crate::wrap::WrapGeometry`] without the
/// mode, which only says whether to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WrapKey {
    cols: u32,
    marker_width: u32,
    tab_width: u32,
}

impl LayoutCache {
    fn record(&mut self, splice: LineSplice) {
        self.pending = Some(match self.pending {
            Some(so_far) => so_far.merge(splice),
            None => splice,
        });
        self.max_width = None;
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Bring every cached count up to date with `text` by re-deriving the region the edits since
    /// replaced.
    fn settle(&mut self, text: &ropey::Rope) {
        let Some(splice) = self.pending.take() else {
            return;
        };
        for (key, rows) in &mut self.wrapped {
            let rows = std::sync::Arc::make_mut(rows);
            let at = (splice.at as usize).min(rows.len());
            let end = (at + splice.removed as usize).min(rows.len());
            let fresh: Vec<u32> = (splice.at..splice.at + splice.inserted)
                .map(|line| wrapped_row_count(text, line, *key))
                .collect();
            rows.splice(at..end, fresh);
        }
    }
}

/// How many rows line `line` of `text` wraps to at `key`.
fn wrapped_row_count(text: &ropey::Rope, line: u32, key: WrapKey) -> u32 {
    if line as usize >= text.len_lines() {
        return 1;
    }
    let mut s: String = text.line(line as usize).chunks().collect();
    if s.ends_with('\n') {
        s.pop();
    }
    crate::wrap::compute_rows(&s, key.cols, key.marker_width, key.tab_width).len() as u32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl Document {
    /// Load a document from disk. Detects line endings, normalizes to LF in-memory.
    ///
    /// `force_defer` skips the inline parse whatever the size, as it does for
    /// [`Self::virtual_content`]: a working-changes view binds every changed file on the tree, and
    /// three hundred affordable parses are not affordable together.
    pub fn load_from_file(
        id: DocumentId,
        canonical: PathBuf,
        force_defer: bool,
    ) -> std::io::Result<Self> {
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
            undo_group_held_by: None,
            externally_modified: false,
            externally_deleted: false,
            backed_up_revision: None,
            disk_blob: None,
            last_shift: None,
            layout: Default::default(),
            internal: false,
            read_last: None,
            virtual_source: None,
        })
    }

    /// Empty document with a target file path attached but no file on disk yet. Used by
    /// `view/open` with `create_if_missing: true` — the file is created by `save_to_disk`
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
            layout: Default::default(),
            internal: false,
            read_last: None,
            generated: None,
            indent_style,
            saved_revision: Some(0),
            next_revision_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_group: None,
            undo_group_held_by: None,
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
        generated: Option<Generated>,
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
            undo_group_held_by: None,
            externally_modified: false,
            externally_deleted: false,
            backed_up_revision: None,
            disk_blob: None,
            last_shift: None,
            layout: Default::default(),
            internal: false,
            read_last: None,
        }
    }

    /// Whether this document refuses edits, saves and reloads — true exactly for the virtual ones.
    pub fn read_only(&self) -> bool {
        self.virtual_source.is_some()
    }

    /// The generated patch this document holds, or `None` for one generated some other way.
    ///
    /// For the readers whose question genuinely *is* "is this a patch?" — the `is_patch` flag, the
    /// patch index, the follow-the-line lookup. Everything that has to behave differently per kind
    /// matches [`Generated`] exhaustively instead, which is what stops a shell being treated as a
    /// patch with an empty index.
    pub fn patch(&self) -> Option<&crate::patch::GeneratedPatch> {
        self.generated.as_ref()?.patch()
    }

    /// The shell transcript this document holds, if it is one.
    pub fn transcript(&self) -> Option<&crate::shell::Transcript> {
        self.generated.as_ref()?.transcript()
    }

    /// The agent conversation this document holds, if it is one.
    pub fn conversation(&self) -> Option<&crate::agent::Conversation> {
        self.generated.as_ref()?.conversation()
    }

    /// An **internal** document: a field of a view rather than a document of the user's — a
    /// shell's input line, and nothing else so far. Editable and ordinary in every other respect,
    /// so the whole edit surface works in it unchanged; see [`Self::internal`] for what it is
    /// excluded from.
    pub fn field(id: DocumentId, language: Option<String>) -> Self {
        Document {
            internal: true,
            read_last: None,
            ..Document::scratch(id, language)
        }
    }

    /// One conversation block's document: **virtual and internal** at once.
    ///
    /// Virtual because a block is a record of what happened and must refuse every edit there is
    /// — the same construction that makes a patch and a shell transcript read-only. Internal
    /// because it is a part of a view rather than a document of the user's: never listed in a
    /// picker, never backed up, never written to the session file, never counted in a view's
    /// dirty aggregate. A shell's input is internal but editable; a block is the other pairing,
    /// and both are wanted.
    ///
    /// `language` is the block's own: an agent's prose is Markdown, a diff is a patch, a tool
    /// call's output is nothing in particular. That per-block choice is only possible because
    /// each block has a document of its own, and is one of the reasons it does.
    pub fn block(id: DocumentId, source: VirtualSource, language: Option<String>) -> Self {
        Document {
            internal: true,
            read_last: None,
            ..Document::virtual_content(id, source, String::new(), language, None, false)
        }
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
            layout: Default::default(),
            internal: false,
            read_last: None,
            generated: None,
            indent_style,
            // Treat empty scratch as "clean"; first edit makes it dirty.
            saved_revision: Some(0),
            next_revision_id: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_group: None,
            undo_group_held_by: None,
            externally_modified: false,
            externally_deleted: false,
            backed_up_revision: None,
            disk_blob: None,
            virtual_source: None,
        }
    }

    fn layout_cache(&self) -> std::sync::MutexGuard<'_, LayoutCache> {
        self.layout.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn layout_cache_mut(&mut self) -> &mut LayoutCache {
        self.layout.get_mut().unwrap_or_else(|e| e.into_inner())
    }

    /// How many rows each line wraps to at `geom`, one count per line of the document, shared
    /// (an `Arc`) with the cache so a render can hold it while it walks. See [`LayoutCache`].
    ///
    /// Asked under soft wrap only — under no wrap every line is one row and nobody needs a table
    /// to say so.
    pub fn wrapped_rows(&self, geom: crate::wrap::WrapGeometry) -> std::sync::Arc<Vec<u32>> {
        let key = WrapKey {
            cols: geom.cols,
            marker_width: geom.marker_width,
            tab_width: geom.tab_width,
        };
        let mut cache = self.layout_cache();
        cache.settle(&self.text);
        if let Some((_, rows)) = cache.wrapped.iter().find(|(k, _)| *k == key) {
            return rows.clone();
        }
        let rows = std::sync::Arc::new(
            (0..self.line_count())
                .map(|line| wrapped_row_count(&self.text, line, key))
                .collect::<Vec<u32>>(),
        );
        if cache.wrapped.len() >= 2 {
            cache.wrapped.remove(0);
        }
        cache.wrapped.push((key, rows.clone()));
        rows
    }

    /// Display width (cols) of the widest line — sizes a client's native horizontal scroller under
    /// no wrap. Cached until the next edit.
    pub fn max_line_width(&self, tab_width: u32) -> u32 {
        let mut cache = self.layout_cache();
        if let Some((tab, width)) = cache.max_width {
            if tab == tab_width {
                return width;
            }
        }
        let mut max = 0u32;
        for line in self.text.lines() {
            let mut col = 0u32;
            for c in line.chars() {
                if c == '\n' {
                    break;
                }
                col += crate::wrap::char_display_width(c, col, tab_width);
            }
            max = max.max(col);
        }
        cache.max_width = Some((tab_width, max));
        max
    }

    /// Lines that hold content — the line count without the phantom empty line ropey reports
    /// after a final newline.
    ///
    /// What a shell's runs partition, and so where the next run starts. The phantom is deliberately
    /// outside every run's element: giving it to the last run would make that element shrink by one
    /// the moment the next run began, and "a finished run's extent never moves" is the property the
    /// whole transcript rests on.
    pub fn content_lines(&self) -> u32 {
        let chars = self.text.len_chars();
        if chars == 0 {
            0
        } else if self.text.char(chars - 1) == '\n' {
            self.line_count().saturating_sub(1)
        } else {
            self.line_count()
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
    ///
    /// An **internal** document answers with its current revision, so it reads as clean however
    /// much has been typed into it. Here rather than at each of the places that ask, because the
    /// question is asked in four of them — the status bar's dot, the dirty aggregate, the session
    /// file, the backup flush — and "the shell input is not unsaved work" has to be one answer.
    pub fn saved_revision(&self) -> Revision {
        if self.internal {
            return self.revision;
        }
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

        // Decide whether to start a new undo group. A held group (`element/undo_group`) takes
        // every edit regardless of kind or timing; otherwise a burst is broken by a pause or a
        // change of kind.
        let start_new_group = match &self.active_group {
            None => true,
            Some(_) if self.undo_group_held_by.is_some() => false,
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
        let at = self.text.char_to_line(start_char) as u32;
        let removed_breaks =
            (self.text.char_to_line(end_char) - self.text.char_to_line(start_char)) as u32;
        let added_breaks = insert_text.matches('\n').count() as u32;
        let shift = LineShift {
            at,
            delta: added_breaks as i32 - removed_breaks as i32,
        };
        if start_char < end_char {
            self.text.remove(start_char..end_char);
        }
        if !insert_text.is_empty() {
            self.text.insert(start_char, insert_text);
        }
        self.last_shift = (shift.delta != 0).then_some(shift);
        // The lines the edit touched: one more than the breaks on each side, since a break's two
        // neighbours are both affected.
        self.layout_cache_mut().record(LineSplice {
            at,
            removed: removed_breaks + 1,
            inserted: added_breaks + 1,
        });
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

    /// Open the bracket of `element/undo_group`: close whatever group is running, so the next
    /// edit starts a fresh undo entry rather than coalescing with typing that happened just before,
    /// and hold that entry open for every edit until [`Self::close_undo_group`]. Opening twice is
    /// the same as opening once.
    ///
    /// Not behind [`Editable`]: a bracket is not an edit, and a read-only document accepts it —
    /// the edits inside are what get refused.
    pub fn open_undo_group(&mut self, client_id: ClientId) {
        self.active_group = None;
        self.undo_group_held_by = Some(client_id);
    }

    /// Close the bracket: release the hold and end the running group, so the edit after the
    /// bracket starts its own entry. Harmless without a matching open.
    pub fn close_undo_group(&mut self) {
        self.active_group = None;
        self.undo_group_held_by = None;
    }

    /// Which client holds this document's undo group open, if any.
    pub fn undo_group_holder(&self) -> Option<ClientId> {
        self.undo_group_held_by
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
        self.layout_cache_mut().clear();
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
        self.layout_cache_mut().clear();
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
        self.layout_cache_mut().clear();
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
        self.layout_cache_mut().clear();
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
        // An internal document has nowhere to save to and nothing worth rescuing: a half-typed
        // shell command is not unsaved work. See [`Self::internal`].
        self.dirty = !self.internal && self.saved_revision != Some(self.revision);
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
    pub(in crate::state) fn replace_generated(&mut self, text: &str, generated: Option<Generated>) {
        self.text = ropey::Rope::from_str(text);
        self.layout_cache_mut().clear();
        self.generated = generated;
        self.revision += 1;
        self.saved_revision = Some(self.revision);
        self.recompute_dirty();
    }

    /// Rewrite a generated document's **tail**: replace everything from char `from` to the end
    /// with `text`.
    ///
    /// The mutation a stream of output needs, and the reason it is not
    /// [`Self::replace_generated`]: a command producing a hundred thousand lines would otherwise
    /// re-rope and re-wrap the whole document twenty times a second, and a carriage-return progress
    /// bar would do it for every frame it draws. `from` is the start of the line being appended to
    /// — the earliest point a `\r` can have changed — so the work is proportional to what actually
    /// moved.
    ///
    /// Four properties, each of which is a bug if it is missing:
    /// - it **splices** the wrap cache rather than clearing it, so the lines above the tail keep
    ///   their measured heights and a long transcript doesn't re-wrap per flush;
    /// - it bumps `revision`, because viewport pushes are revision-guarded and would otherwise be
    ///   dropped as stale;
    /// - it moves `saved_revision` with it, because a transcript is read-only and has nothing to
    ///   save — leaving the two apart would show a modified marker for output the user never typed;
    /// - it **never touches undo**. There is nothing to undo: the text is a record of what
    ///   happened, and an undo stack over it would grow without bound while a build runs.
    ///
    /// Restricted to this module, and reachable only through [`ServerState::extend_transcript`],
    /// which rebuilds the view's layout from the same transcript in the same breath — a tail write
    /// that lands without one leaves the last run's element the length it was before the output
    /// arrived. It is deliberately *not* on [`Editable`]: this is generation, not editing, and a
    /// read-only document must keep refusing every edit there is.
    pub(in crate::state) fn write_tail(&mut self, from: usize, text: &str) {
        let from = from.min(self.text.len_chars());
        let first_line = self.text.char_to_line(from) as u32;
        let removed = self.line_count() - first_line;
        self.text.remove(from..self.text.len_chars());
        self.text.insert(from, text);
        let inserted = self.line_count() - first_line;
        self.layout_cache_mut().record(LineSplice {
            at: first_line,
            removed,
            inserted,
        });
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

/// One client's presentation of one view: its geometry, its scroll, and which element holds the
/// cursor. What the view is *composed of* lives on the [`View`] it references — a viewport used to
/// carry a copy of the elements, and the copy was a second thing to shift on every edit, to rebuild
/// after every stage and to drop on every close.
///
/// A view holds its editors inside one scroller, so there is one scroll position and N windows into
/// N buffers — not N viewports.
#[derive(Clone)]
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

    /// Width every element is wrapped to. One width for the whole view today; a side-by-side diff
    /// is where that stops being true, and it would become per element then.
    pub cols: u32,
    /// Cols the client reserves at the start of each continuation row for its wrap marker.
    pub continuation_marker_width: u32,
    pub rows: u32,
    pub overscan_rows: u32,
    pub wrap: WrapMode,
    pub tab_width: u32,
    /// Inline diff view: when on, rendered windows interleave phantom baseline rows from the
    /// buffer's Git hunks and the hunks are recomputed on every edit. Per-viewport so two views of
    /// the same buffer can differ. Toggled by `git/set_diff_view`.
    pub diff_view: bool,
    /// The slice of each element's buffer this client has loaded, by element index — `None` for
    /// an element with nothing loaded. What a push re-renders, and what an edit shifts.
    ///
    /// The client decides these: it lays the view out from the tree and asks for the elements its
    /// viewport reaches, by row within each (`view/window`). The server used to own a scroll
    /// position in *view lines* and derive one contiguous range from it, which presumed it could
    /// sum the heights of everything above a point — true only while every element was monospace
    /// text it had wrapped itself.
    pub loaded: Vec<Option<std::ops::Range<u32>>>,
    /// Where the client last said its viewport's top was, as content — what a reopen restores.
    pub anchor: ScrollPosition,

    /// Which element holds the live cursor.
    ///
    /// Per-view, because a view has one cursor and it is in exactly one element. It stays 0 while
    /// every element of a view windows the same buffer — which is why storing it earlier would have
    /// been inert — and becomes load-bearing the moment they don't: it is what decides which
    /// *buffer* an edit, a search, a motion or an undo acts on. An index into the view's elements.
    pub focused: aether_protocol::viewport::FieldId,

    /// The collapsible elements this viewport has **opened up**, by the buffer each windows.
    ///
    /// The expanded set rather than the collapsed one, so that folded-by-default costs nothing:
    /// an empty set is every tool call shut, a block the agent adds mid-turn arrives shut without
    /// anyone deciding it should, and there is no moment where a new element's default has to be
    /// written down.
    ///
    /// Keyed by **buffer**, not by element index, because a block *is* a document: its buffer id
    /// is stable for the life of the conversation while its index is positional — the input
    /// element's index moves every time a block is appended. An index would have quietly folded
    /// the wrong block as the turn went on.
    pub expanded: std::collections::HashSet<BufferId>,
}

/// What a view is composed of: its elements, in order, each windowing some buffer.
///
/// The entity a client opens, switches between and closes. It is presented by one buffer — its
/// own document — and for an ordinary file that buffer is all it is: one element over the whole
/// of it. A patch is a tree of elements over the files it describes, built by its driver. Which
/// buffer presents a view is a fact recorded here and asked of the table
/// ([`ServerState::presenting_buffer`]), never derived from the view's id.
#[derive(Debug, Clone)]
pub struct View {
    /// The buffer presenting this view: its own document. For an ordinary file, the file; for a
    /// patch, the generated text, which no element need window.
    pub presenting: BufferId,
    pub elements: Vec<ElementBinding>,
    /// When this view was last opened or subscribed to, on [`ServerState::view_clock`]; `0` for
    /// one nothing has shown yet. Orders a buffer's views by recency.
    pub last_used: u64,
    /// A preview: closes itself once no viewport shows it ([`ServerState::close_orphaned_transients`]),
    /// and its buffer goes with its last view. Set at creation when the open asked for it — a
    /// picker or goto-definition navigation — and cleared, "promoted", by an edit made in it, a
    /// save, a user-initiated reload, or `Space k`. Never set again after creation except by
    /// `Space k`, and then never for a composed view: see
    /// [`ServerState::set_view_transient`], the one place it changes.
    pub transient: bool,
}

impl View {
    /// The view an ordinary buffer presents: one element over the whole of it, following the
    /// buffer's length as it changes.
    pub fn whole(buffer_id: BufferId) -> Self {
        View {
            presenting: buffer_id,
            last_used: 0,
            transient: false,
            elements: vec![ElementBinding {
                buffer_id,
                lines: ElementLines::Whole,
                decorations: None,
                chrome_before: Default::default(),
                chrome_above: std::sync::Arc::new(Vec::new()),
                laid_out_by: LayoutOwner::Server,
                prose: false,
                role: aether_protocol::ui::ElementRole::Field,
                edges: aether_protocol::ui::Edges::NONE,
                box_group: None,
                collapsible: false,
                title: Default::default(),
                band: aether_protocol::ui::Band::None,
            }],
        }
    }

    /// Whether this view is **composed** — built by a driver over other buffers, or around its
    /// own — rather than a document's own view: one element over the whole of its buffer, with
    /// nothing drawn around it. Every clause of the test counts: a fresh shell is also a single
    /// whole-buffer element, and what makes it composed is that the element is an *input*
    /// standing in a box of its own.
    ///
    /// Two questions ride on it. A document's own view is the one kind a client can **read** when
    /// the file is markdown — reading is not a fact of the view: the same view goes out as prose
    /// to a client reading it and as lines to one editing it ([`ServerState::reads`]). And a
    /// document's own view is a **buffer** in the sense the buffers picker lists, and so one the
    /// user may keep or release ([`ServerState::set_view_transient`]); a composed view — a
    /// commit's patch, the working changes, a shell, a conversation — is the presentation of
    /// something reached by a command, and what you read in it lives in the buffers its elements
    /// window.
    pub fn is_composed(&self) -> bool {
        !matches!(
            &self.elements[..],
            [only]
                if only.lines == ElementLines::Whole
                    && only.chrome_above.is_empty()
                    && only.title.is_empty()
                    && only.box_group.is_none()
                    && only.role.is_field()
                    && !only.prose
        )
    }

    /// The view a driver built, bound against the buffer presenting it — the point at which an
    /// `OwnDocument` extent is told which buffer it is a slice of.
    pub fn from_layout(view_buffer: BufferId, layout: Vec<ElementLayout>) -> Self {
        View {
            presenting: view_buffer,
            last_used: 0,
            transient: false,
            elements: layout.iter().map(|l| l.bind(view_buffer)).collect(),
        }
    }

    /// A shell: one element per run, each introduced by its header, and the **input** last.
    ///
    /// The input is an element like any other — it windows an ordinary editable document, which is
    /// what makes typing into it work with no new edit path — and is marked
    /// [`aether_protocol::ui::ElementRole::Input`] so the client can find it without being told
    /// what kind of view it is looking at.
    ///
    /// A shell with no runs is a real state: the view is then just the input, which is what
    /// `Space Alt-t` on a fresh shell shows.
    /// Which element is a shell's input, if this view has one. By role, never by position: the
    /// input is the last element, and "last" is a different number after every run.
    pub fn input_element(&self) -> Option<aether_protocol::ui::FieldId> {
        self.elements
            .iter()
            .position(|e| e.role.is_input())
            .map(|i| i as aether_protocol::ui::FieldId)
    }

    pub fn over_transcript(view_buffer: BufferId, t: &crate::shell::Transcript) -> Self {
        use aether_protocol::ui::{Band, Edges, Sides};
        // Every run is a box of its own, closed on all four sides, and so is the input: a separate
        // box per run rather than one ruled list, because each one is *named* — its directory, and
        // once it is over its outcome and how long it took — and a name belongs to one box. Runs
        // are read one at a time; a shared rail invited them to be read as one document.
        let boxed = Edges {
            border: Sides::all(1),
            padding: Sides::ZERO,
            collapse: false,
        };
        // One blank row of ground between boxes: a chrome row with nothing on it, standing before
        // the box rather than inside it.
        let gap =
            || std::sync::Arc::new(vec![aether_protocol::viewport::Element::chrome(Vec::new())]);
        let mut elements: Vec<ElementBinding> = t
            .runs
            .iter()
            .enumerate()
            .map(|(i, run)| ElementBinding {
                buffer_id: view_buffer,
                lines: ElementLines::Range {
                    start: run.start_line,
                    end_exclusive: run.end_line_exclusive,
                },
                decorations: None,
                // A blank row of ground before every box but the first.
                chrome_before: if i == 0 { Default::default() } else { gap() },
                // Inside the box, the command alone; the directory and the outcome are the box's
                // own name, on the border above it.
                chrome_above: std::sync::Arc::new(crate::shell::command_row(run)),
                laid_out_by: LayoutOwner::Server,
                prose: false,
                role: aether_protocol::ui::ElementRole::Field,
                edges: boxed,
                box_group: Some(i as u32),
                collapsible: false,
                title: std::sync::Arc::new(crate::shell::run_title(run)),
                band: Band::Chrome,
            })
            .collect();
        elements.push(ElementBinding {
            buffer_id: t.input,
            lines: ElementLines::Whole,
            decorations: None,
            chrome_before: if t.runs.is_empty() {
                Default::default()
            } else {
                gap()
            },
            // Nothing above the line you type: the box says where you are, and the box is enough.
            chrome_above: std::sync::Arc::new(Vec::new()),
            laid_out_by: LayoutOwner::Server,
            prose: false,
            role: aether_protocol::ui::ElementRole::Input,
            edges: boxed,
            box_group: Some(t.runs.len() as u32),
            collapsible: false,
            title: std::sync::Arc::new(crate::shell::input_title(&t.cwd)),
            band: Band::Chrome,
        });
        View {
            presenting: view_buffer,
            last_used: 0,
            transient: false,
            elements,
        }
    }

    /// One element per block, each over the block's **own** document, and the input last.
    ///
    /// The shape [`Self::over_transcript`] has, with one difference that is the whole reason the
    /// agent view is built this way: a shell's runs are line ranges into one transcript, so only
    /// the last one can grow; a conversation's blocks are separate documents, so any of them can.
    /// ACP updates a tool call by its id long after later blocks exist, and this is what lets that
    /// be an append rather than a splice.
    pub fn over_conversation(
        view_buffer: BufferId,
        c: &crate::agent::Conversation,
        content_lines: impl Fn(BufferId) -> u32,
    ) -> Self {
        use aether_protocol::ui::{Band, Edges, Sides};
        // **Prose is not boxed.** What you typed and what the agent said back are the conversation
        // itself, and a box around each turn makes a reply look like a machine's output rather
        // than like something written to be read. What stays boxed is the machinery — a tool call,
        // a diff, the plan, the thinking — because each of those *is* a named thing that happened,
        // and a name belongs to one box. The blank row between blocks does the separating either
        // way.
        let boxed = Edges {
            border: Sides::all(1),
            padding: Sides::ZERO,
            collapse: false,
        };
        let gap =
            || std::sync::Arc::new(vec![aether_protocol::viewport::Element::chrome(Vec::new())]);
        let mut elements: Vec<ElementBinding> = c
            .blocks
            .iter()
            .enumerate()
            .map(|(i, block)| {
                let bare = crate::agent::is_prose(&block.kind);
                // The agent's reply is prose: the server sends the markdown *parse* and the shell
                // renders it as type, which is also why the client owns its height.
                let rendered = crate::agent::is_rendered(&block.kind);
                ElementBinding {
                    buffer_id: block.buffer,
                    // The block's **content** lines, not its buffer's. A block's text is a record
                    // of something that happened and almost always ends in a newline; bound
                    // `Whole` that terminator becomes a line of its own and every block trails a
                    // blank row. A file's trailing empty line is a real place to put the cursor,
                    // which is why the layout counts it in general — a block has no cursor to put
                    // there.
                    lines: ElementLines::Range {
                        start: 0,
                        end_exclusive: content_lines(block.buffer),
                    },
                    decorations: None,
                    chrome_before: if i == 0 { Default::default() } else { gap() },
                    // Inside a box: a tool call's permission question. Above bare prose: who is
                    // speaking, when that is not obvious — the agent's own reply wears nothing,
                    // because it is the thing you are reading.
                    chrome_above: std::sync::Arc::new(if bare {
                        crate::agent::speaker_row(block)
                    } else {
                        crate::agent::permission_row(block)
                    }),
                    laid_out_by: if rendered {
                        LayoutOwner::Client
                    } else {
                        LayoutOwner::Server
                    },
                    prose: rendered,
                    role: aether_protocol::ui::ElementRole::Field,
                    edges: if bare { Edges::NONE } else { boxed },
                    box_group: (!bare).then_some(i as u32),
                    // **The machinery folds; the conversation does not.** The same line `bare`
                    // draws for the box, and for the same reason: what is boxed is a named thing
                    // that happened, so its title row still says what it was when it is shut. A
                    // reply has no title — it *is* the text — so folded it would be a rule with
                    // nothing to account for it.
                    //
                    // Except while it is asking: see [`crate::agent::awaits_permission`].
                    collapsible: !bare && !crate::agent::awaits_permission(&block.kind),
                    title: std::sync::Arc::new(if bare {
                        Vec::new()
                    } else {
                        crate::agent::block_title(block)
                    }),
                    band: if bare { Band::None } else { Band::Chrome },
                }
            })
            .collect();
        elements.push(ElementBinding {
            buffer_id: c.input,
            // The input keeps `Whole`, unlike the blocks above: it is a document you type in, so
            // its trailing empty line is a place the cursor can be, exactly as in a file.
            lines: ElementLines::Whole,
            decorations: None,
            chrome_before: if c.blocks.is_empty() {
                Default::default()
            } else {
                gap()
            },
            chrome_above: std::sync::Arc::new(Vec::new()),
            laid_out_by: LayoutOwner::Server,
            prose: false,
            role: aether_protocol::ui::ElementRole::Input,
            // Bare, like the prose around it. The box and its label said which agent, in which
            // directory, doing what, and which key stops it — all of which the status bar already
            // says (`Session::work_indicator`) or the tool call's own box does ("needs
            // permission"). What is left is the line you are typing on, and the cursor is in it.
            edges: Edges::NONE,
            box_group: None,
            collapsible: false,
            title: Default::default(),
            band: Band::None,
        });
        View {
            presenting: view_buffer,
            last_used: 0,
            transient: false,
            elements,
        }
    }

    /// The regions a generated patch divides into on its own — one per run of lines with no chrome
    /// between them — for a generated document no driver built a view over.
    pub fn over_generated(view_buffer: BufferId, g: &crate::patch::GeneratedPatch) -> Self {
        View {
            presenting: view_buffer,
            last_used: 0,
            transient: false,
            elements: g
                .decorations
                .elements
                .iter()
                .map(|e| ElementBinding {
                    buffer_id: view_buffer,
                    lines: ElementLines::Range {
                        start: e.start_line,
                        end_exclusive: e.end_line,
                    },
                    decorations: None,
                    chrome_before: Default::default(),
                    chrome_above: std::sync::Arc::new(
                        g.decorations
                            .chrome
                            .get(e.start_line as usize)
                            .cloned()
                            .unwrap_or_default(),
                    ),
                    laid_out_by: LayoutOwner::Server,
                    prose: false,
                    role: aether_protocol::ui::ElementRole::Field,
                    edges: aether_protocol::ui::Edges::NONE,
                    box_group: None,
                    collapsible: false,
                    title: Default::default(),
                    band: aether_protocol::ui::Band::None,
                })
                .collect(),
        }
    }

    /// Whether any element windows `buffer_id` — "is this buffer on screen in this view?".
    pub fn binds(&self, buffer_id: BufferId) -> bool {
        self.elements.iter().any(|e| e.buffer_id == buffer_id)
    }
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
}

/// How a view divides into elements: each one's extent, the chrome introducing it, and — once a
/// driver builds them — what the view says about its lines.
pub struct ElementLayout {
    pub extent: ElementExtent,
    pub chrome_above: std::sync::Arc<Vec<aether_protocol::viewport::Element>>,
    /// Chrome standing **before** the element's box, outside it — the gap between one box and
    /// the next. `chrome_above` goes inside a box the element opens; this never does.
    pub chrome_before: std::sync::Arc<Vec<aether_protocol::viewport::Element>>,
    pub decorations: Option<std::sync::Arc<ElementDecorations>>,
    /// The accumulated inset of the box this element sits in — see [`ElementBinding::edges`].
    pub edges: aether_protocol::ui::Edges,
    /// Which box this element belongs to — see [`ElementBinding::box_group`].
    pub box_group: Option<u32>,
    /// What the box this element opens says on its top border — see [`ElementBinding::title`].
    pub title: std::sync::Arc<Vec<aether_protocol::viewport::Element>>,
    pub band: aether_protocol::ui::Band,
    /// See [`ElementBinding::role`].
    pub role: aether_protocol::ui::ElementRole,
}

impl ElementLayout {
    /// Bind this element against the view it belongs to — the point at which an `OwnDocument`
    /// extent is told which buffer it is a slice of.
    pub fn bind(&self, view_buffer: BufferId) -> ElementBinding {
        let lines = self.extent.lines();
        ElementBinding {
            buffer_id: self.extent.buffer(view_buffer),
            lines: ElementLines::Range {
                start: lines.start,
                end_exclusive: lines.end,
            },
            decorations: self.decorations.clone(),
            chrome_above: self.chrome_above.clone(),
            chrome_before: self.chrome_before.clone(),
            laid_out_by: LayoutOwner::Server,
            prose: false,
            role: self.role,
            edges: self.edges,
            box_group: self.box_group,
            collapsible: false,
            title: self.title.clone(),
            band: self.band,
        }
    }
}

/// Which lines of its buffer an element windows.
///
/// `Whole` rather than `0..line_count` frozen at some moment: an ordinary buffer's one element has
/// to follow the buffer through every edit, undo and reload, and a stored range would have to be
/// told about each of them. A *range* is what a driver hands over — a hunk's lines — and follows
/// edits through [`ServerState::shift_element_extents`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementLines {
    /// The whole buffer, however long it is now.
    Whole,
    /// `start..end_exclusive` of the buffer.
    Range { start: u32, end_exclusive: u32 },
}

impl ElementLines {
    /// Slide or stretch a range for an edit that changed a line count; the whole buffer needs no
    /// telling. See [`ServerState::shift_element_extents`] for the three cases.
    fn shift(&mut self, at: u32, delta: i32) {
        if let ElementLines::Range {
            start,
            end_exclusive,
        } = self
        {
            let mut range = *start..*end_exclusive;
            shift_range(&mut range, at, delta);
            *start = range.start;
            *end_exclusive = range.end;
        }
    }
}

/// The three cases an edit that changed a line count has for a range of lines: inside it, the
/// range grows or shrinks; above it, the whole range slides; below it, nothing moves.
fn shift_range(range: &mut std::ops::Range<u32>, at: u32, delta: i32) {
    if at < range.start {
        range.start = range.start.saturating_add_signed(delta);
        range.end = range.end.saturating_add_signed(delta);
    } else if at < range.end {
        range.end = range.end.saturating_add_signed(delta);
    }
}

/// One editor element of a view: a window onto a buffer.
///
/// **Identity, not geometry.** Everything here is stable while the view is open — which buffer the
/// element shows and which slice of it — so an [`aether_protocol::viewport::FieldId`] indexing
/// into a view's elements keeps naming the same region across a scroll. Where a *viewport* is
/// scrolled to, and how wide it is, live on the [`Viewport`].
#[derive(Debug, Clone)]
pub struct ElementBinding {
    pub buffer_id: BufferId,
    /// The element's extent in its buffer.
    pub lines: ElementLines,
    /// What the view says about these lines, if it has an opinion. `Arc` because a rebuild clones
    /// what it keeps and this is the one field with any size to it.
    pub decorations: Option<std::sync::Arc<ElementDecorations>>,
    /// Chrome drawn above this element — a file separator, a hunk heading — as sibling nodes.
    ///
    /// Held by the *element* rather than looked up by line in a generated document, because that
    /// lookup was the last thing tying a view's structure to a document's line space. An element
    /// whose content comes from a real file has no line in the patch to anchor its heading to.
    pub chrome_above: std::sync::Arc<Vec<aether_protocol::viewport::Element>>,
    /// Chrome standing **before** the element's box, outside it — the gap between one box and
    /// the next. `chrome_above` goes inside a box the element opens; this never does.
    pub chrome_before: std::sync::Arc<Vec<aether_protocol::viewport::Element>>,
    /// Whose arithmetic the element's height is. A server-laid-out element is wrapped to the
    /// viewport and shipped a screen at a time; a client-laid-out one is sent unwrapped and whole,
    /// and the client measures it. Decided by the view's kind, never per viewport: two clients
    /// presenting one view see the same elements.
    pub laid_out_by: LayoutOwner,
    /// Whether the element's lines are **markdown to be rendered** rather than text to be shown.
    /// The window carries such an element as [`aether_protocol::viewport::Element::Prose`] — the
    /// parse, not the lines — and a shell renders it with real typography.
    ///
    /// Implies [`LayoutOwner::Client`]: proportional type can only be measured where it is drawn.
    /// The converse does not hold yet — the reading view is client-laid-out and still ships its
    /// source, because its focus and its edit-toggle resolve against the buffer.
    pub prose: bool,
    /// What the element is *for* — see [`aether_protocol::ui::ElementRole`]. `Field` for every
    /// element of an ordinary or composed view; `Input` for the line a shell's next command is
    /// typed into. Rides to the client on the window, so no shell has to re-derive it from the
    /// shape of the tree.
    pub role: aether_protocol::ui::ElementRole,
    /// The cells the box around this element spends on its own border and padding — accumulated
    /// over every container enclosing it, so it is the whole inset rather than one frame's share.
    ///
    /// Here rather than read back off the composed tree because the **wrap happens first**: an
    /// element inset by four columns must be wrapped to `cols - 4`, and `compose_tree` runs after
    /// every line has already been wrapped. The driver knows the boxes it is building, so the
    /// driver is what says this.
    ///
    /// `Edges::NONE` for every element outside a box.
    pub edges: aether_protocol::ui::Edges,
    /// Which box this element belongs to, if any — elements sharing a key, and consecutive, are
    /// wrapped in one container by `compose_tree`.
    ///
    /// A key rather than a nested layout because `FieldId` indexes a **flat** list of elements and
    /// the whole view addresses them that way. Consecutiveness is the driver's guarantee: a patch's
    /// file blocks are contiguous by construction, and nothing else builds boxes yet.
    pub box_group: Option<u32>,
    /// Whether this element may be **folded shut** — `view/set_expanded` refuses every element
    /// that says no.
    ///
    /// The driver's call, because folding only makes sense where the box already says what is
    /// inside it: an agent's tool call is named by its title row, so folded it still reads
    /// "✓ Running cargo test", while a reply folded to a rule would be a blank you could not
    /// account for. It is *not* the same question as "is this element boxed" — a patch's file
    /// blocks are boxed and nothing folds them yet — so it is a field rather than a derivation.
    pub collapsible: bool,
    /// What the box this element **opens** says on its top border: the run's directory and
    /// outcome, for a shell. Empty for every element that opens no box and for every box with
    /// nothing to say.
    ///
    /// Read from the element that opens a box and ignored on the rest of the run, exactly as
    /// `edges` and `band` are: only the opening element's is composed onto the box. A title costs
    /// the box no rows, so unlike `chrome_above` it changes nothing about the element's height.
    pub title: std::sync::Arc<Vec<aether_protocol::viewport::Element>>,
    /// What the box paints behind its own border and padding cells.
    pub band: aether_protocol::ui::Band,
}

impl ElementBinding {
    /// Whether the element windows no lines at all — a shell run that said nothing. Such an
    /// element is drawn (its box, its title, its chrome) but never stepped to: there is no line
    /// in it for a cursor to sit on.
    ///
    /// Deliberately asks about the element's **extent**, not about how many rows a viewport is
    /// currently showing of it: a folded element windows its lines as much as ever and *is*
    /// stepped to, which is the distinction `Viewport::expanded` rests on.
    pub fn is_empty(&self) -> bool {
        match self.lines {
            ElementLines::Whole => false,
            ElementLines::Range {
                start,
                end_exclusive,
            } => end_exclusive <= start,
        }
    }

    /// The element's lines, given how many the buffer has **now** — a whole-buffer element is as
    /// long as its buffer, a range is what it was told. Not yet clamped: that is
    /// [`ViewLayout::of`]'s job, and the one place it happens.
    pub fn lines_in(&self, doc_lines: u32) -> std::ops::Range<u32> {
        match self.lines {
            ElementLines::Whole => 0..doc_lines,
            ElementLines::Range {
                start,
                end_exclusive,
            } => start..end_exclusive,
        }
    }

    /// The first line the element windows.
    pub fn start_line(&self) -> u32 {
        match self.lines {
            ElementLines::Whole => 0,
            ElementLines::Range { start, .. } => start,
        }
    }
}

/// Each of a view's elements as a range of its buffer's lines that **exists right now**.
///
/// Built against the buffers' **live** line counts, so an extent that has gone stale (the diff said
/// a hunk was seven lines; the file has since been edited) is clamped at construction rather than
/// indexing past the end of a rope somewhere downstream. A short window is a far better answer than
/// a crash, and doing it once here beats doing it at each use — which is why [`BufferRange`] can
/// only be made here.
///
/// This used to also hold each element's place in a *view line* space, the concatenation of the
/// extents, and to convert between the two. That space is gone: content is addressed per element,
/// by row within it. See [`aether_protocol::coords`].
pub struct ViewLayout {
    ranges: Vec<BufferRange>,
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

    /// The part of `lines` inside this range, or `None` when nothing is — how a slice a client
    /// asked for, or one an edit shifted, is brought inside what the element has now.
    pub fn intersect(&self, lines: std::ops::Range<u32>) -> Option<BufferRange> {
        let start = lines.start.max(self.start);
        let end_exclusive = lines.end.min(self.end_exclusive);
        (start < end_exclusive).then_some(BufferRange {
            start,
            end_exclusive,
        })
    }

    /// The lines of this range before `line` — what sits above it in the element.
    pub fn before(&self, line: u32) -> BufferRange {
        BufferRange {
            start: self.start,
            end_exclusive: line.clamp(self.start, self.end_exclusive),
        }
    }
}

impl ViewLayout {
    /// Lay out `elements`. `doc_lines` gives a buffer's current line count — the clamp that keeps
    /// a stale extent from outliving the lines it described.
    pub fn of(elements: &[ElementBinding], mut doc_lines: impl FnMut(BufferId) -> u32) -> Self {
        let ranges = elements
            .iter()
            .map(|binding| {
                let available = doc_lines(binding.buffer_id);
                let extent = binding.lines_in(available);
                let start = extent.start.min(available);
                let end_exclusive = extent.end.clamp(start, available);
                BufferRange {
                    start,
                    end_exclusive,
                }
            })
            .collect();
        Self { ranges }
    }

    /// The whole of `element`'s window into its buffer — what its *height* is measured over.
    ///
    /// Empty for an element that has fallen off the end of a buffer that shrank under it, or for
    /// an element the view does not have.
    pub fn element_range(&self, element: aether_protocol::viewport::FieldId) -> BufferRange {
        self.ranges
            .get(element as usize)
            .copied()
            .unwrap_or(BufferRange {
                start: 0,
                end_exclusive: 0,
            })
    }

    /// The part of `lines` that `element` has: a loaded slice brought inside the element as it is
    /// now, or `None` when none of it is.
    pub fn clip(
        &self,
        element: aether_protocol::viewport::FieldId,
        lines: std::ops::Range<u32>,
    ) -> Option<BufferRange> {
        self.ranges.get(element as usize)?.intersect(lines)
    }
}

impl Viewport {
    /// The element holding the live cursor, in `view` — the one this viewport presents.
    ///
    /// This replaced a `sole()` accessor that returned element 0 and was named for the assumption
    /// it encoded, so that the audit would be a grep rather than a guess. Working through those
    /// call sites is what showed almost all of them wanted *the view's* buffer — the one being
    /// edited and searched — rather than the first element's, and those two only diverge once
    /// focus exists. Falls back to the first element if `focused` is somehow out of range, since a
    /// view always has at least one and a panic here would be a strange way to report a stale id.
    pub fn focus<'v>(&self, view: &'v View) -> &'v ElementBinding {
        view.elements
            .get(self.focused as usize)
            .unwrap_or(&view.elements[0])
    }

    /// The buffer this viewport is currently acting on: the focused element's.
    pub fn buffer_id(&self, view: &View) -> BufferId {
        self.focus(view).buffer_id
    }

    /// The nearest element in `direction` the cursor can land on, or `None` at the ends.
    ///
    /// **One definition, two callers**: `Tab` steps with it, and a line motion that walks off the
    /// end of its element crosses with it. They have to agree — an element `Tab` refuses to stop
    /// on but `j` falls into is a place you can get to and not get back from — so neither owns the
    /// rule and both ask here.
    ///
    /// Two ways an element qualifies. It has lines, which is somewhere to put the cursor; or it is
    /// **collapsible**, folded or not, because folded it is a single titled row that is the whole
    /// of it and opening it is the thing you came to do. What is left out is the element with no
    /// lines and nothing to open — a shell run that said nothing — which is drawn and skipped,
    /// since stopping there would be a press that visibly did nothing.
    pub fn step_element(
        &self,
        view: &View,
        from: aether_protocol::viewport::FieldId,
        forward: bool,
    ) -> Option<aether_protocol::viewport::FieldId> {
        let last = view.elements.len().saturating_sub(1) as aether_protocol::viewport::FieldId;
        let stoppable = |i: aether_protocol::viewport::FieldId| {
            view.elements
                .get(i as usize)
                .is_some_and(|e| self.can_hold_cursor(e))
        };
        if forward {
            (from + 1..=last).find(|&i| stoppable(i))
        } else {
            (0..from).rev().find(|&i| stoppable(i))
        }
    }

    /// Whether this element is **folded shut for this viewport**.
    ///
    /// The view says what may fold; the viewport says what is folded. One expression, because the
    /// renderer and everything that asks "is there anything of this element on screen" have to
    /// mean the same thing by it.
    pub fn is_collapsed(&self, binding: &ElementBinding) -> bool {
        binding.collapsible && !self.expanded.contains(&binding.buffer_id)
    }

    /// Whether the cursor may be **in** this element — which is to say, whether this viewport is
    /// drawing rows of text the cursor could sit on.
    ///
    /// **The cursor only goes where it can be seen.** Three ways an element fails that, and they
    /// are failures of the same kind rather than three special cases:
    ///
    /// - it windows no lines (a shell run that said nothing);
    /// - it is **prose** — an agent's reply is a parse, not rows, and deliberately wears no cursor
    ///   and no focus bar (it is a record of what was said, not a place you are);
    /// - it is **folded** for this viewport, so none of its rows are being drawn at all.
    ///
    /// Landing anywhere in that list is a keystroke whose only visible effect is that the next one
    /// behaves oddly. Reaching such an element is what `Tab` is for — a folded block by its
    /// disclosure, which *is* drawn — not what a line motion is for.
    pub fn can_hold_cursor(&self, binding: &ElementBinding) -> bool {
        !binding.is_empty() && !binding.prose && !self.is_collapsed(binding)
    }

    /// Whether this viewport is showing `id` at all — as the view it presents, or as one of the
    /// buffers its elements window. The right question for anything fanning out *to viewers*,
    /// because a patch's viewers are watching the view even though no element windows it.
    pub fn shows(&self, view: &View, id: BufferId) -> bool {
        view.presenting == id || view.binds(id)
    }

    /// Whether a change to how `id`'s lines are *styled* — a parse tree landing, a Git baseline
    /// attaching — can alter what this viewport has rendered.
    ///
    /// Narrower than [`Self::shows`] on purpose. A restyle re-renders the slices the viewport has
    /// loaded, and an element with nothing loaded has nothing to restyle: the next `view/window`
    /// that loads it renders from the tree as it stands then. The distinction is what keeps a
    /// working-changes view over three hundred files from re-shipping its whole tree three
    /// hundred times as the deferred parses land, once for each file nobody has scrolled to.
    /// An element the view supplies no decorations for is counted whether or not it is loaded:
    /// its phantom rows come from the buffer's own hunks, so its *height* moves with them.
    pub fn restyles(&self, view: &View, id: BufferId) -> bool {
        view.elements.iter().enumerate().any(|(idx, e)| {
            e.buffer_id == id
                && (e.decorations.is_none()
                    || self.loaded.get(idx).is_some_and(|slice| slice.is_some()))
        })
    }

    /// Every buffer this viewport shows, each named once — [`Self::shows`] enumerated rather than
    /// asked.
    ///
    /// What a viewport being torn down was keeping alive, and so what the transient GC must
    /// consider. Its callers used to build that list from [`Self::buffer_id`], the *focused
    /// element's* buffer — which for a composed view is one of the files and never the patch, so
    /// navigating away from working changes left the patch document and every unfocused element's
    /// buffer behind with nothing showing them and nothing looking for them.
    pub fn shown_buffers(&self, view: &View) -> Vec<BufferId> {
        let mut out = vec![view.presenting];
        for e in &view.elements {
            if !out.contains(&e.buffer_id) {
                out.push(e.buffer_id);
            }
        }
        out
    }

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
mod view_layout_tests {
    use super::*;

    /// Two elements over two different files, each windowing lines 10..13 of its own — the shape
    /// that makes view lines and buffer lines diverge, and the one every coordinate bug was hiding
    /// behind. A whole-buffer element would make the two spaces coincide and prove nothing.
    fn two_hunks() -> Vec<ElementBinding> {
        let binding = |buffer_id| ElementBinding {
            collapsible: false,
            buffer_id,
            lines: ElementLines::Range {
                start: 10,
                end_exclusive: 13,
            },
            decorations: None,
            chrome_before: Default::default(),
            chrome_above: Default::default(),
            laid_out_by: LayoutOwner::Server,
            prose: false,
            role: aether_protocol::ui::ElementRole::Field,
            edges: aether_protocol::ui::Edges::NONE,
            box_group: None,
            title: Default::default(),
            band: aether_protocol::ui::Band::None,
        };
        vec![binding(1), binding(2)]
    }

    #[test]
    fn each_element_is_its_own_range_of_its_own_buffer() {
        let layout = ViewLayout::of(&two_hunks(), |_| 100);
        let range = |e| {
            let r = layout.element_range(e);
            (r.start(), r.end_exclusive())
        };
        assert_eq!(range(0), (10, 13));
        assert_eq!(range(1), (10, 13));
        assert_eq!(
            range(7),
            (0, 0),
            "an element the view does not have is empty"
        );
        // A slice is clipped to what the element has; nothing of it inside is nothing.
        assert_eq!(
            layout
                .clip(1, 12..40)
                .map(|r| (r.start(), r.end_exclusive())),
            Some((12, 13))
        );
        assert_eq!(layout.clip(1, 20..40), None);
    }

    /// A restyle — a parse landing, a baseline attaching — re-renders what a viewport has loaded,
    /// so it reaches a viewport only where that changes something: an element of the buffer with
    /// a slice loaded, or one whose height the buffer's own hunks decide. A bound hunk nobody has
    /// scrolled to is *shown* but not *restyled*; the distinction is what stops a large review
    /// re-shipping its tree once per file as the deferred parses land.
    #[test]
    fn a_restyle_reaches_only_viewports_with_something_of_the_buffer_loaded() {
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
        let (a, b, c) = (
            file(1, &"fn one() {}\n".repeat(40)),
            file(2, &"fn two() {}\n".repeat(40)),
            file(3, &"fn three() {}\n".repeat(40)),
        );
        let element = |buffer, decorated: bool| ElementLayout {
            extent: ElementExtent::Bound {
                buffer,
                lines: 10..14,
            },
            chrome_before: Default::default(),
            chrome_above: Default::default(),
            decorations: decorated.then(|| std::sync::Arc::new(ElementDecorations::default())),
            edges: aether_protocol::ui::Edges::NONE,
            box_group: None,
            title: Default::default(),
            band: aether_protocol::ui::Band::None,
            role: aether_protocol::ui::ElementRole::Field,
        };
        let patch = file(4, "patch\n");
        // Two patch hunks the view decorates itself, and a plain editor element over `c`.
        s.set_view_layout(
            patch,
            vec![element(a, true), element(b, true), element(c, false)],
        );
        let view_id = s.view_presenting(patch).expect("the layout's view");
        let vp = Viewport {
            expanded: Default::default(),
            id: 1,
            client_id: uuid::Uuid::new_v4(),
            view_id,
            rows: 10,
            overscan_rows: 0,
            wrap: WrapMode::None,
            tab_width: 4,
            diff_view: false,
            cols: 80,
            continuation_marker_width: 0,
            // Only `a`'s hunk is on screen.
            loaded: vec![Some(10..14), None, None],
            anchor: ScrollPosition::default(),
            focused: 0,
        };
        let view = s.view(view_id);

        assert!(
            vp.restyles(view, a),
            "loaded: a restyle changes what is drawn"
        );
        assert!(
            !vp.restyles(view, b),
            "bound but nothing loaded: the next window load renders from the tree as it stands"
        );
        assert!(
            vp.shows(view, b),
            "…though the viewport does show it, which is the broader question"
        );
        assert!(
            vp.restyles(view, c),
            "an element the view leaves undecorated takes its phantom rows from the buffer's own \
             hunks, so its height can move — counted whether or not it is loaded"
        );
        assert!(
            vp.shows(view, patch) && !vp.restyles(view, patch),
            "the presenting document is shown, but no element renders it here"
        );
    }

    /// An edit inside a hunk grows it; one above slides it; one below leaves it alone. The same
    /// three answers a re-diff would give, without re-diffing on every keystroke.
    #[test]
    fn an_edit_moves_the_elements_it_lands_in_and_the_ones_below_it() {
        let extents = |elements: &[ElementBinding]| -> Vec<(u32, u32)> {
            elements
                .iter()
                .map(|e| {
                    let r = e.lines_in(u32::MAX);
                    (r.start, r.end)
                })
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
        let bound = |buffer, lines: std::ops::Range<u32>| ElementLayout {
            extent: ElementExtent::Bound { buffer, lines },
            chrome_before: Default::default(),
            chrome_above: Default::default(),
            decorations: None,
            edges: aether_protocol::ui::Edges::NONE,
            box_group: None,
            title: Default::default(),
            band: aether_protocol::ui::Band::None,
            role: aether_protocol::ui::ElementRole::Field,
        };
        let patch = file(3, "patch\n");
        s.set_view_layout(
            patch,
            vec![bound(a, 10..14), bound(a, 30..34), bound(b, 10..14)],
        );
        let view = s.view_presenting(patch).expect("the layout's view");
        let vp = Viewport {
            expanded: Default::default(),
            id: 1,
            client_id: uuid::Uuid::new_v4(),
            view_id: view,
            rows: 10,
            overscan_rows: 0,
            wrap: WrapMode::None,
            tab_width: 4,
            diff_view: false,
            cols: 80,
            continuation_marker_width: 0,
            // The client has the first hunk and the last one loaded.
            loaded: vec![Some(10..14), None, Some(10..14)],
            anchor: ScrollPosition::default(),
            focused: 0,
        };
        s.viewports.insert(1, vp);

        // Two lines typed into the first hunk of `a`.
        s.shift_element_extents(a, LineShift { at: 12, delta: 2 });
        assert_eq!(
            extents(&s.view(view).elements),
            vec![(10, 16), (32, 36), (10, 14)],
            "the hunk grew, the one below it slid, the other file's did not move"
        );
        // The viewport reads the same elements — there is no copy to have gone stale — and the
        // slice it had loaded of the edited hunk grew with it.
        assert_eq!(
            extents(&s.view_of(&s.viewports[&1]).elements),
            vec![(10, 16), (32, 36), (10, 14)]
        );
        assert_eq!(
            s.viewports[&1].loaded,
            vec![Some(10..16), None, Some(10..14)],
            "the loaded slice of the edited hunk holds the new lines; the other file's does not move"
        );

        // A deletion below every element of `a` changes nothing.
        s.shift_element_extents(a, LineShift { at: 38, delta: -1 });
        assert_eq!(
            extents(&s.view(view).elements),
            vec![(10, 16), (32, 36), (10, 14)]
        );
    }

    /// An extent comes from a diff; the buffer it indexes is live. Once the file shrinks under it,
    /// trusting the extent is what indexed past the end of a rope and panicked the server — so the
    /// layout clamps at construction and the view simply gets shorter.
    #[test]
    fn a_stale_extent_is_clamped_to_the_buffer_that_is_actually_there() {
        // The second file has been cut down to 11 lines, so its 10..13 window holds only line 10.
        let layout = ViewLayout::of(&two_hunks(), |id| if id == 2 { 11 } else { 100 });
        let r = layout.element_range(1);
        assert_eq!((r.start(), r.end_exclusive()), (10, 11));
        assert_eq!(
            layout
                .clip(1, 10..13)
                .map(|r| (r.start(), r.end_exclusive())),
            Some((10, 11)),
            "a slice over the old extent is brought inside the new one"
        );
        // And a file gone entirely from under an element contributes no lines at all.
        let gone = ViewLayout::of(&two_hunks(), |id| if id == 2 { 0 } else { 100 });
        assert!(gone.element_range(1).is_empty());
        assert_eq!(gone.clip(1, 10..13), None);
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
            // A shell is not a repo at all. Its key leads with a token no canonical workdir can
            // start with, so the two namespaces can't collide.
            VirtualTarget::shell("my-project", 1),
            VirtualTarget::shell("weird:name@here", 42),
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
        assert_eq!(working.repo_id(), Some("/proj"));
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

        // A shell has no repo, no revision and no path — asking gets `None` rather than an
        // invented empty string that would then be handed to git.
        let shell = VirtualTarget::shell("proj", 2);
        assert_eq!(shell.repo_id(), None);
        assert_eq!(shell.what(), None);
        assert_eq!(shell.rev(), None);
        assert_eq!(shell.path(), None);
        assert!(!shell.is_immutable());
        assert_eq!(shell.title().as_deref(), Some("Shell 2"));
    }
}

#[cfg(test)]
mod transcript_tests {
    use super::*;
    use crate::shell::Transcript;

    /// A shell whose transcript holds `text` and whose runs are `(start, end_exclusive)`.
    fn shell_state(text: &str, runs: &[(u32, u32)]) -> (ServerState, BufferId, BufferId) {
        let mut s = ServerState::new();
        let transcript = s.allocate_buffer_id();
        let input = s.allocate_buffer_id();
        s.insert_buffer_with_document(input, None, false, |id| Document::field(id, None));
        let mut t = Transcript::new(input, PathBuf::from("/tmp"), "Shell 1".into());
        for (start, end) in runs {
            let (handle, _token) = crate::process::cancel_channel();
            let id = t.push_run(format!("cmd{start}"), *start, handle);
            t.run_mut(id).unwrap().end_line_exclusive = *end;
            t.run_mut(id).unwrap().status = aether_protocol::shell::RunStatus::Exited { code: 0 };
        }
        let doc_id = s.allocate_document_id();
        s.documents.insert(
            doc_id,
            Document::virtual_content(
                doc_id,
                VirtualSource {
                    target: VirtualTarget::shell("proj", 1),
                    title: "Shell 1".into(),
                    commit: None,
                },
                text.to_string(),
                None,
                Some(Generated::Shell(t)),
                false,
            ),
        );
        s.buffers.insert(
            transcript,
            Buffer {
                id: transcript,
                document: doc_id,
                scratch_number: None,
            },
        );
        s.buffer_workspaces.insert(transcript, "proj".into());
        s.buffer_workspaces.insert(input, "proj".into());
        // Kept, as `mint_shell` makes it: a shell is somewhere you are working.
        s.open_view(transcript, Some(false));
        (s, transcript, input)
    }

    /// The view a shell presents: one element per run, over the transcript's own lines, and the
    /// **input last** — which is what makes a client able to find it without knowing what kind of
    /// view it is looking at.
    #[test]
    fn a_shell_view_is_its_runs_then_its_input() {
        let (s, transcript, input) = shell_state("one\ntwo\n", &[(0, 1), (1, 2)]);
        let view = s.view(s.view_presenting(transcript).unwrap());
        assert_eq!(view.elements.len(), 3, "two runs and the input");
        // A blank row of ground stands before every box but the first.
        assert!(view.elements[0].chrome_before.is_empty());
        assert!(!view.elements[1].chrome_before.is_empty());
        assert!(!view.elements[2].chrome_before.is_empty());
        assert_eq!(view.elements[0].buffer_id, transcript);
        assert_eq!(
            view.elements[0].lines,
            ElementLines::Range {
                start: 0,
                end_exclusive: 1
            }
        );
        assert_eq!(
            view.elements[0].chrome_above.len(),
            1,
            "a run's box holds the command it ran, and nothing else above its output"
        );
        assert!(
            view.elements[0]
                .title
                .iter()
                .map(aether_protocol::viewport::Element::text_content)
                .collect::<String>()
                .starts_with('/'),
            "and its box is named for the directory it ran in"
        );
        let last = view.elements.last().unwrap();
        assert_eq!(last.buffer_id, input, "the input is the last element");
        assert_eq!(last.lines, ElementLines::Whole);
        assert!(last.role.is_input());
        assert!(
            view.elements[..2].iter().all(|e| !e.role.is_input()),
            "and nothing else claims to be one"
        );
        // A driver-built view is composed, and a shell is one of those.
        assert!(view.is_composed());

        // Each run is a box of its own, and so is the input: closed on all four sides, with
        // nothing shared between them — a run is read on its own, and it is named on its own.
        let groups: Vec<_> = view.elements.iter().map(|e| e.box_group).collect();
        assert_eq!(groups, vec![Some(0), Some(1), Some(2)]);
        for e in &view.elements {
            assert_eq!(
                (
                    e.edges.border.top,
                    e.edges.border.right,
                    e.edges.border.bottom,
                    e.edges.border.left
                ),
                (1, 1, 1, 1),
                "every box closes itself"
            );
            assert!(!e.edges.collapse, "and shares no edge with the next");
            assert_eq!(e.band, aether_protocol::ui::Band::Chrome);
            assert!(!e.title.is_empty(), "every box is named");
        }
        // The input's box says only where it is: there is no outcome yet, and nothing above the
        // line you type.
        assert!(last.chrome_above.is_empty());
        assert_eq!(
            last.title
                .iter()
                .map(aether_protocol::viewport::Element::text_content)
                .collect::<String>(),
            view.elements[0]
                .title
                .iter()
                .map(aether_protocol::viewport::Element::text_content)
                .collect::<String>()
                .split("  ")
                .next()
                .unwrap(),
            "the same directory a run's title opens with"
        );
    }

    /// An empty shell is a real state: the view is just the input, and it still has one.
    #[test]
    fn a_new_shell_is_just_its_input() {
        let (s, transcript, input) = shell_state("", &[]);
        let view = s.view(s.view_presenting(transcript).unwrap());
        assert_eq!(view.elements.len(), 1);
        assert_eq!(view.elements[0].buffer_id, input);
        assert!(view.elements[0].role.is_input());
        // And it is still not a file's editor. A fresh shell is one whole-buffer element with no
        // chrome above it, which is the shape `is_composed()` reads — what tells the two apart is the
        // role, the box and the name on it, and dropping any of those from the test hands a shell
        // a client-choosable kind and offers to re-present it as a reader.
        assert!(view.is_composed());
    }

    /// The tail write is what output arrives through: it extends the document, carries the active
    /// run's extent with it, and leaves the buffer clean — a transcript is not unsaved work.
    #[test]
    fn extending_a_transcript_moves_the_active_runs_extent_and_stays_clean() {
        let (mut s, transcript, _) = shell_state("", &[]);
        let (handle, _token) = crate::process::cancel_channel();
        s.with_transcript(transcript, |t| t.push_run("ls".into(), 0, handle));
        let before = s.doc_of(transcript).revision;

        assert!(s.extend_transcript(transcript, 0, "one\ntwo"));
        let doc = s.doc_of(transcript);
        assert_eq!(doc.text.to_string(), "one\ntwo");
        assert!(doc.revision > before, "pushes are revision-guarded");
        assert!(!doc.dirty, "generated content is never unsaved work");
        assert_eq!(doc.saved_revision(), doc.revision);
        let run = &doc.transcript().unwrap().runs[0];
        assert_eq!((run.start_line, run.end_line_exclusive), (0, 2));

        // A carriage-return redraw rewrites the last line: the tail write starts at that line, so
        // the document ends up with what the reader would see, not with both versions.
        assert!(s.extend_transcript(transcript, 4, "three\n"));
        assert_eq!(s.doc_of(transcript).text.to_string(), "one\nthree\n");
        let run = &s.doc_of(transcript).transcript().unwrap().runs[0];
        assert_eq!(
            (run.start_line, run.end_line_exclusive),
            (0, 2),
            "the phantom line after a final newline belongs to no run"
        );
    }

    /// A run is appended *above* the input, so the input's element number goes up by one with
    /// every command — and a viewport's focus is that number. The caret follows the input, not
    /// the number; a caret parked on a run's output is left where it is.
    #[test]
    fn focus_follows_the_input_when_a_run_is_appended_above_it() {
        let (mut s, transcript, _) = shell_state("", &[]);
        let view = s.view_presenting(transcript).unwrap();
        assert_eq!(
            s.view(view).input_element(),
            Some(0),
            "a fresh shell is only its input"
        );
        let viewport = |id, focused| Viewport {
            expanded: Default::default(),
            id,
            client_id: uuid::Uuid::new_v4(),
            view_id: view,
            rows: 10,
            overscan_rows: 0,
            wrap: WrapMode::None,
            tab_width: 4,
            diff_view: false,
            cols: 80,
            continuation_marker_width: 0,
            loaded: vec![None],
            anchor: ScrollPosition::default(),
            focused,
        };
        s.viewports.insert(1, viewport(1, 0));

        let (handle, _token) = crate::process::cancel_channel();
        s.with_transcript(transcript, |t| t.push_run("one".into(), 0, handle));
        s.extend_transcript(transcript, 0, "first\n");
        assert_eq!(s.view(view).input_element(), Some(1));
        assert_eq!(
            s.viewports[&1].focused, 1,
            "the caret is still in the input"
        );

        // A second viewport is reading the first run's output.
        s.viewports.insert(2, viewport(2, 0));
        let start = s.doc_of(transcript).line_count() - 1;
        let from = s.doc_of(transcript).text.len_chars();
        let (handle, _token) = crate::process::cancel_channel();
        s.with_transcript(transcript, |t| t.push_run("two".into(), start, handle));
        s.extend_transcript(transcript, from, "second\n");
        assert_eq!(s.view(view).input_element(), Some(2));
        assert_eq!(s.viewports[&1].focused, 2, "still in the input");
        assert_eq!(s.viewports[&2].focused, 0, "still reading the first run");
    }

    /// A second run appends an element and leaves the first one exactly where it was — the
    /// property the whole transcript rests on, since a run's output is final once it is over.
    #[test]
    fn a_second_run_leaves_the_first_alone() {
        let (mut s, transcript, _) = shell_state("", &[]);
        let (handle, _token) = crate::process::cancel_channel();
        s.with_transcript(transcript, |t| t.push_run("one".into(), 0, handle));
        s.extend_transcript(transcript, 0, "first\n");
        s.with_transcript(transcript, |t| {
            t.runs[0].status = aether_protocol::shell::RunStatus::Exited { code: 0 };
        });
        let first = s.view(s.view_presenting(transcript).unwrap()).elements[0].lines;

        let start = s.doc_of(transcript).line_count() - 1;
        let (handle, _token) = crate::process::cancel_channel();
        s.with_transcript(transcript, |t| t.push_run("two".into(), start, handle));
        s.extend_transcript(
            transcript,
            s.doc_of(transcript).text.len_chars(),
            "second\n",
        );

        let view = s.view(s.view_presenting(transcript).unwrap());
        assert_eq!(view.elements.len(), 3, "two runs and the input");
        assert_eq!(view.elements[0].lines, first, "the first run has not moved");
        assert_eq!(
            view.elements[1].lines,
            ElementLines::Range {
                start: 1,
                end_exclusive: 2
            }
        );
        assert_eq!(s.doc_of(transcript).text.to_string(), "first\nsecond\n");
    }

    /// The tail write splices the wrap cache rather than clearing it — the reason a hundred
    /// thousand lines of output don't re-wrap twenty times a second.
    #[test]
    fn a_tail_write_splices_the_wrap_cache() {
        let (mut s, transcript, _) = shell_state("", &[]);
        let (handle, _token) = crate::process::cancel_channel();
        s.with_transcript(transcript, |t| t.push_run("ls".into(), 0, handle));
        s.extend_transcript(transcript, 0, "aaaa\nbb\n");
        let geom = crate::wrap::WrapGeometry {
            wrap: aether_protocol::viewport::WrapMode::Soft,
            cols: 3,
            marker_width: 0,
            tab_width: 4,
        };
        // Populate the cache, then extend and read it back: the counts for the untouched lines
        // must survive, and the new ones must be present.
        assert_eq!(&s.doc_of(transcript).wrapped_rows(geom)[..2], &[2, 1]);
        s.extend_transcript(
            transcript,
            s.doc_of(transcript).text.len_chars(),
            "cccccc\n",
        );
        assert_eq!(&s.doc_of(transcript).wrapped_rows(geom)[..3], &[2, 1, 2]);
    }

    /// A transcript is generated content, not something the user typed: appending to it must not
    /// give them an undo step, or a long build would fill the stack with rope snapshots.
    #[test]
    fn appending_output_is_not_undoable() {
        let (mut s, transcript, _) = shell_state("", &[]);
        let (handle, _token) = crate::process::cancel_channel();
        s.with_transcript(transcript, |t| t.push_run("ls".into(), 0, handle));
        s.extend_transcript(transcript, 0, "line\n");
        // The only door to a mutation with undo is `Editable`, and a virtual document is refused
        // at it — which is what makes "output is not undoable" structural rather than a
        // convention the streaming task has to remember.
        assert!(s.editable_doc(transcript).is_err());
    }

    /// The input is a document of the view's, not of the user's: it never counts as unsaved work,
    /// never appears in the session file, and never pins the idle reaper.
    #[test]
    fn the_input_is_internal_and_never_dirty() {
        let (mut s, transcript, input) = shell_state("", &[]);
        {
            let doc = s.doc_of_mut(input);
            assert!(doc.internal);
        }
        // Type into it through the ordinary edit path.
        let mut editable = s.editable_doc(input).expect("the input is editable");
        editable.apply_edit(0, 0, "cargo build", EditKindTag::Text, HashMap::new());
        let doc = s.doc_of(input);
        assert_eq!(doc.text.to_string(), "cargo build");
        assert!(!doc.dirty, "a half-typed command is not unsaved work");
        assert_eq!(
            doc.saved_revision(),
            doc.revision,
            "so the client's dirty dot stays out"
        );
        assert!(!s.has_unprotected_unsaved_buffers());
        assert!(
            s.session_views("proj").is_empty(),
            "and nothing is recorded"
        );
        let _ = transcript;
    }

    /// Closing a shell takes its input and its runs with it. Nothing else can reach either.
    #[test]
    fn closing_a_shell_drops_its_input_and_stops_its_runs() {
        let (mut s, transcript, input) = shell_state("", &[]);
        let (handle, mut token) = crate::process::cancel_channel();
        s.with_transcript(transcript, |t| t.push_run("sleep 100".into(), 0, handle));
        assert!(!*token.borrow_and_update());

        s.close_buffer(transcript);
        assert!(*token.borrow_and_update(), "the run was told to stop");
        assert!(!s.buffers.contains_key(&input), "the input went with it");
        assert!(!s.buffers.contains_key(&transcript));
    }

    /// Shell numbers behave like scratch numbers: lowest free, per workspace, reused on close.
    #[test]
    fn shell_numbers_are_the_lowest_free_per_workspace() {
        let (mut s, transcript, _) = shell_state("", &[]);
        assert_eq!(s.next_shell_number("proj"), 2, "1 is taken");
        assert_eq!(s.next_shell_number("other"), 1, "a different workspace");
        s.close_buffer(transcript);
        assert_eq!(s.next_shell_number("proj"), 1, "and 1 is free again");
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
            mru_views: VecDeque::new(),
            dormant_views: Vec::new(),
            jumplist: None,
            nav_history: Default::default(),
            last_nav: None,
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

    /// One trail per (context, client), plus the copy a context keeps of the last one to leave it.
    ///
    /// Pins the whole ownership rule in one pass: recording in A leaves B's trail alone, switching
    /// away stores a hand-over copy without taking the trail (so the return finds it intact), an
    /// arriving client with none of its own starts from that copy — a *copy*, so stepping it does
    /// not move the trail it came from — and a disconnect takes the trail out, leaving it behind
    /// as the context's hand-over.
    #[test]
    fn nav_trails_belong_to_the_context_and_are_handed_over() {
        let nav_entry = |view: u64| NavEntry {
            view_id: ViewId(view),
            buffer_id: 1,
            path_index: Some(0),
            relative_path: Some("a.txt".to_string()),
            virtual_key: None,
            element: None,
            cursor: CursorState::default(),
            read: None,
        };

        let mut s = ServerState::new();
        s.workspaces
            .insert("a".to_string(), workspace_entry("a", vec![]));
        s.workspaces
            .insert("b".to_string(), workspace_entry("b", vec![]));
        let (c1, sess1) = session("a");
        s.clients.insert(c1, sess1);

        s.nav_history_mut(c1).unwrap().record(nav_entry(1));
        assert_eq!(s.nav_history(c1).unwrap().back.len(), 1);

        // Switch to B: a context the client has never navigated in has nothing to step.
        s.teardown_client_state_for_workspace(c1, "a");
        s.activate_workspace_for_client(c1, "b");
        assert!(s.nav_history(c1).is_none(), "B's trail is not A's");
        s.nav_history_mut(c1).unwrap().record(nav_entry(2));
        assert_eq!(
            s.workspaces["a"].nav_history[&c1].back[0].view_id,
            ViewId(1),
            "recording in B left A's trail alone"
        );

        // ...and back: the trail the client kept in A is the one it finds.
        s.teardown_client_state_for_workspace(c1, "b");
        s.activate_workspace_for_client(c1, "a");
        assert_eq!(s.nav_history(c1).unwrap().back[0].view_id, ViewId(1));

        // A second window in A starts from the hand-over the switch left behind — a copy.
        let (c2, sess2) = session("a");
        s.clients.insert(c2, sess2);
        s.activate_workspace_for_client(c2, "a");
        assert_eq!(s.nav_history(c2).unwrap().back[0].view_id, ViewId(1));
        s.nav_history_mut(c2).unwrap().back.clear();
        assert_eq!(
            s.nav_history(c1).unwrap().back.len(),
            1,
            "stepping one window's trail does not move the other's"
        );

        // The disconnecting window's trail leaves A as A's hand-over.
        s.drop_nav_history_for_client(c1);
        assert!(!s.workspaces["a"].nav_history.contains_key(&c1));
        assert_eq!(
            s.workspaces["a"].last_nav.as_ref().unwrap().back[0].view_id,
            ViewId(1)
        );
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

        // A transient preview at the MRU front: persisted, marked as one — it is where the
        // window was when the session was written.
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
        s.workspaces.get_mut("p").unwrap().dormant_views = vec![
            DormantView {
                id: d1,
                view: ViewId(d1),
                read: false,
                transient: false,
                source: DormantSource::File(PathBuf::from("/p/c.rs")),
            },
            DormantView {
                id: d_dup,
                view: ViewId(d_dup),
                read: false,
                transient: false,
                source: DormantSource::File(PathBuf::from("/p/a.rs")),
            },
        ];

        use crate::config::SessionView;
        assert_eq!(
            s.session_views("p"),
            vec![
                // the clean scratch (2) is the one exclusion; the dirty scratch (MRU front) is
                // kept.
                SessionView::Scratch {
                    number: 1,
                    transient: false
                },
                // the preview is recorded as one — exiting in it comes back to it
                SessionView::file(PathBuf::from("/p/preview.rs"), false, true),
                // most-recent kept file, as it was last shown (never, here: source)
                SessionView::file(PathBuf::from("/p/b.rs"), false, false),
                SessionView::file(PathBuf::from("/p/a.rs"), false, false),
                // dormant; /p/a.rs dropped as a dup of the live buffer
                SessionView::file(PathBuf::from("/p/c.rs"), false, false),
            ]
        );
    }

    /// Transience is honoured for the landing only: after activation has decided where to land,
    /// every other transient dormant row goes — two windows leave two previews, and a launch that
    /// lands elsewhere must not leave one behind as a listed row.
    #[test]
    fn drop_transient_dormant_keeps_only_the_landing() {
        let mut s = ServerState::new();
        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));
        let row = |s: &mut ServerState, path: &str, transient: bool| {
            let id = s.allocate_buffer_id();
            DormantView {
                id,
                view: ViewId(id),
                read: false,
                transient,
                source: DormantSource::File(PathBuf::from(path)),
            }
        };
        let front = row(&mut s, "/p/front.rs", true);
        let second = row(&mut s, "/p/second.rs", true);
        let kept = row(&mut s, "/p/kept.rs", false);
        let landing = front.view;
        s.workspaces.get_mut("p").unwrap().dormant_views = vec![front, second, kept.clone()];

        s.drop_transient_dormant("p", Some(landing));
        let left: Vec<ViewId> = s.workspaces["p"]
            .dormant_views
            .iter()
            .map(|d| d.view)
            .collect();
        assert_eq!(
            left,
            vec![landing, kept.view],
            "the landing and the kept row"
        );
        assert!(
            s.sessions_dirty.contains("p"),
            "dropping rows changes what the session should say"
        );

        // Nothing to land on (a tethered launch): the preview goes too.
        s.drop_transient_dormant("p", None);
        let left: Vec<ViewId> = s.workspaces["p"]
            .dormant_views
            .iter()
            .map(|d| d.view)
            .collect();
        assert_eq!(left, vec![kept.view]);
    }

    /// A buffer presents exactly one view: an open of a buffer that has one is that view again,
    /// whatever it asks, and restoring what a dormant row stood for keeps it so.
    #[test]
    fn a_buffer_presents_exactly_one_view() {
        let mut s = ServerState::new();
        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));
        let b = s.allocate_buffer_id();
        s.insert_buffer_with_document(b, None, true, |d| {
            Document::new_at_path(d, PathBuf::from("/p/a.md"), Some("markdown".into()))
        });
        let first = s.open_view(b, Some(true));
        assert!(s.views[&first].transient);
        assert_eq!(
            s.open_view(b, None),
            first,
            "the view again, whatever the intent"
        );
        assert_eq!(s.open_view(b, Some(false)), first, "pinned, not duplicated");
        assert!(!s.views[&first].transient);
        s.restore_dormant_views(
            b,
            vec![
                DormantPresentation {
                    read: true,
                    transient: false,
                },
                DormantPresentation::default(),
            ],
        );
        assert_eq!(s.views_presenting(b), vec![first]);
        assert_eq!(
            s.doc_of(b).read_last,
            Some(true),
            "the most recent entry's mode is what the file remembers"
        );
        // Reading is per client, seeded from that memory, and never a second view.
        let c1 = ClientId::from_u128(1);
        let c2 = ClientId::from_u128(2);
        assert!(s.land_read(c1, b));
        assert!(s.set_read_mode(c1, b, false));
        assert!(
            !s.land_read(c2, b),
            "seeded from the file's memory, which c1's flip just wrote"
        );
        assert!(!s.read_mode(c1, b) && !s.read_mode(c2, b));
        assert!(s.set_read_mode(c2, b, true));
        assert!(!s.read_mode(c1, b), "c2's flip leaves c1 alone");
        assert_eq!(s.views_presenting(b), vec![first]);
    }

    /// The dormant-registry helpers: `first_dormant_buffer` is the landing target (front of the list),
    /// `take_dormant` removes and returns the entry by id (materialization), and `promote_dormant`
    /// drops a file path once it's loaded.
    #[test]
    fn dormant_registry_take_promote_and_first() {
        let mut s = ServerState::new();
        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));
        let d1 = s.allocate_buffer_id();
        let d2 = s.allocate_buffer_id();
        s.workspaces.get_mut("p").unwrap().dormant_views = vec![
            DormantView {
                id: d1,
                view: ViewId(d1),
                read: false,
                transient: false,
                source: DormantSource::File(PathBuf::from("/p/a.rs")),
            },
            DormantView {
                id: d2,
                view: ViewId(d2),
                read: false,
                transient: false,
                source: DormantSource::File(PathBuf::from("/p/b.rs")),
            },
        ];

        assert_eq!(
            s.first_dormant_buffer("p"),
            Some(d1),
            "front of the list lands"
        );
        assert_eq!(
            s.take_dormant("p", d1).map(|d| d.source),
            Some(DormantSource::File(PathBuf::from("/p/a.rs")))
        );
        assert!(
            s.take_dormant("p", d1).is_none(),
            "removed; a second take is empty"
        );
        assert_eq!(s.first_dormant_buffer("p"), Some(d2));
        s.promote_dormant("p", Path::new("/p/b.rs"));
        assert_eq!(
            s.first_dormant_buffer("p"),
            None,
            "promotion empties the registry"
        );
    }

    /// A restored *scratch* is a landing target like any other dormant buffer: it sits at the front
    /// of the session's buffer list when it's what you were last editing, and that's what a restart
    /// lands you on.
    #[test]
    fn first_dormant_buffer_includes_restored_scratches() {
        let mut s = ServerState::new();
        s.workspaces
            .insert("p".into(), workspace_entry("p", vec![PathBuf::from("/p")]));
        let scratch = s.allocate_buffer_id();
        let file = s.allocate_buffer_id();
        s.workspaces.get_mut("p").unwrap().dormant_views = vec![
            DormantView {
                id: scratch,
                view: ViewId(scratch),
                read: false,
                transient: false,
                source: DormantSource::Scratch { number: 1 },
            },
            DormantView {
                id: file,
                view: ViewId(file),
                read: false,
                transient: false,
                source: DormantSource::File(PathBuf::from("/p/a.rs")),
            },
        ];

        assert_eq!(
            s.first_dormant_buffer("p"),
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
                views: vec![crate::config::SessionView::Editor {
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
        let viewed_view = s.view_presenting(viewed_buffer).expect("a view per buffer");
        let viewport_id = s.allocate_viewport_id();
        s.viewports.insert(
            viewport_id,
            Viewport {
                expanded: Default::default(),
                id: viewport_id,
                view_id: viewed_view,
                focused: 0,
                client_id: uuid::Uuid::new_v4(),
                rows: 24,
                overscan_rows: 0,
                wrap: WrapMode::None,
                tab_width: 4,
                diff_view: false,
                cols: 80,
                continuation_marker_width: 0,
                loaded: vec![Some(0..1)],
                anchor: ScrollPosition::default(),
            },
        );
        s.open_view(viewed_buffer, None);

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

#[cfg(test)]
mod layout_cache_tests {
    use super::*;
    use crate::wrap::WrapGeometry;
    use aether_protocol::viewport::WrapMode;

    fn geom(cols: u32) -> WrapGeometry {
        WrapGeometry {
            wrap: WrapMode::Soft,
            cols,
            marker_width: 0,
            tab_width: 4,
        }
    }

    fn doc(text: &str) -> Document {
        let mut d = Document::scratch(DocumentId(1), None);
        d.text = ropey::Rope::from_str(text);
        d
    }

    /// The oracle: the same table derived from a document that has never been edited.
    fn fresh(d: &Document, cols: u32) -> Vec<u32> {
        doc(&d.text.to_string())
            .wrapped_rows(geom(cols))
            .as_ref()
            .clone()
    }

    fn edit(d: &mut Document, start: usize, end: usize, text: &str) {
        d.apply_edit(start, end, text, EditKindTag::Text, Default::default());
    }

    /// Edits splice the table rather than rebuilding it, and several edits before a read merge
    /// into one splice — so the merged region has to be the right one, or the table quietly drifts
    /// from the text it describes.
    #[test]
    fn wrapped_rows_follow_edits_by_splicing() {
        let mut d = doc("aa\nbb\ncc\ndd\nee\n");
        assert_eq!(d.wrapped_rows(geom(8)).as_ref(), &vec![1, 1, 1, 1, 1, 1]);

        // A long line lands in the middle and wraps to three rows.
        edit(&mut d, 6, 6, "0123456789012345678\n");
        assert_eq!(d.wrapped_rows(geom(8)).as_ref(), &fresh(&d, 8));
        assert_eq!(d.wrapped_rows(geom(8))[2], 3, "the inserted line wraps");

        // Two edits before anyone looks: one above the long line, one that rejoins two lines below
        // it — the merge has to cover both, in each other's shifted coordinates.
        edit(&mut d, 0, 0, "x\n");
        let text = d.text.to_string();
        let join = text.find("dd\nee").unwrap() + 2;
        edit(&mut d, join, join + 1, "");
        assert_eq!(d.wrapped_rows(geom(8)).as_ref(), &fresh(&d, 8));

        // A second geometry is its own table, kept beside the first.
        assert_eq!(d.wrapped_rows(geom(5)).as_ref(), &fresh(&d, 5));
        assert_eq!(d.wrapped_rows(geom(8)).as_ref(), &fresh(&d, 8));
        edit(&mut d, 0, 2, "");
        assert_eq!(d.wrapped_rows(geom(5)).as_ref(), &fresh(&d, 5));
        assert_eq!(d.wrapped_rows(geom(8)).as_ref(), &fresh(&d, 8));
    }

    /// Undo swaps the rope wholesale, where no splice describes what moved: the table starts over.
    #[test]
    fn a_wholesale_replacement_starts_over() {
        let mut d = doc("aa\nbb\n");
        let _ = d.wrapped_rows(geom(8));
        edit(&mut d, 3, 3, "0123456789012345678\n");
        assert_eq!(d.wrapped_rows(geom(8))[1], 3);
        d.undo(Default::default()).expect("one edit to undo");
        assert_eq!(d.wrapped_rows(geom(8)).as_ref(), &fresh(&d, 8));
    }

    #[test]
    fn the_widest_line_follows_edits() {
        let mut d = doc("aa\nbbbb\n");
        assert_eq!(d.max_line_width(4), 4);
        edit(&mut d, 0, 0, "0123456789\n");
        assert_eq!(d.max_line_width(4), 10);
        edit(&mut d, 0, 11, "");
        assert_eq!(d.max_line_width(4), 4);
    }
}
