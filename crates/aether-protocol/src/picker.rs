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
    /// Open buffers, ordered by most-recently-used. The current buffer sits at position 0 and
    /// selecting it is a no-op switch.
    Buffers,
    /// Workspace-wide content search. Each candidate is a single match on a single line; the
    /// query *is* the search (no fuzzy filtering on a pre-built candidate set), so query changes
    /// throw out the prior candidates and start a fresh scan. Persisted hits stay around across
    /// `hide`/`view` so the user can step through results — they may be stale relative to the
    /// file on disk after editing, and that's accepted (jumps clamp to the current line bounds).
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
    /// Selecting a hunk jumps to its anchor line (via `FileAt`). Reset on each open (not preserved).
    GitChanges,
    /// The working-tree changes of a *single* buffer — the modal sibling of [`GitChanges`], opened
    /// by `Space Alt-c`. Locked to the buffer named by [`PickerViewParams::buffer_id`] (the active
    /// one, re-pointed each open), exactly how the [`Diagnostics`] picker locks to its buffer — the
    /// scope is intrinsic, not a filter chip, so there's nothing to add or remove. Its own state
    /// slot, independent of the workspace-wide [`GitChanges`]. Rows are the buffer's hunks, under the
    /// file's header.
    GitChangesFile,
    /// The keyboard-shortcut reference (`Space /`), fuzzy-matched on description, mode, and
    /// chord, with rows grouped under one section header per binding group (the grep-style
    /// grouping — matches keep candidate order so each group stays a contiguous run; the client
    /// ships the rows already bucketed by group). Unique among the kinds in that the *client*
    /// ships the candidate rows on open ([`PickerViewParams::keybindings`]) — the binding tables
    /// live in the client core, not on the server; the server only matches and windows them.
    /// Informational: rows aren't a jump target and there is no `PickerSelectResult` for them
    /// (Enter just closes the picker). Like [`Workspaces`](Self::Workspaces) it's usable before
    /// a workspace is active.
    Keybindings,
}

impl PickerKind {
    /// True for the two changes pickers — the workspace-wide [`Self::GitChanges`] and the
    /// buffer-locked [`Self::GitChangesFile`] — which share hunk rows / select behaviour but live
    /// in separate slots and build their candidates from a different scope.
    pub fn is_git_changes(self) -> bool {
        matches!(self, PickerKind::GitChanges | PickerKind::GitChangesFile)
    }

    /// Whether this picker saves its highlight + query on hide/select so the next open
    /// resumes the prior state. Only Grep does — its candidate set is the result of a
    /// (potentially slow) workspace scan and dropping it on every reopen would be wasteful.
    /// The others reset on each open so the picker stays contextual: Files and Buffers reset
    /// the query so each open is a fresh search; Explorer resets back to the active buffer's
    /// directory so it acts like "show me where I am" rather than a persistent file-manager
    /// session.
    pub fn preserves_state(self) -> bool {
        // Grep keeps its (expensive) search results; the changes pickers keep their query +
        // filters so a re-open resumes. All rebuild their candidate set on re-view regardless
        // (the changes pickers re-snapshot the working tree), and the server wipes a client's
        // pickers on workspace switch, so persisted state never leaks across workspaces.
        matches!(self, PickerKind::Grep) || self.is_git_changes()
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

    /// Whether this picker interleaves non-selectable header rows above grouped runs of items.
    /// A superset of [`Self::groups_by_file`]: the file-grouped kinds plus the section-labelled
    /// ones — References (a `Definition` section and a `References` section) and Keybindings (one
    /// section per binding group). The header *content* differs per kind — file path vs section
    /// label — but the header *row* accounting is identical, so clients gate header-clearance and
    /// virtual-scroll row math on this single predicate.
    pub fn renders_group_headers(self) -> bool {
        matches!(
            self,
            PickerKind::Grep
                | PickerKind::GitChanges
                | PickerKind::References
                | PickerKind::DiagnosticsWorkspace
                | PickerKind::Keybindings
        )
    }

    /// Whether `picker/view`'s `center_on_cursor` applies — the picker can resolve a result near
    /// the buffer's cursor and open framed on it (Grep's nearest hit, the changes pickers' nearest
    /// hunk). Wider than [`Self::groups_by_file`]: it includes the headerless buffer-locked changes
    /// picker, which still wants to land on "where you are".
    pub fn centers_on_cursor(self) -> bool {
        matches!(self, PickerKind::Grep) || self.is_git_changes()
    }
}

/// Save/disk state of an open buffer, shown as a colour-coded dot in the buffer picker and
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
    /// An open buffer. Identity is `buffer_id` — stable across rename / Save-As, where the
    /// `display` string would change. `status` is captured at row-build time and may go stale
    /// between pushes (an active picker re-pushes on status transitions).
    Buffer {
        buffer_id: BufferId,
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
        /// True for a *dormant* buffer — a file restored from the persisted workspace session that
        /// hasn't been loaded into memory yet (no rope/tree-sitter/LSP). It appears in the picker
        /// greyed out; selecting it materializes the real buffer (via `buffer/open`, which the
        /// server intercepts by id). Never set together with `transient`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dormant: bool,
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
        unsaved_buffers: u32,
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
    /// Cross-file, so it carries its own absolute `path` (fed into `buffer/open` on select) plus a
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
    /// `buffer/open` on select; always the picked buffer, but kept uniform with the other `FileAt`
    /// kinds). The matcher haystack is `name`; `match_indices` are char offsets into it. `detail`
    /// is the `DocumentSymbol` signature (shown dim), empty for flat servers; `depth` is the nesting
    /// level (0 = top-level) so the row can indent members under their container.
    Symbol {
        /// Absolute canonical path to the buffer's file.
        path: String,
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
/// Grep derives these from its filter chips; buffer search toggles them in the search prompt
/// (`Alt-c` / `Alt-w` / `Alt-e`). When a grep result primes a buffer's search the grep options
/// ride along (`BufferOpenParams::prime_search_options`) so the primed search matches the same
/// way the grep that found it did.
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

/// Result-narrowing filters, surfaced as chips in the clients (see `docs/picker-filters.md`).
/// The full set is sent whole on every `picker/query` — filters are small and "replace, don't
/// diff" keeps the server stateless about chip edits. Defaults mean "no filtering", so an
/// all-default struct is equivalent to the field being absent on the wire.
///
/// Which fields apply depends on the picker kind: Grep reads everything (including
/// `hide_untracked`); Files reads `globs`/`directories`/`changed_only`/`hide_untracked`; GitChanges
/// reads `globs`/`directories`/`hide_untracked` (it's inherently changed-only); Explorer reads
/// `hide_ignored`/`hide_hidden`/`changed_only`/`hide_untracked`. Inapplicable fields are ignored,
/// not errors — clients only offer the chips that apply.
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
    /// Grep: include hidden (dot-) files (ripgrep `--hidden`). Same Files caveat as
    /// `include_ignored`; the Explorer's equivalent is `hide_hidden`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_hidden: bool,
    /// Explorer only: drop `.gitignore`d entries from the listing. The explorer shows them by
    /// default (colour-tagged), unlike Files/Grep whose walks exclude them — so its chip hides
    /// rather than includes, keeping every field's default equal to current behavior.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hide_ignored: bool,
    /// Explorer only: drop hidden (dot-) entries from the listing. Same default rationale as
    /// `hide_ignored`.
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
    /// Cursor-anchored open: when set, the server resolves this buffer's cursor to the nearest
    /// candidate and uses it as the effective `center_on`, overriding any explicit `center_on` the
    /// client passed. The resolution is per-kind: **Grep** picks the nearest cached hit (at-or-after
    /// the cursor's leading selection edge in walker order, wrapping to the first hit); **GitChanges**
    /// picks the hunk in the buffer's own file nearest at-or-after the cursor line (else that file's
    /// last hunk). The resolved item is echoed back in `effective_center_on` so the client can use it
    /// as its highlight. This is what makes `Space g` / `Space c` land on "where you are" in the
    /// result list. No-op for the other kinds, and when the buffer has no matching candidate.
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
    /// picker (`reset: true`); `None` on resume/scroll re-views (the candidate snapshot is kept).
    /// Also carries the active buffer for [`PickerViewParams::from_selection`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<BufferId>,
    /// Grep only (`Space Alt-g`): derive the initial query from `buffer_id`'s selection — the
    /// grep equivalent of `Alt-/`. The server slices the selection text, installs it as the
    /// query (literally, like the rest of grep), and kicks off the search in this same call;
    /// the derived query and its `generation` come back in the result for the client to adopt.
    /// Requires `buffer_id`; ignored for other kinds and when the selection is empty.
    #[serde(default, skip_serializing_if = "is_false")]
    pub from_selection: bool,
    /// Replace the persisted filters before attaching. `None` keeps whatever the prior
    /// `view`/`query`/`hide` cycle left behind (the default, no-op filters on first open or
    /// after `reset`). `Some` is how a client opens a picker pre-scoped (e.g. `Space Alt-f` /
    /// `Space Alt-g` seeding the buffer's directory chip).
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
    /// The filters now in effect (all-default on first open or after `reset`). Echoed so a
    /// resuming client can rebuild its chip row, the same way `query` restores the input text.
    #[serde(default, skip_serializing_if = "PickerFilters::is_default")]
    pub filters: PickerFilters,
    /// The initial result window (items at `effective_offset`). Mirrors the `picker/update` push
    /// the server also emits, but riding the response lets the client render items atomically with
    /// adopting `generation`/`effective_offset`. The separate push can arrive *before* this
    /// response, when the client's `generation`/`offset` still differ and its staleness guard
    /// discards it — most visible on a Grep resume, where the picker reopens showing the restored
    /// query but no rows. `None` only when there is no subscribed window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<PickerUpdateParams>,
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
        /// When `Some`, the *other* end of a selection to establish on open — anchor at this
        /// position, cursor at `position`. The client forwards it as `buffer/open { jump_to_anchor }`.
        /// `None` (the default) lands a plain point cursor. The outline picker uses it to land a
        /// symbol's identifier selected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<LogicalPosition>,
    },
    /// A workspace was selected. The client follows up with `workspace/activate` to switch.
    Workspace {
        name: String,
    },
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
/// - If the current buffer's workspace-relative path is in the hits, find the next/previous match
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
    /// Also open the target (transient, jumped to the hit, nav origin recorded, search
    /// primed with the grep query) and return it in `opened` — the whole `<`/`>` client
    /// chain in one round-trip (docs/protocol-composites.md, J).
    #[serde(default)]
    pub open: bool,
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
    /// The grep search's match options (case / whole-word / literal). Echoed alongside `query` so
    /// the client's primed search state matches how the grep that found the hits ran. Defaults
    /// (regex, smartcase) when absent.
    #[serde(default, skip_serializing_if = "MatchOptions::is_default")]
    pub options: MatchOptions,
    /// With `open`: the target, fully opened (transient, at `position`, search primed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<crate::buffer::BufferOpenResult>,
}

/// Move a picker's selection to the next/previous *section* boundary — the grouping is per-kind:
/// Grep groups hits into contiguous per-file runs (jump to the next/previous file's first hit);
/// DocumentSymbols groups by top-level unit (jump to the next/previous depth-0 symbol). Computed
/// server-side against the full result list, so it works even when the target is past the client's
/// over-fetched window — the client then frames the returned item via `picker/view { center_on }`.
///
/// `Forward` → the next section. `Backward` → the start of the *current* section, or, if the
/// selection is already there, the start of the previous section (vim-`{` feel). Returns `None`
/// when there's no further section in that direction (already at the first / last).
pub struct PickerSectionJump;
impl RpcMethod for PickerSectionJump {
    const NAME: &'static str = "picker/section_jump";
    type Params = PickerSectionJumpParams;
    type Result = Option<PickerItem>;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PickerSectionJumpParams {
    /// Which picker to act on (the client's open one).
    pub kind: PickerKind,
    /// The selection's current absolute index in the result list (`offset + selected`).
    pub from_index: u32,
    pub direction: Direction,
}

// ---- group spans ----------------------------------------------------------------------------

/// What a group's header row shows. Presentation-neutral, like the items: `File` carries the
/// workspace-relative location and the client formats it (root labels are client-derived);
/// `Label` is rendered verbatim (References' `Definition` / `References` sections, a
/// keybinding group's name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GroupHeader {
    /// The group is a file (grep hits, git changes, workspace diagnostics).
    File { path_index: u32, relative_path: String },
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
