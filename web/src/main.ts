//! Web client entry point. Connects, activates a project, opens a buffer, subscribes a viewport,
//! and renders it; then drives modal editing by translating keyboard events into cursor/input RPCs
//! via the ported keymap (docs/web-client.md §4 — Phase 2).

import "./theme.css";
import { RpcClient, RpcError } from "./client";
import type { ConnState } from "./client";
import { renderMarkdown } from "./markdown";
import { renderBuffer } from "./render";
import { Picker, bufferStatusDot } from "./picker";
import { decodeRow } from "./text";
import { readClipboard, writeClipboard } from "./clipboard";
import { confirmDialog, saveAsDialog } from "./modal";
import { showHelp } from "./help";
import { lspStateClass, statusIcon } from "./icons";
import type { IconKind } from "./icons";
import { WOULD_DISCARD_CHANGES, WOULD_OVERWRITE } from "./protocol";
import {
  chordOf,
  lookup,
  type Action,
  type InsertWhere,
  type ScrollDir,
  type ScrollUnit,
} from "./keymap";
import type {
  BufferClosedParams,
  BufferCopyResult,
  BufferCutResult,
  BufferOpenParams,
  BufferOpenResult,
  BufferCloseResult,
  BufferReloadResult,
  BufferSaveResult,
  BufferStateParams,
  BufferWindow,
  CopyScope,
  CursorState,
  CursorUndoResult,
  DiagnosticCounts,
  DiagnosticSeverity,
  Direction,
  BlameInfo,
  CommitInfo,
  EditResult,
  GitBlameLineResult,
  GitCommitInfoResult,
  GitApplyHunkResult,
  GitNavigateHunkResult,
  LspDiagnosticsChangedParams,
  LspFormatResult,
  LspGotoDefinitionResult,
  LspHoverResult,
  LspNavigateDiagnosticResult,
  LogicalLineRender,
  LspServerRef,
  LspServerStatus,
  LogicalPosition,
  Motion,
  NavGotoParams,
  NavStepResult,
  PickerGrepNavigateTarget,
  PickerItem,
  PickerKind,
  PickerSelectResult,
  PickerUpdateParams,
  SurroundTarget,
  ProjectActivateResult,
  ProjectInfo,
  ProjectListResult,
  ProjectRemoveRootResult,
  ScrollPosition,
  SearchNavResult,
  SearchSetResult,
  BufferStatusSnapshot,
  SearchSummary,
  UndoResult,
  ViewportLinesChangedParams,
  ViewportSubscribeResult,
  ViewportWindowResult,
  WrapMode,
} from "./protocol";

const GUTTER_COLS = 1;
const TAB_WIDTH = 4;
const CONTINUATION_MARKER_WIDTH = 2;
const LINE_END_COL = 0xffffffff; // server clamps an over-long col to the line's end
const BUFFER_PAD = 8; // px of breathing room above the first line / below the last (virtual, not CSS)

type Mode = "normal" | "insert";

interface Config {
  wsBase: string;
  project: string | undefined;
}

function resolveConfig(): Config {
  // No token: the daemon authorizes by loopback Host/Origin. Served from the daemon, `location.host`
  // is the loopback origin; in Vite dev, VITE_AETHER_WS points at the daemon and the dev server's
  // own localhost origin is accepted too.
  const wsBase = import.meta.env.VITE_AETHER_WS ?? `ws://${location.host}`;
  return { wsBase, project: import.meta.env.VITE_AETHER_PROJECT };
}

interface Cell {
  w: number;
  h: number;
}

function measureCell(buffer: HTMLElement): Cell {
  const probe = document.createElement("span");
  probe.style.position = "absolute";
  probe.style.visibility = "hidden";
  probe.style.whiteSpace = "pre";
  probe.textContent = "M".repeat(50);
  buffer.appendChild(probe);
  const rect = probe.getBoundingClientRect();
  probe.remove();
  return { w: rect.width / 50 || 8, h: rect.height || 18 };
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, v));
}
/** Capitalized severity word for the diagnostic hover (matches the terminal client). */
function severityLabel(s: DiagnosticSeverity): string {
  switch (s) {
    case "error":
      return "Error";
    case "warning":
      return "Warning";
    case "information":
      return "Info";
    case "hint":
      return "Hint";
  }
}
function basename(path: string): string {
  const i = path.lastIndexOf("/");
  return i >= 0 ? path.slice(i + 1) : path;
}
function dirname(path: string): string {
  const i = path.lastIndexOf("/");
  return i > 0 ? path.slice(0, i) : "/";
}
function joinPath(dir: string, name: string): string {
  if (!dir) return name;
  return dir.endsWith("/") ? dir + name : `${dir}/${name}`;
}

// ---- project-relative path labels (mirrors the TUI's labels.rs + project_relative_label) --------
// The status bar shows a buffer's path relative to its project root, prefixed by a disambiguating
// root label when the project has several roots that would otherwise read the same.

/** One label per root, aligned by index. Single root → "" (no prefix needed). Roots that share a
 *  basename grow a parenthesized parent (`api (work)`), deepening until unique. */
function rootLabels(paths: string[]): string[] {
  const n = paths.length;
  if (n === 0) return [];
  if (n === 1) return [""];
  const MAX_DEPTH = 16;
  const depths = new Array<number>(n).fill(0);
  const labels = new Array<string>(n).fill("");
  for (let pass = 0; pass <= MAX_DEPTH; pass++) {
    for (let i = 0; i < n; i++) labels[i] = labelAtDepth(paths[i], depths[i]);
    const byLabel = new Map<string, number[]>();
    labels.forEach((l, i) => {
      const arr = byLabel.get(l);
      if (arr) arr.push(i);
      else byLabel.set(l, [i]);
    });
    const collisions = [...byLabel.values()].filter((idxs) => idxs.length > 1);
    if (collisions.length === 0) return labels;
    let bumped = false;
    for (const idxs of collisions)
      for (const i of idxs)
        if (depths[i] < MAX_DEPTH) {
          depths[i]++;
          bumped = true;
        }
    if (!bumped) return labels; // every colliding entry maxed out — accept the duplicates
  }
  return labels;
}

/** `{basename}` at depth 0, else `{basename} ({parent…/})` with `depth` parent components. */
function labelAtDepth(path: string, depth: number): string {
  const comps = path.split("/").filter((c) => c.length > 0);
  const base = comps.length ? comps[comps.length - 1] : path;
  if (depth === 0) return base;
  const parents: string[] = [];
  for (let i = comps.length - 2; i >= 0 && parents.length < depth; i--) parents.push(comps[i]);
  if (parents.length === 0) return base;
  parents.reverse();
  return `${base} (${parents.join("/")})`;
}

/** Strip the longest matching project root off `abs` → `[rootIndex, relativePath]`, or null when
 *  the path is under no root. Component-aware (a trailing-slash boundary), like the TUI. */
function stripLongestRoot(abs: string, paths: string[]): [number, string] | null {
  let best: [number, string] | null = null;
  let bestLen = -1;
  paths.forEach((root, i) => {
    if (abs === root) {
      if (root.length > bestLen) ((best = [i, ""]), (bestLen = root.length));
      return;
    }
    const prefix = root.endsWith("/") ? root : root + "/";
    if (abs.startsWith(prefix) && root.length > bestLen) {
      best = [i, abs.slice(prefix.length)];
      bestLen = root.length;
    }
  });
  return best;
}

/** Display label for a buffer's absolute path: `{rootLabel}: {relative}` (label omitted when empty,
 *  i.e. a single-root project), or the raw absolute path when it's under no project root. */
function projectRelativeLabel(abs: string, paths: string[]): string {
  const stripped = stripLongestRoot(abs, paths);
  if (!stripped) return abs;
  const [i, rel] = stripped;
  const label = rootLabels(paths)[i] ?? "";
  if (rel === "") return label;
  if (label === "") return rel;
  return `${label}: ${rel}`;
}

function lspKey(language: string, workspaceRoot: string): string {
  return `${language}\0${workspaceRoot}`;
}
function minPos(a: LogicalPosition, b: LogicalPosition): LogicalPosition {
  return a.line < b.line || (a.line === b.line && a.col <= b.col) ? a : b;
}
function maxPos(a: LogicalPosition, b: LogicalPosition): LogicalPosition {
  return a.line > b.line || (a.line === b.line && a.col >= b.col) ? a : b;
}

/** Actions `r` replays — cursor/selection motions only (mirrors keymap.rs is_repeatable). */
function isRepeatable(a: Action): boolean {
  switch (a.t) {
    case "moveChar":
    case "moveWord":
    case "moveWordEnd":
    case "moveVisualLine":
    case "moveLogicalLine":
    case "moveLineStart":
    case "moveLineEnd":
    case "moveLineFirstNonblank":
    case "gotoLine":
    case "matchBracket":
    case "pageMotion":
    case "navUnit":
    case "navUnitEdge":
    case "selectLine":
    case "treeExpand":
    case "treeContract":
    case "searchCycle":
      return true;
    default:
      return false;
  }
}

/** Escape regex metacharacters so a literal selection searches as text (mirrors app.rs regex_escape). */
function regexEscape(s: string): string {
  return s.replace(/[\\.+*?()|[\]{}^$#&\-~]/g, "\\$&");
}

/** EOL blame text: "You · Uncommitted" or "author · <relative time>" (mirrors ui.rs). The commit
 * message lives in the `Space o` details popover, not inline. */
function formatBlame(info: BlameInfo): string {
  if (info.is_uncommitted) return "You · Uncommitted";
  return `${info.author} · ${relativeTime(info.timestamp)}`;
}

/** Commit-details popover body, mirroring `git show`'s header (app.rs `format_commit_details`). */
function formatCommitDetails(info: CommitInfo): string {
  return `commit ${info.commit}\nAuthor: ${info.author} <${info.email}>\nDate:   ${info.date}\n\n${info.message}`;
}
function relativeTime(unixSeconds: number): string {
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - unixSeconds);
  const units: [number, string][] = [
    [31536000, "y"],
    [2592000, "mo"],
    [604800, "w"],
    [86400, "d"],
    [3600, "h"],
    [60, "m"],
  ];
  for (const [size, label] of units) {
    if (secs >= size) return `${Math.floor(secs / size)}${label} ago`;
  }
  return "just now";
}

// Status-bar icons live in icons.ts (shared with the LSP picker rows and info dialog).

interface PendingFind {
  dir: Direction;
  till: boolean;
  count: number;
  extend: boolean;
}

class Editor {
  private bufferEl: HTMLElement;
  /** Offscreen focused textarea: captures native paste events so Ctrl-V reads the clipboard via
   *  `clipboardData` (no Firefox readText prompt). Holds focus whenever the buffer is "active". */
  private clipboardCapture: HTMLTextAreaElement;
  private statusEl: HTMLElement;
  private client: RpcClient;
  private cell: Cell;

  private viewportId = 0;
  private bufferId = 0;
  private window: BufferWindow | null = null;
  private cursor: CursorState = { position: { line: 0, col: 0 }, anchor: { line: 0, col: 0 } };
  private scroll: ScrollPosition = { logical_line: 0, sub_row: 0 };
  private fetchInFlight = false; // guards native-scroll window fetches from piling up
  private wrap: WrapMode = "soft";
  private label = "";
  private currentPath: string | null = null;
  private scratchNumber: number | null = null; // for the URL when the buffer has no path
  private projectName = "";
  private projectPaths: string[] = [];
  private cols = 80;
  private rows = 24;
  private resizeTimer: number | undefined;
  /** Debounce for keeping the URL fragment (cursor/selection) live during normal-mode movement. */
  private historyTimer: number | undefined;

  // Dirty / external-change tracking (from buffer/open + edit results + buffer/state).
  private revision = 0;
  private savedRevision = 0;
  private externallyModified = false;
  private externallyDeleted = false;
  private diffView = false;
  // LSP: this buffer's server key, statuses by (language, workspace_root), and diagnostic counts.
  private lspServerRef: LspServerRef | null = null;
  private lspStatuses = new Map<string, LspServerStatus>();
  private diagCounts: DiagnosticCounts | null = null;

  private mode: Mode = "normal";
  private pendingCount = 0;
  private pendingFind: PendingFind | null = null;
  private pendingSurround: SurroundTarget | null = null;
  private pendingLeader = false;
  private lastRepeat: { kind: "action"; action: Action; count: number } | { kind: "find"; motion: Motion } | null = null;
  private picker: Picker | null = null;
  private hoverEl: HTMLElement;
  private hoverOpen = false;
  private faviconEl: HTMLLinkElement; // tab icon: "ae" mark when clean, a state-coloured dot when dirty
  private faviconDark = window.matchMedia("(prefers-color-scheme: dark)"); // tab theme → glyph ink
  private faviconKey = ""; // last-applied (state, theme) key, so we only rewrite the <link> on change
  private toastsEl: HTMLElement;
  private connBanner: HTMLElement; // shown while disconnected/reconnecting
  private disconnected = false; // suspend editing input while the socket is down
  private modalOpen = false; // a confirm/save-as dialog owns the keyboard while true
  private composing = false; // true between compositionstart/end (IME / dead keys)
  // Cursor-line git blame (EOL text), keyed by (line, revision); fetched lazily in Normal mode.
  private blame: { line: number; rev: number; text: string | null } | null = null;
  private blameInflight: string | null = null;

  // Search state.
  private searchBar: HTMLElement;
  private searchInput: HTMLInputElement;
  private searchCountEl: HTMLElement;
  private searchOpen = false;
  private searchCommitted = false; // a search is active in Normal mode (n / Alt-n work)
  private committedQuery = "";
  private extendToCursor = false;
  private searchSummary: SearchSummary | null = null;
  private searchSnapshot: { cursor: CursorState; scrollTop: number; query: string; active: boolean } | null = null;
  private searchHistory: string[] = [];
  private historyIndex: number | null = null;
  private historyDraft = "";

  // Mouse drag-select.
  private dragging = false;
  private dragAnchor: LogicalPosition | null = null;
  /** Serializes async key handling so rapid keystrokes don't race on shared cursor/window state. */
  private queue: Promise<void> = Promise.resolve();

  constructor(root: HTMLElement, cfg: Config) {
    this.bufferEl = document.createElement("div");
    this.bufferEl.id = "buffer";

    this.clipboardCapture = document.createElement("textarea");
    this.clipboardCapture.className = "clipboard-capture";
    this.clipboardCapture.tabIndex = -1;
    this.clipboardCapture.setAttribute("aria-hidden", "true");
    this.clipboardCapture.addEventListener("paste", (e) => this.onPaste(e));
    // Text input flows through this textarea so the OS can compose CJK / dead-key / accented input;
    // we insert the resulting text on `input` (and the final string on compositionend). Keydown
    // handles only non-text keys (motions, modal, Ctrl-chords) — see classify's insert branch.
    this.clipboardCapture.addEventListener("compositionstart", () => (this.composing = true));
    this.clipboardCapture.addEventListener("compositionend", () => {
      this.composing = false;
      this.flushTypedText();
    });
    this.clipboardCapture.addEventListener("input", () => {
      if (!this.composing) this.flushTypedText();
    });

    // Search bar (hidden until `/`): query input + match count, above the status row.
    this.searchBar = document.createElement("div");
    this.searchBar.id = "searchbar";
    this.searchBar.style.display = "none";
    this.searchInput = document.createElement("input");
    this.searchInput.className = "search-input";
    this.searchInput.spellcheck = false;
    this.searchInput.autocomplete = "off";
    this.searchCountEl = document.createElement("span");
    this.searchCountEl.className = "search-count";
    this.searchBar.append(this.searchInput, this.searchCountEl);

    this.statusEl = document.createElement("div");
    this.statusEl.id = "status";
    this.hoverEl = document.createElement("div");
    this.hoverEl.id = "hover";
    this.hoverEl.style.display = "none";
    this.faviconEl = document.createElement("link");
    this.faviconEl.rel = "icon";
    document.head.appendChild(this.faviconEl);
    // The clean "ae" mark is monochrome on a transparent icon, so it must flip with the tab theme.
    this.faviconDark.addEventListener("change", () => this.updateFavicon());
    this.toastsEl = document.createElement("div");
    this.toastsEl.id = "toasts";
    this.toastsEl.setAttribute("role", "status");
    this.toastsEl.setAttribute("aria-live", "polite");
    this.connBanner = document.createElement("div");
    this.connBanner.id = "conn-banner";
    this.connBanner.setAttribute("role", "status");
    this.connBanner.setAttribute("aria-live", "polite");
    this.connBanner.style.display = "none";
    root.append(
      this.bufferEl,
      this.clipboardCapture,
      this.searchBar,
      this.statusEl,
      this.hoverEl,
      this.toastsEl,
      this.connBanner,
    );

    this.searchInput.addEventListener("input", () => {
      this.historyIndex = null; // typing diverges from the browsed history
      this.enqueue(() => this.runIncrementalSearch());
    });
    this.searchInput.addEventListener("keydown", (e) => this.onSearchKey(e));

    this.cell = measureCell(this.bufferEl);

    const url = `${cfg.wsBase}/?client_version=web-0.2`;
    this.client = new RpcClient(url, (method, params) => this.onNotification(method, params), {
      onConnState: (s) => this.setConnState(s),
      onReconnect: () => void this.reestablish(),
    });

    this.bufferEl.addEventListener("scroll", () => this.onScroll(), { passive: true });
    this.bufferEl.addEventListener("mousedown", (e) => this.onMouseDown(e));
    window.addEventListener("mousemove", (e) => this.onMouseMove(e));
    window.addEventListener("mouseup", () => (this.dragging = false));
    window.addEventListener("resize", () => this.onResize());
    window.addEventListener("keydown", (e) => this.onKeyDown(e));
    // Native browser back/forward (Alt-←/→, toolbar buttons, mouse side-buttons, trackpad swipe)
    // surface here — we freed Alt-←/→ in the keymap so the browser handles them. Each entry carries
    // its location in `history.state`, so this also restores cursor/selection on reload-safe nav.
    window.addEventListener("popstate", (e) => this.onPopState(e));

    void this.bootstrap(cfg);
  }

  /** Transient messages are toasts (the status bar shows persistent state only). Kept as a thin
   *  wrapper so existing callers route to a toast. */
  private setStatus(text: string, isError = false): void {
    this.toast(text, isError ? "error" : "info");
  }

  private toast(message: string, kind: "info" | "error" | "warning" | "success" = "info"): void {
    const t = document.createElement("div");
    t.className = `toast ${kind}`;
    t.textContent = message;
    this.toastsEl.append(t);
    window.setTimeout(() => t.classList.add("fade"), 3000);
    window.setTimeout(() => t.remove(), 3600);
  }

  private async bootstrap(cfg: Config): Promise<void> {
    try {
      await this.client.ready;

      const list = await this.client.rpc<ProjectListResult>("project/list", {});
      // The URL drives which project + buffer to open (so the tab is reloadable/shareable); fall
      // back to the configured/first project, and to the project's last/scratch buffer.
      const sp = new URLSearchParams(location.search);
      const urlProject = sp.get("project");
      const urlFile = sp.get("file");
      const urlRoot = Number(sp.get("root")) || 0;
      const urlBufferRaw = sp.get("buffer");
      const urlBuffer =
        urlBufferRaw != null && Number.isInteger(Number(urlBufferRaw)) ? Number(urlBufferRaw) : null;
      const known = list.projects.some((p) => p.name === urlProject);
      const name = (known ? urlProject : null) ?? cfg.project ?? list.projects[0]?.name;
      if (!name) {
        this.setStatus("No projects configured on the server.", true);
        return;
      }
      const activated = await this.client.rpc<ProjectActivateResult>("project/activate", { name });
      this.projectName = activated.project.name;
      this.projectPaths = activated.project.paths;

      const lastOrScratch = { buffer_id: activated.last_buffer_id ?? null, create_if_missing: false };
      let open: BufferOpenResult;
      if (urlFile) {
        try {
          open = await this.client.rpc<BufferOpenResult>("buffer/open", {
            path_index: urlRoot,
            relative_path: urlFile,
            create_if_missing: false,
          });
        } catch {
          this.toast(`could not open ${urlFile}`, "warning");
          open = await this.client.rpc<BufferOpenResult>("buffer/open", lastOrScratch);
        }
      } else if (urlBuffer != null) {
        // A scratch-buffer link (`?buffer=<id>`). The id is session-scoped: if the buffer was closed
        // or the server restarted it no longer exists, so fall back to the project's last/scratch.
        try {
          open = await this.client.rpc<BufferOpenResult>("buffer/open", {
            buffer_id: urlBuffer,
            create_if_missing: false,
          });
        } catch {
          open = await this.client.rpc<BufferOpenResult>("buffer/open", lastOrScratch);
        }
      } else {
        open = await this.client.rpc<BufferOpenResult>("buffer/open", lastOrScratch);
      }
      this.adoptOpenedBuffer(open);
      this.scroll = open.scroll ?? { logical_line: 0, sub_row: 0 };

      this.recomputeGrid();
      const sub = await this.client.rpc<ViewportSubscribeResult>("viewport/subscribe", {
        buffer_id: this.bufferId,
        cols: this.cols,
        rows: this.rows,
        overscan_rows: this.rows,
        scroll: this.scroll,
        wrap: this.wrap,
        continuation_marker_width: CONTINUATION_MARKER_WIDTH,
        tab_width: TAB_WIDTH,
      });
      this.viewportId = sub.viewport_id;
      this.window = sub.window;
      this.seedBufferStatus(sub.buffer_status);
      await this.reapplyDiffView();
      // Restore the cursor/selection from the URL fragment (a reloaded/shared link), clamped server-
      // side. Best-effort — a stale fragment just leaves the cursor at the server's default.
      const frag = this.parseFragment(location.hash);
      if (frag) {
        try {
          this.cursor = await this.client.rpc<CursorState>("cursor/set", {
            buffer_id: this.bufferId,
            position: frag.position,
            anchor: frag.anchor,
          });
        } catch {
          /* out of range after an external change — keep the default */
        }
      }
      this.clipboardCapture.focus();
      this.render();
      this.revealCursor();
      this.replaceHistory(); // seed the initial history entry with our location (for nav restore)
    } catch (err) {
      this.setStatus(`Connection error: ${(err as Error).message}`, true);
    }
  }

  /** Reflect the socket's connection state: banner + suspend editing while down. */
  private setConnState(state: ConnState): void {
    this.disconnected = state !== "connected";
    if (state === "connected") {
      this.connBanner.style.display = "none";
      return;
    }
    this.connBanner.className = state === "failed" ? "failed" : "";
    this.connBanner.replaceChildren();
    const label = document.createElement("span");
    label.textContent =
      state === "failed" ? "Disconnected" : state === "connecting" ? "Connecting…" : "Reconnecting…";
    this.connBanner.append(label);
    if (state === "failed") {
      const retry = document.createElement("button");
      retry.className = "conn-retry";
      retry.textContent = "Retry";
      retry.addEventListener("click", () => this.client.retry());
      this.connBanner.append(retry);
    }
    this.connBanner.style.display = "flex";
  }

  /** Re-establish per-client state after a reconnect (fresh client_id): re-activate the project,
   *  reopen the current buffer by path (recovering its content + unsaved edits if the server stayed
   *  up), re-subscribe the viewport, and restore the cursor + scroll. Undo history is not
   *  recoverable (it was server-side, keyed by the old client_id). */
  private async reestablish(): Promise<void> {
    this.queue = Promise.resolve(); // drop the rejected in-flight chain from the disconnect
    const savedCursor = this.cursor;
    try {
      await this.client.rpc<ProjectActivateResult>("project/activate", { name: this.projectName });
      const params = this.currentPath ? this.resolvePath(this.currentPath) : null;
      let open: BufferOpenResult;
      try {
        open = await this.client.rpc<BufferOpenResult>(
          "buffer/open",
          params ?? { buffer_id: this.bufferId, create_if_missing: false },
        );
      } catch {
        // The buffer is gone (server restarted) — fall back to a scratch buffer.
        open = await this.client.rpc<BufferOpenResult>("buffer/open", { create_if_missing: false });
      }
      this.adoptOpenedBuffer(open);
      this.recomputeGrid();
      const sub = await this.client.rpc<ViewportSubscribeResult>("viewport/subscribe", {
        buffer_id: this.bufferId,
        cols: this.cols,
        rows: this.rows,
        overscan_rows: this.rows,
        // Load the window around the saved cursor line (not line 0) so revealCursor doesn't leave a
        // blank gap while it refetches a deep scroll position.
        scroll: { logical_line: savedCursor.position.line, sub_row: 0 },
        wrap: this.wrap,
        continuation_marker_width: CONTINUATION_MARKER_WIDTH,
        tab_width: TAB_WIDTH,
      });
      this.viewportId = sub.viewport_id;
      this.window = sub.window;
      this.seedBufferStatus(sub.buffer_status);
      await this.reapplyDiffView();
      try {
        this.cursor = await this.client.rpc<CursorState>("cursor/set", {
          buffer_id: this.bufferId,
          position: savedCursor.position,
          anchor: savedCursor.anchor,
        });
      } catch {
        /* position may be out of range if the file changed on disk — keep the server's default */
      }
      this.clipboardCapture.focus();
      this.render();
      this.revealCursor();
      this.replaceHistory(); // refresh the current entry (fresh client_id; no new history entry)
      this.toast("Reconnected", "success");
    } catch (e) {
      this.toast(`Reconnect failed: ${(e as Error).message}`, "error");
    }
  }

  private recomputeGrid(): void {
    this.cols = Math.max(1, Math.floor(this.bufferEl.clientWidth / this.cell.w) - GUTTER_COLS);
    this.rows = Math.max(1, Math.floor(this.bufferEl.clientHeight / this.cell.h));
  }

  /** Mid-chord, waiting for the next keystroke to land (find target, leader, surround delimiter, or a
   *  partially-typed count). Drives the underscore cursor, mirroring the terminal's `awaiting_key`. */
  private get awaitingKey(): boolean {
    return (
      this.pendingLeader ||
      this.pendingFind !== null ||
      this.pendingSurround !== null ||
      this.pendingCount > 0
    );
  }

  private render(): void {
    if (!this.window) return;
    this.bufferEl.classList.toggle("hscroll", this.wrap === "none"); // enable native horizontal scroll
    const blame =
      this.mode === "normal" && this.blame && this.blame.line === this.cursor.position.line
        ? this.blame.text
        : null;
    renderBuffer(this.bufferEl, {
      window: this.window,
      cursor: this.cursor,
      insertMode: this.mode === "insert",
      awaitingKey: this.awaitingKey,
      contentWidthPx: this.wrap === "none" ? this.cell.w * (this.window.max_line_width + 2) : 0,
      spacerHeightPx: this.window.total_visual_rows * this.cell.h + BUFFER_PAD * 2,
      contentTopPx: this.window.first_visual_row * this.cell.h + BUFFER_PAD,
      blame,
      diffView: this.diffView,
    });
    this.renderStatusBar();
    this.maybeRefreshBlame();
    // Keep the URL fragment tracking the cursor in Normal mode (debounced). Skipped in Insert —
    // those positions aren't navigation targets and per-keystroke writes would hit browser limits.
    if (this.mode === "normal") this.scheduleHistoryUpdate();
  }

  /** Visual rows the loaded window occupies (real + diff phantom). */
  private loadedVisualRows(): number {
    if (!this.window) return 0;
    let rows = 0;
    for (const line of this.window.lines) rows += (line.virtual_rows_above?.length ?? 0) + line.visual_rows.length;
    return rows;
  }

  /** Visible viewport height in visual rows. */
  private visibleRows(): number {
    return Math.max(1, Math.floor(this.bufferEl.clientHeight / this.cell.h));
  }

  /** Visual-row index of the cursor within its logical line (which wrapped row the cursor sits on). */
  private cursorVisualRow(line: LogicalLineRender): number {
    let idx = 0;
    for (let i = 0; i < line.visual_rows.length; i++) {
      if (line.visual_rows[i].byte_offset <= this.cursor.position.col) idx = i;
    }
    return idx;
  }

  /** Absolute visual-row index of the cursor in the whole document, or null if its line isn't
   *  in the loaded window. */
  private cursorAbsoluteVisualRow(): number | null {
    if (!this.window) return null;
    const cl = this.cursor.position.line;
    if (cl < this.window.first_logical_line || cl >= this.window.last_logical_line_exclusive) return null;
    let row = this.window.first_visual_row;
    for (const line of this.window.lines) {
      const above = line.virtual_rows_above?.length ?? 0;
      if (line.logical_line === cl) return row + above + this.cursorVisualRow(line);
      row += above + line.visual_rows.length;
    }
    return null;
  }

  /** Fetch blame for the cursor line (Normal mode) when it or the revision changes; re-renders. */
  private maybeRefreshBlame(): void {
    if (this.mode !== "normal") return;
    const line = this.cursor.position.line;
    const rev = this.revision;
    if (this.blame && this.blame.line === line && this.blame.rev === rev) return;
    const key = `${line}:${rev}`;
    if (this.blameInflight === key) return;
    this.blameInflight = key;
    this.queue = this.queue
      .then(async () => {
        const r = await this.client.rpc<GitBlameLineResult>("git/blame_line", {
          buffer_id: this.bufferId,
          line,
        });
        this.blame = { line, rev, text: r.blame ? formatBlame(r.blame) : null };
        this.blameInflight = null;
        this.render();
      })
      .catch(() => {
        this.blameInflight = null;
      });
  }

  /** Status bar matching the TUI: left = `[project] file •` (colour-coded dirty dot) + colored
   *  diagnostic counts; right =
   *  search counter, cursor/selection position, and the LSP status glyph. Mode is shown by the
   *  cursor shape (block vs caret), not text. */
  /** Seed buffer-level status from a viewport/subscribe snapshot, so the status bar is correct the
   *  moment a buffer is shown — not only after the next change-notification. Live updates still
   *  arrive via buffer/state, lsp/diagnostics_changed, and lsp/status_changed. */
  private seedBufferStatus(status: BufferStatusSnapshot | undefined): void {
    if (!status) return;
    this.externallyModified = status.externally_modified ?? false;
    this.externallyDeleted = status.externally_deleted ?? false;
    this.diagCounts = status.diagnostics ?? null;
    if (status.lsp_status) {
      this.lspStatuses.set(
        lspKey(status.lsp_status.language, status.lsp_status.workspace_root),
        status.lsp_status,
      );
    }
  }

  /** Re-apply the sticky inline-diff toggle after a (re)subscribe: a fresh viewport starts with it
   *  off server-side, so if the user had it on we turn it back on and use the re-rendered window.
   *  Runs before the next paint, so there's no flash. Mirrors how the terminal client carries it. */
  private async reapplyDiffView(): Promise<void> {
    if (!this.diffView) return;
    const r = await this.client.rpc<ViewportWindowResult>("git/set_diff_view", {
      viewport_id: this.viewportId,
      enabled: true,
    });
    this.window = r.window;
  }

  private renderStatusBar(): void {
    const left = document.createElement("span");
    left.className = "status-left";
    // Buffer-state dot leads the row, before the project name — matching the terminal title and
    // the tab favicon.
    const dot = this.statusDot();
    if (dot) left.append(dot);
    const proj = this.projectName ? `[${this.projectName}] ` : "";
    left.append(`${proj}${this.label}`);
    // Git cluster next to the file label (tracked files only): `⎇  branch  [base]  +u(s) ~u(s)`,
    // where each per-class count combines the unstaged count with the staged count in parens (each
    // omitted when zero). (Diagnostics live on the right.)
    const gs = this.window?.git_status;
    if (gs) {
      if (gs.branch) {
        const b = document.createElement("span");
        b.className = "status-git git-branch";
        b.append(`⎇  ${gs.branch}`);
        left.append(b);
      }
      // Combined per-class counts: unstaged then `(staged)`.
      const u = gs.unstaged;
      const s = gs.staged;
      for (const [sigil, cls, un, st] of [
        ["+", "git-added", u?.added ?? 0, s?.added ?? 0],
        ["~", "git-modified", u?.modified ?? 0, s?.modified ?? 0],
        ["-", "git-deleted", u?.deleted ?? 0, s?.deleted ?? 0],
      ] as [string, string, number, number][]) {
        if (!un && !st) continue;
        let tok = sigil;
        if (un > 0) tok += String(un);
        if (st > 0) tok += `(${st})`;
        const el = document.createElement("span");
        el.className = `status-git ${cls}`;
        el.append(tok);
        left.append(el);
      }
    }

    const right = document.createElement("span");
    right.className = "status-right";
    if (this.searchCommitted) {
      const c = document.createElement("span");
      c.textContent = this.searchCountLabel();
      right.append(c);
    }
    const grep = this.cursor.grep_position;
    if (grep) {
      const g = document.createElement("span");
      g.textContent = `grep ${grep.current}/${grep.total}`;
      right.append(g);
    }
    // Diagnostic counts, grouped as one flex item so they stay tight, just left of the position.
    if (this.diagCounts) {
      const groups: [number, IconKind, string][] = [
        [this.diagCounts.errors, "error", "sev-error"],
        [this.diagCounts.warnings, "warning", "sev-warning"],
        [this.diagCounts.infos, "info", "sev-information"],
        [this.diagCounts.hints, "hint", "sev-hint"],
      ];
      const diagGroup = document.createElement("span");
      diagGroup.className = "status-diag-group";
      for (const [n, kind, cls] of groups) {
        if (n > 0) {
          const s = document.createElement("span");
          s.className = cls;
          s.append(statusIcon(kind), ` ${n}`);
          diagGroup.append(s);
        }
      }
      if (diagGroup.childElementCount > 0) right.append(diagGroup);
    }
    const pos = document.createElement("span");
    pos.textContent = this.positionLabel();
    right.append(pos);
    const lsp = this.lspIcon();
    if (lsp) {
      const g = document.createElement("span");
      g.className = lsp.cls;
      g.append(statusIcon(lsp.kind, lsp.kind === "lsp-busy"));
      right.append(g);
    }

    this.statusEl.replaceChildren(left, right);
    document.title = this.windowTitle();
    this.updateFavicon();
  }

  /** Buffer-state dot for the on-screen status bar, placed just after the file label and colour-
   *  coded to match the tab favicon (see `bufferStateColor`). `null` when the buffer is clean. The
   *  browser-tab equivalent of this state is the favicon — see `updateFavicon`. */
  private statusDot(): SVGSVGElement | null {
    const color = this.bufferStateColor();
    if (!color) return null;
    const dot = bufferStatusDot();
    dot.classList.add("status-dot");
    dot.style.color = color;
    return dot;
  }

  /** Buffer-state accent colour for the tab favicon and the status-bar dot (Nord palette), in
   *  precedence order: deleted-on-disk → changed-on-disk → unsaved edits. `null` when clean — a
   *  clean buffer shows the plain "ae" app mark / no dot. */
  private bufferStateColor(): string | null {
    if (this.externallyDeleted) return "#bf616a"; // aurora red — gone on disk
    if (this.externallyModified) return "#d08770"; // aurora orange — changed on disk
    if (this.revision !== this.savedRevision) return "#81a1c1"; // frost blue — unsaved edits
    return null; // clean — nothing to flag
  }

  /** Point the tab favicon (transparent background) at either the "ae" app mark when the buffer is
   *  clean or a bold state-coloured dot when it's dirty — distinct shapes, not just a colour swap.
   *  The clean mark is monochrome, so its ink follows the tab's light/dark theme. Skipped when the
   *  (state, theme) pair is unchanged, since this runs on every status-bar render. */
  private updateFavicon(): void {
    const color = this.bufferStateColor();
    const dark = this.faviconDark.matches;
    const key = `${color ?? "ae"}:${dark ? "d" : "l"}`;
    if (key === this.faviconKey) return;
    this.faviconKey = key;
    const mark = color
      ? `<circle cx="16" cy="16" r="8" fill="${color}"/>`
      : // The "æ" app mark as vector text (crisp at favicon size, unlike a hairline glyph path),
        // inked light on a dark tab / dark on a light tab so it reads against the browser chrome.
        // `&#230;` is the ASCII entity for æ, so the data URI stays pure-ASCII through encoding.
        `<text x="16" y="15" text-anchor="middle" dominant-baseline="central" ` +
        `font-family="system-ui,-apple-system,'Segoe UI',Roboto,'Helvetica Neue',Arial,sans-serif" ` +
        `font-weight="400" font-size="32" fill="${dark ? "#d8dee9" : "#2e3440"}">&#230;</text>`;
    const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">${mark}</svg>`;
    this.faviconEl.href = `data:image/svg+xml,${encodeURIComponent(svg)}`;
  }

  /** The window title, mirroring the TUI's terminal title minus the dirty marker (the favicon shows
   *  that in the tab): `[project] file`, falling back to the project alone or `Aether`. */
  private windowTitle(): string {
    if (!this.projectName) return "Aether";
    if (!this.label) return `[${this.projectName}]`;
    return `[${this.projectName}] ${this.label}`;
  }

  /** Cursor `line:col`, or the selection span in Normal mode (1-based). */
  private positionLabel(): string {
    const p = this.cursor.position;
    const a = this.cursor.anchor;
    if (this.mode === "insert" || (p.line === a.line && p.col === a.col)) {
      return `${p.line + 1}:${p.col + 1}`;
    }
    const lo = minPos(p, a);
    const hi = maxPos(p, a);
    return lo.line === hi.line
      ? `${lo.line + 1}:${lo.col + 1}-${hi.col + 1}`
      : `${lo.line + 1}:${lo.col + 1}-${hi.line + 1}:${hi.col + 1}`;
  }

  private lspIcon(): { kind: IconKind; cls: string } | null {
    if (!this.lspServerRef) return null;
    const s = this.lspStatuses.get(lspKey(this.lspServerRef.language, this.lspServerRef.workspace_root));
    if (!s) return null;
    // Background work in flight (indexing, cargo check) → busy spinner, like the TUI.
    const busy = s.status.state === "ready" && (s.progress?.length ?? 0) > 0;
    const kind = busy ? "lsp-busy" : lspStateClass(s.status.state);
    return { kind, cls: kind };
  }

  // ---- keyboard -------------------------------------------------------------------------------

  private onKeyDown(e: KeyboardEvent): void {
    if (!this.window) return;
    if (this.disconnected) return; // input is suspended while the socket is down (banner shown)
    if (this.picker || this.searchOpen || this.modalOpen) return; // overlay owns the keyboard
    if (this.hoverOpen) {
      this.dismissHover();
      if (e.key === "Escape") {
        e.preventDefault();
        return;
      }
    }
    const wasAwaiting = this.awaitingKey;
    const decision = this.classify(e);
    if (decision.handled) e.preventDefault();
    // Keep focus on the capture textarea (Tab would otherwise move it, breaking the next paste).
    else if (e.key === "Tab") e.preventDefault();
    // Arming or clearing a chord (f/t, Space, surround, count) is pure state with no RPC, so nothing
    // else would repaint the cursor — refresh here whenever the underscore-cursor state flips.
    if (this.awaitingKey !== wasAwaiting) this.render();
    if (decision.run) (decision.noSync ? this.enqueueNoSync(decision.run) : this.enqueue(decision.run));
  }

  /** Run an async action, then re-sync the buffer view; serialized so keystrokes don't race. */
  private enqueue(fn: () => Promise<void>): void {
    this.queue = this.queue
      .then(fn)
      .then(() => this.ensureCursorVisible())
      .catch((err) => this.setStatus(`Error: ${(err as Error).message}`, true));
  }

  /** Like enqueue but without cursor-follow syncView — for view-only actions (scroll). */
  private enqueueNoSync(fn: () => Promise<void>): void {
    this.queue = this.queue.then(fn).catch((err) => this.setStatus(`Error: ${(err as Error).message}`, true));
  }

  /** Synchronous half: the capture state machine (counts, find/leader arming) mutates here so the
   *  next event sees it immediately; anything needing an RPC is returned as a thunk to be queued. */
  private classify(e: KeyboardEvent): { handled: boolean; run?: () => Promise<void>; noSync?: boolean } {
    const rawKey = e.key;
    const c = chordOf(e);

    // A lone modifier keydown (Shift/Ctrl/Alt/Meta fire their own events in the browser) must not
    // consume a pending capture (find target, surround delimiter, leader) or do anything — the real
    // chord arrives on the next keydown. Without this, `Space`→`Alt`→`x` eats the leader on `Alt`,
    // and `Ctrl-s`→`Shift`→`{` eats the surround capture on `Shift` (only unshifted delims worked).
    // handled:true (no run, pending state untouched) also suppresses Firefox's Alt-menu flash.
    if (rawKey === "Shift" || rawKey === "Control" || rawKey === "Alt" || rawKey === "Meta") {
      return { handled: true };
    }

    // Pending surround delimiter (both modes): next key is the literal delimiter char.
    if (this.pendingSurround !== null) {
      const target = this.pendingSurround;
      this.pendingSurround = null;
      if (rawKey.length === 1) return { handled: true, run: () => this.surround(rawKey, target) };
      return { handled: true };
    }

    // Pending find target (Normal): next key is the literal char, case-sensitive.
    if (this.mode === "normal" && this.pendingFind) {
      const pf = this.pendingFind;
      this.pendingFind = null;
      if (rawKey.length === 1) {
        const motion: Motion = { kind: "find_char", ch: rawKey, direction: pf.dir, count: pf.count, till: pf.till };
        this.lastRepeat = { kind: "find", motion };
        return { handled: true, run: () => this.moveMotion(motion, pf.extend) };
      }
      return { handled: true };
    }

    // Pending leader (Space then a key). Picker-opening resolves synchronously so the next key
    // routes to the picker, not the editor.
    if (this.pendingLeader) {
      this.pendingLeader = false;
      const a = lookup("leader", c);
      if (!a) {
        return { handled: true }; // unknown leader chord — swallow so the browser default doesn't fire
      }
      if (a.t === "openPicker") {
        this.openPicker(a.kind);
        return { handled: true };
      }
      return { handled: true, run: () => this.runAction(a, 1, false) };
    }

    // Digit count (Normal). `0` is line-start unless mid-count.
    if (this.mode === "normal" && !c.ctrl && !c.alt && !c.meta && !c.shift) {
      if (c.key >= "1" && c.key <= "9") {
        this.pendingCount = Math.min(this.pendingCount * 10 + Number(c.key), 1_000_000);
        return { handled: true };
      }
      if (c.key === "0" && this.pendingCount > 0) {
        this.pendingCount = Math.min(this.pendingCount * 10, 1_000_000);
        return { handled: true };
      }
    }

    const count = this.mode === "normal" ? this.pendingCount || 1 : 1;
    if (this.mode === "normal") this.pendingCount = 0;
    const extend = this.mode === "normal" ? c.shift : false;

    const dispatch = (a: Action): { handled: boolean; run?: () => Promise<void>; noSync?: boolean } => {
      // Capture-arming is pure state, handled synchronously so it can't race the next key.
      if (a.t === "beginFind") {
        this.pendingFind = { dir: a.dir, till: a.till, count, extend };
        return { handled: true };
      }
      if (a.t === "beginSurround") {
        this.pendingSurround = a.target;
        return { handled: true };
      }
      if (a.t === "beginLeader") {
        this.pendingLeader = true;
        return { handled: true };
      }
      // Opening search must be synchronous so the next key routes to the search bar, not the editor.
      if (a.t === "enterSearch" || a.t === "enterSearchToCursor") {
        this.enterSearch(a.t === "enterSearchToCursor");
        return { handled: true };
      }
      // Scrolling is view-only — don't cursor-follow afterwards.
      if (a.t === "scroll") {
        return { handled: true, noSync: true, run: () => this.scrollView(a.dir, a.unit, count) };
      }
      return { handled: true, run: () => this.runAction(a, count, extend) };
    };

    const g = lookup("global", c);
    if (g) return dispatch(g);

    if (this.mode === "normal") {
      const a = lookup("normal", c);
      return a ? dispatch(a) : { handled: false };
    }

    // Insert mode: bound keys (Esc/arrows/Enter/Tab/Ctrl-edits) handled here; plain text is left
    // unhandled so it flows into the capture textarea, where the input/composition handlers insert
    // it (IME-aware). Returning handled:false means no preventDefault, so the char reaches it.
    const a = lookup("insert", c);
    if (a) return dispatch(a);
    return { handled: false };
  }

  /** Insert whatever text has accumulated in the capture textarea (a typed char, or a composed
   *  IME/dead-key string). No-op outside insert mode — stray chars there are just discarded. */
  private flushTypedText(): void {
    const text = this.clipboardCapture.value;
    this.clipboardCapture.value = "";
    if (text && this.mode === "insert") this.enqueue(() => this.insertText(text));
  }

  // ---- pickers --------------------------------------------------------------------------------

  private openPicker(kind: PickerKind): void {
    this.picker = new Picker({
      client: this.client,
      kind,
      onConfirm: (item) => this.onPickerConfirm(item),
      onClose: () => this.closePicker(),
      fileUrl: (item, explorerDir) => this.pickerItemUrl(item, explorerDir),
      explorerInitialDir: kind === "explorer" ? this.explorerInitialDir() : undefined,
      explorerSelectName: kind === "explorer" && this.currentPath ? basename(this.currentPath) : undefined,
      projectPaths: this.projectPaths,
      scopedBufferId: kind === "diagnostics" || kind === "references" ? this.bufferId : undefined,
      activeBufferId: kind === "grep" ? this.bufferId : undefined,
      onToast: (msg, k) => this.toast(msg, k),
      onCreatePath: (p) => this.onCreatePath(p),
      onCreateProject: (n) => this.onCreateProject(n),
    });
  }

  private explorerInitialDir(): string | null {
    if (this.currentPath) return dirname(this.currentPath);
    return this.projectPaths[0] ?? null;
  }

  private destroyPicker(): void {
    this.picker?.destroy();
    this.picker = null;
    this.clipboardCapture.focus();
  }

  private closePicker(): void {
    if (!this.picker) return;
    const kind = this.picker.kind;
    this.destroyPicker();
    void this.client.rpc<null>("picker/hide", { kind }).catch(() => {});
  }

  private onPickerConfirm(item: PickerItem): void {
    const kind = this.picker?.kind;
    if (!kind) return;
    const grepQuery = kind === "grep" ? this.picker?.query() : undefined;
    this.destroyPicker();
    this.enqueue(() => this.confirmSelect(kind, item, grepQuery));
  }

  /** Explorer "+ create file": open the path with create_if_missing, then close the picker. */
  private onCreatePath(absPath: string): void {
    const params = this.resolvePath(absPath);
    this.closePicker();
    if (params) {
      params.create_if_missing = true;
      this.enqueue(() => this.switchBuffer(params, { recordJump: true }));
    } else {
      this.toast(`path outside project: ${absPath}`, "error");
    }
  }

  /** Projects "+ create project": create + activate it, then open its buffer. */
  private onCreateProject(name: string): void {
    this.closePicker();
    this.enqueue(async () => {
      const a = await this.client.rpc<ProjectActivateResult>("project/create", { name });
      this.projectName = a.project.name;
      this.projectPaths = a.project.paths;
      await this.switchBuffer({ buffer_id: a.last_buffer_id ?? null, create_if_missing: false });
    });
  }

  /** Resolve a confirmed item via picker/select (the server returns the open target), then switch
   *  to it. Goes through select rather than the item fields because Explorer entries carry only a
   *  name — the server knows the directory. */
  private async confirmSelect(kind: PickerKind, item: PickerItem, grepQuery?: string): Promise<void> {
    let result: PickerSelectResult;
    try {
      result = await this.client.rpc<PickerSelectResult>("picker/select", { kind, item });
    } finally {
      void this.client.rpc<null>("picker/hide", { kind }).catch(() => {});
    }
    if (result.kind === "project") {
      await this.switchProject(result.name);
      return;
    }
    let params: BufferOpenParams | null = null;
    let jumpTo: LogicalPosition | null = null;
    if (result.kind === "file") {
      params = this.resolvePath(result.path);
    } else if (result.kind === "file_at") {
      params = this.resolvePath(result.path);
      jumpTo = result.position;
      if (params) params.jump_to = result.position;
    } else if (result.kind === "buffer") {
      params = { buffer_id: result.buffer_id, create_if_missing: false };
    }
    if (!params) {
      this.setStatus("could not open selection", true);
      return;
    }
    await this.switchBuffer(params, { recordJump: true });
    // Opening a grep hit primes the buffer's search with the query so n / Alt-n continue from here.
    if (grepQuery && jumpTo) {
      const res = await this.client.rpc<SearchSetResult>("search/set", {
        buffer_id: this.bufferId,
        query: grepQuery,
        anchor: jumpTo,
        extend: false,
      });
      this.committedQuery = grepQuery;
      this.searchCommitted = true;
      this.searchSummary = res.summary;
    }
  }

  /** Activate a different project (Projects picker) and open its last/scratch buffer. */
  private async switchProject(name: string): Promise<void> {
    const activated = await this.client.rpc<ProjectActivateResult>("project/activate", { name });
    this.projectName = activated.project.name;
    this.projectPaths = activated.project.paths;
    await this.switchBuffer({ buffer_id: activated.last_buffer_id ?? null, create_if_missing: false });
  }

  /** Map an absolute path to a project-relative (path_index, relative_path) the server accepts. */
  private resolvePath(abs: string): BufferOpenParams | null {
    for (let i = 0; i < this.projectPaths.length; i++) {
      const root = this.projectPaths[i];
      if (abs === root) return { path_index: i, relative_path: "", create_if_missing: false };
      const prefix = root.endsWith("/") ? root : root + "/";
      if (abs.startsWith(prefix)) {
        return { path_index: i, relative_path: abs.slice(prefix.length), create_if_missing: false };
      }
    }
    return null;
  }

  /** Adopt a freshly-opened buffer's state (shared by bootstrap and buffer switching). */
  private adoptOpenedBuffer(open: BufferOpenResult): void {
    this.bufferId = open.buffer_id;
    this.cursor = open.cursor;
    this.currentPath = open.path ?? null;
    this.scratchNumber = open.path ? null : open.scratch_number ?? null;
    this.label = open.path
      ? projectRelativeLabel(open.path, this.projectPaths)
      : `(scratch ${open.scratch_number ?? open.buffer_id})`;
    this.revision = open.revision;
    this.savedRevision = open.saved_revision;
    this.externallyModified = false;
    this.externallyDeleted = false;
    // diffView is intentionally not reset here — it's sticky for the session (re-applied after
    // each subscribe via reapplyDiffView), matching the terminal client.
    this.blame = null;
    this.lspServerRef = open.lsp_server ?? null;
    this.diagCounts = null;
  }

  // ---- browser history (the jump list) --------------------------------------------------------
  //
  // The web client rides *native* browser history: a qualifying jump `pushState`s the destination,
  // so Alt-←/→, the toolbar buttons, mouse side-buttons and trackpad swipe all step it; `popstate`
  // restores an entry via `nav/goto`. Each entry stores its location in `history.state` (so it
  // survives reconnect and reload), and the URL is kept human-shareable: `?project=…&file=…#L:C`
  // (or `#aL:aC-cL:cC` for a selection, anchor first).

  /** The `?project=&file=` (or `&buffer=`) query reflecting the current buffer. */
  private buildQuery(): string {
    const params = new URLSearchParams();
    if (this.projectName) params.set("project", this.projectName);
    if (this.currentPath) {
      const r = this.resolvePath(this.currentPath);
      if (r) {
        if (r.path_index) params.set("root", String(r.path_index));
        params.set("file", r.relative_path ?? "");
      } else {
        params.set("file", this.currentPath); // outside any root — fall back to the absolute path
      }
    } else if (this.scratchNumber != null) {
      // Scratch buffer: no path, so key the URL on the (session-scoped) buffer id — bootstrap and
      // navState both reopen scratch buffers by id. Stale after a server restart; bootstrap then
      // falls back to a fresh scratch.
      params.set("buffer", String(this.bufferId));
    }
    return params.toString();
  }

  /** The cursor/selection fragment: `#line:col` for a point, `#aLine:aCol-cLine:cCol` (anchor
   *  first, so orientation is encoded) for a selection. 1-based, matching the status bar. */
  private cursorFragment(): string {
    const enc = (q: LogicalPosition) => `${q.line + 1}:${q.col + 1}`;
    const p = this.cursor.position;
    const a = this.cursor.anchor;
    return p.line === a.line && p.col === a.col ? `#${enc(p)}` : `#${enc(a)}-${enc(p)}`;
  }

  /** Parse a `#…` fragment back into a cursor/selection, or null if malformed. */
  private parseFragment(frag: string): { position: LogicalPosition; anchor: LogicalPosition } | null {
    const body = frag.replace(/^#/, "");
    if (!body) return null;
    const pt = (s: string): LogicalPosition | null => {
      const [l, c] = s.split(":").map(Number);
      return Number.isInteger(l) && Number.isInteger(c) && l >= 1 && c >= 1
        ? { line: l - 1, col: c - 1 }
        : null;
    };
    if (body.includes("-")) {
      const [a, p] = body.split("-");
      const anchor = pt(a);
      const position = pt(p);
      return anchor && position ? { position, anchor } : null;
    }
    const position = pt(body);
    return position ? { position, anchor: position } : null;
  }

  private buildUrl(): string {
    const qs = this.buildQuery();
    return `${location.pathname}${qs ? `?${qs}` : ""}${this.cursorFragment()}`;
  }

  /** `?project=&root=&file=` query for a file at (root index, relative path), mirroring buildQuery. */
  private fileQuery(pathIndex: number, relativePath: string): string {
    const params = new URLSearchParams();
    if (this.projectName) params.set("project", this.projectName);
    if (pathIndex) params.set("root", String(pathIndex));
    params.set("file", relativePath);
    return params.toString();
  }

  /** Opener URL for a picker item, so the picker can render the row as an `<a>` that opens in a new
   *  tab on Ctrl/Cmd/middle-click. Returns null when no shareable URL applies (scratch buffers,
   *  directories, items outside any root — bootstrap only opens files relative to a root).
   *  - file / file-backed buffer:  `?project=&root=&file=`
   *  - scratch buffer:  `?project=&buffer=<id>` (session-scoped; bootstrap falls back if stale)
   *  - grep_hit:        …plus the (0-based) hit position as a 1-based `#L:C` fragment
   *  - dir_entry:       resolved from `explorerDir + name` (files only)
   *  - project:         `?project=` alone (bootstrap opens that project's last buffer) */
  private pickerItemUrl(item: PickerItem, explorerDir: string | null): string | null {
    const fromPath = (pathIndex: number, relativePath: string, frag = ""): string =>
      `${location.pathname}?${this.fileQuery(pathIndex, relativePath)}${frag}`;
    switch (item.kind) {
      case "file":
        return fromPath(item.path_index, item.relative_path);
      case "grep_hit":
        return fromPath(item.path_index, item.relative_path, `#${item.line + 1}:${item.col + 1}`);
      case "buffer": {
        if (item.path_index != null && item.relative_path != null) {
          return fromPath(item.path_index, item.relative_path);
        }
        // Scratch buffer: no path, so link by its (session-scoped) buffer id (see buildQuery).
        const params = new URLSearchParams();
        if (this.projectName) params.set("project", this.projectName);
        params.set("buffer", String(item.buffer_id));
        return `${location.pathname}?${params.toString()}`;
      }
      case "dir_entry": {
        if (item.is_dir || !explorerDir) return null;
        const r = this.resolvePath(joinPath(explorerDir, item.name));
        return r ? fromPath(r.path_index ?? 0, r.relative_path ?? "") : null;
      }
      case "project": {
        const params = new URLSearchParams({ project: item.name });
        return `${location.pathname}?${params.toString()}`;
      }
      default:
        return null;
    }
  }

  /** The location to stash in `history.state` so `popstate` can restore it via `nav/goto`. */
  private navState(): { nav: NavGotoParams } {
    const cursor = { position: this.cursor.position, anchor: this.cursor.anchor };
    if (this.currentPath) {
      const r = this.resolvePath(this.currentPath);
      if (r) return { nav: { path_index: r.path_index, relative_path: r.relative_path, cursor } };
    }
    return { nav: { buffer_id: this.bufferId, cursor } }; // scratch, or outside any root
  }

  private replaceHistory(): void {
    window.clearTimeout(this.historyTimer);
    history.replaceState(this.navState(), "", this.buildUrl());
  }

  private pushHistory(): void {
    window.clearTimeout(this.historyTimer);
    history.pushState(this.navState(), "", this.buildUrl());
  }

  /** Keep the current entry's fragment current as the cursor moves (Normal mode), debounced to
   *  respect browser pushState/replaceState throttling and avoid URL churn. */
  private scheduleHistoryUpdate(): void {
    window.clearTimeout(this.historyTimer);
    this.historyTimer = window.setTimeout(() => history.replaceState(this.navState(), "", this.buildUrl()), 200);
  }

  /** Open a different buffer and re-point the viewport at it. With `recordJump`, a move pushes a
   *  new browser-history entry (so Alt-←/→ return here); otherwise the current entry is replaced. */
  private async switchBuffer(
    openParams: BufferOpenParams,
    opts: { recordJump?: boolean } = {},
  ): Promise<void> {
    // Flush the origin's live cursor into the current entry before we move, so a pushed back-entry
    // captures exactly where we left (even if the movement debounce hadn't fired yet).
    if (opts.recordJump) this.replaceHistory();
    const originKey = this.locationKey();
    const open = await this.client.rpc<BufferOpenResult>("buffer/open", openParams);
    await this.applyOpenedBuffer(open);
    const moved = this.locationKey() !== originKey;
    if (opts.recordJump && moved) this.pushHistory();
    else this.replaceHistory();
  }

  /** A compact identity for the current location, for the "did this navigation actually move me?"
   *  check (no entry is recorded for a jump that lands where you already are). */
  private locationKey(): string {
    return `${this.bufferId}:${this.cursor.position.line}:${this.cursor.position.col}`;
  }

  /** Post-`buffer/open` plumbing shared by switchBuffer and nav restore: adopt state, subscribe a
   *  fresh viewport (dropping the old), paint. Does *not* touch browser history. */
  private async applyOpenedBuffer(open: BufferOpenResult): Promise<void> {
    const oldViewport = this.viewportId;
    this.adoptOpenedBuffer(open);
    this.mode = "normal";
    this.scroll = open.scroll ?? {
      logical_line: Math.max(0, open.cursor.position.line - Math.floor(this.rows / 2)),
      sub_row: 0,
    };
    const sub = await this.client.rpc<ViewportSubscribeResult>("viewport/subscribe", {
      buffer_id: this.bufferId,
      cols: this.cols,
      rows: this.rows,
      overscan_rows: this.rows,
      scroll: this.scroll,
      wrap: this.wrap,
      continuation_marker_width: CONTINUATION_MARKER_WIDTH,
      tab_width: TAB_WIDTH,
    });
    this.viewportId = sub.viewport_id;
    this.window = sub.window;
    this.seedBufferStatus(sub.buffer_status);
    await this.reapplyDiffView();
    if (oldViewport && oldViewport !== sub.viewport_id) {
      void this.client.rpc<null>("viewport/unsubscribe", { viewport_id: oldViewport }).catch(() => {});
    }
    this.bufferEl.scrollTop = 0; // reset before revealing the new buffer's cursor
    this.render();
    this.revealCursor();
  }

  /** Restore a jump-list entry on `popstate` (browser back/forward). The server opens the buffer
   *  (reopening a closed file by path) and restores the full cursor/selection; we just re-point. */
  private async gotoEntry(nav: NavGotoParams): Promise<void> {
    const res = await this.client.rpc<NavStepResult>("nav/goto", nav);
    if (res.target) await this.applyOpenedBuffer(res.target);
  }

  private onPopState(e: PopStateEvent): void {
    const st = e.state as { nav?: NavGotoParams } | null;
    if (!st?.nav) return; // not one of our entries (or the initial state) — leave it alone
    const nav = st.nav;
    this.enqueue(() => this.gotoEntry(nav));
  }

  // ---- action execution -----------------------------------------------------------------------

  private async runAction(action: Action, count: number, extend: boolean): Promise<void> {
    switch (action.t) {
      case "moveChar":
        await this.moveMotion({ kind: "char", direction: action.dir, count }, extend);
        break;
      case "moveWord":
        await this.moveMotion(
          { kind: "word", direction: action.dir, count, boundary: action.boundary, exclusive: action.dir === "forward" && extend },
          extend,
        );
        break;
      case "moveWordEnd":
        await this.moveMotion({ kind: "word_end", direction: action.dir, count, boundary: action.boundary }, extend);
        break;
      case "moveVisualLine":
        await this.moveMotion({ kind: "visual_line", viewport_id: this.viewportId, direction: action.dir, count }, extend);
        break;
      case "moveLogicalLine":
        await this.moveMotion({ kind: "logical_line", direction: action.dir, count, preserve_col: true }, extend);
        break;
      case "moveLineStart":
        await this.moveMotion({ kind: "line_start" }, extend);
        break;
      case "moveLineEnd":
        await this.moveMotion({ kind: "line_end" }, extend);
        break;
      case "moveLineFirstNonblank":
        await this.moveMotion({ kind: "line_first_nonblank" }, extend);
        break;
      case "gotoLine": {
        const lineCount = this.window?.line_count ?? 1;
        const line = action.last ? Math.max(0, lineCount - 1) : Math.max(0, count - 1);
        await this.moveMotion({ kind: "goto", position: { line, col: 0 } }, extend);
        break;
      }
      case "matchBracket":
        await this.moveMotion({ kind: "match_bracket", inner: action.inner }, extend);
        break;
      case "pageMotion": {
        const span = action.half ? Math.max(1, Math.floor(this.rows / 2)) : this.rows;
        await this.moveMotion(
          { kind: "visual_line", viewport_id: this.viewportId, direction: action.dir, count: count * span },
          extend,
        );
        break;
      }
      case "navUnit":
        await this.moveMotion(
          { kind: action.dir === "forward" ? "next_navigation_unit" : "prev_navigation_unit" },
          false,
        );
        break;
      case "navUnitEdge":
        await this.moveMotion(
          { kind: action.start ? "start_of_navigation_unit" : "end_of_navigation_unit" },
          true,
        );
        break;
      case "selectLine":
        for (let i = 0; i < Math.max(1, count); i++) {
          this.cursor = await this.client.rpc<CursorState>("cursor/select_line", {
            buffer_id: this.bufferId,
            direction: action.dir,
            extend,
          });
        }
        break;
      case "swapAnchor":
        this.cursor = await this.client.rpc<CursorState>("cursor/swap_anchor", { buffer_id: this.bufferId });
        break;
      case "collapseSelection":
        this.cursor = await this.client.rpc<CursorState>("cursor/set", {
          buffer_id: this.bufferId,
          position: this.cursor.position,
          anchor: this.cursor.position,
        });
        break;
      case "treeExpand":
      case "treeContract":
        for (let i = 0; i < Math.max(1, count); i++) {
          const next = await this.client.rpc<CursorState>(
            action.t === "treeExpand" ? "cursor/expand" : "cursor/contract",
            { buffer_id: this.bufferId },
          );
          if (next.position.line === this.cursor.position.line && next.position.col === this.cursor.position.col &&
              next.anchor.line === this.cursor.anchor.line && next.anchor.col === this.cursor.anchor.col) {
            break;
          }
          this.cursor = next;
        }
        break;
      case "motionUndo":
      case "motionRedo":
        for (let i = 0; i < Math.max(1, count); i++) {
          const r = await this.client.rpc<CursorUndoResult>(
            action.t === "motionUndo" ? "cursor/undo" : "cursor/redo",
            { buffer_id: this.bufferId },
          );
          if (!r.applied) break;
          this.cursor = r.cursor;
        }
        break;
      case "centerCursor": {
        const row = this.cursorAbsoluteVisualRow();
        if (row !== null) {
          this.scrollTopTo((row - Math.floor(this.visibleRows() / 2)) * this.cell.h + BUFFER_PAD, true);
        }
        break;
      }

      case "enterInsert":
        await this.enterInsertAt(action.where);
        break;
      case "leaveInsert":
        this.mode = "normal";
        break;

      case "backspace":
        await this.edit("input/backspace");
        break;
      case "newlineIndent":
        await this.edit("input/newline_and_indent");
        break;
      case "insertTab":
        await this.insertText("\t");
        break;
      case "deletePoint":
        await this.edit("input/delete");
        break;
      case "deleteSelection":
        for (let i = 0; i < Math.max(1, count); i++) await this.edit("input/delete");
        break;
      case "deleteLine":
        await this.edit("input/delete_line");
        break;
      case "change":
        await this.edit("input/delete");
        this.mode = "insert";
        break;
      case "changeLine":
        await this.edit("input/change_line");
        this.mode = "insert";
        break;
      case "undo":
      case "redo":
        for (let i = 0; i < Math.max(1, count); i++) {
          const r = await this.client.rpc<UndoResult>(action.t === "undo" ? "input/undo" : "input/redo", {
            buffer_id: this.bufferId,
          });
          if (!r.applied) break;
          this.cursor = r.cursor;
          this.revision = r.revision;
        }
        break;
      case "moveLines":
        for (let i = 0; i < Math.max(1, count); i++) {
          const r = await this.client.rpc<EditResult>("input/move_lines", {
            buffer_id: this.bufferId,
            direction: action.dir,
          });
          this.cursor = r.cursor;
          this.revision = r.revision;
        }
        break;
      case "joinLines":
        for (let i = 0; i < Math.max(1, count); i++) await this.edit("input/join_lines");
        break;
      case "indent":
        for (let i = 0; i < Math.max(1, count); i++) await this.edit("input/indent");
        break;
      case "dedent":
        for (let i = 0; i < Math.max(1, count); i++) await this.edit("input/dedent");
        break;
      case "toggleComment":
        await this.edit("input/toggle_comment");
        break;
      case "openLineBelow":
        await this.setCursor({ line: this.cursor.position.line, col: LINE_END_COL });
        await this.edit("input/newline_and_indent");
        this.mode = "insert";
        break;
      case "openLineAbove":
        await this.setCursor({ line: this.cursor.position.line, col: 0 });
        await this.insertText("\n");
        await this.moveMotion({ kind: "logical_line", direction: "backward", count: 1, preserve_col: false }, false);
        this.mode = "insert";
        break;

      case "copy":
        await this.clipboardCopy("selection");
        break;
      case "copyLine":
        await this.clipboardCopy("line");
        break;
      case "cut":
        await this.clipboardCut("selection");
        break;
      case "cutLine":
        await this.clipboardCut("line");
        break;
      case "replaceClipboard": {
        const text = await this.readClip();
        if (text !== null) await this.insertText(text.repeat(Math.max(1, count)), true);
        break;
      }
      case "replaceLineClipboard": {
        const text = await this.readClip();
        if (text !== null) {
          const r = await this.client.rpc<EditResult>("input/replace_line", {
            buffer_id: this.bufferId,
            text,
          });
          this.cursor = r.cursor;
          this.revision = r.revision;
        }
        break;
      }

      case "repeatMotion":
        await this.repeatMotion(count, extend);
        break;
      case "unsurround": {
        const r = await this.client.rpc<EditResult>("input/unsurround", {
          buffer_id: this.bufferId,
          target: action.target,
        });
        this.cursor = r.cursor;
        this.revision = r.revision;
        break;
      }
      case "toggleDiffView": {
        const r = await this.client.rpc<ViewportWindowResult>("git/set_diff_view", {
          viewport_id: this.viewportId,
          enabled: !this.diffView,
        });
        this.diffView = !this.diffView;
        this.window = r.window;
        this.setStatus(`diff: ${this.diffView ? "on" : "off"}`);
        break;
      }
      case "applyHunk": {
        const r = await this.client.rpc<GitApplyHunkResult>("git/apply_hunk", {
          buffer_id: this.bufferId,
          action: action.action,
        });
        this.cursor = r.cursor;
        // The toggle's direction comes back in the status.
        const messages: Record<string, string> = {
          staged: "staged change",
          unstaged: "unstaged change",
          reverted: "reverted change",
          no_change: action.action === "revert" ? "no change to revert here" : "no change here",
        };
        if (r.status === "dirty_buffer") this.setStatus("unsaved changes — save first", true);
        else if (r.status === "unavailable") this.setStatus("not in a git repository");
        else this.setStatus(messages[r.status] ?? r.status);
        break;
      }
      case "navigateHunk": {
        const r = await this.client.rpc<GitNavigateHunkResult>("git/navigate_hunk", {
          buffer_id: this.bufferId,
          from_line: this.cursor.position.line,
          direction: action.dir,
        });
        this.cursor = r.cursor;
        if (!r.moved) this.setStatus("no more changes");
        break;
      }
      case "grepNavigate": {
        const target = await this.client.rpc<PickerGrepNavigateTarget | null>("picker/grep_navigate", {
          direction: action.dir,
          buffer_id: this.bufferId,
        });
        if (target) {
          const params = this.resolvePath(target.path);
          if (params) {
            params.jump_to = target.position;
            await this.switchBuffer(params, { recordJump: true });
            // Prime the buffer's search with the grep query so n / Alt-n continue from here.
            if (target.query) {
              const res = await this.client.rpc<SearchSetResult>("search/set", {
                buffer_id: this.bufferId,
                query: target.query,
                anchor: target.position,
                extend: false,
              });
              this.committedQuery = target.query;
              this.searchCommitted = true;
              this.searchSummary = res.summary;
            }
          }
        } else {
          this.setStatus("no more grep hits");
        }
        break;
      }
      case "hover": {
        const r = await this.client.rpc<LspHoverResult>("lsp/hover", { buffer_id: this.bufferId });
        if (r.contents) this.showHover(r.contents, { markdown: true });
        else this.setStatus("No hover info");
        break;
      }
      case "gotoDefinition": {
        const r = await this.client.rpc<LspGotoDefinitionResult>("lsp/goto_definition", {
          buffer_id: this.bufferId,
        });
        if (!r.location) {
          this.setStatus("No definition found");
        } else {
          const params = this.resolvePath(r.location.path);
          if (params) {
            params.jump_to = r.location.position;
            await this.switchBuffer(params, { recordJump: true });
          } else {
            this.setStatus(`definition outside project: ${r.location.path}`, true);
          }
        }
        break;
      }
      case "showDiagnostic":
        this.showDiagnosticAtCursor();
        break;
      case "showCommitInfo": {
        // Blame the cursor line for its commit hash, then resolve full details (the cached EOL
        // blame keeps only display text, so re-fetch). Mirrors the TUI's `Space o`.
        const bl = await this.client.rpc<GitBlameLineResult>("git/blame_line", {
          buffer_id: this.bufferId,
          line: this.cursor.position.line,
        });
        if (!bl.blame) {
          this.setStatus("No commit details for this line");
          break;
        }
        if (bl.blame.is_uncommitted) {
          this.setStatus("Uncommitted line — no commit details");
          break;
        }
        const ci = await this.client.rpc<GitCommitInfoResult>("git/commit_info", {
          buffer_id: this.bufferId,
          commit: bl.blame.commit,
        });
        if (ci.info) this.showHover(formatCommitDetails(ci.info));
        else this.setStatus("Commit not found");
        break;
      }
      case "navigateDiagnostic": {
        const r = await this.client.rpc<LspNavigateDiagnosticResult>("lsp/navigate_diagnostic", {
          buffer_id: this.bufferId,
          from_line: this.cursor.position.line,
          direction: action.dir,
        });
        this.cursor = r.cursor;
        if (!r.moved) this.setStatus("no more diagnostics");
        break;
      }
      case "format": {
        const r = await this.client.rpc<LspFormatResult>("lsp/format", { buffer_id: this.bufferId });
        this.cursor = r.cursor;
        this.setStatus(`format: ${r.status.replace("_", " ")}`);
        break;
      }

      case "searchCycle":
        for (let i = 0; i < Math.max(1, count); i++) {
          const r = await this.client.rpc<SearchNavResult>(
            action.dir === "forward" ? "search/next" : "search/prev",
            { buffer_id: this.bufferId, extend },
          );
          this.cursor = r.cursor;
          this.searchSummary = r.summary;
        }
        break;
      case "searchFromSelection":
        await this.searchFromSelection();
        break;
      case "dropSearch":
        if (this.searchCommitted || this.searchSummary) {
          await this.client.rpc<null>("search/clear", { buffer_id: this.bufferId }).catch(() => {});
        }
        this.searchCommitted = false;
        this.searchSummary = null;
        this.committedQuery = "";
        break;

      case "toggleWrap":
        await this.toggleWrap();
        break;
      case "save":
        await this.saveToPath(null, null);
        break;
      case "saveAs":
        await this.saveAs();
        break;
      case "reload":
        await this.reloadBuffer();
        break;
      case "newScratch":
        await this.switchBuffer({ create_if_missing: false }, { recordJump: true });
        break;
      case "closeBuffer":
        await this.closeBuffer();
        break;
      case "openHelp":
        await this.modal(() => showHelp());
        break;
      case "openProjectSettings":
        this.openProjectSettings();
        break;
    }
    // Record the last repeatable motion so `r` can replay it (find is recorded at its capture site).
    if (isRepeatable(action)) this.lastRepeat = { kind: "action", action, count };
  }

  private async repeatMotion(count: number, extend: boolean): Promise<void> {
    const rep = this.lastRepeat;
    if (!rep) return;
    for (let i = 0; i < Math.max(1, count); i++) {
      if (rep.kind === "find") await this.moveMotion(rep.motion, extend);
      else await this.runAction(rep.action, rep.count, extend);
    }
  }

  private async surround(delimiter: string, target: SurroundTarget): Promise<void> {
    const r = await this.client.rpc<EditResult>("input/surround", {
      buffer_id: this.bufferId,
      delimiter,
      target,
    });
    this.cursor = r.cursor;
    this.revision = r.revision;
  }

  private async moveMotion(motion: Motion, extend: boolean): Promise<void> {
    this.cursor = await this.client.rpc<CursorState>("cursor/move", {
      buffer_id: this.bufferId,
      motion,
      extend_selection: extend,
    });
  }

  private async setCursor(position: LogicalPosition): Promise<void> {
    this.cursor = await this.client.rpc<CursorState>("cursor/set", {
      buffer_id: this.bufferId,
      position,
      anchor: position,
    });
  }

  private async edit(method: string): Promise<void> {
    const r = await this.client.rpc<EditResult>(method, { buffer_id: this.bufferId });
    this.cursor = r.cursor;
    this.revision = r.revision;
  }

  private async insertText(text: string, selectPasted = false): Promise<void> {
    const r = await this.client.rpc<EditResult>("input/text", {
      buffer_id: this.bufferId,
      text,
      select_pasted: selectPasted,
    });
    this.cursor = r.cursor;
    this.revision = r.revision;
  }

  // ---- clipboard (system clipboard via navigator.clipboard) ----------------------------------

  private async clipboardCopy(scope: CopyScope): Promise<void> {
    const r = await this.client.rpc<BufferCopyResult>("buffer/copy", { buffer_id: this.bufferId, scope });
    try {
      await writeClipboard(r.text);
      this.setStatus(`copied ${r.text.length} chars`);
    } catch (e) {
      this.setStatus(`copy failed: ${(e as Error).message}`, true);
    }
  }

  private async clipboardCut(scope: CopyScope): Promise<void> {
    const r = await this.client.rpc<BufferCutResult>("buffer/cut", { buffer_id: this.bufferId, scope });
    this.cursor = r.cursor;
    this.revision = r.revision;
    try {
      await writeClipboard(r.text);
      this.setStatus(`cut ${r.text.length} chars`);
    } catch (e) {
      this.setStatus(`cut failed: ${(e as Error).message}`, true);
    }
  }

  /** Native paste (Ctrl-V / Cmd-V) on the focused capture textarea — reads `clipboardData`, which
   *  needs no permission prompt (unlike navigator.clipboard.readText, which Firefox gates each
   *  time). In Normal mode this is "paste before"; in Insert, "paste at cursor". */
  private onPaste(e: ClipboardEvent): void {
    if (!this.window || this.picker || this.searchOpen) return;
    e.preventDefault();
    const text = e.clipboardData?.getData("text") ?? "";
    this.clipboardCapture.value = "";
    if (!text) return;
    this.enqueue(() => this.pasteText(text));
  }

  private async pasteText(text: string): Promise<void> {
    if (this.mode === "insert") {
      await this.insertText(text);
    } else {
      const start = minPos(this.cursor.position, this.cursor.anchor);
      await this.setCursor(start);
      await this.insertText(text, true);
    }
  }

  /** Read the system clipboard, surfacing a denied/failed read as a status (returns null). */
  private async readClip(): Promise<string | null> {
    try {
      return await readClipboard();
    } catch (e) {
      this.setStatus(`paste failed: ${(e as Error).message}`, true);
      return null;
    }
  }

  private async enterInsertAt(where: InsertWhere): Promise<void> {
    const pos = this.cursor.position;
    const anchor = this.cursor.anchor;
    switch (where) {
      case "selectionStart":
        await this.setCursor(minPos(pos, anchor));
        break;
      case "selectionEnd": {
        const max = maxPos(pos, anchor);
        await this.setCursor(max);
        await this.moveMotion({ kind: "char", direction: "forward", count: 1 }, false);
        break;
      }
      case "firstLineStart":
        // First non-blank of the first line of the selection (consistent with `Alt-h`):
        // park on the line, then resolve the column via the same motion `Alt-h` uses.
        await this.setCursor({ line: Math.min(pos.line, anchor.line), col: 0 });
        await this.moveMotion({ kind: "line_first_nonblank" }, false);
        break;
      case "lastLineEnd":
        await this.setCursor({ line: Math.max(pos.line, anchor.line), col: LINE_END_COL });
        break;
    }
    this.mode = "insert";
  }

  private async toggleWrap(): Promise<void> {
    const next: WrapMode = this.wrap === "soft" ? "none" : "soft";
    const r = await this.client.rpc<ViewportWindowResult>("viewport/set_wrap", {
      viewport_id: this.viewportId,
      wrap: next,
    });
    this.wrap = next;
    if (next === "soft") this.bufferEl.scrollLeft = 0; // no horizontal scroll under soft wrap
    this.window = r.window;
  }

  /** Save to a path (null/null = the buffer's current path). Confirms overwrite via a dialog. */
  private async saveToPath(pathIndex: number | null, relativePath: string | null): Promise<void> {
    const params = {
      buffer_id: this.bufferId,
      path_index: pathIndex,
      relative_path: relativePath,
      confirm: false,
    };
    try {
      const r = await this.client.rpc<BufferSaveResult>("buffer/save", params);
      this.savedRevision = r.revision;
      this.toast("saved", "success");
    } catch (e) {
      if (e instanceof RpcError && e.code === WOULD_OVERWRITE) {
        const ok = await this.modal(() => confirmDialog("Overwrite the existing file on disk?"));
        if (!ok) return;
        const r = await this.client.rpc<BufferSaveResult>("buffer/save", { ...params, confirm: true });
        this.savedRevision = r.revision;
        this.toast("saved", "success");
      } else {
        this.toast((e as Error).message, "error");
      }
    }
  }

  private async saveAs(): Promise<void> {
    const roots = this.projectPaths.map(basename);
    let initialPath = "";
    let initialRootIndex = 0;
    if (this.currentPath) {
      const r = this.resolvePath(this.currentPath);
      if (r) {
        initialPath = r.relative_path ?? "";
        initialRootIndex = r.path_index ?? 0;
      }
    }
    const choice = await this.modal(() => saveAsDialog({ roots, initialPath, initialRootIndex }));
    if (!choice) return;
    await this.saveToPath(choice.pathIndex, choice.relativePath);
    const root = this.projectPaths[choice.pathIndex] ?? "";
    const abs = root.endsWith("/") ? root + choice.relativePath : `${root}/${choice.relativePath}`;
    this.currentPath = abs;
    this.label = projectRelativeLabel(abs, this.projectPaths);
  }

  private async reloadBuffer(): Promise<void> {
    if (!this.currentPath) {
      this.toast("scratch buffer has no path to reload", "warning");
      return;
    }
    const doReload = async (force: boolean): Promise<void> => {
      const r = await this.client.rpc<BufferReloadResult>("buffer/reload", {
        buffer_id: this.bufferId,
        force,
      });
      this.revision = r.revision;
      this.savedRevision = r.revision;
      this.externallyModified = false;
      this.externallyDeleted = false;
      this.toast(`reloaded (rev ${r.revision})`, "success");
    };
    try {
      await doReload(false);
    } catch (e) {
      if (e instanceof RpcError && e.code === WOULD_DISCARD_CHANGES) {
        const ok = await this.modal(() => confirmDialog("Discard local changes and reload from disk?"));
        if (ok) await doReload(true);
      } else {
        this.toast((e as Error).message, "error");
      }
    }
  }

  private async closeBuffer(): Promise<void> {
    if (this.revision !== this.savedRevision) {
      const ok = await this.modal(() => confirmDialog(`Discard unsaved changes in ${this.label}?`));
      if (!ok) return;
    }
    const r = await this.client.rpc<BufferCloseResult>("buffer/close", { buffer_id: this.bufferId });
    await this.switchBuffer(
      r.next_buffer_id != null ? { buffer_id: r.next_buffer_id, create_if_missing: false } : { create_if_missing: false },
    );
  }

  /** Project-settings overlay: list the active project's roots (with delete), plus an add-root
   *  input. Interactive (stays open across edits); Esc / Done closes. */
  private openProjectSettings(): void {
    this.modalOpen = true;
    const ov = document.createElement("div");
    ov.className = "overlay";
    const box = document.createElement("div");
    box.className = "modal project-settings";
    const title = document.createElement("div");
    title.className = "modal-message";
    title.textContent = "Project settings";

    // Name (rename) field.
    const nameField = document.createElement("div");
    nameField.className = "modal-field";
    const nameInput = document.createElement("input");
    nameInput.className = "modal-input";
    nameInput.value = this.projectName;
    nameInput.spellcheck = false;
    nameInput.autocomplete = "off";
    const renameBtn = document.createElement("button");
    renameBtn.className = "modal-btn";
    renameBtn.textContent = "Rename";
    nameField.append(nameInput, renameBtn);

    const rootsLabel = document.createElement("div");
    rootsLabel.className = "ps-label";
    rootsLabel.textContent = "Roots";
    const list = document.createElement("div");
    list.className = "ps-roots";
    const field = document.createElement("div");
    field.className = "modal-field";
    const input = document.createElement("input");
    input.className = "modal-input";
    input.placeholder = "add a root path (~ ok)";
    input.spellcheck = false;
    input.autocomplete = "off";
    const addBtn = document.createElement("button");
    addBtn.className = "modal-btn primary";
    addBtn.textContent = "Add";
    field.append(input, addBtn);
    const buttons = document.createElement("div");
    buttons.className = "modal-buttons";
    const done = document.createElement("button");
    done.className = "modal-btn";
    done.textContent = "Done";
    buttons.append(done);
    box.append(title, nameField, rootsLabel, list, field, buttons);
    ov.append(box);
    document.body.append(ov);

    const renderRoots = () => {
      list.replaceChildren(
        ...this.projectPaths.map((root) => {
          const row = document.createElement("div");
          row.className = "ps-root";
          const label = document.createElement("span");
          label.className = "ps-root-path";
          label.textContent = root;
          const del = document.createElement("button");
          del.className = "ps-root-del";
          del.textContent = "✕";
          del.title = "Remove root";
          del.addEventListener("click", () => void removeRoot(root));
          row.append(label, del);
          return row;
        }),
      );
    };
    const rename = async () => {
      const newName = nameInput.value.trim();
      if (!newName || newName === this.projectName) return;
      try {
        const info = await this.client.rpc<ProjectInfo>("project/rename", {
          project: this.projectName,
          new_name: newName,
        });
        this.projectName = info.name;
        this.projectPaths = info.paths;
        nameInput.value = info.name;
        this.toast(`renamed to ${info.name}`, "success");
        this.render();
      } catch (e) {
        this.toast((e as Error).message, "error");
      }
    };
    const addRoot = async () => {
      const path = input.value.trim();
      if (!path) return;
      try {
        const info = await this.client.rpc<ProjectInfo>("project/add_root", {
          project: this.projectName,
          path,
        });
        this.projectPaths = info.paths;
        input.value = "";
        renderRoots();
      } catch (e) {
        this.toast((e as Error).message, "error");
      }
    };
    const removeRoot = async (root: string) => {
      const ok = await confirmDialog(`Remove root ${root}?`, { danger: true });
      box.focus();
      if (!ok) return;
      try {
        const r = await this.client.rpc<ProjectRemoveRootResult>("project/remove_root", {
          project: this.projectName,
          path: root,
        });
        this.projectPaths = r.project.paths;
        renderRoots();
        if (r.closed_buffer_ids?.includes(this.bufferId)) {
          await this.switchBuffer(
            r.next_buffer_id != null
              ? { buffer_id: r.next_buffer_id, create_if_missing: false }
              : { create_if_missing: false },
          );
        }
      } catch (e) {
        this.toast((e as Error).message, "error");
      }
    };
    const finish = () => {
      ov.removeEventListener("keydown", onKey, true);
      ov.remove();
      this.modalOpen = false;
      this.clipboardCapture.focus();
    };
    const onKey = (e: KeyboardEvent) => {
      e.stopPropagation();
      if (e.key === "Escape") {
        e.preventDefault();
        finish();
      } else if (e.key === "Enter") {
        e.preventDefault();
        if (document.activeElement === nameInput) void rename();
        else if (document.activeElement === input) void addRoot();
      }
    };
    renderRoots();
    ov.addEventListener("keydown", onKey, true);
    renameBtn.addEventListener("click", () => void rename());
    addBtn.addEventListener("click", () => void addRoot());
    done.addEventListener("click", finish);
    ov.addEventListener("mousedown", (e) => {
      if (e.target === ov) finish();
    });
    input.focus();
  }

  /** Run a modal dialog with the editor keymap suspended, restoring buffer focus afterward. */
  private async modal<T>(show: () => Promise<T>): Promise<T> {
    this.modalOpen = true;
    try {
      return await show();
    } finally {
      this.modalOpen = false;
      this.clipboardCapture.focus();
    }
  }

  // ---- mouse ----------------------------------------------------------------------------------

  private onMouseDown(e: MouseEvent): void {
    if (e.button !== 0 || this.picker || this.searchOpen) return;
    const pos = this.mouseToPos(e);
    if (!pos) return;
    e.preventDefault();
    this.dismissHover();
    this.clipboardCapture.focus();
    this.dragging = true;
    this.dragAnchor = pos;
    this.setSelection(pos, pos);
  }

  private onMouseMove(e: MouseEvent): void {
    if (!this.dragging || !this.dragAnchor) return;
    const pos = this.mouseToPos(e);
    if (pos) this.setSelection(pos, this.dragAnchor);
  }

  /** Set the cursor/selection and repaint locally (no window refetch — a click stays in view). */
  private setSelection(position: LogicalPosition, anchor: LogicalPosition): void {
    this.queue = this.queue
      .then(async () => {
        this.cursor = await this.client.rpc<CursorState>("cursor/set", {
          buffer_id: this.bufferId,
          position,
          anchor,
        });
        this.render();
      })
      .catch(() => {});
  }

  /** Map a mouse event to a logical position via the row's data-line/byte and a click-x → char
   *  measurement against the row's text span. Returns null for phantom rows / empty areas. */
  private mouseToPos(e: MouseEvent): LogicalPosition | null {
    const rowEl = (e.target as HTMLElement).closest(".row") as HTMLElement | null;
    if (!rowEl || rowEl.dataset.line === undefined) return null;
    const line = Number(rowEl.dataset.line);
    const rowByte = Number(rowEl.dataset.byte);
    const textEl = rowEl.querySelector(".row-text") as HTMLElement | null;
    if (!textEl) return { line, col: rowByte };
    const rect = textEl.getBoundingClientRect();
    const charIdx = Math.max(0, Math.round((e.clientX - rect.left) / this.cell.w));
    const { byteStart, byteLen } = decodeRow(textEl.textContent ?? "");
    // A click past the last char maps to the line-end byte (not the last char) so you can select to EOL.
    const within = charIdx >= byteStart.length ? byteLen : byteStart[charIdx];
    return { line, col: rowByte + within };
  }

  // ---- search ---------------------------------------------------------------------------------

  /** Enter the incremental-search bar. `extend` (`?`) grows the selection from the cursor to each
   *  match instead of re-selecting just the match. */
  private enterSearch(extend: boolean): void {
    this.searchSnapshot = {
      cursor: this.cursor,
      scrollTop: this.bufferEl.scrollTop,
      query: this.committedQuery,
      active: this.searchCommitted,
    };
    this.extendToCursor = extend;
    this.searchCommitted = false;
    this.searchSummary = null;
    this.searchOpen = true;
    this.historyIndex = null;
    this.historyDraft = "";
    this.searchInput.value = "";
    this.searchInput.placeholder = "Find in buffer…";
    this.searchCountEl.textContent = "";
    // Float just above the status bar (its height varies with font/zoom, so measure it).
    this.searchBar.style.bottom = `${this.statusEl.offsetHeight + 8}px`;
    this.searchBar.style.display = "flex";
    this.searchInput.focus();
    // Clear server-side highlights immediately (restored on Esc).
    this.enqueue(() => this.client.rpc<null>("search/clear", { buffer_id: this.bufferId }).then(() => {}));
  }

  private onSearchKey(e: KeyboardEvent): void {
    if (e.key === "Escape") {
      e.preventDefault();
      this.abortSearch();
    } else if (e.key === "Enter") {
      e.preventDefault();
      this.commitSearch();
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      this.browseHistory(-1);
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      this.browseHistory(1);
    }
  }

  /** Browse committed search queries with ↑/↓ in the search bar (a draft of the in-progress query
   *  is kept so ↓ past the newest returns to it). */
  private browseHistory(delta: number): void {
    if (this.searchHistory.length === 0) return;
    if (this.historyIndex === null) {
      if (delta > 0) return; // nothing newer than the current draft
      this.historyDraft = this.searchInput.value;
      this.historyIndex = this.searchHistory.length;
    }
    const next = this.historyIndex + delta;
    if (next >= this.searchHistory.length) {
      this.historyIndex = null;
      this.searchInput.value = this.historyDraft;
    } else if (next < 0) {
      return;
    } else {
      this.historyIndex = next;
      this.searchInput.value = this.searchHistory[next];
    }
    this.enqueue(() => this.runIncrementalSearch());
  }

  private async runIncrementalSearch(): Promise<void> {
    const query = this.searchInput.value;
    if (!query) {
      await this.client.rpc<null>("search/clear", { buffer_id: this.bufferId }).catch(() => {});
      this.searchSummary = null;
      await this.revertToSnapshotCursor();
      this.updateSearchCount();
      return;
    }
    const snap = this.searchSnapshot;
    const anchor = snap ? minPos(snap.cursor.position, snap.cursor.anchor) : null;
    let zero = false;
    try {
      const r = await this.client.rpc<SearchSetResult>("search/set", {
        buffer_id: this.bufferId,
        query,
        anchor,
        extend: this.extendToCursor,
      });
      this.cursor = r.cursor;
      this.searchSummary = r.summary;
      zero = r.summary.total === 0;
    } catch {
      this.searchSummary = { buffer_id: this.bufferId, total: 0, truncated: false, current_index: 0 };
      this.setStatus("invalid regex", true);
      zero = true;
    }
    if (zero) await this.revertToSnapshotCursor();
    this.updateSearchCount();
  }

  /** `Alt-/`: search for the current selection as a literal (regex-escaped), without entering the
   *  search bar — leaves the cursor where it is and lights up the matches. */
  private async searchFromSelection(): Promise<void> {
    const copied = await this.client.rpc<BufferCopyResult>("buffer/copy", {
      buffer_id: this.bufferId,
      scope: "selection",
    });
    if (!copied.text) return;
    const query = regexEscape(copied.text);
    const res = await this.client.rpc<SearchSetResult>("search/set", {
      buffer_id: this.bufferId,
      query,
      anchor: null,
      extend: false,
    });
    this.committedQuery = query;
    this.searchCommitted = true;
    this.searchSummary = res.summary;
    if (this.searchHistory[this.searchHistory.length - 1] !== query) this.searchHistory.push(query);
  }

  private commitSearch(): void {
    this.committedQuery = this.searchInput.value;
    this.searchCommitted = this.committedQuery.length > 0;
    if (this.searchCommitted && this.searchHistory[this.searchHistory.length - 1] !== this.committedQuery) {
      this.searchHistory.push(this.committedQuery);
    }
    this.historyIndex = null;
    if (!this.searchCommitted) this.searchSummary = null;
    this.searchSnapshot = null;
    this.searchOpen = false;
    this.searchBar.style.display = "none";
    this.clipboardCapture.focus();
    this.render();
  }

  private abortSearch(): void {
    const snap = this.searchSnapshot;
    this.searchSnapshot = null;
    this.searchOpen = false;
    this.searchBar.style.display = "none";
    this.clipboardCapture.focus();
    this.enqueue(async () => {
      if (snap && snap.active && snap.query) {
        const r = await this.client.rpc<SearchSetResult>("search/set", {
          buffer_id: this.bufferId,
          query: snap.query,
          anchor: null,
          extend: false,
        });
        this.searchSummary = r.summary;
        this.committedQuery = snap.query;
        this.searchCommitted = true;
      } else {
        await this.client.rpc<null>("search/clear", { buffer_id: this.bufferId }).catch(() => {});
        this.searchSummary = null;
        this.searchCommitted = false;
        this.committedQuery = "";
      }
      if (snap) {
        this.cursor = await this.client.rpc<CursorState>("cursor/set", {
          buffer_id: this.bufferId,
          position: snap.cursor.position,
          anchor: snap.cursor.anchor,
        });
        this.bufferEl.scrollTop = snap.scrollTop;
      }
    });
  }

  /** Revert the cursor to where search started (so a non-matching query doesn't strand it). */
  private async revertToSnapshotCursor(): Promise<void> {
    const snap = this.searchSnapshot;
    if (!snap) return;
    const c = this.cursor;
    if (
      c.position.line !== snap.cursor.position.line ||
      c.position.col !== snap.cursor.position.col ||
      c.anchor.line !== snap.cursor.anchor.line ||
      c.anchor.col !== snap.cursor.anchor.col
    ) {
      this.cursor = await this.client.rpc<CursorState>("cursor/set", {
        buffer_id: this.bufferId,
        position: snap.cursor.position,
        anchor: snap.cursor.anchor,
      });
    }
  }

  private searchCountLabel(): string {
    const s = this.searchSummary;
    if (!s) return "";
    if (s.total === 0) return "no matches";
    return `${s.current_index}/${s.total}${s.truncated ? "+" : ""}`;
  }

  private updateSearchCount(): void {
    this.searchCountEl.textContent = this.searchCountLabel();
  }

  // ---- view sync (native virtual scroll) ------------------------------------------------------

  /** After a cursor-moving action: load around the cursor if it's outside the loaded window, paint,
   *  then scroll the native container the minimum needed to reveal the cursor. */
  private async ensureCursorVisible(): Promise<void> {
    if (!this.window) return;
    const cl = this.cursor.position.line;
    if (cl < this.window.first_logical_line || cl >= this.window.last_logical_line_exclusive) {
      const res = await this.client.rpc<ViewportWindowResult>("viewport/scroll", {
        viewport_id: this.viewportId,
        scroll: { logical_line: cl, sub_row: 0 },
      });
      this.window = res.window;
    }
    this.render();
    this.revealCursor();
  }

  /** Animate the native scroll to `top`, but only when the move is short enough to look good and the
   *  user hasn't asked for reduced motion; otherwise jump. revealCursor only guarantees the *target*
   *  line is loaded (ensureCursorVisible loads a window around it), so smoothly animating a long jump
   *  would glide over not-yet-loaded rows and trigger a storm of window fetches — hence we cap the
   *  smooth distance to ~1.5 viewports and snap anything longer. */
  private scrollTopTo(top: number, smooth: boolean): void {
    const target = Math.max(0, top);
    const delta = Math.abs(target - this.bufferEl.scrollTop);
    const maxSmooth = this.visibleRows() * this.cell.h * 1.5;
    if (smooth && delta > 0 && delta <= maxSmooth && !matchMedia("(prefers-reduced-motion: reduce)").matches) {
      this.bufferEl.scrollTo({ top: target, behavior: "smooth" });
    } else {
      this.bufferEl.scrollTop = target;
    }
  }

  /** Scroll the native container the minimum to bring the cursor's visual row (and, under no-wrap,
   *  its column) into view. */
  private revealCursor(): void {
    const cursorRow = this.cursorAbsoluteVisualRow();
    if (cursorRow === null) return;
    const topRow = (this.bufferEl.scrollTop - BUFFER_PAD) / this.cell.h;
    const visible = this.visibleRows();
    // Overscroll by half a row past the cursor so it lands just inside the edge (a sliver of the
    // next line shows) rather than flush against it.
    const margin = this.cell.h / 2;
    if (cursorRow < topRow) {
      this.scrollTopTo(cursorRow * this.cell.h - margin + BUFFER_PAD, true);
    } else if (cursorRow >= topRow + visible) {
      this.scrollTopTo((cursorRow - visible + 1) * this.cell.h + margin + BUFFER_PAD, true);
    }
    // Horizontal (no-wrap): keep the cursor's column clear of the sticky gutter and the right edge.
    if (this.wrap === "none") {
      const gutterPx = this.cell.w; // gutter is 1ch wide, sticky at the left
      const cx = gutterPx + this.cursor.position.col * this.cell.w;
      if (cx - this.bufferEl.scrollLeft < gutterPx) {
        this.bufferEl.scrollLeft = cx - gutterPx;
      } else if (cx + this.cell.w - this.bufferEl.scrollLeft > this.bufferEl.clientWidth) {
        this.bufferEl.scrollLeft = cx + this.cell.w - this.bufferEl.clientWidth;
      }
    }
  }

  /** Native scroll event: load a new window when the view nears the loaded window's edge. */
  private onScroll(): void {
    this.dismissHover();
    if (!this.window || this.fetchInFlight) return;
    const topRow = Math.round((this.bufferEl.scrollTop - BUFFER_PAD) / this.cell.h);
    const loadedStart = this.window.first_visual_row;
    const loadedEnd = loadedStart + this.loadedVisualRows();
    const margin = this.rows;
    const visible = this.visibleRows();
    const needAbove = loadedStart > 0 && topRow < loadedStart + margin;
    const needBelow = loadedEnd < this.window.total_visual_rows && topRow + visible > loadedEnd - margin;
    if (needAbove || needBelow) this.fetchByRow(topRow);
  }

  /** Fetch the window around an absolute visual row and reposition the loaded content (the native
   *  scrollTop is unchanged — content is absolutely placed, so there's no jump). */
  private fetchByRow(topRow: number): void {
    this.fetchInFlight = true;
    this.queue = this.queue
      .then(async () => {
        const res = await this.client.rpc<ViewportWindowResult>("viewport/scroll_to_row", {
          viewport_id: this.viewportId,
          top_visual_row: Math.max(0, topRow),
        });
        this.window = res.window;
        this.render();
      })
      .then(() => {
        this.fetchInFlight = false;
        this.onScroll(); // re-check in case the view moved further while fetching
      })
      .catch(() => {
        this.fetchInFlight = false;
      });
  }

  /** View-only scroll (keyboard): vertical adjusts the native scrollTop; horizontal shifts cols. */
  private async scrollView(dir: ScrollDir, unit: ScrollUnit, count: number): Promise<void> {
    if (!this.window) return;
    if (dir === "left" || dir === "right") {
      if (this.wrap === "soft") return; // content never overflows right under soft wrap
      const cw = this.bufferEl.clientWidth;
      const px = unit === "page" ? cw : unit === "half" ? Math.floor(cw / 2) : this.cell.w;
      const delta = (dir === "right" ? 1 : -1) * px * Math.max(1, count);
      this.bufferEl.scrollLeft = Math.max(0, this.bufferEl.scrollLeft + delta);
      return;
    }
    const visible = this.visibleRows();
    const step = unit === "page" ? visible : unit === "half" ? Math.max(1, Math.floor(visible / 2)) : 1;
    const delta = (dir === "down" ? 1 : -1) * step * Math.max(1, count) * this.cell.h;
    this.scrollTopTo(this.bufferEl.scrollTop + delta, true); // fires onScroll → fetch if needed
  }

  // ---- hover popup ----------------------------------------------------------------------------

  private showHover(text: string, opts?: { severity?: DiagnosticSeverity; markdown?: boolean }): void {
    if (opts?.markdown) this.hoverEl.replaceChildren(renderMarkdown(text));
    else this.hoverEl.textContent = text;
    this.hoverEl.className = opts?.severity ? `sev-${opts.severity}` : "";
    this.hoverEl.classList.toggle("markdown", !!opts?.markdown);
    this.placeHover();
  }

  /** Show several diagnostics stacked in the hover box, each line colored by its severity (the
   *  multi-diagnostic counterpart to `showHover`; matches the terminal's `Space j` popup). */
  private showDiagnosticsHover(items: { severity: DiagnosticSeverity; message: string }[]): void {
    this.hoverEl.className = "";
    this.hoverEl.classList.remove("markdown");
    const frag = document.createDocumentFragment();
    for (const it of items) {
      const row = document.createElement("div");
      row.className = `sev-${it.severity}`;
      row.textContent = `${severityLabel(it.severity)}: ${it.message}`;
      frag.append(row);
    }
    this.hoverEl.replaceChildren(frag);
    this.placeHover();
  }

  /** Reveal the hover box (content already set) and position it relative to the cursor cell. */
  private placeHover(): void {
    this.hoverEl.style.display = "block";
    this.hoverOpen = true;
    const cell = this.bufferEl.querySelector(".cursor") as HTMLElement | null;
    const r = (cell ?? this.bufferEl).getBoundingClientRect();
    // Measure the box (content + display are set above) and place it so it stays on-screen: below
    // the cursor when it fits, otherwise flipped above; horizontally clamped to the viewport.
    const margin = 4;
    const box = this.hoverEl.getBoundingClientRect();
    const left = Math.max(margin, Math.min(r.left, window.innerWidth - box.width - margin));
    let top = r.bottom + margin;
    if (top + box.height > window.innerHeight - margin) {
      const above = r.top - box.height - margin;
      top = above >= margin ? above : Math.max(margin, window.innerHeight - box.height - margin);
    }
    this.hoverEl.style.left = `${Math.round(left)}px`;
    this.hoverEl.style.top = `${Math.round(top)}px`;
  }

  private dismissHover(): void {
    if (!this.hoverOpen) return;
    this.hoverOpen = false;
    this.hoverEl.style.display = "none";
  }

  /** `Space j`: show the diagnostics under the cursor — all of them — in the hover box, falling
   *  back to every diagnostic on the cursor's line when none sit under the column. Mirrors the
   *  terminal client so both show the same set. A span covers the column when `start <= col < end`,
   *  widening a zero-width point (`start == end`, common for rust-analyzer "expected …" errors) to
   *  one cell so it's still reachable. */
  private showDiagnosticAtCursor(): void {
    if (!this.window) return;
    const line = this.window.lines.find((l) => l.logical_line === this.cursor.position.line);
    const diags = line?.diagnostics ?? [];
    const col = this.cursor.position.col;
    const under = diags.filter((d) => col >= d.start && col < Math.max(d.end, d.start + 1));
    const shown = under.length > 0 ? under : diags;
    if (shown.length > 0) this.showDiagnosticsHover(shown);
    else this.toast("No diagnostics on this line");
  }

  private onResize(): void {
    window.clearTimeout(this.resizeTimer);
    this.resizeTimer = window.setTimeout(() => {
      if (!this.viewportId) return;
      this.cell = measureCell(this.bufferEl); // re-measure: font size may have changed (zoom)
      this.recomputeGrid();
      this.queue = this.queue
        .then(async () => {
          const res = await this.client.rpc<ViewportWindowResult>("viewport/resize", {
            viewport_id: this.viewportId,
            cols: this.cols,
            rows: this.rows,
          });
          this.window = res.window;
          this.render();
        })
        .catch((err) => this.setStatus(`Error: ${(err as Error).message}`, true));
    }, 100);
  }

  private onNotification(method: string, params: unknown): void {
    if (method === "viewport/lines_changed") {
      const p = params as ViewportLinesChangedParams;
      if (p.viewport_id === this.viewportId) {
        // The notification carries the freshly rendered window for the loaded range — apply it
        // directly (no refetch), then keep the cursor in view with the new geometry.
        this.window = {
          first_logical_line: p.range.start_logical_line,
          last_logical_line_exclusive: p.range.end_logical_line_exclusive,
          line_count: p.line_count,
          max_scroll_logical_line: p.max_scroll_logical_line,
          total_visual_rows: p.total_visual_rows,
          first_visual_row: p.first_visual_row,
          max_line_width: p.max_line_width,
          git_status: p.git_status,
          lines: p.replacement_lines,
        };
        // Keep the revision in sync for edits that only arrive via this notification (e.g. LSP
        // format / code actions, which don't return a revision-carrying RPC result) so the dirty
        // marker updates.
        this.revision = p.revision;
        this.render();
        this.revealCursor();
      }
    } else if (method === "buffer/closed") {
      // Another client (or a path/project deletion) closed a buffer; if it's the one we're on,
      // switch to the server-indicated next buffer (or a fresh scratch when none remain).
      const p = params as BufferClosedParams;
      if (p.buffer_id === this.bufferId) {
        this.toast("buffer closed by another client", "warning");
        this.enqueue(() =>
          this.switchBuffer({ buffer_id: p.next_buffer_id ?? null, create_if_missing: false }),
        );
      }
    } else if (method === "picker/update") {
      this.picker?.onUpdate(params as PickerUpdateParams);
    } else if (method === "buffer/state") {
      const p = params as BufferStateParams;
      if (p.buffer_id === this.bufferId) {
        const wasExternal = this.externallyModified || this.externallyDeleted;
        this.savedRevision = p.saved_revision;
        this.externallyModified = p.externally_modified ?? false;
        this.externallyDeleted = p.externally_deleted ?? false;
        if (!wasExternal && this.externallyDeleted) {
          this.toast("file removed on disk — save to recreate, or close", "warning");
        } else if (!wasExternal && this.externallyModified) {
          this.toast("file changed on disk — Ctrl-s to overwrite, or reload", "warning");
        }
        if (this.window) this.render();
      }
    } else if (method === "lsp/diagnostics_changed") {
      const p = params as LspDiagnosticsChangedParams;
      if (p.buffer_id === this.bufferId) {
        this.diagCounts = p.counts;
        if (this.window) this.render();
      }
    } else if (method === "lsp/status_changed") {
      const s = params as LspServerStatus;
      this.lspStatuses.set(lspKey(s.language, s.workspace_root), s);
      if (this.window) this.render();
    } else if (method === "search/state_changed") {
      const s = params as SearchSummary;
      if (s.buffer_id === this.bufferId) {
        this.searchSummary = s;
        if (this.window) this.render();
      }
    }
    // cursor/update (other clients' cursors): later.
  }
}

const root = document.getElementById("app");
if (root) new Editor(root, resolveConfig());
