//! DOM rendering of a viewport Window. The server sends a fully-resolved render model (segments
//! with byte-offset highlights, plus per-line diagnostics / search / diff data); we paint it as
//! rows of <span>s. Each row is styled at code-point granularity then coalesced into spans, so
//! syntax colour, diagnostic underline, search tint, selection, and the cursor block compose
//! cleanly even when they overlap. (docs/web-client.md §2.)

import { decodeRow, utf8ByteLen } from "./text";
import type {
  BufferWindow,
  CursorState,
  DiagnosticSeverity,
  DiffStage,
  LogicalLineRender,
  LogicalPosition,
  VisualRow,
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
};

function highlightClass(kind: string): string | null {
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
}

function sameStyle(a: CellStyle, b: CellStyle): boolean {
  return (
    a.hl === b.hl &&
    a.diag === b.diag &&
    a.search === b.search &&
    a.sel === b.sel &&
    a.cursor === b.cursor &&
    a.bracket === b.bracket
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
  const classes: string[] = [];
  if (style.hl) classes.push(style.hl);
  if (style.bracket) classes.push("match-bracket");
  if (style.diag) classes.push("diag-" + style.diag);
  if (style.search) classes.push("search-hit");
  if (style.sel) classes.push("sel");
  if (style.cursor) classes.push(cursorClass);
  if (classes.length === 0) return document.createTextNode(text);
  const span = document.createElement("span");
  span.className = classes.join(" ");
  span.textContent = text;
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
  line: LogicalLineRender,
  row: VisualRow,
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
  // Hit-testing data for mouse selection: the logical line and this row's starting byte offset.
  rowEl.dataset.line = String(line.logical_line);
  rowEl.dataset.byte = String(row.byte_offset);
  // Line-background tint is only shown while the inline diff view is on; the gutter change-bar is
  // always on (matching the terminal / the protocol's intent). A staged line gets the dimmer
  // variant of its kind tint (via the extra "staged" class).
  const stage = line.diff_stage ?? "unstaged";
  if (diffView && line.diff_marker === "added") rowEl.classList.add("added-bg");
  else if (diffView && line.diff_marker === "modified") rowEl.classList.add("modified-bg");
  if (diffView && stage === "staged") rowEl.classList.add("staged");
  // Current-line highlight (Vim's `cursorline`). `cursorByte` is non-null on exactly the cursor's
  // logical line, so every visual row of that line (under soft wrap) gets tinted as a whole. The CSS
  // rule is ordered after the diff tints so it wins on the cursor's changed line; the gutter
  // change-bar still marks it. Selection/search/cursor backgrounds sit on inner spans, over the tint.
  if (cursorByte !== null) rowEl.classList.add("cursor-line");

  rowEl.appendChild(gutter(line.diff_marker ?? null, diffView, stage));

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
  let segBase = 0;
  for (const seg of row.segments) {
    for (const h of seg.highlights) {
      const cls = highlightClass(h.kind);
      if (cls) markRange(byteStart, n, segBase + h.start, segBase + h.end, (i) => (hl[i] = cls));
    }
    segBase += utf8ByteLen(seg.text);
  }

  // Diagnostics & search: byte offsets within the logical line → row-local via row.byte_offset.
  for (const d of line.diagnostics ?? []) {
    markRange(byteStart, n, d.start - row.byte_offset, d.end - row.byte_offset, (i) => {
      if (diag[i] === null || SEVERITY_RANK[d.severity] > SEVERITY_RANK[diag[i]!]) diag[i] = d.severity;
    });
  }
  for (const m of line.search_matches ?? []) {
    markRange(byteStart, n, m.start - row.byte_offset, m.end - row.byte_offset, (i) => (search[i] = true));
  }

  // Selection: inclusive line-local range mapped to this row.
  let selTrailing = false;
  if (sel) {
    const localStart = sel.start - row.byte_offset;
    const localEnd = sel.end - row.byte_offset; // inclusive
    for (let i = 0; i < n; i++) {
      if (byteStart[i] >= Math.max(0, localStart) && (sel.toEnd || byteStart[i] <= localEnd)) {
        selected[i] = true;
      }
    }
    selTrailing = isLastRow && (sel.toEnd || localEnd >= byteLen);
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
  });
  let i = 0;
  while (i < n) {
    const style = cellAt(i);
    let j = i + 1;
    while (j < n && sameStyle(style, cellAt(j))) j++;
    textEl.appendChild(makeSpan(cps.slice(i, j).join(""), style, cursorClass));
    i = j;
  }

  if (cursorAtEnd || selTrailing) {
    textEl.appendChild(
      makeSpan(
        " ",
        { hl: null, diag: null, search: false, sel: selTrailing, cursor: cursorAtEnd, bracket: false },
        cursorClass,
      ),
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
): HTMLElement {
  const g = document.createElement("span");
  g.className = "gutter";
  if (marker === "added" || marker === "modified") g.classList.add(marker);
  else if (marker === "deleted" && !diffView) g.classList.add("deleted");
  // A staged change dims the bar to the muted variant of its kind colour.
  if (marker && stage === "staged") g.classList.add("staged");
  return g;
}

function phantomRow(text: string, stage: DiffStage): HTMLElement {
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
  content.textContent = text;
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
  /** Full-document scroll height in px (total_visual_rows × lineHeight) — sizes the scroller. */
  spacerHeightPx: number;
  /** Absolute top of the loaded window inside the scroller (first_visual_row × lineHeight). */
  contentTopPx: number;
  /** End-of-line git blame for the cursor line, or null. */
  blame: string | null;
  /** Inline diff view on — gates the line-background tint (the gutter change-bar is always on). */
  diffView: boolean;
}

/** Repaint the whole buffer area from the current window + cursor. */
export function renderBuffer(container: HTMLElement, opts: RenderOpts): void {
  const { window, cursor, insertMode, awaitingKey, contentWidthPx, spacerHeightPx, contentTopPx, blame, diffView } = opts;
  // The cursor's appearance is decided once here: an underscore while waiting for the next key of a
  // chord (overriding mode), else a bar in Insert, else a block. `makeSpan` just appends this class.
  const cursorClass = awaitingKey ? "cursor pending" : insertMode ? "cursor insert" : "cursor";
  const isPoint =
    cursor.position.line === cursor.anchor.line && cursor.position.col === cursor.anchor.col;
  const min = lessEq(cursor.anchor, cursor.position) ? cursor.anchor : cursor.position;
  const max = lessEq(cursor.anchor, cursor.position) ? cursor.position : cursor.anchor;
  const bracketPair = cursor.match_bracket ?? null;

  const frag = document.createDocumentFragment();
  for (const line of window.lines) {
    for (const v of line.virtual_rows_above ?? []) frag.appendChild(phantomRow(v.text, v.stage ?? "unstaged"));

    const L = line.logical_line;
    const cursorByte = cursor.position.line === L ? cursor.position.col : null;
    const bracketBytes = bracketPair
      ? bracketPair.filter((p) => p.line === L).map((p) => p.col)
      : [];

    let sel: LineSelection | null = null;
    if (!isPoint && L >= min.line && L <= max.line) {
      sel = {
        start: L === min.line ? min.col : 0,
        end: L === max.line ? max.col : 0,
        toEnd: L < max.line,
      };
    }

    const rows = line.visual_rows;
    const blameLine = !insertMode && blame && cursor.position.line === L;
    rows.forEach((row, idx) => {
      const isLast = idx === rows.length - 1;
      frag.appendChild(
        renderVisualRow(
          line,
          row,
          idx,
          isLast,
          cursorByte,
          sel,
          cursorClass,
          bracketBytes,
          blameLine && isLast ? blame : null,
          diffView,
        ),
      );
    });
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
  const spacer = document.createElement("div");
  spacer.className = "buffer-spacer";
  spacer.style.height = `${spacerHeightPx}px`;
  spacer.style.width = widthCss;
  spacer.appendChild(content);
  container.replaceChildren(spacer);
}
