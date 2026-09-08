//! The markdown reading view's DOM renderer. Renders the shared markdown AST (`markdown.ts` types,
//! parsed in the Rust core) as semantic, typographic HTML — real headings, tables, lists, quotes,
//! code panels, images via the server's confined `/asset/` route. Everything goes through
//! `textContent` (never `innerHTML`); link hrefs are scheme-checked.
//!
//! Focus (the reading cursor) is derived core-side from the server cursor and arrives as a
//! source byte span: the node whose `data-espan` matches gets `.md-focus`. The shell scrolls
//! the focused node into view when the focus *changes*, off the shared layout the shell measured
//! this document into (`Shell.measureReader` / `revealBlock`).

import type { MdBlock, MdInline, MdSpan } from "./markdown";
import { highlightClass } from "./render";

/** One tree-sitter run into a fence's code (mirrors the viewport `Highlight` wire type). */
export interface CodeHighlight {
  start: number;
  end: number;
  kind: string;
}

export interface ReadDoc {
  loading: boolean;
  /** The core's stand-in line for a document with nothing to render — "Loading…" while the first
   *  fetch is in flight, "Empty document" once it lands with no blocks. Null when there are
   *  blocks to show. */
  placeholder: string | null;
  blocks: MdBlock[];
  /** The reading position (block grain) — rendered as the left bar, always present for a
   *  non-empty document. */
  focus_span: MdSpan | null;
  /** The Enter target (interactive grain) — the link/image/footnote-ref span the cursor sits
   *  inside, rendered as the pill on top of the block bar; null otherwise. Both derive from
   *  the one server cursor. */
  target_span: MdSpan | null;
  /** The extended block selection's inclusive source byte range —
   *  intersecting top-level blocks tint `.md-selected`. Null while the cursor is a point. */
  selection_span?: MdSpan | null;
  buffer_id: number;
  /** Content revision the document was parsed at — the shell's DOM-rebuild key. */
  revision: number;
  /** Bumped as fence highlights land — the rebuild key's second half. */
  hl_gen: number;
  /** Fence highlights keyed by the code block's span start (stringified). */
  code_highlights: Record<string, CodeHighlight[]>;
  /** Shell-provided (not part of the wasm view): the app URL for a relative doc link, or
   *  `null` when it can't be expressed — internal links render as real `<a href>`s so the
   *  browser's new-tab affordances work (the picker-row treatment). */
  internalHref?: (href: string) => string | null;
}

const EXTERNAL = /^(https?|mailto):/i;

function spanKey(s: MdSpan): string {
  return `${s.start}:${s.end}`;
}

/** Render the whole document into `container` (replacing its children), then mark focus. */
export function renderReadView(container: HTMLElement, doc: ReadDoc): void {
  const root = document.createElement("div");
  root.className = "md-read";
  // Nothing to render — still loading, or a document with no blocks at all. The core names the
  // line for every shell.
  if (doc.placeholder) {
    const msg = document.createElement("div");
    msg.className = "md-read-placeholder";
    msg.textContent = doc.placeholder;
    root.append(msg);
  }
  for (const b of doc.blocks) root.append(renderBlock(b, doc));
  container.replaceChildren(root);
  markFocus(container, doc.focus_span, doc.target_span, doc.selection_span ?? null);
}

/** Mark the focus projections (cheap enough to run per render): `.md-focus` — the block
 *  bar — on the reading-position node, `.md-target` — the pill — on the interactive node the
 *  cursor sits inside. They usually differ (bar on the paragraph, pill on its link) and may
 *  coincide (a block image is both position and target). An extended selection adds
 *  `.md-selected` — the editor's selection tint — to every *top-level* block intersecting its
 *  byte range (the core suppresses the pill while one exists). */
/** Render an agent's reply: the reading view's typography, and none of its behaviour.
 *
 *  A reply is a **static render** — it is not a projection of a document you are editing. So the
 *  reading position is never marked, no `internalHref` is supplied (so links to other files render
 *  as plain text rather than as navigation), and nothing here reads or writes a cursor. What it
 *  keeps is `renderBlock`: real headings, lists, quotes and code panels, so a reply reads as prose
 *  rather than as the source it arrived as.
 *
 *  Full width by design. The reader centres a narrow column because it is a page; a reply shares
 *  its view with tool calls and diffs that run the whole width. */
export function renderReply(container: HTMLElement, blocks: MdBlock[]): void {
  const doc: ReadDoc = {
    loading: false,
    placeholder: null,
    blocks,
    focus_span: null,
    target_span: null,
    buffer_id: 0,
    revision: 0,
    hl_gen: 0,
    code_highlights: {},
  };
  const root = document.createElement("div");
  // Both classes: `md-read` is where the reading view's typography lives, and `md-reply`
  // overrides the parts of it that belong to a *page* — the centred narrow measure and its padding.
  root.className = "md-read md-reply";
  for (const b of blocks) root.append(renderBlock(b, doc));
  container.replaceChildren(root);
}

export function markFocus(
  container: HTMLElement,
  block: MdSpan | null,
  target: MdSpan | null,
  selection: MdSpan | null = null,
): void {
  for (const el of container.querySelectorAll(".md-focus, .md-target, .md-selected")) {
    el.classList.remove("md-focus", "md-target", "md-selected");
  }
  if (block) {
    container.querySelector(`[data-espan="${spanKey(block)}"]`)?.classList.add("md-focus");
  }
  if (target) {
    container.querySelector(`[data-espan="${spanKey(target)}"]`)?.classList.add("md-target");
  }
  if (selection) {
    // Every *contained* stamped node gets the class; the CSS scopes the tint to the same
    // block-node list the focus bar uses, so inline spans inside stay unpainted. Containment
    // rather than overlap: a list item's span contains its nested children's, so overlap would
    // tint every ancestor of the selected item too.
    for (const el of container.querySelectorAll("[data-espan]")) {
      const parts = (el.getAttribute("data-espan") ?? "").split(":").map(Number);
      if (parts.length === 2 && parts[0] >= selection.start && parts[1] <= selection.end + 1) {
        el.classList.add("md-selected");
      }
    }
  }
}

/** Fill a <code> element with highlighted runs (editor hl-* classes; gaps stay plain). */
function fillCode(code: HTMLElement, text: string, hls: CodeHighlight[]): void {
  code.replaceChildren();
  let pos = 0;
  for (const h of hls) {
    const s = Math.max(0, Math.min(h.start, text.length));
    const e = Math.max(s, Math.min(h.end, text.length));
    if (s > pos) code.append(document.createTextNode(text.slice(pos, s)));
    const cls = highlightClass(h.kind);
    if (cls) {
      const span = document.createElement("span");
      span.className = cls;
      span.textContent = text.slice(s, e);
      code.append(span);
    } else {
      code.append(document.createTextNode(text.slice(s, e)));
    }
    pos = e;
  }
  if (pos < text.length) code.append(document.createTextNode(text.slice(pos)));
}

/** Patch newly arrived fence highlights into the rendered document in place. A big spec can
 *  carry dozens of fences whose results stream in one by one — rebuilding the whole DOM per
 *  result is what made large documents take seconds to settle. Idempotent per block
 *  (`data-hl` marks painted fences). */
export function applyFenceHighlights(container: HTMLElement, doc: ReadDoc): void {
  for (const [start, hls] of Object.entries(doc.code_highlights)) {
    if (!hls || hls.length === 0) continue;
    const block = container.querySelector(`.md-codeblock[data-espan^="${start}:"]`);
    if (!(block instanceof HTMLElement) || block.dataset.hl === "1") continue;
    const code = block.querySelector("code");
    if (!code) continue;
    fillCode(code as HTMLElement, code.textContent ?? "", hls);
    block.dataset.hl = "1";
  }
}

/** Selects the nodes a shell **measures**: the block-grain stamps, and not the interactive ones
 *  sitting inside them.
 *
 *  Both grains carry `data-espan`, because focus marking looks a span up whichever grain it is.
 *  Measurement must not. The core takes the *innermost* span covering a source line as where that
 *  line starts, and a link beginning partway through a line is not where the line starts. The GUI
 *  stamps blocks and list items only, and the entire reason the measuring rule lives in the core
 *  is that the two shells cannot be allowed to disagree about a height. */
export const MEASURABLE_BLOCKS = "[data-espan]:not([data-etarget])";

/** Stamp a block-grain node: a `j`/`k` reading stop, and a node measurement places lines by. */
function stampBlock(el: HTMLElement, span: MdSpan): void {
  el.dataset.espan = spanKey(span);
}

/** Stamp an interactive-grain node: a `Tab` stop and `Enter` target, living inside some block.
 *  Findable by span exactly like a block, and deliberately invisible to measurement. */
function stampTarget(el: HTMLElement, span: MdSpan): void {
  el.dataset.espan = spanKey(span);
  el.dataset.etarget = "";
}

function renderBlock(b: MdBlock, doc: ReadDoc): Node {
  const bufferId = doc.buffer_id;
  switch (b.kind) {
    case "heading": {
      const h = document.createElement(`h${Math.min(Math.max(b.level, 1), 6)}`);
      stampBlock(h, b.span);
      renderInlines(b.content, h, doc);
      return h;
    }
    case "paragraph": {
      const p = document.createElement("p");
      stampBlock(p, b.span);
      renderInlines(b.content, p, doc);
      return p;
    }
    case "code": {
      const wrap = document.createElement("div");
      wrap.className = "md-codeblock";
      stampBlock(wrap, b.span);
      if (b.language) {
        const tag = document.createElement("div");
        tag.className = "md-codeblock-lang";
        tag.textContent = b.language;
        wrap.append(tag);
      }
      const pre = document.createElement("pre");
      const code = document.createElement("code");
      // Tree-sitter runs (when the server's snippet highlights have landed for this fence),
      // styled with the editor's own hl-* classes; plain text until then (and patched in
      // place by `applyFenceHighlights` when they arrive — no full rebuild).
      const hls = doc.code_highlights[String(b.span.start)];
      if (hls && hls.length > 0) {
        fillCode(code, b.code, hls);
        wrap.dataset.hl = "1";
      } else {
        code.textContent = b.code;
      }
      pre.append(code);
      wrap.append(pre);
      return wrap;
    }
    case "html": {
      // Raw HTML is shown literally, never interpreted.
      const pre = document.createElement("pre");
      pre.className = "md-rawhtml";
      stampBlock(pre, b.span);
      pre.textContent = b.raw;
      return pre;
    }
    case "list": {
      const list = document.createElement(b.ordered ? "ol" : "ul");
      if (b.ordered && (b.start ?? 1) !== 1) (list as HTMLOListElement).start = b.start;
      for (const item of b.items) {
        const li = document.createElement("li");
        stampBlock(li, item.span);
        if (item.checked !== undefined) {
          li.className = "md-task" + (item.checked ? " md-task-done" : "");
          const box = document.createElement("span");
          box.className = "md-task-box";
          box.textContent = item.checked ? "☑" : "☐";
          li.append(box);
        }
        for (const ib of item.blocks) li.append(renderBlock(ib, doc));
        list.append(li);
      }
      return list;
    }
    case "quote": {
      const q = document.createElement("blockquote");
      stampBlock(q, b.span);
      if (b.alert) {
        q.className = `md-alert md-alert-${b.alert}`;
        const label = document.createElement("div");
        label.className = "md-alert-label";
        label.textContent = b.alert[0].toUpperCase() + b.alert.slice(1);
        q.append(label);
      }
      for (const cb of b.content) q.append(renderBlock(cb, doc));
      return q;
    }
    case "rule": {
      // Wrapped in a padded div so the focus bar has height to stand next to — on a bare
      // <hr> (~1px tall) the bar's inset top/bottom collapse it to nothing.
      const wrap = document.createElement("div");
      wrap.className = "md-rule";
      stampBlock(wrap, b.span);
      wrap.append(document.createElement("hr"));
      return wrap;
    }
    case "table": {
      // Outer wrapper carries the focus stamp/bar; the inner div owns the horizontal scroll
      // (an overflow container would clip the bar pseudo-element).
      const outer = document.createElement("div");
      outer.className = "md-table-outer";
      stampBlock(outer, b.span);
      const scroll = document.createElement("div");
      scroll.className = "md-table-scroll";
      const table = document.createElement("table");
      const align = (i: number): string | undefined =>
        b.alignments[i] === "left" || b.alignments[i] === "center" || b.alignments[i] === "right"
          ? b.alignments[i]
          : undefined;
      if (b.head.length > 0) {
        const thead = document.createElement("thead");
        const tr = document.createElement("tr");
        b.head.forEach((cell, i) => {
          const th = document.createElement("th");
          const a = align(i);
          if (a) th.style.textAlign = a;
          renderInlines(cell, th, doc);
          tr.append(th);
        });
        thead.append(tr);
        table.append(thead);
      }
      const tbody = document.createElement("tbody");
      for (const row of b.rows) {
        const tr = document.createElement("tr");
        row.forEach((cell, i) => {
          const td = document.createElement("td");
          const a = align(i);
          if (a) td.style.textAlign = a;
          renderInlines(cell, td, doc);
          tr.append(td);
        });
        tbody.append(tr);
      }
      table.append(tbody);
      scroll.append(table);
      outer.append(scroll);
      return outer;
    }
    case "image":
      return renderImage(b.src, b.alt, b.span, bufferId, true, b.inner_span);
    case "front_matter": {
      const fm = document.createElement("pre");
      fm.className = "md-front-matter";
      stampBlock(fm, b.span);
      fm.textContent = b.text;
      return fm;
    }
    case "footnote_def": {
      const d = document.createElement("div");
      d.className = "md-footnote-def";
      stampBlock(d, b.span);
      const label = document.createElement("span");
      label.className = "md-footnote-label";
      label.textContent = `[${b.label}]: `;
      d.append(label);
      for (const cb of b.content) d.append(renderBlock(cb, doc));
      return d;
    }
  }
}

/** An image node: relative and root-relative sources ride the server's confined asset route;
 *  remote http(s) sources load directly (the browser fetches; an `<img>` context never runs
 *  SVG scripts). Other schemes and protocol-relative URLs render as their alt text. A display
 *  image's wrapper
 *  carries the block span (the bar host) while the `<img>` carries `innerSpan` — the Enter
 *  target — so the `.md-target` ring appears only once `l` arms it. */
function renderImage(
  src: string,
  alt: string,
  span: MdSpan,
  bufferId: number,
  block: boolean,
  innerSpan?: MdSpan,
): Node {
  const remote = /^https?:/i.test(src);
  const external = /^[a-z][a-z0-9+.-]*:/i.test(src);
  // Root-relative sources (`/img.png`) ride the asset route like any relative source — the
  // server resolves the leading `/` against the buffer's workspace root (GitHub semantics)
  // and 404s for buffers outside every root, where the alt text renders. Protocol-relative
  // (`//host/…`) and non-http schemes stay placeholders.
  if ((external && !remote) || src.startsWith("//")) {
    const ph = document.createElement(block ? "div" : "span");
    ph.className = block ? "md-image-alt md-image-block" : "md-image-alt";
    (block ? stampBlock : stampTarget)(ph, span);
    ph.textContent = `▨ [${alt || "image"}]  (${src})`;
    return ph;
  }
  const img = document.createElement("img");
  img.className = block ? "md-image" : "md-image md-image-inline";
  // The relative path is encoded as ONE opaque segment (slashes included): a literal `../`
  // would be collapsed by URL normalization before the request leaves the browser, so the
  // server would never see it — `..%2F` survives to be resolved (and confined) server-side.
  img.src = remote ? src : `/asset/${bufferId}/${encodeURIComponent(src)}`;
  img.alt = alt;
  // Eager: heights settle right after render, so focus reveals aren't invalidated by images
  // finishing their loads above the focused element (lazy loads fired *during* scrolling).
  img.decoding = "async";
  if (block) {
    // A display image gets a stamped wrapper: ::before can't render on a replaced element,
    // so the focus bar lives on the div (block span), which shrinks to the image; the img
    // itself is stamped with the target span, hosting the armed ring.
    const wrap = document.createElement("div");
    wrap.className = "md-image-block";
    stampBlock(wrap, span);
    stampTarget(img, innerSpan ?? span);
    wrap.append(img);
    return wrap;
  }
  stampTarget(img, span);
  return img;
}

function renderInlines(inlines: MdInline[], parent: HTMLElement, doc: ReadDoc): void {
  for (const inl of inlines) parent.append(renderInline(inl, doc));
}

function renderInline(inl: MdInline, doc: ReadDoc): Node {
  const bufferId = doc.buffer_id;
  switch (inl.kind) {
    case "text":
      return document.createTextNode(inl.text);
    case "code": {
      const c = document.createElement("code");
      c.textContent = inl.text;
      return c;
    }
    case "emphasis": {
      const em = document.createElement("em");
      renderInlines(inl.content, em, doc);
      return em;
    }
    case "strong": {
      const s = document.createElement("strong");
      renderInlines(inl.content, s, doc);
      return s;
    }
    case "strikethrough": {
      const del = document.createElement("del");
      renderInlines(inl.content, del, doc);
      return del;
    }
    case "link": {
      const a = document.createElement("a");
      a.className = "md-link";
      stampTarget(a, inl.span);
      renderInlines(inl.content, a, doc);
      if (EXTERNAL.test(inl.href)) {
        a.href = inl.href;
        a.target = "_blank";
        a.rel = "noopener noreferrer";
      } else {
        // Cross-file targets get a real app URL (the picker-row treatment): modified/middle
        // clicks open the doc in a new tab natively; plain clicks are intercepted by the
        // shell's read click handler, which follows the link in-app like Enter. In-document
        // anchors stay hrefless (plain click still follows via the shell handler).
        a.classList.add("md-link-internal");
        a.title = inl.href;
        const href = doc.internalHref?.(inl.href);
        if (href) a.href = href;
      }
      return a;
    }
    case "image":
      return renderImage(inl.src, inl.alt, inl.span, bufferId, false);
    case "footnote_ref": {
      const sup = document.createElement("sup");
      sup.className = "md-footnote-ref";
      stampTarget(sup, inl.span);
      sup.textContent = `[${inl.label}]`;
      return sup;
    }
    case "hard_break":
      return document.createElement("br");
  }
}
