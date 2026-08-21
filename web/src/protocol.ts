//! Hand-written mirror of the *subset* of `aether-protocol` types the web shell actually touches.
//!
//! The shell is a dumb transport for almost the entire protocol: it forwards core-issued RPC params
//! and results as opaque JSON (see shell.ts `sendRequest`/`onNotification` → `on_rpc_result`/
//! `on_event`). Those wire messages — editing, motions, input, undo, search, git, LSP, picker
//! actions — are owned and (de)serialised by the `aether-client` core, so the shell never needs
//! their TypeScript shapes. This file is therefore only the two surfaces the shell *itself* reads:
//!
//!   1. Render/view types embedded in the `View` that `view()` returns (the viewport render chain,
//!      cursor, diagnostics, LSP status, picker items) — consumed by render.ts / shell.ts.
//!   2. The handful of results from RPCs the shell issues *directly* (bootstrap: workspace/list,
//!      workspace/activate, buffer/open; geometry: viewport/subscribe|scroll|scroll_to_row|resize),
//!      because their params need pixels or they run before the core exists.
//!
//! Keep field names exactly matching the serde wire format.

export type BufferId = number;
export type ViewportId = number;
export type Revision = number;

export interface LogicalPosition {
  line: number;
  /** 0-based byte offset within the line's UTF-8 representation. */
  col: number;
}

// ---- viewport render model ----------------------------------------------------------------------

export type WrapMode = "soft" | "none";

export interface ScrollPosition {
  logical_line: number;
  sub_row: number;
}

export interface Highlight {
  /** Byte offsets within the containing Segment's text. */
  start: number;
  end: number;
  /** Tree-sitter highlight name, e.g. "keyword", "string", "comment". */
  kind: string;
}

export interface Segment {
  text: string;
  highlights: Highlight[];
}

export interface VisualRow {
  /** Byte offset within the logical line where this row's text starts. */
  byte_offset: number;
  continuation_indent: number;
  segments: Segment[];
}

export interface SearchMatchRange {
  start: number;
  end: number;
}

/** A sneak (s/S) word-jump target: byte range of a matched word within the logical line. The chip
 *  `start..prefix_end` is the typed-prefix highlight (one cell per char typed), with `label` over
 *  its first cell (absent — and the chip empty — while deferring labels). */
export interface SneakTarget {
  start: number;
  end: number;
  prefix_end: number;
  label?: string | null;
}

export type VirtualRowKind = "deleted";

/** One intra-line diff emphasis range: byte offsets within the owning line's / row's text. */
export interface EmphasisRange {
  start: number;
  end: number;
}

export interface VirtualRow {
  text: string;
  kind: VirtualRowKind;
  /** Staged (text is HEAD's, already replaced in the index) vs unstaged (text is the index's).
   *  Omitted on the wire when "unstaged". */
  stage?: DiffStage;
  /** Intra-line emphasis on this removed line (the parts its paired buffer line replaced). */
  emphasis?: EmphasisRange[];
}

export type DiffMarker = "added" | "modified" | "deleted";

/** Which part of a merge-conflict block a line belongs to. Present only on files a stopped merge
 *  or rebase left conflicted; the blocks are masked out of that file's diff, so a line never
 *  carries both this and a diff_marker. */
export type ConflictLine = "marker" | "ours" | "base" | "theirs";

/** Which side of the index a change sits on. Binary by design: where staged and unstaged
 *  overlap (modified, staged, modified again), the unstaged top layer wins. Omitted when
 *  "unstaged". */
export type DiffStage = "unstaged" | "staged";

/** Git status of a file-explorer entry, used to colour it. For a directory this is the
 *  highest-priority status among its descendants (folder aggregation). Mirrors the server's
 *  `GitStatus` enum. */
export type GitStatus = "conflicted" | "deleted" | "modified" | "added" | "untracked" | "ignored";

/** Mirrors aether-protocol::picker::BufferDirtyState. Save/disk state of a buffer-picker row,
 *  rendered as a colour-coded dot. Omitted on the wire (→ `clean`) for a clean buffer. */
export type BufferDirtyState = "clean" | "unsaved" | "externally_modified" | "externally_deleted";

export type DiagnosticSeverity = "error" | "warning" | "information" | "hint";

export interface DiagnosticSpan {
  /** Byte offsets within the logical line. */
  start: number;
  end: number;
  severity: DiagnosticSeverity;
  message: string;
}

export interface LogicalLineRender {
  logical_line: number;
  visual_rows: VisualRow[];
  search_matches?: SearchMatchRange[];
  virtual_rows_above?: VirtualRow[];
  diff_marker?: DiffMarker | null;
  /** Qualifies diff_marker in the combined view; omitted when "unstaged". */
  diff_stage?: DiffStage;
  /** Intra-line diff emphasis (diff view only): sub-ranges of the line a Modified hunk changed. */
  diff_emphasis?: EmphasisRange[];
  /** Which side of a merge conflict this line is; absent unless the file is conflicted. Unlike
   *  the diff tint this is not gated on the diff view. */
  conflict?: ConflictLine | null;
  diagnostics?: DiagnosticSpan[];
  sneak_targets?: SneakTarget[];
}

export interface BufferWindow {
  first_logical_line: number;
  last_logical_line_exclusive: number;
  line_count: number;
  max_scroll_logical_line: number;
  /** Total visual rows in the buffer (real + diff phantom) — sizes the native scroll container. */
  total_visual_rows: number;
  /** Visual-row index where first_logical_line begins — positions the window in the scroller. */
  first_visual_row: number;
  /** Display cols of the widest line — sizes the native horizontal scroller (no-wrap). 0 under soft wrap. */
  max_line_width: number;
  /** Buffer-level Git status (branch + staged/unstaged counts) for the status bar; absent outside a repo. */
  git_status?: GitBufferStatus;
  lines: LogicalLineRender[];
}

/** Buffer-wide Git change line counts vs HEAD, for the status bar (`+added ~modified -deleted`). */
export interface GitChangeCounts {
  added: number;
  modified: number;
  deleted: number;
}

/**
 * Divergence between the current branch and its upstream, from local refs only — it reports the
 * world as of the last fetch and never contacts a remote. Absent when there is nothing to compare
 * against (detached, unborn, or a never-pushed branch).
 */
export interface GitUpstreamStatus {
  name: string;
  ahead: number;
  behind: number;
}

/**
 * A long-running git operation in flight (`git/operation_changed`). Only user-initiated ones are
 * announced — the periodic background fetch stays silent.
 */
export interface GitOperation {
  kind: "fetch" | "push" | "pull";
  /** git's latest progress line, verbatim. */
  detail?: string;
}

/**
 * A multi-step git operation the repo is stopped part-way through (`.git/MERGE_HEAD`,
 * `.git/rebase-merge`, …). Distinct from `GitOperation`, which is one we are running right now.
 */
export type GitRepoOperation =
  | "merge"
  | "rebase"
  | "cherry_pick"
  | "revert"
  | "bisect"
  | "apply_mailbox";

/** Mirrors `GitRepoOperation::label` in the core, which the native shells call directly. */
export const REPO_OPERATION_LABELS: Record<GitRepoOperation, string> = {
  merge: "merging",
  rebase: "rebasing",
  cherry_pick: "cherry-picking",
  revert: "reverting",
  bisect: "bisecting",
  apply_mailbox: "applying",
};

/** Buffer-level Git status: branch + staged (HEAD→index) and unstaged (index→buffer) counts. */
export interface GitBufferStatus {
  branch?: string | null;
  /** Set when the repo is stopped mid-merge/rebase — the status bar must say so. */
  operation?: GitRepoOperation | null;
  staged?: GitChangeCounts;
  unstaged?: GitChangeCounts;
  upstream?: GitUpstreamStatus | null;
  /** True when this file lives in a linked worktree rather than the repo's main checkout. The
   *  branch alone can't say it — git allows one checkout per branch per family, so the same name
   *  reads identically either way. Absent means false. */
  worktree?: boolean;
}

// ---- cursor -------------------------------------------------------------------------------------

export interface CursorState {
  position: LogicalPosition;
  anchor: LogicalPosition;
  match_bracket?: [LogicalPosition, LogicalPosition] | null;
  jumplist_position?: { current: number; total: number } | null;
}

// ---- bootstrap RPC results (workspace/list, workspace/activate, buffer/open) -------------------------

export interface WorkspaceSummary {
  name: string;
}
export interface WorkspaceListResult {
  workspaces: WorkspaceSummary[];
}

export interface WorkspaceInfo {
  name: string;
  paths: string[];
}
export interface WorkspaceActivateResult {
  workspace: WorkspaceInfo;
  last_buffer_id?: BufferId | null;
  /** With `open_last`: the landing buffer (MRU or fresh transient scratch), fully opened. */
  opened?: BufferOpenResult | null;
}

export interface LspServerRef {
  language: string;
  workspace_root: string;
}

export interface BufferOpenResult {
  buffer_id: BufferId;
  language?: string | null;
  line_count: number;
  byte_count: number;
  revision: Revision;
  saved_revision: Revision;
  path?: string | null;
  scratch_number?: number | null;
  cursor: CursorState;
  scroll?: ScrollPosition | null;
  lsp_server?: LspServerRef | null;
  /** True while the buffer is transient (auto-closes once hidden). */
  transient?: boolean;
}

// ---- LSP status (embedded in the View + picker rows) --------------------------------------------

export interface DiagnosticCounts {
  errors: number;
  warnings: number;
  infos: number;
  hints: number;
}
/** One in-flight `$/progress` work-done operation reported by a server. */
export interface LspProgress {
  title: string;
  message?: string | null;
  percentage?: number | null;
}
/** Mirrors aether-protocol::lsp::LspStatus (serde internally tagged on `state`). */
export type LspStatus =
  | { state: "starting" | "initializing" | "ready" | "restarting" | "stopped" }
  | { state: "crashed"; code?: number | null; message: string };
/** lsp/status_changed payload (also a row in the LSP servers picker). */
export interface LspServerStatus {
  name: string;
  language: string;
  workspace_root: string;
  status: LspStatus;
  /** Active `$/progress` work (indexing, cargo check, …). Non-empty ⇒ busy. Absent when idle. */
  progress?: LspProgress[];
}

// ---- geometry RPC results (viewport/subscribe, scroll, scroll_to_row, resize) -------------------

export interface ViewportSubscribeResult {
  viewport_id: ViewportId;
  window: BufferWindow;
  /** Buffer-level status snapshotted at subscribe time — see the server's BufferStatusSnapshot.
   *  Lets a client seed external-change flags, diagnostic counts, and LSP health the moment it
   *  starts showing a buffer, rather than waiting for the next change-notification. */
  buffer_status?: BufferStatusSnapshot;
}

/** Buffer-level state delivered with viewport/subscribe (counterpart to the server struct). */
export interface BufferStatusSnapshot {
  externally_modified?: boolean;
  externally_deleted?: boolean;
  diagnostics?: DiagnosticCounts;
  lsp_status?: LspServerStatus | null;
}

export interface ViewportWindowResult {
  window: BufferWindow;
}

/** Mirrors aether-protocol::git::BlameInfo (serde snake_case). */
export interface BlameInfo {
  /** Abbreviated (7-char) commit hash; empty when `is_uncommitted`. */
  commit: string;
  author: string;
  /** Author time as Unix seconds; `0` when `is_uncommitted`. */
  timestamp: number;
  /** A local, not-yet-committed edit (or a brand-new working-tree line). */
  is_uncommitted: boolean;
}

/** Result of `git/blame_line`. `blame` is null when the line has no blame (no repo, untracked,
 *  or past end-of-file); an uncommitted line is present with `is_uncommitted = true`. */
export interface GitBlameLineResult {
  blame: BlameInfo | null;
}

// ---- picker rows (embedded in the View) ---------------------------------------------------------

export type PickerKind =
  | "files"
  | "buffers"
  | "grep"
  | "git_changes"
  | "git_changes_file"
  | "explorer"
  | "workspaces"
  | "diagnostics"
  | "diagnostics_workspace"
  | "lsp_servers"
  | "references"
  | "document_symbols"
  | "workspace_symbols"
  | "keybindings"
  | "jumplist"
  | "git_branches"
  | "git_log"
  | "git_log_file"
  | "git_stash";

/** Mirrors aether-protocol::picker::SymbolKind (serde snake_case). `unknown` covers any value
 *  outside the LSP-defined 1..=26 range. */
export type SymbolKind =
  | "file" | "module" | "namespace" | "package" | "class" | "method" | "property" | "field"
  | "constructor" | "enum" | "interface" | "function" | "variable" | "constant" | "string"
  | "number" | "boolean" | "array" | "object" | "key" | "null" | "enum_member" | "struct"
  | "event" | "operator" | "type_parameter" | "unknown";

/** Mirrors aether-protocol::picker::PickerItem (serde tag = "kind", snake_case). `match_indices`
 *  are code-point offsets into the row's display string, covered by the fuzzy match. */
export type PickerItem =
  | { kind: "file"; path_index: number; relative_path: string; match_indices?: number[]; git_status?: GitStatus }
  | { kind: "buffer"; buffer_id: BufferId; display: string; status?: BufferDirtyState; path_index?: number; relative_path?: string; match_indices?: number[]; transient?: boolean }
  | {
      kind: "grep_hit";
      path_index: number;
      relative_path: string;
      line: number;
      col: number;
      preview: string;
      match_indices?: number[];
    }
  | {
      kind: "git_change";
      path_index: number;
      relative_path: string;
      hunk_index: number;
      line: number;
      /** Side of the index the hunk sits on; omitted on the wire (→ `unstaged`). */
      stage?: DiffStage;
      /** New-side lines added (0 for a pure deletion). */
      added: number;
      /** Baseline lines removed (0 for a pure addition). */
      removed: number;
      /** First changed line of the hunk, already trimmed. The fuzzy match is on the path, so
       *  `match_indices` index `relative_path` (shown in the file header), not this preview. */
      preview: string;
      match_indices?: number[];
    }
  | { kind: "diagnostic"; path_index?: number; relative_path?: string; line: number; col: number; end_line?: number; end_col?: number; severity: DiagnosticSeverity; message: string; match_indices?: number[] }
  | { kind: "workspace"; name: string; unsaved_buffers?: number; match_indices?: number[] }
  | { kind: "dir_entry"; name: string; is_dir: boolean; match_indices?: number[]; git_status?: GitStatus }
  | { kind: "root"; path_index: number; match_indices?: number[] }
  | {
      kind: "git_branch";
      /** Which repo the row belongs to — echoed onto the checkout/delete it triggers, so the
       *  action can't re-resolve to a different repo if the active buffer moved meanwhile. */
      repo_id: string;
      name: string;
      is_head?: boolean;
      subject?: string;
      /** Tip commit's author time, Unix seconds; 0/absent when unknown. */
      timestamp?: number;
      upstream?: string | null;
      ahead?: number;
      behind?: number;
      /** The checkout in this family holding this branch, when one does — including the tree you
       *  are standing in. Its presence is what makes this a worktree row: Enter opens that tree
       *  rather than moving HEAD, and Ctrl-d removes it rather than deleting the branch. */
      checkout?: {
        /** Working directory of the checkout. */
        path: string;
        /** It is the repo's main working tree: no admin name, and not removable. */
        is_main?: boolean;
        /** Admin name of the linked worktree — empty for the main tree, which is also what
         *  `workspace/bind_worktree` reads as "unbind". Drifts from the branch: a tree made for
         *  `feature/auth` is called `feature-auth`, and a checkout inside it later moves HEAD
         *  without renaming anything. */
        worktree?: string;
        /** This is the checkout you are standing in. */
        is_current?: boolean;
        locked?: boolean;
        /** The admin entry outlived its directory — prunable, and rendered as such. */
        prunable?: boolean;
      } | null;
      /** Set when the row is a detached worktree rather than a branch: `name` is the tree's admin
       *  name and this is its short commit id (empty for a prunable entry). */
      detached_at?: string | null;
      match_indices?: number[];
    }
  | {
      kind: "git_stash";
      /** Which repo the row belongs to — echoed onto the stash action it triggers. */
      repo_id: string;
      /** Position at listing time, rendered as `stash@{n}`. Display only: every action
       *  re-resolves it server-side from `oid`, because positions shift as entries are dropped. */
      index: number;
      /** The stash commit's hash — the row's identity, and what `git/show` previews. */
      oid: string;
      message?: string;
      /** When the entry was made, Unix seconds; 0/absent when unknown. */
      timestamp?: number;
      match_indices?: number[];
    }
  | {
      kind: "git_commit";
      /** Which repo the row belongs to — echoed onto the `git/show` it triggers. */
      repo_id: string;
      /** Full hash; what `git/show` receives. */
      hash: string;
      short_hash: string;
      subject?: string;
      author?: string;
      /** Author time, Unix seconds; 0/absent when unknown. */
      timestamp?: number;
      /** Offsets into `subject` covered by the fuzzy match. The author is rendered but never
       *  matched; the hash is matched by prefix instead (`hash_match_len`). */
      match_indices?: number[];
      /** How many leading characters of `short_hash` the query abbreviated (0 = no hash match).
       *  A hash is an identifier, so `git show 20a3a8a` means a prefix — and the prefix is tested
       *  against the *full* hash, so a pasted 40-character sha matches a row rendering 7. */
      hash_match_len?: number;
    }
  | {
      kind: "lsp_server";
      name: string;
      language: string;
      workspace_root: string;
      root_label?: string;
      status: LspStatus;
      progress?: LspProgress[];
      match_indices?: number[];
    }
  | {
      kind: "reference";
      /** Absolute path to the file containing the reference (fed into buffer/open on select). */
      path: string;
      /** Row label: workspace-relative path (references are filtered to workspace roots server-side). */
      display_path: string;
      line: number;
      col: number;
      /** The referenced line's text; the fuzzy haystack + preview. */
      preview: string;
      /** True for the row that is the symbol's definition (vs an ordinary use). Drives the
       *  Definition / References section split; references arrive definition-first. */
      is_definition?: boolean;
      match_indices?: number[];
    }
  | {
      kind: "symbol";
      /** Absolute path to the buffer's file (fed into buffer/open on select). */
      path: string;
      line: number;
      col: number;
      /** Symbol name — fuzzy haystack + the row's primary label. */
      name: string;
      symbol_kind: SymbolKind;
      /** The DocumentSymbol signature; empty for flat servers (containerName is not surfaced). */
      detail?: string;
      /** Nesting depth (0 = top-level), for indenting members. */
      depth?: number;
      /** True when this row is only an ancestor of a match, shown dim for tree context while
       *  filtering — non-selectable (the core's navigation skips it). Absent when false. */
      context?: boolean;
      match_indices?: number[];
    }
  | {
      kind: "keybinding";
      /** Section the binding belongs to (e.g. "Editing"). */
      group: string;
      /** What the binding does — the row's main label. */
      desc: string;
      /** Mode the binding applies in (e.g. "Normal", "Any"). */
      mode: string;
      /** The chord itself (e.g. "Ctrl-w"). */
      keys: string;
      /** Code-point offsets into the composed haystack `"{group} > {desc} ({mode}) {keys}"` —
       *  rebased per segment by the shell (mirrors aether-client `keybinding_match_segments`). */
      match_indices?: number[];
    }
  | {
      kind: "jumplist_entry";
      /** 0-based position in the jumplist — the row's identity. */
      index: number;
      /** 0-based landing line — the right-aligned dim line number, like a grep hit. Absent for a
       *  whole-target entry (a file or buffer captured without a position, docs/jumplist.md),
       *  which renders with no number at all. */
      line?: number;
      /** The captured entry's flat display text (docs/jumplist.md §2.2). */
      display: string;
      match_indices?: number[];
    }
  | {
      kind: "group";
      /** A collapsible group's header row (docs/picker-groups.md §9) — a real, selectable
       *  window row, not a derived decoration. Click selects (and expands) the group; Enter
       *  jumps to its first item — both routed through the core. */
      header: GroupHeader;
      /** Items in the run, shown on the row whether collapsed or expanded. */
      count: number;
      /** Whether the run's items follow this row in the window. Absent = collapsed. */
      expanded?: boolean;
    };

/** Mirrors aether-protocol::picker::GroupHeader (serde tag = "kind", snake_case). What a group's
 *  header row shows: `file` carries the workspace-relative location and the client formats it
 *  (root labels are client-derived); `label` is rendered verbatim (References' Definition /
 *  References sections, a keybinding group's name). */
export type GroupHeader =
  | { kind: "file"; path_index: number; relative_path: string }
  | { kind: "label"; label: string };

/** Mirrors aether-protocol::picker::GroupSpan. One group run within a pushed window: the items
 *  from `start` (0-based index into the *window's* items, NOT the absolute ranked index) up to
 *  the next span (or the window's end) render under `header`. The server is the single source of
 *  group boundaries. Invariant for grouped kinds: a non-empty window's first span always has
 *  `start === 0` — a window starting mid-group repeats the split group's header. Ungrouped kinds
 *  send no spans. */
export interface GroupSpan {
  start: number;
  header: GroupHeader;
  /** Collapsible kinds only (docs/picker-groups.md): the run's item count and whether it is
   *  the expanded run — the same decoration as its `group` row, so a sticky pin standing in
   *  for a scrolled-off header renders identically. Absent for the derived-header kinds. */
  count?: number;
  expanded?: boolean;
}
