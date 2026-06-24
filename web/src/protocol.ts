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
//!   2. The handful of results from RPCs the shell issues *directly* (bootstrap: project/list,
//!      project/activate, buffer/open; geometry: viewport/subscribe|scroll|scroll_to_row|resize),
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

export type VirtualRowKind = "deleted";

export interface VirtualRow {
  text: string;
  kind: VirtualRowKind;
  /** Staged (text is HEAD's, already replaced in the index) vs unstaged (text is the index's).
   *  Omitted on the wire when "unstaged". */
  stage?: DiffStage;
}

export type DiffMarker = "added" | "modified" | "deleted";

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
  diagnostics?: DiagnosticSpan[];
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

/** Buffer-level Git status: branch + staged (HEAD→index) and unstaged (index→buffer) counts. */
export interface GitBufferStatus {
  branch?: string | null;
  staged?: GitChangeCounts;
  unstaged?: GitChangeCounts;
}

// ---- cursor -------------------------------------------------------------------------------------

export interface CursorState {
  position: LogicalPosition;
  anchor: LogicalPosition;
  match_bracket?: [LogicalPosition, LogicalPosition] | null;
  grep_position?: { current: number; total: number } | null;
}

// ---- bootstrap RPC results (project/list, project/activate, buffer/open) -------------------------

export interface ProjectSummary {
  name: string;
}
export interface ProjectListResult {
  projects: ProjectSummary[];
}

export interface ProjectInfo {
  name: string;
  paths: string[];
}
export interface ProjectActivateResult {
  project: ProjectInfo;
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
  | "projects"
  | "diagnostics"
  | "diagnostics_project"
  | "lsp_servers"
  | "references"
  | "document_symbols";

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
  | { kind: "project"; name: string; unsaved_buffers?: number; match_indices?: number[] }
  | { kind: "dir_entry"; name: string; is_dir: boolean; match_indices?: number[]; git_status?: GitStatus }
  | { kind: "root"; path_index: number; match_indices?: number[] }
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
      /** Row label: project-relative path (references are filtered to project roots server-side). */
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
    };
