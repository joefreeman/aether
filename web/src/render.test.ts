//! Painter tests for the browser shell.
//!
//! The web client had no tests at all, and it is the shell that lags: the Rust core is shared
//! through wasm, but `render.ts` and the type mirror in `protocol.ts` are hand-written, so a
//! protocol change type-checks clean while the DOM quietly reads `undefined`.
//!
//! What these cover is the **row layout** — which chrome, phantom and text rows land in which
//! order — because that is where every cross-shell bug has been: a patch whose hunk starts partway
//! down its file, and two files in one view whose line numbers collide. Both were blank screens in
//! the terminal and the GUI; nothing here would have said so.
//!
//! Deliberately not pixels. Geometry, not rendering.

import { describe, expect, it } from "vitest";
import { renderBuffer } from "./render";
import { totalRows } from "./protocol";
import type { BufferWindow, CursorState, LogicalLineRender, Measured, ViewNode } from "./protocol";

const line = (n: number, text: string): LogicalLineRender => ({
  logical_line: n,
  visual_rows: [{ byte_offset: 0, continuation_indent: 0, segments: [{ text, highlights: [] }] }],
});

const chrome = (text: string): ViewNode => ({
  node: "chrome",
  kind: "file_header",
  rail: "opens",
  children: [{ node: "text", text, highlights: [] }],
});

const editor = (element: number, first: number, lines: LogicalLineRender[]): ViewNode => ({
  node: "editor",
  element,
  buffer: element + 1,
  rows: lines.length,
  first_row: 0,
  first_buffer_line: first,
  lines,
});

const windowOf = (root: ViewNode): BufferWindow => ({
  max_line_width: 0,
  root,
});

const cursor: CursorState = {
  position: { line: 0, col: 0 },
  anchor: { line: 0, col: 0 },
};

/** Paint into a detached element and read back one string per row, tagged by kind. */
function painted(window: BufferWindow, opts: { cursor?: CursorState; focused?: number } = {}): string[] {
  const container = document.createElement("div");
  renderBuffer(container, {
    window,
    cursor: opts.cursor ?? cursor,
    insertMode: false,
    awaitingKey: false,
    contentWidthPx: 0,
    spacerHeightPx: 0,
    contentTopPx: 0,
    rowHeightPx: 0,
    measured: {},
    blame: null,
    diffView: false,
    focusedElement: opts.focused ?? 0,
  });
  return [...container.querySelectorAll(".row")].map((el) => {
    const kind = el.classList.contains("patch-chrome")
      ? "chrome"
      : el.classList.contains("deleted-phantom")
        ? "phantom"
        : "text";
    return `${kind} ${el.textContent?.trim()}`;
  });
}

describe("the buffer painter", () => {
  /// The shape that blanked the terminal and the GUI: an element windowing a *file*, so its lines
  /// start at 16 while the view's own first line is 0. Anything that indexes by
  /// `logical_line - first_view_line` reads off the end of a 3-item list and draws nothing.
  it("paints a hunk whose lines start partway down its file", () => {
    const w = windowOf(
      {
        node: "stack",
        children: [chrome("a.rs"), editor(0, 16, [line(16, "fn f17"), line(17, "fn f18")])],
      },
    );
    expect(painted(w)).toEqual(["chrome a.rs", "text fn f17", "text fn f18"]);
  });

  /// Two files in one view. The second's lines are numbered *below* the first's, so any lookup by
  /// line number alone finds the wrong row — or, dropping the element entirely, no row at all.
  it("paints both files when their line numbers collide", () => {
    const w = windowOf(
      {
        node: "stack",
        children: [
          chrome("a.rs"),
          editor(0, 10, [line(10, "a10"), line(11, "a11")]),
          chrome("b.rs"),
          editor(1, 10, [line(10, "b10"), line(11, "b11")]),
        ],
      },
    );
    expect(painted(w)).toEqual([
      "chrome a.rs",
      "text a10",
      "text a11",
      "chrome b.rs",
      "text b10",
      "text b11",
    ]);
  });

  /// A slice loaded partway into its element sits at its row, and the rows above it — loaded by
  /// nobody — are a gap of the same height, so everything below keeps its place in the scroller.
  /// Mirrors `a_slice_sits_at_its_row_within_its_element` on the Rust side.
  it("leaves a gap for the rows of an element nothing is loaded at", () => {
    const w = windowOf({
      node: "stack",
      children: [
        chrome("a.rs"),
        {
          node: "editor",
          element: 0,
          buffer: 1,
          rows: 5,
          first_row: 3,
          first_buffer_line: 19,
          lines: [line(19, "f20"), line(20, "f21")],
        },
      ],
    });
    const container = document.createElement("div");
    renderBuffer(container, {
      window: w,
      cursor,
      insertMode: false,
      awaitingKey: false,
      contentWidthPx: 0,
      spacerHeightPx: 0,
      contentTopPx: 0,
      rowHeightPx: 10,
      measured: {},
      blame: null,
      diffView: false,
      focusedElement: 0,
    });
    const content = container.querySelector(".buffer-content")!;
    const kinds = [...content.children].map((el) =>
      el.classList.contains("row-gap") ? `gap ${(el as HTMLElement).style.height}` : el.textContent?.trim(),
    );
    // The heading on row 0; rows 1..3 are the element's first three, unloaded; the slice from row 4.
    expect(kinds).toEqual(["a.rs", "gap 30px", "f20", "f21"]);
  });

  /// An element the client lays out paints each line on the row the shell measured it as starting
  /// on — a tall line pushes the next down — and, unmeasured, one line per row. Mirrors the Rust
  /// `a_measured_client_element_is_as_tall_as_the_shell_says`.
  it("paints a client-laid-out element's lines where the shell measured them", () => {
    const prose: ViewNode = {
      node: "editor",
      element: 0,
      buffer: 1,
      rows: 10,
      first_row: 2,
      first_buffer_line: 2,
      lines: [line(2, "two"), line(3, "three"), line(4, "four")],
      laid_out_by: "client",
    };
    const w = windowOf({ node: "stack", children: [chrome("a.md"), prose] });
    const paint = (measured: Measured) => {
      const container = document.createElement("div");
      renderBuffer(container, {
        window: w,
        cursor,
        insertMode: false,
        awaitingKey: false,
        contentWidthPx: 0,
        spacerHeightPx: 0,
        contentTopPx: 0,
        rowHeightPx: 10,
        measured,
        blame: null,
        diffView: false,
        focusedElement: 0,
      });
      const content = container.querySelector(".buffer-content")!;
      return [...content.children].map((el) =>
        el.classList.contains("row-gap") ? `gap ${(el as HTMLElement).style.height}` : el.textContent?.trim(),
      );
    };
    // Unmeasured: one row per line, two rows into the element (row 1 is the heading).
    expect(paint({})).toEqual(["a.md", "gap 20px", "two", "three", "four"]);
    // Measured: line 3 laid out three rows tall, so line 4 moves down by two.
    expect(paint({ 0: { first_row: 2, starts: [2, 3, 6], end: 8 } })).toEqual([
      "a.md",
      "gap 20px",
      "two",
      "three",
      "gap 20px",
      "four",
    ]);
    expect(totalRows(w.root, { 0: { first_row: 2, starts: [2, 3, 6], end: 8 } })).toBe(1 + 2 + 6 + 5);
  });

  /// The closing rule hangs off the *last rendered row*, not off a line number. Asking
  /// `logical_line + 1 === view_line_count` compares a buffer line to a view line: here the last
  /// line is numbered 11 in a 4-line view, so that test never fires and the rule vanishes.
  it("draws the closing chrome after the final row, whatever that row's line number is", () => {
    const w = windowOf(
      {
        node: "stack",
        children: [editor(0, 10, [line(10, "x"), line(11, "y")]), chrome("closing")],
      },
    );
    expect(painted(w)).toEqual(["text x", "text y", "chrome closing"]);
  });

  /// Phantom baseline rows occupy a row each and sit above the line that replaced them — the
  /// inline diff view's shape, and the one a patch's removed lines now use too.
  it("draws phantom baseline rows above their line", () => {
    const l = line(4, "after");
    l.baseline_above = [{ text: "before", stage: "unstaged", emphasis: [] }];
    const w = windowOf(editor(0, 4, [l]));
    expect(painted(w)).toEqual(["phantom before", "text after"]);
  });

  /// Reported from the terminal as two cursors moving in sync, and the browser had it too: the
  /// cursor line was decided by `logical_line === cursor.line` alone, so in a patch where two files
  /// both have a line 10 it painted on both.
  it("marks the cursor line in the focused element only", () => {
    const w = windowOf(
      {
        node: "stack",
        children: [
          editor(0, 10, [line(10, "alpha ten")]),
          editor(1, 10, [line(10, "beta ten")]),
        ],
      },
    );
    const at10: CursorState = {
      position: { line: 10, col: 0 },
      anchor: { line: 10, col: 0 },
    };
    const container = document.createElement("div");
    renderBuffer(container, {
      window: w,
      cursor: at10,
      insertMode: false,
      awaitingKey: false,
      contentWidthPx: 0,
      spacerHeightPx: 0,
      contentTopPx: 0,
      rowHeightPx: 0,
      measured: {},
      blame: null,
      diffView: false,
      focusedElement: 1,
    });
    const marked = [...container.querySelectorAll(".row.cursor-line")].map((el) =>
      el.textContent?.trim(),
    );
    expect(marked).toEqual(["beta ten"]);
  });

  /// An ordinary buffer is a single editor with no chrome — the overwhelmingly common case, and
  /// the one every patch-shaped test above must not have broken.
  /// Every text row says which element it belongs to, because that is what a click needs.
  ///
  /// The shell hit-tests a click to the row under the pointer and reads its line from `data-line` —
  /// but a logical line names a line only *within* an element, so a click resolved to a line alone
  /// lands in whichever element holds the cursor. In the browser that meant clicking anywhere
  /// outside the focused editor did nothing at all: the press set a cursor the server bounded
  /// straight back into the element that still had focus.
  it("tags each row with the element it belongs to, for hit-testing", () => {
    const w = windowOf(
      {
        node: "stack",
        children: [
          chrome("a.rs"),
          editor(0, 16, [line(16, "from a")]),
          chrome("b.rs"),
          editor(1, 16, [line(16, "from b")]),
        ],
      },
    );
    const container = document.createElement("div");
    renderBuffer(container, {
      window: w,
      cursor,
      insertMode: false,
      awaitingKey: false,
      contentWidthPx: 0,
      spacerHeightPx: 0,
      contentTopPx: 0,
      rowHeightPx: 0,
      measured: {},
      blame: null,
      diffView: false,
      focusedElement: 0,
    });
    const rows = [...container.querySelectorAll<HTMLElement>(".row")].filter(
      (el) => el.dataset.line !== undefined,
    );
    expect(rows.map((el) => [el.dataset.element, el.dataset.line])).toEqual([
      ["0", "16"],
      ["1", "16"],
    ]);
  });

  it("paints a plain buffer as plain rows", () => {
    const w = windowOf(editor(0, 0, [line(0, "one"), line(1, "two")]));
    expect(painted(w)).toEqual(["text one", "text two"]);
  });
});
