//! Server-side picker state. One `PickerState` per `(ClientId, PickerKind)`; the server owns
//! the query, the ranked match list, and the subscribed window. The client owns the highlighted
//! row (it persists the last item locally and uses `view { center_on }` to restore on resume).
//!
//! Matching uses `nucleo_matcher` directly — sort once on query change, slice the window on
//! demand. No background ticking; for v1 the walk is the only slow step and that lives in
//! `WorkspaceIndex`. When the workspace grows enough to warrant streaming results during the
//! walk, switch to `nucleo::Nucleo` and a per-picker tick task.

use crate::workspace_index::CachedFile;
use aether_protocol::cursor::Direction;
use aether_protocol::lsp::{LspProgress, LspStatus};
use aether_protocol::picker::{
    BufferDirtyState, CaseMode, GroupHeader, GroupSpan, KeybindingEntry, MatchOptions,
    PickerFilters, PickerItem, PickerKind, PickerSelectResult, PickerUpdateParams,
};
use aether_protocol::viewport::{DiagnosticSeverity, DiffStage};
use aether_protocol::{BufferId, LogicalPosition};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::sync::Arc;

/// One buffer-picker candidate. Built fresh per `picker/view` / `picker/query` from
/// `ServerState.buffers` + per-client MRU. The buffer set changes often enough that we don't
/// pin an `Arc` snapshot like the file picker does — just rebuild.
#[derive(Debug, Clone)]
pub struct BufferCandidate {
    pub buffer_id: BufferId,
    /// Display string used for both rendering and fuzzy matching. Workspace-relative for
    /// file-backed buffers; `(scratch N)` for scratch buffers.
    pub display: String,
    pub status: BufferDirtyState,
    /// Workspace-relative location (root index + path) when the buffer is a file inside a root;
    /// `None` for scratch buffers / out-of-root files. Sent so the client can build an opener URL.
    pub path: Option<(u32, String)>,
    /// Buffer is transient (auto-closes once hidden) — the row renders in italics.
    pub transient: bool,
}

/// One workspace-picker candidate. Built fresh per `picker/view` from
/// `config::list_workspace_names()` — the configured-workspaces set changes only via the user
/// editing `~/.config/aether/workspaces/*.toml` and we re-list on each open anyway.
#[derive(Debug, Clone)]
pub struct WorkspaceCandidate {
    pub name: String,
    /// Open buffers in this workspace with unsaved edits, counted when the candidate is built.
    /// `0` for a workspace with no loaded/dirty buffers.
    pub unsaved_buffers: u32,
}

/// One explorer-picker entry. Children of the picker's `current_path` directory; rebuilt by
/// each `picker/view` (Explorer always re-lists, like Buffers always rebuilds — directories
/// can change underneath us and there's no point caching them).
#[derive(Debug, Clone)]
pub struct ExplorerEntry {
    pub name: String,
    pub is_dir: bool,
    /// Git status for colouring, or `None` when clean / outside a repo. Directories carry the
    /// aggregated status of their descendants (see [`crate::git::dir_statuses`]).
    pub git_status: Option<aether_protocol::git::GitStatus>,
}

/// The directory listing the explorer picker is currently matching against. `path` is the
/// canonical absolute path of the listing; `parent` is the parent's canonical path *if it's
/// still inside the workspace boundary* (otherwise `None`, meaning Alt-h is a no-op).
///
/// With path-peeking (a query like `src/foo`), this is the *peeked* directory — `anchor` joined
/// with the query's path part — not necessarily the committed [`ExplorerAnchorInfo`]. Entries the
/// user sees (and `select_result` resolves a file against) live here.
#[derive(Debug, Clone)]
pub struct ExplorerCandidates {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<ExplorerEntry>,
}

/// The Explorer's *committed* directory — the one navigation (Enter on a dir, Alt-h, root select)
/// moves between, distinct from the query-derived peek listing. The query is interpreted relative
/// to this: `query` up to the last `/` selects which directory under the anchor to list (the
/// peek), the remainder prefix-filters it. Echoed to the client as `directory_path`/`_parent`, so
/// the breadcrumb and the client's "+ Create" base stay pinned to the anchor while peeking.
#[derive(Debug, Clone)]
pub struct ExplorerAnchorInfo {
    pub path: String,
    pub parent: Option<String>,
}

/// Split an Explorer query into `(path_part, filter_part)` at the last `/`. The path part (no
/// trailing slash) selects the directory to list relative to the anchor; the filter part
/// prefix-matches that directory's entries. No `/` → the whole query is the filter, in the
/// anchor itself. `src/` → list `src`, empty filter; `src/ma` → list `src`, filter `ma`.
pub fn explorer_query_split(query: &str) -> (&str, &str) {
    match query.rfind('/') {
        Some(i) => (&query[..i], &query[i + 1..]),
        None => ("", query),
    }
}

/// One grep-picker candidate. One per *match* (a line with N matches yields N candidates), in
/// the order ripgrep emitted them — walker order, then line order within each file.
#[derive(Debug, Clone)]
pub struct GrepHitCandidate {
    /// Index into the workspace's root list this file lives under.
    pub path_index: u32,
    /// Path relative to `roots[path_index]`. Stored separately from `abs_path` so the picker can
    /// render without re-resolving against workspace roots on every push.
    pub relative_path: String,
    /// Absolute canonical path. Returned via `PickerSelectResult::FileAt` for the client to feed
    /// into `buffer/open`.
    pub abs_path: String,
    /// 0-based line number within the file.
    pub line: u32,
    /// 0-based byte offset of the match's first byte within the line.
    pub col: u32,
    /// Byte length of the match within the line. Needed alongside `col` so the server can
    /// reconstruct the match's end position for "is the cursor exactly on this match?" checks.
    pub match_byte_len: u32,
    /// The full text of the matching line (trailing newline trimmed). Used as the haystack for
    /// match-indices and as the preview shown in the picker row.
    pub preview: String,
    /// Char offsets into `preview` covered by the match.
    pub match_indices: Vec<u32>,
}

/// One Git-changes-picker candidate — a single hunk of one changed file. The candidates are
/// grouped by file (contiguous runs, like grep), in `(path_index, relative_path)` order with the
/// hunks of each file in anchor-line order; `hunk_index` is that position within the file. Built
/// once on `picker/view` from the workspace's working-tree changes (combined staged+unstaged vs
/// HEAD), so positional identity is stable for the picker's lifetime.
#[derive(Debug, Clone)]
pub struct GitChangeCandidate {
    /// Index into the workspace's root list this file lives under.
    pub path_index: u32,
    /// Path relative to `roots[path_index]` (forward-slash). The file-group key.
    pub relative_path: String,
    /// Absolute path, returned via `PickerSelectResult::FileAt` for `buffer/open`.
    pub abs_path: String,
    /// Position of this hunk within its file's change list (0-based, anchor order).
    pub hunk_index: u32,
    /// 0-based buffer line the hunk anchors to — the jump target.
    pub line: u32,
    /// Staged vs unstaged, for the row's colour.
    pub stage: DiffStage,
    /// New-side lines added (`0` for a pure deletion).
    pub added: u32,
    /// Baseline lines removed (`0` for a pure addition).
    pub removed: u32,
    /// The hunk's changed lines, trimmed — new-side (added/modified) lines first, then the removed
    /// baseline lines. The query regex-matches against these line-by-line (grep-style, content not
    /// path), and the row previews the first non-blank line by default, or the first line that
    /// matches the query.
    pub lines: Vec<String>,
    /// File-level: the file has no HEAD blob and no index blob (wholly untracked). All hunks of one
    /// file share this. The `hide_untracked` filter drops these; a staged-new file is `false`.
    pub untracked: bool,
}

impl GitChangeCandidate {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path_index: u32,
        relative_path: String,
        abs_path: String,
        hunk_index: u32,
        line: u32,
        stage: DiffStage,
        added: u32,
        removed: u32,
        lines: Vec<String>,
    ) -> Self {
        GitChangeCandidate {
            path_index,
            relative_path,
            abs_path,
            hunk_index,
            line,
            stage,
            added,
            removed,
            lines,
            untracked: false,
        }
    }

    /// Builder tweak: mark every hunk of an untracked file. Set during candidate flattening so
    /// `::new`'s arg list stays put (its many call sites don't care about tracking state).
    pub fn with_untracked(mut self, untracked: bool) -> Self {
        self.untracked = untracked;
        self
    }

    /// The line to show on the row, and the char offsets of the regex match within it. With no
    /// query (`re` is `None`), the hunk's first changed line and no highlight. With one, the first
    /// changed line the regex matches (which is why the candidate was ranked) and the match's char
    /// span. Only ever called for the fetched window's rows (a handful). Falls back to the first
    /// line if nothing matches.
    pub fn preview(&self, re: Option<&regex::Regex>) -> (String, Vec<u32>) {
        let default = || (self.first_nonblank_line(), Vec::new());
        let Some(re) = re else { return default() };
        match regex_line_match(&self.lines, re) {
            Some((i, start, len)) => (
                self.lines[i].clone(),
                (start as u32..(start + len) as u32).collect(),
            ),
            None => default(),
        }
    }

    /// The buffer line to jump to on select. With no query (`re` is `None`), the hunk's anchor
    /// line. With one, the buffer line of the first changed line the regex matches (the previewed
    /// line) — so accepting a result lands on the match, not the top of the hunk. A match on a
    /// *removed* line has no buffer position, so it falls back to the anchor (the line the deletion
    /// sits above). The hunk's new-side lines are `lines[0..added]` at buffer lines `line..line+added`.
    pub fn select_line(&self, re: Option<&regex::Regex>) -> u32 {
        let Some(re) = re else { return self.line };
        match regex_line_match(&self.lines, re) {
            Some((i, _, _)) if (i as u32) < self.added => self.line + i as u32,
            _ => self.line,
        }
    }

    /// Whether the regex matches any of the hunk's changed lines — the hot path, run over every
    /// candidate on each keystroke. Line-oriented like grep; the compiled regex matches the original
    /// bytes (no per-call allocation or case folding — case is baked into `re`).
    pub fn matches(&self, re: &regex::Regex) -> bool {
        self.lines.iter().any(|line| re.is_match(line))
    }

    /// The first non-blank changed line (lines are stored trimmed, so "blank" is empty), falling
    /// back to the first line and then the empty string. This is the row's default preview, so a
    /// hunk that leads with a blank added/removed line doesn't render as an empty row.
    fn first_nonblank_line(&self) -> String {
        self.lines
            .iter()
            .find(|l| !l.is_empty())
            .or_else(|| self.lines.first())
            .cloned()
            .unwrap_or_default()
    }
}

/// The first line `re` matches, as `(line_index, char_start, char_len)` — the char-offset span so
/// the result is a valid `match_indices` range (the regex works in bytes; we convert). `None` when
/// no line matches.
fn regex_line_match(lines: &[String], re: &regex::Regex) -> Option<(usize, usize, usize)> {
    lines.iter().enumerate().find_map(|(i, line)| {
        re.find(line).map(|m| {
            let start = line[..m.start()].chars().count();
            let len = line[m.start()..m.end()].chars().count();
            (i, start, len)
        })
    })
}

/// Build the content-search regex from a query + match options — fixed-string escaping, whole-word
/// fencing, and smartcase — identical to grep / buffer search. Shared by buffer search
/// ([`crate::handlers::compute_search_entry`]) and the Git-changes picker so their query semantics
/// stay in lock-step. `Err` for an unparseable pattern (a half-typed regex), which callers surface
/// as "no matches".
pub fn build_match_regex(
    query: &str,
    options: &MatchOptions,
) -> Result<regex::Regex, regex::Error> {
    // Literal queries (the default) are escaped first; regex queries pass through raw. Whole-word
    // then fences the (escaped or raw) pattern with word boundaries. Smartcase reads the *original*
    // query's casing.
    let body = if options.regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    let pattern = if options.whole_word {
        format!(r"\b(?:{body})\b")
    } else {
        body
    };
    let case_insensitive = match options.case {
        CaseMode::Smart => !query.chars().any(|c| c.is_uppercase()),
        CaseMode::Sensitive => false,
        CaseMode::Insensitive => true,
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .build()
}

/// One diagnostics-picker candidate — a single diagnostic in the scoped buffer. `message` is the
/// fuzzy haystack; `abs_path` + `(line, col)` drive the `FileAt` jump on select.
#[derive(Debug, Clone)]
pub struct DiagnosticCandidate {
    /// The file as workspace root index + root-relative path — only used by the workspace-wide picker
    /// to group by file (the buffer-scoped picker renders flat).
    pub path_index: u32,
    pub relative_path: String,
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub abs_path: String,
}

/// One references-picker candidate — a single reference location from `textDocument/references`.
/// `preview` is the fuzzy haystack; `abs_path` + `(line, col)` drive the `FileAt` jump on select.
/// Cross-file, so it carries its own `abs_path` and a precomputed `display_path` label rather than
/// a workspace-root index (references may point outside every root).
#[derive(Debug, Clone)]
pub struct ReferenceCandidate {
    /// Absolute canonical path of the file containing the reference.
    pub abs_path: String,
    /// Row label: workspace-relative when inside a root, else the absolute path.
    pub display_path: String,
    pub line: u32,
    pub col: u32,
    /// Inclusive last position of the reference's identifier span (`== (line, col)` when the server
    /// gave no distinct span). Selecting the row lands the identifier selected — anchor at
    /// `(line, col)`, cursor here — like the outline picker.
    pub end_line: u32,
    pub end_col: u32,
    /// The referenced line's text (trailing newline trimmed). Haystack + preview.
    pub preview: String,
    /// True for the one candidate that is the symbol's definition (resolved via a parallel
    /// `textDocument/definition`), false for ordinary uses. Drives the Definition/References
    /// section split; candidates are sorted definition-first.
    pub is_definition: bool,
}

impl ReferenceCandidate {
    /// True when `pos` falls within this reference's identifier span (start-inclusive, end-inclusive).
    /// find-references is invoked from *inside* the identifier, so the seeded cursor usually sits
    /// past the span's start column — a start-only comparison would skip to the next occurrence.
    pub fn contains(&self, pos: LogicalPosition) -> bool {
        let p = (pos.line, pos.col);
        (self.line, self.col) <= p && p <= (self.end_line, self.end_col)
    }
}

/// One document-symbols-picker candidate — a single symbol from `textDocument/documentSymbol`,
/// scoped to the picked buffer. `name` is the fuzzy haystack; `start` (the name position) drives
/// the `FileAt` jump on select. Hierarchical responses are flattened depth-first, so `depth`
/// records the nesting level for indentation.
#[derive(Debug, Clone)]
pub struct SymbolCandidate {
    /// Absolute canonical path of the buffer's file.
    pub abs_path: String,
    /// The identifier (name) span. `start` is `selectionRange.start` — the jump target + identity;
    /// `end` is its inclusive last char (`selectionRange.end` stepped back one char, same line).
    /// `o` / picker-select land this span *selected*; `end` falls back to `start` (a point) for
    /// empty/multi-line ranges or flat servers without a distinct name range. Distinct from the
    /// `range_*` full extent below.
    pub start: LogicalPosition,
    pub end: LogicalPosition,
    /// Symbol name — haystack + the row's primary label.
    pub name: String,
    pub symbol_kind: aether_protocol::picker::SymbolKind,
    /// Signature (`DocumentSymbol`) or container name (`SymbolInformation`); empty when absent.
    pub detail: String,
    /// Nesting depth (0 = top-level).
    pub depth: u32,
    /// The symbol's full enclosing extent (`DocumentSymbol.range` / `SymbolInformation.location`),
    /// used only to find the symbol the cursor sits in for the initial highlight — not sent on the
    /// wire. Falls back to a zero-width span at the name position when the server omits it.
    pub range_start: LogicalPosition,
    pub range_end: LogicalPosition,
}

impl SymbolCandidate {
    /// True when `pos` falls within this symbol's enclosing range (start-inclusive, end-inclusive
    /// so a cursor resting on the closing brace still counts as "inside").
    pub fn contains(&self, pos: LogicalPosition) -> bool {
        let p = (pos.line, pos.col);
        (self.range_start.line, self.range_start.col) <= p
            && p <= (self.range_end.line, self.range_end.col)
    }
}

/// One workspace-symbols candidate — a symbol from an LSP `workspace/symbol` response
/// (`docs/workspace-symbols.md`). `name` is the fuzzy haystack; `(line, col)` drives the `FileAt`
/// jump.
///
/// Cross-file *and* potentially cross-root: rust-analyzer and gopls report symbols from
/// dependencies and the standard library, which live outside every workspace root. So it carries its
/// own absolute `abs_path` plus a `display_path` that's workspace-relative only when it can be —
/// exactly the treatment [`ReferenceCandidate`] uses.
#[derive(Debug, Clone)]
pub struct WorkspaceSymbolCandidate {
    pub abs_path: String,
    /// The workspace root containing the file, or `None` for an out-of-root symbol. `Some` means
    /// `display_path` is relative to that root — the `(path_index, relative_path)` pair the
    /// glob/dir filter chips are defined over.
    pub path_index: Option<u32>,
    /// Row label: workspace-relative when inside a root, else the absolute path.
    pub display_path: String,
    /// 0-based line of the symbol's name.
    pub line: u32,
    /// 0-based *byte* offset within that line (converted from the server's encoding).
    pub col: u32,
    /// Symbol name — the fuzzy haystack and the row's primary label.
    pub name: String,
    pub symbol_kind: aether_protocol::picker::SymbolKind,
    /// `SymbolInformation::containerName` — the enclosing type/module, shown dim after the name.
    pub container: String,
}

impl WorkspaceSymbolCandidate {
    /// Identity for dedup. Two declared projects can cover the same code (a Cargo workspace root and
    /// one of its member crates), so the *same* symbol arrives from different servers — and, because
    /// results are merged incrementally, usually in different batches.
    pub fn dedup_key(&self) -> (&str, u32, u32, &str) {
        (&self.abs_path, self.line, self.col, &self.name)
    }
}

/// One LSP-servers-picker candidate — a language server for the active workspace. `name` is the
/// fuzzy haystack; `language` is the key the client restarts by. Rebuilt on every `picker/view`
/// and on each `lsp/status_changed` (the list is tiny and the status changes), so the row's
/// `status` glyph stays live.
#[derive(Debug, Clone)]
pub struct LspServerCandidate {
    pub name: String,
    pub language: String,
    pub workspace_root: String,
    /// `workspace_root` relative to its workspace root, or empty when rooted at a workspace root.
    /// Display-only (see [`PickerItem::LspServer`]).
    pub root_label: String,
    pub status: LspStatus,
    /// Active `$/progress` work-done operations (empty when idle).
    pub progress: Vec<LspProgress>,
}

/// One Keybindings-picker candidate — a [`KeybindingEntry`] the client shipped on `picker/view`
/// (the binding tables live client-side; the server only matches and windows). `haystack` is the
/// entry's canonical composition ([`KeybindingEntry::haystack`]), precomputed once at build so
/// rerank doesn't re-format ~150 rows per keystroke.
#[derive(Debug, Clone)]
pub struct KeybindingCandidate {
    pub entry: KeybindingEntry,
    pub haystack: String,
}

impl From<KeybindingEntry> for KeybindingCandidate {
    fn from(entry: KeybindingEntry) -> Self {
        let haystack = entry.haystack();
        KeybindingCandidate { entry, haystack }
    }
}

/// How a candidate set turns a non-empty query into a ranked subset. Each `PickerCandidates`
/// variant picks one; `rerank` and `build_window_items` dispatch on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStrategy {
    /// Nucleo fuzzy match. Ranking is by score descending; match indices are the char
    /// positions nucleo highlighted. Used by Files and Buffers.
    Fuzzy,
    /// Smart-case prefix match. Natural candidate order preserved; match indices are the
    /// first N chars of the haystack (where N = query char count). Used by Explorer.
    PrefixSmartcase,
    /// No client-driven filter — the candidate set itself *is* the match set, in whatever
    /// order it was assembled. Used by Grep, where ripgrep filters server-side.
    Preserved,
    /// Regex (grep-style: smartcase, whole-word, fixed-string) over each candidate's content lines
    /// (not its path), natural order preserved. Used by GitChanges: the query greps the diff
    /// content, and the matched line becomes the row's preview. Glob/dir chips still scope by path
    /// alongside it.
    RegexContent,
}

/// The candidate set a `PickerState` is matching against. Per-kind variant keeps the candidate
/// data shape strict — selecting an item of the wrong kind out of a Files picker is a type
/// error, not a runtime branch.
#[derive(Debug, Clone)]
pub enum PickerCandidates {
    /// Workspace files, plus a per-file Git status aligned by index (`git_status[i]` is the status
    /// of `files[i]`). The files are a shared `Arc` (one snapshot per refresh, borrowed by every
    /// picker that touches it); the status vector is computed per `picker/view` against that same
    /// snapshot and carried alongside so `make_item` can colour each row.
    Files {
        files: Arc<Vec<CachedFile>>,
        git_status: Arc<Vec<Option<aether_protocol::git::GitStatus>>>,
    },
    /// Open buffers in MRU order (most-recent first). Cheap to rebuild — small N, no I/O.
    Buffers(Vec<BufferCandidate>),
    /// Grep matches in walker + line order. Grows as the streaming search runs; rerank is a
    /// no-op (the query is the search, so the candidate set already *is* the match set).
    Grep(Vec<GrepHitCandidate>),
    /// Filesystem entries of the picker's current directory. Re-listed on every `picker/view`
    /// (directories can mutate underneath us; no point caching).
    Explorer(ExplorerCandidates),
    /// The workspace's roots, shown by the Explorer when the client requests Roots mode (via
    /// `picker/view { explorer_roots: true }`). One row per root; selecting one transitions the
    /// explorer back into `Explorer` mode at that root's top.
    ExplorerRoots(Vec<RootCandidate>),
    /// Configured workspace names. Re-listed on each `picker/view` — small N, no caching needed,
    /// and the user may have edited `~/.config/aether/workspaces/` between opens.
    Workspaces(Vec<WorkspaceCandidate>),
    /// The scoped buffer's diagnostics. Built on open; preserved across non-reset re-views (like
    /// Grep) so scrolling doesn't rebuild against a possibly-changed set.
    Diagnostics(Vec<DiagnosticCandidate>),
    /// The active workspace's language servers. Rebuilt on every view and on each status change
    /// (small N), so it's never preserved — the row glyphs reflect live status.
    LspServers(Vec<LspServerCandidate>),
    /// References to the cursor's symbol. Built on open from a one-shot `textDocument/references`
    /// snapshot; preserved across non-reset re-views (like Diagnostics) so scrolling doesn't
    /// rebuild against a possibly-changed set.
    References(Vec<ReferenceCandidate>),
    /// The picked buffer's symbols. Built on open from a one-shot `textDocument/documentSymbol`
    /// snapshot; preserved across non-reset re-views (like References) so scrolling doesn't rebuild
    /// against a possibly-changed set.
    Symbols(Vec<SymbolCandidate>),
    /// Workspace-wide symbols from the pinned projects' servers. Grows as each server answers
    /// (`docs/workspace-symbols.md` § Merging), like Grep's streaming hits rather than a snapshot.
    WorkspaceSymbols(Vec<WorkspaceSymbolCandidate>),
    /// The workspace's working-tree hunks, grouped by file. Built fresh on every `picker/view`
    /// (a snapshot of the repo state at open); the query fuzzy-filters the file path while keeping
    /// the file grouping (document order, like the symbols outline).
    GitChanges(Vec<GitChangeCandidate>),
    /// The client's keyboard shortcuts, shipped on open (`PickerViewParams::keybindings`) and
    /// preserved across scroll/resume re-views (the re-view sends no rows), like Diagnostics.
    /// Static for the picker's lifetime — bindings can't change under a running client.
    Keybindings(Vec<KeybindingCandidate>),
    /// The client's jumplist (docs/jumplist.md), cloned from
    /// `ServerState.results` on every `picker/view` — cheap, in-memory, and the backing list
    /// persists regardless of the picker. Positional identity (`PickerItem::JumplistEntry::index`) is
    /// stable for the picker's lifetime: the list only changes via a re-capture, which resets
    /// the picker.
    Jumplist(Vec<crate::jumplist::JumplistEntry>),
}

/// One row in the Explorer's Roots mode. `absolute_path` is what the client navigates to on
/// select; `basename` is the matcher haystack (the disambiguator the client shows alongside is
/// derived client-side from `path_index` + the workspace's root list).
#[derive(Debug, Clone)]
pub struct RootCandidate {
    pub path_index: u32,
    pub absolute_path: String,
    pub basename: String,
}

impl PickerCandidates {
    pub fn len(&self) -> usize {
        match self {
            PickerCandidates::Files { files, .. } => files.len(),
            PickerCandidates::Buffers(v) => v.len(),
            PickerCandidates::Grep(v) => v.len(),
            PickerCandidates::Explorer(e) => e.entries.len(),
            PickerCandidates::ExplorerRoots(v) => v.len(),
            PickerCandidates::Workspaces(v) => v.len(),
            PickerCandidates::Diagnostics(v) => v.len(),
            PickerCandidates::LspServers(v) => v.len(),
            PickerCandidates::References(v) => v.len(),
            PickerCandidates::Symbols(v) => v.len(),
            PickerCandidates::WorkspaceSymbols(v) => v.len(),
            PickerCandidates::GitChanges(v) => v.len(),
            PickerCandidates::Keybindings(v) => v.len(),
            PickerCandidates::Jumplist(v) => v.len(),
        }
    }

    /// Drop the candidates, keeping the variant (so the slot still knows its kind). What
    /// `picker/hide` uses to release a closed picker's results — notably a finished or in-flight
    /// grep walk's hits, which are unbounded.
    pub fn clear(&mut self) {
        match self {
            PickerCandidates::Files { files, git_status } => {
                *files = Default::default();
                *git_status = Default::default();
            }
            PickerCandidates::Buffers(v) => v.clear(),
            PickerCandidates::Grep(v) => v.clear(),
            PickerCandidates::Explorer(e) => e.entries.clear(),
            PickerCandidates::ExplorerRoots(v) => v.clear(),
            PickerCandidates::Workspaces(v) => v.clear(),
            PickerCandidates::Diagnostics(v) => v.clear(),
            PickerCandidates::LspServers(v) => v.clear(),
            PickerCandidates::References(v) => v.clear(),
            PickerCandidates::Symbols(v) => v.clear(),
            PickerCandidates::WorkspaceSymbols(v) => v.clear(),
            PickerCandidates::GitChanges(v) => v.clear(),
            PickerCandidates::Keybindings(v) => v.clear(),
            PickerCandidates::Jumplist(v) => v.clear(),
        }
    }

    pub fn kind(&self) -> PickerKind {
        match self {
            PickerCandidates::Files { .. } => PickerKind::Files,
            PickerCandidates::Buffers(_) => PickerKind::Buffers,
            PickerCandidates::Grep(_) => PickerKind::Grep,
            PickerCandidates::Explorer(_) => PickerKind::Explorer,
            PickerCandidates::ExplorerRoots(_) => PickerKind::Explorer,
            PickerCandidates::Workspaces(_) => PickerKind::Workspaces,
            PickerCandidates::Diagnostics(_) => PickerKind::Diagnostics,
            PickerCandidates::LspServers(_) => PickerKind::LspServers,
            PickerCandidates::References(_) => PickerKind::References,
            PickerCandidates::Symbols(_) => PickerKind::DocumentSymbols,
            PickerCandidates::WorkspaceSymbols(_) => PickerKind::WorkspaceSymbols,
            PickerCandidates::GitChanges(_) => PickerKind::GitChanges,
            PickerCandidates::Keybindings(_) => PickerKind::Keybindings,
            PickerCandidates::Jumplist(_) => PickerKind::Jumplist,
        }
    }

    /// Haystack string used for fuzzy matching at index `idx`. For Files this is the relative
    /// path alone — root identity is *not* part of the fuzzy match (the user disambiguates roots
    /// via the explorer's Roots mode, not the fuzzy filter). For Grep this is the preview but
    /// it's only consulted by the fuzzy matcher, which we skip for Grep.
    pub fn display_at(&self, idx: usize) -> &str {
        match self {
            PickerCandidates::Files { files, .. } => &files[idx].relative_path,
            PickerCandidates::Buffers(v) => &v[idx].display,
            PickerCandidates::Grep(v) => &v[idx].preview,
            PickerCandidates::Explorer(e) => &e.entries[idx].name,
            PickerCandidates::ExplorerRoots(v) => &v[idx].basename,
            PickerCandidates::Workspaces(v) => &v[idx].name,
            PickerCandidates::Diagnostics(v) => &v[idx].message,
            PickerCandidates::LspServers(v) => &v[idx].name,
            PickerCandidates::References(v) => &v[idx].preview,
            PickerCandidates::Symbols(v) => &v[idx].name,
            PickerCandidates::WorkspaceSymbols(v) => &v[idx].name,
            // Not used as a match haystack (GitChanges greps content via SubstringContent, not
            // `display_at`); kept defined for completeness.
            PickerCandidates::GitChanges(v) => &v[idx].relative_path,
            PickerCandidates::Keybindings(v) => &v[idx].haystack,
            PickerCandidates::Jumplist(v) => &v[idx].display,
        }
    }

    /// Build the protocol-level `PickerItem` for candidate `idx`. `match_indices` is supplied by
    /// the fuzzy matcher for Files/Buffers/Explorer/Workspaces and ignored for Grep (the candidate
    /// already carries the ripgrep-computed match positions, which we use verbatim).
    pub fn make_item(&self, idx: usize, match_indices: Vec<u32>) -> PickerItem {
        match self {
            PickerCandidates::Files { files, git_status } => PickerItem::File {
                path_index: files[idx].path_index,
                relative_path: files[idx].relative_path.clone(),
                match_indices,
                git_status: git_status.get(idx).copied().flatten(),
            },
            PickerCandidates::Buffers(v) => {
                let c = &v[idx];
                PickerItem::Buffer {
                    buffer_id: c.buffer_id,
                    display: c.display.clone(),
                    status: c.status,
                    path_index: c.path.as_ref().map(|(i, _)| *i),
                    relative_path: c.path.as_ref().map(|(_, r)| r.clone()),
                    match_indices,
                    transient: c.transient,
                }
            }
            PickerCandidates::Grep(v) => {
                let c = &v[idx];
                PickerItem::GrepHit {
                    path_index: c.path_index,
                    relative_path: c.relative_path.clone(),
                    line: c.line,
                    col: c.col,
                    preview: c.preview.clone(),
                    match_indices: c.match_indices.clone(),
                }
            }
            PickerCandidates::Explorer(e) => {
                let entry = &e.entries[idx];
                PickerItem::DirEntry {
                    name: entry.name.clone(),
                    is_dir: entry.is_dir,
                    match_indices,
                    git_status: entry.git_status,
                }
            }
            PickerCandidates::ExplorerRoots(v) => PickerItem::Root {
                path_index: v[idx].path_index,
                match_indices,
            },
            PickerCandidates::Workspaces(v) => PickerItem::Workspace {
                name: v[idx].name.clone(),
                unsaved_buffers: v[idx].unsaved_buffers,
                match_indices,
            },
            PickerCandidates::Diagnostics(v) => {
                let c = &v[idx];
                PickerItem::Diagnostic {
                    path_index: c.path_index,
                    relative_path: c.relative_path.clone(),
                    line: c.line,
                    col: c.col,
                    end_line: c.end_line,
                    end_col: c.end_col,
                    severity: c.severity,
                    message: c.message.clone(),
                    match_indices,
                }
            }
            PickerCandidates::LspServers(v) => {
                let c = &v[idx];
                PickerItem::LspServer {
                    name: c.name.clone(),
                    language: c.language.clone(),
                    workspace_root: c.workspace_root.clone(),
                    root_label: c.root_label.clone(),
                    status: c.status.clone(),
                    progress: c.progress.clone(),
                    match_indices,
                }
            }
            PickerCandidates::References(v) => {
                let c = &v[idx];
                PickerItem::Reference {
                    path: c.abs_path.clone(),
                    display_path: c.display_path.clone(),
                    line: c.line,
                    col: c.col,
                    preview: c.preview.clone(),
                    is_definition: c.is_definition,
                    match_indices,
                }
            }
            PickerCandidates::Symbols(v) => {
                let c = &v[idx];
                PickerItem::Symbol {
                    path: c.abs_path.clone(),
                    // Buffer-local: the client already knows the file, so no label.
                    display_path: String::new(),
                    line: c.start.line,
                    col: c.start.col,
                    name: c.name.clone(),
                    symbol_kind: c.symbol_kind,
                    detail: c.detail.clone(),
                    depth: c.depth,
                    // `context` is query-dependent (a candidate is "context" only when it's an
                    // ancestor of a match but not matched itself) — set by `build_window_items`,
                    // which knows the current match set; here it defaults off.
                    context: false,
                    match_indices,
                }
            }
            PickerCandidates::WorkspaceSymbols(v) => {
                let c = &v[idx];
                PickerItem::Symbol {
                    path: c.abs_path.clone(),
                    display_path: c.display_path.clone(),
                    line: c.line,
                    col: c.col,
                    name: c.name.clone(),
                    symbol_kind: c.symbol_kind,
                    // The container name stands in for the signature `documentSymbol` gives us —
                    // `SymbolInformation` has no `detail`, and the enclosing type is the next most
                    // useful thing to show dim after the name.
                    detail: c.container.clone(),
                    depth: 0,
                    context: false,
                    match_indices,
                }
            }
            // Default preview (first changed line, no highlight). The window builder constructs the
            // query-matched variant directly via `GitChangeCandidate::preview`; the center-on /
            // section-jump callers reach here with empty `match_indices` and only use the item for
            // identity, so the default preview suffices.
            PickerCandidates::GitChanges(v) => {
                let c = &v[idx];
                PickerItem::GitChange {
                    path_index: c.path_index,
                    relative_path: c.relative_path.clone(),
                    hunk_index: c.hunk_index,
                    line: c.line,
                    stage: c.stage,
                    added: c.added,
                    removed: c.removed,
                    preview: c.first_nonblank_line(),
                    match_indices,
                }
            }
            PickerCandidates::Keybindings(v) => {
                let e = &v[idx].entry;
                PickerItem::Keybinding {
                    group: e.group.clone(),
                    desc: e.desc.clone(),
                    mode: e.mode.clone(),
                    keys: e.keys.clone(),
                    match_indices,
                }
            }
            PickerCandidates::Jumplist(v) => PickerItem::JumplistEntry {
                index: idx as u32,
                line: v[idx].position.line,
                display: v[idx].display.clone(),
                match_indices,
            },
        }
    }

    /// Find a candidate by the stable identity of a `PickerItem`. Returns the candidate index.
    /// Used by `view { center_on }` and `select` to round-trip an item to its candidate slot.
    pub fn position_of(&self, item: &PickerItem) -> Option<usize> {
        match (self, item) {
            (
                PickerCandidates::Files { files, .. },
                PickerItem::File {
                    path_index,
                    relative_path,
                    ..
                },
            ) => files
                .iter()
                .position(|c| c.path_index == *path_index && c.relative_path == *relative_path),
            (PickerCandidates::Buffers(v), PickerItem::Buffer { buffer_id, .. }) => {
                v.iter().position(|c| c.buffer_id == *buffer_id)
            }
            (
                PickerCandidates::Grep(v),
                PickerItem::GrepHit {
                    path_index,
                    relative_path,
                    line,
                    col,
                    ..
                },
            ) => v.iter().position(|c| {
                c.path_index == *path_index
                    && c.relative_path == *relative_path
                    && c.line == *line
                    && c.col == *col
            }),
            (PickerCandidates::Explorer(e), PickerItem::DirEntry { name, .. }) => {
                e.entries.iter().position(|c| c.name == *name)
            }
            (PickerCandidates::ExplorerRoots(v), PickerItem::Root { path_index, .. }) => {
                v.iter().position(|c| c.path_index == *path_index)
            }
            (PickerCandidates::Workspaces(v), PickerItem::Workspace { name, .. }) => {
                v.iter().position(|c| c.name == *name)
            }
            (
                PickerCandidates::Diagnostics(v),
                PickerItem::Diagnostic {
                    line, col, message, ..
                },
            ) => v
                .iter()
                .position(|c| c.line == *line && c.col == *col && c.message == *message),
            (
                PickerCandidates::LspServers(v),
                PickerItem::LspServer {
                    language,
                    workspace_root,
                    ..
                },
            ) => v
                .iter()
                .position(|c| c.language == *language && c.workspace_root == *workspace_root),
            (
                PickerCandidates::References(v),
                PickerItem::Reference {
                    path, line, col, ..
                },
            ) => v
                .iter()
                .position(|c| c.abs_path == *path && c.line == *line && c.col == *col),
            (
                PickerCandidates::Symbols(v),
                PickerItem::Symbol {
                    path, line, col, ..
                },
            ) => v
                .iter()
                .position(|c| c.abs_path == *path && c.start.line == *line && c.start.col == *col),
            // Same item type, different candidate shape: workspace symbols carry one position
            // rather than a name span, so they match on it directly.
            (
                PickerCandidates::WorkspaceSymbols(v),
                PickerItem::Symbol {
                    path, line, col, ..
                },
            ) => v
                .iter()
                .position(|c| c.abs_path == *path && c.line == *line && c.col == *col),
            (
                PickerCandidates::GitChanges(v),
                PickerItem::GitChange {
                    path_index,
                    relative_path,
                    hunk_index,
                    ..
                },
            ) => v.iter().position(|c| {
                c.path_index == *path_index
                    && c.relative_path == *relative_path
                    && c.hunk_index == *hunk_index
            }),
            (
                PickerCandidates::Keybindings(v),
                PickerItem::Keybinding {
                    mode, keys, desc, ..
                },
            ) => v.iter().position(|c| {
                c.entry.mode == *mode && c.entry.keys == *keys && c.entry.desc == *desc
            }),
            // Positional identity: the captured list is a snapshot, stable for the picker's
            // lifetime (a re-capture resets the picker), so the index alone is the identity.
            (PickerCandidates::Jumplist(v), PickerItem::JumplistEntry { index, .. }) => {
                ((*index as usize) < v.len()).then_some(*index as usize)
            }
            _ => None,
        }
    }

    /// How the matcher should turn a non-empty query into a ranked subset for this candidate
    /// set. Centralises the per-variant decision so `rerank` and `build_window_items` can
    /// dispatch through one switch instead of scattered `matches!(..., Grep|Explorer)` checks.
    pub fn match_strategy(&self) -> MatchStrategy {
        match self {
            PickerCandidates::Files { .. }
            | PickerCandidates::Buffers(_)
            | PickerCandidates::Workspaces(_)
            | PickerCandidates::Diagnostics(_)
            | PickerCandidates::LspServers(_)
            | PickerCandidates::References(_)
            | PickerCandidates::Symbols(_)
            // Fuzzy here *orders* but must never filter — the server did the matching, and its
            // query conventions (rust-analyzer's `#`/`*`) would fail a literal fuzzy match. See
            // `rerank`'s workspace-symbols arm and `docs/workspace-symbols.md` § Merging.
            | PickerCandidates::WorkspaceSymbols(_)
            | PickerCandidates::Keybindings(_)
            | PickerCandidates::Jumplist(_) => MatchStrategy::Fuzzy,
            // GitChanges greps the diff content (regex, not path); document order is kept so the
            // per-file grouping stays contiguous, like the symbols outline.
            PickerCandidates::GitChanges(_) => MatchStrategy::RegexContent,
            PickerCandidates::Explorer(_) | PickerCandidates::ExplorerRoots(_) => {
                MatchStrategy::PrefixSmartcase
            }
            // Grep's candidates *are* the matches (ripgrep already filtered + ordered them),
            // so query changes don't re-rank — they trigger a fresh walk elsewhere.
            PickerCandidates::Grep(_) => MatchStrategy::Preserved,
        }
    }

    /// Produce the per-kind result of `picker/select` for candidate `idx`. `None` when the item
    /// is not a "selectable" leaf — currently only the Explorer picker's directory entries,
    /// which the client should navigate into (via `picker/view`) instead of selecting.
    pub fn select_result(&self, idx: usize) -> Option<PickerSelectResult> {
        match self {
            PickerCandidates::Files { files, .. } => Some(PickerSelectResult::File {
                path: files[idx].abs.clone(),
            }),
            PickerCandidates::Buffers(v) => Some(PickerSelectResult::Buffer {
                buffer_id: v[idx].buffer_id,
            }),
            PickerCandidates::Grep(v) => {
                let c = &v[idx];
                Some(PickerSelectResult::FileAt {
                    path: c.abs_path.clone(),
                    position: LogicalPosition {
                        line: c.line,
                        col: c.col,
                    },
                    anchor: None,
                })
            }
            PickerCandidates::Explorer(e) => {
                let entry = &e.entries[idx];
                if entry.is_dir {
                    None
                } else {
                    let abs = std::path::Path::new(&e.path)
                        .join(&entry.name)
                        .display()
                        .to_string();
                    Some(PickerSelectResult::File { path: abs })
                }
            }
            // Roots are always "navigate, don't select" — the client looks up the root's
            // absolute path from its workspace_paths and fires `picker/view` to enter it.
            PickerCandidates::ExplorerRoots(_) => None,
            PickerCandidates::Workspaces(v) => Some(PickerSelectResult::Workspace {
                name: v[idx].name.clone(),
            }),
            PickerCandidates::Diagnostics(v) => {
                let c = &v[idx];
                Some(PickerSelectResult::FileAt {
                    path: c.abs_path.clone(),
                    position: LogicalPosition {
                        line: c.line,
                        col: c.col,
                    },
                    anchor: None,
                })
            }
            // LSP servers aren't a jump target — the client restarts the highlighted server via
            // `lsp/restart_server` (Ctrl-r). `select` never fires for this kind.
            PickerCandidates::LspServers(_) => None,
            PickerCandidates::References(v) => {
                let c = &v[idx];
                // Land the identifier *selected*, like the outline picker: cursor on the span's
                // last char, anchor at its start. A point when there's no distinct span.
                let start = LogicalPosition {
                    line: c.line,
                    col: c.col,
                };
                let end = LogicalPosition {
                    line: c.end_line,
                    col: c.end_col,
                };
                Some(PickerSelectResult::FileAt {
                    path: c.abs_path.clone(),
                    position: end,
                    anchor: (end != start).then_some(start),
                })
            }
            PickerCandidates::Symbols(v) => {
                let c = &v[idx];
                // Land the identifier *selected*: cursor on the name's last char, anchor at its
                // start (matches `o`/`Alt-o`). A point when there's no distinct name span.
                Some(PickerSelectResult::FileAt {
                    path: c.abs_path.clone(),
                    position: c.end,
                    anchor: (c.end != c.start).then_some(c.start),
                })
            }
            PickerCandidates::WorkspaceSymbols(v) => {
                let c = &v[idx];
                // `workspace/symbol` gives one position, not a name span, so there's nothing to
                // select — land the cursor on the symbol.
                Some(PickerSelectResult::FileAt {
                    path: c.abs_path.clone(),
                    position: LogicalPosition {
                        line: c.line,
                        col: c.col,
                    },
                    anchor: None,
                })
            }
            // Query-less default (anchor line). The query-aware variant — landing on the matched
            // line — is applied by `resolve_select`, which has the picker's query.
            PickerCandidates::GitChanges(v) => Some(git_change_select(&v[idx], None)),
            // Informational — a shortcut row isn't a jump target and `select` never fires for
            // this kind (the client's Enter just closes the picker), like LspServers.
            PickerCandidates::Keybindings(_) => None,
            // Entries land exactly as selecting the source row would: `position`/`anchor` were
            // captured from the source picker's own select semantics.
            PickerCandidates::Jumplist(v) => {
                let e = &v[idx];
                Some(PickerSelectResult::FileAt {
                    path: e.abs_path.clone(),
                    position: e.position,
                    anchor: e.anchor,
                })
            }
        }
    }
}

/// The `FileAt` result for a Git-changes hunk, landing on the line the `re` (the query regex)
/// matched, or the anchor with no query. Factored out so `select_result` (query-less) and
/// `resolve_select` (query-aware) share one construction.
fn git_change_select(c: &GitChangeCandidate, re: Option<&regex::Regex>) -> PickerSelectResult {
    PickerSelectResult::FileAt {
        path: c.abs_path.clone(),
        position: LogicalPosition {
            line: c.select_line(re),
            col: 0,
        },
        anchor: None,
    }
}

/// Per-picker server state. Held under the global `ServerState` lock.
/// One contiguous group run in ranked space, for the collapsible kinds: the items
/// `ranked[start..start + len]` share a group key.
#[derive(Debug, Clone, Copy)]
pub struct GroupRun {
    pub start: u32,
    pub len: u32,
}

/// What one row of a collapsible picker's row space resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowRef {
    /// A group's header row — the position of its run in [`RowLayout::runs`].
    Header(usize),
    /// An item row of the expanded run — the item's ranked-space position (index into
    /// `ranked`).
    Item(u32),
}

/// The row space of a collapsible picker (docs/picker-groups.md): one selectable header row
/// per group run, plus the expanded run's items inline after its header — everything the
/// window/offset/selection space counts for these kinds. At most one run is expanded
/// (accordion), so every mapping here is O(1) arithmetic over the run list.
pub struct RowLayout {
    pub runs: Vec<GroupRun>,
    /// Position in `runs` of the expanded run: [`PickerState::expanded`] resolved against the
    /// current ranking, falling back to the first run when unset/unresolved — so this is
    /// `Some` whenever `runs` is non-empty (exactly one group is always open,
    /// docs/picker-groups.md §9). `None` only for an empty result set.
    pub expanded: Option<usize>,
}

impl RowLayout {
    fn expanded_len(&self) -> u32 {
        self.expanded.map(|i| self.runs[i].len).unwrap_or(0)
    }

    /// Total rows: one header per run + the expanded run's items.
    pub fn total_rows(&self) -> u32 {
        self.runs.len() as u32 + self.expanded_len()
    }

    /// Absolute row of run `pos`'s header: one row per run above it, plus the expanded run's
    /// items when that run sits above `pos`.
    pub fn header_row(&self, pos: usize) -> u32 {
        pos as u32
            + match self.expanded {
                Some(e) if e < pos => self.runs[e].len,
                _ => 0,
            }
    }

    /// Resolve absolute row `row`; `None` past the end.
    pub fn row_ref(&self, row: u32) -> Option<RowRef> {
        let total_runs = self.runs.len() as u32;
        let Some(e) = self.expanded else {
            return (row < total_runs).then_some(RowRef::Header(row as usize));
        };
        let (e_row, e_len) = (e as u32, self.runs[e].len);
        if row <= e_row {
            return Some(RowRef::Header(row as usize));
        }
        if row <= e_row + e_len {
            return Some(RowRef::Item(self.runs[e].start + (row - e_row - 1)));
        }
        let pos = row - e_len;
        (pos < total_runs).then_some(RowRef::Header(pos as usize))
    }

    /// The run containing ranked position `rank`.
    fn run_of_rank(&self, rank: u32) -> usize {
        self.runs.partition_point(|r| r.start <= rank) - 1
    }

    /// The row where ranked position `rank` is *reachable*: its own item row when its run is
    /// expanded, else its run's header row (the item itself is hidden behind it).
    pub fn row_of_rank(&self, rank: u32) -> u32 {
        let pos = self.run_of_rank(rank);
        match self.expanded {
            Some(e) if e == pos => self.header_row(pos) + (rank - self.runs[pos].start) + 1,
            _ => self.header_row(pos),
        }
    }
}

/// The group key a wire [`GroupHeader`] denotes — the owned mirror of
/// [`PickerState::group_key_at`]'s per-row derivation, resolving headers a client sends back
/// (`picker/set_group`, a `Group` row in `center_on`) to runs. The two MUST agree for the
/// collapsible kinds: `File` ↔ `(path_index, relative_path)`, `Label` ↔ `(u32::MAX, label)`.
pub fn group_key_of_header(header: &GroupHeader) -> (u32, String) {
    match header {
        GroupHeader::File {
            path_index,
            relative_path,
        } => (*path_index, relative_path.clone()),
        GroupHeader::Label { label } => (u32::MAX, label.clone()),
    }
}

/// Per-window item-building context — see [`PickerState::item_ctx`].
struct ItemCtx {
    re: Option<regex::Regex>,
    pattern: Option<Pattern>,
    prefix_len: u32,
    query_active: bool,
}

pub struct PickerState {
    pub kind: PickerKind,
    pub query: String,
    /// Result-narrowing filters (chips client-side). Replaced whole by `picker/query` /
    /// `picker/view { filters }`; persisted across hide/show like `query`. Grep applies them in
    /// the spawned search; Files applies them in `rerank`; Explorer applies them when the
    /// listing is built (so they're consulted in `picker_view`, not here).
    pub filters: PickerFilters,
    pub generation: u64,
    /// Indices into `candidates` in match-score order (descending). On empty query, this is
    /// the candidate set's natural order — alphabetical for files, MRU for buffers.
    pub ranked: Vec<u32>,
    /// The candidate snapshot `ranked` was computed against. Pinned here so `select` and
    /// `center_on` resolve against the same set the client most recently saw — even if the
    /// underlying source (workspace index, buffer set) is later refreshed.
    pub candidates: PickerCandidates,
    /// Explorer only: the committed directory the query peeks relative to (see
    /// [`ExplorerAnchorInfo`]). Set by `picker/view` when navigation moves the directory; left
    /// untouched by `picker/query` (typing a path peeks without committing). `None` for every
    /// other kind and until the first Explorer `view`.
    pub explorer_anchor: Option<ExplorerAnchorInfo>,
    /// Explorer only: true when the current query's peek directory (anchor + path part) doesn't
    /// resolve to an in-workspace directory. Set wherever the peek listing is (re)built; echoed in
    /// `picker/update` so the client only offers "+ Create directory" when it's actually missing.
    pub explorer_peek_missing: bool,
    /// `Some` while the client has the picker open and is receiving pushes. `None` after `hide`,
    /// which also clears the rest of the slot — the fields around this one are only ever populated
    /// while a picker is open.
    pub subscribed: Option<SubscribedWindow>,
    /// References only: tracks an in-flight async resolve (the `textDocument/references` round-trip
    /// runs off the lock, so the picker opens empty and is populated by a spawned task). `Some(epoch)`
    /// while a resolve is outstanding; the epoch is a monotonic token so a stale task — one whose
    /// picker was reset/reopened (minting a newer epoch) — notices the mismatch on completion and
    /// drops its result instead of clobbering the current load. `is_some()` also drives the
    /// "loading" (`ticking`) state for the row count + spinner. Cleared to `None` when the matching
    /// task applies its result. Distinct from `generation` (which a *query* change bumps): a query
    /// while loading must re-filter the pending result, not cancel the resolve.
    pub pending_async_load: Option<u64>,
    /// WorkspaceSymbols only: how many `workspace/symbol` requests from the current generation's
    /// fan-out are still unanswered. Private — mutated only through [`Self::seed_symbol_fanout`],
    /// [`Self::add_symbol_query`], and [`Self::note_symbol_merge`], whose docs carry the
    /// balance invariant this counter lives or dies by.
    pending_symbol_queries: usize,
    /// Grep only: the `(query, filters)` whose walk last completed (`ticking: false` push went
    /// out). When the next `picker/query` arrives with the same pair, the candidates are still
    /// valid — skip the wipe + respawn and just re-emit the current window. Cleared whenever a
    /// new search starts; set by the streaming coordinator's final-push branch.
    pub last_completed_search: Option<(String, PickerFilters)>,
    /// WorkspaceSymbols only: the `(query, (root, language) server identities)` of the fan-out
    /// that produced the current candidates — grep's `last_completed_search` in fan-out shape.
    /// Filters aren't part of the key because they never reach the servers: the Dir chip only
    /// *prunes* who gets asked, so a settled fan-out's candidates are valid for any filter set
    /// whose admitted servers it covers ([`Self::symbol_fanout_covers`]) and the change is just
    /// a rerank. Recorded when a fan-out spawns; a became-ready re-query appends itself
    /// ([`crate::symbols::requery_ready_server`]).
    pub symbol_fanned: Option<(String, Vec<(std::path::PathBuf, String)>)>,
    /// Collapsible kinds only (docs/picker-groups.md §9): the group key of the one expanded
    /// run — exactly one group shows its items whenever there are groups (accordion; the
    /// *selected* group under the two-level navigation model). The key is resolved against
    /// the current ranking lazily at row-build time, so it survives streaming re-ranks and
    /// query keystrokes for as long as its group does. `None` — a fresh open, or the key's
    /// group re-ranked away — falls back to the *first* run at resolve time
    /// ([`Self::row_layout`]): the top group opens itself, matching the client's
    /// selection-starts-at-the-top convention.
    pub expanded: Option<(u32, String)>,
}

#[derive(Debug, Clone, Copy)]
pub struct SubscribedWindow {
    pub offset: u32,
    pub limit: u32,
}

/// Compile filter globs into an rg-style override matcher (`!` = exclude; with ≥1 plain glob,
/// non-matching files are excluded). `Ok(None)` when there are no globs. Globs match against
/// root-relative paths (the builder root is empty, so relative inputs are compared as-is).
/// Shared by the Files rerank pass and the grep search worker.
pub fn build_overrides(
    globs: &[String],
) -> Result<Option<ignore::overrides::Override>, ignore::Error> {
    if globs.is_empty() {
        return Ok(None);
    }
    let mut builder = ignore::overrides::OverrideBuilder::new("");
    for g in globs {
        builder.add(g)?;
    }
    builder.build().map(Some)
}

/// A path-scope filter-chip predicate (globs + directory scopes + the `changed_only` flag),
/// applied during `rerank` before fuzzy matching. Cheap to rebuild per rerank (a handful of globs
/// at most). The Files picker uses the full predicate; the Git-changes picker uses only the
/// path-scope half ([`FilesFilter::passes_path`]) — it's inherently changed-only and has no status
/// vector — via the glob/dir chips it shares.
struct FilesFilter {
    overrides: Option<ignore::overrides::Override>,
    /// An invalid glob rejects everything — mirrors grep's invalid-pattern behavior (the chip
    /// stays visible so the user can see what to fix).
    reject_all: bool,
    /// Union of path scopes `(path_index, relative_path, is_file)`: a file passes when it's under
    /// *any* of them (matching how multiple include globs combine). A directory scope passes its
    /// subtree — empty `relative_path` scopes to the whole root; an `is_file` scope passes only
    /// that exact file. An empty vec means no scope narrowing.
    directories: Vec<(u32, String, bool)>,
    changed_only: bool,
    hide_untracked: bool,
    /// Files only: drop entries whose path has a dot-component. The Files walk *includes* hidden
    /// entries (unlike grep's), so this chip hides them back out — the Explorer-style inverted
    /// polarity. Never set for Git changes (that picker doesn't offer the chip).
    hide_hidden: bool,
}

/// A workspace-relative path is "hidden" when any component starts with a dot — matching ripgrep's
/// rule and the Explorer's `hide_hidden`, so a file under `.circleci/` counts as hidden, not just
/// a leaf dotfile.
pub(crate) fn path_is_hidden(relative_path: &str) -> bool {
    relative_path.split('/').any(|c| c.starts_with('.'))
}

/// True when `file` sits under the `(path_index, relative_path)` scope. A directory scope (the
/// default) matches as a prefix — an empty relative path scopes to the whole root; a `is_file`
/// scope matches that one file exactly. Shared shape with grep's `FileFilter`.
pub(crate) fn under_scope(
    file_path_index: u32,
    file_relative_path: &str,
    path_index: u32,
    rel: &str,
    is_file: bool,
) -> bool {
    if file_path_index != path_index {
        return false;
    }
    if is_file {
        return file_relative_path == rel;
    }
    rel.is_empty()
        || (file_relative_path.starts_with(rel)
            && file_relative_path.as_bytes().get(rel.len()) == Some(&b'/'))
}

impl FilesFilter {
    fn new(filters: &PickerFilters) -> FilesFilter {
        let (overrides, reject_all) = match build_overrides(&filters.globs) {
            Ok(o) => (o, false),
            Err(_) => (None, true),
        };
        FilesFilter {
            overrides,
            reject_all,
            directories: filters
                .directories
                .iter()
                .map(|d| (d.path_index, d.relative_path.clone(), d.is_file))
                .collect(),
            changed_only: filters.changed_only,
            hide_untracked: filters.hide_untracked,
            hide_hidden: filters.hide_hidden,
        }
    }

    /// The path-scope half: reject-all (invalid glob), directory scopes, and include/exclude globs,
    /// all keyed only on `(path_index, relative_path)`. Shared by Files and Git changes.
    fn passes_path(&self, path_index: u32, relative_path: &str) -> bool {
        if self.reject_all {
            return false;
        }
        if !self.directories.is_empty()
            && !self
                .directories
                .iter()
                .any(|(pi, rel, isf)| under_scope(path_index, relative_path, *pi, rel, *isf))
        {
            return false;
        }
        if let Some(ov) = &self.overrides {
            if ov.matched(relative_path, false).is_ignore() {
                return false;
            }
        }
        true
    }

    /// Whether an entry with *no* root-relative path (the Jumplist's out-of-workspace captures)
    /// survives: only when no path filter is active at all. Scopes and globs are defined over
    /// root-relative paths, so a pathless entry can't satisfy — or be meaningfully exempted
    /// from — either; the predictable reading is "any path narrowing drops it".
    fn passes_pathless(&self) -> bool {
        !self.reject_all && self.directories.is_empty() && self.overrides.is_none()
    }

    fn passes(&self, file: &CachedFile, status: Option<aether_protocol::git::GitStatus>) -> bool {
        if !self.passes_path(file.path_index, &file.relative_path) {
            return false;
        }
        // The Files walk includes hidden entries by default (so tracked dot-files are reachable);
        // this chip drops anything with a dot-component back out. Files-only — `passes` isn't
        // called for Git changes.
        if self.hide_hidden && path_is_hidden(&file.relative_path) {
            return false;
        }
        // The workspace walk never yields ignored files, so any status here is a real change.
        if self.changed_only && status.is_none() {
            return false;
        }
        if self.hide_untracked && status == Some(aether_protocol::git::GitStatus::Untracked) {
            return false;
        }
        true
    }
}

impl PickerState {
    pub fn new(candidates: PickerCandidates) -> Self {
        let kind = candidates.kind();
        let ranked: Vec<u32> = (0..candidates.len() as u32).collect();
        Self {
            kind,
            query: String::new(),
            filters: PickerFilters::default(),
            generation: 0,
            ranked,
            candidates,
            explorer_anchor: None,
            explorer_peek_missing: false,
            subscribed: None,
            pending_async_load: None,
            pending_symbol_queries: 0,
            last_completed_search: None,
            symbol_fanned: None,
            expanded: None,
        }
    }

    /// Whether the recorded fan-out ([`Self::symbol_fanned`]) already accounts for every server
    /// `admitted` by the current filters, for the current query — i.e. a re-fan-out could not
    /// produce anything the accumulated candidates don't already hold, so a filter-only change
    /// can keep them and just rerank. Requires the fan-out to have *settled*: with answers still
    /// outstanding, the generation bump the caller is processing would orphan them
    /// (`merge_results` drops on generation mismatch) and the candidates would silently stay
    /// partial.
    pub fn symbol_fanout_covers<'a>(
        &self,
        admitted: impl Iterator<Item = (&'a std::path::Path, &'a str)>,
    ) -> bool {
        if self.pending_symbol_queries != 0 {
            return false;
        }
        let Some((query, fanned)) = &self.symbol_fanned else {
            return false;
        };
        *query == self.query
            && admitted
                .into_iter()
                .all(|(root, language)| fanned.iter().any(|(r, l)| r == root && l == language))
    }

    /// Seed the WorkspaceSymbols fan-out counter for a fresh query: `n` `workspace/symbol`
    /// requests are about to be spawned, and every one ends in exactly one
    /// [`crate::symbols::merge_results`] call ([`Self::note_symbol_merge`]), whatever the server
    /// answered — that balance is the whole contract. While the counter is non-zero the merges
    /// push `ticking: true`; the merge that takes it to zero always pushes, even with nothing
    /// new, so the picker settles to its empty note instead of spinning forever on a query no
    /// server matched. Generation-scoped implicitly: a query re-seeds it, and a stale merge
    /// bails on the generation check before counting itself. `picker/hide` seeds 0 (nothing
    /// pending survives a close).
    pub fn seed_symbol_fanout(&mut self, n: usize) {
        self.pending_symbol_queries = n;
    }

    /// A became-ready server's re-query counts itself into the fan-out — one more merge is
    /// coming. Must be called under the same lock as the generation snapshot the re-query was
    /// filtered by: a keystroke in the gap would re-seed the counter, and this increment would
    /// unbalance the new fan-out.
    pub fn add_symbol_query(&mut self) {
        self.pending_symbol_queries += 1;
    }

    /// One fan-out answer merged: count it off, and report whether that settles the picker.
    /// `true` means this was the final answer — the caller MUST push `ticking: false`, even
    /// with nothing new, because that push is the only thing that settles the client.
    pub fn note_symbol_merge(&mut self) -> bool {
        self.pending_symbol_queries = self.pending_symbol_queries.saturating_sub(1);
        self.pending_symbol_queries == 0
    }

    /// Recompute the ranked match list against the current candidates and query. Cheap for
    /// "small" workspaces (< ~50k files in benchmarks); revisit if we ever need to stream.
    pub fn rerank(&mut self, matcher: &mut Matcher) {
        self.ranked.clear();
        let strategy = self.candidates.match_strategy();
        // Files: filter chips narrow the candidate set before (and independently of) the fuzzy
        // match. Files, Git changes, the Jumplist and workspace symbols all narrow by glob/dir
        // chips before fuzzy matching (Git changes is inherently changed-only; the Jumplist and
        // workspace symbols filter by file identity — the Dir chip additionally prunes the
        // symbol *fan-out*, but that only skips servers with nothing in scope; this predicate
        // is what actually narrows the rows). The other kinds filter elsewhere (Grep in the
        // search worker, Explorer when the listing is built), so their predicate is always
        // "pass".
        let files_filter = match &self.candidates {
            PickerCandidates::Files { .. }
            | PickerCandidates::GitChanges(_)
            | PickerCandidates::Jumplist(_)
            | PickerCandidates::WorkspaceSymbols(_)
                if !self.filters.is_default() =>
            {
                Some(FilesFilter::new(&self.filters))
            }
            _ => None,
        };
        let passes = |candidates: &PickerCandidates, i: usize| -> bool {
            let Some(ff) = files_filter.as_ref() else {
                return true;
            };
            match candidates {
                PickerCandidates::Files { files, git_status } => {
                    ff.passes(&files[i], git_status.get(i).copied().flatten())
                }
                PickerCandidates::GitChanges(v) => {
                    ff.passes_path(v[i].path_index, &v[i].relative_path)
                        && !(ff.hide_untracked && v[i].untracked)
                }
                PickerCandidates::Jumplist(v) => {
                    match (v[i].path_index, v[i].relative_path.as_deref()) {
                        (Some(pi), Some(rp)) => ff.passes_path(pi, rp),
                        // Out-of-workspace entry (references into dependencies): no root-relative
                        // path for scopes/globs to speak about, so any active path filter
                        // excludes it.
                        _ => ff.passes_pathless(),
                    }
                }
                PickerCandidates::WorkspaceSymbols(v) => match v[i].path_index {
                    // In-root: `display_path` is the root-relative path.
                    Some(pi) => ff.passes_path(pi, &v[i].display_path),
                    // Out-of-root symbol (dependencies, the standard library) — the Jumplist's
                    // pathless rule: any path narrowing drops it. Notably this includes
                    // rust-analyzer's `#`/`*` widened results, which is the consistent reading —
                    // globs and scopes speak about workspace files.
                    None => ff.passes_pathless(),
                },
                _ => true,
            }
        };
        // Two paths converge on "preserve natural order": Grep's strategy is always Preserved,
        // and the other strategies short-circuit to natural order on an empty query.
        if strategy == MatchStrategy::Preserved || self.query.is_empty() {
            let n = self.candidates.len();
            let mut ranked: Vec<u32> = Vec::with_capacity(n);
            for i in 0..n {
                if passes(&self.candidates, i) {
                    ranked.push(i as u32);
                }
            }
            self.ranked = ranked;
            return;
        }
        match strategy {
            MatchStrategy::Fuzzy => {
                let pattern =
                    Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
                let mut buf = Vec::new();
                let n = self.candidates.len();
                let mut scored: Vec<(u32, u32)> = Vec::with_capacity(n);
                for i in 0..n {
                    if !passes(&self.candidates, i) {
                        continue;
                    }
                    let haystack = Utf32Str::new(self.candidates.display_at(i), &mut buf);
                    if let Some(score) = pattern.score(haystack, matcher) {
                        scored.push((score, i as u32));
                    }
                }
                if let PickerCandidates::WorkspaceSymbols(v) = &self.candidates {
                    // Order, never filter. The *server* ran the match — and some servers give the
                    // query extra meaning (rust-analyzer widens into dependencies with a trailing
                    // `#`, the stdlib with `*`), which a literal fuzzy match against symbol names
                    // fails on every row. Filtering here would empty the picker for a query that
                    // worked perfectly at the server.
                    //
                    // And order at *group* grain, not row grain: the collapsible row space derives
                    // its runs from consecutive group keys and resolves a key to its *first* run
                    // (`row_layout` / `run_of_key`), so a file interleaved across ranked space
                    // renders as duplicate groups and traps the expanded-group resolution in the
                    // first of them. Files sort by their best row's score; rows within a file by
                    // their own score, unscored rows last in the merge's line order; files nucleo
                    // matched nothing in trail in the merge's path order, unhighlighted. `ranked[0]`
                    // is still the globally best match. See `docs/workspace-symbols.md` § Merging.
                    let mut own: Vec<Option<u32>> = vec![None; v.len()];
                    let mut best: std::collections::HashMap<&str, u32> =
                        std::collections::HashMap::new();
                    for &(score, i) in &scored {
                        own[i as usize] = Some(score);
                        best.entry(v[i as usize].display_path.as_str())
                            .and_modify(|b| *b = (*b).max(score))
                            .or_insert(score);
                    }
                    let mut ranked: Vec<u32> = (0..v.len() as u32)
                        .filter(|i| passes(&self.candidates, *i as usize))
                        .collect();
                    ranked.sort_unstable_by_key(|&i| {
                        let c = &v[i as usize];
                        (
                            std::cmp::Reverse(best.get(c.display_path.as_str()).copied()),
                            c.display_path.as_str(),
                            std::cmp::Reverse(own[i as usize]),
                            i,
                        )
                    });
                    self.ranked = ranked;
                    return;
                }
                if let PickerCandidates::Symbols(syms) = &self.candidates {
                    // DocumentSymbols read as a tree: keep the matches in document order (not score
                    // order) and pull in each match's ancestor chain so the filtered list shows the
                    // sub-tree with context. Ancestors are added as plain candidate indices; because
                    // they don't match the query they get no `match_indices`, which the client/window
                    // builder reads as a non-selectable `context` row. The BTreeSet both dedups
                    // shared ancestors and yields document (index) order.
                    let mut keep: std::collections::BTreeSet<u32> =
                        scored.iter().map(|(_, i)| *i).collect();
                    for (_, m) in &scored {
                        let mut depth = syms[*m as usize].depth;
                        let mut j = *m;
                        while depth > 0 && j > 0 {
                            j -= 1;
                            if syms[j as usize].depth < depth {
                                keep.insert(j);
                                depth = syms[j as usize].depth;
                            }
                        }
                    }
                    self.ranked = keep.into_iter().collect();
                } else if matches!(
                    &self.candidates,
                    PickerCandidates::GitChanges(_)
                        | PickerCandidates::Keybindings(_)
                        | PickerCandidates::Jumplist(_)
                ) {
                    // Grouped kinds: keep matches in document (candidate) order, not score order,
                    // so each group's rows stay a contiguous run the client can put a single
                    // header above — GitChanges' per-file hunks, Keybindings' per-group bindings
                    // (shipped pre-bucketed), the jumplist's carried source groups. The fuzzy score
                    // only decides which rows survive.
                    let mut keep: Vec<u32> = scored.into_iter().map(|(_, i)| i).collect();
                    keep.sort_unstable();
                    self.ranked = keep;
                } else {
                    // Higher score first; ties fall back to candidate order for determinism.
                    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                    self.ranked.extend(scored.into_iter().map(|(_, i)| i));
                }
            }
            MatchStrategy::PrefixSmartcase => {
                // Shell-tab-completion style: the typed query is a literal prefix of the entry
                // name. Natural candidate order preserved (dirs-then-files, alphabetical
                // within each, as the listing builder produced it).
                //
                // Explorer path-peeking: the query is a path. Its part *after* the last `/` is the
                // prefix filter applied here; the part before it selected which directory got
                // listed (handled when the listing was built, server-side). So `src/ma` matches
                // entries of the already-listed `src` against `ma`. ExplorerRoots has no path
                // component — match the whole query against root basenames.
                let effective_query: &str =
                    if matches!(&self.candidates, PickerCandidates::Explorer(_)) {
                        explorer_query_split(&self.query).1
                    } else {
                        &self.query
                    };
                let (qc, case_insensitive) = smartcase_query(effective_query);
                let mut buf = String::new();
                for i in 0..self.candidates.len() {
                    let name = self.candidates.display_at(i);
                    let starts = if case_insensitive {
                        buf.clear();
                        buf.extend(name.chars().flat_map(char::to_lowercase));
                        buf.starts_with(qc.as_str())
                    } else {
                        name.starts_with(qc.as_str())
                    };
                    if starts {
                        self.ranked.push(i as u32);
                    }
                }
            }
            MatchStrategy::RegexContent => {
                // GitChanges: keep every candidate (in document order) whose diff content the query
                // regex matches, after the path-scope chip filter (`passes`). The empty-query case
                // is handled by the natural-order short-circuit above, so the query is non-empty
                // here — a `None` regex therefore means an unparseable pattern, which matches
                // nothing (the picker shows no results until the regex is valid).
                let PickerCandidates::GitChanges(v) = &self.candidates else {
                    return;
                };
                let Some(re) = self.content_regex() else {
                    return;
                };
                for (i, c) in v.iter().enumerate() {
                    if passes(&self.candidates, i) && c.matches(&re) {
                        self.ranked.push(i as u32);
                    }
                }
            }
            MatchStrategy::Preserved => unreachable!("handled above"),
        }
    }

    /// The compiled content-search regex for the current query + filter options (GitChanges).
    /// `None` for an empty query *or* an unparseable pattern — callers distinguish: the empty case
    /// is handled before this is reached (natural order / default preview), so a `None` here during
    /// matching means "invalid regex → no matches".
    pub(crate) fn content_regex(&self) -> Option<regex::Regex> {
        if self.query.is_empty() {
            return None;
        }
        build_match_regex(&self.query, &self.filters.match_options()).ok()
    }

    /// Locate a ranked index for `item` (used by `view { center_on }`). Returns `None` if the
    /// item is no longer present (file deleted, buffer closed, no longer matches the query, ...).
    pub fn rank_of(&self, item: &PickerItem) -> Option<u32> {
        let cand_idx = self.candidates.position_of(item)? as u32;
        self.ranked
            .iter()
            .position(|&ci| ci == cand_idx)
            .map(|p| p as u32)
    }

    /// Build the items + match indices for the subscribed window. Returns the slice items and
    /// the effective offset (clamped to the total). For the collapsible kinds the window is in
    /// row space — `Group` header rows interleaved with the expanded run's items
    /// ([`Self::build_window_rows`]); for everything else it's the bare ranked slice.
    pub fn build_window_items(
        &self,
        offset: u32,
        limit: u32,
        matcher: &mut Matcher,
    ) -> (u32, Vec<PickerItem>) {
        if let Some(layout) = self.row_layout() {
            return self.build_window_rows(&layout, offset, limit, matcher);
        }
        let total = self.ranked.len() as u32;
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        let ctx = self.item_ctx();
        let mut buf = Vec::new();
        let items = (start..end)
            .map(|rank| self.window_item(rank, &ctx, matcher, &mut buf))
            .collect();
        (start, items)
    }

    /// The collapsible kinds' window builder (docs/picker-groups.md): rows, not bare items —
    /// one [`PickerItem::Group`] header row per run, the expanded run's items inline after
    /// its header.
    fn build_window_rows(
        &self,
        layout: &RowLayout,
        offset: u32,
        limit: u32,
        matcher: &mut Matcher,
    ) -> (u32, Vec<PickerItem>) {
        let total = layout.total_rows();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        let ctx = self.item_ctx();
        let mut buf = Vec::new();
        let mut items: Vec<PickerItem> = Vec::with_capacity((end - start) as usize);
        for row in start..end {
            match layout.row_ref(row).expect("row < total") {
                RowRef::Header(pos) => {
                    let run = layout.runs[pos];
                    items.push(PickerItem::Group {
                        header: self
                            .group_header_at(self.ranked[run.start as usize] as usize)
                            .expect("collapsible kinds key every row"),
                        count: run.len,
                        expanded: layout.expanded == Some(pos),
                    });
                }
                RowRef::Item(rank) => items.push(self.window_item(rank, &ctx, matcher, &mut buf)),
            }
        }
        (start, items)
    }

    /// Per-window item-building context, hoisted once per window (see [`Self::window_item`]):
    /// the GitChanges preview regex, the fuzzy highlight pattern, and the prefix-highlight
    /// length. Shared by the flat and row-space window builders so the two can't drift.
    fn item_ctx(&self) -> ItemCtx {
        let strategy = self.candidates.match_strategy();
        let query_active = !self.query.is_empty();
        // For prefix-match highlighting, count chars in the *effective* query — for Explorer
        // that's only the filter part (after the last `/`), since the path part selected the
        // listing rather than matching an entry. (E.g. `src/ma` against entry `main.rs` should
        // highlight `ma`, not 6 chars.)
        let prefix_len: u32 = if query_active && strategy == MatchStrategy::PrefixSmartcase {
            let effective = if matches!(&self.candidates, PickerCandidates::Explorer(_)) {
                explorer_query_split(&self.query).1
            } else {
                &self.query
            };
            effective.chars().count() as u32
        } else {
            0
        };
        ItemCtx {
            re: matches!(self.candidates, PickerCandidates::GitChanges(_))
                .then(|| self.content_regex())
                .flatten(),
            pattern: (query_active && strategy == MatchStrategy::Fuzzy)
                .then(|| Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart)),
            prefix_len,
            query_active,
        }
    }

    /// The item at ranked position `rank`, with its match-index highlighting. The indices'
    /// source depends on the strategy: fuzzy → nucleo's `indices` helper; prefix → the leading
    /// N chars of the name; preserved → none (Grep candidates carry their own ripgrep-computed
    /// indices, applied inside `make_item`).
    fn window_item(
        &self,
        rank: u32,
        ctx: &ItemCtx,
        matcher: &mut Matcher,
        buf: &mut Vec<char>,
    ) -> PickerItem {
        let idx = self.ranked[rank as usize] as usize;
        // GitChanges previews the query-matched line (not the path), so it builds its items
        // directly rather than going through the path/name match-index machinery below.
        if let PickerCandidates::GitChanges(v) = &self.candidates {
            let c = &v[idx];
            let (preview, match_indices) = c.preview(ctx.re.as_ref());
            return PickerItem::GitChange {
                path_index: c.path_index,
                relative_path: c.relative_path.clone(),
                hunk_index: c.hunk_index,
                line: c.line,
                stage: c.stage,
                added: c.added,
                removed: c.removed,
                preview,
                match_indices,
            };
        }
        let match_indices: Vec<u32> = if let Some(pat) = ctx.pattern.as_ref() {
            let haystack = Utf32Str::new(self.candidates.display_at(idx), buf);
            let mut indices: Vec<u32> = Vec::new();
            pat.indices(haystack, matcher, &mut indices);
            indices.sort_unstable();
            indices.dedup();
            indices
        } else if ctx.prefix_len > 0 {
            let name_chars = self.candidates.display_at(idx).chars().count() as u32;
            (0..ctx.prefix_len.min(name_chars)).collect()
        } else {
            Vec::new()
        };
        let mut item = self.candidates.make_item(idx, match_indices);
        // A symbol in the filtered ranked set that didn't match the query is an ancestor pulled
        // in for tree context (see `rerank`): flag it so the client renders it dim + skips it.
        if ctx.query_active {
            if let PickerItem::Symbol {
                context,
                match_indices,
                ..
            } = &mut item
            {
                *context = match_indices.is_empty();
            }
        }
        item
    }

    /// Total candidates the picker is matching against (whether matched or not).
    pub fn total_candidates(&self) -> u32 {
        self.candidates.len() as u32
    }

    /// The group key of ranked row `ci`, for the header-grouped kinds — `(path_index,
    /// relative_path)` for the file-grouped kinds, a synthetic `(is_definition, "")` section
    /// key for References, and the binding group for Keybindings. Only equality matters. The
    /// borrowing, allocation-free sibling of [`Self::group_header_at`] (which builds the
    /// wire-shaped header the key summarizes) — `grouped_display_metrics` walks the *whole*
    /// ranked list with this; the window-sized span payload uses the owned form. The two MUST
    /// group identically.
    fn group_key_at(&self, ci: usize) -> Option<(u32, &str)> {
        match &self.candidates {
            PickerCandidates::Grep(v) => Some((v[ci].path_index, v[ci].relative_path.as_str())),
            PickerCandidates::GitChanges(v) => {
                Some((v[ci].path_index, v[ci].relative_path.as_str()))
            }
            PickerCandidates::Diagnostics(v) => {
                Some((v[ci].path_index, v[ci].relative_path.as_str()))
            }
            PickerCandidates::References(v) => Some((v[ci].is_definition as u32, "")),
            // Must agree with `group_header_at`'s `Label`: same discriminant convention as the
            // jumplist's label groups.
            PickerCandidates::WorkspaceSymbols(v) => Some((u32::MAX, v[ci].display_path.as_str())),
            PickerCandidates::Keybindings(v) => Some((0, v[ci].entry.group.as_str())),
            // Entries carry their source picker's header — capture makes grouping total
            // (`jumplist::assign_file_groups`), which the collapsible row space relies on
            // ("collapsible kinds key every row").
            PickerCandidates::Jumplist(v) => v[ci].group.as_ref().map(|g| match g {
                GroupHeader::File {
                    path_index,
                    relative_path,
                } => (*path_index, relative_path.as_str()),
                GroupHeader::Label { label } => (u32::MAX, label.as_str()),
            }),
            _ => None,
        }
    }

    /// The wire-shaped group header of ranked row `ci` (see [`Self::group_key_at`]).
    fn group_header_at(&self, ci: usize) -> Option<GroupHeader> {
        match &self.candidates {
            PickerCandidates::Grep(v) => Some(GroupHeader::File {
                path_index: v[ci].path_index,
                relative_path: v[ci].relative_path.clone(),
            }),
            PickerCandidates::GitChanges(v) => Some(GroupHeader::File {
                path_index: v[ci].path_index,
                relative_path: v[ci].relative_path.clone(),
            }),
            PickerCandidates::Diagnostics(v) => Some(GroupHeader::File {
                path_index: v[ci].path_index,
                relative_path: v[ci].relative_path.clone(),
            }),
            // A `Label`, not a `File`, header: a symbol can come from a dependency outside every
            // root, where no `path_index` exists to render a root label from. `display_path` is
            // already "workspace-relative when it can be, absolute otherwise", which is exactly the
            // header text either way. (Cost: no sticky pinning — see `groups_by_file`.)
            PickerCandidates::WorkspaceSymbols(v) => Some(GroupHeader::Label {
                label: v[ci].display_path.clone(),
            }),
            PickerCandidates::References(v) => Some(GroupHeader::Label {
                label: if v[ci].is_definition {
                    "Definition".into()
                } else {
                    "References".into()
                },
            }),
            PickerCandidates::Keybindings(v) => Some(GroupHeader::Label {
                label: v[ci].entry.group.clone(),
            }),
            PickerCandidates::Jumplist(v) => v[ci].group.clone(),
            _ => None,
        }
    }

    /// The collapsible kinds' row-space layout ([`RowLayout`], docs/picker-groups.md); `None`
    /// for every other kind. Derived per call from `ranked` — an O(n) walk, the same cost
    /// [`Self::grouped_display_metrics`] pays per push for the derived-header kinds — never
    /// stored: `ranked` is rebuilt by streaming re-ranks and the layout must always describe
    /// the current permutation.
    pub fn row_layout(&self) -> Option<RowLayout> {
        if !self.kind.collapsible() {
            return None;
        }
        let mut runs: Vec<GroupRun> = Vec::new();
        let mut last: Option<(u32, &str)> = None;
        for (pos, &ci) in self.ranked.iter().enumerate() {
            let key = self
                .group_key_at(ci as usize)
                .expect("collapsible kinds key every row");
            if last != Some(key) {
                last = Some(key);
                runs.push(GroupRun {
                    start: pos as u32,
                    len: 0,
                });
            }
            runs.last_mut().expect("just pushed").len += 1;
        }
        let expanded = self
            .expanded
            .as_ref()
            .and_then(|key| self.run_of_key(&runs, key))
            // No key, or its group re-ranked away: the first run opens itself — exactly one
            // group is expanded whenever there are groups (docs/picker-groups.md §9).
            .or((!runs.is_empty()).then_some(0));
        Some(RowLayout { runs, expanded })
    }

    /// Position in `runs` of the run whose group key is `key`, if it's in the current ranking.
    fn run_of_key(&self, runs: &[GroupRun], key: &(u32, String)) -> Option<usize> {
        runs.iter().position(|r| {
            self.group_key_at(self.ranked[r.start as usize] as usize)
                == Some((key.0, key.1.as_str()))
        })
    }

    /// Total window rows: bare `ranked` items for the flat / derived-header kinds; headers plus
    /// the expanded run's items for the collapsible kinds, whose whole offset/window/selection
    /// space counts rows (docs/picker-groups.md).
    pub fn total_rows(&self) -> u32 {
        match self.row_layout() {
            Some(layout) => layout.total_rows(),
            None => self.ranked.len() as u32,
        }
    }

    /// The absolute window row `item` is reachable at: its rank for the flat / derived-header
    /// kinds; for the collapsible kinds a `Group` resolves to its run's header row, any other
    /// item to its own row when its run is expanded — else to the covering header row (the
    /// item itself is hidden). `None` when the item (or a `Group`'s run) isn't in the current
    /// result set.
    pub fn row_of(&self, item: &PickerItem) -> Option<u32> {
        let Some(layout) = self.row_layout() else {
            return self.rank_of(item);
        };
        if let PickerItem::Group { header, .. } = item {
            let pos = self.run_of_key(&layout.runs, &group_key_of_header(header))?;
            return Some(layout.header_row(pos));
        }
        Some(layout.row_of_rank(self.rank_of(item)?))
    }

    /// The candidate index of `key`'s first ranked item — for anchoring operations that need a
    /// real item (jumplist capture) when the selection sits on the group's header row. `None`
    /// when the kind doesn't collapse or the group isn't in the current ranking.
    pub fn first_candidate_of_group(&self, key: &(u32, String)) -> Option<usize> {
        let layout = self.row_layout()?;
        let pos = self.run_of_key(&layout.runs, key)?;
        Some(self.ranked[layout.runs[pos].start as usize] as usize)
    }

    /// The group key of the run adjacent to the expanded one — `picker/set_group { step }`,
    /// the group-level `Alt-j`/`Alt-k` (docs/picker-groups.md §9). Resolved here, against the
    /// full run list, so stepping works past the client's fetched window. `None` when the
    /// step runs off the ends (group navigation stops there, like the jumplist's `]`/`[`),
    /// the result set is empty, or the kind doesn't collapse.
    pub fn step_group_key(&self, direction: Direction) -> Option<(u32, String)> {
        let layout = self.row_layout()?;
        let cur = layout.expanded?;
        let target = match direction {
            Direction::Forward => cur.checked_add(1)?,
            Direction::Backward => cur.checked_sub(1)?,
        };
        let run = layout.runs.get(target)?;
        self.group_key_at(self.ranked[run.start as usize] as usize)
            .map(|(pi, path)| (pi, path.to_string()))
    }

    /// The owned group key of the group `item` belongs to — a `Group` row keys by its header,
    /// any other item by the candidate it resolves to. `None` when the item isn't in the
    /// candidate set (or the kind doesn't group).
    pub fn group_key_of_item(&self, item: &PickerItem) -> Option<(u32, String)> {
        if let PickerItem::Group { header, .. } = item {
            return Some(group_key_of_header(header));
        }
        let ci = self.candidates.position_of(item)?;
        self.group_key_at(ci)
            .map(|(pi, path)| (pi, path.to_string()))
    }

    /// The group runs of the window `[offset, offset + len)`, with window-relative starts: one
    /// span per group transition and — the invariant clients rely on — always one at
    /// `start: 0` for a non-empty window, so a window beginning mid-group repeats the split
    /// group's header and is self-describing (this replaces the clients' old "synthesize a
    /// header above the first row" convention). Empty for the kinds that don't render group
    /// headers — gated on the *kind*: the buffer-locked GitChangesFile shares GitChanges'
    /// candidate shape but renders headerless. For the collapsible kinds `offset`/`len` are in
    /// row space and the spans carry the count/expanded decoration
    /// ([`Self::build_row_window_spans`]).
    pub fn build_window_spans(&self, offset: u32, len: usize) -> Vec<GroupSpan> {
        if !self.kind.renders_group_headers() {
            return Vec::new();
        }
        if let Some(layout) = self.row_layout() {
            return self.build_row_window_spans(&layout, offset, len);
        }
        let mut spans: Vec<GroupSpan> = Vec::new();
        let mut last_key: Option<(u32, &str)> = None;
        for rel in 0..len {
            let ci = self.ranked[offset as usize + rel] as usize;
            let Some(key) = self.group_key_at(ci) else {
                continue;
            };
            if last_key != Some(key) {
                last_key = Some(key);
                let header = self
                    .group_header_at(ci)
                    .expect("group_key_at and group_header_at cover the same kinds");
                spans.push(GroupSpan {
                    start: rel as u32,
                    header,
                    count: None,
                    expanded: None,
                });
            }
        }
        spans
    }

    /// Row-space spans for the collapsible kinds: one span per run whose region intersects the
    /// window, starting at its header row — and, keeping the leading-span invariant, at `0`
    /// for a window that begins inside the expanded run's items (the split run's header is
    /// repeated so the window stays self-describing; it's what a shell's sticky pin renders).
    fn build_row_window_spans(
        &self,
        layout: &RowLayout,
        offset: u32,
        len: usize,
    ) -> Vec<GroupSpan> {
        let mut spans: Vec<GroupSpan> = Vec::new();
        for rel in 0..len as u32 {
            match layout.row_ref(offset + rel) {
                Some(RowRef::Header(pos)) => spans.push(self.run_span(layout, pos, rel)),
                Some(RowRef::Item(_)) if rel == 0 => {
                    let pos = layout
                        .expanded
                        .expect("item rows only exist in the expanded run");
                    spans.push(self.run_span(layout, pos, 0));
                }
                _ => {}
            }
        }
        spans
    }

    /// The span for run `pos`, placed at window-relative row `start` — carrying the same
    /// count/expanded decoration as the run's [`PickerItem::Group`] row, so a sticky pin
    /// standing in for the scrolled-off header renders identically to it.
    fn run_span(&self, layout: &RowLayout, pos: usize, start: u32) -> GroupSpan {
        let run = layout.runs[pos];
        GroupSpan {
            start,
            header: self
                .group_header_at(self.ranked[run.start as usize] as usize)
                .expect("collapsible kinds key every row"),
            count: Some(run.len),
            expanded: Some(layout.expanded == Some(pos)),
        }
    }

    /// The ranked-space position `picker/section_jump` lands on from `from`, for any
    /// header-grouped kind: `Forward` → the first row of the next group; `Backward` → the
    /// current group's first row, or the previous group's first row when already there
    /// (vim-`{` feel). `None` when there's no further boundary that way, or the kind doesn't
    /// group. Grouping comes from [`Self::group_key_at`] — the same source as the pushed spans,
    /// so the jump always lands where a client renders a header.
    pub fn group_boundary(&self, from: usize, direction: Direction) -> Option<usize> {
        if !self.kind.renders_group_headers() || self.ranked.is_empty() {
            return None;
        }
        let from = from.min(self.ranked.len() - 1);
        let key = |pos: usize| self.group_key_at(self.ranked[pos] as usize);
        let cur = key(from)?;
        match direction {
            Direction::Forward => (from + 1..self.ranked.len()).find(|&j| key(j) != Some(cur)),
            Direction::Backward => {
                let mut run_start = from;
                while run_start > 0 && key(run_start - 1) == Some(cur) {
                    run_start -= 1;
                }
                if run_start < from {
                    return Some(run_start);
                }
                // Already at the group's first row: step to the previous group's first row.
                let prev = key(run_start.checked_sub(1)?)?;
                let mut j = run_start - 1;
                while j > 0 && key(j - 1) == Some(prev) {
                    j -= 1;
                }
                Some(j)
            }
        }
    }

    /// Grouped display-row metrics for a window starting at ranked index `offset`: the display-row
    /// index of that item (one header row per group is interleaved above the rows) and the
    /// total display rows (`ranked.len()` rows + the number of groups). `None` for the kinds
    /// that don't render group headers. Mirrors the clients' header-per-group rendering so their
    /// virtual-scroll spacer + positioning are exact. Display rows are an abstract uniform
    /// unit — clients map them to terminal lines / `ROW_H` / measured pixel heights.
    fn grouped_display_metrics(&self, offset: u32) -> Option<(u32, u32)> {
        if !self.kind.renders_group_headers() {
            return None;
        }
        // Collapsible kinds: the row space IS the display space — headers are real window
        // rows (docs/picker-groups.md) — so there is nothing to reconcile.
        if let Some(layout) = self.row_layout() {
            return Some((offset, layout.total_rows()));
        }
        self.group_key_at(*self.ranked.first()? as usize)?; // bail for non-grouped kinds (and empty sets)
        let mut total_groups = 0u32;
        let mut headers_at_or_before = 0u32;
        let mut prev: Option<(u32, &str)> = None;
        for (rank, &ci) in self.ranked.iter().enumerate() {
            let key = self.group_key_at(ci as usize);
            if prev != key {
                total_groups += 1;
                prev = key;
                if (rank as u32) <= offset {
                    headers_at_or_before += 1;
                }
            }
        }
        Some((
            offset + headers_at_or_before,
            self.ranked.len() as u32 + total_groups,
        ))
    }
}

/// Construct a `PickerUpdateParams` for the current window. Mirrors `build_window_items` plus
/// the metadata fields. Caller is responsible for `generation` matching the latest query.
pub fn build_update(state: &PickerState, matcher: &mut Matcher) -> Option<PickerUpdateParams> {
    let window = state.subscribed?;
    let (offset, items) = state.build_window_items(window.offset, window.limit, matcher);
    let (display_offset, total_display_rows) = match state.grouped_display_metrics(offset) {
        Some((d, t)) => (Some(d), Some(t)),
        None => (None, None),
    };
    let groups = state.build_window_spans(offset, items.len());
    // The expanded run's absolute geometry, for the client's local two-level navigation math
    // (docs/picker-groups.md §9). `None` for the non-collapsible kinds and empty result sets.
    let expanded_run = state.row_layout().and_then(|layout| {
        layout
            .expanded
            .map(|e| aether_protocol::picker::ExpandedRun {
                header_row: layout.header_row(e),
                len: layout.runs[e].len,
            })
    });
    Some(PickerUpdateParams {
        kind: state.kind,
        generation: state.generation,
        offset,
        items: Some(items),
        total_matches: state.ranked.len() as u32,
        total_candidates: state.total_candidates(),
        ticking: false,
        groups,
        display_offset,
        total_display_rows,
        expanded_run,
        // Set by callers that resolve a cursor-based highlight (DocumentSymbols' async fill).
        center_on: None,
        // Explorer-only; false (skipped on the wire) for every other kind.
        explorer_peek_missing: matches!(state.candidates, PickerCandidates::Explorer(_))
            && state.explorer_peek_missing,
    })
}

/// Construct a `Matcher` with path-matching tuning. Called once and stored in `ServerState`;
/// callers borrow it mutably per picker operation.
pub fn make_matcher() -> Matcher {
    Matcher::new(Config::DEFAULT.match_paths())
}

/// Smartcase normalization for the Explorer picker's prefix matcher. Returns the query the
/// caller should compare against and whether comparisons need a lowercased haystack. The
/// query is lowercased iff it contains no uppercase letters — matching the convention nucleo
/// uses for the other pickers (`CaseMatching::Smart`).
fn smartcase_query(query: &str) -> (String, bool) {
    let has_upper = query.chars().any(|c| c.is_uppercase());
    if has_upper {
        (query.to_string(), false)
    } else {
        (query.chars().flat_map(char::to_lowercase).collect(), true)
    }
}

/// Resolve a `picker/select` item to its per-kind result. Returns `None` if the item is no
/// longer in the candidate set the picker last ranked against, *or* if the item exists but
/// isn't selectable (e.g. an Explorer directory entry — those navigate via `picker/view`).
/// A `Group` header row resolves to its run's *first item* (docs/picker-groups.md §9):
/// `Enter` on a header is a jump to the group's top hit, not a disclosure gesture.
pub fn resolve_select(state: &PickerState, item: &PickerItem) -> Option<PickerSelectResult> {
    let idx = if let PickerItem::Group { header, .. } = item {
        state.first_candidate_of_group(&group_key_of_header(header))?
    } else {
        state.candidates.position_of(item)?
    };
    // GitChanges lands on the line that matched the active query (the previewed line), not the
    // hunk's anchor — so accepting a content search jumps to the match.
    if let PickerCandidates::GitChanges(v) = &state.candidates {
        return Some(git_change_select(&v[idx], state.content_regex().as_ref()));
    }
    state.candidates.select_result(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lsp_candidates() -> PickerCandidates {
        PickerCandidates::LspServers(vec![
            LspServerCandidate {
                name: "rust-analyzer".into(),
                language: "rust".into(),
                workspace_root: "/proj".into(),
                root_label: String::new(),
                status: LspStatus::Ready,
                progress: Vec::new(),
            },
            LspServerCandidate {
                name: "gopls".into(),
                language: "go".into(),
                workspace_root: "/proj/svc".into(),
                root_label: "svc".into(),
                status: LspStatus::Starting,
                progress: Vec::new(),
            },
        ])
    }

    #[test]
    fn explorer_query_split_partitions_at_last_slash() {
        assert_eq!(explorer_query_split(""), ("", ""));
        assert_eq!(explorer_query_split("ma"), ("", "ma"));
        assert_eq!(explorer_query_split("src/"), ("src", ""));
        assert_eq!(explorer_query_split("src/ma"), ("src", "ma"));
        assert_eq!(explorer_query_split("a/b/c"), ("a/b", "c"));
        assert_eq!(explorer_query_split("a/b/"), ("a/b", ""));
    }

    #[test]
    fn lsp_server_candidates_round_trip_to_items() {
        let c = lsp_candidates();
        assert_eq!(c.kind(), PickerKind::LspServers);
        assert_eq!(c.len(), 2);
        // The name is the fuzzy haystack, and servers fuzzy-match like the other small lists.
        assert_eq!(c.display_at(0), "rust-analyzer");
        assert_eq!(c.match_strategy(), MatchStrategy::Fuzzy);
        match c.make_item(0, vec![0, 1]) {
            PickerItem::LspServer {
                name,
                language,
                workspace_root,
                root_label,
                status,
                progress: _,
                match_indices,
            } => {
                assert_eq!(name, "rust-analyzer");
                assert_eq!(language, "rust");
                assert_eq!(workspace_root, "/proj");
                assert_eq!(root_label, ""); // rooted at the workspace root → no label
                assert_eq!(status, LspStatus::Ready);
                assert_eq!(match_indices, vec![0, 1]);
            }
            other => panic!("expected LspServer, got {other:?}"),
        }
        // The sub-rooted server carries its relative label.
        match c.make_item(1, vec![]) {
            PickerItem::LspServer { root_label, .. } => assert_eq!(root_label, "svc"),
            other => panic!("expected LspServer, got {other:?}"),
        }
    }

    #[test]
    fn lsp_server_identity_is_language_and_root() {
        let c = lsp_candidates();
        // Round-trips by (language, workspace_root).
        assert_eq!(c.position_of(&c.make_item(1, vec![])), Some(1));
        // Same language, different root → no match (monorepo dual-root case).
        let elsewhere = PickerItem::LspServer {
            name: "ignored".into(),
            language: "go".into(),
            workspace_root: "/elsewhere".into(),
            root_label: "elsewhere".into(),
            status: LspStatus::Ready,
            progress: vec![],
            match_indices: vec![],
        };
        assert_eq!(c.position_of(&elsewhere), None);
    }

    fn keybinding_candidates() -> PickerCandidates {
        let entries = [
            ("Editing", "Delete word back", "Any", "Ctrl-w"),
            ("Pickers", "Find file", "Application", "Space f"),
            ("Movement", "Word forward", "Normal", "w"),
        ];
        PickerCandidates::Keybindings(
            entries
                .into_iter()
                .map(|(group, desc, mode, keys)| {
                    KeybindingEntry {
                        group: group.into(),
                        desc: desc.into(),
                        mode: mode.into(),
                        keys: keys.into(),
                    }
                    .into()
                })
                .collect(),
        )
    }

    #[test]
    fn keybinding_candidates_round_trip_to_items() {
        let c = keybinding_candidates();
        assert_eq!(c.kind(), PickerKind::Keybindings);
        assert_eq!(c.len(), 3);
        // The haystack is the composed row — description + chord (and the mode, on the
        // Insert/Search rows that show one). The group is a section header, not row text,
        // so it isn't matched.
        assert_eq!(c.display_at(0), "Delete word back Ctrl-w");
        assert_eq!(c.match_strategy(), MatchStrategy::Fuzzy);
        match c.make_item(1, vec![0, 1]) {
            PickerItem::Keybinding {
                group,
                desc,
                mode,
                keys,
                match_indices,
            } => {
                assert_eq!(group, "Pickers");
                assert_eq!(desc, "Find file");
                assert_eq!(mode, "Application");
                assert_eq!(keys, "Space f");
                assert_eq!(match_indices, vec![0, 1]);
            }
            other => panic!("expected Keybinding, got {other:?}"),
        }
        // Informational rows aren't selectable — Enter closes client-side, `select` never fires.
        assert!(c.select_result(0).is_none());
    }

    #[test]
    fn keybindings_group_metrics_and_query_keep_candidate_order() {
        // Two Editing rows then a Motion row — one section header per group run, mirroring the
        // client's `display_rows`, so the virtual-scroll row math lines up.
        let kb = |group: &str, desc: &str, keys: &str| {
            KeybindingCandidate::from(KeybindingEntry {
                group: group.into(),
                desc: desc.into(),
                mode: "Any".into(),
                keys: keys.into(),
            })
        };
        let cands = PickerCandidates::Keybindings(vec![
            kb("Editing", "Delete selection", "Ctrl-d"),
            kb("Editing", "Undo", "Ctrl-z"),
            kb("Motion", "Word forward", "w"),
        ]);
        let mut s = PickerState::new(cands);
        let mut m = make_matcher();
        s.rerank(&mut m);

        // 3 items + 2 group headers = 5 display rows; a window at ranked row 0 sits below the
        // first header, one at ranked row 2 sits below both.
        let (display_offset, total) = s
            .grouped_display_metrics(0)
            .expect("keybindings are header-grouped");
        assert_eq!(total, 5, "3 items + 2 group headers");
        assert_eq!(display_offset, 1);
        let (display_offset, _) = s.grouped_display_metrics(2).unwrap();
        assert_eq!(display_offset, 4);

        // A query keeps candidate (bucketed) order — the score only picks survivors — so each
        // group stays a contiguous run under its single header.
        s.query = "d".into();
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![0, 1, 2], "document order, not score order");
    }

    #[test]
    fn window_spans_repeat_the_split_groups_header() {
        let kb = |group: &str, desc: &str| {
            KeybindingCandidate::from(KeybindingEntry {
                group: group.into(),
                desc: desc.into(),
                mode: "Any".into(),
                keys: "x".into(),
            })
        };
        let cands = PickerCandidates::Keybindings(vec![
            kb("Editing", "Delete selection"),
            kb("Editing", "Undo"),
            kb("Motion", "Word forward"),
        ]);
        let mut s = PickerState::new(cands);
        let mut m = make_matcher();
        s.rerank(&mut m);

        // Full window: one span per group, in window-relative starts.
        let label = |h: &GroupHeader| match h {
            GroupHeader::Label { label } => label.clone(),
            GroupHeader::File { relative_path, .. } => relative_path.clone(),
        };
        let spans = s.build_window_spans(0, 3);
        assert_eq!(
            spans
                .iter()
                .map(|sp| (sp.start, label(&sp.header)))
                .collect::<Vec<_>>(),
            [(0, "Editing".to_string()), (2, "Motion".to_string())]
        );
        // A window starting mid-group repeats the split group's header at start 0, so the
        // window is self-describing (no client-side "synthesize a header" convention).
        let spans = s.build_window_spans(1, 2);
        assert_eq!(
            spans
                .iter()
                .map(|sp| (sp.start, label(&sp.header)))
                .collect::<Vec<_>>(),
            [(0, "Editing".to_string()), (1, "Motion".to_string())]
        );
        // Header-less kinds send no spans, even when the candidate shape could group: the
        // buffer-locked GitChangesFile gate is the kind, not the data.
        s.kind = PickerKind::GitChangesFile;
        assert!(s.build_window_spans(0, 3).is_empty());
    }

    #[test]
    fn group_boundary_jumps_by_group_run() {
        // Three files in walker order: a.rs (3 hits), b.rs (1 hit), c.rs (2 hits) — the exact
        // semantics `picker/section_jump` (Alt-l / Alt-h) relies on, for every grouped kind.
        let hit = |rel: &str, line: u32| GrepHitCandidate {
            path_index: 0,
            relative_path: rel.to_string(),
            abs_path: format!("/ws/{rel}"),
            line,
            col: 0,
            match_byte_len: 1,
            preview: String::new(),
            match_indices: Vec::new(),
        };
        let cands = PickerCandidates::Grep(vec![
            hit("a.rs", 1),
            hit("a.rs", 5),
            hit("a.rs", 9), // indices 0,1,2
            hit("b.rs", 2), // index 3
            hit("c.rs", 1),
            hit("c.rs", 4), // indices 4,5
        ]);
        let mut s = PickerState::new(cands);
        let mut m = make_matcher();
        s.rerank(&mut m);

        // Forward: the next group's first row; nothing past the last group.
        assert_eq!(s.group_boundary(0, Direction::Forward), Some(3));
        assert_eq!(s.group_boundary(2, Direction::Forward), Some(3));
        assert_eq!(s.group_boundary(3, Direction::Forward), Some(4));
        assert_eq!(s.group_boundary(4, Direction::Forward), None);
        assert_eq!(s.group_boundary(5, Direction::Forward), None);
        // Backward: this group's first row, else the previous group's (vim-`{` feel).
        assert_eq!(s.group_boundary(2, Direction::Backward), Some(0));
        assert_eq!(s.group_boundary(1, Direction::Backward), Some(0));
        assert_eq!(s.group_boundary(4, Direction::Backward), Some(3));
        assert_eq!(s.group_boundary(3, Direction::Backward), Some(0));
        assert_eq!(s.group_boundary(0, Direction::Backward), None);
        // Headerless kinds never jump.
        s.kind = PickerKind::GitChangesFile;
        assert_eq!(s.group_boundary(0, Direction::Forward), None);
    }

    #[test]
    fn keybinding_identity_is_mode_keys_desc() {
        let c = keybinding_candidates();
        assert_eq!(c.position_of(&c.make_item(2, vec![])), Some(2));
        // Same chord in a different mode is a different row.
        let other_mode = PickerItem::Keybinding {
            group: "Movement".into(),
            desc: "Word forward".into(),
            mode: "Insert".into(),
            keys: "w".into(),
            match_indices: vec![],
        };
        assert_eq!(c.position_of(&other_mode), None);
    }

    fn reference_candidates() -> PickerCandidates {
        PickerCandidates::References(vec![
            ReferenceCandidate {
                abs_path: "/proj/src/lib.rs".into(),
                display_path: "src/lib.rs".into(),
                line: 4,
                col: 8,
                end_line: 4,
                end_col: 13, // "helper" spans cols 8..=13
                preview: "    helper();".into(),
                is_definition: false,
            },
            ReferenceCandidate {
                abs_path: "/proj/src/main.rs".into(),
                display_path: "src/main.rs".into(),
                line: 0,
                col: 3,
                end_line: 0,
                end_col: 8, // "helper" spans cols 3..=8
                preview: "fn helper() {}".into(),
                is_definition: true,
            },
        ])
    }

    #[test]
    fn reference_candidates_round_trip_to_items() {
        let c = reference_candidates();
        assert_eq!(c.kind(), PickerKind::References);
        assert_eq!(c.len(), 2);
        // The preview line is the fuzzy haystack, like the grep preview.
        assert_eq!(c.display_at(0), "    helper();");
        assert_eq!(c.match_strategy(), MatchStrategy::Fuzzy);
        match c.make_item(0, vec![4, 5]) {
            PickerItem::Reference {
                path,
                display_path,
                line,
                col,
                preview,
                is_definition,
                match_indices,
            } => {
                assert_eq!(path, "/proj/src/lib.rs");
                assert_eq!(display_path, "src/lib.rs");
                assert_eq!(line, 4);
                assert_eq!(col, 8);
                assert_eq!(preview, "    helper();");
                assert!(!is_definition); // candidate 0 is a use
                assert_eq!(match_indices, vec![4, 5]);
            }
            other => panic!("expected Reference, got {other:?}"),
        }
        // The definition flag rides through make_item too (candidate 1 is the definition).
        assert!(
            matches!(
                c.make_item(1, vec![]),
                PickerItem::Reference {
                    is_definition: true,
                    ..
                }
            ),
            "the definition candidate makes a definition item"
        );
    }

    #[test]
    fn reference_identity_is_path_line_col() {
        let c = reference_candidates();
        // Round-trips by (path, line, col).
        assert_eq!(c.position_of(&c.make_item(1, vec![])), Some(1));
        // Same file, different line → no match (the preview text is irrelevant to identity).
        let elsewhere = PickerItem::Reference {
            path: "/proj/src/lib.rs".into(),
            display_path: "src/lib.rs".into(),
            line: 99,
            col: 8,
            preview: "ignored".into(),
            is_definition: false,
            match_indices: vec![],
        };
        assert_eq!(c.position_of(&elsewhere), None);
    }

    #[test]
    fn reference_selects_to_file_at_with_the_identifier_selected() {
        // Selecting a reference lands the identifier *selected* (like the outline): cursor on the
        // span's last char, anchor at its start.
        match reference_candidates().select_result(1) {
            Some(PickerSelectResult::FileAt {
                path,
                position,
                anchor,
            }) => {
                assert_eq!(path, "/proj/src/main.rs");
                assert_eq!(position, LogicalPosition { line: 0, col: 8 }); // span's last char
                assert_eq!(anchor, Some(LogicalPosition { line: 0, col: 3 })); // span start
            }
            other => panic!("expected FileAt, got {other:?}"),
        }
    }

    #[test]
    fn reference_with_no_distinct_span_lands_a_point() {
        // A zero-width span (end == start) is a point cursor, no selection.
        let c = PickerCandidates::References(vec![ReferenceCandidate {
            abs_path: "/proj/a.rs".into(),
            display_path: "a.rs".into(),
            line: 2,
            col: 5,
            end_line: 2,
            end_col: 5,
            preview: "x".into(),
            is_definition: false,
        }]);
        match c.select_result(0) {
            Some(PickerSelectResult::FileAt {
                position, anchor, ..
            }) => {
                assert_eq!(position, LogicalPosition { line: 2, col: 5 });
                assert_eq!(anchor, None);
            }
            other => panic!("expected FileAt, got {other:?}"),
        }
    }

    #[test]
    fn references_group_into_definition_and_use_sections() {
        // A definition (is_definition) followed by two uses. `grouped_display_metrics` opens one
        // section header per `is_definition` transition — Definition then References — exactly as
        // the client's `display_rows` does, so the virtual-scroll row math lines up.
        let cand = |rel: &str, line: u32, is_definition: bool| ReferenceCandidate {
            abs_path: format!("/proj/{rel}"),
            display_path: rel.into(),
            line,
            col: 0,
            end_line: line,
            end_col: 0,
            preview: "x".into(),
            is_definition,
        };
        let cands = PickerCandidates::References(vec![
            cand("lib.rs", 0, true),
            cand("a.rs", 5, false),
            cand("b.rs", 9, false),
        ]);
        let mut s = PickerState::new(cands);
        let mut m = make_matcher();
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![0, 1, 2], "definition-first, then the uses");

        // Two sections over three rows → 3 + 2 = 5 display rows. A window opening at ranked row 0
        // sits one row down (the Definition header above it).
        let (display_offset, total) = s
            .grouped_display_metrics(0)
            .expect("references are header-grouped");
        assert_eq!(total, 5, "3 items + 2 section headers");
        assert_eq!(
            display_offset, 1,
            "the Definition header precedes ranked row 0"
        );
        // A window opening at the first use is below both headers.
        let (display_offset, total) = s.grouped_display_metrics(1).unwrap();
        assert_eq!(total, 5);
        assert_eq!(
            display_offset, 3,
            "Definition + References headers precede the first use"
        );
    }

    #[test]
    fn references_with_no_definition_are_a_single_section() {
        // No row flagged the definition (server couldn't resolve one): a single References section,
        // so just one header row.
        let cand = |rel: &str| ReferenceCandidate {
            abs_path: format!("/proj/{rel}"),
            display_path: rel.into(),
            line: 1,
            col: 0,
            end_line: 1,
            end_col: 0,
            preview: "x".into(),
            is_definition: false,
        };
        let cands = PickerCandidates::References(vec![cand("a.rs"), cand("b.rs")]);
        let mut s = PickerState::new(cands);
        let mut m = make_matcher();
        s.rerank(&mut m);
        let (display_offset, total) = s.grouped_display_metrics(0).unwrap();
        assert_eq!(total, 3, "2 items + 1 References header");
        assert_eq!(display_offset, 1);
    }

    #[test]
    fn workspace_diagnostics_row_space_metrics() {
        // `PickerCandidates::Diagnostics` backs two kinds: the flat buffer-locked `Diagnostics`
        // (no header accounting at all) and the collapsible `DiagnosticsWorkspace`, whose
        // window space IS the row space (docs/picker-groups.md): display metrics are the
        // identity offset plus the row total — headers per file, items only when expanded.
        let cand = |rel: &str, line: u32| DiagnosticCandidate {
            path_index: 0,
            relative_path: rel.into(),
            line,
            col: 0,
            end_line: line,
            end_col: 0,
            severity: aether_protocol::viewport::DiagnosticSeverity::Error,
            message: "boom".into(),
            abs_path: format!("/proj/{rel}"),
        };
        // Two files: a.rs (two diagnostics) then b.rs (one) → 3 rows + 2 file headers = 5.
        let cands = PickerCandidates::Diagnostics(vec![
            cand("src/a.rs", 2),
            cand("src/a.rs", 9),
            cand("src/b.rs", 4),
        ]);

        // Buffer-locked `Diagnostics` is flat → no header accounting at all.
        let mut flat = PickerState::new(cands.clone());
        flat.rerank(&mut make_matcher());
        assert_eq!(flat.kind, PickerKind::Diagnostics);
        assert_eq!(flat.grouped_display_metrics(0), None);

        // `DiagnosticsWorkspace` is collapsible: exactly one group is always expanded — with
        // no key set, the first run (a.rs) opens itself (docs/picker-groups.md §9).
        let mut proj = PickerState::new(cands);
        proj.kind = PickerKind::DiagnosticsWorkspace;
        proj.rerank(&mut make_matcher());
        assert_eq!(
            proj.grouped_display_metrics(0),
            Some((0, 4)),
            "fresh: 2 headers + the auto-expanded a.rs's 2 diagnostics"
        );
        proj.expanded = Some((0, "src/b.rs".into()));
        assert_eq!(
            proj.grouped_display_metrics(1),
            Some((1, 3)),
            "b.rs selected: 2 headers + its 1 diagnostic; offset stays the identity"
        );
    }

    #[test]
    fn workspace_symbols_rank_at_group_grain() {
        // The collapsible row space needs each file's rows contiguous in ranked order —
        // `row_layout` derives runs from consecutive keys and `run_of_key` resolves a key to
        // its *first* run, so flat score ordering used to split a file around a better-scoring
        // one, render its group twice, and trap the expanded-group resolution in the first
        // copy. Files order by best member score, rows within by own score, no-match files
        // trail; nothing is filtered.
        let cand = |path: &str, line: u32, name: &str| WorkspaceSymbolCandidate {
            abs_path: format!("/w/{path}"),
            path_index: Some(0),
            display_path: path.into(),
            line,
            col: 0,
            name: name.into(),
            symbol_kind: aether_protocol::picker::SymbolKind::Function,
            container: String::new(),
        };
        // Candidate order is the merge's `(display_path, line)` sort. Scores for "boot":
        // the consecutive "boot" far outscores the scattered subsequence matches
        // ("brotli_footer", "bag_of_roots"), and "unrelated" doesn't match at all. Old
        // flat-score ranked order split z_late.rs around a_early.rs.
        let mut s = PickerState::new(PickerCandidates::WorkspaceSymbols(vec![
            cand("a_early.rs", 0, "bag_of_roots"),
            cand("m_mid.rs", 0, "unrelated"),
            cand("z_late.rs", 0, "brotli_footer"),
            cand("z_late.rs", 5, "boot"),
        ]));
        assert_eq!(s.kind, PickerKind::WorkspaceSymbols);
        s.query = "boot".into();
        s.rerank(&mut make_matcher());
        let names: Vec<&str> = s
            .ranked
            .iter()
            .map(|&i| s.candidates.display_at(i as usize))
            .collect();
        assert_eq!(
            names,
            vec!["boot", "brotli_footer", "bag_of_roots", "unrelated"],
            "best-match file first and contiguous (best row leading), no-match file last"
        );
        let layout = s.row_layout().expect("collapsible kind");
        assert_eq!(layout.runs.len(), 3, "one run per file, none duplicated");
        assert_eq!(
            layout.expanded,
            Some(0),
            "fresh rank: the best file's run opens itself"
        );
    }

    #[test]
    fn row_layout_maps_rows_headers_and_ranks() {
        // Three runs: a (2 items), b (3), c (1) — ranked positions 0..6.
        let runs = vec![
            GroupRun { start: 0, len: 2 },
            GroupRun { start: 2, len: 3 },
            GroupRun { start: 5, len: 1 },
        ];
        // Fully collapsed: three header rows; items are unreachable and map to their headers.
        // (Constructed directly — `row_layout()` itself never produces `expanded: None` for a
        // non-empty run list any more (docs/picker-groups.md §9); the None arithmetic stays
        // as the degenerate path.)
        let collapsed = RowLayout {
            runs: runs.clone(),
            expanded: None,
        };
        assert_eq!(collapsed.total_rows(), 3);
        assert_eq!(collapsed.row_ref(0), Some(RowRef::Header(0)));
        assert_eq!(collapsed.row_ref(2), Some(RowRef::Header(2)));
        assert_eq!(collapsed.row_ref(3), None, "past the end");
        assert_eq!(collapsed.row_of_rank(4), 1, "hidden item → its header row");

        // b expanded: rows are [a, b, b0, b1, b2, c].
        let layout = RowLayout {
            runs,
            expanded: Some(1),
        };
        assert_eq!(layout.total_rows(), 6);
        assert_eq!(layout.header_row(0), 0);
        assert_eq!(layout.header_row(1), 1);
        assert_eq!(layout.header_row(2), 5, "c's header sits below b's items");
        assert_eq!(layout.row_ref(1), Some(RowRef::Header(1)));
        assert_eq!(layout.row_ref(2), Some(RowRef::Item(2)), "b's first item");
        assert_eq!(layout.row_ref(4), Some(RowRef::Item(4)), "b's last item");
        assert_eq!(layout.row_ref(5), Some(RowRef::Header(2)));
        assert_eq!(layout.row_ref(6), None);
        assert_eq!(
            layout.row_of_rank(0),
            0,
            "collapsed run's item → its header"
        );
        assert_eq!(layout.row_of_rank(3), 3, "expanded item → its own row");
        assert_eq!(
            layout.row_of_rank(5),
            5,
            "run below the expansion → its header"
        );
    }

    #[test]
    fn collapsible_window_rows_emit_headers_and_expanded_items() {
        let cand = |rel: &str, line: u32| DiagnosticCandidate {
            path_index: 0,
            relative_path: rel.into(),
            line,
            col: 0,
            end_line: line,
            end_col: 0,
            severity: aether_protocol::viewport::DiagnosticSeverity::Error,
            message: "boom".into(),
            abs_path: format!("/proj/{rel}"),
        };
        let mut s = PickerState::new(PickerCandidates::Diagnostics(vec![
            cand("src/a.rs", 2),
            cand("src/a.rs", 9),
            cand("src/b.rs", 4),
        ]));
        s.kind = PickerKind::DiagnosticsWorkspace;
        let mut m = make_matcher();
        s.rerank(&mut m);

        // Fresh (no key): the first run opens itself (docs/picker-groups.md §9) — a.rs's
        // diagnostics slot in under its header, b.rs stays a bare row.
        let (start, items) = s.build_window_items(0, 10, &mut m);
        assert_eq!(start, 0);
        let expect_group = |item: &PickerItem, rel: &str, count: u32, expanded: bool| {
            let PickerItem::Group {
                header: GroupHeader::File { relative_path, .. },
                count: c,
                expanded: e,
            } = item
            else {
                panic!("expected a Group row, got {item:?}");
            };
            assert_eq!((relative_path.as_str(), *c, *e), (rel, count, expanded));
        };
        assert_eq!(items.len(), 4);
        expect_group(&items[0], "src/a.rs", 2, true);
        assert!(matches!(&items[1], PickerItem::Diagnostic { line: 2, .. }));
        assert!(matches!(&items[2], PickerItem::Diagnostic { line: 9, .. }));
        expect_group(&items[3], "src/b.rs", 1, false);

        // Selecting b.rs moves the expansion (accordion): a.rs collapses to a bare header.
        s.expanded = Some((0, "src/b.rs".into()));
        let (_, items) = s.build_window_items(0, 10, &mut m);
        assert_eq!(items.len(), 3);
        expect_group(&items[0], "src/a.rs", 2, false);
        expect_group(&items[1], "src/b.rs", 1, true);
        assert!(matches!(&items[2], PickerItem::Diagnostic { line: 4, .. }));

        // Back to a.rs for the span checks below.
        s.expanded = Some((0, "src/a.rs".into()));

        // A window starting mid-run leads with the split run's span at 0, dressed like its
        // header row; a window starting on a header row places the span at that row.
        let spans = s.build_window_spans(2, 2); // rows 2..4: a.rs's 2nd diag + b.rs's header
        assert_eq!(spans.len(), 2);
        assert_eq!(
            (spans[0].start, spans[0].count, spans[0].expanded),
            (0, Some(2), Some(true))
        );
        assert!(
            matches!(&spans[0].header, GroupHeader::File { relative_path, .. } if relative_path == "src/a.rs")
        );
        assert_eq!(
            (spans[1].start, spans[1].count, spans[1].expanded),
            (1, Some(1), Some(false))
        );
    }

    #[test]
    fn step_group_key_walks_neighbours_and_stops_at_ends() {
        let cand = |rel: &str, line: u32| DiagnosticCandidate {
            path_index: 0,
            relative_path: rel.into(),
            line,
            col: 0,
            end_line: line,
            end_col: 0,
            severity: aether_protocol::viewport::DiagnosticSeverity::Error,
            message: "boom".into(),
            abs_path: format!("/proj/{rel}"),
        };
        let mut s = PickerState::new(PickerCandidates::Diagnostics(vec![
            cand("src/a.rs", 2),
            cand("src/b.rs", 4),
            cand("src/c.rs", 1),
        ]));
        s.kind = PickerKind::DiagnosticsWorkspace;
        s.rerank(&mut make_matcher());

        // No key set: the first run stands expanded (the fallback), so Forward steps to b.
        assert_eq!(
            s.step_group_key(Direction::Forward),
            Some((0, "src/b.rs".into()))
        );
        // ... and Backward runs off the start — a stop, like the jumplist's `[` at its end.
        assert_eq!(s.step_group_key(Direction::Backward), None);

        s.expanded = Some((0, "src/b.rs".into()));
        assert_eq!(
            s.step_group_key(Direction::Forward),
            Some((0, "src/c.rs".into()))
        );
        assert_eq!(
            s.step_group_key(Direction::Backward),
            Some((0, "src/a.rs".into()))
        );

        // At the last run, Forward stops.
        s.expanded = Some((0, "src/c.rs".into()));
        assert_eq!(s.step_group_key(Direction::Forward), None);

        // A key whose group re-ranked away falls back to the first run as the reference.
        s.expanded = Some((0, "src/gone.rs".into()));
        assert_eq!(
            s.step_group_key(Direction::Forward),
            Some((0, "src/b.rs".into()))
        );

        // Empty result set / non-collapsible kinds: no stepping at all.
        let mut empty = PickerState::new(PickerCandidates::Diagnostics(vec![]));
        empty.kind = PickerKind::DiagnosticsWorkspace;
        empty.rerank(&mut make_matcher());
        assert_eq!(empty.step_group_key(Direction::Forward), None);
    }

    #[test]
    fn resolve_select_on_a_group_header_lands_on_its_first_item() {
        let cand = |rel: &str, line: u32| DiagnosticCandidate {
            path_index: 0,
            relative_path: rel.into(),
            line,
            col: 0,
            end_line: line,
            end_col: 0,
            severity: aether_protocol::viewport::DiagnosticSeverity::Error,
            message: "boom".into(),
            abs_path: format!("/proj/{rel}"),
        };
        let mut s = PickerState::new(PickerCandidates::Diagnostics(vec![
            cand("src/a.rs", 2),
            cand("src/a.rs", 9),
            cand("src/b.rs", 4),
        ]));
        s.kind = PickerKind::DiagnosticsWorkspace;
        s.rerank(&mut make_matcher());
        // Enter on b.rs's header: the jump resolves to the group's first diagnostic.
        let header = PickerItem::Group {
            header: GroupHeader::File {
                path_index: 0,
                relative_path: "src/b.rs".into(),
            },
            count: 1,
            expanded: false,
        };
        match resolve_select(&s, &header) {
            Some(PickerSelectResult::FileAt { path, position, .. }) => {
                assert_eq!(path, "/proj/src/b.rs");
                assert_eq!(position.line, 4);
            }
            other => panic!("expected FileAt for the group's first item, got {other:?}"),
        }
        // A header whose group isn't in the ranking resolves to nothing.
        let gone = PickerItem::Group {
            header: GroupHeader::File {
                path_index: 0,
                relative_path: "src/gone.rs".into(),
            },
            count: 0,
            expanded: false,
        };
        assert!(resolve_select(&s, &gone).is_none());
    }

    fn symbol_candidates() -> PickerCandidates {
        use aether_protocol::picker::SymbolKind;
        PickerCandidates::Symbols(vec![
            SymbolCandidate {
                abs_path: "/proj/src/lib.rs".into(),
                start: LogicalPosition { line: 0, col: 3 },
                end: LogicalPosition { line: 0, col: 8 },
                name: "Parser".into(),
                symbol_kind: SymbolKind::Struct,
                detail: String::new(),
                depth: 0,
                range_start: LogicalPosition { line: 0, col: 0 },
                range_end: LogicalPosition { line: 9, col: 1 },
            },
            SymbolCandidate {
                abs_path: "/proj/src/lib.rs".into(),
                start: LogicalPosition { line: 4, col: 7 },
                end: LogicalPosition { line: 4, col: 11 },
                name: "parse".into(),
                symbol_kind: SymbolKind::Method,
                detail: "fn(&self) -> Ast".into(),
                depth: 1,
                range_start: LogicalPosition { line: 4, col: 4 },
                range_end: LogicalPosition { line: 6, col: 5 },
            },
        ])
    }

    #[test]
    fn symbol_candidates_round_trip_to_items() {
        use aether_protocol::picker::SymbolKind;
        let c = symbol_candidates();
        assert_eq!(c.kind(), PickerKind::DocumentSymbols);
        assert_eq!(c.len(), 2);
        // The symbol name is the fuzzy haystack (not a preview line, unlike References).
        assert_eq!(c.display_at(1), "parse");
        assert_eq!(c.match_strategy(), MatchStrategy::Fuzzy);
        match c.make_item(1, vec![0, 1]) {
            PickerItem::Symbol {
                path,
                display_path: _,
                line,
                col,
                name,
                symbol_kind,
                detail,
                depth,
                context,
                match_indices,
            } => {
                assert!(!context); // make_item defaults context off (set by build_window_items)
                assert_eq!(path, "/proj/src/lib.rs");
                assert_eq!(line, 4);
                assert_eq!(col, 7);
                assert_eq!(name, "parse");
                assert_eq!(symbol_kind, SymbolKind::Method);
                assert_eq!(detail, "fn(&self) -> Ast");
                assert_eq!(depth, 1);
                assert_eq!(match_indices, vec![0, 1]);
            }
            other => panic!("expected Symbol, got {other:?}"),
        }
    }

    #[test]
    fn symbol_identity_is_path_line_col() {
        use aether_protocol::picker::SymbolKind;
        let c = symbol_candidates();
        // Round-trips by (path, line, col); name/detail/depth are irrelevant to identity.
        assert_eq!(c.position_of(&c.make_item(0, vec![])), Some(0));
        let elsewhere = PickerItem::Symbol {
            path: "/proj/src/lib.rs".into(),
            display_path: String::new(),
            line: 99,
            col: 0,
            name: "ignored".into(),
            symbol_kind: SymbolKind::Function,
            detail: String::new(),
            depth: 0,
            context: false,
            match_indices: vec![],
        };
        // wrong line → no match
        assert_eq!(c.position_of(&elsewhere), None);
    }

    #[test]
    fn symbol_filter_pulls_in_ancestors_as_context() {
        use aether_protocol::picker::SymbolKind;
        // A struct `Widget` with a nested method `parse`. Filtering to "parse" keeps `parse` (the
        // match) and pulls in `Widget` as a non-matching `context` ancestor for tree context, in
        // document order. (`Widget` doesn't fuzzy-match "parse", so it's purely context.)
        let cands = PickerCandidates::Symbols(vec![
            SymbolCandidate {
                abs_path: "/p/a.rs".into(),
                start: LogicalPosition { line: 0, col: 7 },
                end: LogicalPosition { line: 0, col: 12 },
                name: "Widget".into(),
                symbol_kind: SymbolKind::Struct,
                detail: String::new(),
                depth: 0,
                range_start: LogicalPosition { line: 0, col: 0 },
                range_end: LogicalPosition { line: 9, col: 1 },
            },
            SymbolCandidate {
                abs_path: "/p/a.rs".into(),
                start: LogicalPosition { line: 4, col: 7 },
                end: LogicalPosition { line: 4, col: 11 },
                name: "parse".into(),
                symbol_kind: SymbolKind::Method,
                detail: String::new(),
                depth: 1,
                range_start: LogicalPosition { line: 4, col: 4 },
                range_end: LogicalPosition { line: 6, col: 5 },
            },
        ]);
        let mut s = PickerState::new(cands);
        let mut m = make_matcher();
        s.query = "parse".into();
        s.rerank(&mut m);
        assert_eq!(
            s.ranked,
            vec![0, 1],
            "match (parse, idx 1) + its ancestor (Widget, idx 0)"
        );
        let (_, items) = s.build_window_items(0, 10, &mut m);
        match &items[0] {
            PickerItem::Symbol {
                name,
                context,
                match_indices,
                ..
            } => {
                assert_eq!(name, "Widget");
                assert!(*context, "the unmatched ancestor is a context row");
                assert!(match_indices.is_empty());
            }
            other => panic!("expected Symbol, got {other:?}"),
        }
        match &items[1] {
            PickerItem::Symbol {
                name,
                context,
                match_indices,
                ..
            } => {
                assert_eq!(name, "parse");
                assert!(!*context, "the match is selectable, not context");
                assert!(!match_indices.is_empty(), "the match is highlighted");
            }
            other => panic!("expected Symbol, got {other:?}"),
        }
    }

    #[test]
    fn symbol_selects_to_file_at_with_name_selected() {
        // Selecting a symbol lands its identifier selected: cursor on the name's last char
        // (`end`), anchor at its start (`start`).
        match symbol_candidates().select_result(1) {
            Some(PickerSelectResult::FileAt {
                path,
                position,
                anchor,
            }) => {
                assert_eq!(path, "/proj/src/lib.rs");
                assert_eq!(position, LogicalPosition { line: 4, col: 11 });
                assert_eq!(anchor, Some(LogicalPosition { line: 4, col: 7 }));
            }
            other => panic!("expected FileAt, got {other:?}"),
        }
    }

    #[test]
    fn symbol_contains_cursor_is_range_based() {
        let PickerCandidates::Symbols(v) = symbol_candidates() else {
            unreachable!()
        };
        // A cursor on line 5 is inside both the struct and its nested method (innermost wins when
        // the caller picks the deepest match).
        assert!(v[0].contains(LogicalPosition { line: 5, col: 0 }));
        assert!(v[1].contains(LogicalPosition { line: 5, col: 0 }));
        // Line 8 is past the method but still inside the struct.
        assert!(v[0].contains(LogicalPosition { line: 8, col: 0 }));
        assert!(!v[1].contains(LogicalPosition { line: 8, col: 0 }));
    }

    fn git_change_candidates() -> PickerCandidates {
        PickerCandidates::GitChanges(vec![
            GitChangeCandidate::new(
                0,
                "src/a.rs".into(),
                "/proj/src/a.rs".into(),
                0,
                4,
                DiffStage::Unstaged,
                2,
                1,
                vec!["let x = 1;".into()],
            ),
            GitChangeCandidate::new(
                0,
                "src/a.rs".into(),
                "/proj/src/a.rs".into(),
                1,
                20,
                DiffStage::Staged,
                0,
                3,
                vec!["old".into()],
            ),
        ])
    }

    #[test]
    fn git_change_candidates_round_trip_to_items() {
        let c = git_change_candidates();
        assert_eq!(c.kind(), PickerKind::GitChanges);
        assert_eq!(c.len(), 2);
        // The query greps content, not the path — regex over each candidate's `lines`.
        assert_eq!(c.match_strategy(), MatchStrategy::RegexContent);
        match c.make_item(1, vec![0, 1]) {
            PickerItem::GitChange {
                path_index,
                relative_path,
                hunk_index,
                line,
                stage,
                added,
                removed,
                preview,
                match_indices,
            } => {
                assert_eq!(path_index, 0);
                assert_eq!(relative_path, "src/a.rs");
                assert_eq!(hunk_index, 1);
                assert_eq!(line, 20);
                assert_eq!(stage, DiffStage::Staged);
                assert_eq!((added, removed), (0, 3));
                assert_eq!(preview, "old");
                assert_eq!(match_indices, vec![0, 1]);
            }
            other => panic!("expected GitChange, got {other:?}"),
        }
    }

    #[test]
    fn git_change_identity_is_path_and_hunk_index() {
        let c = git_change_candidates();
        // Two hunks of the same file are distinguished by hunk_index.
        assert_eq!(c.position_of(&c.make_item(0, vec![])), Some(0));
        assert_eq!(c.position_of(&c.make_item(1, vec![])), Some(1));
        // A hunk_index past the file's change list doesn't match.
        let elsewhere = PickerItem::GitChange {
            path_index: 0,
            relative_path: "src/a.rs".into(),
            hunk_index: 9,
            line: 0,
            stage: DiffStage::Unstaged,
            added: 1,
            removed: 0,
            preview: "x".into(),
            match_indices: vec![],
        };
        assert_eq!(c.position_of(&elsewhere), None);
    }

    #[test]
    fn git_change_rerank_filters_by_dir_and_glob_keeping_grouping() {
        use aether_protocol::picker::{PickerFilters, ScopedPath};
        let cands = PickerCandidates::GitChanges(vec![
            GitChangeCandidate::new(
                0,
                "src/a.rs".into(),
                "/p/src/a.rs".into(),
                0,
                1,
                DiffStage::Unstaged,
                1,
                0,
                vec!["a".into()],
            ),
            GitChangeCandidate::new(
                0,
                "src/a.rs".into(),
                "/p/src/a.rs".into(),
                1,
                9,
                DiffStage::Unstaged,
                1,
                0,
                vec!["a2".into()],
            ),
            GitChangeCandidate::new(
                0,
                "docs/b.md".into(),
                "/p/docs/b.md".into(),
                0,
                2,
                DiffStage::Unstaged,
                1,
                0,
                vec!["b".into()],
            ),
        ]);
        let mut m = make_matcher();

        // A directory scope keeps both src/a.rs hunks (contiguous) and drops docs/b.md.
        let mut s = PickerState::new(cands.clone());
        s.filters = PickerFilters {
            directories: vec![ScopedPath {
                path_index: 0,
                relative_path: "src".into(),
                is_file: false,
            }],
            ..Default::default()
        };
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![0, 1], "both src/a.rs hunks, no docs/b.md");

        // An exclude glob drops src and leaves docs.
        let mut s = PickerState::new(cands);
        s.filters = PickerFilters {
            globs: vec!["!src/**".into()],
            ..Default::default()
        };
        s.rerank(&mut m);
        assert_eq!(
            s.ranked,
            vec![2],
            "only docs/b.md survives the exclude glob"
        );

        // An exact-file scope on src/a.rs keeps both of its hunks and drops docs/b.md — same
        // result as the `src` dir scope here, but via equality, not prefix.
        let cands = PickerCandidates::GitChanges(vec![
            GitChangeCandidate::new(
                0,
                "src/a.rs".into(),
                "/p/src/a.rs".into(),
                0,
                1,
                DiffStage::Unstaged,
                1,
                0,
                vec!["a".into()],
            ),
            GitChangeCandidate::new(
                0,
                "src/a.rs".into(),
                "/p/src/a.rs".into(),
                1,
                9,
                DiffStage::Unstaged,
                1,
                0,
                vec!["a2".into()],
            ),
            GitChangeCandidate::new(
                0,
                "docs/b.md".into(),
                "/p/docs/b.md".into(),
                0,
                2,
                DiffStage::Unstaged,
                1,
                0,
                vec!["b".into()],
            ),
        ]);
        let mut s = PickerState::new(cands);
        s.filters = PickerFilters {
            directories: vec![ScopedPath {
                path_index: 0,
                relative_path: "src/a.rs".into(),
                is_file: true,
            }],
            ..Default::default()
        };
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![0, 1], "both src/a.rs hunks, no docs/b.md");
    }

    #[test]
    fn jumplist_rerank_filters_by_dir_and_glob_and_drops_pathless_entries() {
        use aether_protocol::picker::{GroupHeader, PickerFilters, ScopedPath};
        let entry = |rel: Option<&str>, abs: &str, line: u32, display: &str| {
            crate::jumplist::JumplistEntry {
                path_index: rel.map(|_| 0),
                relative_path: rel.map(str::to_string),
                abs_path: abs.into(),
                position: aether_protocol::LogicalPosition { line, col: 0 },
                anchor: None,
                group: Some(match rel {
                    Some(r) => GroupHeader::File {
                        path_index: 0,
                        relative_path: r.into(),
                    },
                    None => GroupHeader::Label { label: abs.into() },
                }),
                display: display.into(),
            }
        };
        let cands = PickerCandidates::Jumplist(vec![
            entry(Some("src/a.rs"), "/p/src/a.rs", 1, "hit one"),
            entry(Some("src/a.rs"), "/p/src/a.rs", 9, "hit two"),
            entry(Some("docs/b.md"), "/p/docs/b.md", 2, "hit three"),
            // Out-of-workspace (a references capture into a dependency): no relative parts.
            entry(None, "/dep/x.rs", 3, "hit four"),
        ]);
        let mut m = make_matcher();

        // No filters: everything passes, document order.
        let mut s = PickerState::new(cands.clone());
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![0, 1, 2, 3]);

        // A directory scope keeps src/a.rs's entries (contiguous) and drops the rest —
        // including the pathless entry, which no scope can speak about.
        let mut s = PickerState::new(cands.clone());
        s.filters = PickerFilters {
            directories: vec![ScopedPath {
                path_index: 0,
                relative_path: "src".into(),
                is_file: false,
            }],
            ..Default::default()
        };
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![0, 1], "src entries only, pathless dropped");

        // An exclude glob drops src; docs survives. The pathless entry still drops — any
        // active path filter excludes what has no root-relative path.
        let mut s = PickerState::new(cands.clone());
        s.filters = PickerFilters {
            globs: vec!["!src/**".into()],
            ..Default::default()
        };
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![2], "docs survives; src and pathless drop");

        // Filters compose with the fuzzy query: scope to src, query "two" — matches stay in
        // document (candidate) order, and only in-scope rows are scored at all.
        let mut s = PickerState::new(cands);
        s.filters = PickerFilters {
            directories: vec![ScopedPath {
                path_index: 0,
                relative_path: "src".into(),
                is_file: false,
            }],
            ..Default::default()
        };
        s.query = "two".into();
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![1], "query narrows within the scoped set");
    }

    #[test]
    fn workspace_symbols_rerank_filters_by_dir_and_glob_and_drops_out_of_root_symbols() {
        use aether_protocol::picker::{PickerFilters, ScopedPath};
        let cand = |path_index: Option<u32>, display: &str, name: &str| WorkspaceSymbolCandidate {
            abs_path: match path_index {
                Some(_) => format!("/w/{display}"),
                None => display.to_string(),
            },
            path_index,
            display_path: display.into(),
            line: 0,
            col: 0,
            name: name.into(),
            symbol_kind: aether_protocol::picker::SymbolKind::Function,
            container: String::new(),
        };
        let cands = PickerCandidates::WorkspaceSymbols(vec![
            cand(Some(0), "docs/guide.md", "Heading"),
            cand(Some(0), "src/main.rs", "main"),
            cand(Some(0), "src/util.rs", "helper"),
            // Out-of-root (a dependency / stdlib symbol from rust-analyzer's widening).
            cand(None, "/deps/lib.rs", "dep_fn"),
        ]);
        let mut m = make_matcher();

        // No filters: everything passes in candidate (merge) order.
        let mut s = PickerState::new(cands.clone());
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![0, 1, 2, 3]);

        // A directory scope keeps src's symbols and drops the rest — including the out-of-root
        // symbol, which no root-relative scope can speak about.
        let mut s = PickerState::new(cands.clone());
        s.filters = PickerFilters {
            directories: vec![ScopedPath {
                path_index: 0,
                relative_path: "src".into(),
                is_file: false,
            }],
            ..Default::default()
        };
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![1, 2], "src symbols only, out-of-root dropped");

        // A glob narrows by extension; the out-of-root symbol drops under any path filter.
        let mut s = PickerState::new(cands.clone());
        s.filters = PickerFilters {
            globs: vec!["*.md".into()],
            ..Default::default()
        };
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![0], "only the markdown symbol survives");

        // Filters compose with the rank-don't-filter query ordering: scope to src, query
        // "helper" — both src rows survive (nucleo never filters symbols), best match first.
        let mut s = PickerState::new(cands);
        s.filters = PickerFilters {
            directories: vec![ScopedPath {
                path_index: 0,
                relative_path: "src".into(),
                is_file: false,
            }],
            ..Default::default()
        };
        s.query = "helper".into();
        s.rerank(&mut m);
        assert_eq!(
            s.ranked,
            vec![2, 1],
            "scope narrows, the query only orders what's left"
        );
    }

    #[test]
    fn symbol_fanout_covers_needs_settled_same_query_and_every_admitted_server() {
        use std::path::{Path, PathBuf};
        let frontend = || (Path::new("/w/frontend"), "typescript");
        let backend = || (Path::new("/w/backend"), "rust");

        let mut s = PickerState::new(PickerCandidates::WorkspaceSymbols(Vec::new()));
        s.query = "boot".into();
        assert!(
            !s.symbol_fanout_covers([frontend()].into_iter()),
            "nothing recorded yet"
        );

        s.symbol_fanned = Some((
            "boot".into(),
            vec![
                (PathBuf::from("/w/frontend"), "typescript".into()),
                (PathBuf::from("/w/backend"), "rust".into()),
            ],
        ));
        assert!(
            s.symbol_fanout_covers([frontend()].into_iter()),
            "a narrowed admitted set is covered — filter-only change reuses candidates"
        );
        assert!(s.symbol_fanout_covers([frontend(), backend()].into_iter()));

        // A server the fan-out never asked (became ready after, or was scoped out) is a miss.
        assert!(!s.symbol_fanout_covers([frontend(), (Path::new("/w/other"), "go")].into_iter()));

        // A different query is a miss regardless of servers.
        s.query = "bootstrap".into();
        assert!(!s.symbol_fanout_covers([frontend()].into_iter()));
        s.query = "boot".into();

        // Answers still outstanding: the generation bump would orphan them, so no reuse.
        s.seed_symbol_fanout(1);
        assert!(!s.symbol_fanout_covers([frontend()].into_iter()));
        assert!(s.note_symbol_merge(), "settles");
        assert!(s.symbol_fanout_covers([frontend()].into_iter()));
    }

    #[test]
    fn under_scope_file_matches_by_equality_not_prefix() {
        // Directory scope: prefix match, with a `/` boundary so `src` doesn't catch `src2`.
        assert!(under_scope(0, "src/a.rs", 0, "src", false));
        assert!(under_scope(0, "src/deep/a.rs", 0, "src", false));
        assert!(!under_scope(0, "src2/a.rs", 0, "src", false));
        assert!(under_scope(0, "anything", 0, "", false)); // empty = whole root
        assert!(!under_scope(0, "src/a.rs", 1, "src", false)); // wrong root

        // File scope: exact equality only — no subtree, no sibling.
        assert!(under_scope(0, "src/a.rs", 0, "src/a.rs", true));
        assert!(!under_scope(0, "src/a.rs", 0, "src", true)); // dir path, file flag → no match
        assert!(!under_scope(0, "src/a_helper.rs", 0, "src/a.rs", true)); // sibling
        assert!(!under_scope(0, "src/a.rs", 1, "src/a.rs", true)); // wrong root
    }

    #[test]
    fn git_change_query_greps_content_and_previews_the_matched_line() {
        let cand = |rel: &str, lines: Vec<&str>| {
            GitChangeCandidate::new(
                0,
                rel.into(),
                format!("/{rel}"),
                0,
                0,
                DiffStage::Unstaged,
                lines.len() as u32,
                0,
                lines.into_iter().map(String::from).collect(),
            )
        };
        let cands = PickerCandidates::GitChanges(vec![
            cand("a.rs", vec!["fn one()", "let TODO = 1", "return"]),
            cand("b.rs", vec!["nothing here"]),
        ]);
        let mut s = PickerState::new(cands);
        // The flat sibling kind: preview/matching mechanics are what's under test, and the
        // grouped GitChanges wraps the same items in header rows (covered by the row-space
        // tests) which would just get in the way of indexing `items` here.
        s.kind = PickerKind::GitChangesFile;
        let mut m = make_matcher();

        // Smartcase substring over content: "todo" matches "TODO" in a.rs, not b.rs.
        s.query = "todo".into();
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![0], "only the hunk whose content matches");
        let (_, items) = s.build_window_items(0, 10, &mut m);
        match &items[0] {
            PickerItem::GitChange {
                preview,
                match_indices,
                ..
            } => {
                assert_eq!(
                    preview, "let TODO = 1",
                    "previews the matched line, not the first"
                );
                assert_eq!(
                    match_indices,
                    &vec![4, 5, 6, 7],
                    "highlights the matched span"
                );
            }
            other => panic!("expected GitChange, got {other:?}"),
        }

        // Empty query → every hunk, previewing its first non-blank changed line with no highlight.
        s.query = String::new();
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![0, 1]);
        let (_, items) = s.build_window_items(0, 10, &mut m);
        match &items[0] {
            PickerItem::GitChange {
                preview,
                match_indices,
                ..
            } => {
                assert_eq!(preview, "fn one()");
                assert!(match_indices.is_empty());
            }
            other => panic!("expected GitChange, got {other:?}"),
        }
    }

    #[test]
    fn git_change_preview_skips_leading_blank_lines() {
        // A hunk that adds a blank line before any code (lines stored trimmed, so "" is blank).
        let cand = |rel: &str, lines: Vec<&str>| {
            GitChangeCandidate::new(
                0,
                rel.into(),
                format!("/{rel}"),
                0,
                0,
                DiffStage::Unstaged,
                lines.len() as u32,
                0,
                lines.into_iter().map(String::from).collect(),
            )
        };
        let cands = PickerCandidates::GitChanges(vec![
            cand("a.rs", vec!["", "", "fn real()"]),
            cand("blank.rs", vec!["", ""]), // all-blank hunk → falls back to the (empty) first line
        ]);
        let mut s = PickerState::new(cands);
        // Flat sibling kind — see `git_change_query_greps_content_and_previews_the_matched_line`.
        s.kind = PickerKind::GitChangesFile;
        let mut m = make_matcher();
        s.rerank(&mut m);
        let (_, items) = s.build_window_items(0, 10, &mut m);
        match &items[0] {
            PickerItem::GitChange { preview, .. } => {
                assert_eq!(preview, "fn real()", "skips the leading blank lines");
            }
            other => panic!("expected GitChange, got {other:?}"),
        }
        match &items[1] {
            PickerItem::GitChange { preview, .. } => {
                assert_eq!(
                    preview, "",
                    "an all-blank hunk falls back to empty, not a panic"
                );
            }
            other => panic!("expected GitChange, got {other:?}"),
        }
    }

    #[test]
    fn git_change_selects_to_file_at_anchor_line() {
        // Selecting a hunk jumps to the start of its anchor line (a point, not a selection).
        match git_change_candidates().select_result(0) {
            Some(PickerSelectResult::FileAt {
                path,
                position,
                anchor,
            }) => {
                assert_eq!(path, "/proj/src/a.rs");
                assert_eq!(position, LogicalPosition { line: 4, col: 0 });
                assert_eq!(anchor, None);
            }
            other => panic!("expected FileAt, got {other:?}"),
        }
    }

    #[test]
    fn git_change_query_supports_regex_and_match_options() {
        let cand = |rel: &str, line: &str| {
            GitChangeCandidate::new(
                0,
                rel.into(),
                format!("/{rel}"),
                0,
                0,
                DiffStage::Unstaged,
                1,
                0,
                vec![line.into()],
            )
        };
        let cands = PickerCandidates::GitChanges(vec![
            cand("a.rs", "let count = 42;"),
            cand("b.rs", "axb pattern"), // regex `a.b` matches; literal `a.b` does not
            cand("c.rs", "a.b literal"), // both match
        ]);
        let mut m = make_matcher();

        // Regex is opt-in: `\d+` matches literally (nothing) by default, as a regex with the chip.
        let mut s = PickerState::new(cands.clone());
        s.query = r"\d+".into();
        s.rerank(&mut m);
        assert!(
            s.ranked.is_empty(),
            "literal `\\d+` matches no line by default"
        );
        s.filters.regex = true;
        s.rerank(&mut m);
        assert_eq!(
            s.ranked,
            vec![0],
            "regex `\\d+` matches the line with digits"
        );

        // Whole-word: a substring matches by default, but not as a whole word with the chip.
        let mut s = PickerState::new(cands.clone());
        s.query = "ount".into();
        s.rerank(&mut m);
        assert_eq!(
            s.ranked,
            vec![0],
            "substring 'ount' matches 'count' without the chip"
        );
        s.filters.whole_word = true;
        s.rerank(&mut m);
        assert!(
            s.ranked.is_empty(),
            "whole-word: 'ount' isn't a whole word in 'count'"
        );

        // Regex chip: `a.b` is literal by default (only 'a.b literal'), a regex (`.` = any) with it.
        let mut s = PickerState::new(cands.clone());
        s.query = "a.b".into();
        s.rerank(&mut m);
        assert_eq!(
            s.ranked,
            vec![2],
            "literal 'a.b' matches only 'a.b literal'"
        );
        s.filters.regex = true;
        s.rerank(&mut m);
        assert_eq!(s.ranked, vec![1, 2], "regex `a.b` matches 'axb' and 'a.b'");

        // An unparseable regex matches nothing (only reachable in regex mode — a literal '(' is
        // a valid query that simply matches no line here).
        let mut s = PickerState::new(cands);
        s.query = "(".into();
        s.filters.regex = true;
        s.rerank(&mut m);
        assert!(s.ranked.is_empty(), "an invalid regex matches nothing");
    }

    #[test]
    fn git_change_select_line_lands_on_the_matched_line() {
        // A hunk anchored at buffer line 10 adding three lines.
        let c = GitChangeCandidate::new(
            0,
            "a.rs".into(),
            "/a.rs".into(),
            0,
            10,
            DiffStage::Unstaged,
            3,
            0,
            vec!["zero".into(), "one TODO".into(), "two".into()],
        );
        let re = |q: &str| build_match_regex(q, &MatchOptions::default()).unwrap();
        assert_eq!(c.select_line(None), 10, "no query → the anchor line");
        assert_eq!(
            c.select_line(Some(&re("todo"))),
            11,
            "the 2nd new-side line is buffer line 11"
        );
        assert_eq!(
            c.select_line(Some(&re("zero"))),
            10,
            "the 1st new-side line is the anchor"
        );
        assert_eq!(
            c.select_line(Some(&re("nope"))),
            10,
            "no match → the anchor"
        );

        // A match only on a removed line (no buffer position) falls back to the anchor.
        let d = GitChangeCandidate::new(
            0,
            "b.rs".into(),
            "/b.rs".into(),
            0,
            5,
            DiffStage::Unstaged,
            1,
            1,
            vec!["kept".into(), "REMOVED gone".into()],
        );
        assert_eq!(
            d.select_line(Some(&re("gone"))),
            5,
            "a removed-line match anchors"
        );
    }

    #[test]
    fn git_change_resolve_select_jumps_to_the_query_match() {
        let cands = PickerCandidates::GitChanges(vec![GitChangeCandidate::new(
            0,
            "a.rs".into(),
            "/a.rs".into(),
            0,
            10,
            DiffStage::Unstaged,
            2,
            0,
            vec!["fn foo".into(), "let MATCH = 1".into()],
        )]);
        let mut s = PickerState::new(cands);
        let item = s.candidates.make_item(0, vec![]);

        // With a content query, select lands on the matched line (anchor 10 + new-side index 1).
        s.query = "match".into();
        match resolve_select(&s, &item) {
            Some(PickerSelectResult::FileAt { position, .. }) => assert_eq!(position.line, 11),
            other => panic!("expected FileAt, got {other:?}"),
        }
        // With no query, it lands on the anchor.
        s.query = String::new();
        match resolve_select(&s, &item) {
            Some(PickerSelectResult::FileAt { position, .. }) => assert_eq!(position.line, 10),
            other => panic!("expected FileAt, got {other:?}"),
        }
    }

    #[test]
    fn lsp_server_is_not_a_select_target() {
        // Restart is driven client-side (Ctrl-r → lsp/restart_server); `select` is a no-op, so
        // `select_result` yields None and `picker/select` never acts on this kind.
        assert!(lsp_candidates().select_result(0).is_none());
    }
}
