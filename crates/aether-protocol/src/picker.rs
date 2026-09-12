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
use crate::git::GitStatus;
use crate::history::HistoryKind;
use crate::lsp::{LspProgress, LspStatus};
use crate::viewport::{DiagnosticSeverity, DiffStage};
use crate::{BufferId, LogicalPosition};
use serde::{Deserialize, Serialize};

/// Which picker the client is talking about. Keyed `(client_id, kind)` server-side; only one
/// instance per kind per client lives at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickerKind {
    /// Workspace files, fuzzy-matched on path.
    Files,
    /// The workspace's **buffers**, ordered by most-recently-used — then the kept-but-unvisited
    /// ones, then the session's dormant rows. The current view sits at position 0 and selecting it
    /// is a no-op switch.
    ///
    /// A buffer is *a thing you reached on purpose that is not a shell and not an agent*: files,
    /// scratches, and the read-only git views (a commit's patch, a file at a revision, the working
    /// changes). Shells and conversations have pickers of their own ([`Self::Shells`],
    /// [`Self::Agents`]) because their rows answer different questions — what is running, and how
    /// it went. One row per buffer: how a client is reading a markdown file is that client's
    /// presentation of it, not a row.
    Buffers,
    /// The workspace's shell views, most-recently-used first, then its dormant (session-restored,
    /// not-yet-loaded) ones. Rows are [`PickerItem::Shell`]; status is a badge, never a sort key,
    /// so a run starting or finishing re-paints a row and never reorders the list.
    Shells,
    /// The workspace's agent conversations, ordered exactly as [`Self::Shells`] is. Rows are
    /// [`PickerItem::Agent`].
    Agents,
    /// Workspace-wide content search. Each candidate is a single match on a single line; the query
    /// *is* the search (no fuzzy filtering on a pre-built candidate set), so query changes throw
    /// out the prior candidates and start a fresh scan. Each open starts a *fresh* search: query,
    /// hits and filter chips all go ([`PickerReset::All`]) — the jobs the old resume did are
    /// covered better elsewhere (the jumplist keeps a result set you can step with `]`/`[`, and
    /// `Up` recalls a past query *with its chips*). Hits are still preserved
    /// *within* one open, across the scroll/re-view cycle.
    Grep,
    /// Filesystem explorer. Entries are the children of one directory. The query is a *path*
    /// relative to the committed *anchor* directory: its part up to the last `/` selects which
    /// directory under the anchor to list (a "peek" — `src/` lists `src`, `src/ma` lists `src`
    /// filtered by `ma`), and the server re-lists it on each `picker/query`. The part after the
    /// last `/` prefix-matches entry names. Committing navigation (Enter on a dir, parent via
    /// Alt-h, root select) moves the anchor via `picker/view` with a new `directory_path`; the
    /// result + push carry the anchor's canonical path (not the peeked listing's), so the
    /// breadcrumb stays put while peeking and backspacing the query walks the peek back.
    Explorer,
    /// Configured workspaces under `$XDG_CONFIG_HOME/aether/workspaces/`. Fuzzy-matched on name.
    /// Selecting one triggers the client to send `workspace/activate`. Distinct from the other
    /// kinds in that this picker is usable *before* a workspace is active (it's how the user
    /// gets one active in the first place) — every other picker requires `active_workspace`.
    Workspaces,
    /// The current buffer's LSP diagnostics (`Space Alt-d`), fuzzy-matched on the message. Scoped to
    /// one buffer (`PickerViewParams::buffer_id`), flat (no file header). Selecting one jumps to its
    /// position (via `FileAt`).
    Diagnostics,
    /// **Workspace-wide** LSP diagnostics (`Space d`) — the modal sibling of [`Diagnostics`], grouped
    /// by file. Pulled via `workspace/diagnostic` from every server in the active workspace that
    /// advertises it, merged with the open buffers' live diagnostics (so servers without pull, and
    /// unsaved edits, still show). A one-shot async snapshot taken on open (like [`References`]):
    /// the picker opens empty + `ticking` and is filled by the spawned resolve. Rows are
    /// [`PickerItem::Diagnostic`] carrying their file; selecting one jumps to the line (via
    /// `FileAt`).
    DiagnosticsWorkspace,
    /// The language servers for the active workspace, fuzzy-matched on server name. Unlike the
    /// other kinds this isn't a jump target: the client restarts the highlighted server in place
    /// (`Ctrl-r` → `lsp/restart_server`) and the list live-updates as statuses change.
    LspServers,
    /// References to the symbol at the cursor, gathered via the language server's
    /// `textDocument/references` (`PickerViewParams::buffer_id` scopes the request — the server
    /// resolves them against that buffer's cursor when the picker opens). Cross-file: each
    /// candidate is one reference location with a preview of its line. Fuzzy-matched on the
    /// preview text; selecting one jumps to its position (via `FileAt`). The candidate set is a
    /// one-shot LSP snapshot taken on open and preserved across scroll/resume re-views (like
    /// Diagnostics) — it doesn't live-update as the buffer changes.
    References,
    /// The symbols defined in the current buffer, gathered via the language server's
    /// `textDocument/documentSymbol` (`PickerViewParams::buffer_id` scopes the request). Buffer-local
    /// (every symbol lives in the picked buffer), fuzzy-matched on the symbol name. Hierarchical
    /// responses are flattened depth-first with a `depth` per item so the picker can indent nested
    /// members; selecting one jumps to its name position (via `FileAt`). Like References/Diagnostics
    /// it's a one-shot LSP snapshot taken on open and preserved across scroll/resume re-views.
    DocumentSymbols,
    /// The working-tree changes of the active workspace's repos, one row per hunk grouped by file
    /// (like Grep). Combined staged+unstaged vs HEAD: each hunk carries its [`DiffStage`] so the
    /// row colours like the inline diff. The candidate set is a one-shot snapshot taken on open —
    /// computed from disk + index, but using the *live buffer* text for any file currently open,
    /// so unsaved edits are reflected. Untracked files appear as a single whole-file addition.
    /// Selecting a hunk jumps to its anchor line (via `FileAt`). Each open is fresh — query, chips
    /// and highlight all reset — and lands on the hunk nearest the cursor
    /// ([`Self::centers_on_cursor`]), which resumes a review better than a saved highlight could:
    /// it tracks where you are now, and can't point at a hunk you've since staged away.
    GitChanges,
    /// The working-tree changes of a *single* buffer — the modal sibling of [`GitChanges`], opened
    /// by `Space Alt-c`. Locked to the buffer named by [`PickerViewParams::buffer_id`] (the active
    /// one, re-pointed each open), exactly how the [`Diagnostics`] picker locks to its buffer — the
    /// scope is intrinsic, not a filter chip, so there's nothing to add or remove. Its own state
    /// slot, independent of the workspace-wide [`GitChanges`]. Rows are the buffer's hunks, under the
    /// file's header.
    GitChangesFile,
    /// The keyboard-shortcut reference (`Space y`), fuzzy-matched on description, mode, and
    /// chord, with rows grouped under one section header per binding group (the grep-style
    /// grouping — matches keep candidate order so each group stays a contiguous run; the client
    /// ships the rows already bucketed by group). Unique among the kinds in that the *client*
    /// ships the candidate rows on open ([`PickerViewParams::keybindings`]) — the binding tables
    /// live in the client core, not on the server; the server only matches and windows them.
    /// Informational: rows aren't a jump target and there is no `PickerSelectResult` for them
    /// (Enter is a no-op — the picker stays open; Esc dismisses it). Like
    /// [`Workspaces`](Self::Workspaces) it's usable before a workspace is active.
    Keybindings,
    /// **Workspace-wide** symbol search (`Space Alt-o`) — the modal
    /// sibling of [`DocumentSymbols`](Self::DocumentSymbols). Answered by LSP `workspace/symbol`
    /// across the servers pinned by the workspace's declared *projects*, and
    /// deliberately **not** every ready server: a lazily-launched one is reaped when its last
    /// buffer closes, which would make the same query answer differently depending on what happens
    /// to be open.
    ///
    /// Query-driven like [`Grep`](Self::Grep) rather than a snapshot like `DocumentSymbols`: most
    /// servers return nothing for an empty query, so each `picker/query` re-issues the request and
    /// results stream in per server. Rows are [`PickerItem::Symbol`] carrying a `display_path`
    /// (symbols can come from dependencies outside every root), grouped by file.
    WorkspaceSymbols,
    /// The client's jumplist (`Space j`), one row per captured entry, fuzzy-matched on the entry's
    /// display text, grouped by the entries' carried source headers (file or section label; a
    /// grouped capture gives every entry one — out-of-workspace files get their absolute path as a
    /// label). Collapsible when grouped, which is a property of the *capture*, not the kind: a
    /// centred open expands the cursor-nearest entry's group and the rest sit collapsed, but a
    /// capture from the file-shaped pickers ([`Self::groups_in_jumplist`]) renders flat instead —
    /// see [`PickerViewResult::collapsible`]. Rebuilt from the live list on every open — nothing to
    /// resume, the backing list persists regardless. Selecting a row jumps to its entry (via
    /// `FileAt`); `Ctrl-j` *re-captures* the currently-filtered subset, narrowing the list in
    /// place.
    Jumplist,
    /// The local branches **and worktrees** of one repo (`Space g b`), fuzzy-matched on branch
    /// name, checked-out branches first then most-recently-committed. The repo is resolved
    /// server-side from [`PickerViewParams::buffer_id`] by the same rule `git/prepare_commit` uses,
    /// so a single-repo workspace never sees a chooser.
    ///
    /// **One list, keyed by branch.** A branch checked out in a worktree is not a separate row in a
    /// separate picker — it is a branch row carrying a [`BranchCheckout`]. That is what the
    /// separate worktree picker became: it listed the same branches under a different verb, and
    /// having two meant the branch picker could only *refuse* a branch held elsewhere, with the way
    /// forward under a different key in a list you were not looking at.
    ///
    /// The verbs split along one line — navigation never creates or destroys:
    ///
    /// - `Enter` — go to this branch. Has a checkout → open that tree; has none → `git/checkout`
    ///   here. One intent; git's state picks the mechanism.
    /// - `Ctrl-Enter` — the same, in a new window (GUI and web; the TUI has no second window).
    /// - `Ctrl-o` — create a worktree for this branch and *stay put*. Creation is a long, cancellable
    ///   checkout ([`crate::git::GitOperationKind::WorktreeAdd`]), not something to hang off a
    ///   navigation key.
    /// - `Ctrl-d` — remove the worktree if the row has one, else delete the branch; `Ctrl-Alt-d`
    ///   forces. The inverse of `Ctrl-o`, taking the outermost thing off first.
    ///
    /// Rows a branch cannot produce — a **detached** worktree, and a **prunable** one whose
    /// directory is gone — appear keyed by their admin name with `detached_at` set. Omitting them
    /// would make them unreachable, including for the removal that is the only thing left to do
    /// with a prunable entry.
    ///
    /// Not a jump target, like [`LspServers`](Self::LspServers): the client acts on the highlighted
    /// row, so there's no `PickerSelectResult` for it. A query naming no existing branch offers the
    /// synthetic "+ Create" row, which creates *and* switches — that is also what makes the picker
    /// usable in a repo with an unborn HEAD, where there are no branches to list at all.
    ///
    /// Deliberately per-repo rather than grouped across a multi-repo workspace, as every other
    /// `Space g` surface is: it keeps "one row = one repo's binding" true by construction.
    GitBranches,
    /// One repo's commit history (`Space g l`), newest first — the editor's `git log`. Rows are
    /// [`PickerItem::GitCommit`]; `Enter` opens the commit as a read-only virtual buffer
    /// (`git/show`), so it is not a file jump and has no `PickerSelectResult`.
    ///
    /// The query **filters what has been loaded** rather than re-running a search: matching is
    /// fuzzy over the composed haystack of subject, author and short hash, and no keystroke
    /// re-walks history. That makes the walk's cap part of the contract — see
    /// [`PickerViewResult::truncated`].
    GitLog,
    /// The commit history of a *single* file — the modal sibling of [`GitLog`] (`Space g Alt-l`),
    /// locked to [`PickerViewParams::buffer_id`] exactly as [`GitChangesFile`](Self::GitChangesFile)
    /// is. Its own state slot.
    ///
    /// "History of this path", not of the file's identity: a rename ends the history, because
    /// following renames is a separate mechanism (git's `--follow`) rather than a filter on the
    /// walk. Materially more expensive than [`GitLog`] too — the walk has to diff each commit
    /// against its parent to know whether the path was touched — which is why the cap counts
    /// commits *examined*, not rows produced.
    GitLogFile,
    /// The repo's stash entries (`Space g a`), newest first. Rows are [`PickerItem::GitStash`];
    /// `Enter` previews the entry as a read-only virtual buffer, exactly as the log picker shows a
    /// commit — a stash *is* a commit, so it costs no new read path.
    ///
    /// Not a jump target: the mutations (`Ctrl-p` pop, `Ctrl-Alt-p` apply, `Ctrl-d` drop) are RPCs
    /// the client fires against the highlighted row, so there is no `PickerSelectResult` for it.
    /// `refs/stash` is shared across worktrees, so a linked worktree lists the whole repo's.
    GitStash,
    /// What the repo's gutter and inline diff compare against (`Space Alt-i`). Rows are
    /// [`PickerItem::GitBaseline`]; `Enter` fires [`crate::git::GitSetBaseline`], so it is not a
    /// jump target and has no `PickerSelectResult`.
    ///
    /// Sits with the diff *toggle* on `Space i` rather than under `Space g`, for the reason the
    /// toggle does: both are ways of looking at the buffer you are already in, and neither writes
    /// anything. Plain toggles the view, Alt chooses what it shows.
    ///
    /// Two sections, because the rows are two different kinds of thing and conflating them is what
    /// makes the whole idea hard to hold: **working state** — `(index)` and `(saved)`, the states
    /// this checkout is in right now — then **revisions**, `HEAD` and the repo's branches. The
    /// parentheses carry the same distinction down to the row: everything unbracketed is something
    /// `git rev-parse` would accept, and everything bracketed is not.
    ///
    /// Rows are bare labels. The sections say what kind of answer each one is, which is what a
    /// per-row description was doing badly.
    ///
    /// The query doubles as revision entry: it matches names fuzzily *and* hashes by prefix (as
    /// [`GitLog`](Self::GitLog) does), because the set of things `git rev-parse` accepts is
    /// unbounded and a list cannot enumerate it.
    ///
    /// Opens highlighting the baseline in force, the way the branch picker opens on the checkout
    /// you are standing in — "where you are" is the selection, not a glyph on the row. So the row
    /// carries no `current` flag: nothing renders one.
    GitBaseline,
}

impl PickerKind {
    /// True for the two changes pickers — the workspace-wide [`Self::GitChanges`] and the
    /// buffer-locked [`Self::GitChangesFile`] — which share hunk rows / select behaviour but live
    /// in separate slots and build their candidates from a different scope.
    pub fn is_git_changes(self) -> bool {
        matches!(self, PickerKind::GitChanges | PickerKind::GitChangesFile)
    }

    /// Which input-history list this picker's query draws on for `Up`/`Down` recall, if any. Only
    /// Grep, because only Grep's query *is* a search: the other kinds fuzzy-filter a live candidate
    /// set (the workspace's files, the open buffers, a directory listing, the working tree's
    /// hunks), where recalling yesterday's string to narrow today's set means little. Grep instead
    /// re-runs a workspace walk, and the walk is the expensive, repeatable thing worth naming.
    pub fn history_kind(self) -> Option<HistoryKind> {
        (self == PickerKind::Grep).then_some(HistoryKind::Grep)
    }

    /// Whether this picker groups its rows into per-file sections, rendering a non-selectable file
    /// header above each file's first row (Grep hits and workspace Git changes). The single source of
    /// truth for the file-grouped layout: clients gate their header rendering, sticky-header pin,
    /// header clearance when revealing a row, and virtual-scroll row math on it. The buffer-locked
    /// [`Self::GitChangesFile`] is *not* here — it's a single file, so a header would just repeat it;
    /// it still centres on the cursor (see [`Self::centers_on_cursor`]).
    pub fn groups_by_file(self) -> bool {
        matches!(
            self,
            PickerKind::Grep | PickerKind::GitChanges | PickerKind::DiagnosticsWorkspace
        )
    }

    /// Whether this picker pins a **sticky group header** over the list's first visible row: the
    /// file-grouped kinds, plus Keybindings and Jumplist. References renders section labels but
    /// deliberately doesn't pin.
    ///
    /// The pin covers the top row, so the shells that draw it also owe a revealed row one row of
    /// clearance or it slides underneath. Lives here beside [`Self::groups_by_file`], which it is
    /// defined in terms of, rather than once per shell — it was previously spelled out
    /// character-for-character in both the terminal and native clients. The browser has no copy:
    /// it pins with CSS `position: sticky` and never asks the question.
    pub fn pins_group_header(self) -> bool {
        self.groups_by_file() || matches!(self, PickerKind::Keybindings | PickerKind::Jumplist)
    }

    /// Whether this kind's groups are collapsible: group headers are pushed as first-class
    /// *selectable rows* ([`PickerItem::Group`]) interleaved into the window — the whole
    /// window/offset/selection space counts rows, not bare items — with every group collapsed
    /// until something opens it, and any number of them open at once (`picker/set_group`).
    /// Selection is two-level: on a header it steps between groups, inside an open run it walks
    /// that run's items. The [`Self::groups_by_file`]
    /// kinds plus WorkspaceSymbols and Jumplist today, but deliberately a separate predicate: the
    /// two can diverge. The remaining grouped kinds (References, Keybindings) keep derived,
    /// non-selectable, always-expanded headers.
    ///
    /// **This is the default, not the authority.** [`Self::Jumplist`] is collapsible only when
    /// its captured entries carry groups — a capture from Files or Views is flat — so the
    /// server answers per view in [`PickerViewResult::collapsible`], which is what clients
    /// render from. Use this predicate only where no view response is in hand yet.
    pub fn collapsible(self) -> bool {
        matches!(
            self,
            PickerKind::Grep
                | PickerKind::GitChanges
                | PickerKind::DiagnosticsWorkspace
                | PickerKind::WorkspaceSymbols
                | PickerKind::Jumplist
        )
    }

    /// Whether this picker renders header rows above grouped runs of items (and the server
    /// pushes [`GroupSpan`]s describing them). A superset of [`Self::groups_by_file`]: the
    /// file-grouped kinds plus the section-labelled ones — References (a `Definition` section
    /// and a `References` section),
    /// Keybindings (one section per binding group), Jumplist and
    /// WorkspaceSymbols (file-or-label headers). The header
    /// *content* differs per kind — file path vs section label — and so does the header row's
    /// nature: for the [`Self::collapsible`] kinds it's a real, selectable window row
    /// ([`PickerItem::Group`]); for the rest it's a client-derived decoration interleaved
    /// above the run, gating header-clearance and virtual-scroll row math.
    pub fn renders_group_headers(self) -> bool {
        matches!(
            self,
            PickerKind::Grep
                | PickerKind::GitChanges
                | PickerKind::References
                | PickerKind::DiagnosticsWorkspace
                | PickerKind::WorkspaceSymbols
                | PickerKind::GitBaseline
                | PickerKind::Keybindings
                | PickerKind::Jumplist
        )
    }

    /// Whether `picker/view`'s `center_on_cursor` applies — the picker resolves "where you are"
    /// **server-side from the named buffer** and opens framed on it. The field carries a buffer id
    /// and the answer is per kind: the changes pickers take the hunk nearest the cursor, the
    /// jumplist its nearest entry, and the log and stash pickers the revision the buffer *is* — a
    /// `git/show` buffer holds one, so opening either from it lands on that row (a stash entry is a
    /// commit, so one rule covers both). (The name is a slight stretch for those: the cursor plays
    /// no part, the buffer's identity does.)
    ///
    /// This is what carries the weight now that no picker resumes its highlight
    /// ([`PickerReset`]): "where you are" is derived on every open, so it can't go stale the way a
    /// saved selection did. Grep is deliberately absent — it opens with no hits to frame, so there
    /// is nothing to resolve against.
    pub fn centers_on_cursor(self) -> bool {
        matches!(
            self,
            // The outline: land on the entry the cursor is *in*. Over a composed view that is the
            // change you are reading; over an ordinary buffer the enclosing symbol, resolved the
            // same way every other cursor-anchored kind resolves.
            PickerKind::DocumentSymbols
                | PickerKind::Jumplist
                | PickerKind::GitLog
                | PickerKind::GitLogFile
                | PickerKind::GitStash
        ) || self.is_git_changes()
    }

    /// Whether `jumplist/capture` (picker `Ctrl-j`) applies — the position-shaped kinds, whose rows
    /// are jump targets *into* a file, plus the file-shaped [`Self::Files`] and [`Self::Buffers`],
    /// whose rows are whole targets with no position (they capture as position-less entries and
    /// open where the cursor last sat). Excludes the non-jump kinds (Explorer, Workspaces,
    /// LspServers, Keybindings) — and [`Self::Shells`] / [`Self::Agents`], whose rows are not
    /// places in a file at all: a transcript row is a session you return to, and a captured set of
    /// them would be a jumplist you cannot step. Includes [`Self::Jumplist`] itself: capturing
    /// there replaces the list with the picker's currently-filtered subset — iterative narrowing.
    pub fn captures_to_jumplist(self) -> bool {
        matches!(
            self,
            PickerKind::Files
                | PickerKind::Buffers
                | PickerKind::Grep
                | PickerKind::Diagnostics
                | PickerKind::DiagnosticsWorkspace
                | PickerKind::References
                | PickerKind::DocumentSymbols
                | PickerKind::WorkspaceSymbols
                | PickerKind::Jumplist
        ) || self.is_git_changes()
    }

    /// Whether a jumplist captured *from* this kind carries group headers. The position-shaped
    /// sources group by file (or keep their section labels); the file-shaped ones
    /// ([`Self::Files`], [`Self::Buffers`]) have exactly one entry per target, so a per-file
    /// header would just repeat its own row — they capture ungrouped and the Jumplist picker
    /// renders them flat ([`PickerViewResult::collapsible`]). Grouping is all-or-nothing per
    /// capture: that uniformity is what keeps the collapsible row space's "every row is keyed"
    /// invariant true whenever the view *is* collapsible.
    ///
    /// [`Self::Jumplist`] isn't listed — a re-capture inherits whatever the entries already
    /// carry rather than consulting this.
    pub fn groups_in_jumplist(self) -> bool {
        self.captures_to_jumplist() && !matches!(self, PickerKind::Files | PickerKind::Buffers)
    }
}

/// Save/disk state of an open buffer, shown as a colour-coded dot in the view picker and
/// mirrored by the editor status bar. Precedence when several conditions hold (highest first):
/// deleted-on-disk → changed-on-disk → unsaved local edits → clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BufferDirtyState {
    /// Saved and matching disk — no dot.
    #[default]
    Clean,
    /// Unsaved local edits (`revision != saved_revision`).
    Unsaved,
    /// The file changed on disk underneath us.
    ExternallyModified,
    /// The file was removed on disk.
    ExternallyDeleted,
}

impl BufferDirtyState {
    /// `true` for the clean state — used to skip the field on the wire.
    pub fn is_clean(&self) -> bool {
        matches!(self, BufferDirtyState::Clean)
    }
}

/// The kind of a document symbol, mirroring the LSP `SymbolKind` enumeration. Carried by
/// [`PickerItem::Symbol`] so the clients can show a short type tag (and, later, a coloured icon)
/// next to each symbol. `Unknown` covers any value outside the LSP-defined 1..=26 range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    File,
    Module,
    Namespace,
    Package,
    Class,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    EnumMember,
    Struct,
    Event,
    Operator,
    TypeParameter,
    #[default]
    Unknown,
}

impl SymbolKind {
    /// Map an LSP `SymbolKind` integer (1..=26) to its variant. Anything else → `Unknown`.
    pub fn from_lsp(n: u64) -> SymbolKind {
        match n {
            1 => SymbolKind::File,
            2 => SymbolKind::Module,
            3 => SymbolKind::Namespace,
            4 => SymbolKind::Package,
            5 => SymbolKind::Class,
            6 => SymbolKind::Method,
            7 => SymbolKind::Property,
            8 => SymbolKind::Field,
            9 => SymbolKind::Constructor,
            10 => SymbolKind::Enum,
            11 => SymbolKind::Interface,
            12 => SymbolKind::Function,
            13 => SymbolKind::Variable,
            14 => SymbolKind::Constant,
            15 => SymbolKind::String,
            16 => SymbolKind::Number,
            17 => SymbolKind::Boolean,
            18 => SymbolKind::Array,
            19 => SymbolKind::Object,
            20 => SymbolKind::Key,
            21 => SymbolKind::Null,
            22 => SymbolKind::EnumMember,
            23 => SymbolKind::Struct,
            24 => SymbolKind::Event,
            25 => SymbolKind::Operator,
            26 => SymbolKind::TypeParameter,
            _ => SymbolKind::Unknown,
        }
    }

    /// The kind's full lowercase name, shown as a dim tag on the symbol row (e.g. `function`,
    /// `interface`, `struct`). Clients render it verbatim.
    pub fn label(self) -> &'static str {
        match self {
            SymbolKind::File => "file",
            SymbolKind::Module => "module",
            SymbolKind::Namespace => "namespace",
            SymbolKind::Package => "package",
            SymbolKind::Class => "class",
            SymbolKind::Method => "method",
            SymbolKind::Property => "property",
            SymbolKind::Field => "field",
            SymbolKind::Constructor => "constructor",
            SymbolKind::Enum => "enum",
            SymbolKind::Interface => "interface",
            SymbolKind::Function => "function",
            SymbolKind::Variable => "variable",
            SymbolKind::Constant => "constant",
            SymbolKind::String => "string",
            SymbolKind::Number => "number",
            SymbolKind::Boolean => "boolean",
            SymbolKind::Array => "array",
            SymbolKind::Object => "object",
            SymbolKind::Key => "key",
            SymbolKind::Null => "null",
            SymbolKind::EnumMember => "enum member",
            SymbolKind::Struct => "struct",
            SymbolKind::Event => "event",
            SymbolKind::Operator => "operator",
            SymbolKind::TypeParameter => "type parameter",
            SymbolKind::Unknown => "symbol",
        }
    }
}

/// What an agent conversation is doing, as the agents picker paints it. A badge, never a sort key
/// — the list is ordered by recency, so a turn starting re-paints a row rather than moving it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentRowState {
    /// Connected with nothing in flight — ready for a prompt.
    #[default]
    Idle,
    /// A turn is running. `activity` is the title of the tool call it is working through, when it
    /// has said; `None` while it is thinking with no tool named.
    Thinking {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activity: Option<String>,
    },
    /// Blocked on us: the agent has asked permission and the turn cannot proceed until it is
    /// answered (`Space v a` / `Space v d`).
    AwaitingPermission,
    /// No agent behind the conversation — restored from disk and not yet reconnected (an agent is
    /// a subprocess, and one starts on the first prompt), or one whose process has gone.
    Disconnected,
}

/// A pickable item. Tagged enum so different pickers can carry the data they need; match-index
/// highlighting rides in `match_indices` (char positions within the display string).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PickerItem {
    /// A file from the workspace walk. `relative_path` is path-relative to the root at
    /// `path_index` in the workspace's root list. The client formats the row by joining its own
    /// disambiguated root label with the relative path; the server stays out of presentation.
    File {
        path_index: u32,
        relative_path: String,
        /// Indices into `relative_path` (char offsets) covered by fuzzy matches. Empty on empty
        /// query. Note that the matcher haystack is `relative_path` alone — root labels are not
        /// part of the fuzzy match.
        #[serde(default)]
        match_indices: Vec<u32>,
        /// Git status used to colour a leading indicator, or `None` when clean / outside a repo.
        /// `.gitignore`d files don't appear in the Files picker at all (the walker skips them), so
        /// this is never `Ignored` here. Absent on the wire when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_status: Option<GitStatus>,
    },
    /// An open (or dormant) buffer. Identity is `view_id`; `buffer_id` is what it shows — stable
    /// across rename / Save-As, where the `display` string would change. `status` is captured at
    /// row-build time and may go stale between pushes (an active picker re-pushes on status
    /// transitions). One row per buffer: how a client is reading a markdown file is that client's
    /// presentation of it, not a view of its own, so nothing about it rides here.
    Buffer {
        buffer_id: BufferId,
        /// The row's **view**: what selecting the row presents and what closing it closes.
        #[serde(default)]
        view_id: crate::ViewId,
        /// What the row renders: workspace-relative path for file-backed buffers, `(scratch N)`
        /// for scratch buffers. Also the haystack the matcher scores against.
        display: String,
        /// Save/disk state, rendered as a colour-coded dot. Omitted on the wire (and defaulting
        /// to `Clean`) for a clean buffer — the common case.
        #[serde(default, skip_serializing_if = "BufferDirtyState::is_clean")]
        status: BufferDirtyState,
        /// Workspace-relative location (root index + path) for a file-backed buffer that lives inside
        /// a workspace root — mirrors `File`'s fields so the client can build an opener URL. Both are
        /// `None` for scratch buffers and for files outside every root (no `?file=` URL possible).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path_index: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relative_path: Option<String>,
        /// Indices into `display` (char offsets) covered by fuzzy matches.
        #[serde(default)]
        match_indices: Vec<u32>,
        /// True while the buffer is transient (auto-closes once hidden) — rendered in italics.
        /// Captured at row-build time, like `status`; an active picker re-pushes on changes.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        transient: bool,
    },
    /// One shell view, live or dormant. Identity is `view_id`.
    ///
    /// The fuzzy haystack is `"{title}  {cwd}  {last_command}"` (the empty parts elided) — that
    /// composition is a **wire contract**: the server scores against it and `match_indices` are
    /// char offsets into it, so a shell rendering the three fields separately must split the
    /// offsets the same way the server joined them.
    ///
    /// Status is a badge, never a sort key: `running` / `exit` / `elapsed_ms` describe the last
    /// run and change under an open picker without moving the row.
    Shell {
        /// The row's view: what selecting the row presents and what closing it closes.
        view_id: crate::ViewId,
        /// `Shell N` — the shell's own name, and the head of the haystack.
        title: String,
        /// Where the next command would run, already shortened to `~/...` when it is under the
        /// user's home — the same string the run boxes inside the shell wear. Shortened
        /// server-side rather than per shell: the server is the one that knows the home directory
        /// (the browser client does not), and the haystack must hold the same string the row shows
        /// or the fuzzy highlight would land off the text. Empty for a dormant row, whose directory
        /// is in a snapshot nothing has read yet.
        cwd: String,
        /// The last command this shell ran, or `None` for one that has run nothing yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_command: Option<String>,
        /// A run is in flight. Then `exit` and `elapsed_ms` are the *previous* run's, if any —
        /// which is why they are three fields and not one status enum.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        running: bool,
        /// The last finished run's exit code. `None` for a shell that has finished no run, and
        /// for one killed or truncated rather than exited.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit: Option<i32>,
        /// How long the last finished run took.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        elapsed_ms: Option<u64>,
        /// A session-restored shell nothing has opened yet: its transcript is on disk and the row
        /// materialises it on select. It has no live run, so `running` is always false here.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dormant: bool,
        /// Char offsets into the composed haystack described above.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One agent conversation, live or dormant. Identity is `view_id`.
    ///
    /// The fuzzy haystack is `"{title}  {agent}  {last_prompt}"` (the empty parts elided) — a
    /// **wire contract**, exactly as [`Self::Shell`]'s is.
    Agent {
        /// The row's view: what selecting the row presents and what closing it closes.
        view_id: crate::ViewId,
        /// `Agent N` — the conversation's own name, and the head of the haystack.
        title: String,
        /// The agent behind it, by its display name (`Claude Code`), not its id.
        agent: String,
        /// What it is doing, as a badge.
        #[serde(default)]
        state: AgentRowState,
        /// The last thing the user said to it, or `None` for a conversation with nothing in it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_prompt: Option<String>,
        /// A session-restored conversation nothing has opened yet: a record on disk with no
        /// subprocess behind it. Its state is always [`AgentRowState::Disconnected`].
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dormant: bool,
        /// Char offsets into the composed haystack described above.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One match found by the grep picker. Identity is `(path_index, relative_path, line, col)`.
    /// One row per match (a line with N matches produces N hits) — keeps `match_indices` a flat
    /// list within the preview, same as the other variants.
    GrepHit {
        /// Index into the workspace's root list — pairs with `relative_path` to recover the
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
    /// One hunk from the Git-changes picker. Identity is `(path_index, relative_path, hunk_index)`.
    /// Rows are grouped by file like grep hits: the client renders one per-file header (carrying a
    /// `-removed +added ~modified` summary it sums from the group) and, on each hunk row, the
    /// hunk's own `+added -removed` counts. The *change class* is read off the counts — both
    /// non-zero → modified, added-only → added, removed-only → deletion — so no separate kind rides
    /// the wire. `line` is the 0-based buffer line the hunk anchors to (the `FileAt` jump target).
    GitChange {
        /// Index into the workspace's root list — pairs with `relative_path` for the absolute path.
        /// Root-addressed like grep hits and workspace diagnostics, because this picker lists the
        /// *workspace's* changes: every row is a changed file under one of the roots, aggregated
        /// across however many repos those roots span. A change elsewhere in a root's repo is a git
        /// question, not a workspace one, and isn't listed here.
        path_index: u32,
        /// Path relative to root `path_index` (forward-slash separated). The fuzzy haystack + the
        /// group key the client renders a file header for.
        relative_path: String,
        /// 0-based index of this hunk within its file's change list — the identity tiebreak (the
        /// list is a snapshot, so positional identity is stable for the picker's lifetime).
        hunk_index: u32,
        /// 0-based buffer line the hunk anchors to: the first changed line for an add/modify, or
        /// the line a pure deletion sits above. The jump target.
        line: u32,
        /// Staged vs unstaged, mirroring the inline diff's bright/dim. A file can contribute hunks
        /// of both stages.
        #[serde(default, skip_serializing_if = "DiffStage::is_unstaged")]
        stage: DiffStage,
        /// New-side lines this hunk adds (`0` for a pure deletion).
        added: u32,
        /// Baseline lines this hunk removes (`0` for a pure addition).
        removed: u32,
        /// The changed line shown on the row: with no query, the hunk's first changed line; with a
        /// query, the first of the hunk's changed lines that contains it. Trimmed; the client
        /// truncates to fit.
        preview: String,
        /// Char offsets into `preview` covered by the query match. The query greps the hunk's diff
        /// *content* (substring, smartcase), not the file path — so this highlights the match within
        /// the previewed line, like a grep hit. Empty when there's no query.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One diagnostic in the current buffer. Identity is `(line, col, message)`. The matcher
    /// haystack is `message`; `match_indices` are char offsets into it. Selecting jumps to
    /// `(line, col)`. `(line, col)` is the range start; `(end_line, end_col)` the (exclusive) end —
    /// the picker shows the full range so distinct diagnostics that read alike are tellable apart.
    Diagnostic {
        /// The diagnostic's file, as workspace root index + root-relative path. Used by the
        /// workspace-wide picker to group by file; the buffer-scoped picker fills it with the buffer's
        /// own path (it renders flat, so the value is unused there).
        #[serde(default)]
        path_index: u32,
        #[serde(default)]
        relative_path: String,
        line: u32,
        col: u32,
        #[serde(default)]
        end_line: u32,
        #[serde(default)]
        end_col: u32,
        severity: DiagnosticSeverity,
        message: String,
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One configured workspace. Identity is `name` (the file stem of the workspace's TOML config).
    /// Selecting a `Workspace` returns a `PickerSelectResult::Workspace` and the client follows up
    /// with `workspace/activate`.
    Workspace {
        name: String,
        /// Number of open buffers in this workspace with unsaved edits (`revision != saved_revision`).
        /// `0` when the workspace has no dirty buffers (or isn't loaded). Absent on the wire when `0`.
        #[serde(default, skip_serializing_if = "is_zero")]
        unsaved: u32,
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
        /// Git status used to colour the entry, or `None` when clean / outside a repo. For a
        /// directory this is the highest-priority status among its descendants (folder
        /// aggregation). Absent on the wire when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_status: Option<GitStatus>,
    },
    /// One of the workspace's roots, shown in the Explorer's Roots mode (entered by `Alt-Backspace`
    /// at the top of a root). Identity is `path_index`; the client knows the absolute path via
    /// its own copy of `workspace_paths`. Match indices index into the root's basename — the
    /// disambiguator is client-derived and not part of the haystack.
    Root {
        path_index: u32,
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One reference location from `textDocument/references`. Identity is `(path, line, col)`.
    /// Cross-file, so it carries its own absolute `path` (fed into `view/open` on select) plus a
    /// server-computed `display_path` for the row label — workspace-relative when the file lives
    /// inside a root, otherwise the absolute path (references can point into dependencies / stdlib
    /// outside every root, where no `path_index`/root label applies). The matcher haystack is
    /// `preview`; `match_indices` are char offsets into it.
    Reference {
        /// Absolute canonical path to the file containing the reference.
        path: String,
        /// Row label: workspace-relative path when inside a root, else the absolute path.
        display_path: String,
        /// 0-based line number within the file.
        line: u32,
        /// 0-based byte offset of the reference within the line.
        col: u32,
        /// The text of the referenced line, trailing newline trimmed. Fuzzy haystack + preview.
        preview: String,
        /// True for the row that is the symbol's definition (the location `textDocument/definition`
        /// resolves to), false for an ordinary use. Drives the `Definition` / `References` section
        /// split: candidates are ordered definition-first, and clients open a section header above
        /// each run. At most one row is the definition; `false` for every row when the server can't
        /// resolve a definition (no `textDocument/definition` support, or it falls outside the
        /// returned references), in which case the list is a single `References` section.
        #[serde(default)]
        is_definition: bool,
        /// Char offsets into `preview` covered by fuzzy matches.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One symbol from `textDocument/documentSymbol`, scoped to the picked buffer. Identity is
    /// `(path, line, col)` — the symbol's name position. Carries its own absolute `path` (fed into
    /// `view/open` on select; always the picked buffer, but kept uniform with the other `FileAt`
    /// kinds). The matcher haystack is `name`; `match_indices` are char offsets into it. `detail`
    /// is the `DocumentSymbol` signature (shown dim), empty for flat servers; `depth` is the nesting
    /// level (0 = top-level) so the row can indent members under their container.
    Symbol {
        /// Absolute canonical path to the symbol's file.
        path: String,
        /// Row label: workspace-relative when the file lives inside a root, else the absolute path.
        /// Empty for [`PickerKind::DocumentSymbols`], where every symbol is in the picked buffer and
        /// the client already knows which file that is. Populated for
        /// [`PickerKind::WorkspaceSymbols`], where a symbol can come from a dependency or the
        /// standard library — outside every root, so no `path_index` applies (same treatment as
        /// [`PickerItem::Reference`]).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        display_path: String,
        /// 0-based line of the symbol's name.
        line: u32,
        /// 0-based byte offset of the symbol's name within the line.
        col: u32,
        /// The symbol name — fuzzy haystack + the row's primary label.
        name: String,
        /// LSP symbol kind, for the dim type tag (and future icon).
        symbol_kind: SymbolKind,
        /// The `DocumentSymbol` signature, shown dim after the name; empty for flat servers.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        detail: String,
        /// Nesting depth (0 = top-level), for indenting nested members.
        #[serde(default, skip_serializing_if = "is_zero")]
        depth: u32,
        /// True when this row is shown only as an *ancestor* of a match, to give tree context while
        /// filtering — not itself a match. Such rows render dim and are non-selectable (the client's
        /// navigation skips them). Always false on an empty query (the whole tree is shown) and for
        /// a row that is itself a match. Absent on the wire when false.
        #[serde(default, skip_serializing_if = "is_false")]
        context: bool,
        /// Char offsets into `name` covered by fuzzy matches. Empty for `context` rows.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One language server for the active workspace. Identity is `(language, workspace_root)` — the
    /// server key. Carries `status` so the client renders the health glyph; the matcher haystack
    /// is `name`. Not a jump target: the client acts on it via `lsp/restart_server`, so there's
    /// no corresponding `PickerSelectResult` variant.
    LspServer {
        name: String,
        language: String,
        /// Absolute workspace root — the stable identity half (with `language`).
        workspace_root: String,
        /// Display-only: `workspace_root` relative to its workspace root, or empty when the server
        /// is rooted *at* a workspace root (so single-root workspaces show no redundant path; only
        /// monorepo sub-roots get a disambiguating label). Server-computed.
        #[serde(default)]
        root_label: String,
        status: LspStatus,
        /// Work the server is currently doing (`$/progress`), so the picker row can show a busy
        /// indicator and the active operation(s). Empty when idle.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        progress: Vec<LspProgress>,
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One local branch in the [`PickerKind::GitBranches`] picker. Identity is `(repo_id, name)`;
    /// the matcher haystack is `name`. Not a jump target — the client acts on the row — so there's
    /// no corresponding `PickerSelectResult` variant, like [`PickerItem::LspServer`].
    GitBranch {
        /// Which repo this row belongs to, echoed onto every action the client fires.
        ///
        /// Redundant-looking (the server could re-resolve it) and load-bearing anyway: resolution
        /// runs off the *active buffer*, so a picker opened over repo A would have its checkout
        /// land in repo B if the user switched buffers — or a transient preview closed — while the
        /// list was up. Carrying the id makes each row name what it acts on, which is the rule
        /// every repo-level operation follows.
        repo_id: crate::git::RepoId,
        /// Shorthand name (`main`), not `refs/heads/main`.
        name: String,
        /// This branch is the repo's current HEAD.
        #[serde(default, skip_serializing_if = "is_false")]
        is_head: bool,
        /// The tip commit's summary line, shown dim after the name. Empty when unreadable.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        subject: String,
        /// Tip commit's author time as Unix seconds, rendered as a relative date. `0` when unknown.
        #[serde(default, skip_serializing_if = "is_zero_i64")]
        timestamp: i64,
        /// Configured upstream (`origin/main`); `None` for a branch never pushed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upstream: Option<String>,
        /// Commits ahead of / behind `upstream`. Both `0` without one. Computed locally rather than
        /// by contacting the remote, so they are only as fresh as the last `git/fetch` — advisory.
        #[serde(default, skip_serializing_if = "is_zero")]
        ahead: u32,
        #[serde(default, skip_serializing_if = "is_zero")]
        behind: u32,
        /// The checkout in this family holding this branch, when one does — including the tree the
        /// caller is standing in. Its presence is what makes this a *worktree* row: `Enter` opens
        /// that tree instead of moving HEAD, and `Ctrl-d` removes it rather than deleting the
        /// branch. Absent means no tree has this branch, so `Enter` checks it out here.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkout: Option<BranchCheckout>,
        /// Set when the row is a **detached** worktree rather than a branch: `name` is the tree's
        /// admin name and this is its short commit id (empty for a prunable entry, which has no
        /// head left to read). Such a row offers no checkout and no branch deletion.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detached_at: Option<String>,
        /// Char offsets into `name` covered by fuzzy matches.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One keyboard shortcut in the Keybindings picker — the [`KeybindingEntry`] the client
    /// shipped on open, echoed back with match highlighting. Identity is `(mode, keys, desc)`
    /// (a chord can be bound in several modes, and an Alt-pair fold can reuse a description).
    /// The matcher haystack is [`KeybindingEntry::haystack`] — the row segments composed in
    /// display order — and `match_indices` are char offsets into *that* string; the client
    /// rebuilds the same composition to map them back onto the segments it renders. Not a jump
    /// target: informational only, no `PickerSelectResult` variant.
    Keybinding {
        /// The row's group — rendered as the section header above the group's run, not on the
        /// row itself (and so not part of the match haystack).
        group: String,
        /// One-line description, e.g. `Delete word back`.
        desc: String,
        /// The mode the binding applies in: `Normal` / `Insert` / `Search` / `Application` /
        /// `Any` (the shared Ctrl-editing keys live in both Normal and Insert).
        mode: String,
        /// Display chord, e.g. `Ctrl-w`, `Space f ␣`.
        keys: String,
        /// Char offsets into [`KeybindingEntry::haystack`] covered by fuzzy matches.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// A group's header in the collapsible kinds ([`PickerKind::collapsible`]) — a first-class,
    /// selectable *row* in the pushed window, not a client-derived decoration like the other
    /// grouped kinds' headers. Identity is `header`'s group key. Groups start collapsed and any
    /// number can be expanded at once; an expanded group's items follow its header, a collapsed
    /// one renders as a bare header row. `Enter` on a header *is* a jump: `picker/select` resolves
    /// it server-side to the group's first item. Click is the disclosure gesture — it toggles
    /// expansion via `picker/set_group`.
    Group {
        header: GroupHeader,
        /// Items in the group's run — rendered on the row (the collapsed row's tell for
        /// what's inside), counted whether or not the run is expanded.
        count: u32,
        /// Whether this run's items follow it in the row space.
        #[serde(default, skip_serializing_if = "is_false")]
        expanded: bool,
    },
    /// One commit in the log picker ([`PickerKind::GitLog`] / [`PickerKind::GitLogFile`]).
    /// Identity is `hash`, which is unique and stable — unlike the positional identity the
    /// snapshot kinds use, a commit is the same commit however the list is filtered.
    GitCommit {
        /// The repo this commit belongs to, echoed onto `git/show`. Carried per row for the same
        /// reason the branch rows carry it: resolution runs off the active buffer, which can change
        /// while the list is up.
        repo_id: crate::git::RepoId,
        /// Full 40-char hash — what `git/show` receives.
        hash: String,
        /// Repo-relative path this history is *of*, for the file-locked log (`Space g Alt-l`);
        /// `None` for the whole-repo log, which is about no file in particular.
        ///
        /// Carried per row for the same reason `repo_id` is: it's resolved from the active buffer
        /// when the list is built, and that buffer can change while the list is up. The client
        /// echoes it back as `GitShowParams::focus_path`, so opening a commit from a file's history
        /// lands on *that* file's changes rather than at the top of a diff that may touch dozens.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// Abbreviated hash for display, as git prints it.
        short_hash: String,
        /// First line of the message. Empty when unreadable.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        subject: String,
        /// The refs pointing at this commit, rendered between the hash and the subject the way
        /// `git log --oneline --decorate` prints them — `(HEAD -> main, tag: v1.0, origin/main)`,
        /// each kind in its own colour. Empty for the overwhelming majority of commits, which is
        /// why the row spends no fixed width on it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        decorations: Vec<crate::git::CommitRef>,
        /// Char offsets into `subject` covered by fuzzy matches. The subject is the only thing
        /// matched *fuzzily*: the decorations aren't matched at all (they're what the commit is
        /// *labelled*, not what it says) and the hash is matched by prefix instead
        /// (`hash_match_len`).
        #[serde(default)]
        match_indices: Vec<u32>,
        /// How many leading characters of `short_hash` the query abbreviates, or `0`. A commit
        /// hash is an identifier, not prose: `git show 20a3a8a` means a **prefix**, so scattered
        /// fuzzy hits inside it are noise. The prefix is tested against the *full* hash, so
        /// pasting 12 or 40 characters finds the commit even though the row renders 7 — this
        /// caps at what's rendered, since it only says how much of the row to highlight.
        #[serde(default, skip_serializing_if = "is_zero")]
        hash_match_len: u32,
    },
    /// One stash entry ([`PickerKind::GitStash`]). Identity is `oid` — the stash commit's hash —
    /// because `stash@{n}` positions shift as entries are dropped, and acting on a stale position
    /// would hit the *wrong* stash rather than failing.
    GitStash {
        /// The repo this entry belongs to, echoed onto every action the row triggers.
        repo_id: crate::git::RepoId,
        /// Position at listing time, rendered as `stash@{n}` — display only. Every action
        /// re-resolves it server-side from `oid`.
        index: u32,
        /// The stash commit's hash: the row's identity, and what `git/show` previews.
        oid: String,
        /// git's own description — `WIP on main: abc1234 subject`, or the user's message. The
        /// fuzzy haystack, and the row's text.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        message: String,
        /// When the entry was made, Unix seconds; rendered as a relative date by the client.
        #[serde(default, skip_serializing_if = "is_zero_i64")]
        timestamp: i64,
        /// Char offsets into `message` covered by fuzzy matches.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One candidate diff baseline ([`PickerKind::GitBaseline`]). Identity is `choice`: two rows
    /// can share a label (a branch named `saved`) but never a choice.
    GitBaseline {
        /// The repo this row would re-baseline, echoed onto the `git/set_baseline` it fires.
        repo_id: crate::git::RepoId,
        /// What `Enter` sends. `None` is the "back to the default" row — the same `None` the RPC
        /// takes, so the row needs no special case at the call site.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice: Option<crate::git::GitBaselineChoice>,
        /// The row's text and fuzzy haystack: `(index)`, `(saved)`, `HEAD`, a branch name. The
        /// bracketed ones are not revisions — see [`PickerKind::GitBaseline`].
        label: String,
        /// Char offsets into `label` covered by fuzzy matches.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
    /// One captured entry in the Jumplist picker. Identity is `index` — the entry's position in the
    /// captured list, stable for the picker's lifetime (the list only changes via a re-capture,
    /// which resets the picker). Deliberately flat: one presentation-neutral line per entry,
    /// quickfix-style — the source pickers' richer row dressing (severity colours, stage tints, ±
    /// counts) doesn't carry over.
    JumplistEntry {
        /// 0-based position in the captured list.
        index: u32,
        /// 0-based line of the entry's landing position, rendered right-aligned and dim like a
        /// grep hit's line number (shells add 1 for display). `None` for a *whole-target* entry —
        /// a file or buffer captured without a position (the Files and view pickers), which
        /// opens wherever the cursor last sat — where a line number would be a fiction. Shells
        /// render nothing in its place.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        /// The source row's text (grep line, diagnostic message, reference preview, symbol
        /// name, hunk preview, file path) — rendered verbatim (leading whitespace stripped like
        /// a grep preview), and the fuzzy haystack.
        display: String,
        /// Char offsets into `display` covered by fuzzy matches.
        #[serde(default)]
        match_indices: Vec<u32>,
    },
}

/// One keyboard-shortcut row for the Keybindings picker, shipped *by the client* on open
/// ([`PickerViewParams::keybindings`]) — the binding tables live in the client core, so each
/// client's picker reflects exactly its own keymap; the server only fuzzy-matches and windows
/// the rows it was given. Field meanings mirror [`PickerItem::Keybinding`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeybindingEntry {
    pub group: String,
    pub desc: String,
    pub mode: String,
    pub keys: String,
}

impl KeybindingEntry {
    /// Whether `mode` is part of the rendered row (and therefore the haystack). Only Insert and
    /// Search qualify — Normal, the shared `Any` keys, and the Space-leader Application chords
    /// read as the default, so spelling their mode out on every row would be noise. The mode
    /// still always rides the wire: it's the row's identity half and what a future
    /// palette-execution layer would gate on.
    pub fn shows_mode(mode: &str) -> bool {
        matches!(mode, "Insert" | "Search")
    }

    /// The canonical string the server matches against and `match_indices` index into (char
    /// offsets): the row's segments in display order — `{desc} ({mode}) {keys}` when
    /// [`Self::shows_mode`], else `{desc} {keys}`. The group is *not* part of the haystack: rows
    /// render under a per-group section header (the grep-style grouping), not with an inline
    /// group label, so a group match would highlight nothing visible. Defined here — in the
    /// shared protocol crate — so the server's haystack and the client's index-to-segment
    /// mapping can never drift.
    pub fn haystack(&self) -> String {
        if Self::shows_mode(&self.mode) {
            format!("{} ({}) {}", self.desc, self.mode, self.keys)
        } else {
            format!("{} {}", self.desc, self.keys)
        }
    }
}

// ---- picker filters -----------------------------------------------------------------------------

/// How the grep query treats letter case. `Smart` is the default everywhere (case-insensitive
/// unless the query contains an uppercase letter, matching buffer search and the fuzzy pickers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseMode {
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

impl CaseMode {
    fn is_smart(&self) -> bool {
        matches!(self, CaseMode::Smart)
    }
}

/// The three pattern-matching options shared by the grep picker ([`PickerFilters`]) and buffer
/// search (`search/set`): how the pattern treats letter case, whether it matches only at word
/// boundaries, and whether the query is interpreted as a regex rather than a literal string. The
/// defaults (`Smart`, off, off) mean "literal, smartcase" — regex is opt-in — so an all-default
/// value is a no-op on the wire and equivalent to the field being absent.
///
/// Grep and the changes pickers derive these from their filter chips; buffer search toggles them in
/// the search prompt (`Alt-c` / `Alt-w` / `Alt-e`). Neither side carries them into the *next*
/// search: a picker open resets its chips ([`PickerReset::All`]) and a prompt open resets its
/// options, so the only way a past configuration comes back is recalling the entry that recorded
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MatchOptions {
    #[serde(default, skip_serializing_if = "CaseMode::is_smart")]
    pub case: CaseMode,
    #[serde(default, skip_serializing_if = "is_false")]
    pub whole_word: bool,
    /// Treat the query as a regular expression. Default (`false`) matches the query literally
    /// (the query is `regex::escape`d before compiling); `true` opts into full regex syntax.
    #[serde(default, skip_serializing_if = "is_false")]
    pub regex: bool,
}

impl MatchOptions {
    /// True when every option is at its default — used to skip the field on the wire.
    pub fn is_default(&self) -> bool {
        *self == MatchOptions::default()
    }
}

/// A path inside one of the workspace's roots — the scope filter chip. Addressed the same
/// way picker items are (`path_index` + root-relative path) so it survives root reordering no
/// worse than everything else does. There is deliberately no separate root filter: scoping to
/// a whole root is this with an empty `relative_path` (a directory always implies its root).
///
/// Usually a directory (a prefix scope: every file beneath it passes). When `is_file` is set the
/// `relative_path` names a single file and the scope matches that file exactly — what `Space
/// Alt-c` uses to pin the Git-changes picker to the active buffer. File scopes are produced only
/// for Grep / GitChanges (the Files picker stays directory-only — narrowing a file list to one
/// file is degenerate).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedPath {
    /// Index into the workspace's root list.
    pub path_index: u32,
    /// Path relative to `roots[path_index]`, forward-slash separated, no trailing slash.
    /// Empty scopes to the root itself.
    pub relative_path: String,
    /// When true, `relative_path` is a single file matched exactly rather than a directory
    /// prefix. Defaults to false, so the field is absent on the wire for the common dir scope.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_file: bool,
}

/// Result-narrowing filters, surfaced as chips in the clients. The full set is sent whole on every
/// `picker/query` — filters are small and "replace, don't diff" keeps the server stateless about
/// chip edits. Defaults mean "no filtering", so an all-default struct is equivalent to the field
/// being absent on the wire.
///
/// Which fields apply depends on the picker kind: Grep reads everything (including
/// `hide_untracked`); Files reads
/// `globs`/`directories`/`changed_only`/`hide_untracked`/`hide_hidden`; GitChanges reads
/// `globs`/`directories`/`hide_untracked` (it's inherently changed-only); Explorer reads
/// `hide_ignored`/`hide_hidden`/`changed_only`/`hide_untracked`; Jumplist reads
/// `globs`/`directories` (against each captured entry's file identity — and only when the capture
/// spans in-root files at all, see `PickerViewResult::path_filterable`); WorkspaceSymbols reads
/// `globs`/`directories` (against each symbol's file — `directories` additionally prunes which
/// projects' servers the query fans out to). Inapplicable fields are ignored, not errors — clients
/// only offer the chips that apply.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PickerFilters {
    /// Grep: how the search pattern treats case.
    #[serde(default, skip_serializing_if = "CaseMode::is_smart")]
    pub case: CaseMode,
    /// Grep: match only at word boundaries (ripgrep `-w`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub whole_word: bool,
    /// Grep / changes pickers: interpret the query as a regex. Default (`false`) matches literally
    /// (ripgrep `-F`); `true` opts into regex syntax.
    #[serde(default, skip_serializing_if = "is_false")]
    pub regex: bool,
    /// Grep: include `.gitignore`d files (ripgrep `--no-ignore`). Not offered for Files — the
    /// workspace index excludes ignored files at walk time and re-walking per toggle is too
    /// costly there. (The Explorer's equivalent is `hide_ignored`, inverted: its listing shows
    /// ignored entries by default.)
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_ignored: bool,
    /// Grep: include hidden (dot-) files (ripgrep `--hidden`). Not offered for Files — the Files
    /// index *includes* hidden files, so Files uses the inverted `hide_hidden` chip instead (like
    /// the Explorer). The `include_ignored` re-walk caveat still applies to ignored files there.
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_hidden: bool,
    /// Explorer only: drop `.gitignore`d entries from the listing. The explorer shows them by
    /// default (colour-tagged), unlike Files/Grep whose walks exclude them — so its chip hides
    /// rather than includes, keeping every field's default equal to current behavior.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_ignored: bool,
    /// Explorer and Files: drop hidden (dot-) entries — any path component starting with `.`. Both
    /// walk hidden entries in by default, so this chip *hides* rather than includes, keeping the
    /// default (off) equal to current behavior. (Explorer also colour-tags them; Files just lists.)
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_hidden: bool,
    /// All kinds: restrict to files with uncommitted changes (any non-clean, non-ignored Git
    /// status). For Explorer, directories with changed descendants stay visible.
    #[serde(default, skip_serializing_if = "is_false")]
    pub changed_only: bool,
    /// Grep / Files / GitChanges / Explorer: drop untracked entries (no HEAD blob *and* no index
    /// blob — a wholly-new file git isn't tracking yet; a staged-new file has an index blob and
    /// stays). Orthogonal to `changed_only`: on the Grep/Files/Explorer pickers the two compose
    /// (changed + tracked-only, or all-tracked on its own), and GitChanges — inherently
    /// changed-only — uses it to show only diffs to tracked files. Hide-only, defaulting off, like
    /// `hide_ignored`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_untracked: bool,
    /// Grep + Files: ripgrep-style include globs, matched against the root-relative path.
    /// A leading `!` makes a glob an exclude. With at least one non-`!` glob present, a file
    /// must match some include glob; independently, it must match no exclude glob.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub globs: Vec<String>,
    /// Grep + Files: restrict to files under *any* of these scopes (union semantics, matching how
    /// multiple include globs combine; a whole root is an entry with an empty `relative_path` —
    /// there is no separate root filter). A directory scope passes everything beneath it; a
    /// [`ScopedPath::is_file`] scope passes only that exact file. Repeatable, like `globs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directories: Vec<ScopedPath>,
}

impl PickerFilters {
    /// True when every field is at its default — i.e. no narrowing is in effect. Used to skip
    /// the field on the wire and to short-circuit filter passes server-side.
    pub fn is_default(&self) -> bool {
        *self == PickerFilters::default()
    }

    /// The pattern-matching subset (case / whole-word / regex) — the options that also apply to
    /// buffer search. Used when a grep result primes a buffer's search so the primed search
    /// matches the same way the grep did.
    pub fn match_options(&self) -> MatchOptions {
        MatchOptions {
            case: self.case,
            whole_word: self.whole_word,
            regex: self.regex,
        }
    }

    /// The inverse: a filter set carrying only match options, no scoping. How the buffer search —
    /// which has options but nothing to scope — stores its configuration in an input-history entry.
    pub fn from_match_options(options: MatchOptions) -> Self {
        PickerFilters {
            case: options.case,
            whole_word: options.whole_word,
            regex: options.regex,
            ..PickerFilters::default()
        }
    }
}

// ---- picker/view --------------------------------------------------------------------------------

/// Whether this `picker/view` is a **fresh open** or a **re-view within one open** — not a per-kind
/// policy. Every kind opens with [`PickerReset::All`]; every scroll refetch and navigation step
/// *within* an open sends [`PickerReset::Keep`], which is what stops the window cycle from
/// re-running the search underneath it. Closing the picker ([`PickerHide`]) releases its state, so
/// there is never anything on the far side of a close for `Keep` to resume.
///
/// It used to be a per-kind table (`PickerKind::reset_on_open`), with Grep and the two changes
/// pickers keeping some or all of their state across opens. That went away in stages: state
/// surviving an open is state the user can't see and didn't ask for, and it silently changes what
/// the next open shows. Getting back to a previous result set is the jumplist's job (`Ctrl-j`) and
/// getting back to a previous query is the input history's (`Up`) — both explicit acts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickerReset {
    /// Resume from whatever the prior `view`/`query` cycle left behind — a re-view within one open.
    #[default]
    Keep,
    /// Wipe the whole slot — query, candidates, filters, selection — so the picker opens exactly as
    /// it would on the first open of the session. What every fresh open sends.
    All,
}

/// Attach to a picker, declare the scroll window to be pushed, and start receiving updates.
/// `reset` says how much persisted state (query, candidates, filters) is wiped first; the picker
/// otherwise resumes from whatever the prior `view`/`query` cycle left behind. If `center_on` is
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
    /// How much persisted state to wipe before attaching: [`PickerReset::All`] on a fresh open,
    /// [`PickerReset::Keep`] on a re-view within one.
    #[serde(default)]
    pub reset: PickerReset,
    /// First row of the window the client wants pushed. Ignored when `center_on` is set.
    #[serde(default)]
    pub offset: u32,
    pub limit: u32,
    /// If set, the server picks an `effective_offset` such that this item is inside the returned
    /// window (used on resume to restore the client's prior highlight). If the item is no longer
    /// in the results, the server falls back to `offset: 0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_on: Option<PickerItem>,
    /// Cursor-anchored open: when set, the server resolves this buffer's cursor to the nearest
    /// candidate and uses it as the effective `center_on`, overriding any explicit `center_on` the
    /// client passed. The resolution is per-kind ([`PickerKind::centers_on_cursor`]): **GitChanges**
    /// picks the hunk in the buffer's own file nearest at-or-after the cursor line (else that
    /// file's last hunk); **Jumplist** the nearest captured entry. The resolved item is echoed back
    /// in `effective_center_on` so the client can use it as its highlight. This is what makes
    /// `Space c` / `Space j` land on "where you are" in the list. No-op for the other kinds
    /// (including Grep, which opens with an empty result set), and when the buffer has no matching
    /// candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_on_cursor: Option<BufferId>,
    /// Explorer only: absolute path of the directory to list. `None` means "keep whatever
    /// directory the picker last listed; default to the first workspace root on first open".
    /// Ignored when `explorer_roots` is set, and for other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_path: Option<String>,
    /// Explorer only: when true, list the workspace's roots instead of a filesystem directory.
    /// Wins over `directory_path` when both are set. The client uses this to enter "Roots
    /// mode" by pressing `Alt-Backspace` at the top of a root.
    #[serde(default, skip_serializing_if = "is_false")]
    pub explorer_roots: bool,
    /// Diagnostics only: the buffer to list diagnostics for. Required when opening the Diagnostics
    /// picker (a wiping [`PickerReset`]); `None` on scroll re-views (the candidate snapshot is kept).
    /// Also carries the active buffer for [`PickerViewParams::from_selection`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    /// The **view** the listing kinds list for, when it differs from [`Self::buffer_id`].
    ///
    /// Listings go view-wide: `Space d` and `Space c` in a composed view answer for everything the
    /// view is showing, not just the hunk the cursor happens to be in. `buffer_id` cannot say that —
    /// it is the focused element's buffer, and in a working-changes view that is one file of many.
    ///
    /// Carried *beside* `buffer_id` rather than replacing it, because the other kinds that take a
    /// buffer want the focused one: `GitBranches` and `GitLog` resolve a repo from it, `GitLogFile`
    /// takes a path, and a from-selection grep slices that buffer's selection. Sending one id for
    /// two questions is what made this ambiguous in the first place.
    ///
    /// `None` from a client that doesn't send it, and for an ordinary view, where the view *is* the
    /// buffer and the fan-out would be over a set of one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_id: Option<crate::ViewId>,
    /// Grep only (`Space Alt-/`): derive the initial query from `buffer_id`'s selection — the
    /// grep equivalent of `Alt-/`. The server slices the selection text, installs it as the
    /// query (literally, like the rest of grep), and kicks off the search in this same call;
    /// the derived query and its `generation` come back in the result for the client to adopt.
    /// Requires `buffer_id`; ignored for other kinds and when the selection is empty.
    #[serde(default, skip_serializing_if = "is_false")]
    pub from_selection: bool,
    /// Replace the persisted filters before attaching. `None` keeps whatever the prior
    /// `view`/`query` cycle left behind (the default, no-op filters on a fresh open). `Some` is how a client opens a picker pre-scoped (e.g. `Space Alt-f` /
    /// `Space Alt-/` seeding the buffer's directory chip).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<PickerFilters>,
    /// Keybindings only: the candidate rows, shipped on a fresh open (the binding tables live
    /// client-side — see [`KeybindingEntry`]). `None` on scroll/resume re-views: the server keeps
    /// the previously-shipped set, like the Diagnostics snapshot. Ignored for other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keybindings: Option<Vec<KeybindingEntry>>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
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
    /// `center_on` unless `center_on_cursor` resolved (and overrode it) — in which
    /// case this is the resolved hit, so the client can set its local highlight to match.
    /// `None` when no centering happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_center_on: Option<PickerItem>,
    /// Explorer only: the canonical absolute path of the committed *anchor* directory (the one
    /// navigation moves between), not the query-derived peek listing. `None` for the other picker
    /// kinds and in Roots mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_path: Option<String>,
    /// Explorer only: the canonical absolute path of the anchor's parent, if it's still inside
    /// the workspace's access boundary. `None` when at (or above) a workspace root, and `None` for
    /// the other picker kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory_parent: Option<String>,
    /// The filters now in effect — all-default after a [`PickerReset::All`] open, the resumed set
    /// for the changes pickers, the caller's seed for a seeded open. Echoed so the client can
    /// rebuild its chip row from the server's copy, which is the authority; the client's chip list
    /// is a render of this, not a parallel store.
    #[serde(default, skip_serializing_if = "PickerFilters::is_default")]
    pub filters: PickerFilters,
    /// Jumplist only: whether the captured list is worth path-scoping — it spans more than one
    /// file and at least one entry sits inside a workspace root. Gates the dir/glob chips
    /// client-side (a single-file capture is all-or-nothing, like `GitChangesFile`'s intrinsic
    /// scope; an all-external capture has no root-relative paths for scopes/globs to match).
    /// Always `false` for the other kinds — their chip availability is static per kind.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub path_filterable: bool,
    /// The log pickers: the walk stopped at its cap rather than reaching the repo's root commit,
    /// so older history is **not** in the candidate set. Clients must surface it — the query
    /// filters what was loaded, so a silent cap makes "no matches" indistinguishable from "your
    /// match is older than the cap".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Whether *this view* renders as collapsible groups: group headers as selectable
    /// [`PickerItem::Group`] rows, two-level selection, the row space counting headers. Normally a
    /// per-kind constant ([`PickerKind::collapsible`]) — the second data gate after
    /// [`Self::path_filterable`], and for the same reason: a Jumplist captured from a *file-shaped*
    /// picker (Files, Views) has one entry per file and nothing to group by, so it renders flat,
    /// while the same kind captured from Grep renders grouped. Clients must read this rather than
    /// the kind predicate; the kind is only the pre-response default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collapsible: bool,
    /// The initial result window (items at `effective_offset`). Mirrors the `picker/update` push
    /// the server also emits, but riding the response lets the client render items atomically with
    /// adopting `generation`/`effective_offset`. The separate push can arrive *before* this
    /// response, when the client's `generation`/`offset` still differ and its staleness guard
    /// discards it. `None` only when there is no subscribed window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<PickerUpdateParams>,
}

// ---- picker/query -------------------------------------------------------------------------------

/// Shortest grep query that actually runs a search. Below this the server installs the query (so
/// the input shows what you typed) but doesn't walk the workspace — a one-character pattern matches
/// most of it. Shared so the client can apply the same floor without a round-trip: it's what
/// decides whether a query is worth recording to the input history when the picker closes.
pub const MIN_GREP_QUERY_LEN: usize = 2;

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
    /// The full filter set in effect for this query, sent whole on every change. A filter
    /// change is a query change: the client bumps `generation` and the server re-runs (for
    /// Grep, respawning the search worker). All-default (the serde default when absent) means
    /// no narrowing.
    #[serde(default, skip_serializing_if = "PickerFilters::is_default")]
    pub filters: PickerFilters,
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
/// (via `view/open`). For `Buffers` / `Shells` / `Agents`, the `view_id` the client should present
/// (via `view/open { view_id }`) — one variant serves all three, because presenting a row is the
/// same act whatever the row is. For `Grep`, the canonical absolute path plus the position to
/// jump to (client opens via `view/open { jump_to }`). The picker handler doesn't perform the
/// switch itself — that's the client's job, same as the file browser flow.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PickerSelectResult {
    File {
        /// Absolute canonical path on disk.
        path: String,
    },
    /// A view to present: a picker row's, or a pathless jumplist entry's.
    View { view_id: crate::ViewId },
    FileAt {
        /// Absolute canonical path on disk.
        path: String,
        /// Position to land the cursor on. Coordinates may be stale if the file changed since the
        /// hit was recorded; the server clamps in `view/open` when applying.
        position: LogicalPosition,
        /// When `Some`, the *other* end of a selection to establish on open — anchor at this
        /// position, cursor at `position`. The client forwards it as `view/open { jump_to_anchor }`.
        /// `None` (the default) lands a plain point cursor. The outline picker uses it to land a
        /// symbol's identifier selected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<LogicalPosition>,
    },
    /// A place inside the view **already open**: focus this element, and land the cursor in it.
    ///
    /// Distinct from [`Self::ViewAt`] because a composed view's cursor is only drawn inside the
    /// *focused* element. Moving it into another element without focusing there puts it outside the
    /// window that renders it: the jump resolves, travels, is applied — and nothing moves on screen,
    /// which is precisely how this presented.
    ///
    /// The client does the focus-then-set-cursor pair it already does for a click, and for the same
    /// reason: setting a cursor without focusing first applies the line to whichever element holds
    /// focus, which in a patch is a different file.
    ViewElement {
        element: crate::viewport::FieldId,
        /// The buffer that element windows — the cursor is set on it, not on the view's document.
        buffer_id: BufferId,
        position: LogicalPosition,
        /// The view, reopened, when nothing was showing it. The client adopts this *before* seating
        /// — the element is an index into that view's tree, so focusing it means nothing until the
        /// view is on screen. Absent when the view was already showing, which is the common case
        /// and the only one that used to exist.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open: Option<Box<crate::view::ViewOpenResult>>,
    },
    /// A jumplist row whose view no longer holds it: the change was staged, committed or reverted
    /// since the capture. Nowhere to land and nowhere else to go — the row is a place *in that
    /// view* — so the client says so rather than opening the row's file in an editor, which is
    /// where such a row used to take you. `open` is the view when it had to be brought back to
    /// look, for the client to show.
    Gone {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open: Option<Box<crate::view::ViewOpenResult>>,
    },
    /// Present an open view *and* land the cursor somewhere in it — [`Self::View`] with a
    /// position, and the pathless counterpart of [`Self::FileAt`].
    ///
    /// There is no file to reopen: a generated patch was materialised rather than loaded, so its
    /// rows can only be addressed by the view. The client presents it via
    /// `view/open { view_id, jump_to }` exactly as it would for a file.
    ViewAt {
        view_id: crate::ViewId,
        position: LogicalPosition,
    },
    /// A workspace was selected. The client follows up with `workspace/activate` to switch.
    Workspace { name: String },
}

/// The checkout in a repo family holding one branch — the annotation that turns a branch row into a
/// worktree row in the merged branch picker ([`PickerKind::GitBranches`]).
///
/// This replaced a flat `checked_out_in` / `checked_out_in_main` pair, which could only answer
/// "somewhere else has it". Acting on the row needs more: the admin name to bind or remove the tree
/// by, and the lock/prune state to know whether removal is even offered. Those are never
/// individually meaningful — a row either has a checkout or it doesn't — so they travel as one.
///
/// Includes the tree the caller is standing in ([`Self::is_current`]), unlike the pair it replaced.
/// A branch-keyed list has to render the branch you are on as the worktree row it is; "already
/// here" and "another tree has it" are different sentences, not the same refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchCheckout {
    /// Working directory of the checkout.
    pub path: String,
    /// It is the repo's **main** working tree rather than a linked worktree. It has no admin name
    /// and cannot be removed, so a row carrying this offers no `Ctrl-d`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_main: bool,
    /// Admin name of the linked worktree — **empty for the main tree**, matching
    /// [`crate::git::GitWorktreeRow::name`] and what `workspace/bind_worktree` reads as "unbind".
    ///
    /// Carried rather than re-derived from the row's name, because the two drift: `git worktree
    /// add` names a tree once, a later checkout inside it moves HEAD without renaming anything, and
    /// `git worktree move` relocates the directory while leaving the admin name untouched (git has
    /// no rename verb at all). So a tree admin-named `feature-auth` can be sitting on `main`, in a
    /// directory called neither.
    ///
    /// **Not shown on a branch row** — the shells render a bare `⧉`. The name is machinery: the
    /// user never chose it, git lets it go stale, and it names nothing else on screen. It surfaces
    /// only where it is a row's *identity* rather than an annotation (a detached or prunable tree,
    /// whose row `name` this is) or where a sentence has room to say which tree
    /// (`{branch} is already in worktree {name}`). Still needed on the wire: it is what
    /// `workspace/bind_worktree` and worktree removal act on.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worktree: String,
    /// This is the checkout the caller is standing in.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_current: bool,
    /// `git worktree lock` is holding it — never removed, not even with force.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub locked: bool,
    /// The admin entry outlived its directory (`rm -rf` rather than `git worktree remove`).
    /// Rendered dimmed; the only condition under which pruning is offered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prunable: bool,
}

// ---- picker/hide --------------------------------------------------------------------------------

/// The user closed this picker: stop pushing updates *and* release its state — query, chips,
/// results, highlight. Nothing resumes from it; the next `view` is a fresh open
/// ([`PickerReset::All`]).
///
/// Releasing at the close rather than at the next open is what stops the work: a `grep` walk
/// streams into its picker until the generation moves past the one it was spawned with, so a
/// dismissed search used to keep scanning the whole workspace, filling a list nobody would see.
/// Closing bumps the generation, and the walk drops out at its next batch.
///
/// No payload — the client owns the highlight, and doesn't persist it either.
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

// ---- group spans ----------------------------------------------------------------------------

/// What a group's header row shows. Presentation-neutral, like the items: `File` carries the
/// workspace-relative location and the client formats it (root labels are client-derived);
/// `Label` is rendered verbatim (References' `Definition` / `References` sections, a
/// keybinding group's name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GroupHeader {
    /// The group is a file (grep hits, git changes, workspace diagnostics). The client composes the
    /// label from the root's disambiguated name and `relative_path` — see
    /// `aether-client/src/labels.rs`, which owns how a path is printed.
    File {
        path_index: u32,
        relative_path: String,
    },
    /// The group is a named section (references, keybindings).
    Label { label: String },
}

/// One group run within a pushed window: the items from `start` (0-based index into the
/// window's `items`, NOT the absolute ranked index) up to the next span (or the window's end)
/// render under `header`. The server is the single source of group boundaries — clients render
/// spans verbatim instead of re-deriving keys from item fields.
///
/// Invariant for the grouped kinds: a non-empty window's first span always has `start == 0`,
/// *including* when the window begins mid-group — the split group's header is repeated so the
/// window is self-describing (this replaces the clients' old "synthesize a header above the
/// first row" convention). Ungrouped kinds send no spans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSpan {
    pub start: u32,
    pub header: GroupHeader,
    /// Collapsible kinds only: the run's item count and whether it is expanded — the same
    /// decoration the run's [`PickerItem::Group`] row carries, so a sticky pin standing in for a
    /// scrolled-off header renders identically to the row itself. `None` for the non-collapsible
    /// grouped kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
}

/// A group run's place in a collapsible picker's row space: its header's absolute row plus its
/// item count — an expanded run's item rows occupy `[header_row + 1, header_row + len]`, and a
/// collapsed run reports `len: 0` (its header row is still real). Rides `picker/update` for the
/// *focused* run so the client can do exact, local navigation math (clamping item-level moves to
/// the run, telling item rows from header rows by interval) even when the run overflows the
/// fetched window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRunRows {
    /// Absolute row (in the collapsible row space) of the run's header.
    pub header_row: u32,
    /// Item rows following the header: the run's length when it's expanded, `0` when collapsed.
    pub len: u32,
}

// ---- picker/set_group ---------------------------------------------------------------------------

/// Expand, collapse or move between the groups of a collapsible picker
/// ([`PickerKind::collapsible`]). Groups start collapsed and any number can be open at once; the
/// server holds the expansion set plus a *focused* group — the one group-stepping is relative to
/// and the one whose geometry rides the reply. Expansion is sticky across query changes (a group
/// you opened re-opens if it comes back), and reset when the picker closes.
///
/// The server recomputes the row space, replies with the focused run's place in it, and pushes a
/// fresh window through the normal `picker/update` path; the client picks its landing row from the
/// reply and lets its offset/generation guards + refetch reconcile the window, so response/push
/// arrival order doesn't matter.
pub struct PickerSetGroup;
impl RpcMethod for PickerSetGroup {
    const NAME: &'static str = "picker/set_group";
    type Params = PickerSetGroupParams;
    type Result = PickerSetGroupResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerSetGroupParams {
    /// Which picker to act on (the client's open one). Must be a collapsible kind.
    pub kind: PickerKind,
    pub action: PickerGroupAction,
}

/// What [`PickerSetGroup`] does to the expansion state. Groups are addressed by their `header` —
/// both sides derive the same group key from it, so no index rides the wire to go stale across a
/// re-rank.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PickerGroupAction {
    /// Expand the group and focus it (`Alt-l` on a header, a click on a collapsed one).
    /// Idempotent: expanding an already-open group just moves the focus onto it.
    Expand { header: GroupHeader },
    /// Collapse the group and focus it (`Alt-h`, a click on an expanded header).
    Collapse { header: GroupHeader },
    /// Focus the run adjacent to the focused one (`Forward` = the next, `Backward` = the
    /// previous), resolved server-side so it works past the client's fetched window. A step past
    /// the first/last run is a no-op (`run: None`) — group navigation stops at the ends, like the
    /// jumplist's `]`/`[`. `expand` opens the group landed on: false for the group-level
    /// `Alt-j`/`Alt-k`, true for an item-level spill over a run edge (which walks into the
    /// neighbour's items and so must open it, leaving the run it came from open too).
    Step { direction: Direction, expand: bool },
    /// `Alt-a`: expand every group, or — when none is collapsed — collapse every group. The
    /// server decides which way, since only it sees the whole run list.
    ToggleAll,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerSetGroupResult {
    /// The focused run's place in the reshaped row space — its header row + item count (`0` when
    /// it ended up collapsed). The client picks its own landing row from it: the header for
    /// group-level navigation and collapses, the run's first item for a descend, its first/last
    /// item for an item-level spill over a run edge (which needs the new run's *length* at reply
    /// time). `None` when nothing changed: the named group is no longer in the result set (it
    /// re-ranked away mid-flight), a `Step` ran off the ends, or the kind doesn't collapse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<GroupRunRows>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerUpdateParams {
    pub kind: PickerKind,
    pub generation: u64,
    pub offset: u32,
    /// The window's items, or `None` to keep the client's current window — only the counts /
    /// `ticking` changed. The server sends `None` (throttled) as a streaming grep's candidate count
    /// climbs but the visible window — already full and, being insertion-ordered, stable — doesn't,
    /// so it isn't re-serialized on every batch. The window is re-sent (`Some`) only while it's
    /// still filling or when a scroll moves it. `Some(vec![])` is a genuinely empty result set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<PickerItem>>,
    pub total_matches: u32,
    pub total_candidates: u32,
    /// True while the matcher is still consuming candidates (walk in progress, or matcher hasn't
    /// quiesced after a query change). The client may use this to show a spinner.
    pub ticking: bool,
    /// The window's group runs (see [`GroupSpan`]), in order. Present (non-empty) for the
    /// grouped kinds whenever `items` is; meaningless on a count-only tick (`items: None`),
    /// where the client keeps its current window's spans. Empty for the flat kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<GroupSpan>,
    /// Grouped kinds only: the display-row index (items interleaved with one header row per
    /// group) of this window's first item, accounting for the header rows above it. Lets a
    /// client virtual-scroll a list that renders group headers without its spacer
    /// under-counting those rows. Display rows are an abstract uniform unit — each client maps
    /// them to its own measure (terminal lines, `ROW_H`, a measured pixel height).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_offset: Option<u32>,
    /// Grouped kinds only: total display rows in the whole result set (`total_matches` + the
    /// number of groups). Sizes the client's virtual-scroll spacer so every item (incl. the
    /// last group's) is reachable. `None` for the flat kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_display_rows: Option<u32>,
    /// Collapsible kinds only: where the *focused* run sits in the row space (see
    /// [`GroupRunRows`]) — the group the client's selection is working in, and the one
    /// `Step`ping is relative to. Several runs can be expanded at once, so this is the one the
    /// client needs geometry for, not "the expanded one". Describes the same result set as `items`,
    /// so like the spans it's meaningless on a count-only tick (`items: None`), where the client
    /// keeps its current value. `None` for the other kinds and while the result set is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_run: Option<GroupRunRows>,
    /// A server-resolved highlight to adopt when this push lands — currently the DocumentSymbols
    /// picker's cursor-enclosing symbol, computed on the async fill (the picker opens before the
    /// `textDocument/documentSymbol` round-trip returns, so this can't ride the `picker/view`
    /// response's `effective_center_on` the way the synchronous kinds do). The client treats it
    /// like `effective_center_on`: sets it as the pending centre and reveals it. `None` on every
    /// other push (the common case). The item is one of this window's rows, so the client's
    /// identity match finds it without a refetch. Boxed to keep `PickerUpdateParams` (and the
    /// `Event`/message enums that embed it) small — `PickerItem` is a large tagged union.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_on: Option<Box<PickerItem>>,
    /// Explorer only: true when the directory the query *peeks into* (the anchor joined with the
    /// query's path part) doesn't exist as an in-workspace directory — e.g. mid-typing a not-yet-
    /// created path. The client uses it to decide whether a trailing-slash query offers
    /// "+ Create directory" (offered only when the directory is missing — you can't tell from the
    /// listing alone, since a peek lists the directory's *contents*). Absent on the wire (and for
    /// non-Explorer kinds) when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub explorer_peek_missing: bool,
}

impl PickerUpdateParams {
    /// The window's items as a slice — empty for a count-only tick (`items: None`), where the
    /// client keeps its current window. Convenience for readers that don't distinguish "unchanged"
    /// from "empty result set"; consumers that do (e.g. `apply_update`) match on `items` directly.
    pub fn items(&self) -> &[PickerItem] {
        self.items.as_deref().unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which kinds pin a sticky group header. Every shell that draws one reads this, and the two
    /// that used to spell it out themselves owed a revealed row a row of clearance to match — so a
    /// kind moving in or out of this set is a rendering change in all of them at once.
    #[test]
    fn only_the_grouped_kinds_pin_a_header() {
        assert!(PickerKind::Grep.pins_group_header());
        assert!(PickerKind::GitChanges.pins_group_header());
        assert!(PickerKind::DiagnosticsWorkspace.pins_group_header());
        assert!(PickerKind::Keybindings.pins_group_header());
        assert!(PickerKind::Jumplist.pins_group_header());
        // Renders section labels, but deliberately doesn't pin one.
        assert!(!PickerKind::References.pins_group_header());
        assert!(!PickerKind::Files.pins_group_header());
        // A single file needs no header repeating its own name.
        assert!(!PickerKind::GitChangesFile.pins_group_header());
    }
}
