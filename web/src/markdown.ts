//! Renders the shared Markdown AST (parsed in the Rust core with pulldown-cmark, see
//! `aether-client/src/markdown.rs`) to DOM. The same AST drives the native and terminal clients, so
//! there's no Markdown parser here — only a renderer. Everything goes through `textContent` /
//! `createTextNode` (never `innerHTML`), so server/LSP content can't inject markup; link hrefs are
//! scheme-checked so an LSP can't smuggle a `javascript:` URL.
//!
//! Hover popovers render through `renderHoverDoc`; the document-scale reading view has its own
//! renderer (read.ts) over the same types. Blocks and interactive inlines carry source `span`s
//! (byte ranges into the buffer) — unused by hover, load-bearing for the reading view.

/** A source byte range — mirrors `aether_client::markdown::Span`. */
export type MdSpan = { start: number; end: number };

/** An inline (span-level) AST node — mirrors `aether_client::markdown::Inline` (serde `kind` tag). */
export type MdInline =
  | { kind: "text"; text: string }
  | { kind: "code"; text: string }
  | { kind: "emphasis"; content: MdInline[] }
  | { kind: "strong"; content: MdInline[] }
  | { kind: "strikethrough"; content: MdInline[] }
  | { kind: "link"; href: string; content: MdInline[]; span: MdSpan }
  | { kind: "image"; src: string; alt: string; span: MdSpan }
  | { kind: "footnote_ref"; label: string; span: MdSpan }
  | { kind: "hard_break" };

/** One list item — mirrors `aether_client::markdown::ListItem` (`checked` present on task items). */
export type MdListItem = { checked?: boolean; blocks: MdBlock[]; span: MdSpan };

/** A block-level AST node — mirrors `aether_client::markdown::Block`. */
export type MdBlock =
  | { kind: "heading"; level: number; content: MdInline[]; span: MdSpan }
  | { kind: "paragraph"; content: MdInline[]; span: MdSpan }
  | { kind: "code"; language: string | null; code: string; span: MdSpan }
  | { kind: "list"; ordered: boolean; start: number; items: MdListItem[]; span: MdSpan }
  | { kind: "quote"; alert?: string; content: MdBlock[]; span: MdSpan }
  | { kind: "rule"; span: MdSpan }
  | {
      kind: "table";
      alignments: string[];
      head: MdInline[][];
      rows: MdInline[][][];
      span: MdSpan;
    }
  | {
      kind: "image";
      src: string;
      alt: string;
      span: MdSpan;
      /** The image markup itself (no trailing whitespace) — the Enter-target span; the block
       *  span hosts the position bar (`l` opts into the image like a link). */
      inner_span: MdSpan;
    }
  | { kind: "front_matter"; text: string; span: MdSpan }
  | { kind: "footnote_def"; label: string; content: MdBlock[]; span: MdSpan }
  | { kind: "html"; raw: string; span: MdSpan };

/** Flatten the AST back to plain text for "copy whole popover" (Ctrl-y). Mirrors the Rust
 *  `aether_client::markdown::to_plain` so every client copies the same shape. Blocks are separated by
 *  a blank line; lists keep bullets/numbers, code its lines, links their visible text. */
export function mdToPlain(blocks: MdBlock[]): string {
  return blocksToPlain(blocks).trimEnd();
}

function blocksToPlain(blocks: MdBlock[]): string {
  let out = "";
  for (const b of blocks) {
    switch (b.kind) {
      case "heading":
      case "paragraph":
        out += inlinesToPlain(b.content) + "\n\n";
        break;
      case "code":
        for (const line of b.code.split("\n")) out += line + "\n";
        out += "\n";
        break;
      case "html":
        for (const line of b.raw.split("\n")) out += line + "\n";
        out += "\n";
        break;
      case "list":
        b.items.forEach((item, i) => {
          let marker = b.ordered ? `${(b.start ?? 1) + i}. ` : "- ";
          if (item.checked !== undefined) marker += item.checked ? "[x] " : "[ ] ";
          const pad = " ".repeat(marker.length);
          const lines = blocksToPlain(item.blocks).trimEnd().split("\n");
          lines.forEach((line, j) => {
            out += (j === 0 ? marker : pad) + line + "\n";
          });
        });
        out += "\n";
        break;
      case "quote":
        for (const line of blocksToPlain(b.content).trimEnd().split("\n")) {
          out += "> " + line + "\n";
        }
        out += "\n";
        break;
      case "rule":
        out += "---\n\n";
        break;
      case "table":
        for (const row of [b.head, ...b.rows]) {
          if (row.length === 0) continue;
          out += row.map(inlinesToPlain).join(" | ") + "\n";
        }
        out += "\n";
        break;
      case "image":
        out += b.alt + "\n\n";
        break;
      case "front_matter":
        break; // metadata, not prose
      case "footnote_def":
        out += `[${b.label}]: ` + blocksToPlain(b.content).trimEnd() + "\n\n";
        break;
    }
  }
  return out;
}

function inlinesToPlain(inlines: MdInline[]): string {
  let out = "";
  for (const inl of inlines) {
    switch (inl.kind) {
      case "text":
      case "code":
        out += inl.text;
        break;
      case "emphasis":
      case "strong":
      case "strikethrough":
      case "link":
        out += inlinesToPlain(inl.content);
        break;
      case "image":
        out += inl.alt;
        break;
      case "footnote_ref":
        out += `[${inl.label}]`;
        break;
      case "hard_break":
        out += "\n";
        break;
    }
  }
  return out;
}

export function renderHoverDoc(blocks: MdBlock[]): DocumentFragment {
  const frag = document.createDocumentFragment();
  for (const b of blocks) frag.append(renderBlock(b));
  return frag;
}

function renderBlock(b: MdBlock): Node {
  switch (b.kind) {
    case "heading": {
      const d = document.createElement("div");
      d.className = `md-line md-heading md-h${b.level}`;
      renderInlines(b.content, d);
      return d;
    }
    case "paragraph": {
      const d = document.createElement("div");
      d.className = "md-line";
      renderInlines(b.content, d);
      return d;
    }
    case "code": {
      const pre = document.createElement("pre");
      pre.className = "md-code";
      pre.textContent = b.code;
      return pre;
    }
    case "html": {
      // Raw HTML is shown literally, never interpreted.
      const pre = document.createElement("pre");
      pre.className = "md-code md-html";
      pre.textContent = b.raw;
      return pre;
    }
    case "list": {
      const list = document.createElement(b.ordered ? "ol" : "ul");
      list.className = "md-list";
      if (b.ordered && (b.start ?? 1) !== 1) (list as HTMLOListElement).start = b.start;
      for (const item of b.items) {
        const li = document.createElement("li");
        if (item.checked !== undefined) {
          li.className = "md-task";
          li.append(document.createTextNode(item.checked ? "☑ " : "☐ "));
        }
        for (const ib of item.blocks) li.append(renderBlock(ib));
        list.append(li);
      }
      return list;
    }
    case "quote": {
      const q = document.createElement("blockquote");
      q.className = b.alert ? `md-quote md-alert md-alert-${b.alert}` : "md-quote";
      for (const cb of b.content) q.append(renderBlock(cb));
      return q;
    }
    case "rule":
      return document.createElement("hr");
    case "table": {
      const table = document.createElement("table");
      table.className = "md-table";
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
          renderInlines(cell, th);
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
          renderInlines(cell, td);
          tr.append(td);
        });
        tbody.append(tr);
      }
      table.append(tbody);
      return table;
    }
    case "image": {
      // Hover images degrade to their alt text; the reading view resolves real sources.
      const d = document.createElement("div");
      d.className = "md-line md-image-alt";
      d.textContent = `[image: ${b.alt}]`;
      return d;
    }
    case "front_matter":
      return document.createDocumentFragment(); // metadata — hover never shows it
    case "footnote_def": {
      const d = document.createElement("div");
      d.className = "md-line md-footnote-def";
      d.append(document.createTextNode(`[${b.label}]: `));
      for (const cb of b.content) d.append(renderBlock(cb));
      return d;
    }
  }
}

function renderInlines(inlines: MdInline[], parent: HTMLElement): void {
  for (const inl of inlines) parent.append(renderInline(inl));
}

function renderInline(inl: MdInline): Node {
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
      renderInlines(inl.content, em);
      return em;
    }
    case "strong": {
      const s = document.createElement("strong");
      renderInlines(inl.content, s);
      return s;
    }
    case "strikethrough": {
      const del = document.createElement("del");
      renderInlines(inl.content, del);
      return del;
    }
    case "link": {
      const a = document.createElement("a");
      a.className = "md-link";
      renderInlines(inl.content, a);
      // Only web/mail/file links get a live href (opened in a new tab); others render as plain text.
      if (/^(https?|mailto|file):/i.test(inl.href)) {
        a.href = inl.href;
        a.target = "_blank";
        a.rel = "noopener noreferrer";
      }
      return a;
    }
    case "image": {
      const s = document.createElement("span");
      s.className = "md-image-alt";
      s.textContent = `[${inl.alt}]`;
      return s;
    }
    case "footnote_ref": {
      const sup = document.createElement("sup");
      sup.className = "md-footnote-ref";
      sup.textContent = `[${inl.label}]`;
      return sup;
    }
    case "hard_break":
      return document.createElement("br");
  }
}
