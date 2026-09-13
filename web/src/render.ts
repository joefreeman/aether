//! DOM rendering of a viewport Window. The server sends a fully-resolved render model (segments
//! with byte-offset highlights, plus per-line diagnostics / search / diff data); we paint it as
//! rows of <span>s. Each row is styled at code-point granularity then coalesced into spans, so
//! syntax colour, diagnostic underline, search tint, selection, and the cursor block compose
//! cleanly even when they overlap.

import { decodeRow, utf8ByteLen } from "./text";
import type { MdBlock } from "./markdown";
import { renderReply } from "./read";
import type {
  Band,
  BufferWindow,
  ConflictLine,
  Measured,
  CursorState,
  DiagnosticSeverity,
  DiffStage,
  EmphasisRange,
  LogicalLineRender,
  LogicalPosition,
  PatchLine,
  RailJoin,
  UiElement,
  BaselineRow,
  ElementPlacement,
  PaintedRow,
  ViewNode,
  WrappedRow,
} from "./protocol";
// Value imports: the `change*` accessors mirror the Rust ones on `LineChange`, so the call sites
// below stay as short as the five parallel fields they replaced.
import {
  elementOrigins,
  holdsCollapsed,
  inlineOf,
  paintedRows,
  proseOf,
  nodeLines,
  changeConflict,
  changeEmphasis,
  changeMarker,
  changePatchSide,
  changeStage,
} from "./protocol";

const CONTINUATION_MARKER = "↪ ";

/** Tree-sitter highlight kind → CSS class suffix. Mirrors ui.rs::lookup_exact. Unlisted kinds
 *  fall back by stripping trailing `.segments` (e.g. "function.call" → "function"). */
const HL_CLASS: Record<string, string> = {
  keyword: "keyword",
  string: "string",
  "string.escape": "string-special",
  "string.special": "string-special",
  comment: "comment",
  number: "constant",
  boolean: "constant",
  constant: "constant",
  "constant.builtin": "constant",
  function: "function",
  "function.call": "function",
  "function.macro": "macro",
  type: "type",
  "type.builtin": "type",
  constructor: "type",
  module: "type",
  namespace: "type",
  "variable.parameter": "parameter",
  "variable.builtin": "keyword",
  operator: "keyword",
  "punctuation.bracket": "punct",
  "punctuation.delimiter": "punct",
  "punctuation.special": "macro",
  attribute: "macro",
  label: "macro",
  tag: "keyword",
  property: "property",
  "text.title": "title",
  "text.literal": "string",
  "text.uri": "uri",
  "text.reference": "reference",
  "text.emphasis": "emphasis",
  "text.strong": "strong",
  "diff.meta": "diff-meta",
  "diff.hunk": "diff-hunk",
  "diff.file": "diff-file",
  "diff.added": "diff-added",
  "diff.removed": "diff-removed",
};

export function highlightClass(kind: string): string | null {
  let k = kind;
  while (k.length > 0) {
    const cls = HL_CLASS[k];
    if (cls) return "hl-" + cls;
    const dot = k.lastIndexOf(".");
    if (dot < 0) break;
    k = k.slice(0, dot);
  }
  return null;
}

const SEVERITY_RANK: Record<DiagnosticSeverity, number> = {
  error: 3,
  warning: 2,
  information: 1,
  hint: 0,
};

interface CellStyle {
  hl: string | null;
  diag: DiagnosticSeverity | null;
  search: boolean;
  sel: boolean;
  cursor: boolean;
  bracket: boolean;
  /** Inside an intra-line diff emphasis range (stronger change fill, under search/sel). */
  emph: boolean;
  /** Inside a sneak candidate word (quiet tint). */
  sneak: boolean;
  /** Inside a sneak typed-prefix chip (bright; blanked unless it's the label cell). */
  chip: boolean;
  /** The sneak label char to paint over this cell, or null. */
  sneakLabel: string | null;
}

function sameStyle(a: CellStyle, b: CellStyle): boolean {
  return (
    a.hl === b.hl &&
    a.diag === b.diag &&
    a.search === b.search &&
    a.sel === b.sel &&
    a.cursor === b.cursor &&
    a.bracket === b.bracket &&
    a.emph === b.emph &&
    a.sneak === b.sneak &&
    a.chip === b.chip &&
    a.sneakLabel === b.sneakLabel
  );
}

/** Selection footprint on one logical line, in line-local byte offsets. `toEnd` means the
 *  selection runs through the line's end (and newline) — a fully-covered interior line. */
interface LineSelection {
  start: number;
  end: number; // inclusive; ignored when toEnd
  toEnd: boolean;
}

function makeSpan(text: string, style: CellStyle, cursorClass: string): Node {
  // Sneak typed-prefix chip overrides other styling on its cells (like the terminal/iced clients):
  // the label glyph on the first cell, blanks on the rest, all on the bright label colour.
  if (style.sneakLabel) {
    const span = document.createElement("span");
    span.className = "sneak-label";
    span.textContent = style.sneakLabel;
    return span;
  }
  if (style.chip) {
    const span = document.createElement("span");
    span.className = "sneak-chip";
    span.textContent = text;
    return span;
  }
  const classes: string[] = [];
  if (style.hl) classes.push(style.hl);
  if (style.bracket) classes.push("match-bracket");
  if (style.diag) classes.push("diag-" + style.diag);
  // Emphasis is NOT a class here: renderVisualRow wraps each contiguous emphasized run in a
  // single `.diff-emph` wrapper span, so the rounded band is one box (per-span classes would
  // notch the rounding at every internal syntax-colour boundary).
  if (style.search) classes.push("search-hit");
  if (style.sneak) classes.push("sneak-target");
  if (style.sel) classes.push("sel");
  if (style.cursor) classes.push(cursorClass);
  if (classes.length === 0) return document.createTextNode(text);
  const span = document.createElement("span");
  span.className = classes.join(" ");
  span.textContent = text;
  return span;
}

/** A whitespace-indicator span (selected tab/trailing-space/newline). Reuses `makeSpan`'s styling
 *  (it carries the `sel` blue background) and tags it `ws-{kind}` so CSS paints the muted glyph. */
function wsSpan(text: string, style: CellStyle, kind: "tab" | "dot" | "nl", cursorClass: string): HTMLElement {
  const node = makeSpan(text, style, cursorClass);
  let span: HTMLElement;
  if (node instanceof HTMLElement) {
    span = node;
  } else {
    span = document.createElement("span");
    span.textContent = text;
  }
  span.classList.add("ws-" + kind);
  return span;
}

/** First index `i` in the sorted `byteStart[0..n)` with `byteStart[i] >= target` (i.e. `n` if none). */
function lowerBound(byteStart: number[], n: number, target: number): number {
  let lo = 0;
  let hi = n;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (byteStart[mid] < target) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

/** Code-point index whose byte == `target`, or -1. `byteStart` is sorted, so binary-search it. */
function cpAtByte(byteStart: number[], n: number, target: number): number {
  const i = lowerBound(byteStart, n, target);
  return i < n && byteStart[i] === target ? i : -1;
}

/** Apply `fn` to every code-point index whose byte falls in [start, end). Boundaries are assumed
 *  to land on code-point edges (they're byte offsets from a UTF-8-aware server). O(log n + hits). */
function markRange(byteStart: number[], n: number, start: number, end: number, fn: (i: number) => void): void {
  if (end <= start) return;
  for (let i = lowerBound(byteStart, n, start); i < n && byteStart[i] < end; i++) fn(i);
}

function renderVisualRow(
  element: number,
  line: LogicalLineRender,
  row: WrappedRow,
  rowIndex: number,
  isLastRow: boolean,
  cursorByte: number | null,
  sel: LineSelection | null,
  cursorClass: string,
  bracketBytes: number[],
  blame: string | null,
  diffView: boolean,
): HTMLElement {
  const rowEl = document.createElement("div");
  rowEl.className = "row";
  // Hit-testing data for mouse selection: which element the row belongs to, its logical line, and
  // the row's starting byte offset. The element is part of it because a logical line names a line
  // only *within* an element — two files' hunks both have a line 10 — so a click that resolved to a
  // line alone landed in whichever element happened to hold the cursor.
  rowEl.dataset.element = String(element);
  rowEl.dataset.line = String(line.logical_line);
  rowEl.dataset.byte = String(row.byte_offset);
  // Line-background tint is only shown while the inline diff view is on; the gutter change-bar is
  // always on (matching the terminal / the protocol's intent). A staged line gets the dimmer
  // variant of its kind tint (via the extra "staged" class).
  const change = line.change;
  const stage = changeStage(change);
  const marker = changeMarker(change);
  const conflict = changeConflict(change);
  const patchSide = changePatchSide(change);
  if (diffView && marker === "added") rowEl.classList.add("added-bg");
  else if (diffView && marker === "modified") rowEl.classList.add("modified-bg");
  // A patch buffer *is* a diff, so its staged/unstaged split is ungated — there is no view
  // to toggle it behind (the same reasoning as the patch tints themselves).
  if ((diffView || patchSide) && stage === "staged") rowEl.classList.add("staged");
  // A merge conflict's side tints, which unlike the diff ones are *not* gated on the diff view:
  // the sides are how the file is read at all. They never collide with the diff tints — the server
  // masks the diff out of the blocks. The marker lines get a class too (their text colour) but no
  // tint.
  if (conflict) rowEl.classList.add("conflict", `conflict-${conflict}`);
  // A generated patch's own sides. Ungated for the same reason the conflict tints are: the buffer
  // *is* a diff, so there's no view to toggle it behind. Shares the diff view's colours — the
  // mechanisms differ, what you look at doesn't.
  if (patchSide) rowEl.classList.add(`patch-${patchSide}`);
  // Current-line highlight (Vim's `cursorline`). `cursorByte` is non-null on exactly the cursor's
  // logical line, so every visual row of that line (under soft wrap) gets tinted as a whole. The CSS
  // rule is ordered after the diff tints so it wins on the cursor's changed line; the gutter
  // change-bar still marks it. Selection/search/cursor backgrounds sit on inner spans, over the tint.
  if (cursorByte !== null) rowEl.classList.add("cursor-line");

  rowEl.appendChild(
    gutter(marker, diffView, stage, conflict, patchSide),
  );

  const content = document.createElement("span");
  content.className = "content";

  if (rowIndex > 0) {
    const marker = document.createElement("span");
    marker.className = "cont-marker";
    marker.textContent = CONTINUATION_MARKER + " ".repeat(row.continuation_indent);
    content.appendChild(marker);
  }

  // The buffer text (excluding the continuation marker) goes in its own span so mouse hit-testing
  // can measure its left edge and read its text to map a click x → byte column.
  const textEl = document.createElement("span");
  textEl.className = "row-text";

  const rowText = row.segments.map((s) => s.text).join("");
  const { cps, byteStart, byteLen } = decodeRow(rowText);
  const n = cps.length;

  const hl: (string | null)[] = new Array(n).fill(null);
  const diag: (DiagnosticSeverity | null)[] = new Array(n).fill(null);
  const search: boolean[] = new Array(n).fill(false);
  const emph: boolean[] = new Array(n).fill(false);
  const sneak: boolean[] = new Array(n).fill(false);
  const chip: boolean[] = new Array(n).fill(false);
  const sneakLabel: (string | null)[] = new Array(n).fill(null);
  const selected: boolean[] = new Array(n).fill(false);
  const cursor: boolean[] = new Array(n).fill(false);
  const bracket: boolean[] = new Array(n).fill(false);

  // Match-bracket highlight: bracketBytes are line-local byte offsets of the paired brackets.
  for (const b of bracketBytes) {
    const local = b - row.byte_offset;
    if (local >= 0 && local < byteLen) {
      const idx = cpAtByte(byteStart, n, local);
      if (idx >= 0) bracket[idx] = true;
    }
  }

  // Syntax: highlights are byte offsets within each segment; segments concatenate to form the row.
  // Skipped entirely on a conflict marker line: whatever the grammar made of `<<<<<<< HEAD` is
  // noise, and leaving the cells classless lets them take the row's marker colour while selection,
  // search and cursor spans keep their own (the terminal client's rule, expressed structurally).
  let segBase = 0;
  if (changeConflict(line.change) !== "marker") {
    for (const seg of row.segments) {
      for (const h of seg.highlights) {
        const cls = highlightClass(h.kind);
        if (cls) markRange(byteStart, n, segBase + h.start, segBase + h.end, (i) => (hl[i] = cls));
      }
      segBase += utf8ByteLen(seg.text);
    }
  }

  // Diagnostics & search: byte offsets within the logical line → row-local via row.byte_offset.
  // Zero-width diagnostics (rust-analyzer's "expected …" points, start == end) widen to one cell so
  // the squiggle is visible — matching the core's cursor-hit widening for Space j.
  for (const d of line.diagnostics ?? []) {
    const start = d.start - row.byte_offset;
    const end = Math.max(d.end, d.start + 1) - row.byte_offset;
    markRange(byteStart, n, start, end, (i) => {
      if (diag[i] === null || SEVERITY_RANK[d.severity] > SEVERITY_RANK[diag[i]!]) diag[i] = d.severity;
    });
  }
  for (const m of line.search_matches ?? []) {
    markRange(byteStart, n, m.start - row.byte_offset, m.end - row.byte_offset, (i) => (search[i] = true));
  }
  // Intra-line diff emphasis (diff view only; the server omits it otherwise).
  for (const r of changeEmphasis(line.change)) {
    markRange(byteStart, n, r.start - row.byte_offset, r.end - row.byte_offset, (i) => (emph[i] = true));
  }
  // Sneak word-jump targets: tint each candidate word, and put its label on the first cell — but
  // only when the word's start actually falls in this row (so a word wrapped from a previous row
  // keeps the tint without a stray label).
  for (const t of line.sneak_targets ?? []) {
    const localStart = t.start - row.byte_offset;
    markRange(byteStart, n, localStart, t.end - row.byte_offset, (i) => (sneak[i] = true));
    markRange(byteStart, n, localStart, t.prefix_end - row.byte_offset, (i) => (chip[i] = true));
    if (t.label) {
      const idx = cpAtByte(byteStart, n, localStart);
      if (idx >= 0) sneakLabel[idx] = t.label;
    }
  }

  // Selection: inclusive line-local range mapped to this row.
  let selTrailing = false;
  // Selected whitespace gets a muted indicator glyph (terminal parity): `→` for tabs, `·` for
  // trailing spaces. Per code-point: "tab" | "dot" | null.
  const wsGlyph: (null | "tab" | "dot")[] = new Array(n).fill(null);
  if (sel) {
    const localStart = sel.start - row.byte_offset;
    const localEnd = sel.end - row.byte_offset; // inclusive
    for (let i = 0; i < n; i++) {
      if (byteStart[i] >= Math.max(0, localStart) && (sel.toEnd || byteStart[i] <= localEnd)) {
        selected[i] = true;
      }
    }
    selTrailing = isLastRow && (sel.toEnd || localEnd >= byteLen);
    // The row's trailing-whitespace run (code-point index it starts at) — only spaces from here
    // on are glyphed; tabs are glyphed wherever they're selected.
    let trailingWsStart = n;
    for (let k = n - 1; k >= 0; k--) {
      if (cps[k] === " " || cps[k] === "\t") trailingWsStart = k;
      else break;
    }
    for (let i = 0; i < n; i++) {
      if (!selected[i]) continue;
      if (cps[i] === "\t") wsGlyph[i] = "tab";
      else if (cps[i] === " " && i >= trailingWsStart) wsGlyph[i] = "dot";
    }
  }

  // Cursor: a single code point, when it falls inside this row's byte span.
  let cursorAtEnd = false;
  if (cursorByte !== null) {
    const local = cursorByte - row.byte_offset;
    if (local >= 0 && local < byteLen) {
      const idx = cpAtByte(byteStart, n, local);
      if (idx >= 0) cursor[idx] = true;
    } else if (local === byteLen && isLastRow) {
      cursorAtEnd = true;
    }
  }

  // Coalesce equal-styled code points into spans.
  const cellAt = (k: number): CellStyle => ({
    hl: hl[k],
    diag: diag[k],
    search: search[k],
    sel: selected[k],
    cursor: cursor[k],
    bracket: bracket[k],
    emph: emph[k],
    sneak: sneak[k],
    chip: chip[k],
    sneakLabel: sneakLabel[k],
  });
  // Consecutive emphasized runs collect into one `.diff-emph` wrapper span (the rounded band is
  // a single box that way); everything else appends to the row text directly.
  let emphWrap: HTMLElement | null = null;
  const sink = (node: Node, inEmph: boolean) => {
    if (inEmph) {
      if (!emphWrap) {
        emphWrap = document.createElement("span");
        emphWrap.className = "diff-emph";
        textEl.appendChild(emphWrap);
      }
      emphWrap.appendChild(node);
    } else {
      emphWrap = null;
      textEl.appendChild(node);
    }
  };
  let i = 0;
  while (i < n) {
    // A selected tab keeps its literal `\t` (so CSS `tab-size` preserves its width) in its own
    // span, which overlays the `→` glyph via `.ws-tab::before`.
    if (wsGlyph[i] === "tab") {
      sink(wsSpan("\t", cellAt(i), "tab", cursorClass), emph[i]);
      i++;
      continue;
    }
    const style = cellAt(i);
    const g = wsGlyph[i];
    let j = i + 1;
    while (j < n && wsGlyph[j] === g && g !== "tab" && sameStyle(style, cellAt(j))) j++;
    if (g === "dot") {
      // Trailing spaces → `·`, width-neutral, in NORD3 over the selection blue.
      sink(wsSpan("·".repeat(j - i), style, "dot", cursorClass), style.emph);
    } else {
      sink(makeSpan(cps.slice(i, j).join(""), style, cursorClass), style.emph);
    }
    i = j;
  }

  // A diagnostic clamped to the line end (e.g. "expected ;") has no real char to underline; mark
  // the virtual EOL cell — where the newline glyph sits — on the line's last row.
  let eolDiag: DiagnosticSeverity | null = null;
  if (isLastRow) {
    for (const d of line.diagnostics ?? []) {
      if (d.start - row.byte_offset >= byteLen) {
        if (eolDiag === null || SEVERITY_RANK[d.severity] > SEVERITY_RANK[eolDiag]) eolDiag = d.severity;
      }
    }
  }

  if (cursorAtEnd || selTrailing || eolDiag) {
    // The consumed newline reads as `↵` when selected (terminal parity). A cursor parked on it
    // keeps the glyph and renders the block over it (the `.cursor` rule inverts the `↵` to NORD0),
    // matching the terminal — rather than blanking it out. An end-of-line diagnostic underlines
    // this cell even with no cursor/selection present.
    const style = {
      hl: null,
      diag: eolDiag,
      search: false,
      sel: selTrailing,
      cursor: cursorAtEnd,
      bracket: false,
      emph: false,
      sneak: false,
      chip: false,
      sneakLabel: null,
    };
    textEl.appendChild(
      selTrailing ? wsSpan("↵", style, "nl", cursorClass) : makeSpan(" ", style, cursorClass),
    );
  }

  content.appendChild(textEl);

  // End-of-line git blame (cursor line, Normal mode), rendered dim/italic after the text.
  if (blame) {
    const b = document.createElement("span");
    b.className = "blame-eol";
    b.textContent = `    ${blame}`;
    content.appendChild(b);
  }

  rowEl.appendChild(content);
  return rowEl;
}

/** The change-bar gutter: a native colored left border for added/modified (always shown). A pure
 *  deletion shows a triangle between the lines — but only when the diff view is off; with it on, the
 *  removed lines render as phantom rows above, so no marker is needed on the anchor line. */
function gutter(
  marker: "added" | "modified" | "deleted" | null,
  diffView: boolean,
  stage: DiffStage,
  conflict: ConflictLine | null = null,
  patch: PatchLine | null = null,
): HTMLElement {
  const g = document.createElement("span");
  g.className = "gutter";
  // One unbroken bar down the whole block: the gutter says "conflict here", the row tints say
  // which side each line is on.
  if (conflict) {
    g.classList.add("conflict");
    return g;
  }
  // A generated patch marks its own sides. Dropping the `+`/`-` columns left the background tint as
  // the only signal of which side a line is; the bar carries it too, dimmed once staged.
  if (patch) {
    g.classList.add(patch === "added" ? "added" : "patch-removed");
    if (stage === "staged") g.classList.add("staged");
    return g;
  }
  if (marker === "added" || marker === "modified") g.classList.add(marker);
  else if (marker === "deleted" && !diffView) g.classList.add("deleted");
  // A staged change dims the bar to the muted variant of its kind colour.
  if (marker && stage === "staged") g.classList.add("staged");
  return g;
}

/** The spans a row's inline nodes draw, appended to `into` left to right.
 *
 *  Shared by a chrome row and a box's title: the same vocabulary, in the same roles, drawn in two
 *  different places on the row. */
function appendInline(into: HTMLElement, nodes: ViewNode[]): void {
  for (const w of nodes) {
    if (w.node === "space") {
      const span = document.createElement("span");
      span.textContent = " ".repeat(w.cols);
      into.appendChild(span);
    } else if (w.node === "fill") {
      // Nothing to append: a rule is a flex-grown `::after` border keyed off the row's `.rule`
      // class (see theme.css), not a repeated glyph. The element says "absorb the slack"; the DOM
      // spells that with flex, the terminal with a repeated `─`, the GUI with a hairline rect.
    } else if (w.node === "text") {
      // Runs split at the server's span boundaries; gaps between spans take the muted default,
      // since chrome is never plain body text.
      const { cps, byteStart } = decodeRow(w.text);
      const n = cps.length;
      const cls: (string | null)[] = new Array(n).fill(null);
      for (const h of w.highlights ?? []) {
        const c = highlightClass(h.kind);
        markRange(byteStart, n, h.start, h.end, (i) => (cls[i] = c));
      }
      let i = 0;
      while (i < n) {
        let j = i + 1;
        while (j < n && cls[j] === cls[i]) j++;
        const span = document.createElement("span");
        if (cls[i]) span.className = cls[i] as string;
        span.textContent = cps.slice(i, j).join("");
        into.appendChild(span);
        i = j;
      }
    }
  }
}

/** One row of a box's own border or padding.
 *
 *  No tree node stands for it, so unlike `chromeRow` there is nothing to read text out of — except
 *  on the border a *named* box opens with, which carries the name on the rule itself. `join` is
 *  the same alphabet the terminal spells with `┌`/`├`/`└`; here it is a CSS class, as the file
 *  rail already was, and the name is a span that masks the rule where it sits (`.box-edge .title`)
 *  — the rule being one gradient across the row, splitting it in two would mean teaching the
 *  `--rule-from`/`--rule-to` rails to count characters. */
function edgeRow(
  side: "top" | "bottom",
  join: RailJoin,
  band: Band,
  title: ViewNode[],
  holdsCursor: boolean,
): HTMLElement {
  const rowEl = document.createElement("div");
  rowEl.className = `row box-edge ${side} ${join}`;
  // A box's own cells take its band, the same as any other row of it.
  if (band === "chrome") rowEl.classList.add("patch-chrome");
  // A **collapsed** element's title row is the whole of that element, so the cursor in it has no
  // row of its own to be drawn on. The row takes the cursorline instead, which is what marks the
  // cursor's row everywhere else.
  if (holdsCursor) rowEl.classList.add("cursor-line");
  const g = document.createElement("span");
  g.className = "gutter";
  rowEl.appendChild(g);
  const content = document.createElement("span");
  content.className = "content";
  if (title.length) {
    const named = document.createElement("span");
    named.className = "title";
    appendInline(named, title.flatMap(inlineOf));
    content.appendChild(named);
  }
  rowEl.appendChild(content);
  return rowEl;
}

/** A generated patch's file or hunk separator, the patch's summary caption, or the blank space
 *  between them.
 *
 *  Chrome, not content: it holds no cursor position (that's the whole reason it's a virtual row
 *  rather than a buffer line) and carries no gutter change-bar, since it belongs to no line of
 *  either side. The file separator's trailing rule is drawn in CSS, so it fills whatever width is
 *  left. */
function chromeRow(v: ViewNode, band: Band): HTMLElement {
  const rowEl = document.createElement("div");
  rowEl.className = "row";
  // A row of presentation with no band paints none — since one vocabulary covers both axes, an
  // inline element may stand on its own. It still draws; it just sits on the editor's background.
  // The band was a `chrome` variant carrying a `ChromeKind` no shell branched on and a `RailJoin`
  // that is derived from the tree now; what it delivered was this shade, so this is what says it.
  if (band === "chrome") rowEl.classList.add("patch-chrome");
  const g = document.createElement("span");
  g.className = "gutter";
  rowEl.appendChild(g);
  const content = document.createElement("span");
  content.className = "content";
  appendInline(content, inlineOf(v));
  rowEl.appendChild(content);
  return rowEl;
}

function phantomRow(text: string, stage: DiffStage, emphasis: EmphasisRange[]): HTMLElement {
  const rowEl = document.createElement("div");
  rowEl.className = "row deleted-phantom";
  const g = document.createElement("span");
  g.className = "gutter phantom"; // solid bar marking the removed content (red, cyan when staged)
  if (stage === "staged") {
    rowEl.classList.add("staged");
    g.classList.add("staged");
  }
  rowEl.appendChild(g);
  const content = document.createElement("span");
  content.className = "content";
  if (emphasis.length === 0) {
    content.textContent = text;
  } else {
    // Split the text at the emphasis boundaries; the changed sub-ranges get the stronger fill.
    // Ranges are byte offsets from the server — decode to code points like the buffer rows.
    const { cps, byteStart } = decodeRow(text);
    const n = cps.length;
    const inEmph: boolean[] = new Array(n).fill(false);
    for (const r of emphasis) markRange(byteStart, n, r.start, r.end, (i) => (inEmph[i] = true));
    let i = 0;
    while (i < n) {
      let j = i + 1;
      while (j < n && inEmph[j] === inEmph[i]) j++;
      const run = cps.slice(i, j).join("");
      if (inEmph[i]) {
        const span = document.createElement("span");
        span.className = "diff-emph";
        span.textContent = run;
        content.appendChild(span);
      } else {
        content.appendChild(document.createTextNode(run));
      }
      i = j;
    }
  }
  rowEl.appendChild(content);
  return rowEl;
}

function lessEq(a: LogicalPosition, b: LogicalPosition): boolean {
  return a.line < b.line || (a.line === b.line && a.col <= b.col);
}

export interface RenderOpts {
  window: BufferWindow;
  cursor: CursorState;
  insertMode: boolean;
  /** Waiting for the next keystroke of a chord (find target, leader, surround, partial count) —
   *  shown as an underscore cursor, matching the terminal. Takes precedence over insert/normal. */
  awaitingKey: boolean;
  /** Full content width in px for native horizontal scroll (no-wrap), or 0 to fit the container. */
  contentWidthPx: number;
  /** Full-document scroll height in px (`totalRows` × row height, plus padding) — sizes the
   *  scroller. */
  spacerHeightPx: number;
  /** Where row 0 of the view sits inside the scroller — the padding above it. */
  contentTopPx: number;
  /** One row's height in px: what the rows nothing is loaded at are sized by. */
  rowHeightPx: number;
  /** What the shell measured of elements it laid out itself — see `Measured`. */
  measured: Measured;
  /** End-of-line git blame for the cursor line, or null. */
  blame: string | null;
  /** Inline diff view on — gates the line-background tint (the gutter change-bar is always on). */
  diffView: boolean;
  /** Which editor element holds the live cursor. A logical line number names a line only within
   *  its element, so everything decided against the cursor's line is narrowed to this one. */
  focusedElement: number;
}

/** Repaint the whole buffer area from the current window + cursor. `container` is the shell's
 *  buffer surface — a shadow root in the browser (see `Shell.bufferSurface`), a plain element in
 *  tests; both satisfy the `:scope > .buffer-spacer` lookup and `replaceChildren` used below. */
export function renderBuffer(container: HTMLElement | ShadowRoot, opts: RenderOpts): void {
  const {
    window,
    cursor,
    insertMode,
    awaitingKey,
    contentWidthPx,
    spacerHeightPx,
    contentTopPx,
    rowHeightPx,
    measured,
    blame,
    diffView,
    focusedElement,
  } = opts;
  // The cursor's appearance is decided once here: an underscore while waiting for the next key of a
  // chord (overriding mode), else a bar in Insert, else a block. `makeSpan` just appends this class.
  const cursorClass = awaitingKey ? "cursor pending" : insertMode ? "cursor insert" : "cursor";
  const isPoint =
    cursor.position.line === cursor.anchor.line && cursor.position.col === cursor.anchor.col;
  const min = lessEq(cursor.anchor, cursor.position) ? cursor.anchor : cursor.position;
  const max = lessEq(cursor.anchor, cursor.position) ? cursor.position : cursor.anchor;
  const bracketPair = cursor.match_bracket ?? null;

  const frag = document.createDocumentFragment();
  // One walk of the shared row layout: chrome, phantom and text rows in the order every shell must
  // draw them, each at its absolute row. `paintedRows` mirrors `grid::painted_rows`, which is the
  // tested specification — the three painters each used to walk the tree themselves and disagreed
  // about where rows landed. Rows nothing is loaded at — an element the viewport has not reached,
  // a fetch still in flight — are a gap the same height, so everything below keeps its row.
  // Offsets are in the measured resolution; a row is `units_per_row` of them.
  const unit = measured.units_per_row;
  // Cells the enclosing boxes claimed, applied to whatever the row turns out to be. `boxed`
  // is what puts the band behind those cells and takes the pre-frame rail out of the gutter:
  // inside a box the rail is the box's border, and drawing both is two lines down one file.
  const insetBy = (place: ElementPlacement | PaintedRow, el: HTMLElement): HTMLElement => {
    if (!place.left && !place.right) return el;
    el.classList.add("boxed");
    el.style.setProperty("--inset-left", `${place.left}ch`);
    el.style.setProperty("--inset-right", `${place.right}ch`);
    // A rail per side the box actually draws — the padding beside it gets no line.
    if (place.rails.left) el.classList.add("rail-left");
    if (place.rails.right) el.classList.add("rail-right");
    return el;
  };
  let next = 0;
  const gapTo = (row: number): void => {
    if (row <= next) return;
    const gap = document.createElement("div");
    gap.className = "row-gap";
    gap.style.height = `${((row - next) / unit) * rowHeightPx}px`;
    frag.appendChild(gap);
  };
  // Prose contributes no rows to the walk below — the window carries its parse, not its lines — so
  // it is placed from its own origin and emitted in row order between the rows around it. At the
  // *origin*, not at whatever row happened to be nearest: with the top of a long reply scrolled
  // away there is no row at its start, and hanging the block off a surviving row slid it down the
  // screen by however much was missing.
  const placements = elementOrigins(window.root, measured);
  const pending = proseOf(window.root)
    .map((node) => ({ node, place: placements[node.element] }))
    .filter((p) => p.place !== undefined)
    .sort((a, b) => a.place.at - b.place.at);
  let emitted = 0;
  const flushProse = (upTo: number): void => {
    while (emitted < pending.length && pending[emitted].place.at <= upTo) {
      const { node, place } = pending[emitted++];
      gapTo(place.at);
      const box = document.createElement("div");
      box.className = "md-reply-box";
      // The shell finds this again to measure what the browser made of it: proportional type has
      // no height until it is laid out, so the grid's idea of how tall this reply is comes back
      // on the pass after this one.
      box.dataset.element = String(node.element);
      renderReply(box, node.blocks);
      frag.appendChild(insetBy(place, box));
      // The measured height if the shell has one; a single row until then, which is wrong and is
      // corrected the moment the measure lands.
      next = place.at + (measured.elements[node.element]?.end ?? unit);
    }
  };
  for (const item of paintedRows(window.root, measured)) {
    flushProse(item.at);
    gapTo(item.at);
    next = item.at + unit;
    const inset = (el: HTMLElement): HTMLElement => insetBy(item, el);
    if (item.kind === "chrome") {
      frag.appendChild(inset(chromeRow(item.node, item.band)));
      continue;
    }
    if (item.kind === "baseline") {
      const v = item.row;
      frag.appendChild(inset(phantomRow(v.text, v.stage ?? "unstaged", v.emphasis ?? [])));
      continue;
    }
    if (item.kind === "edge") {
      // A box is named on the border it opens with; the closing one carries nothing.
      const title =
        item.side === "top" && item.owner.node === "column" ? (item.owner.title ?? []) : [];
      frag.appendChild(
        inset(
          edgeRow(
            item.side,
            item.join,
            item.band,
            title,
            holdsCollapsed(item.owner, focusedElement),
          ),
        ),
      );
      continue;
    }
    const { line, row, rowIndex, element } = item;
    const L = line.logical_line;
    // The pair, not the number: two files' hunks both have a line 10, so deciding the cursor line
    // by the number alone paints a second cursor in the other file — two, moving in sync.
    const onCursorElement = element === focusedElement;
    const cursorByte = onCursorElement && cursor.position.line === L ? cursor.position.col : null;
    const bracketBytes = bracketPair
      ? bracketPair.filter((p) => p.line === L).map((p) => p.col)
      : [];

    // A point cursor is the 1-char selection of the char under it (Helix-style): under a block
    // (or pending-underscore) cursor it renders with the same selection styling — fill +
    // whitespace/newline glyphs — as inside a multi-char range (terminal parity). Insert's bar
    // cursor is a gap between chars, not a selection, so a point draws nothing there.
    let sel: LineSelection | null = null;
    if (onCursorElement && (!isPoint || !insertMode) && L >= min.line && L <= max.line) {
      sel = {
        start: L === min.line ? min.col : 0,
        end: L === max.line ? max.col : 0,
        toEnd: L < max.line,
      };
    }

    const isLast = rowIndex === line.visual_rows.length - 1;
    const blameLine = !insertMode && blame && onCursorElement && cursor.position.line === L;
    frag.appendChild(
      inset(
        renderVisualRow(
          element,
          line,
          row,
          rowIndex,
          isLast,
          cursorByte,
          sel,
          cursorClass,
          bracketBytes,
          blameLine && isLast ? blame : null,
          diffView,
        ),
      ),
    );
  }
  // Virtual scroll: a full-document-height spacer (so the native scrollbar reflects the whole
  // file), with the loaded window absolutely positioned at its visual-row offset. Both axes scroll
  // natively; `contentWidthPx` widens the content past the container so the widest line is reachable.
  const content = document.createElement("div");
  content.className = "buffer-content";
  content.style.top = `${contentTopPx}px`;
  const widthCss = contentWidthPx > 0 ? `max(100%, ${contentWidthPx}px)` : "";
  content.style.width = widthCss;
  content.appendChild(frag);
  // Reuse a persistent spacer and swap only the content layer, so other spacer children survive a
  // re-render — namely the sticky hover popover, which lives in the spacer's coordinate space so the
  // browser glues it to its line and clamps it to the editor edges on scroll (no JS repositioning).
  let spacer = container.querySelector(":scope > .buffer-spacer") as HTMLElement | null;
  if (!spacer) {
    spacer = document.createElement("div");
    spacer.className = "buffer-spacer";
    container.replaceChildren(spacer);
  }
  spacer.style.height = `${spacerHeightPx}px`;
  spacer.style.width = widthCss;
  const oldContent = spacer.querySelector(":scope > .buffer-content");
  if (oldContent) oldContent.replaceWith(content);
  else spacer.insertBefore(content, spacer.firstChild);
}
