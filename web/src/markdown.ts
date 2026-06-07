//! Minimal, dependency-free Markdown → DOM renderer for LSP hover content. The server forwards the
//! language server's hover string verbatim (usually Markdown: a fenced code block with the signature
//! plus prose); the TUI parses it into blocks, we render a small subset here. All text goes through
//! textContent (never innerHTML), so server/LSP content can't inject markup.
//!
//! Supported: fenced code blocks (```), inline `code`, **bold**, *italic* / _italic_, # headings,
//! and --- rules. Anything else renders as plain text lines — good enough for hover popups.

function renderInline(text: string, parent: HTMLElement): void {
  const re = /(`[^`]+`|\*\*[^*]+\*\*|\*[^*]+\*|_[^_]+_)/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    if (m.index > last) parent.append(text.slice(last, m.index));
    const tok = m[0];
    if (tok.startsWith("`")) {
      const c = document.createElement("code");
      c.textContent = tok.slice(1, -1);
      parent.append(c);
    } else if (tok.startsWith("**")) {
      const b = document.createElement("strong");
      b.textContent = tok.slice(2, -2);
      parent.append(b);
    } else {
      const it = document.createElement("em");
      it.textContent = tok.slice(1, -1);
      parent.append(it);
    }
    last = m.index + tok.length;
  }
  if (last < text.length) parent.append(text.slice(last));
}

export function renderMarkdown(md: string): DocumentFragment {
  const frag = document.createDocumentFragment();
  const lines = md.replace(/\r\n/g, "\n").split("\n");
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (/^```/.test(line)) {
      i++;
      const code: string[] = [];
      while (i < lines.length && !/^```/.test(lines[i])) {
        code.push(lines[i]);
        i++;
      }
      i++; // consume the closing fence
      const pre = document.createElement("pre");
      pre.className = "md-code";
      pre.textContent = code.join("\n");
      frag.append(pre);
      continue;
    }
    const trimmed = line.trim();
    if (trimmed === "---" || trimmed === "***" || trimmed === "___") {
      frag.append(document.createElement("hr"));
      i++;
      continue;
    }
    if (trimmed === "") {
      i++;
      continue; // paragraph break — spacing comes from CSS
    }
    const div = document.createElement("div");
    div.className = "md-line";
    const heading = line.match(/^(#{1,6})\s+(.*)/);
    if (heading) {
      div.classList.add("md-heading");
      renderInline(heading[2], div);
    } else {
      renderInline(line, div);
    }
    frag.append(div);
    i++;
  }
  return frag;
}
