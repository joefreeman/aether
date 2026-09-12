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
//!      workspace/activate, view/open; geometry: view/subscribe|window|resize),
//!      because their params need pixels or they run before the core exists.
//!
//! Keep field names exactly matching the serde wire format.

import type { MdBlock } from "./markdown";

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

/** Where a viewport's top is, as **content**: the element it is in, a line of that element's
 *  buffer, and how far into the line's rows it sits. What a subscribe opens at, what a window
 *  request reports, and what a reopen restores — never a row, which only the client can count. */
export interface ScrollPosition {
  element: number;
  line: number;
  sub_row: number;
}

/** One element's rows a viewport reaches, by row within that element — mirrors `SliceRequest`. */
export interface SliceRequest {
  element: number;
  from_row: number;
  rows: number;
}

/** `view/window`: the slices a client's viewport reaches, and where its top is as content. Built
 *  by the core (`window_request`) from the tree it holds; the shell only sends it. */
export interface ViewportWindowParams {
  viewport_id: ViewportId;
  anchor: ScrollPosition;
  slices: SliceRequest[];
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

/** Cells per side, in the order a stylesheet names them. Mirrors `ui::Sides`; absent sides are 0. */
export interface Sides {
  top?: number;
  right?: number;
  bottom?: number;
  left?: number;
}

/** The cells a container spends on itself — a border, and padding inside it. Mirrors `ui::Edges`.
 *
 *  Structure, not geometry: how many cells the box costs changes how many its content gets, and the
 *  server wraps to that. What a border *looks like* is this shell's business (a CSS border), as the
 *  terminal's is a box-drawing glyph. Counted in cells; absent means zero, and the whole object is
 *  omitted when nothing is set.
 *
 *  `collapse` asks whether adjacent bordered children share an edge rather than each drawing its
 *  own — `border-collapse`, on the container because only a parent can see two siblings meet. */
export interface Edges {
  border?: Sides;
  padding?: Sides;
  collapse?: boolean;
}

/** The fill a container paints behind its border and padding cells, with children painting over
 *  their own content area. Mirrors `ui::Band` — a closed set, not a colour: every shell matches it
 *  exhaustively, so a new band cannot render as nothing here. */
export type Band = "none" | "chrome";

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
 *  browser context. Mirrors `aether_protocol::ui::Element` — **one** vocabulary for both axes now.
 *  An ordinary buffer is a single `editor`; a generated patch interleaves chrome and hunks.
 *
 *  An `editor` inside a `row` is representable and *not* renderable: a view is laid out as a flat
 *  top-to-bottom list of rows, which cannot express two editors sharing rows. Side-by-side diff
 *  needs a different row model. Until then an editor is expected to be a child of a `stack`. */
export type ViewNode =
  | {
      node: "column";
      edges?: Edges;
      band?: Band;
      /** What the box's **top border** says, drawn on the border row itself: a rule cell after the
       *  corner, a space, these nodes, a space, then the rule on to the far corner. Inline nodes,
       *  the kinds a row holds. Absent for an untitled box, which is every box a patch draws.
       *
       *  It costs the box no rows — `paintedRows` says nothing about it, and the painter reads it
       *  off the top edge row's `owner`. Mirrors `Element::Column`'s `title`. */
      title?: ViewNode[];
      children: ViewNode[];
    }
  | { node: "row"; edges?: Edges; band?: Band; children: ViewNode[] }
  | { node: "text"; text: string; highlights?: Highlight[] }
  | { node: "space"; cols: number }
  | { node: "fill"; glyph: string }
  | {
      node: "editor";
      element: number;
      /** The buffer this element windows, and its **total** visual row count — of which `lines` is
       *  the slice currently loaded, starting `first_row` rows into the element. Together they let
       *  the shell lay out and scroll a view from the tree alone, requesting more of a buffer only
       *  as its element scrolls into range. Mirrors `Element::Editor`. */
      buffer: number;
      rows: number;
      first_row: number;
      /** A line of **`buffer`**, not of the view: in a patch these match no view numbering. */
      first_buffer_line: number;
      lines: LogicalLineRender[];
      /** Who lays the lines out — mirrors `LayoutOwner`. Absent for the ordinary server-wrapped
       *  editor. `client`: the lines come unwrapped, one row per line, and the true height is
       *  whatever the shell measured (`Measured`). */
      laid_out_by?: "server" | "client";
      /** What the element is *for* — mirrors `ElementRole`. Absent for content, which is every
       *  element of an ordinary or composed view. `input`: the line a shell's next command is
       *  typed into, which is also how a client knows the view it is showing is a shell. */
      role?: "field" | "input";
    }
  | {
      /** Rendered prose — a span of a buffer as markdown, not as lines. Mirrors `Element::Prose`.
       *
       *  It carries no lines at all: the server parses once, for every shell, and the blocks *are*
       *  the content. Its height is whatever this shell measured of the type it drew, so it is
       *  absent from `paintedRows` — `elementOrigins` is where it is placed.
       *
       *  A field id, the parse, and `source` — where the parsed text's lines begin and how long it
       *  is. The reading view is addressed by position (the server owns the cursor and reports it
       *  as a line and a column), and that table is what resolves one against a block. The core
       *  reads it; this shell measures blocks and lets the core place the lines. */
      node: "prose";
      element: number;
      blocks: MdBlock[];
      source: SourceLines;
    };

/** Mirrors `ui::SourceLines`: the shape of the text a parse came from, without the text — the byte
 *  offset each line starts at (one entry per line, always starting 0) and the text's byte length. */
export interface SourceLines {
  starts: number[];
  byte_len: number;
}

/** What the shell measured of an element it laid out itself — mirrors `grid::MeasuredElement`:
 *  the wire row of the first loaded line, the offset within the element each loaded line starts
 *  at (the first is `first_row` rows down; unloaded lines above count one row each), and the
 *  offset after the last loaded line. Offsets are in the `Measured`'s units. */
export interface MeasuredElement {
  first_row: number;
  starts: number[];
  end: number;
}

/** Mirrors `grid::Measured`: the resolution every height and offset of the view's vertical layout
 *  is counted in — `units_per_row` units make one row — and, per element keyed by field id, what
 *  the shell measured of the elements it laid out itself. The terminal counts whole rows; this
 *  shell counts thousandths of one, so a block of prose lands where it was drawn, while every row
 *  the server laid out is exactly `units_per_row`. */
export interface Measured {
  units_per_row: number;
  elements: Record<number, MeasuredElement>;
}

/** Whole rows, nothing measured — every shell's starting point. */
export const WHOLE_ROWS: Measured = { units_per_row: 1, elements: {} };

function measuredOf(n: ViewNode, measured: Measured): MeasuredElement | undefined {
  if (n.node === "prose") return measured.elements[n.element];
  return n.node === "editor" && n.laid_out_by === "client"
    ? measured.elements[n.element]
    : undefined;
}

/** Units an editor node occupies: the tree's row count at this resolution, or the measured
 *  height for one the client laid out — unloaded lines at one row each, loaded ones as measured. */
function editorHeight(n: ViewNode & { node: "editor" }, measured: Measured): number {
  const unit = measured.units_per_row;
  const m = measuredOf(n, measured);
  if (!m) return n.rows * unit;
  const loaded = m.starts.length;
  const before = m.first_row;
  const measuredUnits = Math.max(0, m.end - (m.starts[0] ?? before * unit));
  return before * unit + measuredUnits + Math.max(0, n.rows - before - loaded) * unit;
}

/** The offset within an editor node that wire row `row` starts at. */
export function offsetOf(n: ViewNode, row: number, measured: Measured): number {
  const unit = measured.units_per_row;
  const m = measuredOf(n, measured);
  if (!m) return row * unit;
  const i = row - m.first_row;
  if (i < 0) return row * unit;
  return i < m.starts.length ? m.starts[i] : m.end + (i - m.starts.length) * unit;
}

/** Every rendered line of a view, in order — for the paths that want lines and no structure. */
/** Every editor node in the tree, in view order — mirrors `Element::editors` in the core.
 *
 *  Recurses through the **container** kinds only. Not `inlineOf`: that flattens a subtree to its
 *  inline *leaves* and answers `[n]` for a text/space/fill node, so walking it as if it gave
 *  children recurses forever — which is exactly what it did.
 */
export function editorsOf(root: ViewNode): Extract<ViewNode, { node: "editor" }>[] {
  const out: Extract<ViewNode, { node: "editor" }>[] = [];
  const walk = (n: ViewNode): void => {
    if (n.node === "editor") {
      out.push(n);
      return;
    }
    if (n.node === "column" || n.node === "row") n.children.forEach(walk);
  };
  walk(root);
  return out;
}

/** Every prose node in the tree, in view order — mirrors the prose half of `Element::content`. */
export function proseOf(root: ViewNode): Extract<ViewNode, { node: "prose" }>[] {
  const out: Extract<ViewNode, { node: "prose" }>[] = [];
  const walk = (n: ViewNode): void => {
    if (n.node === "prose") {
      out.push(n);
      return;
    }
    if (n.node === "column" || n.node === "row") n.children.forEach(walk);
  };
  walk(root);
  return out;
}

export function nodeLines(n: ViewNode): LogicalLineRender[] {
  if (n.node === "editor") return n.lines;
  // Descends into `row` as well as `column`, matching `Element::lines`'s walk. It only matters for
  // a shape nothing produces yet, but a mirror that stops one level shallower than the thing it
  // mirrors is a difference waiting to be discovered the hard way.
  if (n.node === "column" || n.node === "row") return n.children.flatMap(nodeLines);
  return [];
}

/** The leaves of one row, left to right — text, spaces and fills, in painting order.
 *  Mirrors `Element::inline`. */
export function inlineOf(n: ViewNode): ViewNode[] {
  if (n.node === "text" || n.node === "space" || n.node === "fill") return [n];
  if (n.node === "column" || n.node === "row") return n.children.flatMap(inlineOf);
  return [];
}

/** What a painter draws on one visual row, and the absolute row it sits on — mirrors
 *  `grid::PaintedRow` paired with its `VisualRow`.
 *
 *  Every loaded row of a view is exactly one of these, in the order `paintedRows` produces; the
 *  rows between two entries that are not consecutive are loaded by nobody and painted blank. */
export type PaintedRow = {
  at: number;
  /** Cells the enclosing boxes claim on each side — an inset, not a width: this walk never learns
   *  the viewport's size, so the shell subtracts these from the width it has. */
  left: number;
  right: number;
  /** Border cells the enclosing boxes run down each side — the rails, as distinct from the
   *  padding beside them, so a shell knows where to draw a line and not only how far to indent. */
  rails: Sides;
  /** What the innermost enclosing box paints behind its own border and padding cells. */
  band: Band;
} & (
  | { kind: "chrome"; node: ViewNode }
  /** A box's own border/padding row. No tree node stands for one, so it names its container. */
  | { kind: "edge"; owner: ViewNode; side: "top" | "bottom"; join: RailJoin }
  | { kind: "baseline"; element: number; line: LogicalLineRender; index: number; row: BaselineRow }
  | {
      kind: "text";
      element: number;
      line: LogicalLineRender;
      row: WrappedRow;
      rowIndex: number;
      /** The final rendered row of the whole view. Positional, not `logical_line + 1 === some
       *  line count`, which compares a buffer line to a count of the wrong space. */
      lastLine: boolean;
    }
);

/** Units the whole view occupies: chrome, one row each, every editor's height — the tree's, or
 *  the shell's own measurement for an element the client laid out — at the measured resolution.
 *  Mirrors `grid::total_rows`. */
export function totalRows(root: ViewNode, measured: Measured = WHOLE_ROWS): number {
  let total = 0;
  walkRows(root, measured, NO_FRAME, (_v, _f, rows) => (total += rows));
  return total;
}

/** One pass over the tree in painting order, telling `f` each node's height in units: an editor's
 *  height, one row for chrome or any inline element standing on its own, nothing for a stack.
 *  Mirrors `grid::walk_rows`. */
/** How far in the boxes hold a row, and whether any runs a rail down its left side. Mirrors
 *  `grid::Frame`. */
interface Frame {
  left: number;
  right: number;
  rails: Sides;
  band: Band;
}

const NO_FRAME: Frame = { left: 0, right: 0, rails: {}, band: "none" };

function sides(s?: Sides): { t: number; r: number; b: number; l: number } {
  return { t: s?.top ?? 0, r: s?.right ?? 0, b: s?.bottom ?? 0, l: s?.left ?? 0 };
}

/** This frame with `edges`' own sides added — one box deeper. Mirrors `grid::Frame::plus`. */
function deeper(f: Frame, edges?: Edges, band?: Band): Frame {
  const b = sides(edges?.border);
  const p = sides(edges?.padding);
  return {
    left: f.left + b.l + p.l,
    right: f.right + b.r + p.r,
    // Rails accumulate: a box inside a box is railed on any side either of them draws.
    rails: {
      left: Math.max(f.rails.left ?? 0, b.l),
      right: Math.max(f.rails.right ?? 0, b.r),
    },
    // The innermost band that declares one, so a plain box inside a banded one does not repaint it.
    band: !band || band === "none" ? f.band : band,
  };
}

/** Rows a box spends above and below its content. Mirrors `grid::Measured::box_rows`. */
function boxRows(edges: Edges | undefined, unit: number): number {
  const b = sides(edges?.border);
  const p = sides(edges?.padding);
  return (b.t + p.t + b.b + p.b) * unit;
}

type Visit =
  | { node: ViewNode; edge?: undefined }
  | { node: ViewNode; edge: "top" | "bottom" };

function walkRows(
  n: ViewNode,
  measured: Measured,
  frame: Frame,
  f: (v: Visit, frame: Frame, rows: number) => void,
): void {
  const unit = measured.units_per_row;
  if (n.node === "column") {
    const inner = deeper(frame, n.edges, n.band);
    const b = sides(n.edges?.border);
    const p = sides(n.edges?.padding);
    for (let i = 0; i < b.t + p.t; i++) f({ node: n, edge: "top" }, inner, unit);
    n.children.forEach((c) => walkRows(c, measured, inner, f));
    for (let i = 0; i < b.b + p.b; i++) f({ node: n, edge: "bottom" }, inner, unit);
  } else if (n.node === "editor") {
    f({ node: n }, frame, editorHeight(n, measured));
  } else if (n.node === "prose") {
    // Prose has no height until this shell has drawn it — proportional type cannot be counted in
    // rows — so it stands at one row until the measure lands, and a guess would only put
    // everything below it somewhere it has to move back from.
    f({ node: n }, frame, measured.elements[n.element]?.end ?? unit);
  } else if (n.node === "row") {
    const inner = deeper(frame, n.edges, n.band);
    const b = sides(n.edges?.border);
    const p = sides(n.edges?.padding);
    for (let i = 0; i < b.t + p.t; i++) f({ node: n, edge: "top" }, inner, unit);
    f({ node: n }, inner, unit);
    for (let i = 0; i < b.b + p.b; i++) f({ node: n, edge: "bottom" }, inner, unit);
  }
  // A chrome group is one screen row — its children share it — as is any inline element standing
  // on its own. An editor nested in one would be drawn as a single chrome row while `nodeLines`
  // still counted its lines; the Rust builder asserts against that shape, and this mirror inherits
  // the same expectation rather than re-deriving it.
  else f({ node: n }, frame, unit);
}

/** Fill in each edge row's join from **rail continuity** — whether a railed box encloses the row
 *  above the rule and the row below it. Mirrors `grid::resolve_joins`; see it for why a join is
 *  about the rail rather than about which edges happen to be adjacent. */
function resolveJoins(rows: PaintedRow[]): void {
  // Which box each rule belongs to, by identity — `undefined` for a row that is not one.
  const owner = rows.map((r) => (r.kind === "edge" ? r.owner : undefined));
  // The nearest neighbour that is not one of *this box's own* rules. Another box's rule is not
  // skipped over — it is the answer, and the answer is no rail: two boxes that each draw their own
  // edge close and open rather than both tee-ing. Sharing an edge is one row, not two.
  const neighbour = (from: number, step: number): boolean => {
    const mine = owner[from];
    for (let i = from + step; i >= 0 && i < rows.length; i += step) {
      if (owner[i] === undefined) return (rows[i].rails.left ?? 0) > 0;
      if (owner[i] !== mine) return false;
    }
    return false;
  };
  rows.forEach((r, i) => {
    if (r.kind !== "edge") return;
    const above = neighbour(i, -1);
    const below = neighbour(i, 1);
    r.join = above && below ? "tees" : below ? "opens" : above ? "closes" : "detached";
  });
}

/** Where each content element starts — its absolute row in the measured resolution, and the box
 *  around it. Mirrors `grid::element_origins`.
 *
 *  What a shell needs to paint an element it laid out **itself**: the rendered rows go at
 *  `origin + row`, indented by the placement's inset. `paintedRows` cannot answer this for prose,
 *  which contributes no rows of its own, and for a client-laid-out editor it places the *source
 *  lines* instead. The origin is also right when the top of a long block has scrolled out of the
 *  loaded slice, which the first placed line is not. */
export function elementOrigins(
  root: ViewNode,
  measured: Measured = WHOLE_ROWS,
): Record<number, ElementPlacement> {
  const out: Record<number, ElementPlacement> = {};
  let at = 0;
  walkRows(root, measured, NO_FRAME, (v, frame, height) => {
    if (v.edge === undefined && (v.node.node === "editor" || v.node.node === "prose")) {
      out[v.node.element] = {
        at,
        left: frame.left,
        right: frame.right,
        rails: frame.rails,
        band: frame.band,
      };
    }
    at += height;
  });
  return out;
}

/** Where one content element sits: its absolute row and the cells the boxes around it claim — the
 *  placement half of a `PaintedRow`, for an element whose rows this walk does not produce. */
export interface ElementPlacement {
  at: number;
  left: number;
  right: number;
  rails: Sides;
  band: Band;
}

/** Every loaded visual row of the view, top to bottom, each at its absolute row. Mirrors
 *  `grid::painted_rows`; the Rust side is the specification and is tested against these same
 *  shapes. Chrome occupies one row; an editor's loaded lines sit `first_row` rows into it and the
 *  rest of its height is unloaded — absent here, so consecutive entries need not be consecutive
 *  rows. */
export function paintedRows(root: ViewNode, measured: Measured = WHOLE_ROWS): PaintedRow[] {
  const out: PaintedRow[] = [];
  const unit = measured.units_per_row;
  const total = nodeLines(root).length;
  let seen = 0;
  let at = 0;
  walkRows(root, measured, NO_FRAME, (v, frame, height) => {
    const place = (row: number) => ({
      at: row,
      left: frame.left,
      right: frame.right,
      rails: frame.rails,
      band: frame.band,
    });
    if (v.edge) {
      // The join needs the whole sequence, so it is filled in below.
      out.push({ ...place(at), kind: "edge", owner: v.node, side: v.edge, join: "detached" });
      at += height;
      return;
    }
    const n = v.node;
    if (n.node === "editor") {
      const clientLaidOut = n.laid_out_by === "client";
      let row = at + n.first_row * unit;
      n.lines.forEach((line, i) => {
        // Where the shell put the line — or, unmeasured, one row per line.
        if (clientLaidOut) row = at + offsetOf(n, n.first_row + i, measured);
        (line.baseline_above ?? []).forEach((brow, index) => {
          out.push({ ...place(row), kind: "baseline", element: n.element, line, index, row: brow });
          row += unit;
        });
        seen += 1;
        line.visual_rows.forEach((wrow, rowIndex) => {
          out.push({
            ...place(row),
            kind: "text",
            element: n.element,
            line,
            row: wrow,
            rowIndex,
            lastLine: seen === total,
          });
          row += unit;
        });
      });
    } else if (n.node === "prose") {
      // Prose paints no rows here. It occupies its measured height — the rows below it are placed
      // past it — but *what* is in those rows is the shell's own rendering of the blocks, placed
      // by `elementOrigins`. A row list cannot carry it: a rendered block has more rows than the
      // wire has anything to put in them.
    } else out.push({ ...place(at), kind: "chrome", node: n });
    at += height;
  });
  resolveJoins(out);
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

/** A view as the server renders it: the tree, which carries every element's height and each
 *  loaded slice's place within its element, plus the view-wide facts riding along. No geometry of
 *  the view as a whole: the shell lays it out from the tree (`paintedRows`, `totalRows`). */
export interface BufferWindow {
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

// ---- bootstrap RPC results (workspace/list, workspace/activate, view/open) -------------------------

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
  last_view_id?: number | null;
  /** With `open_last`: the landing buffer (MRU or fresh transient scratch), fully opened. */
  opened?: ViewOpenResult | null;
}

export interface LspServerRef {
  language: string;
  workspace_root: string;
}

export interface ViewOpenResult {
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
  /** True when this client is reading the file as the rendered document — its window will carry
   *  one prose element rather than lines. Absent means false; a client's own presentation mode
   *  of a markdown file, flipped with `view/set_read`. */
  read?: boolean;
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

// ---- geometry RPC results (view/subscribe, window, resize) --------------------------------------

/** Which element of a view holds the cursor, and the buffer it windows — the reply to
 *  `view/focus_element`, and what a composed view's subscribe carries so a client binds to the
 *  buffer it is actually looking at rather than to the view's own document. */
export interface ViewportFocusElementResult {
  element: number;
  buffer: ViewOpenResult;
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
  | "shells"
  | "agents"
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

/** Mirrors aether-protocol::picker::AgentRowState (serde tag = "state", snake_case). A badge on
 *  the agents-picker row, never a sort key. */
export type AgentRowState =
  | { state: "idle" }
  | { state: "thinking"; activity?: string }
  | { state: "awaiting_permission" }
  | { state: "disconnected" };

/** Mirrors aether-protocol::picker::PickerItem (serde tag = "kind", snake_case). `match_indices`
 *  are code-point offsets into the row's display string, covered by the fuzzy match. */
export type PickerItem =
  | { kind: "file"; path_index: number; relative_path: string; match_indices?: number[]; git_status?: GitStatus }
  | { kind: "buffer"; buffer_id: BufferId; view_id: number; display: string; status?: BufferDirtyState; path_index?: number; relative_path?: string; match_indices?: number[]; transient?: boolean }
  /** A shell view. `match_indices` are code-point offsets into the composed haystack
   *  `"{title}  {cwd}  {last_command}"` (empty parts elided, two spaces between the rest) — the
   *  server's `shell_haystack`, mirrored by `rowMatchSegments`. `cwd` arrives already shortened
   *  to `~/…`; `exit`/`elapsed_ms` describe the last *finished* run, so they survive a new one
   *  starting. */
  | {
      kind: "shell";
      view_id: number;
      title: string;
      cwd: string;
      last_command?: string;
      running?: boolean;
      exit?: number;
      elapsed_ms?: number;
      dormant?: boolean;
      match_indices?: number[];
    }
  /** An agent conversation. Haystack is `"{title}  {agent}  {last_prompt}"`, composed like the
   *  shell row's. */
  | {
      kind: "agent";
      view_id: number;
      title: string;
      agent: string;
      state?: AgentRowState;
      last_prompt?: string;
      dormant?: boolean;
      match_indices?: number[];
    }
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
  | { kind: "workspace"; name: string; unsaved?: number; match_indices?: number[] }
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
      /** Absolute path to the file containing the reference (fed into view/open on select). */
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
      /** Absolute path to the buffer's file (fed into view/open on select). */
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
