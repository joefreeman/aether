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
        /// What the row renders: project-relative path for file-backed buffers, `(scratch N)`
        /// for scratch buffers. Also the haystack the matcher scores against.
        display: String,
        /// Save/disk state, rendered as a colour-coded dot. Omitted on the wire (and defaulting
        /// to `Clean`) for a clean buffer — the common case.
        #[serde(default, skip_serializing_if = "BufferDirtyState::is_clean")]
        status: BufferDirtyState,
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
        /// True while the buffer is transient (auto-closes once hidden) — rendered in italics.
        /// Captured at row-build time, like `status`; an active picker re-pushes on changes.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        transient: bool,
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
    /// `(line, col)`. `(line, col)` is the range start; `(end_line, end_col)` the (exclusive) end —
    /// the picker shows the full range so distinct diagnostics that read alike are tellable apart.
    Diagnostic {
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
        /// Git status used to colour the entry, or `None` when clean / outside a repo. For a
        /// directory this is the highest-priority status among its descendants (folder
        /// aggregation). Absent on the wire when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_status: Option<GitStatus>,
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
    /// One reference location from `textDocument/references`. Identity is `(path, line, col)`.
    /// Cross-file, so it carries its own absolute `path` (fed into `buffer/open` on select) plus a
    /// server-computed `display_path` for the row label — project-relative when the file lives
    /// inside a root, otherwise the absolute path (references can point into dependencies / stdlib
    /// outside every root, where no `path_index`/root label applies). The matcher haystack is
    /// `preview`; `match_indices` are char offsets into it.
    Reference {
        /// Absolute canonical path to the file containing the reference.
        path: String,
        /// Row label: project-relative path when inside a root, else the absolute path.
        display_path: String,
        /// 0-based line number within the file.
        line: u32,
        /// 0-based byte offset of the reference within the line.
        col: u32,
        /// The text of the referenced line, trailing newline trimmed. Fuzzy haystack + preview.
        preview: String,
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
        /// Work the server is currently doing (`$/progress`), so the picker row can show a busy
        /// indicator and the active operation(s). Empty when idle.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        progress: Vec<LspProgress>,
        #[serde(default)]
        match_indices: Vec<u32>,
    },
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
/// boundaries, and whether the query is a literal string rather than a regex. The defaults
/// (`Smart`, off, off) mean "regex, smartcase" — the long-standing buffer-search behavior — so an
/// all-default value is a no-op on the wire and equivalent to the field being absent.
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
    #[serde(default, skip_serializing_if = "is_false")]
    pub fixed_string: bool,
}

impl MatchOptions {
    /// True when every option is at its default — used to skip the field on the wire.
    pub fn is_default(&self) -> bool {
        *self == MatchOptions::default()
    }
}

/// A directory inside one of the project's roots — the `dir:` filter chip. Addressed the same
/// way picker items are (`path_index` + root-relative path) so it survives root reordering no
/// worse than everything else does. There is deliberately no separate root filter: scoping to
/// a whole root is this with an empty `relative_path` (a directory always implies its root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedPath {
    /// Index into the project's root list.
    pub path_index: u32,
    /// Path relative to `roots[path_index]`, forward-slash separated, no trailing slash.
    /// Empty scopes to the root itself.
    pub relative_path: String,
}

/// Result-narrowing filters, surfaced as chips in the clients (see `docs/picker-filters.md`).
/// The full set is sent whole on every `picker/query` — filters are small and "replace, don't
/// diff" keeps the server stateless about chip edits. Defaults mean "no filtering", so an
/// all-default struct is equivalent to the field being absent on the wire.
///
/// Which fields apply depends on the picker kind: Grep reads everything; Files reads
/// `globs`/`directories`/`changed_only`; Explorer reads `hide_ignored`/`hide_hidden`/
/// `changed_only`. Inapplicable fields are ignored, not errors — clients only offer the chips
/// that apply.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PickerFilters {
    /// Grep: how the search pattern treats case.
    #[serde(default, skip_serializing_if = "CaseMode::is_smart")]
    pub case: CaseMode,
    /// Grep: match only at word boundaries (ripgrep `-w`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub whole_word: bool,
    /// Grep: treat the query as a literal string, not a regex (ripgrep `-F`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub fixed_string: bool,
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
    /// Grep + Files: ripgrep-style include globs, matched against the root-relative path.
    /// A leading `!` makes a glob an exclude. With at least one non-`!` glob present, a file
    /// must match some include glob; independently, it must match no exclude glob.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub globs: Vec<String>,
    /// Grep + Files: restrict to files under *any* of these directories (union semantics,
    /// matching how multiple include globs combine; a whole root is an entry with an empty
    /// `relative_path` — there is no separate root filter). Repeatable, like `globs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directories: Vec<ScopedPath>,
}

impl PickerFilters {
    /// True when every field is at its default — i.e. no narrowing is in effect. Used to skip
    /// the field on the wire and to short-circuit filter passes server-side.
    pub fn is_default(&self) -> bool {
        *self == PickerFilters::default()
    }

    /// The pattern-matching subset (case / whole-word / literal) — the options that also apply to
    /// buffer search. Used when a grep result primes a buffer's search so the primed search
    /// matches the same way the grep did.
    pub fn match_options(&self) -> MatchOptions {
        MatchOptions {
            case: self.case,
            whole_word: self.whole_word,
            fixed_string: self.fixed_string,
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
    /// Replace the persisted filters before attaching. `None` keeps whatever the prior
    /// `view`/`query`/`hide` cycle left behind (the default, no-op filters on first open or
    /// after `reset`). `Some` is how a client opens a picker pre-scoped (e.g. a future
    /// "grep this directory").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<PickerFilters>,
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
    },
    /// A project was selected. The client follows up with `project/activate` to switch.
    Project {
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
}

impl PickerUpdateParams {
    /// The window's items as a slice — empty for a count-only tick (`items: None`), where the
    /// client keeps its current window. Convenience for readers that don't distinguish "unchanged"
    /// from "empty result set"; consumers that do (e.g. `apply_update`) match on `items` directly.
    pub fn items(&self) -> &[PickerItem] {
        self.items.as_deref().unwrap_or(&[])
    }
}
