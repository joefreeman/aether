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

export interface WrappedRow {
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

/** "deleted" is the inline diff view's phantom baseline row; the rest are a generated patch's
 *  chrome, which is deliberately not buffer text so the cursor can never land on it. */
/** Which piece of a generated patch's chrome a row is. */
export type ChromeKind =
  | "rule"
  | "file_header"
  | "hunk_header"
  | "spacer"
  | "summary";

/** Where a chrome row sits on the file rail. Structure, not presentation: the server answers it
 *  once and each client spells it its own way (here, CSS classes). */
export type RailJoin = "opens" | "tees" | "closes" | "detached";

/** The leaves and rows of the element vocabulary — the part of `ViewNode` a single row contains.
 *  A convenience alias, not a separate type: since the two vocabularies merged there is only
 *  `ViewNode`. Mirrors `aether_protocol::ui::Element`. */
export type UiElement = ViewNode;

/** One intra-line diff emphasis range: byte offsets within the owning line's / row's text. */
export interface EmphasisRange {
  start: number;
  end: number;
}

export type DiffMarker = "added" | "modified" | "deleted";

/** Which part of a merge-conflict block a line belongs to. Present only on files a stopped merge
 *  or rebase left conflicted; the blocks are masked out of that file's diff, so a line never
 *  carries both this and a change marker — `LineChange` has no variant that could. */
export type ConflictLine = "marker" | "ours" | "base" | "theirs";

/** Which side of a *generated* patch a line is — the read-only buffers `git/show` materialises
 *  from a commit. Separate from DiffMarker, which decorates a file against its baseline: there a
 *  removal is a phantom row, here both sides are ordinary lines. Never both on one buffer. */
export type PatchLine = "added" | "removed";

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

/** A baseline line the buffer removed or replaced, drawn above the surviving line while the inline
 *  diff view is on. Occupies a screen row, holds no cursor position.
 *
 *  Anchored to a line, unlike a patch's chrome — which belongs *between* two hunks and is a
 *  `Node` in the view's tree. Mirrors `aether_protocol::viewport::BaselineRow`. */
export interface BaselineRow {
  text: string;
  /** Omitted on the wire when "unstaged". */
  stage?: DiffStage;
  emphasis?: EmphasisRange[];
}

/** A view's content: what it is composed of, in order.
 *
 *  Named `ViewNode`, not `Element`: the DOM has a global of that name and this file is read in a
 *  browser context. Mirrors `aether_protocol::ui::Element` — **one** vocabulary for both axes now,
 *  so an editor may sit inside a row. An ordinary buffer is a single `editor`; a generated patch
 *  interleaves chrome and hunks. */
export type ViewNode =
  | { node: "stack"; children: ViewNode[] }
  | { node: "row"; children: ViewNode[] }
  | { node: "chrome"; kind: ChromeKind; rail: RailJoin; children: ViewNode[] }
  | { node: "text"; text: string; highlights?: Highlight[] }
  | { node: "space"; cols: number }
  | { node: "fill"; glyph: string }
  | {
      node: "editor";
      element: number;
      /** The buffer this element windows, and its **total** visual row count — of which `lines` is
       *  the slice currently loaded. Together they let the shell lay out and scroll a view from the
       *  tree alone, requesting more of a buffer only as its element scrolls into range. */
      buffer: number;
      rows: number;
      /** A line of **`buffer`**, not of the view — see `BufferWindow.first_view_line`. */
      first_buffer_line: number;
      lines: LogicalLineRender[];
    };

/** One row-producing item of a view, in order — mirrors `grid::RowItem`.
 *
 *  A chrome row occupies a screen row exactly as a phantom baseline row does, but belongs to no
 *  logical line, which is why rows can no longer simply be summed per line.
 *
 *  A line carries its element because a logical line number is unique only *within* an element once
 *  elements window different buffers. The lookups below still match on the number alone — correct
 *  while every element windows one document, and what changes when focus arrives to supply the
 *  element (the Rust side already threads it through `grid::position_cell`/`hit_test`). */
export type RowItem =
  | { kind: "chrome" }
  | { kind: "line"; element: number; line: LogicalLineRender };

export function rowItems(root: ViewNode): RowItem[] {
  const out: RowItem[] = [];
  const walk = (n: ViewNode) => {
    if (n.node === "stack") n.children.forEach(walk);
    else if (n.node === "editor")
      for (const line of n.lines) out.push({ kind: "line", element: n.element, line });
    // Chrome, or any inline element standing on its own: one screen row, no cursor position.
    else out.push({ kind: "chrome" });
  };
  walk(root);
  return out;
}

/** Visual rows an item occupies. Mirrors `grid::RowItem::rows`. */
export function itemRows(i: RowItem): number {
  return i.kind === "chrome"
    ? 1
    : (i.line.baseline_above?.length ?? 0) + i.line.visual_rows.length;
}

/** Every rendered line of a view, in order — for the paths that want lines and no structure. */
export function nodeLines(n: ViewNode): LogicalLineRender[] {
  if (n.node === "editor") return n.lines;
  if (n.node === "stack") return n.children.flatMap(nodeLines);
  return [];
}

/** The leaves of one row, left to right — text, spaces and fills, in painting order.
 *  Mirrors `Element::inline`. */
export function inlineOf(n: ViewNode): ViewNode[] {
  if (n.node === "text" || n.node === "space" || n.node === "fill") return [n];
  if (n.node === "stack" || n.node === "row" || n.node === "chrome")
    return n.children.flatMap(inlineOf);
  return [];
}

/** What a painter draws on one visual row — mirrors `grid::PaintedRow`.
 *
 *  Every row of a view is exactly one of these, in the order `paintedRows` produces. Replaces the
 *  old `chromeByLine`, which keyed chrome by the logical line beneath it: two elements windowing
 *  different files both start at, say, line 10, so their headings collapsed onto one key and the
 *  first file lost its own. */
export type PaintedRow =
  | { kind: "chrome"; node: ViewNode }
  | { kind: "baseline"; element: number; line: LogicalLineRender; index: number; row: BaselineRow }
  | {
      kind: "text";
      element: number;
      line: LogicalLineRender;
      row: WrappedRow;
      rowIndex: number;
      /** The final rendered row of the whole view — what a closing rule hangs off. Positional, not
       *  `logical_line + 1 === view_line_count`, which compares a buffer line to a view line. */
      lastLine: boolean;
    };

/** Every visual row of the loaded window, top to bottom. Mirrors `grid::painted_rows`; the Rust
 *  side is the specification and is tested against these same shapes. */
export function paintedRows(root: ViewNode): PaintedRow[] {
  const out: PaintedRow[] = [];
  let pending: ViewNode[] = [];
  const editors: { element: number; lines: LogicalLineRender[]; chrome: ViewNode[] }[] = [];
  const walk = (n: ViewNode) => {
    if (n.node === "stack") n.children.forEach(walk);
    else if (n.node === "editor") {
      editors.push({ element: n.element, lines: n.lines, chrome: pending });
      pending = [];
    } else pending.push(n);
  };
  walk(root);
  const total = editors.reduce((n, e) => n + e.lines.length, 0);
  let seen = 0;
  for (const e of editors) {
    for (const node of e.chrome) out.push({ kind: "chrome", node });
    for (const line of e.lines) {
      (line.baseline_above ?? []).forEach((row, index) =>
        out.push({ kind: "baseline", element: e.element, line, index, row }),
      );
      seen += 1;
      line.visual_rows.forEach((row, rowIndex) =>
        out.push({
          kind: "text",
          element: e.element,
          line,
          row,
          rowIndex,
          lastLine: seen === total,
        }),
      );
    }
  }
  // Chrome with no editor below it is the patch's closing rule.
  for (const node of pending) out.push({ kind: "chrome", node });
  return out;
}

export interface LogicalLineRender {
  logical_line: number;
  visual_rows: WrappedRow[];
  search_matches?: SearchMatchRange[];
  baseline_above?: BaselineRow[];
  /** Closing chrome after the final line — a patch has no trailing newline to hang it on. */
  /** This line's change-state. Omitted on the wire when there is none, which is most lines. */
  change?: LineChange;
  diagnostics?: DiagnosticSpan[];
  sneak_targets?: SneakTarget[];
}

/** What a line's own change-state is: changed against a baseline, conflicted, or a side of a
 *  generated patch.
 *
 *  One tagged value rather than the five parallel fields this used to be, because the cases are
 *  mutually exclusive by construction: a conflicted file's blocks are masked out of its own diff,
 *  and a generated patch has no baseline to diff against. Mirrors `aether_protocol::viewport::
 *  LineChange`. */
export type LineChange =
  | { kind: "none" }
  | {
      kind: "changed";
      marker: DiffMarker;
      stage: DiffStage;
      /** Diff-view only: sub-ranges the change actually touched. */
      emphasis?: EmphasisRange[];
    }
  | { kind: "conflict"; side: ConflictLine }
  | {
      kind: "patch";
      side: PatchLine;
      stage: DiffStage;
      emphasis?: EmphasisRange[];
    };

/** Accessors mirroring the Rust ones, so call sites stay as short as the five fields were. */
export const changeMarker = (c?: LineChange): DiffMarker | null =>
  c?.kind === "changed" ? c.marker : null;
export const changeStage = (c?: LineChange): DiffStage =>
  c && (c.kind === "changed" || c.kind === "patch") ? c.stage : "unstaged";
export const changeEmphasis = (c?: LineChange): EmphasisRange[] =>
  c && (c.kind === "changed" || c.kind === "patch") ? (c.emphasis ?? []) : [];
export const changeConflict = (c?: LineChange): ConflictLine | null =>
  c?.kind === "conflict" ? c.side : null;
export const changePatchSide = (c?: LineChange): PatchLine | null =>
  c?.kind === "patch" ? c.side : null;

export interface BufferWindow {
  /** The slice of the **view** loaded, in view lines — indices into the concatenation of its
   *  elements' extents. Not lines of any file: in a patch these match no buffer's numbering.
   *  Mirrors `aether_protocol::coords::ViewLine`. */
  first_view_line: number;
  last_view_line_exclusive: number;
  /** How many lines the **view** has: its elements' extents, summed. */
  view_line_count: number;
  max_scroll_view_line: number;
  /** Total visual rows in the view (real + diff phantom + chrome) — sizes the native scroller. */
  total_visual_rows: number;
  /** Visual row where first_view_line begins — positions the window in the scroller. */
  first_visual_row: number;
  /** Display cols of the widest line — sizes the native horizontal scroller (no-wrap). 0 under soft wrap. */
  max_line_width: number;
  /** Buffer-level Git status (branch + staged/unstaged counts) for the status bar; absent outside a repo. */
  git_status?: GitBufferStatus;
  /** Any buffer this view windows *other than* the focused element's has unsaved changes — the
   *  view-wide half of the status dot. Excludes the focused element deliberately: the client knows
   *  that one first-hand and instantly, so the dot is `focused dirty || this`, which stays right
   *  across a save (a save pushes buffer/state, not a new window). Always false for one element. */
  other_elements_dirty?: boolean;
  /** What the view is composed of. Use `nodeLines` where the structure is irrelevant. */
  root: ViewNode;
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
  /** Set when the repo is diffed against something other than the index. The gutter then means
   *  "changed since this commit" — or "since I last saved" — and `staged` is always empty, so the
   *  status bar has to say so. Absent for the default baseline, which needs no explaining. */
  baseline?: GitBaselineSource | null;
}

/** What to diff against — the request form (`git/set_baseline`). */
export type GitBaselineChoice =
  | { kind: "saved" }
  | { kind: "rev"; rev: string };

/** A non-default diff baseline in force, resolved. A revision is pinned at set time, so `label` is
 *  what the user asked for and `commit` is what it resolved to. */
export type GitBaselineSource =
  | { kind: "saved" }
  | { kind: "rev"; label: string; commit: string };

/** One ref pointing at a commit — an entry of the `(HEAD -> main, tag: v1.0, origin/main)`
 *  decoration `git log --decorate` prints after the hash. `head_branch` is the checked-out branch
 *  (`HEAD -> main`), `head` a detached HEAD; the two never appear together on one commit. */
export interface CommitRef {
  kind: "head" | "head_branch" | "branch" | "remote" | "tag" | "stash";
  /** The short name git prints: `main`, `origin/main`, `v1.0`, `HEAD`. Never carries the `tag: `
   *  marker — that's implied by the kind and added when rendering. */
  name: string;
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

/** Which element of a view holds the cursor, and the buffer it windows — the reply to
 *  `view/focus_element`, and what a composed view's subscribe carries so a client binds to the
 *  buffer it is actually looking at rather than to the view's own document. */
export interface ViewportFocusElementResult {
  element: number;
  buffer: BufferOpenResult;
}

export interface ViewportSubscribeResult {
  viewport_id: ViewportId;
  window: BufferWindow;
  /** Buffer-level status snapshotted at subscribe time — see the server's BufferStatusSnapshot.
   *  Lets a client seed external-change flags, diagnostic counts, and LSP health the moment it
   *  starts showing a buffer, rather than waiting for the next change-notification. */
  buffer_status?: BufferStatusSnapshot;
  /** Which element holds the cursor and the buffer it windows — present only for a *composed* view,
   *  whose elements window buffers other than the one subscribed to. The core adopts it; a shell
   *  never reads it directly. Absent for an ordinary editor view, where the subscribed buffer is
   *  already the one the cursor is in. */
  focus?: ViewportFocusElementResult;
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
  | "git_stash"
  | "git_baseline";

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
      kind: "git_baseline";
      /** Which repo the row would re-baseline — echoed onto the `git/set_baseline` it fires. */
      repo_id: string;
      /** What Enter sends. Absent is the "back to the default" row, which is the same absent the
       *  RPC takes to clear a baseline. */
      choice?: GitBaselineChoice | null;
      /** Row text and fuzzy haystack: `(index)`, `(saved)`, `HEAD`, a branch name. The bracketed
       *  ones are not revisions, which is also the section split. */
      label: string;
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
      /** The refs pointing at this commit, rendered between the hash and the subject the way
       *  `git log --oneline --decorate` prints them. Absent for almost every commit. */
      decorations?: CommitRef[];
      /** Offsets into `subject` covered by the fuzzy match. The decorations are rendered but never
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
       * whole-target entry (a file or buffer captured without a position), which renders with no
       * number at all. */
      line?: number;
      /** The captured entry's flat display text. */
      display: string;
      match_indices?: number[];
    }
  | {
      kind: "group";
      /** A collapsible group's header row — a real, selectable
       *  window row, not a derived decoration. Click toggles the group open or shut; Enter
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
  /** Collapsible kinds only: the run's item count and whether it is
   *  expanded — the same decoration as its `group` row, so a sticky pin standing in
   *  for a scrolled-off header renders identically. Absent for the derived-header kinds. */
  count?: number;
  expanded?: boolean;
}
