//! The markdown reading view's DOM renderer (docs/markdown-view.md §2.8, web). Renders the
//! shared markdown AST (`markdown.ts` types, parsed in the Rust core) as semantic, typographic
//! HTML — real headings, tables, lists, quotes, code panels, images via the server's confined
//! `/asset/` route. Everything goes through `textContent` (never `innerHTML`); link
//! hrefs are scheme-checked.
//!
//! Focus (the reading cursor) is derived core-side from the server cursor and arrives as a
//! source byte span: the node whose `data-espan` matches gets `.md-focus`. The shell scrolls
//! the focused node into view when the focus *changes* (`revealFocus`).

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
  blocks: MdBlock[];
  /** The reading position (block grain) — rendered as the left bar, always present for a
   *  non-empty document. */
  focus_span: MdSpan | null;
  /** The Enter target (interactive grain) — the link/image/footnote-ref span the cursor sits
   *  inside, rendered as the pill on top of the block bar; null otherwise. Both derive from
   *  the one server cursor. */
  target_span: MdSpan | null;
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
  if (doc.loading && doc.blocks.length === 0) {
    const load = document.createElement("div");
    load.className = "md-read-loading";
    load.textContent = "Loading…";
    root.append(load);
  }
  for (const b of doc.blocks) root.append(renderBlock(b, doc));
  container.replaceChildren(root);
  markFocus(container, doc.focus_span, doc.target_span);
}

/** Mark the two focus projections (cheap enough to run per render): `.md-focus` — the block
 *  bar — on the reading-position node, `.md-target` — the pill — on the interactive node the
 *  cursor sits inside. They usually differ (bar on the paragraph, pill on its link) and may
 *  coincide (a block image is both position and target). */
export function markFocus(
  container: HTMLElement,
  block: MdSpan | null,
  target: MdSpan | null,
): void {
  for (const el of container.querySelectorAll(".md-focus, .md-target")) {
    el.classList.remove("md-focus", "md-target");
  }
  if (block) {
    container.querySelector(`[data-espan="${spanKey(block)}"]`)?.classList.add("md-focus");
  }
  if (target) {
    container.querySelector(`[data-espan="${spanKey(target)}"]`)?.classList.add("md-target");
  }
}

/** The scroll target that reveals the focused node — `null` when it's already comfortably
 *  visible. The shell applies it through its own `scrollTopTo`, so reveals glide exactly like
 *  the editor's (smooth when short, snap when far).
 *
 *  Deliberately not `scrollIntoView(block: "nearest")`: nearest leaves a downward step pinned
 *  flush to the bottom edge, and any later layout shift (an image finishing its load above)
 *  pushes it off screen with no focus change to trigger a re-reveal. Instead: when the element
 *  isn't comfortably visible, rest its top ~20% down the viewport — the editor's jump
 *  placement, so reading `j`-steps hold a steady eye line. */
export function revealFocus(container: HTMLElement, span: MdSpan | null): number | null {
  if (!span) return null;
  const target = container.querySelector(`[data-espan="${spanKey(span)}"]`);
  if (!(target instanceof HTMLElement)) return null;
  const c = container.getBoundingClientRect();
  const t = target.getBoundingClientRect();
  const margin = Math.min(48, c.height * 0.08);
  if (t.top >= c.top + margin && t.bottom <= c.bottom - margin) return null; // comfortably visible
  // Rest ~20% down; an element taller than the viewport pins nearer the top instead.
  const rest = Math.min(c.height * 0.2, Math.max(margin, c.height - t.height - margin));
  return container.scrollTop + (t.top - c.top - rest);
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

function stamp(el: HTMLElement, span: MdSpan): void {
  el.dataset.espan = spanKey(span);
}

function renderBlock(b: MdBlock, doc: ReadDoc): Node {
  const bufferId = doc.buffer_id;
  switch (b.kind) {
    case "heading": {
      const h = document.createElement(`h${Math.min(Math.max(b.level, 1), 6)}`);
      stamp(h, b.span);
      renderInlines(b.content, h, doc);
      return h;
    }
    case "paragraph": {
      const p = document.createElement("p");
      stamp(p, b.span);
      renderInlines(b.content, p, doc);
      return p;
    }
    case "code": {
      const wrap = document.createElement("div");
      wrap.className = "md-codeblock";
      stamp(wrap, b.span);
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
      stamp(pre, b.span);
      pre.textContent = b.raw;
      return pre;
    }
    case "list": {
      const list = document.createElement(b.ordered ? "ol" : "ul");
      if (b.ordered && (b.start ?? 1) !== 1) (list as HTMLOListElement).start = b.start;
      for (const item of b.items) {
        const li = document.createElement("li");
        stamp(li, item.span);
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
      stamp(q, b.span);
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
      stamp(wrap, b.span);
      wrap.append(document.createElement("hr"));
      return wrap;
    }
    case "table": {
      // Outer wrapper carries the focus stamp/bar; the inner div owns the horizontal scroll
      // (an overflow container would clip the bar pseudo-element).
      const outer = document.createElement("div");
      outer.className = "md-table-outer";
      stamp(outer, b.span);
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
      stamp(fm, b.span);
      fm.textContent = b.text;
      return fm;
    }
    case "footnote_def": {
      const d = document.createElement("div");
      d.className = "md-footnote-def";
      stamp(d, b.span);
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
    stamp(ph, span);
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
    stamp(wrap, span);
    stamp(img, innerSpan ?? span);
    wrap.append(img);
    return wrap;
  }
  stamp(img, span);
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
      stamp(a, inl.span);
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
      stamp(sup, inl.span);
      sup.textContent = `[${inl.label}]`;
      return sup;
    }
    case "hard_break":
      return document.createElement("br");
  }
}
