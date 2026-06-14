//! The core-driven web shell (docs/web-core.md, Phase 3 milestone (a)): the editor/buffer read+edit
//! loop running on the shared `aether-client` core compiled to wasm. The TS side is now a *shell*,
//! the same shape as `aether-tui/src/shell.rs` and `aether-iced/src/app.rs`:
//!
//!   input → WasmSession.on_key / on_event / on_rpc_result → Effect[] → execute → render(view())
//!
//! Semantic RPCs are core-issued (an `Effect.Request` we send over the socket, feeding the result
//! back through `on_rpc_result`). Geometry RPCs (`viewport/subscribe`/`scroll_to_row`/`scroll`) are
//! shell-issued — their params need pixels — but their results are adopted by the core
//! (`adopt_subscribe`/`adopt_window`); the shell then does the pixel positioning. (docs/web-core.md
//! §"Two kinds of RPC".)
//!
//! Milestone (a) scope: bootstrap, keyboard editing, native virtual scroll, server pushes. Search,
//! pickers, prompts, hover, blame refresh, mouse, clipboard, and core-driven reconnect are later
//! milestones — `view().has_picker`/`has_prompt` and a few effects are intentionally stubbed below.

import "./theme.css";
import init, { WasmSession, hover_key } from "./wasm/aether_web";
import { RpcClient, type ConnState } from "./client";
import { renderBuffer } from "./render";
import { decodeRow } from "./text";
import { statusIcon, severityIcon, lspStateClass, type IconKind } from "./icons";
import { truncatePath, charBudget } from "./paths";
import { rootLabels } from "./labels";
import { renderHoverDoc, mdToPlain, type MdBlock } from "./markdown";
import type {
  BufferOpenResult,
  BufferWindow,
  CursorState,
  DiagnosticCounts,
  LogicalPosition,
  LspServerStatus,
  PickerItem,
  PickerKind,
  ProjectActivateResult,
  ProjectListResult,
  ScrollPosition,
  ViewportSubscribeResult,
  ViewportWindowResult,
  WrapMode,
} from "./protocol";

const GUTTER_COLS = 1;
const TAB_WIDTH = 4;
const CONTINUATION_MARKER_WIDTH = 2;
const BUFFER_PAD = 8; // px of breathing room above the first line / below the last (virtual)

const PLACEHOLDER: Record<PickerKind, string> = {
  files: "Find files…",
  buffers: "Switch buffer…",
  grep: "Grep workspace…",
  explorer: "Explore files…",
  projects: "Select project…",
  diagnostics: "List diagnostics…",
  lsp_servers: "List LSPs…",
  references: "List references…",
};

interface Config {
  wsBase: string;
  project: string | undefined;
}

function resolveConfig(): Config {
  // No token: the daemon authorizes by loopback Host/Origin (served same-origin; Vite dev points
  // VITE_AETHER_WS at the daemon, whose origin check accepts localhost).
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

// ---- the boundary DTOs (mirror crates/aether-web; serialized by serde-wasm-bindgen) -------------

type ToastLevel = "info" | "error" | "warning" | "success";

interface ShellActionDesc {
  name: string;
  dir?: string;
  unit?: string;
}

/** One effect from the core (docs/web-core.md §"The boundary"). `tag` selects the variant. */
interface CoreEffect {
  tag: string;
  token?: number;
  method?: string;
  params?: unknown;
  message?: string;
  level?: ToastLevel;
  text?: string;
  paste?: unknown;
  action?: ShellActionDesc;
  hover?: HoverContent;
}

/** Hover-popover content from the core (Effect::ShowHover): rendered markdown (LSP hover) or stacked
 *  severity-coloured blocks (diagnostics-at-cursor, commit details). */
type HoverContent =
  | { kind: "markdown"; blocks: MdBlock[] }
  | { kind: "blocks"; blocks: { text: string; severity: string | null }[] };

/** Result of the core's `hover_key` resolver: what an open popover does with a key (null = none, so
 *  the shell dismisses it). Mirrors `aether_client::keymap::HoverAction`. */
type HoverKeyResult =
  | null
  | { kind: "copy" }
  | { kind: "scroll"; down: boolean; unit: "line" | "half" | "page" };

/** Map a hover block's severity label (the core sends "Error"/"Warning"/"Info"/"Hint") to its
 *  text-colour CSS class. */
function hoverSevClass(label: string): string {
  switch (label) {
    case "Error": return "sev-error";
    case "Warning": return "sev-warning";
    case "Info": return "sev-information";
    case "Hint": return "sev-hint";
    default: return "";
  }
}

interface SearchView {
  query: string;
  cursor: number;
  active: boolean;
  extend_to_cursor: boolean;
  summary: { total: number; current_index: number; truncated: boolean } | null;
}

type PromptView =
  | { kind: "confirm"; message: string }
  | { kind: "saveas"; path_index: number; input: string; cursor: number }
  | { kind: "lspinfo"; status: LspServerStatus };

interface PickerView {
  kind: PickerKind;
  query: string;
  cursor: number;
  offset: number;
  selected: number;
  items: PickerItem[];
  total_matches: number;
  total_candidates: number;
  ticking: boolean;
  total_display_rows: number;
  window_base: number;
  directory: string | null;
  directory_parent: string | null;
  /** The Explorer's synthetic "+ Create …" affordance (view.rs `create`); null when not offered.
   *  `abs` is its selection index, one past the last match. */
  create: { name: string; is_dir: boolean; abs: number } | null;
  chips: { label: string; flag: boolean }[];
  chip_selected: number | null;
  chip_editor: ChipEditorView | null;
}

interface EditorInput {
  text: string;
  cursor: number;
}

/** The glob/dir filter-creation editor (view.rs `chip_editor`). The core drives editing; the shell
 *  renders it and routes keys. `root_*` apply only to a multi-root dir editor. */
interface ChipEditorView {
  is_dir: boolean;
  field: "root" | "path";
  input: EditorInput;
  root_filter: EditorInput;
  multi_root: boolean;
  root_ghost: string | null;
  root_invalid: boolean;
  root_display: string;
  path_ghost: string | null;
  path_invalid: boolean;
}

/** One picker row's display, distilled from a `PickerItem` (the polish — path-budget truncation,
 *  git bullets, LSP icons — is deferred; this covers navigation). */
interface RowDesc {
  primary: string;
  matches?: number[];
  meta?: string;
  prefix?: string;
  prefixClass?: string;
  dir?: boolean;
  italic?: boolean;
  suffix?: string;
  /** Reserve the leading status-bullet cell (keeps names aligned); `bulletStatus` colours the dot,
   *  or `bulletIcon` draws an SVG status icon (LSP rows) in the cell instead. */
  bullet?: boolean;
  bulletStatus?: string;
  bulletIcon?: IconKind;
  bulletSpin?: boolean;
  /** Colour class for `bulletIcon` (the SVG inherits `currentColor`). Defaults to the icon kind
   *  name (LSP rows use `.lsp-*`); diagnostics pass a `.sev-*` class. */
  bulletIconClass?: string;
  /** Buffer dirty-state dot on the right (non-"clean"). */
  dirty?: string;
  /** Ignored entry — dim the text, no bullet. */
  dim?: boolean;
}

/** The render projection from `WasmSession.view()` (the editor/status/search/prompt slice; view.rs). */
interface CoreView {
  mode: "normal" | "insert" | "search";
  wrap: WrapMode;
  diff_view: boolean;
  window: BufferWindow | null;
  viewport_id: number | null;
  buffer: {
    buffer_id: number;
    path: string | null;
    label: string;
    cursor: CursorState;
    scroll: ScrollPosition | null;
    revision: number;
    saved_revision: number;
    transient: boolean;
  };
  blame: { line: number; text: string } | null;
  count: number | null;
  pending: unknown | null;
  project: string;
  project_paths: string[];
  externally_modified: boolean;
  externally_deleted: boolean;
  diagnostics: DiagnosticCounts;
  lsp: LspServerStatus | null;
  search: SearchView;
  prompt: PromptView | null;
  picker: PickerView | null;
}

/** Cumulative visual rows before `line` in the loaded window (phantom rows included), or null when
 *  the line isn't loaded — mirrors `grid::rows_before_line`, used to position a restored scroll. */
function rowsBeforeLine(w: BufferWindow, line: number): number | null {
  if (line < w.first_logical_line || line >= w.last_logical_line_exclusive) return null;
  let rows = 0;
  for (const l of w.lines) {
    if (l.logical_line === line) return rows;
    rows += (l.virtual_rows_above?.length ?? 0) + l.visual_rows.length;
  }
  return null;
}

function basename(p: string): string {
  const parts = p.split("/").filter(Boolean);
  return parts.length ? parts[parts.length - 1] : p;
}

/** Buffer-state accent colour for the status dot (ported from the old client): deleted-on-disk →
 *  red, externally-changed → orange, unsaved edits → frost blue, clean → none. */
function bufferStateColor(v: CoreView): string | null {
  if (v.externally_deleted) return "#bf616a";
  if (v.externally_modified) return "#d08770";
  if (v.buffer.revision !== v.buffer.saved_revision) return "#81a1c1";
  return null;
}

function searchCountLabel(s: SearchView["summary"]): string {
  if (!s) return "";
  if (s.total === 0) return "no matches";
  return `${s.current_index}/${s.total}${s.truncated ? "+" : ""}`;
}

/** Cursor `line:col`, or a `lo-hi` selection range (Normal mode with an extended selection). */
function positionLabel(v: CoreView): string {
  const p = v.buffer.cursor.position;
  const a = v.buffer.cursor.anchor;
  if (v.mode === "insert" || (p.line === a.line && p.col === a.col)) {
    return `${p.line + 1}:${p.col + 1}`;
  }
  const before = (x: LogicalPosition, y: LogicalPosition) =>
    x.line < y.line || (x.line === y.line && x.col <= y.col);
  const lo = before(p, a) ? p : a;
  const hi = before(p, a) ? a : p;
  return lo.line === hi.line
    ? `${lo.line + 1}:${lo.col + 1}-${hi.col + 1}`
    : `${lo.line + 1}:${lo.col + 1}-${hi.line + 1}:${hi.col + 1}`;
}

/** LSP status icon + colour class: ready / busy (ready + active progress, or starting) / crashed /
 *  stopped — mirroring the TUI status indicator. Busy spins. */
function lspIcon(lsp: LspServerStatus | null): { kind: IconKind; cls: string; spin: boolean } | null {
  if (!lsp) return null;
  const state = lsp.status.state;
  const busy = state === "ready" && (lsp.progress?.length ?? 0) > 0;
  const cls = busy ? "lsp-busy" : lspStateClass(state);
  return { kind: cls, cls, spin: cls === "lsp-busy" };
}

/** The explorer breadcrumb: the listed directory shown *within* its project root (ported from the
 *  old client's `explorerDisplayPath`). Empty at a single root's top; `root: rel/` under multi-root.
 *  (Deferred polish: the disambiguated root label + path-budget elision — uses the basename here.) */
function explorerPrefix(dir: string | null, projectPaths: string[]): string {
  if (!dir) return ""; // roots mode — the rows already say "pick a root"
  let best = "";
  let bestIdx = -1;
  projectPaths.forEach((root, i) => {
    const norm = root.endsWith("/") ? root.slice(0, -1) : root;
    if ((dir === norm || dir.startsWith(norm + "/")) && norm.length > best.length) {
      best = norm;
      bestIdx = i;
    }
  });
  if (bestIdx < 0) return `${dir}/`;
  const label = projectPaths.length > 1 ? `${basename(best)}: ` : "";
  if (dir === best) return label;
  return `${label}${dir.slice(best.length + 1)}/`;
}

/** Bold the fuzzy-matched code points (`indices` are char offsets into `text`). */
function matched(text: string, indices?: number[]): DocumentFragment {
  const frag = document.createDocumentFragment();
  if (!indices || indices.length === 0) {
    frag.append(text);
    return frag;
  }
  const set = new Set(indices);
  const cps = [...text];
  let i = 0;
  while (i < cps.length) {
    const hit = set.has(i);
    let j = i + 1;
    while (j < cps.length && set.has(j) === hit) j++;
    const chunk = cps.slice(i, j).join("");
    if (hit) {
      const b = document.createElement("b");
      b.className = "match";
      b.textContent = chunk;
      frag.append(b);
    } else {
      frag.append(chunk);
    }
    i = j;
  }
  return frag;
}

/** Distil a `PickerItem` to its row display. `labels` is the disambiguated per-root label set
 *  (`rootLabels`, "" for single-root); `budget` is the char allowance for paths (segment-elided). */
function describePickerItem(
  item: PickerItem,
  projectPaths: string[],
  labels: string[],
  budget: number,
): RowDesc {
  switch (item.kind) {
    case "file": {
      // Multi-root: a dim, disambiguated root label after the path (basenames alone read alike).
      const suffix =
        labels.length > 1 ? (labels[item.path_index] ?? `root ${item.path_index}`) : undefined;
      const pb = Math.max(8, budget - 2 - (suffix ? [...suffix].length + 2 : 0));
      const { display, indices } = truncatePath(item.relative_path, item.match_indices, pb);
      // Files-picker entries never include ignored files, so any git_status is a real change.
      return {
        primary: display,
        matches: indices,
        suffix,
        bullet: true,
        bulletStatus: item.git_status,
      };
    }
    case "buffer":
      return {
        primary: item.display,
        matches: item.match_indices,
        italic: item.transient,
        dirty: item.status && item.status !== "clean" ? item.status : undefined,
      };
    case "grep_hit": {
      // match_indices index the untrimmed preview; shift them by the stripped leading whitespace.
      const trimmed = item.preview.trimStart();
      const lead = [...item.preview].length - [...trimmed].length;
      return {
        primary: trimmed.trimEnd(),
        matches: item.match_indices?.map((i) => i - lead).filter((i) => i >= 0),
        meta: `${item.line + 1}`,
      };
    }
    case "diagnostic":
      return {
        primary: item.message.split("\n")[0],
        matches: item.match_indices,
        // Same SVG icon + severity colour the status-bar count uses.
        bullet: true,
        bulletIcon: severityIcon(item.severity),
        bulletIconClass: `sev-${item.severity}`,
        meta: `${item.line + 1}:${item.col}`,
      };
    case "dir_entry": {
      const st = item.git_status;
      return {
        primary: item.is_dir ? `${item.name}/` : item.name,
        matches: item.match_indices,
        dir: item.is_dir,
        bullet: true,
        bulletStatus: st && st !== "ignored" ? st : undefined,
        dim: st === "ignored",
      };
    }
    case "root": {
      const p = projectPaths[item.path_index];
      return { primary: `${p ? basename(p) : `root ${item.path_index}`}/`, matches: item.match_indices, dir: true };
    }
    case "project":
      return { primary: item.name, matches: item.match_indices };
    case "lsp_server": {
      // The status bar's SVG icon in the leading cell (spinning when busy); language as the meta.
      const busy = item.status.state === "ready" && (item.progress?.length ?? 0) > 0;
      const cls = busy ? "lsp-busy" : lspStateClass(item.status.state);
      return {
        primary: item.name,
        matches: item.match_indices,
        meta: item.language,
        bullet: true,
        bulletIcon: cls,
        bulletSpin: busy,
      };
    }
    case "reference": {
      // Dim `path:line` location prefix, path-elided to ~half the row so the filename survives.
      const linePart = `:${item.line + 1}`;
      const pb = Math.max(8, Math.floor(budget / 2) - [...linePart].length);
      const { display } = truncatePath(item.display_path, undefined, pb);
      return {
        primary: item.preview,
        matches: item.match_indices,
        prefix: `${display}${linePart}`,
        prefixClass: "picker-loc",
      };
    }
  }
}

export class Shell {
  private readonly bufferEl: HTMLElement;
  private readonly statusEl: HTMLElement;
  private readonly toastsEl: HTMLElement;
  private readonly connBanner: HTMLElement;
  private readonly searchBar: HTMLElement;
  private readonly searchInput: HTMLInputElement;
  private readonly searchPrefixEl: HTMLElement;
  private readonly searchCountEl: HTMLElement;
  private readonly overlayEl: HTMLElement;
  /** Save-as has its own persistent overlay (with a native <input>); confirm/lsp-info prompts are
   *  rebuilt in overlayEl (no input, so rebuilding is fine). */
  private readonly saveAsEl: HTMLElement;
  private readonly saveAsInput: HTMLInputElement;
  private readonly saveAsPrefixEl: HTMLElement;
  private readonly pickerEl: HTMLElement;
  private readonly pickerInput: HTMLInputElement;
  private readonly pickerPathEl: HTMLElement;
  private readonly pickerCountEl: HTMLElement;
  /** CSS-animated throbber to the left of the count, shown while a search streams. */
  private readonly pickerSpinnerEl: HTMLElement;
  private readonly pickerChipsEl: HTMLElement;
  private readonly pickerEditorRow: HTMLElement;
  private readonly pickerListEl: HTMLElement;
  /** The dir/glob chip editor's native inputs (real caret/selection/IME). Persistent across renders
   *  so they keep focus; only the *focused* segment is mounted as an input (the other is a clickable
   *  span). The structure (which segment is the input) is rebuilt only when `editorStructKey` changes
   *  — never per keystroke, which would drop a live input's caret. */
  private readonly editorPathInput: HTMLInputElement;
  private readonly editorRootInput: HTMLInputElement;
  private editorPathGhost: HTMLElement | null = null;
  private editorRootGhost: HTMLElement | null = null;
  private editorRootSpan: HTMLElement | null = null;
  private editorPathSpan: HTMLElement | null = null;
  private editorSepEl: HTMLElement | null = null;
  private editorStructKey: string | null = null;
  /** A hidden, always-focused textarea. Keystrokes are handled by the global keydown → the core, so
   *  it captures nothing itself (handled keys are preventDefaulted; stray ones are cleared). Its sole
   *  job is to keep a form field focused, which stops Firefox/Chrome opening the menu bar on Alt. */
  private readonly capture: HTMLTextAreaElement;
  private readonly cell: Cell;
  private client!: RpcClient;
  private session!: WasmSession;

  private cols = 80;
  private rows = 24;
  /** The most recent `view()` — refreshed every `render()`, read by the geometry methods so they
   *  don't re-serialize the window on every scroll event. */
  private snapshot: CoreView | null = null;
  /** Pending coalesced-render frame (see `scheduleRender`); null when none is queued. */
  private renderRaf: number | null = null;
  /** Set by the `RevealPickerSelection` effect: the next picker render scrolls the highlighted row
   *  into view (keyboard nav / refetch reveal). Free wheel-scrolling never sets it. */
  private pickerReveal = false;
  /** Set by the `PickerScrollReset` effect (a query change): the next picker render jumps to the top. */
  private pickerScrollReset = false;
  /** Measured picker display-row height (px), for the virtual-scroll spacer + window positioning.
   *  Defaults to the native client's 24; re-measured from a real row each render. */
  private pickerRowH = 24;
  /** The address-bar URL we last wrote (the boot URL scheme reflecting the current buffer + cursor),
   *  and a debounce timer so a burst of cursor moves coalesces into one replaceState. */
  private lastUrl: string | null = null;
  private urlTimer: number | undefined;
  /** The hover popover (LSP hover / diagnostics-at-cursor / commit details). The core decides the
   *  content (Effect::ShowHover); this element renders+positions it. Anchored at the cursor cell,
   *  scrollable, dismissed on the next key / scroll / click / buffer switch. */
  private readonly hoverEl: HTMLElement;
  private readonly hoverStrut: HTMLElement;
  private hoverOpen = false;
  /** Content of the currently-shown popover, retained so Ctrl-y can copy it as plain text. */
  private hoverContent?: HoverContent;
  /** The keyboard-shortcut help overlay (Space ?). A shell-local overlay — the core only triggers it
   *  (Effect::ShellAction OpenHelp); its content is sourced from the core's keymap (help_entries) and
   *  its tab/scroll/close keys are handled here, not the core. Built once, cached. */
  private readonly helpEl: HTMLElement;
  private readonly helpTabsEl: HTMLElement;
  private readonly helpGridEl: HTMLElement;
  private helpOpen = false;
  private helpTab = 0;
  private helpData: { label: string; sections: { title: string; rows: [string, string][] }[] }[] | null = null;
  private helpTabEls: HTMLElement[] = [];
  /** Monotonic id for the latest `viewport/subscribe`. Async viewport results (subscribe / fetch /
   *  reveal-scroll) captured at an older epoch are dropped, so a superseded request can't reinstate a
   *  stale window — the robust guard against the reply/push interleaving + concurrent-jump races. */
  private viewportEpoch = 0;
  private fetchInFlight = false;
  /** A left-button drag-select is in progress (mousedown → mouseup), extending the selection. */
  private dragging = false;
  /** The search prompt's Esc-restore scroll position (`SaveScrollAnchor` effect). */
  private scrollAnchor: number | null = null;
  /** Whether the picker overlay is open — gates focus handling (its <input> owns focus). */
  private pickerOpen = false;
  /** Open-transition tracking for the search bar / save-as overlay (refocus the buffer on close). */
  private searchOpen = false;
  private saveAsOpen = false;
  /** A paste gesture awaiting its native `paste` event (Ctrl-v): the effect's paste descriptor,
   *  held so the paste-event handler can feed the clipboard text back without a permission prompt. */
  private pendingPaste: unknown = null;
  /** An IME composition is in progress on the capture textarea — `on_key` stands down so the
   *  composed text isn't double-inserted; it's flushed on compositionend (`insert_text`). */
  private composing = false;

  constructor(root: HTMLElement, cfg: Config) {
    this.bufferEl = document.createElement("div");
    this.bufferEl.id = "buffer";
    this.bufferEl.tabIndex = 0;
    this.statusEl = document.createElement("div");
    this.statusEl.id = "status";
    this.toastsEl = document.createElement("div");
    this.toastsEl.id = "toasts";
    this.toastsEl.setAttribute("role", "status");
    this.toastsEl.setAttribute("aria-live", "polite");
    this.connBanner = document.createElement("div");
    this.connBanner.id = "conn-banner";
    this.connBanner.setAttribute("role", "status");
    this.connBanner.setAttribute("aria-live", "polite");
    this.connBanner.style.display = "none";
    // Search bar (shown in Search mode) — a persistent native <input> that owns text editing and
    // syncs to the core (search_set_query); nav/commit/cancel keys route through on_key.
    this.searchBar = document.createElement("div");
    this.searchBar.id = "searchbar";
    this.searchBar.style.display = "none";
    this.searchPrefixEl = document.createElement("span");
    this.searchPrefixEl.className = "search-count";
    this.searchInput = document.createElement("input");
    this.searchInput.className = "search-input";
    this.searchInput.spellcheck = false;
    this.searchInput.autocapitalize = "off";
    this.searchInput.setAttribute("autocomplete", "off");
    this.searchCountEl = document.createElement("span");
    this.searchCountEl.className = "search-count";
    this.searchBar.append(this.searchPrefixEl, this.searchInput, this.searchCountEl);
    this.searchInput.addEventListener("input", () => {
      if (this.session) {
        this.runEffects(this.session.search_set_query(this.searchInput.value) as CoreEffect[]);
      }
    });
    this.searchInput.addEventListener("keydown", (e) => this.onSearchInputKey(e));

    // The modal overlay host for confirm / lsp-info prompts (rebuilt — no input, so that's fine).
    this.overlayEl = document.createElement("div");
    this.overlayEl.style.display = "none";

    // Save-as has its own persistent overlay with a native path <input> (owns editing, syncs via
    // prompt_set_input); accept/cancel route through on_key.
    this.saveAsEl = document.createElement("div");
    this.saveAsEl.className = "overlay";
    this.saveAsEl.style.display = "none";
    const saveModal = document.createElement("div");
    saveModal.className = "modal";
    const saveMsg = document.createElement("div");
    saveMsg.className = "modal-message";
    saveMsg.textContent = "Save as";
    const saveField = document.createElement("div");
    saveField.className = "modal-field";
    this.saveAsPrefixEl = document.createElement("span");
    this.saveAsPrefixEl.className = "search-count";
    this.saveAsInput = document.createElement("input");
    this.saveAsInput.className = "modal-input";
    this.saveAsInput.spellcheck = false;
    this.saveAsInput.autocapitalize = "off";
    this.saveAsInput.setAttribute("autocomplete", "off");
    saveField.append(this.saveAsPrefixEl, this.saveAsInput);
    saveModal.append(saveMsg, saveField);
    this.saveAsEl.append(saveModal);
    this.saveAsInput.addEventListener("input", () => {
      if (this.session) {
        this.runEffects(this.session.prompt_set_input(this.saveAsInput.value) as CoreEffect[]);
      }
    });
    this.saveAsInput.addEventListener("keydown", (e) => this.onSaveAsInputKey(e));
    // The picker overlay is persistent DOM (built once) so its native <input> keeps focus + caret
    // across re-renders — only the results list is rebuilt. The input owns text editing and syncs
    // its value to the core (picker_set_query); nav/accept/cancel keys route through on_key.
    this.pickerEl = document.createElement("div");
    this.pickerEl.className = "overlay";
    this.pickerEl.style.display = "none";
    const pickerPanel = document.createElement("div");
    pickerPanel.className = "picker";
    const pickerInputRow = document.createElement("div");
    pickerInputRow.className = "picker-input-row";
    this.pickerPathEl = document.createElement("span");
    this.pickerPathEl.className = "picker-path";
    this.pickerPathEl.style.display = "none";
    this.pickerInput = document.createElement("input");
    this.pickerInput.className = "picker-input";
    this.pickerInput.spellcheck = false;
    this.pickerInput.autocapitalize = "off";
    this.pickerInput.setAttribute("autocomplete", "off");
    this.pickerCountEl = document.createElement("span");
    this.pickerCountEl.className = "picker-count";
    this.pickerSpinnerEl = document.createElement("span");
    this.pickerSpinnerEl.className = "picker-spinner";
    this.pickerSpinnerEl.style.display = "none";
    // Chips lead the row, left of the breadcrumb + query they prefix (`:empty` hides the box).
    this.pickerChipsEl = document.createElement("div");
    this.pickerChipsEl.className = "picker-chips";
    pickerInputRow.append(
      this.pickerChipsEl,
      this.pickerPathEl,
      this.pickerInput,
      this.pickerSpinnerEl,
      this.pickerCountEl,
    );
    this.pickerEditorRow = document.createElement("div");
    this.pickerEditorRow.className = "picker-editor-row";
    this.pickerEditorRow.style.display = "none";
    this.pickerListEl = document.createElement("div");
    this.pickerListEl.className = "picker-list";
    // Scrolling into an unloaded range refetches the window around it (no selection change) — the
    // native client's virtual scroll. `scrolled_refetch` no-ops when the window already covers the
    // view, so firing on every scroll event is cheap.
    this.pickerListEl.addEventListener("scroll", () => this.onPickerListScroll(), { passive: true });
    pickerPanel.append(pickerInputRow, this.pickerEditorRow, this.pickerListEl);
    this.pickerEl.append(pickerPanel);
    this.pickerInput.addEventListener("input", () => {
      if (this.session) {
        this.runEffects(this.session.picker_set_query(this.pickerInput.value) as CoreEffect[]);
      }
    });
    this.pickerInput.addEventListener("keydown", (e) => this.onPickerInputKey(e));
    // Click outside the panel (on the dim backdrop) dismisses the whole picker — the natural
    // "click away" gesture. mousedown, not click, so it beats the focus self-heal; preventDefault
    // keeps focus where it is until the close re-targets it to the buffer.
    this.pickerEl.addEventListener("mousedown", (e) => {
      if (e.target === this.pickerEl && this.session) {
        e.preventDefault();
        // No project selected yet: the chooser is mandatory — a click outside it must not close it.
        if (this.snapshot?.buffer.buffer_id === 0) return;
        this.runEffects(this.session.close_picker() as CoreEffect[]);
      }
    });
    // The chip editor's native inputs: the path field (glob text / dir path) and, for a multi-root
    // dir editor, the root typeahead. Editing stays native; the value syncs to the core, which owns
    // the suggestion/validity logic and feeds the ghost back through the view.
    this.editorPathInput = document.createElement("input");
    this.editorRootInput = document.createElement("input");
    for (const inp of [this.editorPathInput, this.editorRootInput]) {
      inp.className = "picker-editor-input";
      inp.spellcheck = false;
      inp.autocapitalize = "off";
      inp.setAttribute("autocomplete", "off");
      inp.addEventListener("keydown", (e) => this.onEditorKey(e));
    }
    this.editorPathInput.addEventListener("input", () => {
      if (this.session) this.runEffects(this.session.chip_editor_set_input(this.editorPathInput.value) as CoreEffect[]);
    });
    this.editorRootInput.addEventListener("input", () => {
      if (this.session) this.runEffects(this.session.chip_editor_set_root_filter(this.editorRootInput.value) as CoreEffect[]);
    });
    // The wheel scrolls the list natively (overflow-y: auto) without touching the selection — like
    // the native client. The highlight only moves on keyboard nav, which reveals it via the
    // `RevealPickerSelection` effect; free scrolling leaves it where it is.
    this.capture = document.createElement("textarea");
    this.capture.className = "clipboard-capture";
    this.capture.tabIndex = -1;
    this.capture.setAttribute("aria-hidden", "true");
    this.capture.spellcheck = false;
    this.capture.autocapitalize = "off";
    this.capture.setAttribute("autocomplete", "off");
    // It must never accumulate text: every handled key is preventDefaulted, but clear any stray
    // character an unbound key let through.
    this.capture.addEventListener("input", () => {
      if (!this.composing) this.capture.value = ""; // keep empty (don't disturb a live composition)
    });
    // IME composition (CJK, dead keys, etc.): the keydowns flow to the textarea (onKeyDown stands
    // down while composing); the committed string is inserted on compositionend.
    this.capture.addEventListener("compositionstart", () => {
      this.composing = true;
    });
    this.capture.addEventListener("compositionend", (e) => {
      this.composing = false;
      const text = e.data;
      this.capture.value = "";
      if (text && this.snapshot?.mode === "insert" && this.session) {
        this.runEffects(this.session.insert_text(text) as CoreEffect[]);
      }
    });
    // Native paste (Ctrl-v): when a paste gesture is pending, take the text from the event — no
    // clipboard-read permission prompt (the whole reason input lives on a focused textarea).
    this.capture.addEventListener("paste", (e) => {
      const paste = this.pendingPaste;
      this.pendingPaste = null;
      if (!paste) return; // not our gesture (e.g. a stray right-click paste) — ignore
      e.preventDefault();
      const text = e.clipboardData?.getData("text") ?? "";
      if (this.session) this.runEffects(this.session.clipboard_read(paste, text) as CoreEffect[]);
    });
    // Self-heal focus across the whole document: if it ever drifts to <body> (a stray click, an
    // element being removed, the Firefox menu trying to grab it), snap it back to the field that
    // should own the keyboard. This keeps keystrokes captured for buffer AND overlays — without it,
    // an unfocused overlay input is a dead state (its keydown can't fire, and the window handler has
    // deferred to it). Next tick — a synchronous refocus inside focusout is ignored.
    document.addEventListener("focusout", () => {
      window.setTimeout(() => {
        if (document.activeElement && document.activeElement !== document.body) return; // moved to a real field
        this.ensureFocus();
      }, 0);
    });
    // A zero-content strut placed before the popover in the spacer; its height sets the popover's
    // flow offset to the anchor line. (A large `margin-top` on the popover itself would block its
    // `position: sticky` bottom-edge clamp — the reserved margin can't be shifted up; a real strut
    // can. See `positionHover`.)
    this.hoverStrut = document.createElement("div");
    this.hoverStrut.className = "hover-strut";
    this.hoverStrut.style.pointerEvents = "none";
    this.hoverEl = document.createElement("div");
    this.hoverEl.id = "hover";
    // The popover is a sticky child of the buffer's spacer (see `placeHover`). Wheeling over it
    // scrolls its own overflow, never the buffer (CSS `overscroll-behavior: contain` + this guard);
    // and a mousedown on it (e.g. dragging its scrollbar) must not reach the buffer's
    // click-to-dismiss handler.
    this.hoverEl.addEventListener("wheel", (e) => e.stopPropagation());
    this.hoverEl.addEventListener("mousedown", (e) => e.stopPropagation());
    // The help overlay (Space ?): a backdrop + a tabbed, scrollable modal. Content is filled lazily
    // from the core's keymap on first open; clicking the backdrop closes it.
    this.helpEl = document.createElement("div");
    this.helpEl.className = "overlay";
    this.helpEl.style.display = "none";
    const helpBox = document.createElement("div");
    helpBox.className = "modal help";
    this.helpTabsEl = document.createElement("div");
    this.helpTabsEl.className = "help-tabs";
    this.helpGridEl = document.createElement("div");
    this.helpGridEl.className = "help-grid";
    helpBox.append(this.helpTabsEl, this.helpGridEl);
    this.helpEl.append(helpBox);
    this.helpEl.addEventListener("mousedown", (e) => {
      if (e.target === this.helpEl) this.closeHelp();
    });
    root.append(
      this.bufferEl,
      this.capture,
      this.statusEl,
      this.searchBar,
      this.toastsEl,
      this.overlayEl,
      this.saveAsEl,
      this.pickerEl,
      // `hoverEl` is not appended here — it's parented into the buffer's spacer while shown (so it
      // can be `position: sticky` relative to the scrolling buffer) and removed on dismiss.
      this.helpEl,
      this.connBanner,
    );

    this.cell = measureCell(this.bufferEl);

    this.bufferEl.addEventListener("scroll", () => this.onScroll(), { passive: true });
    this.bufferEl.addEventListener("mousedown", (e) => this.onBufferMouseDown(e));
    window.addEventListener("mousemove", (e) => this.onMouseMove(e));
    window.addEventListener("mouseup", () => this.onMouseUp());
    window.addEventListener("resize", () => this.onResize());
    window.addEventListener("keydown", (e) => this.onKeyDown(e));
    // The editor owns the whole keyboard, so suppress browser keyup defaults too (e.g. Firefox
    // decides menu-bar focus on the Alt keyup). Hard-reserved combos ignore this and still work.
    window.addEventListener("keyup", (e) => e.preventDefault());

    this.capture.focus();

    void this.boot(cfg);
  }

  // ---- bootstrap ------------------------------------------------------------------------------

  private async boot(cfg: Config): Promise<void> {
    await init(); // instantiate the wasm module

    const url = `${cfg.wsBase}/?client_version=web-core-0.1`;
    this.client = new RpcClient(url, (m, p) => this.onNotification(m, p), {
      onConnState: (s) => this.onConnState(s),
      onReconnect: () => void this.reestablish(),
    });

    try {
      await this.client.ready;
      const list = await this.client.rpc<ProjectListResult>("project/list", {});
      // The URL drives which project + buffer to open, so a picker link (Ctrl/Cmd-click → new tab)
      // lands on the right file, and the tab is reloadable/shareable. Falls back to the configured/
      // first project and its last (MRU) buffer or a fresh scratch.
      const sp = new URLSearchParams(location.search);
      const urlProject = sp.get("project");
      const urlFile = sp.get("file");
      const urlRoot = Number(sp.get("root")) || 0;
      const urlBufferRaw = sp.get("buffer");
      const urlBuffer =
        urlBufferRaw != null && Number.isInteger(Number(urlBufferRaw)) ? Number(urlBufferRaw) : null;
      const known = list.projects.some((pr) => pr.name === urlProject);
      const specified = (known ? urlProject : null) ?? cfg.project ?? null;
      // A URL-directed open (file/buffer link) opens separately; otherwise `open_last` folds the
      // landing buffer into the activate.
      const directed = Boolean(urlFile) || urlBuffer != null;
      // Project selection is explicit. With none specified (and not a direct file/buffer link) we
      // DON'T activate one: keep a placeholder session and raise the Projects chooser — nothing is
      // rendered behind it. Picking a project activates it (PickerSelected → ProjectActivated →
      // adopt_switch) and the editor first appears then. Matches the native shells' no-args start.
      if (specified === null && !directed) {
        this.session = new WasmSession();
        this.runEffects(this.session.open_projects() as CoreEffect[]);
        this.capture.focus();
        return;
      }
      const name = specified ?? list.projects[0]?.name;
      if (!name) {
        this.toast("No projects configured on the server.", "error");
        return;
      }
      const activated = await this.client.rpc<ProjectActivateResult>("project/activate", {
        name,
        open_last: !directed,
      });
      const lastOrScratch = (): Promise<BufferOpenResult> =>
        this.client.rpc<BufferOpenResult>("buffer/open", {
          buffer_id: activated.last_buffer_id ?? null,
          create_if_missing: false,
          ...(activated.last_buffer_id == null ? { transient: true } : {}),
        });
      const jump = this.parseFragment(location.hash); // `#L:C` from a grep-hit / shared-cursor link
      let open: BufferOpenResult;
      if (urlFile) {
        try {
          open = await this.client.rpc<BufferOpenResult>("buffer/open", {
            path_index: urlRoot,
            relative_path: urlFile,
            create_if_missing: false,
            ...(jump ? { jump_to: jump } : {}),
          });
        } catch {
          this.toast(`could not open ${urlFile}`, "warning");
          open = await lastOrScratch();
        }
      } else if (urlBuffer != null) {
        // A scratch-buffer link (`?buffer=<id>`); the id is session-scoped, so fall back if stale.
        try {
          open = await this.client.rpc<BufferOpenResult>("buffer/open", {
            buffer_id: urlBuffer,
            create_if_missing: false,
          });
        } catch {
          open = await lastOrScratch();
        }
      } else {
        open = activated.opened ?? (await lastOrScratch());
      }

      this.session = WasmSession.bootstrap(activated.project.name, activated.project.paths, open);
      await this.subscribe(); // derives its scroll from the buffer (open.scroll / cursor)
      this.capture.focus(); // ensure the menu-suppressing field has focus once we're live
    } catch (e) {
      this.toast(`bootstrap failed: ${String(e)}`, "error");
    }
  }

  // ---- the core loop --------------------------------------------------------------------------

  private view(): CoreView {
    return this.session.view() as CoreView;
  }

  /** An overlay with a native <input> (picker, search bar, save-as) owns the keyboard: its own
   *  keydown routes to the core, so the window-level handler must not also swallow/route the event.
   *  Confirm/lsp-info prompts have no input and stay on the window handler. */
  private overlayOwnsKeyboard(): boolean {
    const v = this.snapshot;
    return !!(v && (v.picker || v.mode === "search" || v.prompt?.kind === "saveas"));
  }

  /** The element that should hold focus for the current state: an open text-overlay's `<input>` (it
   *  needs focus for native typing/IME, and its keydown routes commands to the core), otherwise the
   *  hidden capture field (keeping a form field focused is what suppresses the Firefox Alt menu). */
  private focusTarget(): HTMLElement {
    const v = this.snapshot;
    const ce = v?.picker?.chip_editor;
    if (ce) return ce.is_dir && ce.multi_root && ce.field === "root" ? this.editorRootInput : this.editorPathInput;
    if (v?.picker) return this.pickerInput;
    if (v?.mode === "search") return this.searchInput;
    if (v?.prompt?.kind === "saveas") return this.saveAsInput;
    return this.capture;
  }

  /** Keep the keyboard captured by snapping focus back to the field that should own it. Without this,
   *  if focus drifts to <body> while an overlay is open, the global keydown handler stands down for it
   *  (overlayOwnsKeyboard) AND its input can't fire its own keydown — so keys are dropped. Self-healing
   *  removes that dead state; it also keeps the menu-suppressing capture field focused in buffer mode.
   *  Generalises the capture-blur idiom to every overlay. */
  private ensureFocus(): void {
    const target = this.focusTarget();
    if (document.activeElement !== target) target.focus();
  }

  private onKeyDown(e: KeyboardEvent): void {
    // Overlay inputs (picker query, chip editor, search, save-as) have their own keydown handlers;
    // ignore events they already handled so the same keypress isn't re-processed here once an overlay
    // closes mid-event (e.g. Enter on the LSP-servers picker → LSP-info dialog, which the bubbled
    // event would otherwise immediately close). preventDefault doesn't stop propagation; this does.
    const t = e.target;
    if (
      t === this.pickerInput ||
      t === this.editorPathInput ||
      t === this.editorRootInput ||
      t === this.searchInput ||
      t === this.saveAsInput
    ) {
      return;
    }
    // The help overlay owns the keyboard while open (tab switching, scrolling, close).
    if (this.helpOpen) {
      e.preventDefault();
      this.handleHelpKey(e);
      return;
    }
    // While a hover popover is open, it reuses the editor's own Copy / Scroll bindings — resolved
    // by the core (`keymap::hover_action` via the wasm `hover_key`), so the chords never drift.
    // Copy/scroll keep it open; any other key dismisses it — Esc is then consumed, every other key
    // still acts on the buffer. (Content is also freely mouse-selectable; theme.css lifts
    // `user-select` on #hover — this Ctrl-y path is the copy-all.)
    if (this.hoverOpen) {
      const ha = hover_key(e.key, e.ctrlKey, e.altKey, e.shiftKey) as HoverKeyResult;
      if (ha?.kind === "copy") {
        e.preventDefault();
        void navigator.clipboard?.writeText(this.hoverPlainText()).catch(() => {});
        this.toast("copied popover", "success");
        return;
      }
      if (ha?.kind === "scroll") {
        e.preventDefault();
        this.hoverEl.scrollBy({ top: this.hoverScrollDelta(ha) });
        return;
      }
      // A lone modifier press (e.g. holding Alt to begin an Alt-Up chord) must not dismiss the
      // popover — fall through to the shared modifier guard below, which swallows it.
      const loneModifier =
        e.key === "Shift" || e.key === "Control" || e.key === "Alt" || e.key === "Meta";
      if (!loneModifier) {
        this.dismissHover();
        if (e.key === "Escape") {
          e.preventDefault();
          return;
        }
      }
    }
    // A native-input overlay owns the keyboard while open; let its own handler take the event.
    if (this.overlayOwnsKeyboard()) return;
    // IME composition: let it run on the focused capture textarea (the composed text is flushed on
    // compositionend → insert_text). keyCode 229 is the IME-processing sentinel on the starting key.
    if (e.isComposing || e.keyCode === 229) return;
    // The editor owns the keyboard: preventDefault every key unconditionally. This is the correct
    // model for a modal editor — it suppresses the cancellable browser behaviors the editor should
    // own (the Firefox Alt menu, Space-scroll, `/` quick-find, Ctrl-S/P/F, Backspace-back) while
    // hard-reserved combos (Ctrl-W/T/N, F5/F11/F12, Ctrl-L) ignore preventDefault and keep working.
    // Tying this to whether the core returned effects was the bug: many handled keys (leader,
    // opening a prompt, a mode switch) produce no effect, so those leaked to the browser.
    // EXCEPT Ctrl/Cmd-V: leave its default so the native `paste` event fires into the capture
    // textarea (no clipboard-read prompt). The core still processes it (→ ReadClipboard); the
    // paste-event handler supplies the text.
    const isPaste = (e.ctrlKey || e.metaKey) && !e.altKey && (e.key === "v" || e.key === "V");
    if (!isPaste) e.preventDefault();
    // A lone modifier keydown isn't fed to the core (it would disturb pending captures).
    if (e.key === "Shift" || e.key === "Control" || e.key === "Alt" || e.key === "Meta") return;
    if (!this.session) return;
    const effects = this.session.on_key(
      e.key,
      e.ctrlKey,
      e.altKey,
      e.shiftKey,
      this.visibleRows(),
    ) as CoreEffect[];
    this.runEffects(effects);
  }

  /** The current popover as plain text (for Ctrl-y). Markdown flattens via the shared serializer;
   *  diagnostic/commit blocks join by blank lines. */
  private hoverPlainText(): string {
    const c = this.hoverContent;
    if (!c) return "";
    return c.kind === "markdown"
      ? mdToPlain(c.blocks)
      : c.blocks.map((b) => b.text).join("\n\n");
  }

  /**
   * Vertical scroll delta (px) for a resolved popover scroll action. A line is one cell height;
   * half/page use the popover's client height — mirroring the editor's scroll units. Native
   * `scrollBy` clamps to range.
   */
  private hoverScrollDelta(s: { down: boolean; unit: "line" | "half" | "page" }): number {
    const page = this.hoverEl.clientHeight;
    const mag = s.unit === "line" ? this.cell.h : s.unit === "half" ? page / 2 : page;
    return s.down ? mag : -mag;
  }

  private onNotification(method: string, params: unknown): void {
    if (!this.session) return;
    // Coalesce the redraw: a streaming grep emits a `picker/update` per batch (and the broad
    // intermediate queries flood them), so rendering synchronously per push falls badly behind —
    // each render re-serializes the whole wasm view + reconciles the DOM. Apply every push to core
    // state immediately, but paint at most once per frame (the native client coalesces the same way).
    this.runEffects(this.session.on_event(method, params) as CoreEffect[], true);
  }

  /** Connection-state changes from the transport. `client.ts` owns the socket reconnect (backoff);
   *  the shell suspends core input while down (`connection_lost` → ConnState::Reconnecting, so
   *  `on_key` no-ops and stray RPCs don't error) and shows a banner. `reestablish` (on socket-up)
   *  rebuilds the session. */
  private onConnState(s: ConnState): void {
    if (s === "connected") {
      this.connBanner.style.display = "none";
      return;
    }
    // A down state: suspend the core (only meaningful once a session exists) and show the banner.
    if (this.session) this.runEffects(this.session.connection_lost() as CoreEffect[]);
    this.connBanner.className = s === "failed" ? "failed" : "";
    this.connBanner.replaceChildren();
    const label = document.createElement("span");
    label.textContent = s === "failed" ? "Disconnected" : "Reconnecting…";
    this.connBanner.append(label);
    if (s === "failed") {
      const retry = document.createElement("button");
      retry.className = "conn-retry";
      retry.textContent = "Retry";
      retry.addEventListener("click", () => this.client.retry());
      this.connBanner.append(retry);
    }
    this.connBanner.style.display = "flex";
  }

  /** Rebuild the session after the socket reconnects (a fresh client_id ⇒ the server dropped this
   *  client's cursor/selection/viewport). Re-activate the project and reopen the current buffer (by
   *  id, restoring the cursor; a server *restart* invalidates the id, so fall back to the project's
   *  last/scratch). Buffer content + unsaved edits survive a socket drop server-side. */
  private async reestablish(): Promise<void> {
    const snap = this.snapshot;
    if (!snap) return;
    // Reconnected while still choosing a project (no project activated yet): just re-raise the
    // chooser on the fresh connection rather than activating an empty-named project.
    if (snap.buffer.buffer_id === 0) {
      this.session = new WasmSession();
      this.connBanner.style.display = "none";
      this.runEffects(this.session.open_projects() as CoreEffect[]);
      this.capture.focus();
      return;
    }
    try {
      const activated = await this.client.rpc<ProjectActivateResult>("project/activate", {
        name: snap.project,
        open_last: false,
      });
      let open: BufferOpenResult;
      try {
        open = await this.client.rpc<BufferOpenResult>("buffer/open", {
          buffer_id: snap.buffer.buffer_id,
          jump_to: snap.buffer.cursor.position,
        });
      } catch {
        const relanded = await this.client.rpc<ProjectActivateResult>("project/activate", {
          name: snap.project,
          open_last: true,
        });
        open =
          relanded.opened ??
          (await this.client.rpc<BufferOpenResult>("buffer/open", { transient: true }));
      }
      this.session = WasmSession.bootstrap(activated.project.name, activated.project.paths, open);
      this.connBanner.style.display = "none";
      await this.subscribe();
      this.capture.focus();
    } catch (e) {
      this.toast(`reconnect failed: ${String(e)}`, "error");
    }
  }

  /** Execute one batch of effects, then repaint from the fresh view. Async effects (Request, the
   *  geometry reveals) repaint again when they settle. `coalesce` defers the final paint to the next
   *  animation frame so a burst (streaming server pushes) collapses into one render. */
  private runEffects(effects: CoreEffect[], coalesce = false): void {
    for (const e of effects) {
      switch (e.tag) {
        case "Request":
          this.sendRequest(e.token!, e.method!, e.params);
          break;
        case "Toast":
          this.toast(e.message ?? "", e.level ?? "info");
          break;
        case "RevealCursor":
          void this.ensureCursorVisible();
          break;
        case "Resubscribe":
          this.dismissHover(); // a buffer switch resets view-side presentation
          void this.subscribe();
          break;
        case "ShowHover":
          if (e.hover) this.showHover(e.hover);
          break;
        case "DismissHover":
          this.dismissHover();
          break;
        case "WindowAdopted": {
          this.render();
          // Diff toggle re-layout: restore the view to the pending content anchor (same content on
          // screen) if there is one; otherwise reveal the cursor as before.
          const row = this.session.resolve_scroll_anchor();
          if (row != null) this.scrollTopTo(row * this.cell.h + BUFFER_PAD, false);
          else this.revealCursor();
          break;
        }
        case "WriteClipboard":
          if (e.text != null) void navigator.clipboard?.writeText(e.text).catch(() => {});
          break;
        case "ReadClipboard":
          this.handleReadClipboard(e.paste);
          break;
        case "ShellAction":
          if (e.action) this.runShellAction(e.action);
          break;
        case "SaveScrollAnchor":
          this.scrollAnchor = this.bufferEl.scrollTop;
          break;
        case "SaveContentAnchor": {
          // Capture the top-of-viewport content anchor before a wrap/diff re-layout.
          const topRow = Math.max(0, Math.round((this.bufferEl.scrollTop - BUFFER_PAD) / this.cell.h));
          this.session.capture_scroll_anchor(topRow, this.visibleRows());
          break;
        }
        case "RestoreScrollAnchor":
          if (this.scrollAnchor !== null) {
            this.scrollTopTo(this.scrollAnchor, false);
            this.scrollAnchor = null;
          }
          break;
        // Reveal the highlighted row on the next render (keyboard nav / refetch) — but not on a
        // free wheel-scroll, which emits no effect. A query change resets the scroll to the top.
        case "RevealPickerSelection":
          this.pickerReveal = true;
          break;
        case "PickerScrollReset":
          this.pickerScrollReset = true;
          break;
        // Deferred to later milestones: Reconnect, Exit.
        default:
          break;
      }
    }
    if (coalesce) this.scheduleRender();
    else this.render();
  }

  /** Paint at most once per animation frame. Used for streaming server pushes so a flood collapses
   *  into one render; any direct `render()` in the meantime cancels the pending frame. */
  private scheduleRender(): void {
    if (this.renderRaf !== null) return;
    this.renderRaf = requestAnimationFrame(() => {
      this.renderRaf = null;
      this.render();
    });
  }

  /** A core-issued (semantic) RPC: send it, feed the outcome back through `on_rpc_result`. */
  private sendRequest(token: number, method: string, params: unknown): void {
    this.client.rpc(method, params).then(
      (result) => this.runEffects(this.rpcResult(token, true, method, result)),
      (err: { code?: number; rpcMessage?: string; message?: string }) =>
        this.runEffects(
          this.rpcResult(token, false, method, {
            code: err?.code ?? 0,
            message: err?.rpcMessage ?? err?.message ?? "error",
          }),
        ),
    );
  }

  private rpcResult(token: number, ok: boolean, method: string, value: unknown): CoreEffect[] {
    return this.session.on_rpc_result(BigInt(token), ok, method, value) as CoreEffect[];
  }

  /** Handle `Effect::ReadClipboard`. A Ctrl-v gesture (before / at_cursor) rides the native `paste`
   *  event into the focused capture textarea — no permission prompt — so we just stash the descriptor
   *  for the paste handler. Ctrl-r (replace / line) has no native paste, so read directly (prompts in
   *  Firefox — acceptable per the user). */
  private handleReadClipboard(paste: unknown): void {
    const kind = (paste as { kind?: string } | null)?.kind;
    const nativePasteable =
      (kind === "before" || kind === "at_cursor") && document.activeElement === this.capture;
    if (nativePasteable) {
      this.pendingPaste = paste;
    } else {
      this.readClipboard(paste);
    }
  }

  /** Read the clipboard directly (`navigator.clipboard.readText`) — used for Ctrl-r replace, which
   *  has no native paste event. Prompts for permission in Firefox. */
  private readClipboard(paste: unknown): void {
    const deliver = (text: string | undefined) => {
      if (this.session) this.runEffects(this.session.clipboard_read(paste, text) as CoreEffect[]);
    };
    const cb = navigator.clipboard;
    if (!cb) {
      deliver(undefined);
      return;
    }
    cb.readText().then(deliver, () => deliver(undefined));
  }

  private runShellAction(a: ShellActionDesc): void {
    switch (a.name) {
      case "scroll":
        this.scrollView(a.dir ?? "down", a.unit ?? "line");
        break;
      case "center_cursor":
        void this.centerCursor();
        break;
      case "toggle_wrap":
        this.session.toggle_wrap(); // flip core wrap state (no effects); then re-render the viewport
        void this.setWrap();
        break;
      case "open_help":
        this.openHelp();
        break;
      // open_project_settings: a shell-local editor, a later milestone.
      default:
        break;
    }
  }

  /** Re-render the viewport at the just-toggled wrap mode. The core already flipped `Session.wrap`;
   *  this issues the geometry RPC (mirrors iced): zero the horizontal scroll, ask the server to
   *  re-render the existing viewport at the new wrap, adopt the window, then keep the cursor on-screen
   *  under the new layout. */
  private async setWrap(): Promise<void> {
    const v = this.view();
    if (!v.viewport_id) return;
    this.bufferEl.scrollLeft = 0; // a wrapped layout has no horizontal scroll
    const epoch = ++this.viewportEpoch;
    let res: ViewportWindowResult;
    try {
      res = await this.client.rpc<ViewportWindowResult>("viewport/set_wrap", {
        viewport_id: v.viewport_id,
        wrap: v.wrap,
      });
    } catch {
      return; // a failed set_wrap (e.g. raced a buffer close) — a newer geometry op will follow
    }
    if (epoch !== this.viewportEpoch) return; // superseded
    this.session.adopt_window(res);
    this.render();
    // Restore the view to the content anchor captured before the toggle (same content on screen
    // across the reflow); fall back to revealing the cursor when none is pending.
    const row = this.session.resolve_scroll_anchor();
    if (row != null) this.scrollTopTo(row * this.cell.h + BUFFER_PAD, false);
    else this.revealCursor();
  }

  // ---- geometry (shell-owned; viewport RPCs issued here, results adopted by the core) ----------

  private async subscribe(): Promise<void> {
    this.recomputeGrid();
    const v = this.view();
    if (v.buffer.buffer_id === 0) return; // placeholder session — no buffer to subscribe to yet
    // Position the new viewport at the buffer's restored scroll, else centre the cursor — which, for
    // a grep/goto jump, sits on the target. Derived FRESH from the current buffer every time (never a
    // cached value), so a jump always loads the window containing its target and the reveal lands.
    const cursorLine = v.buffer.cursor.position.line;
    const scroll = v.buffer.scroll ?? {
      logical_line: Math.max(0, cursorLine - Math.floor(this.rows / 2)),
      sub_row: 0,
    };
    const epoch = ++this.viewportEpoch;
    this.fetchInFlight = false;
    let res: ViewportSubscribeResult;
    try {
      res = await this.client.rpc<ViewportSubscribeResult>("viewport/subscribe", {
        buffer_id: v.buffer.buffer_id,
        cols: this.cols,
        rows: this.rows,
        overscan_rows: this.rows,
        scroll,
        wrap: v.wrap,
        continuation_marker_width: CONTINUATION_MARKER_WIDTH,
        tab_width: TAB_WIDTH,
      });
    } catch {
      return; // a failed subscribe (e.g. raced a buffer close) — a newer one will follow
    }
    if (epoch !== this.viewportEpoch) return; // superseded by a newer subscribe — drop this window
    this.session.adopt_subscribe(res);
    this.render();
    // A subscribe replaces the whole window (a buffer switch / wrap toggle), so it snaps — there's no
    // scroll to animate. Same-buffer *moves* (grep next-hit, cursor motions) animate via the
    // cursor-move path (RevealCursor → revealCursor → scrollTopTo), not here.
    const w = this.snapshot?.window;
    if (w) {
      const rel = rowsBeforeLine(w, scroll.logical_line);
      if (rel !== null) {
        this.bufferEl.scrollTop = (w.first_visual_row + rel) * this.cell.h + BUFFER_PAD;
      }
    }
    this.revealCursor();
  }

  /** After a cursor-moving action: load around the cursor if it left the loaded window, paint, then
   *  scroll the minimum to reveal it. */
  private async ensureCursorVisible(): Promise<void> {
    const v = this.view();
    if (!v.window) return;
    const cl = v.buffer.cursor.position.line;
    if (cl < v.window.first_logical_line || cl >= v.window.last_logical_line_exclusive) {
      const epoch = this.viewportEpoch;
      let res: ViewportWindowResult;
      try {
        res = await this.client.rpc<ViewportWindowResult>("viewport/scroll", {
          viewport_id: v.viewport_id,
          scroll: { logical_line: cl, sub_row: 0 },
        });
      } catch {
        return; // viewport gone (e.g. a resubscribe raced in) — that subscribe reveals afresh
      }
      if (epoch !== this.viewportEpoch) return; // a resubscribe superseded this fetch
      this.session.adopt_window(res);
    }
    this.render();
    this.revealCursor();
  }

  /** Native scroll event: fetch a new window when the view nears the loaded window's edge. */
  private onScroll(): void {
    // The popover tracks its line via CSS `position: sticky` (it lives in the buffer's spacer), so
    // scrolling needs no repositioning here — just the window prefetch below.
    const w = this.snapshot?.window;
    if (!w || this.fetchInFlight) return;
    const topRow = Math.round((this.bufferEl.scrollTop - BUFFER_PAD) / this.cell.h);
    const loadedStart = w.first_visual_row;
    const loadedEnd = loadedStart + this.loadedVisualRows(w);
    const margin = this.rows;
    const visible = this.visibleRows();
    const needAbove = loadedStart > 0 && topRow < loadedStart + margin;
    const needBelow = loadedEnd < w.total_visual_rows && topRow + visible > loadedEnd - margin;
    if (needAbove || needBelow) this.fetchByRow(topRow);
  }

  /** Fetch the window around an absolute visual row; content is absolutely placed so scrollTop is
   *  unchanged (no jump). */
  private fetchByRow(topRow: number): void {
    const epoch = this.viewportEpoch;
    this.fetchInFlight = true;
    this.client
      .rpc<ViewportWindowResult>("viewport/scroll_to_row", {
        viewport_id: this.snapshot?.viewport_id,
        top_visual_row: Math.max(0, topRow),
      })
      .then(
        (res) => {
          this.fetchInFlight = false;
          if (epoch !== this.viewportEpoch) return; // a resubscribe superseded this fetch
          this.session.adopt_window(res);
          this.render();
          this.onScroll(); // re-check in case the view moved further while fetching
        },
        () => {
          this.fetchInFlight = false;
        },
      );
  }

  private scrollView(dir: string, unit: string): void {
    const page = this.visibleRows();
    const delta = unit === "page" ? page : unit === "half" ? Math.max(1, Math.floor(page / 2)) : 1;
    if (dir === "up") this.scrollTopTo(this.bufferEl.scrollTop - delta * this.cell.h, true);
    else if (dir === "down") this.scrollTopTo(this.bufferEl.scrollTop + delta * this.cell.h, true);
    else if (this.snapshot?.wrap === "none") {
      const mag = unit === "half" ? Math.max(1, Math.floor(this.cols / 2)) : 1;
      this.bufferEl.scrollLeft += (dir === "left" ? -mag : mag) * this.cell.w;
    }
  }

  private async centerCursor(): Promise<void> {
    const v = this.view();
    if (!v.window) return;
    const cl = v.buffer.cursor.position.line;
    // When the cursor's line has been scrolled out of the loaded window its visual row is unknown —
    // pull that region from the server (scrolling the viewport to the line), then centre. Mirrors
    // `ensureCursorVisible`.
    if (cl < v.window.first_logical_line || cl >= v.window.last_logical_line_exclusive) {
      const epoch = this.viewportEpoch;
      let res: ViewportWindowResult;
      try {
        res = await this.client.rpc<ViewportWindowResult>("viewport/scroll", {
          viewport_id: v.viewport_id,
          scroll: { logical_line: cl, sub_row: 0 },
        });
      } catch {
        return; // viewport gone (e.g. a resubscribe raced in)
      }
      if (epoch !== this.viewportEpoch) return; // a resubscribe superseded this fetch
      this.session.adopt_window(res);
      this.render();
    }
    const row = this.cursorAbsoluteVisualRow();
    if (row === null) return;
    this.scrollTopTo((row - Math.floor(this.visibleRows() / 2)) * this.cell.h + BUFFER_PAD, true);
  }

  private revealCursor(): void {
    const cursorRow = this.cursorAbsoluteVisualRow();
    if (cursorRow === null) return;
    const topRow = (this.bufferEl.scrollTop - BUFFER_PAD) / this.cell.h;
    const visible = this.visibleRows();
    const margin = this.cell.h / 2;
    if (cursorRow < topRow) {
      this.scrollTopTo(cursorRow * this.cell.h - margin + BUFFER_PAD, true);
    } else if (cursorRow >= topRow + visible) {
      this.scrollTopTo((cursorRow - visible + 1) * this.cell.h + margin + BUFFER_PAD, true);
    }
    const v = this.snapshot;
    if (v && v.wrap === "none") {
      const gutterPx = this.cell.w;
      const cx = gutterPx + v.buffer.cursor.position.col * this.cell.w;
      if (cx - this.bufferEl.scrollLeft < gutterPx) this.bufferEl.scrollLeft = cx - gutterPx;
      else if (cx + this.cell.w - this.bufferEl.scrollLeft > this.bufferEl.clientWidth) {
        this.bufferEl.scrollLeft = cx + this.cell.w - this.bufferEl.clientWidth;
      }
    }
  }

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

  private onResize(): void {
    const before = `${this.cols}x${this.rows}`;
    this.recomputeGrid();
    const v = this.snapshot;
    if (!v?.viewport_id || `${this.cols}x${this.rows}` === before) return;
    this.client
      .rpc<ViewportWindowResult>("viewport/resize", {
        viewport_id: v.viewport_id,
        cols: this.cols,
        rows: this.rows,
      })
      .then(
        (res) => {
          this.session.adopt_window(res);
          this.render();
        },
        () => {},
      );
  }

  // ---- mouse: click-to-position + drag-select -------------------------------------------------

  private onBufferMouseDown(e: MouseEvent): void {
    this.dismissHover(); // a click in the buffer dismisses the popover
    // Left button only; ignore while an overlay owns the keyboard (picker/search/save-as).
    if (e.button !== 0 || this.overlayOwnsKeyboard()) return;
    const pos = this.mouseToPos(e);
    e.preventDefault(); // keep focus on the capture textarea + suppress native text selection
    this.capture.focus();
    if (!pos || !this.session) return;
    this.dragging = true;
    // The browser counts clicks for us (`detail`): double = word, triple = line.
    const granularity = e.detail <= 1 ? "char" : e.detail === 2 ? "word" : "line";
    this.runEffects(
      this.session.pointer_press(pos.line, pos.col, granularity, e.shiftKey) as CoreEffect[],
    );
  }

  private onMouseMove(e: MouseEvent): void {
    if (!this.dragging || !this.session) return;
    const pos = this.mouseToPos(e);
    if (pos) this.runEffects(this.session.pointer_drag(pos.line, pos.col) as CoreEffect[]);
  }

  private onMouseUp(): void {
    if (!this.dragging) return;
    this.dragging = false;
    this.session?.pointer_release();
  }

  /** Map a mouse event to a buffer `(line, col)`: find the `.row` under it (render.ts tags each with
   *  `data-line` + `data-byte`), then measure the click x against the row text → byte column. */
  private mouseToPos(e: MouseEvent): { line: number; col: number } | null {
    // Use the element under the pointer (not e.target): during a window-level drag, e.target may be
    // outside the buffer, but the coordinates still resolve to the row under the cursor.
    const rowEl = document.elementFromPoint(e.clientX, e.clientY)?.closest(".row") as HTMLElement | null;
    if (!rowEl || rowEl.dataset.line === undefined) return null;
    const line = Number(rowEl.dataset.line);
    const rowByte = Number(rowEl.dataset.byte);
    const textEl = rowEl.querySelector(".row-text") as HTMLElement | null;
    if (!textEl) return { line, col: rowByte };
    const rect = textEl.getBoundingClientRect();
    const charIdx = Math.max(0, Math.round((e.clientX - rect.left) / this.cell.w));
    const { byteStart, byteLen } = decodeRow(textEl.textContent ?? "");
    // A click past the last char maps to the line-end byte (so you can select to EOL).
    const within = charIdx >= byteStart.length ? byteLen : byteStart[charIdx];
    return { line, col: rowByte + within };
  }

  private recomputeGrid(): void {
    this.cols = Math.max(1, Math.floor(this.bufferEl.clientWidth / this.cell.w) - GUTTER_COLS);
    this.rows = Math.max(1, Math.floor(this.bufferEl.clientHeight / this.cell.h));
  }

  private visibleRows(): number {
    return Math.max(1, Math.floor(this.bufferEl.clientHeight / this.cell.h));
  }

  private loadedVisualRows(w: BufferWindow): number {
    let rows = 0;
    for (const l of w.lines) rows += (l.virtual_rows_above?.length ?? 0) + l.visual_rows.length;
    return rows;
  }

  /** Absolute visual-row index of the cursor in the document, or null if its line isn't loaded. */
  private cursorAbsoluteVisualRow(): number | null {
    const v = this.snapshot;
    if (!v?.window) return null;
    const cl = v.buffer.cursor.position.line;
    if (cl < v.window.first_logical_line || cl >= v.window.last_logical_line_exclusive) return null;
    let row = v.window.first_visual_row;
    for (const l of v.window.lines) {
      const above = l.virtual_rows_above?.length ?? 0;
      if (l.logical_line === cl) {
        let idx = 0;
        for (let i = 0; i < l.visual_rows.length; i++) {
          if (l.visual_rows[i].byte_offset <= v.buffer.cursor.position.col) idx = i;
        }
        return row + above + idx;
      }
      row += above + l.visual_rows.length;
    }
    return null;
  }

  // ---- render ---------------------------------------------------------------------------------

  private render(): void {
    // A direct paint supersedes any frame queued by scheduleRender — drop it so we don't double-render.
    if (this.renderRaf !== null) {
      cancelAnimationFrame(this.renderRaf);
      this.renderRaf = null;
    }
    const v = this.view();
    this.snapshot = v;
    this.renderSearch(v);
    this.renderPrompt(v);
    this.renderPicker(v);
    // No project yet (placeholder boot session): the mandatory chooser is the whole UI. Render only
    // a bare backdrop behind it — no buffer, no status bar — and don't sync a bogus `?buffer=0` URL.
    if (v.buffer.buffer_id === 0) {
      this.bufferEl.replaceChildren();
      this.statusEl.replaceChildren();
      return;
    }
    this.syncUrl(v); // keep the address bar in sync with the current buffer + cursor
    this.renderStatus(v);
    if (!v.window) return;
    this.bufferEl.classList.toggle("hscroll", v.wrap === "none");
    renderBuffer(this.bufferEl, {
      window: v.window,
      cursor: v.buffer.cursor,
      insertMode: v.mode === "insert",
      awaitingKey: v.pending !== null || (v.count ?? 0) > 0,
      contentWidthPx: v.wrap === "none" ? this.cell.w * (v.window.max_line_width + 2) : 0,
      spacerHeightPx: v.window.total_visual_rows * this.cell.h + BUFFER_PAD * 2,
      contentTopPx: v.window.first_visual_row * this.cell.h + BUFFER_PAD,
      blame: v.blame && v.mode === "normal" ? v.blame.text : null,
      diffView: v.diff_view,
    });
  }

  /** Whether a keydown is plain text-editing (the native <input> should handle it and sync via its
   *  `input` event) rather than a navigation/accept/cancel/chord key routed to the core. */
  private static isEditingKey(e: KeyboardEvent): boolean {
    return (
      !e.ctrlKey &&
      !e.altKey &&
      !e.metaKey &&
      (e.key.length === 1 ||
        e.key === "Backspace" ||
        e.key === "Delete" ||
        e.key === "ArrowLeft" ||
        e.key === "ArrowRight" ||
        e.key === "Home" ||
        e.key === "End")
    );
  }

  /** Route an overlay <input>'s keydown: editing keys (and the native clipboard / select-all combos)
   *  stay native; everything else goes to the core (which dispatches to on_search_key /
   *  on_picker_key / on_prompt_key by state). */
  private routeOverlayKey(e: KeyboardEvent): void {
    const clip =
      (e.ctrlKey || e.metaKey) && !e.altKey && ["c", "v", "x", "a"].includes(e.key.toLowerCase());
    if (clip || Shell.isEditingKey(e)) return;
    e.preventDefault();
    if (this.session) {
      this.runEffects(
        this.session.on_key(e.key, e.ctrlKey, e.altKey, e.shiftKey, this.visibleRows()) as CoreEffect[],
      );
    }
  }

  private onSearchInputKey(e: KeyboardEvent): void {
    this.routeOverlayKey(e);
  }

  private onSaveAsInputKey(e: KeyboardEvent): void {
    this.routeOverlayKey(e);
  }

  private renderSearch(v: CoreView): void {
    const wasOpen = this.searchOpen;
    this.searchOpen = v.mode === "search";
    if (v.mode !== "search") {
      if (wasOpen) {
        this.searchBar.style.display = "none";
        this.capture.focus();
      }
      return;
    }
    const s = v.search;
    this.searchBar.style.display = "flex";
    this.searchPrefixEl.textContent = s.extend_to_cursor ? "?" : "/";
    if (this.searchInput.value !== s.query) this.searchInput.value = s.query;
    this.searchCountEl.textContent = s.summary
      ? `${s.summary.current_index}/${s.summary.total}${s.summary.truncated ? "+" : ""}`
      : "";
    if (document.activeElement !== this.searchInput) this.searchInput.focus();
  }

  private renderPrompt(v: CoreView): void {
    const p = v.prompt;
    const wasSaveAs = this.saveAsOpen;
    this.saveAsOpen = p?.kind === "saveas";

    // Save-as: the persistent native-input overlay (confirm/lsp-info are rebuilt in overlayEl).
    if (p?.kind === "saveas") {
      this.overlayEl.style.display = "none";
      this.overlayEl.replaceChildren();
      this.saveAsEl.style.display = "";
      const root = v.project_paths[p.path_index] ?? "";
      this.saveAsPrefixEl.textContent = root ? `${root}/` : "";
      if (this.saveAsInput.value !== p.input) this.saveAsInput.value = p.input;
      if (document.activeElement !== this.saveAsInput) this.saveAsInput.focus();
      return;
    }
    if (wasSaveAs) {
      this.saveAsEl.style.display = "none";
      this.capture.focus();
    }
    if (!p) {
      this.overlayEl.style.display = "none";
      this.overlayEl.replaceChildren();
      return;
    }
    this.overlayEl.style.display = "";
    this.overlayEl.className = "overlay" + (p.kind === "confirm" ? " confirm-overlay" : "");
    const modal = document.createElement("div");
    modal.className = "modal" + (p.kind === "lspinfo" ? " lsp-info" : "");
    if (p.kind === "confirm") {
      const msg = document.createElement("div");
      msg.className = "modal-message";
      msg.textContent = p.message;
      const buttons = document.createElement("div");
      buttons.className = "modal-buttons";
      const cancel = document.createElement("span");
      cancel.className = "modal-btn";
      cancel.textContent = "Esc";
      const ok = document.createElement("span");
      ok.className = "modal-btn primary";
      ok.textContent = "Enter";
      buttons.append(cancel, ok);
      modal.append(msg, buttons);
    } else {
      // lsp-info: full server detail (the status is already projected in the view). `r` restarts and
      // any other key / Esc closes — both routed through the core's on_prompt_key, so this only paints.
      const st = p.status;
      const busy = st.status.state === "ready" && (st.progress?.length ?? 0) > 0;
      const header = document.createElement("div");
      header.className = "modal-message";
      const dot = document.createElement("span");
      dot.className = `lsp-info-dot ${busy ? "lsp-busy" : lspStateClass(st.status.state)}`;
      dot.textContent = "●";
      header.append(dot, document.createTextNode(st.name));
      const rows = document.createElement("div");
      rows.className = "lsp-info-rows";
      const kv = (k: string, v: string) => {
        const key = document.createElement("span");
        key.className = "lsp-info-key";
        key.textContent = k;
        const val = document.createElement("span");
        val.className = "lsp-info-val";
        val.textContent = v;
        rows.append(key, val);
      };
      let statusLabel: string;
      if (st.status.state === "crashed") {
        statusLabel =
          st.status.code != null
            ? `crashed (${st.status.code}): ${st.status.message}`
            : `crashed: ${st.status.message}`;
      } else {
        statusLabel = busy ? "busy" : st.status.state;
      }
      kv("Language", st.language);
      kv("Workspace", st.workspace_root);
      kv("Status", statusLabel);
      for (const pr of st.progress ?? []) {
        let line = pr.title;
        if (pr.message) line += ` — ${pr.message}`;
        if (pr.percentage != null) line += ` (${pr.percentage}%)`;
        kv("Working", line);
      }
      const hint = document.createElement("div");
      hint.className = "lsp-info-hint";
      hint.textContent = "r — restart · Esc — close";
      modal.append(header, rows, hint);
    }
    this.overlayEl.replaceChildren(modal);
  }

  /** The picker query <input>'s keydown: text-editing stays native (synced via `input` →
   *  picker_set_query); nav/accept/cancel/chord keys route to the core. (When the chip editor is open
   *  its own inputs hold focus and handle keys via onEditorKey, so this only runs for the query.)
   *  Filter chips are selected with the keyboard like the native client: Left/Backspace at the query
   *  start steps into the chip row, then Left/Right navigate, Enter edits, Backspace/Delete removes. */
  private onPickerInputKey(e: KeyboardEvent): void {
    const p = this.snapshot?.picker;
    // No project selected yet: the chooser is mandatory. Unlike the native clients (which exit on
    // dismiss), a browser tab has nothing to fall back to, so Esc must not close it.
    if (e.key === "Escape" && this.snapshot?.buffer.buffer_id === 0) {
      e.preventDefault();
      return;
    }
    // Ctrl/Cmd-Enter opens the selected item in a new browser tab (keyboard parity with Ctrl-click).
    if (p && this.snapshot && (e.ctrlKey || e.metaKey) && !e.altKey && e.key === "Enter") {
      const sel = p.items[p.selected - p.offset];
      const href = sel ? this.pickerItemUrl(sel, this.snapshot) : null;
      if (href) {
        e.preventDefault();
        window.open(href, "_blank", "noopener");
        return;
      }
    }
    if (p && !p.chip_editor && this.session) {
      // A chip is selected: the core owns the editing keys; the native query input is inert until
      // a Right past the last chip, Esc, or a typed char deselects it.
      if (p.chip_selected !== null) {
        e.preventDefault();
        this.runEffects(
          this.session.on_key(e.key, e.ctrlKey, e.altKey, e.shiftKey, this.visibleRows()) as CoreEffect[],
        );
        return;
      }
      // At the very start of the query (the native caret owns the position), Left / Backspace step
      // into the chip row, selecting the rightmost chip.
      const atStart = this.pickerInput.selectionStart === 0 && this.pickerInput.selectionEnd === 0;
      if (atStart && p.chips.length > 0 && !e.ctrlKey && !e.altKey && !e.metaKey &&
          (e.key === "ArrowLeft" || e.key === "Backspace")) {
        e.preventDefault();
        this.runEffects(this.session.picker_select_last_chip() as CoreEffect[]);
        return;
      }
    }
    this.routeOverlayKey(e);
  }

  private renderPicker(v: CoreView): void {
    const p = v.picker;
    const wasOpen = this.pickerOpen;
    this.pickerOpen = p !== null;
    if (!p) {
      if (wasOpen) {
        this.pickerEl.style.display = "none";
        this.capture.focus(); // return the keyboard to the buffer
      }
      return;
    }
    this.pickerEl.style.display = "";
    // Explorer shows the directory being listed *within its project root* as a dim prefix.
    const prefix = p.kind === "explorer" ? explorerPrefix(p.directory, v.project_paths) : "";
    this.pickerPathEl.textContent = prefix;
    this.pickerPathEl.style.display = prefix ? "" : "none";
    // The breadcrumb already says where typing acts; otherwise show the per-kind hint.
    this.pickerInput.placeholder = prefix ? "" : PLACEHOLDER[p.kind];
    // The input is the source of truth for the text while focused; only write when the core changed
    // it out from under us (grep priming, a seeded open) to avoid clobbering the caret mid-type.
    if (this.pickerInput.value !== p.query) this.pickerInput.value = p.query;
    // A CSS-animated throbber to the left of the count while a search streams (`ticking`); the
    // count itself shows progress. CSS drives the rotation, so it stays smooth regardless of the
    // push cadence.
    this.pickerSpinnerEl.style.display = p.ticking ? "" : "none";
    // A filtered file/buffer list shows `matched/total`; an unfiltered list — and grep, where every
    // candidate is a hit — collapses to a single total (rather than the redundant "M/M").
    this.pickerCountEl.textContent =
      p.total_matches === 0
        ? ""
        : p.total_matches === p.total_candidates
          ? `${p.total_matches}`
          : `${p.total_matches}/${p.total_candidates}`;
    this.renderPickerChips(p);
    this.renderChipEditor(p.chip_editor);
    this.renderPickerList(p, v);
    // Keep the query input focused whenever the picker is open (idempotent — a no-op when already
    // focused, so it never disturbs the caret), so it's ready to type the moment it opens. While the
    // chip editor is open one of its own inputs holds focus (syncEditorState), so stand down.
    if (!p.chip_editor && document.activeElement !== this.pickerInput) this.pickerInput.focus();
  }

  /** The active filter chips (display only; toggled/edited via the keyboard → the core). Exclusion
   *  rides the label's leading `!`; the word-boundary chip is underlined (`flag`). */
  private renderPickerChips(p: PickerView): void {
    const nodes = p.chips.map((c, i) => {
      const el = document.createElement("span");
      let cls = "picker-chip";
      if (c.label.startsWith("!")) cls += " exclude";
      if (i === p.chip_selected) cls += " selected";
      if (c.flag) cls += " flag";
      el.className = cls;
      el.textContent = c.label;
      return el;
    });
    this.pickerChipsEl.replaceChildren(...nodes);
  }

  /** A ghost-overlay wrap for a focused editor segment: the native `input` stacked over a ghost layer
   *  (an invisible metric-accurate copy of the typed text + the dim suggestion suffix). `hug` sizes
   *  the wrap to its content (the root segment, so the `:` sits flush); otherwise it stretches across
   *  the row (the path segment). Records the ghost element so per-keystroke updates can refill it. */
  private editorWrap(input: HTMLInputElement, hug: boolean): HTMLElement {
    const wrap = document.createElement("span");
    wrap.className = hug ? "picker-editor-rootwrap hug" : "picker-editor-rootwrap";
    const ghost = document.createElement("span");
    ghost.className = "picker-editor-ghost";
    input.classList.add("picker-editor-root");
    wrap.append(ghost, input);
    if (hug) this.editorRootGhost = ghost;
    else this.editorPathGhost = ghost;
    return wrap;
  }

  /** Fill a ghost layer: the typed text (invisible via CSS, but reserving exact glyph metrics) then
   *  the dim suggestion suffix the core computed, so it sits flush after the caret. */
  private fillGhost(ghost: HTMLElement | null, typed: string, suffix: string | null): void {
    if (!ghost) return;
    const t = document.createElement("span");
    t.className = "typed";
    t.textContent = typed;
    const s = document.createElement("span");
    s.textContent = suffix ?? "";
    ghost.replaceChildren(t, s);
  }

  /** An unfocused dir-editor segment: plain text, clicking it focuses that segment (via the core,
   *  which enforces the can't-enter-path-under-invalid-root gate). */
  private editorSeg(text: string, cls: string, root: boolean): HTMLElement {
    const span = document.createElement("span");
    span.className = cls;
    span.textContent = text;
    span.addEventListener("mousedown", (e) => {
      e.preventDefault();
      if (this.session) this.runEffects(this.session.chip_editor_set_field(root) as CoreEffect[]);
    });
    return span;
  }

  /** The glob/dir filter editor row (mirrors the old client + iced's `editor_line`): `glob:`/`dir:`
   *  tag, then for a multi-root dir a root typeahead segment + `:` separator, then the path. Only the
   *  *focused* segment is a native `<input>` (with a ghost-suggestion overlay); the other is a
   *  clickable span. The row's structure is rebuilt only when it changes (open / kind / field switch)
   *  — never per keystroke, which would drop a live input's caret — while ghosts, validity and text
   *  sync every render. */
  private renderChipEditor(ce: ChipEditorView | null): void {
    if (!ce) {
      if (this.editorStructKey !== null) {
        this.pickerEditorRow.style.display = "none";
        this.pickerEditorRow.replaceChildren();
        this.editorStructKey = null;
      }
      return;
    }
    const structKey = `${ce.is_dir}|${ce.multi_root}|${ce.field}`;
    if (structKey !== this.editorStructKey) {
      this.rebuildEditorRow(ce);
      this.editorStructKey = structKey;
    }
    this.syncEditorState(ce);
  }

  /** (Re)build the editor row for the current focused field. Mounts the persistent inputs into their
   *  ghost wraps; clicking moves focus to the newly focused segment's input. */
  private rebuildEditorRow(ce: ChipEditorView): void {
    this.pickerEditorRow.style.display = "flex";
    this.editorPathGhost = null;
    this.editorRootGhost = null;
    this.editorRootSpan = null;
    this.editorPathSpan = null;
    this.editorSepEl = null;
    const tag = document.createElement("span");
    tag.className = "picker-editor-label";
    tag.textContent = ce.is_dir ? "dir:" : "glob:";
    this.pickerEditorRow.replaceChildren(tag);
    if (!ce.is_dir) {
      // Glob: a single plain input (no ghost), with the syntax hint as its placeholder.
      this.editorPathInput.classList.remove("picker-editor-root");
      this.editorPathInput.placeholder = "*.rs · !*_test.rs · src/**";
      this.pickerEditorRow.append(this.editorPathInput);
      return;
    }
    this.editorPathInput.placeholder = "";
    if (ce.multi_root) {
      if (ce.field === "root") {
        this.pickerEditorRow.append(this.editorWrap(this.editorRootInput, true));
      } else {
        this.editorRootSpan = this.editorSeg(
          ce.root_invalid ? ce.root_filter.text : ce.root_display,
          ce.root_invalid ? "picker-editor-seg invalid" : "picker-editor-seg root",
          true,
        );
        this.pickerEditorRow.append(this.editorRootSpan);
      }
      this.editorSepEl = document.createElement("span");
      this.editorSepEl.className = "picker-editor-sep";
      this.editorSepEl.textContent = ":";
      this.pickerEditorRow.append(this.editorSepEl);
    }
    if (ce.field === "path" || !ce.multi_root) {
      this.pickerEditorRow.append(this.editorWrap(this.editorPathInput, false));
    } else {
      this.editorPathSpan = this.editorSeg(ce.input.text, "picker-editor-seg", false);
      this.pickerEditorRow.append(this.editorPathSpan);
    }
  }

  /** Per-render sync of the editor row's mutable bits: each native input's value (only when the core
   *  changed it out from under the caret — Tab-accept, segment-pop, root-commit), the ghost layers,
   *  the invalid colouring, the separator visibility, and focus on the active input. */
  private syncEditorState(ce: ChipEditorView): void {
    const sepVisible = ce.field === "path" || ce.input.text.length > 0;
    if (this.editorSepEl) this.editorSepEl.style.display = sepVisible ? "" : "none";
    if (!ce.is_dir) {
      if (this.editorPathInput.value !== ce.input.text) this.setInputValue(this.editorPathInput, ce.input.text);
      this.editorPathInput.classList.toggle("invalid", ce.path_invalid);
    } else {
      if (ce.field === "root") {
        if (this.editorRootInput.value !== ce.root_filter.text) this.setInputValue(this.editorRootInput, ce.root_filter.text);
        this.editorRootInput.classList.toggle("invalid", ce.root_invalid);
        this.fillGhost(this.editorRootGhost, ce.root_filter.text, ce.root_ghost);
      } else if (this.editorRootSpan && ce.multi_root) {
        this.editorRootSpan.textContent = ce.root_invalid ? ce.root_filter.text : ce.root_display;
        this.editorRootSpan.className = ce.root_invalid ? "picker-editor-seg invalid" : "picker-editor-seg root";
      }
      if (ce.field === "path" || !ce.multi_root) {
        if (this.editorPathInput.value !== ce.input.text) this.setInputValue(this.editorPathInput, ce.input.text);
        this.editorPathInput.classList.toggle("invalid", ce.path_invalid);
        this.fillGhost(this.editorPathGhost, ce.input.text, ce.path_ghost);
      } else if (this.editorPathSpan) {
        this.editorPathSpan.textContent = ce.input.text;
        this.editorPathSpan.classList.toggle("invalid", ce.path_invalid);
      }
    }
    // Keep the focused segment's input focused (idempotent when already so).
    const active = ce.is_dir && ce.multi_root && ce.field === "root" ? this.editorRootInput : this.editorPathInput;
    if (document.activeElement !== active) {
      active.focus();
      active.setSelectionRange(active.value.length, active.value.length);
    }
  }

  /** Write a value the core changed (not the user) into a native input, parking the caret at the end
   *  — a programmatic write doesn't fire `input`, so it won't loop back through the core. */
  private setInputValue(input: HTMLInputElement, value: string): void {
    input.value = value;
    if (document.activeElement === input) input.setSelectionRange(value.length, value.length);
  }

  /** A chip-editor input's keydown. Text editing (chars, plain Backspace/Delete, arrows) stays native
   *  and syncs through the input's `input` event; the keys the core owns — commit/cancel, field nav,
   *  ghost-accept, segment ops — are forwarded to `on_chip_editor_key`. */
  private onEditorKey(e: KeyboardEvent): void {
    const ce = this.snapshot?.picker?.chip_editor;
    if (!ce || !this.session) return;
    const k = e.key;
    if (k === "Shift" || k === "Control" || k === "Alt" || k === "Meta") {
      e.preventDefault(); // swallow lone modifiers so they don't reach the window handler
      return;
    }
    const inRoot = ce.is_dir && ce.multi_root && ce.field === "root";
    const emptyPath = ce.is_dir && ce.multi_root && ce.field === "path" && ce.input.text.length === 0;
    const coreKey =
      k === "Enter" ||
      k === "Escape" ||
      k === "Tab" ||
      (e.altKey && (k === "l" || k === "h" || k === "j" || k === "k" || k === "Backspace")) ||
      (k === ":" && inRoot && !e.altKey && !e.ctrlKey) ||
      (k === "Backspace" && !e.altKey && !e.ctrlKey && emptyPath);
    if (!coreKey) return; // native editing; the `input` event syncs the new text to the core
    e.preventDefault();
    this.runEffects(this.session.on_key(k, e.ctrlKey, e.altKey, e.shiftKey, this.visibleRows()) as CoreEffect[]);
  }

  /** Rebuild just the results list (the persistent input/panel stay, keeping focus + caret). */
  /** Map an absolute path to (root index, root-relative path), or null if it's outside every root —
   *  bootstrap only opens files relative to a root. */
  private resolvePath(abs: string, projectPaths: string[]): { path_index: number; relative_path: string } | null {
    for (let i = 0; i < projectPaths.length; i++) {
      const root = projectPaths[i];
      if (abs === root) return { path_index: i, relative_path: "" };
      const prefix = root.endsWith("/") ? root : root + "/";
      if (abs.startsWith(prefix)) return { path_index: i, relative_path: abs.slice(prefix.length) };
    }
    return null;
  }

  /** Parse a `#L:C` (or `#aL:aC-cL:cC`) location fragment into the cursor position (1-based on the
   *  wire, like the status bar), or null if absent/malformed. Used to jump a deep-linked open. */
  private parseFragment(hash: string): LogicalPosition | null {
    const body = hash.replace(/^#/, "");
    if (!body) return null;
    const seg = body.includes("-") ? body.split("-")[1] : body; // selection → the cursor end
    const [l, c] = seg.split(":").map(Number);
    return Number.isInteger(l) && Number.isInteger(c) && l >= 1 && c >= 1
      ? { line: l - 1, col: c - 1 }
      : null;
  }

  /** A shareable opener URL for a picker item, so its row can be an `<a>` that opens in a new tab on
   *  Ctrl/Cmd/middle-click (and Ctrl/Cmd-Enter on the selection). Mirrors the old client + the boot
   *  URL scheme: `?project=&root=&file=` for files (+ `#L:C` for grep hits), `?project=&buffer=<id>`
   *  for scratch buffers, `?project=` for a project. Returns null for rows with no shareable target
   *  (directories, diagnostics, references, LSP servers, items outside any root). */
  private pickerItemUrl(item: PickerItem, v: CoreView): string | null {
    const project = v.project;
    const fileQuery = (pathIndex: number, relativePath: string): string => {
      const params = new URLSearchParams();
      if (project) params.set("project", project);
      if (pathIndex) params.set("root", String(pathIndex));
      params.set("file", relativePath);
      return params.toString();
    };
    const fromPath = (pathIndex: number, relativePath: string, frag = ""): string =>
      `${location.pathname}?${fileQuery(pathIndex, relativePath)}${frag}`;
    switch (item.kind) {
      case "file":
        return fromPath(item.path_index, item.relative_path);
      case "grep_hit":
        return fromPath(item.path_index, item.relative_path, `#${item.line + 1}:${item.col + 1}`);
      case "buffer": {
        if (item.path_index != null && item.relative_path != null) {
          return fromPath(item.path_index, item.relative_path);
        }
        const params = new URLSearchParams();
        if (project) params.set("project", project);
        params.set("buffer", String(item.buffer_id));
        return `${location.pathname}?${params.toString()}`;
      }
      case "dir_entry": {
        const dir = v.picker?.directory;
        if (item.is_dir || !dir) return null;
        const joined = dir.endsWith("/") ? dir + item.name : `${dir}/${item.name}`;
        const r = this.resolvePath(joined, v.project_paths);
        return r ? fromPath(r.path_index, r.relative_path) : null;
      }
      case "project":
        return `${location.pathname}?${new URLSearchParams({ project: item.name }).toString()}`;
      default:
        return null;
    }
  }

  /** Keep the address bar reflecting the current buffer + cursor, the way the boot URL reader consumes
   *  it (`?project=&root=&file=#L:C`, or `?project=&buffer=<id>` for a scratch), so a reload or a copied
   *  link reopens where you are. `replaceState`, not `push` — browser back/forward isn't a second nav
   *  system; in-file/cross-file nav is the core's job (Alt-←/→). Debounced so a burst of cursor moves is
   *  one URL write; skipped when unchanged. */
  private syncUrl(v: CoreView): void {
    const url = this.buildUrl(v);
    if (url === this.lastUrl) return;
    this.lastUrl = url;
    window.clearTimeout(this.urlTimer);
    this.urlTimer = window.setTimeout(() => history.replaceState(null, "", url), 150);
  }

  private buildUrl(v: CoreView): string {
    const params = new URLSearchParams();
    if (v.project) params.set("project", v.project);
    const path = v.buffer.path;
    const r = path ? this.resolvePath(path, v.project_paths) : null;
    if (r) {
      if (r.path_index) params.set("root", String(r.path_index));
      params.set("file", r.relative_path);
    } else if (path) {
      params.set("file", path); // a file outside every root — fall back to the absolute path
    } else {
      params.set("buffer", String(v.buffer.buffer_id)); // scratch buffer: key on the session id
    }
    const qs = params.toString();
    return `${location.pathname}${qs ? `?${qs}` : ""}${this.cursorFragment(v.buffer.cursor)}`;
  }

  /** `#line:col` for a point, `#aLine:aCol-cLine:cCol` (anchor first) for a selection. 1-based, like
   *  the status bar and `parseFragment`. */
  private cursorFragment(c: CursorState): string {
    const enc = (q: LogicalPosition) => `${q.line + 1}:${q.col + 1}`;
    const p = c.position;
    const a = c.anchor;
    return p.line === a.line && p.col === a.col ? `#${enc(p)}` : `#${enc(a)}-${enc(p)}`;
  }

  // ---- hover popover --------------------------------------------------------------------------

  /** Show the hover popover with content the core produced (Effect::ShowHover): rendered markdown
   *  (LSP hover) or stacked severity-coloured blocks (diagnostics-at-cursor, commit details). */
  private showHover(content: HoverContent): void {
    this.hoverContent = content;
    this.hoverEl.classList.toggle("markdown", content.kind === "markdown");
    if (content.kind === "markdown") {
      this.hoverEl.replaceChildren(renderHoverDoc(content.blocks));
    } else {
      const blocks = content.blocks.map((b) => {
        const el = document.createElement("div");
        el.className = b.severity ? `hover-block ${hoverSevClass(b.severity)}` : "hover-block";
        // Diagnostic blocks lead with the severity icon (the core sends "Error"/"Warning"/"Info"/
        // "Hint"; lowercased these are the IconKinds); commit/plain blocks have no severity.
        if (b.severity) {
          const kind = b.severity.toLowerCase() as IconKind;
          el.append(statusIcon(kind), " ", b.text);
        } else {
          el.textContent = b.text;
        }
        return el;
      });
      this.hoverEl.replaceChildren(...blocks);
    }
    this.placeHover();
  }

  /** Reveal the popover (content already set) and anchor it at the cursor cell: below the line when it
   *  fits, flipped above otherwise; clamped into the viewport so it never spills off-screen. The body
   *  scrolls within its max-height (theme.css #hover). Mirrors the old web client + iced. */
  private placeHover(): void {
    const spacer = this.bufferEl.querySelector(".buffer-spacer") as HTMLElement | null;
    if (!spacer) return;
    const el = this.hoverEl;
    el.scrollTop = 0;
    this.hoverOpen = true;
    // Park the popover (preceded by its offset strut) in the spacer's coordinate space; CSS
    // `position: sticky` then keeps it glued to its line and clamped to the editor edges as the
    // buffer scrolls — no JS on scroll.
    if (this.hoverStrut.parentElement !== spacer) spacer.appendChild(this.hoverStrut);
    if (el.parentElement !== spacer) spacer.appendChild(el);
    this.positionHover();
  }

  /** Set the popover's flow offset within the spacer (via the strut height) so it rests at the
   *  anchor line — below it when there's room, else above. Done once when shown; the browser's
   *  sticky positioning takes over for all scrolling (tracking the line, then clamping to the
   *  editor's top/bottom). Anchor coordinates are read in the spacer's space, so they're stable
   *  across scroll/re-render. The strut (not a `margin-top`) is what makes the bottom-edge clamp
   *  work — a large top margin can't be shifted up by sticky, a real element can. */
  private positionHover(): void {
    const el = this.hoverEl;
    const spacer = el.parentElement;
    if (!spacer) return;
    const sr = spacer.getBoundingClientRect();
    const cur = this.bufferEl.querySelector(".cursor") as HTMLElement | null;
    const margin = 4;
    let lineTop: number, lineH: number, lineLeft: number;
    if (cur) {
      const cr = cur.getBoundingClientRect();
      lineTop = cr.top - sr.top; // anchor line top in spacer (content) coords
      lineH = cr.height;
      lineLeft = cr.left - sr.left;
    } else {
      lineTop = this.bufferEl.scrollTop + margin;
      lineH = this.cell.h;
      lineLeft = this.cell.w;
    }
    const h = el.offsetHeight;
    const w = el.offsetWidth;
    // Orientation: the line is on-screen when shown, so decide by the room above/below it now.
    const lineScreenTop = lineTop - this.bufferEl.scrollTop;
    const viewH = this.bufferEl.clientHeight;
    const fitsBelow = lineScreenTop + lineH + margin + h <= viewH - margin;
    const fitsAbove = lineScreenTop - margin - h >= margin;
    const top = fitsBelow || !fitsAbove ? lineTop + lineH + margin : lineTop - h - margin;
    const left = Math.max(margin, Math.min(lineLeft, spacer.offsetWidth - w - margin));
    this.hoverStrut.style.height = `${Math.max(0, Math.round(top))}px`;
    el.style.marginTop = "0";
    el.style.marginLeft = `${Math.round(left)}px`;
  }

  private dismissHover(): void {
    if (!this.hoverOpen) return;
    this.hoverOpen = false;
    this.hoverContent = undefined;
    this.hoverEl.remove(); // detach popover + its offset strut from the buffer spacer
    this.hoverStrut.remove();
    this.hoverEl.replaceChildren();
  }

  // ---- help overlay (Space ?) -----------------------------------------------------------------

  /** Show the keyboard-shortcut help overlay (Effect::ShellAction OpenHelp). Lazily sources the table
   *  from the core's keymap (help_entries) and builds the tab bar once, then reveals it. */
  private openHelp(): void {
    if (!this.helpData) {
      const entries = this.session.help_entries() as { tab: string; group: string; keys: string; desc: string }[];
      const order = ["Normal", "Insert", "Search", "Application"];
      this.helpData = order.map((label) => {
        const sections: { title: string; rows: [string, string][] }[] = [];
        for (const e of entries.filter((x) => x.tab === label)) {
          let sec = sections.find((s) => s.title === e.group);
          if (!sec) {
            sec = { title: e.group, rows: [] };
            sections.push(sec);
          }
          sec.rows.push([e.keys, e.desc]);
        }
        return { label, sections };
      });
      this.helpTabEls = this.helpData.map((tab, i) => {
        const t = document.createElement("button");
        t.className = "help-tab";
        t.textContent = tab.label;
        t.addEventListener("mousedown", (e) => {
          e.preventDefault();
          this.selectHelpTab(i);
        });
        return t;
      });
      this.helpTabsEl.replaceChildren(...this.helpTabEls);
    }
    this.helpOpen = true;
    this.helpEl.style.display = "";
    this.selectHelpTab(this.helpTab);
  }

  /** Switch the active help tab (← / → / Tab / 1-4 / click) and re-render its sections. */
  private selectHelpTab(i: number): void {
    if (!this.helpData) return;
    this.helpTab = (i + this.helpData.length) % this.helpData.length;
    this.helpTabEls.forEach((t, j) => t.classList.toggle("active", j === this.helpTab));
    const sections = this.helpData[this.helpTab].sections.map((section) => {
      const sec = document.createElement("div");
      sec.className = "help-section";
      const h = document.createElement("div");
      h.className = "help-section-title";
      h.textContent = section.title;
      sec.append(h);
      for (const [keys, desc] of section.rows) {
        const row = document.createElement("div");
        row.className = "help-row";
        const k = document.createElement("span");
        k.className = "help-key";
        k.textContent = keys;
        const d = document.createElement("span");
        d.className = "help-desc";
        d.textContent = desc;
        row.append(k, d);
        sec.append(row);
      }
      return sec;
    });
    this.helpGridEl.replaceChildren(...sections);
    this.helpGridEl.scrollTop = 0;
  }

  private closeHelp(): void {
    if (!this.helpOpen) return;
    this.helpOpen = false;
    this.helpEl.style.display = "none";
    this.ensureFocus();
  }

  /** The help overlay owns the keyboard while open: tab switching, scrolling, and close. Returns true
   *  when it consumed the key (so the window handler stops). */
  private handleHelpKey(e: KeyboardEvent): boolean {
    const k = e.key;
    if (k === "Escape" || k === "?" || k === "q") {
      this.closeHelp();
    } else if (k === "ArrowRight" || (k === "Tab" && !e.shiftKey) || k === "l") {
      this.selectHelpTab(this.helpTab + 1);
    } else if (k === "ArrowLeft" || (k === "Tab" && e.shiftKey) || k === "h") {
      this.selectHelpTab(this.helpTab - 1);
    } else if (k >= "1" && k <= String(this.helpData?.length ?? 4)) {
      this.selectHelpTab(Number(k) - 1);
    } else if (k === "ArrowDown" || k === "j" || k === " ") {
      this.helpGridEl.scrollBy({ top: k === " " ? this.helpGridEl.clientHeight - 40 : 40 });
    } else if (k === "ArrowUp" || k === "k") {
      this.helpGridEl.scrollBy({ top: -40 });
    } else if (k === "PageDown") {
      this.helpGridEl.scrollBy({ top: this.helpGridEl.clientHeight - 40 });
    } else if (k === "PageUp") {
      this.helpGridEl.scrollBy({ top: -(this.helpGridEl.clientHeight - 40) });
    } else if (k === "g" || k === "Home") {
      this.helpGridEl.scrollTop = 0;
    } else if (k === "G" || k === "End") {
      this.helpGridEl.scrollTop = this.helpGridEl.scrollHeight;
    } else if (k === "Shift" || k === "Control" || k === "Alt" || k === "Meta") {
      return true; // swallow lone modifiers; wait for the real key
    }
    return true; // the help overlay consumes every key while open
  }

  /** The Explorer's synthetic "+ Create …" row — italic, like the TUI/iced. Selecting it (click or
   *  Enter on the highlight) routes through `picker_click(abs)` → the core's create action. */
  private makePickerCreateRow(p: PickerView): HTMLElement {
    const c = p.create!;
    const row = document.createElement("div");
    row.className = p.selected === c.abs ? "picker-row selected" : "picker-row";
    row.addEventListener("mousedown", (e: MouseEvent) => {
      e.preventDefault(); // keep focus on the query input; create via the core
      if (this.session) this.runEffects(this.session.picker_click(c.abs) as CoreEffect[]);
    });
    const bullet = document.createElement("span");
    bullet.className = "picker-bullet"; // empty cell, keeps names column-aligned with entries
    row.append(bullet);
    const main = document.createElement("span");
    main.className = "picker-main picker-italic";
    main.textContent = c.is_dir ? `+ Create directory ${c.name}/` : `+ Create file ${c.name}`;
    row.append(main);
    return row;
  }

  /** The results list scrolled: refetch the window around the new position when it's left the loaded
   *  range. `picker_scrolled` returns no effects (and we don't repaint) when the window still covers
   *  the view — including the programmatic scrolls from reveal / scroll-reset. */
  private onPickerListScroll(): void {
    if (!this.session || !this.snapshot?.picker) return;
    const first = Math.max(0, Math.floor(this.pickerListEl.scrollTop / this.pickerRowH));
    const fx = this.session.picker_scrolled(first) as CoreEffect[];
    if (fx.length) this.runEffects(fx, true);
  }

  private renderPickerList(p: PickerView, v: CoreView): void {
    const projectPaths = v.project_paths;
    const list = this.pickerListEl;
    // No results at all: show a status line so a slow search (grep streaming, references resolving)
    // reads as "working", not "broken". Gated on `total_matches`, NOT `items.length` — a scroll
    // refetch momentarily empties the window while results still exist, and collapsing the spacer
    // here would reset scrollTop to the top. That case falls through to the spacer render below.
    if (p.total_matches === 0) {
      if (p.create) {
        list.classList.add("filled");
        list.replaceChildren(this.makePickerCreateRow(p));
        list.querySelector(".picker-row.selected")?.scrollIntoView({ block: "nearest" });
        return;
      }
      let text = "";
      if (p.kind === "references") {
        text = p.ticking ? "Finding references…" : "No references found";
      } else if (p.ticking) {
        text = "Searching…";
      } else if (p.query.length > 0) {
        text = "No matches";
      }
      if (text) {
        const msg = document.createElement("div");
        msg.className = "picker-empty";
        msg.textContent = text;
        list.classList.add("filled");
        list.replaceChildren(msg);
      } else {
        list.classList.remove("filled"); // empty query, nothing to search yet — no border/message
        list.replaceChildren();
      }
      return;
    }
    list.classList.add("filled");
    // Path budget for the row (chars), and the disambiguated root labels — both computed once.
    const ls = getComputedStyle(list);
    const budget = charBudget(list.clientWidth * 0.6, `${ls.fontSize} ${ls.fontFamily}`);
    const labels = rootLabels(projectPaths);
    // Virtual scroll (matching the native client): a full-height spacer sized to the whole result set
    // (in display rows) holds the loaded window, absolutely positioned `window_base` rows down — so
    // the scrollbar spans every result and scrolling into an unloaded range refetches it
    // (onPickerListScroll). Grep rows are grouped per file in a `.grep-section` so the file header can
    // stick while its hits scroll; a hit's `scroll-margin-top` keeps it clear of that sticky header.
    const win = document.createElement("div");
    win.className = "picker-window";
    const localSel = p.selected - p.offset;
    let selectedRow: HTMLElement | null = null;
    let prevGrepKey: string | null = null;
    let section: HTMLElement | null = null;
    p.items.forEach((item, i) => {
      // Grep: a non-selectable file header before the first hit of each file in the window.
      if (item.kind === "grep_hit") {
        const key = `${item.path_index}\0${item.relative_path}`;
        if (key !== prevGrepKey) {
          prevGrepKey = key;
          section = document.createElement("div");
          section.className = "grep-section";
          const h = document.createElement("div");
          h.className = "picker-row grep-header";
          if (labels.length > 1) {
            const label = labels[item.path_index] ?? `root ${item.path_index}`;
            const pb = Math.max(8, budget - [...label].length - 2);
            h.textContent = `${label}: ${truncatePath(item.relative_path, undefined, pb).display}`;
          } else {
            h.textContent = truncatePath(item.relative_path, undefined, budget).display;
          }
          section.append(h);
          win.append(section);
        }
      }
      // File-backed rows are <a> so Ctrl/Cmd/middle-click opens in a new browser tab (the boot URL
      // reader lands the tab on the file); other rows stay plain <div>s. CSS makes them look alike.
      const href = this.pickerItemUrl(item, v);
      const row: HTMLElement = document.createElement(href ? "a" : "div");
      if (href) (row as HTMLAnchorElement).href = href;
      row.className = i === localSel ? "picker-row selected" : "picker-row";
      if (item.kind === "grep_hit") row.classList.add("grep-hit");
      if (i === localSel) selectedRow = row;
      row.addEventListener("mousedown", (e: MouseEvent) => {
        // New-tab gesture on an anchor row: let the browser open the <a> itself.
        if (href && (e.ctrlKey || e.metaKey || e.button === 1)) return;
        e.preventDefault(); // keep focus on the query input; open in this tab via the core
        if (this.session) this.runEffects(this.session.picker_click(p.offset + i) as CoreEffect[]);
      });
      if (href) {
        // A plain left-click already opened it in-place (mousedown above) — stop the <a> from also
        // navigating this tab; a modified click falls through to the browser's new-tab handling.
        row.addEventListener("click", (e: MouseEvent) => {
          if (!(e.ctrlKey || e.metaKey || e.button === 1)) e.preventDefault();
        });
      }
      const d = describePickerItem(item, projectPaths, labels, budget);
      if (d.bullet) {
        const b = document.createElement("span");
        if (d.bulletIcon) {
          // The status bar's SVG icon, coloured by its class: lsp-* for LSP rows (default), or a
          // sev-* class for diagnostics. Spins when busy.
          b.className = `picker-bullet icon ${d.bulletIconClass ?? d.bulletIcon}`;
          b.append(statusIcon(d.bulletIcon, d.bulletSpin));
        } else {
          // Fixed-width cell so names stay aligned; the • only shows when coloured (a git change).
          b.className = d.bulletStatus ? `picker-bullet picker-bullet-${d.bulletStatus}` : "picker-bullet";
          b.textContent = d.bulletStatus ? "•" : "";
        }
        row.append(b);
      }
      if (d.prefix) {
        const pre = document.createElement("span");
        pre.className = d.prefixClass ? `picker-prefix ${d.prefixClass}` : "picker-prefix";
        pre.textContent = d.prefix;
        row.append(pre);
      }
      const main = document.createElement("span");
      main.className =
        "picker-main" +
        (d.dim ? " picker-dim" : d.dir ? " picker-dir" : "") +
        (d.italic ? " picker-italic" : "");
      main.append(matched(d.primary, d.matches));
      row.append(main);
      if (d.suffix) {
        const s = document.createElement("span");
        s.className = "picker-suffix";
        s.textContent = d.suffix;
        row.append(s);
      }
      if (d.meta) {
        const m = document.createElement("span");
        m.className = "picker-meta";
        m.textContent = d.meta;
        row.append(m);
      }
      if (d.dirty) {
        const dot = document.createElement("span");
        dot.className = `picker-dirty-dot picker-dirty-${d.dirty}`;
        dot.textContent = "●";
        row.append(dot);
      }
      (section ?? win).append(row);
    });

    const spacer = document.createElement("div");
    spacer.className = "picker-spacer";
    spacer.append(win);
    // The Explorer's "+ Create …" row trails the final match (non-grep), absolutely placed within the
    // spacer at display-row `total_matches` so it follows the last item.
    let createRow: HTMLElement | null = null;
    if (p.create && p.offset + p.items.length >= p.total_matches) {
      createRow = this.makePickerCreateRow(p);
      if (p.selected === p.total_matches) selectedRow = createRow;
      spacer.append(createRow);
    }

    // Position the window/create row and size the spacer from the row height (a `picker/update` push
    // never carries the create row, so add a row for it). Applied before insertion — the window/create
    // are absolute, so without an explicit spacer height the list would collapse and clamp scrollTop.
    const applyGeometry = () => {
      win.style.top = `${p.window_base * this.pickerRowH}px`;
      spacer.style.height = `${(p.total_display_rows + (createRow ? 1 : 0)) * this.pickerRowH}px`;
      if (createRow) createRow.style.top = `${p.total_matches * this.pickerRowH}px`;
      list.style.setProperty("--picker-row-h", `${this.pickerRowH}px`);
    };
    applyGeometry();
    list.replaceChildren(spacer);
    // Re-measure the row height once in the DOM (fractional, so it doesn't drift over a long list) and
    // re-apply if it changed.
    const probe = win.querySelector(".picker-row:not(.grep-header)") as HTMLElement | null;
    const measured = probe?.getBoundingClientRect().height ?? 0;
    if (measured > 0 && Math.abs(measured - this.pickerRowH) > 0.5) {
      this.pickerRowH = measured;
      applyGeometry();
    }

    // Only move the scroll on an explicit signal: jump to the top on a query change, or reveal the
    // highlighted row after keyboard nav / a refetch. A free wheel-scroll sets neither, so it stays
    // where the user left it. (`scroll-margin-top` on grep hits keeps a revealed hit below the sticky
    // file header.) Reset wins — the selection is row 0.
    if (this.pickerScrollReset) {
      list.scrollTop = 0;
      this.pickerScrollReset = false;
      this.pickerReveal = false;
    } else if (this.pickerReveal && selectedRow) {
      selectedRow.scrollIntoView({ block: "nearest" });
      this.pickerReveal = false;
    } else if (this.pickerReveal && p.total_matches === 0) {
      // Nothing to reveal (empty result) — drop the pending reveal so it doesn't fire later.
      this.pickerReveal = false;
    }
    // Otherwise keep `pickerReveal` armed: the resumed window hasn't painted the selected row yet
    // (it arrives a render later), and we want to scroll to it once it does.
  }

  /** The status bar, matching the TUI / old web client: left = buffer-state dot + `[project] label`
   *  + git cluster; right = search/grep counters + diagnostic glyphs + position + LSP glyph. The mode
   *  is shown by the cursor shape (block/I-beam/underscore), not text. */
  private renderStatus(v: CoreView): void {
    const left = document.createElement("span");
    left.className = "status-left";
    const color = bufferStateColor(v);
    if (color) {
      const dot = document.createElement("span");
      dot.className = "status-dot";
      dot.style.color = color;
      dot.textContent = "●";
      left.append(dot);
    }
    const proj = v.project ? `[${v.project}] ` : "";
    left.append(proj);
    const name = document.createElement("span");
    if (v.buffer.transient) name.className = "status-transient"; // preview buffers slant
    // The file label takes at most the left half of the bar, segment-elided so the filename
    // survives (CSS ellipsis is the safety net for the char-budget estimate's error).
    const barStyle = getComputedStyle(this.statusEl);
    const labelBudget = Math.max(
      12,
      charBudget(this.statusEl.clientWidth * 0.5, `${barStyle.fontSize} ${barStyle.fontFamily}`) -
        [...proj].length,
    );
    name.textContent = truncatePath(v.buffer.label, undefined, labelBudget).display;
    left.append(name);
    // Git cluster: `⎇ branch  +u(s) ~u(s) -u(s)` (unstaged then staged-in-parens; zero omitted).
    const gs = v.window?.git_status;
    if (gs) {
      if (gs.branch) {
        const b = document.createElement("span");
        b.className = "status-git git-branch";
        b.textContent = `⎇  ${gs.branch}`;
        left.append(b);
      }
      const u = gs.unstaged;
      const s = gs.staged;
      const classes: [string, string, number, number][] = [
        ["+", "git-added", u?.added ?? 0, s?.added ?? 0],
        ["~", "git-modified", u?.modified ?? 0, s?.modified ?? 0],
        ["-", "git-deleted", u?.deleted ?? 0, s?.deleted ?? 0],
      ];
      for (const [sigil, cls, un, st] of classes) {
        if (!un && !st) continue;
        let tok = sigil;
        if (un > 0) tok += String(un);
        if (st > 0) tok += `(${st})`;
        const el = document.createElement("span");
        el.className = `status-git ${cls}`;
        el.textContent = tok;
        left.append(el);
      }
    }

    const right = document.createElement("span");
    right.className = "status-right";
    if (v.search.active) {
      const label = searchCountLabel(v.search.summary);
      if (label) {
        const c = document.createElement("span");
        c.textContent = label;
        right.append(c);
      }
    }
    const grep = v.buffer.cursor.grep_position;
    if (grep) {
      const g = document.createElement("span");
      g.textContent = `grep ${grep.current}/${grep.total}`;
      right.append(g);
    }
    const dc = v.diagnostics;
    const diagGroup = document.createElement("span");
    diagGroup.className = "status-diag-group";
    const diags: [number, IconKind, string][] = [
      [dc.errors, "error", "sev-error"],
      [dc.warnings, "warning", "sev-warning"],
      [dc.infos, "info", "sev-information"],
      [dc.hints, "hint", "sev-hint"],
    ];
    for (const [n, kind, cls] of diags) {
      if (n > 0) {
        const d = document.createElement("span");
        d.className = cls;
        d.append(statusIcon(kind), ` ${n}`);
        diagGroup.append(d);
      }
    }
    if (diagGroup.childElementCount > 0) right.append(diagGroup);
    const pos = document.createElement("span");
    pos.textContent = positionLabel(v);
    right.append(pos);
    const lsp = lspIcon(v.lsp);
    if (lsp) {
      const g = document.createElement("span");
      g.className = lsp.cls;
      g.append(statusIcon(lsp.kind, lsp.spin));
      right.append(g);
    }

    this.statusEl.replaceChildren(left, right);
    document.title = v.project ? (v.buffer.label ? `[${v.project}] ${v.buffer.label}` : `[${v.project}]`) : "Aether";
  }

  // ---- toasts ---------------------------------------------------------------------------------

  private toast(message: string, kind: ToastLevel = "info"): void {
    const t = document.createElement("div");
    t.className = `toast ${kind}`;
    t.textContent = message;
    this.toastsEl.append(t);
    window.setTimeout(() => t.classList.add("fade"), 3000);
    window.setTimeout(() => t.remove(), 3600);
  }
}

const root = document.getElementById("app");
if (root) new Shell(root, resolveConfig());
