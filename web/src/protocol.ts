//! Hand-written mirror of the `aether-protocol` types the Phase-1 web client needs (the viewport
//! render chain + bootstrap methods). INTERIM: the plan (docs/web-client.md §2.8) is to generate
//! these from the Rust crate with ts-rs once the generate→consume loop can be validated against a
//! node toolchain. Keep field names exactly matching the serde wire format.

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
}

export type DiffMarker = "added" | "modified" | "deleted";

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
  lines: LogicalLineRender[];
}

// ---- cursor -------------------------------------------------------------------------------------

export interface CursorState {
  position: LogicalPosition;
  anchor: LogicalPosition;
  match_bracket?: [LogicalPosition, LogicalPosition] | null;
  grep_position?: { current: number; total: number } | null;
}

// ---- methods: params & results ------------------------------------------------------------------

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
export interface ProjectActivateParams {
  name: string;
}
export interface ProjectActivateResult {
  project: ProjectInfo;
  last_buffer_id?: BufferId | null;
}
export interface ProjectRemoveRootResult {
  project: ProjectInfo;
  closed_buffer_ids?: BufferId[];
  next_buffer_id?: BufferId | null;
}

export interface BufferOpenParams {
  buffer_id?: BufferId | null;
  path_index?: number | null;
  relative_path?: string | null;
  language?: string | null;
  create_if_missing?: boolean;
  jump_to?: LogicalPosition | null;
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
}

// ---- server→client notifications (beyond viewport/picker) ---------------------------------------

export interface BufferStateParams {
  buffer_id: BufferId;
  saved_revision: Revision;
  saved_at_unix_ms?: number | null;
  externally_modified?: boolean;
  externally_deleted?: boolean;
}
export interface DiagnosticCounts {
  errors: number;
  warnings: number;
  infos: number;
  hints: number;
}
export interface LspDiagnosticsChangedParams {
  buffer_id: BufferId;
  counts: DiagnosticCounts;
}
/** lsp/status_changed payload (also a row in lsp/server_status). */
export interface LspServerStatus {
  name: string;
  language: string;
  workspace_root: string;
  status: LspStatus;
}

export interface ViewportSubscribeParams {
  buffer_id: BufferId;
  cols: number;
  rows: number;
  overscan_rows: number;
  scroll: ScrollPosition;
  wrap: WrapMode;
  continuation_marker_width: number;
  tab_width: number;
}
export interface ViewportSubscribeResult {
  viewport_id: ViewportId;
  window: BufferWindow;
}

export interface ViewportResizeParams {
  viewport_id: ViewportId;
  cols: number;
  rows: number;
}
export interface ViewportScrollParams {
  viewport_id: ViewportId;
  scroll: ScrollPosition;
}
export interface ViewportScrollToRowParams {
  viewport_id: ViewportId;
  top_visual_row: number;
}
export interface ViewportWindowResult {
  window: BufferWindow;
}

// ---- motions, cursor, input (Phase 2) -----------------------------------------------------------

export type Direction = "forward" | "backward";
export type VerticalDirection = "up" | "down";
export type WordBoundary = "word" | "WORD" | "subword";

/** Mirrors aether-protocol::cursor::Motion (serde tag = "kind", snake_case variants). */
export type Motion =
  | { kind: "char"; direction: Direction; count: number }
  | { kind: "word"; direction: Direction; count: number; boundary: WordBoundary; exclusive: boolean }
  | { kind: "word_end"; direction: Direction; count: number; boundary: WordBoundary }
  | { kind: "logical_line"; direction: Direction; count: number; preserve_col: boolean }
  | { kind: "line_start" }
  | { kind: "line_end" }
  | { kind: "line_first_nonblank" }
  | { kind: "buffer_start" }
  | { kind: "buffer_end" }
  | { kind: "goto"; position: LogicalPosition }
  | { kind: "visual_line"; viewport_id: ViewportId; direction: VerticalDirection; count: number }
  | { kind: "find_char"; ch: string; direction: Direction; count: number; till: boolean }
  | { kind: "match_bracket"; inner: boolean }
  | { kind: "next_navigation_unit" }
  | { kind: "prev_navigation_unit" }
  | { kind: "end_of_navigation_unit" }
  | { kind: "start_of_navigation_unit" };

export interface CursorMoveParams {
  buffer_id: BufferId;
  motion: Motion;
  extend_selection: boolean;
}
export interface CursorSetParams {
  buffer_id: BufferId;
  position: LogicalPosition;
  anchor: LogicalPosition;
}
export interface CursorSelectLineParams {
  buffer_id: BufferId;
  direction: Direction;
  extend: boolean;
}
export interface CursorBufferOnlyParams {
  buffer_id: BufferId;
}
export interface CursorUndoResult {
  applied: boolean;
  cursor: CursorState;
}

export interface BufferOnlyParams {
  buffer_id: BufferId;
}
export interface InputTextParams {
  buffer_id: BufferId;
  text: string;
  select_pasted?: boolean;
}
export interface InputMoveLinesParams {
  buffer_id: BufferId;
  direction: VerticalDirection;
}
export interface EditResult {
  revision: Revision;
  cursor: CursorState;
}
export interface UndoResult {
  revision: Revision;
  applied: boolean;
  cursor: CursorState;
}

export type CopyScope = "selection" | "line";
export interface BufferCopyParams {
  buffer_id: BufferId;
  scope: CopyScope;
}
export interface BufferCopyResult {
  text: string;
}
export interface BufferCutResult {
  text: string;
  revision: Revision;
  cursor: CursorState;
}
export interface InputReplaceLineParams {
  buffer_id: BufferId;
  text: string;
}

// ---- surround -----------------------------------------------------------------------------------

export type SurroundTarget = "selection" | "line";
export interface InputSurroundParams {
  buffer_id: BufferId;
  delimiter: string;
  target: SurroundTarget;
}
export interface InputUnsurroundParams {
  buffer_id: BufferId;
  target: SurroundTarget;
}

// ---- git --------------------------------------------------------------------------------------

export type HunkDirection = "next" | "prev";
export interface GitNavigateHunkParams {
  buffer_id: BufferId;
  from_line: number;
  direction: HunkDirection;
}
export interface GitNavigateHunkResult {
  cursor: CursorState;
  moved: boolean;
}
export interface GitSetDiffViewParams {
  viewport_id: ViewportId;
  enabled: boolean;
}
export interface BlameInfo {
  commit: string;
  author: string;
  timestamp: number;
  summary: string;
  is_uncommitted: boolean;
}
export interface GitBlameLineResult {
  blame?: BlameInfo | null;
}

// ---- grep navigate (cached hits, < / >) ---------------------------------------------------------

export interface PickerGrepNavigateParams {
  direction: Direction;
  buffer_id: BufferId;
}
export interface PickerGrepNavigateTarget {
  path: string;
  position: LogicalPosition;
  query: string;
}

// ---- LSP editor actions -------------------------------------------------------------------------

export interface LspBufferParams {
  buffer_id: BufferId;
}
export interface LspHoverResult {
  contents?: string | null;
}
export interface LspLocation {
  path: string;
  position: LogicalPosition;
}
export interface LspGotoDefinitionResult {
  location?: LspLocation | null;
}
export type FormatStatus = "applied" | "no_change" | "not_ready" | "unavailable" | "unsupported";
export interface LspFormatResult {
  cursor: CursorState;
  status: FormatStatus;
}
export type DiagnosticDirection = "next" | "prev";
export interface LspNavigateDiagnosticParams {
  buffer_id: BufferId;
  from_line: number;
  direction: DiagnosticDirection;
}
export interface LspNavigateDiagnosticResult {
  cursor: CursorState;
  moved: boolean;
}

// ---- search -------------------------------------------------------------------------------------

export interface SearchSummary {
  buffer_id: BufferId;
  total: number;
  truncated: boolean;
  /** 1-based index of the match the cursor head is on, or 0 if not on a match. */
  current_index: number;
}
export interface SearchSetParams {
  buffer_id: BufferId;
  query: string;
  anchor?: LogicalPosition | null;
  extend?: boolean;
}
export interface SearchSetResult {
  cursor: CursorState;
  summary: SearchSummary;
}
export interface SearchNavParams {
  buffer_id: BufferId;
  extend: boolean;
}
export interface SearchNavResult {
  cursor: CursorState;
  summary: SearchSummary;
}
export interface SearchClearParams {
  buffer_id: BufferId;
}

export interface ViewportSetWrapParams {
  viewport_id: ViewportId;
  wrap: WrapMode;
}
export interface BufferSaveParams {
  buffer_id: BufferId;
  path_index?: number | null;
  relative_path?: string | null;
  confirm?: boolean;
}
export interface BufferReloadResult {
  revision: Revision;
  saved_at_unix_ms?: number | null;
}
export interface BufferSaveResult {
  saved_at_unix_ms: number;
  revision: Revision;
}
export interface BufferCloseResult {
  next_buffer_id?: BufferId | null;
}

/** JSON-RPC error codes the client branches on (crates/aether-protocol/src/error.rs). */
export const WOULD_OVERWRITE = -32016;
export const WOULD_DISCARD_CHANGES = -32021;

export interface LogicalLineRange {
  start_logical_line: number;
  end_logical_line_exclusive: number;
}
export interface ViewportUnsubscribeParams {
  viewport_id: ViewportId;
}

// ---- pickers (Phase 4) --------------------------------------------------------------------------

export type PickerKind =
  | "files"
  | "buffers"
  | "grep"
  | "explorer"
  | "projects"
  | "diagnostics"
  | "lsp_servers";

/** Mirrors aether-protocol::lsp::LspStatus (serde internally tagged on `state`). */
export type LspStatus =
  | { state: "starting" | "initializing" | "ready" | "restarting" | "stopped" }
  | { state: "crashed"; code?: number | null; message: string };

/** Mirrors aether-protocol::picker::PickerItem (serde tag = "kind", snake_case). `match_indices`
 *  are code-point offsets into the row's display string, covered by the fuzzy match. */
export type PickerItem =
  | { kind: "file"; path_index: number; relative_path: string; match_indices?: number[] }
  | { kind: "buffer"; buffer_id: BufferId; display: string; dirty: boolean; match_indices?: number[] }
  | {
      kind: "grep_hit";
      path_index: number;
      relative_path: string;
      line: number;
      col: number;
      preview: string;
      match_indices?: number[];
    }
  | { kind: "diagnostic"; line: number; col: number; severity: DiagnosticSeverity; message: string; match_indices?: number[] }
  | { kind: "project"; name: string; match_indices?: number[] }
  | { kind: "dir_entry"; name: string; is_dir: boolean; match_indices?: number[] }
  | { kind: "root"; path_index: number; match_indices?: number[] }
  | {
      kind: "lsp_server";
      name: string;
      language: string;
      workspace_root: string;
      root_label?: string;
      status: LspStatus;
      match_indices?: number[];
    };

export interface PickerViewParams {
  kind: PickerKind;
  reset?: boolean;
  offset: number;
  limit: number;
  /** Explorer: absolute directory to list (null/omitted = server default). */
  directory_path?: string | null;
  /** Explorer: list the project roots instead of a directory. */
  explorer_roots?: boolean;
  /** Diagnostics: the buffer whose diagnostics to list (required on reset open). */
  buffer_id?: BufferId;
  /** Frame the window so this item is visible (used to pre-select a row on open / navigation). */
  center_on?: PickerItem | null;
  /** Grep only: resolve this buffer's cursor to the nearest cached hit and center on it. */
  center_on_cursor_grep_hit?: BufferId;
}
export interface PickerViewResult {
  query: string;
  generation: number;
  total_candidates: number;
  effective_offset: number;
  /** The item the server framed the window around (the resolved grep hit when cursor-centering). */
  effective_center_on?: PickerItem | null;
  /** Explorer: canonical absolute path of the directory being listed. */
  directory_path?: string | null;
  /** Explorer: parent directory if still inside the project boundary, else null. */
  directory_parent?: string | null;
}
export interface PickerSelectParams {
  kind: PickerKind;
  item: PickerItem;
}
/** Per-kind action result (serde tag = "kind", snake_case). */
export type PickerSelectResult =
  | { kind: "file"; path: string }
  | { kind: "buffer"; buffer_id: BufferId }
  | { kind: "file_at"; path: string; position: LogicalPosition }
  | { kind: "project"; name: string };
export interface PickerQueryParams {
  kind: PickerKind;
  query: string;
  generation: number;
}
export interface PickerHideParams {
  kind: PickerKind;
}
export interface PickerUpdateParams {
  kind: PickerKind;
  generation: number;
  offset: number;
  items: PickerItem[];
  total_matches: number;
  total_candidates: number;
  ticking: boolean;
  /** Grep: display-row index (hits + file headers) of this window's first item. */
  grep_display_offset?: number | null;
  /** Grep: total display rows (hits + file groups), for sizing the virtual-scroll spacer. */
  grep_total_display_rows?: number | null;
}

export interface ViewportLinesChangedParams {
  viewport_id: ViewportId;
  revision: Revision;
  range: LogicalLineRange;
  replacement_lines: LogicalLineRender[];
  line_count: number;
  max_scroll_logical_line: number;
  total_visual_rows: number;
  first_visual_row: number;
  max_line_width: number;
}
