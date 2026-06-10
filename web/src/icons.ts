//! Status icons — inline SVG drawn with `currentColor`, so the severity/LSP colour classes
//! (`.sev-*`, `.lsp-*`) apply wherever the icon lands. Shared by the status bar, the LSP picker
//! rows, and the LSP info dialog.

export type IconKind =
  | "error"
  | "warning"
  | "info"
  | "hint"
  | "lsp-ready"
  | "lsp-busy"
  | "lsp-crashed"
  | "lsp-stopped";

const ICONS: Record<IconKind, string> = {
  error: '<circle cx="8" cy="8" r="6.25"/><path d="M5.6 5.6l4.8 4.8M10.4 5.6l-4.8 4.8"/>',
  warning:
    '<path d="M8 2.4l6.1 11H1.9z"/><path d="M8 6.7v3"/><circle cx="8" cy="11.4" r="0.75" fill="currentColor" stroke="none"/>',
  info: '<circle cx="8" cy="8" r="6.25"/><path d="M8 7.7v3.1"/><circle cx="8" cy="5.2" r="0.75" fill="currentColor" stroke="none"/>',
  hint: '<path d="M8 2.4a3.6 3.6 0 0 0-2.2 6.5c.4.3.6.7.6 1.1v.4h3.2v-.4c0-.4.2-.8.6-1.1A3.6 3.6 0 0 0 8 2.4z"/><path d="M6.8 12.6h2.4M7.3 13.9h1.4"/>',
  "lsp-ready": '<path d="M3.8 8.5l2.7 2.6L12.2 5.2"/>',
  "lsp-busy": '<path d="M8 2.7a5.3 5.3 0 1 1-5 3.6"/>',
  "lsp-crashed": '<path d="M5.2 5.2l5.6 5.6M10.8 5.2l-5.6 5.6"/>',
  "lsp-stopped": '<circle cx="8" cy="8" r="4.2"/>',
};

export function statusIcon(kind: IconKind, spin = false): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("class", spin ? "status-icon spin" : "status-icon");
  svg.innerHTML = ICONS[kind];
  return svg;
}

export type LspIconKind = Extract<IconKind, `lsp-${string}`>;

/** Icon kind / colour class for an LSP lifecycle state. starting/initializing/restarting share
 *  the busy icon — they have no dedicated kind (callers fold "ready + active progress" into
 *  busy themselves, since that needs the progress list). */
export function lspStateClass(state: string): LspIconKind {
  return state === "ready" || state === "crashed" || state === "stopped"
    ? (`lsp-${state}` as LspIconKind)
    : "lsp-busy";
}
