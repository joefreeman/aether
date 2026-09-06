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

import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { renderBuffer } from "./render";
import { paintedRows, totalRows } from "./protocol";
import { WHOLE_ROWS } from "./protocol";
import type { BufferWindow, CursorState, LogicalLineRender, Measured, ViewNode } from "./protocol";

const line = (n: number, text: string): LogicalLineRender => ({
  logical_line: n,
  visual_rows: [{ byte_offset: 0, continuation_indent: 0, segments: [{ text, highlights: [] }] }],
});

const chrome = (text: string): ViewNode => ({
  node: "row",
  band: "chrome",
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
    measured: WHOLE_ROWS,
    blame: null,
    diffView: false,
    focusedElement: opts.focused ?? 0,
  });
  return [...container.querySelectorAll(".row")].map((el) => {
    // An edge row sits on the chrome band too, so it is asked about first.
    const kind = el.classList.contains("box-edge")
      ? "edge"
      : el.classList.contains("patch-chrome")
        ? "chrome"
        : el.classList.contains("deleted-phantom")
          ? "phantom"
          : "text";
    return `${kind} ${el.textContent?.trim()}`;
  });
}


// ---- the shared corpus -----------------------------------------------------------------------

/** The row layout both walks must agree on, as data.
 *
 *  `grid::painted_rows` is the specification and `paintedRows` here is a hand-written mirror of
 *  it — `render.ts` paints from this mirror, not from the wasm core. Each side was tested before
 *  this, and each against fixtures written in its own language, which is coverage of both and
 *  none at all of their *agreement*. `crates/aether-client/src/grid.rs` reads this same file. */
// Resolved from the vitest root (`web/`), not from `import.meta.url`: under happy-dom the module
// URL is an `http:` one and `fileURLToPath` refuses it.
const corpus = JSON.parse(
  readFileSync(
    resolve(process.cwd(), "../crates/aether-client/tests/fixtures/painted_rows.json"),
    "utf8",
  ),
) as {
  cases: {
    name: string;
    why: string;
    measured: Measured;
    total_rows: number;
    root: ViewNode;
    rows: unknown[];
  }[];
};

/** The literal text a node draws, left to right — `Element::text_content`'s mirror. */
function textContent(n: ViewNode): string {
  if (n.node === "text") return n.text;
  if (n.node === "column" || n.node === "row") return n.children.map(textContent).join("");
  return "";
}

/** One painted row reduced to what both walks can produce — the corpus's own shape.
 *
 *  `left`/`right` ride on every row, so a case that only exercises the vertical layout still pins
 *  that the boxes claimed nothing. */
function reduce(root: ViewNode, measured: Measured): unknown[] {
  return paintedRows(root, measured).map((r) => {
    const at = {
      at: r.at,
      left: r.left,
      right: r.right,
      rails_left: r.rails.left ?? 0,
      rails_right: r.rails.right ?? 0,
    };
    if (r.kind === "chrome") return { kind: "chrome", ...at, text: textContent(r.node) };
    if (r.kind === "edge") return { kind: "edge", ...at, side: r.side, join: r.join };
    if (r.kind === "baseline")
      return {
        kind: "baseline",
        ...at,
        element: r.element,
        line: r.line.logical_line,
        index: r.index,
        text: r.row.text,
      };
    return {
      kind: "text",
      ...at,
      element: r.element,
      line: r.line.logical_line,
      row_index: r.rowIndex,
      last_line: r.lastLine,
    };
  });
}

describe("the row layout", () => {
  it("has a corpus that has not quietly shrunk", () => {
    expect(corpus.cases.length).toBeGreaterThanOrEqual(14);
  });

  for (const c of corpus.cases) {
    it(`matches the shared corpus: ${c.name}`, () => {
      // `toEqual` ignores key order but not key *presence*, so a mirror that stopped emitting a
      // field fails here rather than passing on a subset.
      expect(reduce(c.root, c.measured), c.why).toEqual(c.rows);
      expect(totalRows(c.root, c.measured), `${c.name}: total height`).toEqual(c.total_rows);
    });
  }
});

describe("column layout", () => {
  /** Every row's horizontal structure: the gutter column, then the content.
   *
   *  The vertical half is pinned by the shared corpus above, which both walks read. The horizontal
   *  half cannot be shared — a cell is not a pixel — so each shell pins its own, here and in
   *  `aether-tui/src/ui.rs` and `aether-iced/src/app/headless.rs`. Nothing covered this before;
   *  frames move all of it. */
  function rowParts(window: BufferWindow): { gutters: number; contents: number; order: string }[] {
    const container = document.createElement("div");
    renderBuffer(container, {
      window,
      cursor,
      insertMode: false,
      awaitingKey: false,
      contentWidthPx: 0,
      spacerHeightPx: 0,
      contentTopPx: 0,
      rowHeightPx: 0,
      measured: WHOLE_ROWS,
      blame: null,
      diffView: false,
      focusedElement: 0,
    });
    return [...container.querySelectorAll(".row")].map((el) => ({
      gutters: el.querySelectorAll(":scope > .gutter").length,
      contents: el.querySelectorAll(":scope > .content").length,
      order: [...el.children].map((c) => (c.classList.contains("gutter") ? "gutter" : c.classList.contains("content") ? "content" : "?")).join(","),
    }));
  }

  const patch = windowOf({
    node: "column",
    children: [chrome("a.rs"), editor(0, 16, [line(16, "fn f17"), line(17, "fn f18")])],
  });

  it("gives every row exactly one gutter, before its content", () => {
    const parts = rowParts(patch);
    expect(parts.length).toBeGreaterThan(0);
    for (const p of parts) {
      expect(p.gutters, `one gutter per row (${p.order})`).toBe(1);
      expect(p.contents, `one content per row (${p.order})`).toBe(1);
      // The gutter is `position: sticky`, so it must lead — it paints over whatever precedes it.
      expect(p.order, "the gutter leads the row").toBe("gutter,content");
    }
  });

  it("puts a row's text inside its content, never in the gutter", () => {
    const container = document.createElement("div");
    renderBuffer(container, {
      window: patch,
      cursor,
      insertMode: false,
      awaitingKey: false,
      contentWidthPx: 0,
      spacerHeightPx: 0,
      contentTopPx: 0,
      rowHeightPx: 0,
      measured: WHOLE_ROWS,
      blame: null,
      diffView: false,
      focusedElement: 0,
    });
    for (const el of container.querySelectorAll(".row")) {
      const gutter = el.querySelector(":scope > .gutter");
      const content = el.querySelector(":scope > .content");
      expect(gutter?.textContent ?? "", "the gutter carries no text").toBe("");
      // Chrome and code alike: the words live in `.content`, which is what an inset would move.
      expect((content?.textContent ?? "").length + (gutter?.textContent ?? "").length).toBe(
        (el.textContent ?? "").length,
      );
    }
    const texts = [...container.querySelectorAll(".row > .content")].map((c) => c.textContent?.trim());
    expect(texts).toEqual(["a.rs", "fn f17", "fn f18"]);
  });
});

describe("boxes", () => {
  /** A box holds its rows in and owns a rule row of its own.
   *
   *  Nothing produces this tree yet — `patch.rs` switches over in the next stage — so it is built
   *  by hand. The painter has to be right before a producer depends on it. */
  const boxed = windowOf({
    node: "column",
    children: [
      {
        node: "column",
        edges: { border: { top: 1, left: 1 }, padding: { left: 1 }, collapse: true },
        band: "chrome",
        children: [chrome("a.rs"), editor(0, 16, [line(16, "fn f17")])],
      },
    ],
  });

  function rowsOf(window: BufferWindow): HTMLElement[] {
    const container = document.createElement("div");
    renderBuffer(container, {
      window,
      cursor,
      insertMode: false,
      awaitingKey: false,
      contentWidthPx: 0,
      spacerHeightPx: 0,
      contentTopPx: 0,
      rowHeightPx: 0,
      measured: WHOLE_ROWS,
      blame: null,
      diffView: false,
      focusedElement: 0,
    });
    return [...container.querySelectorAll(".row")] as HTMLElement[];
  }

  it("draws the box's own edge row, opening its rail", () => {
    const rows = rowsOf(boxed);
    expect(rows[0].classList.contains("box-edge"), rows[0].className).toBe(true);
    expect(rows[0].classList.contains("opens"), rows[0].className).toBe(true);
    // An edge is a rule, not words — there is no node to read text from.
    expect(rows[0].textContent).toBe("");
  });

  it("insets every row inside it, chrome and code alike", () => {
    for (const row of rowsOf(boxed)) {
      expect(row.style.getPropertyValue("--inset-left"), row.className).toBe("2ch");
    }
  });

  it("leaves rows outside a box uninset", () => {
    const plain = windowOf({
      node: "column",
      children: [chrome("a.rs"), editor(0, 16, [line(16, "fn f17")])],
    });
    for (const row of rowsOf(plain)) {
      expect(row.style.getPropertyValue("--inset-left"), row.className).toBe("");
      expect(row.classList.contains("boxed"), row.className).toBe(false);
    }
  });

  /** `boxed` is what the stylesheet keys the band and the rails off. Without it a boxed row is
   *  inset with nothing drawn in the cells it gave up, and the line tint underneath simply runs
   *  through them — which is how the cursor's line came to read a box wider than every other. */
  it("marks every row inside a box as boxed", () => {
    for (const row of rowsOf(boxed)) {
      expect(row.classList.contains("boxed"), row.className).toBe(true);
    }
  });
});

describe("the well and the ground", () => {
  /** The class each painted row carries, in order.
   *
   *  The well/ground split is drawn entirely in CSS, off these classes: an editor row is `.row`
   *  and paints `--bg`; a chrome row adds `.patch-chrome` and paints `--bg-app`, which is also
   *  what `#buffer` behind them shows. Nothing in the DOM can be asked what colour it came out —
   *  happy-dom has no layout — so what a painter test can pin is that the classes the rules key
   *  off are the ones the rows actually get. */
  function classesOf(window: BufferWindow): string[][] {
    const container = document.createElement("div");
    renderBuffer(container, {
      window,
      cursor,
      insertMode: false,
      awaitingKey: false,
      contentWidthPx: 0,
      spacerHeightPx: 0,
      contentTopPx: 0,
      rowHeightPx: 0,
      measured: WHOLE_ROWS,
      blame: null,
      diffView: false,
      focusedElement: 0,
    });
    return [...container.querySelectorAll(".row")].map((el) => [...el.classList]);
  }

  const patch = windowOf({
    node: "column",
    children: [chrome("a.rs"), editor(0, 16, [line(16, "fn f17"), line(17, "fn f18")])],
  });

  it("marks chrome rows and leaves buffer rows plain", () => {
    const [heading, ...text] = classesOf(patch);
    expect(heading, "a chrome row carries the class the ground rule keys off").toContain(
      "patch-chrome",
    );
    for (const row of text) {
      expect(row, "a buffer row is a plain `.row` — the well is its own default").toEqual(["row"]);
    }
  });

  it("leaves the gaps between elements to the pane", () => {
    // A row nothing is loaded at is a bare spacer with no `.row` class, so it paints neither the
    // well nor a tint — it is `#buffer` showing through, which is the ground.
    const container = document.createElement("div");
    renderBuffer(container, {
      window: patch,
      cursor,
      insertMode: false,
      awaitingKey: false,
      contentWidthPx: 0,
      spacerHeightPx: 0,
      contentTopPx: 0,
      rowHeightPx: 0,
      measured: WHOLE_ROWS,
      blame: null,
      diffView: false,
      focusedElement: 0,
    });
    for (const gap of container.querySelectorAll(".row-gap")) {
      expect([...gap.classList]).toEqual(["row-gap"]);
    }
  });
});

/** The stylesheet, read as text.
 *
 *  Not a rendering test — happy-dom has no layout — but the one thing about these rules that can
 *  break silently and cannot be seen in the DOM: their *order*. */
describe("the box stylesheet", () => {
  const css = readFileSync(resolve(process.cwd(), "src/theme.css"), "utf8");

  /** One rule's declarations, by its selector. */
  const rule = (selector: string): string => {
    const at = css.indexOf(selector);
    expect(at, `${selector} should be in theme.css`).toBeGreaterThan(0);
    return css.slice(at, css.indexOf("}", at));
  };

  /** Every row-background rule uses the `background` **shorthand**, which resets
   *  `background-image` to `none`. The box's band and rails are background images, so a box rule
   *  placed before them is wiped off the cursor's line and every diff-tinted row — the rows a
   *  missing rail is noticed on, and the only ones the bug ever showed up on. */
  it("draws boxes after every rule that sets a row background", () => {
    const lastShorthand = Math.max(
      ...[...css.matchAll(/^\.row[^{\n]*\{[^}\n]*\bbackground:/gm)].map((m) => m.index ?? 0),
    );
    expect(lastShorthand).toBeGreaterThan(0);
    for (const selector of [".row.boxed {", ".row.box-edge {"]) {
      const at = css.indexOf(selector);
      expect(at, `${selector} should be in theme.css`).toBeGreaterThan(0);
      expect(
        at,
        `${selector} must come after the last \`background:\` shorthand on a row, which would ` +
          `otherwise reset its background-image to none`,
      ).toBeGreaterThan(lastShorthand);
    }
  });

  /** The app's ground is the canvas and the editor's well is the rows on it.
   *
   *  Hand-mirrored from `Theme` in `crates/aether-client/src/theme.rs`, so which var each surface
   *  names is exactly the thing that can drift here and nowhere else. `.row` must set the well
   *  itself: `#buffer` behind it is the ground, so a row that named no background would come out
   *  as ground. And it has to come first, or the tints after it would never win. */
  it("puts the ground on the app and the well on the rows", () => {
    // The declaration block, not the `html, body, #app` sizing rule that precedes it.
    expect(
      rule("#app {\n  display: flex;"),
      "the application's background is the chrome shade",
    ).toContain("background: var(--bg-app)");
    expect(rule(".row {"), "an editor row paints the well").toContain("background: var(--bg)");
    expect(rule(".row.patch-chrome {"), "a chrome row is the ground showing through").toContain(
      "background: var(--bg-app)",
    );
    expect(
      css.indexOf(".row.patch-chrome {"),
      "the chrome rule must follow the plain `.row` one it overrides",
    ).toBeGreaterThan(css.indexOf(".row {"));
    // Prose is an editor element too, so the whole reading pane is a well.
    expect(rule("#buffer.md-read-host {")).toContain("background: var(--bg)");
    // The retired role must not linger in either theme block or any rule.
    expect(css, "--patch-chrome-bg is retired; the ground is one role").not.toContain(
      "--patch-chrome-bg",
    );
  });

  /** Both rails read one offset, each measured from its own edge.
   *
   *  A percentage in `background-position` resolves against the positioning area **minus the
   *  layer's own width**, so `calc(100% - …)` put the right rail a pixel further in than the left
   *  — the lopsidedness a single shared offset exists to rule out. The four-value form
   *  (`right <offset>`) measures from the named edge and has no such subtraction. */
  it("draws both rails one shared offset in from their own edge", () => {
    const boxed = rule(".row.boxed {");
    expect(boxed).toContain("left var(--rail-inset)");
    expect(boxed).toContain("right var(--rail-inset)");
    expect(
      /background-position:[^;]*calc\(\s*100%/.test(boxed),
      "a rail positioned with calc(100% - …) lands a layer-width short of its own edge",
    ).toBe(false);
  });

  /** The gutter is `position: sticky` and opaque, so it paints over whatever the row draws
   *  beneath it — on a border row, a cell-wide gap in the rule. It has nothing to protect there
   *  (no text, no change bar), so it has to get out of the way. */
  it("clears the gutter's background on a border row", () => {
    expect(rule(".row.box-edge .gutter {")).toContain("background-color: transparent");
    expect(
      css.indexOf(".row.box-edge .gutter {"),
      "and after the rule that makes an ordinary chrome gutter opaque",
    ).toBeGreaterThan(css.indexOf(".row.patch-chrome .gutter {"));
  });

  /** A box's name lies *on* the rule, so it has to hide the stretch of rule it covers — and hide
   *  it in the shade the row itself paints, or the name reads as a differently-coloured patch cut
   *  out of the border. The space either side of it is the padding, which the same background
   *  covers: the terminal's `┌─ name ────┐` has a blank cell there, not a rule. */
  it("masks the rule under a box's name, in the row's own shade", () => {
    const title = rule(".row.box-edge .title {");
    expect(title, "a blank cell either side of the name").toContain("padding: 0 1ch");
    expect(title, "the plain row's shade").toContain("background-color: var(--bg)");
    expect(
      rule(".row.box-edge.patch-chrome .title {"),
      "and the chrome band's, on a box that declares one",
    ).toContain("background-color: var(--bg-app)");
    expect(
      css.indexOf(".row.box-edge .title {"),
      "the mask must follow the rule it masks, or the gradient paints over it",
    ).toBeGreaterThan(css.indexOf(".row.box-edge {"));
  });
});

describe("the buffer painter", () => {
  /// The shape that blanked the terminal and the GUI: an element windowing a *file*, so its lines
  /// start at 16 while the view's own first line is 0. Anything that indexes by
  /// `logical_line - first_view_line` reads off the end of a 3-item list and draws nothing.
  it("paints a hunk whose lines start partway down its file", () => {
    const w = windowOf(
      {
        node: "column",
        children: [chrome("a.rs"), editor(0, 16, [line(16, "fn f17"), line(17, "fn f18")])],
      },
    );
    expect(painted(w)).toEqual(["chrome a.rs", "text fn f17", "text fn f18"]);
  });

  /// A shell: each run in a box named on its top border, and the input element last in a box of
  /// its own. The input is an editor like any other — that is the whole point of the role riding
  /// on the node rather than the view having a kind — so the painter needs no branch for it, and
  /// this is what proves it.
  ///
  /// The name rides the border row, so it adds no row: a run's box is exactly as tall named as it
  /// would be nameless, and the row the name lands on is the one the box was already spending.
  it("paints a shell's runs above its input", () => {
    const input: ViewNode = {
      ...(editor(2, 0, [line(0, "cargo build")]) as ViewNode & { node: "editor" }),
      role: "input",
    };
    // Each run in a box of its own, closed on all four sides and named on its top border.
    const boxed = (name: string, children: ViewNode[]): ViewNode => ({
      node: "column",
      edges: { border: { top: 1, right: 1, bottom: 1, left: 1 } },
      band: "chrome",
      title: [{ node: "text", text: name }],
      children,
    });
    const w = windowOf({
      node: "column",
      children: [
        boxed("~/proj  ok", [chrome("echo one"), editor(0, 0, [line(0, "one")])]),
        chrome(""),
        boxed("~/proj  ok", [chrome("echo two"), editor(1, 1, [line(1, "two")])]),
        chrome(""),
        boxed("~/proj", [input]),
      ],
    });
    expect(painted(w)).toEqual([
      "edge ~/proj  ok",
      "chrome echo one",
      "text one",
      "edge ",
      "chrome ",
      "edge ~/proj  ok",
      "chrome echo two",
      "text two",
      "edge ",
      "chrome ",
      "edge ~/proj",
      "text cargo build",
      "edge ",
    ]);
  });

  /// A new shell is a real state: just the line you type into.
  it("paints a shell with no runs as its input alone", () => {
    const input: ViewNode = {
      ...(editor(0, 0, [line(0, "ls -la")]) as ViewNode & { node: "editor" }),
      role: "input",
    };
    expect(painted(windowOf({ node: "column", children: [input] }))).toEqual([
      "text ls -la",
    ]);
  });

  /// Two files in one view. The second's lines are numbered *below* the first's, so any lookup by
  /// line number alone finds the wrong row — or, dropping the element entirely, no row at all.
  it("paints both files when their line numbers collide", () => {
    const w = windowOf(
      {
        node: "column",
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
      node: "column",
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
      measured: WHOLE_ROWS,
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
    const w = windowOf({ node: "column", children: [chrome("a.md"), prose] });
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
    expect(paint(WHOLE_ROWS)).toEqual(["a.md", "gap 20px", "two", "three", "four"]);
    // Measured: line 3 laid out three rows tall, so line 4 moves down by two.
    expect(
      paint({ units_per_row: 1, elements: { 0: { first_row: 2, starts: [2, 3, 6], end: 8 } } }),
    ).toEqual([
      "a.md",
      "gap 20px",
      "two",
      "three",
      "gap 20px",
      "four",
    ]);
    expect(
      totalRows(w.root, { units_per_row: 1, elements: { 0: { first_row: 2, starts: [2, 3, 6], end: 8 } } }),
    ).toBe(1 + 2 + 6 + 5);
  });

  /// The closing rule hangs off the *last rendered row*, not off a line number. Asking
  /// `logical_line + 1 === view_line_count` compares a buffer line to a view line: here the last
  /// line is numbered 11 in a 4-line view, so that test never fires and the rule vanishes.
  it("draws the closing chrome after the final row, whatever that row's line number is", () => {
    const w = windowOf(
      {
        node: "column",
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
        node: "column",
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
      measured: WHOLE_ROWS,
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
        node: "column",
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
      measured: WHOLE_ROWS,
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
