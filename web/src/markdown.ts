//! Renders the hover Markdown AST (parsed in the Rust core with pulldown-cmark, see
//! `aether-client/src/markdown.rs`) to DOM. The same AST drives the native and terminal clients, so
//! there's no Markdown parser here — only a renderer. Everything goes through `textContent` /
//! `createTextNode` (never `innerHTML`), so server/LSP content can't inject markup; link hrefs are
//! scheme-checked so an LSP can't smuggle a `javascript:` URL.

/** An inline (span-level) AST node — mirrors `aether_client::markdown::Inline` (serde `kind` tag). */
export type MdInline =
  | { kind: "text"; text: string }
  | { kind: "code"; text: string }
  | { kind: "emphasis"; content: MdInline[] }
  | { kind: "strong"; content: MdInline[] }
  | { kind: "link"; href: string; content: MdInline[] };

/** A block-level AST node — mirrors `aether_client::markdown::Block`. */
export type MdBlock =
  | { kind: "heading"; level: number; content: MdInline[] }
  | { kind: "paragraph"; content: MdInline[] }
  | { kind: "code"; language: string | null; code: string }
  | { kind: "list"; ordered: boolean; items: MdBlock[][] }
  | { kind: "quote"; content: MdBlock[] }
  | { kind: "rule" };

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
      case "list":
        b.items.forEach((item, i) => {
          const marker = b.ordered ? `${i + 1}. ` : "- ";
          const pad = " ".repeat(marker.length);
          const lines = blocksToPlain(item).trimEnd().split("\n");
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
      case "link":
        out += inlinesToPlain(inl.content);
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
    case "list": {
      const list = document.createElement(b.ordered ? "ol" : "ul");
      list.className = "md-list";
      for (const item of b.items) {
        const li = document.createElement("li");
        for (const ib of item) li.append(renderBlock(ib));
        list.append(li);
      }
      return list;
    }
    case "quote": {
      const q = document.createElement("blockquote");
      q.className = "md-quote";
      for (const cb of b.content) q.append(renderBlock(cb));
      return q;
    }
    case "rule":
      return document.createElement("hr");
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
  }
}
