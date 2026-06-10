//! Native picker overlay (command-palette style). The server owns the candidate cache, fuzzy
//! matching, and ranking and pushes `picker/update`s with `match_indices`; this component is just a
//! query input + result list + selection. Same protocol the TUI uses, different presentation
//! (docs/web-client.md §2.7). Phase 4: Files, Buffers, Grep, Explorer.
//!
//! Windowing: the client tracks an absolute `selected` index into the full ranked result set and a
//! loaded window `[offset, offset+items.length)`. When the selection leaves the window we re-issue
//! `picker/view` at a new offset (mirroring the TUI's slide-to-keep-visible).
//!
//! Explorer navigation matches the TUI: Alt-l / Enter enters the highlighted directory (or root);
//! Alt-h / Alt-Backspace clears the filter, then steps to the parent directory, then (multi-root
//! projects) into roots mode. File rows confirm — but Explorer items carry only a name, so opening
//! goes through picker/select to resolve the absolute path (true for all kinds here).

import type { RpcClient } from "./client";
import { rootLabels } from "./labels";
import { charBudget, truncatePath } from "./paths";
import { confirmDialog, lspInfoDialog } from "./modal";
import type { LspInfoData } from "./modal";
import { lspStateClass, statusIcon } from "./icons";
import type { BufferDirtyState, CaseMode, DirectoryListResult, GitStatus, LspProgress, PickerFilters, PickerItem, PickerKind, PickerUpdateParams, PickerViewResult, ScopedPath } from "./protocol";

type ToastKind = "info" | "error" | "warning" | "success";

const LIMIT = 64; // minimum window size fetched per view (see fetchLimit)
const PAGE = 10; // PageUp/PageDown step
// Vertical gap (px) between grep file groups, applied as margin-top on every .grep-section (set
// inline, since the virtual-scroll geometry below has to count these same pixels).
const GREP_GROUP_GAP = 6;

/** Query-input placeholder per picker: the picker's action, ellipsised. Kept in sync with the
 *  terminal client's `picker_placeholder` (crates/aether-tui/src/ui.rs). */
const PLACEHOLDER: Record<PickerKind, string> = {
  files: "Find files…",
  buffers: "Switch buffer…",
  grep: "Grep workspace…",
  explorer: "Explore files…",
  projects: "Switch project…",
  diagnostics: "List diagnostics…",
  lsp_servers: "List LSPs…",
  references: "List references…",
};

/** `PickerFilters` with every field present — the client's working copy (the wire shape skips
 *  defaults; normalizing once keeps the chip/toggle logic free of undefined-checks). */
/** Which filter a chip stands for; `dir` and `glob` carry their index into the chip list (the
 *  rendered row is the stored list, so row index = storage index). There is no root chip: a
 *  whole-root scope is a dir chip with an empty relative path. */
type ChipId =
  | { t: "dir"; i: number }
  | { t: "glob"; i: number }
  | { t: "case" }
  | { t: "word" }
  | { t: "lit" }
  | { t: "ignored" }
  | { t: "hidden" }
  | { t: "changed" };

interface Chip {
  id: ChipId;
  label: string;
}

/** One chip, by value — the element of the client's ordered filter state (the single source of
 *  truth; the wire `PickerFilters` is derived by `wireFilters` and converted back by
 *  `adoptFilters`). `case` holds "sensitive" | "insensitive" — "smart" is "no chip"; the
 *  ignored/hidden values record the per-kind direction at creation time (the Explorer hides,
 *  Grep includes), so the wire conversion needs no kind context. */
type ChipValue =
  | { t: "dir"; d: ScopedPath }
  | { t: "glob"; g: string }
  | { t: "case"; mode: CaseMode }
  | { t: "word" }
  | { t: "lit" }
  | { t: "ignored"; hide: boolean }
  | { t: "hidden"; hide: boolean }
  | { t: "changed" };

function sameScope(a: ScopedPath, b: ScopedPath): boolean {
  return a.path_index === b.path_index && a.relative_path === b.relative_path;
}

/** True when two values are the same chip — for the repeatable kinds that means equal values
 *  (dirs and globs dedupe at commit time), for flags just the kind (`case` mid-cycle and an
 *  ignored/hidden direction change are the same chip). */
function sameChip(a: ChipValue, b: ChipValue): boolean {
  if (a.t !== b.t) return false;
  if (a.t === "dir" && b.t === "dir") return sameScope(a.d, b.d);
  if (a.t === "glob" && b.t === "glob") return a.g === b.g;
  return true;
}

/** Render the chip row: the stored list, verbatim — insertion order *is* the storage order.
 *  Mirrors the terminal client's `PickerState::chips` (crates/aether-tui/src/picker.rs). */
function deriveChips(values: ChipValue[], projectPaths: string[]): Chip[] {
  return values.map((v, i): Chip => {
    switch (v.t) {
      case "dir": {
        // Compact: the trailing slash implies "directory" (no `dir:` prefix). Multi-root
        // scopes read like the status bar: `{root label}: {path}/`, with the same
        // disambiguated root labels; an empty relative path is a whole-root scope.
        let label: string;
        if (projectPaths.length > 1) {
          const rootLabel = rootLabels(projectPaths)[v.d.path_index] ?? "?";
          label = v.d.relative_path === "" ? rootLabel : `${rootLabel}: ${v.d.relative_path}/`;
        } else {
          label = `${v.d.relative_path}/`;
        }
        return { id: { t: "dir", i }, label };
      }
      case "glob":
        return { id: { t: "glob", i }, label: v.g };
      case "case":
        return { id: { t: "case" }, label: v.mode === "insensitive" ? "aa" : "Aa" };
      case "word":
        return { id: { t: "word" }, label: "wd" };
      case "lit":
        return { id: { t: "lit" }, label: "lit" };
      case "ignored":
        return { id: { t: "ignored" }, label: v.hide ? "-ig" : "+ig" };
      case "hidden":
        return { id: { t: "hidden" }, label: v.hide ? "-." : "+." };
      case "changed":
        return { id: { t: "changed" }, label: "Δ" };
    }
  });
}

/** Fold a chip list into the wire format — the normalized, unordered `PickerFilters`. */
function wireFilters(values: ChipValue[]): PickerFilters {
  const f: PickerFilters = {};
  for (const v of values) {
    switch (v.t) {
      case "dir":
        (f.directories ??= []).push(v.d);
        break;
      case "glob":
        (f.globs ??= []).push(v.g);
        break;
      case "case":
        f.case = v.mode;
        break;
      case "word":
        f.whole_word = true;
        break;
      case "lit":
        f.fixed_string = true;
        break;
      case "ignored":
        if (v.hide) f.hide_ignored = true;
        else f.include_ignored = true;
        break;
      case "hidden":
        if (v.hide) f.hide_hidden = true;
        else f.include_hidden = true;
        break;
      case "changed":
        f.changed_only = true;
        break;
    }
  }
  return f;
}

/** Convert a wire filter set into a chip list. The wire carries no order, so adopted chips
 *  come back in canonical order (dirs, globs, flags) — insertion order is session-ephemeral. */
function adoptFilters(f?: PickerFilters): ChipValue[] {
  const values: ChipValue[] = [];
  for (const d of f?.directories ?? []) values.push({ t: "dir", d });
  for (const g of f?.globs ?? []) values.push({ t: "glob", g });
  if (f?.case && f.case !== "smart") values.push({ t: "case", mode: f.case });
  if (f?.whole_word) values.push({ t: "word" });
  if (f?.fixed_string) values.push({ t: "lit" });
  if (f?.include_ignored || f?.hide_ignored)
    values.push({ t: "ignored", hide: !!f?.hide_ignored });
  if (f?.include_hidden || f?.hide_hidden) values.push({ t: "hidden", hide: !!f?.hide_hidden });
  if (f?.changed_only) values.push({ t: "changed" });
  return values;
}

// Only the whole-word chip underlines (`.flag`): "wd" alone reads as a stray token; the other
// abbreviations (Aa, +ig, Δ, …) carry enough shape on their own.

/** Normalize a committed glob. `null` means "don't keep a chip": empty input, or a degenerate
 *  match-everything glob (`*`, `**`, also negated — `!*` would exclude everything). A glob
 *  starting with `.` (or `!.`) with no other glob syntax is an extension shorthand:
 *  `.rs` → `*.rs`. Mirrors the TUI's `normalize_glob`. */
function normalizeGlob(text: string): string | null {
  const trimmed = text.trim();
  const neg = trimmed.startsWith("!") ? "!" : "";
  const body = neg ? trimmed.slice(1) : trimmed;
  if (body === "" || body === "*" || body === "**") return null;
  const extensionShorthand = body.startsWith(".") && !/[*?[/]/.test(body);
  return extensionShorthand ? `${neg}*${body}` : `${neg}${body}`;
}

/** Indices of `names` matching `filter` as a smartcase prefix (everything on an empty filter):
 *  case-insensitive unless the filter contains an uppercase letter. Used for the dir editor's
 *  root typeahead (mirroring the TUI's `root_candidates`) and its path-field directory
 *  suggestions (mirroring the save-as prompt's `matching_indices`). */
function prefixMatchIndices(names: string[], filter: string): number[] {
  const all = names.map((_, i) => i);
  if (filter === "") return all;
  const sensitive = /[A-Z]/.test(filter);
  const needle = sensitive ? filter : filter.toLowerCase();
  return all.filter((i) =>
    (sensitive ? names[i] : names[i].toLowerCase()).startsWith(needle),
  );
}

/** The root field's typeahead candidates. */
function rootCandidates(labels: string[], filter: string): number[] {
  return prefixMatchIndices(labels, filter);
}

/** Whether a filter chip applies to this picker kind (chords are clean no-ops elsewhere). */
function filterApplies(kind: PickerKind, id: ChipId): boolean {
  if (kind === "grep") return true;
  if (kind === "files") return id.t === "dir" || id.t === "glob" || id.t === "changed";
  if (kind === "explorer") return id.t === "ignored" || id.t === "hidden" || id.t === "changed";
  return false;
}

export interface PickerOptions {
  client: RpcClient;
  kind: PickerKind;
  onConfirm: (item: PickerItem) => void;
  onClose: () => void;
  /** Explorer: absolute directory to open in (null = server default). */
  explorerInitialDir?: string | null;
  /** Explorer: name to pre-select on open (the current file's basename). */
  explorerSelectName?: string;
  /** Project roots, for resolving Root rows in the Explorer's roots mode. */
  projectPaths?: string[];
  /** Diagnostics / References: the buffer the candidate set is scoped to (diagnostics to list;
   *  cursor to resolve references at). Passed as `buffer_id` on the reset open. */
  scopedBufferId?: number;
  /** Grep: the active buffer, so the picker opens centered on the cursor's nearest hit. */
  activeBufferId?: number;
  /** Surface a transient message (deletion result / errors). */
  onToast?: (message: string, kind?: ToastKind) => void;
  /** Create + open a file at this absolute path (Explorer "+ create file"). */
  onCreatePath?: (absPath: string) => void;
  /** Create + activate a new project (Projects "+ create project"). */
  onCreateProject?: (name: string) => void;
  /** Explorer `Ctrl-g`/`Ctrl-f`: switch to the Grep/Files picker carrying the seeded filters
   *  (the browsed dir as scope, explorer flags translated). The host closes this picker and
   *  opens the target with `initialFilters`. */
  onSwitch?: (kind: "grep" | "files", filters: PickerFilters) => void;
  /** Open with this filter set (replacing whatever the server persisted) — the seeded scope
   *  from an Explorer switch. */
  initialFilters?: PickerFilters;
  /** Opener URL for a picker item, or null. When it returns a URL the row is rendered as an
   *  `<a href>` so Ctrl/Cmd/middle-click opens it in a new browser tab. `explorerDir` is the current
   *  explorer directory (for resolving `dir_entry` paths); ignored by the other picker kinds. */
  fileUrl?: (item: PickerItem, explorerDir: string | null) => string | null;
}

export class Picker {
  readonly kind: PickerKind;
  private client: RpcClient;
  private onConfirm: (item: PickerItem) => void;
  private onClose: () => void;
  private projectPaths: string[];
  private scopedBufferId: number | undefined;
  private activeBufferId: number | undefined;
  private onToast: (message: string, kind?: ToastKind) => void;
  private onCreatePath: (absPath: string) => void;
  private onCreateProject: (name: string) => void;
  private fileUrl: (item: PickerItem, explorerDir: string | null) => string | null;

  private overlay: HTMLElement;
  private pathEl: HTMLElement;
  private chipsEl: HTMLElement;
  private input: HTMLInputElement;
  private editorRow: HTMLElement;
  private listEl: HTMLElement;
  private countEl: HTMLElement;

  // Filter chips (docs/picker-filters.md). `chipValues` is the ordered chip list — the single
  // source of truth for the active filters, in insertion order; the wire `PickerFilters` is
  // derived per send (wireFilters) and converted back on open/resume (adoptFilters), so the
  // order itself is session-ephemeral. `chipSelected` indexes the row (= the list) while
  // chip-editing keys are captured. `chipEditor` is the glob/dir editor revealed on its own
  // row *below* the input (chips + query stay visible while editing); its fields are real
  // inputs built per open, so browser focus moves into the row and back to the query input on
  // close.
  private chipValues: ChipValue[] = [];
  private chipSelected: number | null = null;
  /** Filter set this picker opened with (Explorer switch seed); carried on the open view. */
  private seedFilters: PickerFilters | undefined;
  private onSwitch: (kind: "grep" | "files", filters: PickerFilters) => void = () => {};
  private chipEditor: {
    kind: "glob" | "dir";
    edit: number | null;
    /** Dir: which segment is live (renders the one-and-only <input>); glob is always "path". */
    field: "root" | "path";
    rootSelected: number;
    rootIndex: number;
    /** Dir: cached `directory/list` names (subdirectories only — files never complete a dir
     *  scope) for the dir portion of the path field, powering its ghost suggestions. */
    listing: string[];
    /** Dir: the absolute path `listing` was last synced against (the staleness key). */
    listingDirAbs: string;
    /** Dir: where `listing` stands relative to `listingDirAbs` — only a loaded listing can
     *  vouch for the typed path's validity; `failed` means the dir portion doesn't exist. */
    listingState: "pending" | "loaded" | "failed";
    /** Dir: position within the filtered match set producing the current path ghost. */
    suggestionIdx: number;
  } | null = null;
  private editorRootInput: HTMLInputElement | null = null;
  private editorPathInput: HTMLInputElement | null = null;
  private editorGhostEl: HTMLElement | null = null;
  private editorPathGhostEl: HTMLElement | null = null;
  private editorSepEl: HTMLElement | null = null;
  /** The unfocused segment's static text (null while that segment is the live input). */
  private editorRootSpan: HTMLElement | null = null;
  private editorPathSpan: HTMLElement | null = null;

  // Open LSP-info dialog (if any) plus the server it's showing, so status/progress pushes can
  // refresh it live. Cleared when the dialog closes.
  private lspModal: { update: (info: LspInfoData) => void; language: string; root: string } | null = null;

  private items: PickerItem[] = [];
  private offset = 0; // absolute index of items[0] in the full result set
  private selected = 0; // absolute index of the highlighted item
  private total = 0;
  private generation = 0;
  // Generation of the last *processed* update — lets onUpdate tell a same-query re-rank (preserve the
  // highlighted item) from the first push after a query change (selection already reset to the top).
  private lastPushGeneration = -1;
  private ticking = false;
  private requestedOffset = 0;

  // Explorer state.
  private explorerDir: string | null = null;
  private explorerParent: string | null = null;
  /** Predicate to select once the next listing arrives (explorer current-file/left-dir; grep jump). */
  private pendingSelectMatch: ((item: PickerItem) => boolean) | null = null;
  private rowH = 24; // measured row height (px), for virtual-scroll spacer + positioning
  private listFetchInFlight = false; // guards scroll-driven window fetches from piling up
  private selectedRowEl: HTMLElement | null = null; // current selected row, for scroll-into-view
  private scrollToSelOnUpdate = false; // a selection-driven fetch is pending; scroll once it lands
  // Grep virtual-scroll coordinate space is DISPLAY ROWS (hits + per-file headers), reported by the
  // server so the spacer counts headers; null for non-grep kinds (which use plain item rows).
  private grepDisplayOffset: number | null = null;
  private grepTotalDisplayRows: number | null = null;
  // Loaded-window geometry in PIXELS, set by renderList's applyGeometry and read by onListScroll.
  // Px is the one coordinate space where both the scroll position and the loaded window's bounds
  // are exact regardless of how grep's per-group gaps distribute — row-space comparisons drifted
  // when gaps clustered unevenly (skewed file-group sizes) and mis-fetched.
  private winTopPx = 0;
  private winBottomPx = 0;
  private spacerPx = 0;

  constructor(opts: PickerOptions) {
    this.client = opts.client;
    this.kind = opts.kind;
    this.onConfirm = opts.onConfirm;
    this.onClose = opts.onClose;
    this.projectPaths = opts.projectPaths ?? [];
    this.scopedBufferId = opts.scopedBufferId;
    this.activeBufferId = opts.activeBufferId;
    this.onToast = opts.onToast ?? (() => {});
    this.onCreatePath = opts.onCreatePath ?? (() => {});
    this.onCreateProject = opts.onCreateProject ?? (() => {});
    this.fileUrl = opts.fileUrl ?? (() => null);
    this.onSwitch = opts.onSwitch ?? (() => {});

    this.overlay = document.createElement("div");
    this.overlay.className = "overlay";
    const box = document.createElement("div");
    box.className = "picker";
    const inputRow = document.createElement("div");
    inputRow.className = "picker-input-row";
    this.pathEl = document.createElement("span");
    this.pathEl.className = "picker-path";
    this.pathEl.style.display = "none";
    this.chipsEl = document.createElement("span");
    this.chipsEl.className = "picker-chips";
    this.input = document.createElement("input");
    this.input.className = "picker-input";
    this.input.placeholder = PLACEHOLDER[this.kind];
    this.input.spellcheck = false;
    this.input.autocomplete = "off";
    this.countEl = document.createElement("span");
    this.countEl.className = "picker-count";
    // The "X/Y" summary sits to the right of the input, like the terminal UI. Chips lead the
    // row, before the explorer breadcrumb, which stays flush with the query it prefixes.
    inputRow.append(this.chipsEl, this.pathEl, this.input, this.countEl);
    // The chip editor line (glob/dir) reveals between the input row and the results.
    this.editorRow = document.createElement("div");
    this.editorRow.className = "picker-editor-row";
    this.editorRow.style.display = "none";
    this.listEl = document.createElement("div");
    this.listEl.className = "picker-list";
    box.append(inputRow, this.editorRow, this.listEl);
    this.overlay.append(box);
    document.body.append(this.overlay);

    this.input.addEventListener("input", () => this.onQueryInput());
    this.input.addEventListener("keydown", (e) => this.onKey(e));
    this.listEl.addEventListener("scroll", () => this.onListScroll(), { passive: true });
    this.overlay.addEventListener("mousedown", (e) => {
      if (e.target === this.overlay) this.onClose();
    });

    this.input.focus();
    // A seeded open (Explorer switch) starts from the given filter set: render its chips now
    // and carry it on the open view so the server replaces its persisted filters.
    if (opts.initialFilters) {
      this.seedFilters = opts.initialFilters;
      this.chipValues = adoptFilters(opts.initialFilters);
      this.renderChips();
    }
    if (this.kind === "explorer") {
      void this.viewExplorer({ directory_path: opts.explorerInitialDir ?? null, selectName: opts.explorerSelectName });
    } else if (this.kind === "grep") {
      void this.viewGrepOpen();
    } else {
      // References resolves asynchronously (an LSP round-trip); open in the loading state so the
      // first render — if it beats the server's initial push — reads "Finding references…" rather
      // than "No references found".
      if (this.kind === "references") this.ticking = true;
      void this.view(true, 0);
    }
  }

  /** Grep opens without resetting (the server persists hits across hide/reopen) and centers on the
   *  nearest hit at/after the active buffer's cursor, so reopening lands on "where you are". */
  private async viewGrepOpen(): Promise<void> {
    const r = await this.client.rpc<PickerViewResult>("picker/view", {
      kind: "grep",
      reset: false,
      offset: 0,
      limit: this.fetchLimit(),
      center_on_cursor_grep_hit: this.activeBufferId,
      // A seeded open replaces the persisted filter set (the echo below adopts it back).
      filters: this.seedFilters,
    });
    this.generation = r.generation; // adopt the persisted query's generation baseline
    this.offset = r.effective_offset;
    this.requestedOffset = r.effective_offset;
    this.input.value = r.query; // restore the persisted query text
    // Restore the persisted chips alongside the query. The wire carries no order, so restored
    // chips come back canonical; insertion order is session-ephemeral.
    this.chipValues = adoptFilters(r.filters);
    this.renderChips();
    const target = r.effective_center_on;
    if (target) this.pendingSelectMatch = (it) => sameGrepHit(it, target);
    this.renderList();
  }

  destroy(): void {
    this.overlay.remove();
  }

  /** The current query text (used to prime search when a grep hit is opened). */
  query(): string {
    return this.input.value;
  }

  /** Server-pushed window contents. Discard stale generations (in-flight prior queries). */
  onUpdate(p: PickerUpdateParams): void {
    if (p.kind !== this.kind || p.generation !== this.generation) return;
    // Across a same-query re-rank — the server re-pushing the window without a query change, e.g. the
    // buffers list reordering when a buffer is opened (in this or another tab) — keep the highlight on
    // the same *item* rather than the same index, which would otherwise jump to whatever now sits at
    // that index. A query change bumps the generation, so prevKey is null then and selection (already
    // reset to the top by query()) stands.
    const prevKey =
      p.generation === this.lastPushGeneration ? this.selectedKeyInWindow() : null;
    this.lastPushGeneration = p.generation;
    this.items = p.items;
    this.offset = p.offset;
    this.total = p.total_matches;
    this.ticking = p.ticking;
    this.grepDisplayOffset = p.grep_display_offset ?? null;
    this.grepTotalDisplayRows = p.grep_total_display_rows ?? null;
    let resolved = false;
    if (this.pendingSelectMatch) {
      const idx = this.items.findIndex(this.pendingSelectMatch);
      if (idx >= 0) {
        this.selected = this.offset + idx;
        this.pendingSelectMatch = null;
        resolved = true; // a centering target just landed — scroll to it
      } else if (!this.ticking) {
        this.pendingSelectMatch = null; // not in the listing — give up
      }
    } else if (prevKey != null) {
      const idx = this.items.findIndex((it) => itemKey(it) === prevKey);
      if (idx >= 0) this.selected = this.offset + idx; // follow the item to its new position
    }
    // No ensureWindow here: the loaded window can legitimately differ from the selection's location
    // (the user scrolled away). Re-centering on the selection here would replace the just-loaded
    // scroll window, making results flash and vanish. ensureWindow runs only when the selection moves.
    this.renderList();
    // Keep an open LSP-info dialog live: the LSP picker re-pushes on every `lsp/status_changed`, so
    // refresh the dialog from the matching server's fresh row when one's in the loaded window.
    if (this.lspModal) {
      const m = this.lspModal;
      const fresh = this.items.find(
        (it): it is Extract<PickerItem, { kind: "lsp_server" }> =>
          it.kind === "lsp_server" && it.language === m.language && it.workspace_root === m.root,
      );
      if (fresh) m.update(this.lspInfoFromItem(fresh));
    }
    // Scroll to the selection only for selection-driven updates (centering / a pending move) — never
    // for plain scroll-fetches, which would yank the view back to the selection. Grep centering
    // targets (reopen on the cursor's nearest hit; Alt-l/h file jump) align to the TOP of the list
    // — there's context below to read, and the row's scroll-margin keeps it clear of the sticky
    // file header. Everything else keeps "nearest".
    if (resolved || this.scrollToSelOnUpdate) {
      this.scrollSelectionIntoView(resolved && this.kind === "grep" ? "start" : "nearest");
      this.scrollToSelOnUpdate = false;
    }
  }

  /** Identity key of the highlighted item, if it's within the loaded window (null otherwise — e.g.
   *  the selection scrolled out of view, or it's the synthetic "+ create" row). */
  private selectedKeyInWindow(): string | null {
    const local = this.selected - this.offset;
    const it = local >= 0 && local < this.items.length ? this.items[local] : undefined;
    return it ? itemKey(it) : null;
  }

  private async view(reset: boolean, offset: number): Promise<void> {
    this.requestedOffset = offset;
    const r = await this.client.rpc<PickerViewResult>("picker/view", {
      kind: this.kind,
      reset,
      offset,
      limit: this.fetchLimit(),
      // Diagnostics and References are scoped to a buffer, required when (re)opening the set.
      buffer_id:
        reset && (this.kind === "diagnostics" || this.kind === "references")
          ? this.scopedBufferId
          : undefined,
      // A seeded open (Explorer switch) carries its filter set past the reset.
      filters: reset ? this.seedFilters : undefined,
    });
    if (reset) this.generation = r.generation;
  }

  /** (Re)list a directory or the project roots, resetting query + selection. `selectName` frames
   *  and pre-selects a matching entry once the listing arrives. */
  private async viewExplorer(opts: {
    directory_path?: string | null;
    roots?: boolean;
    selectName?: string;
  }): Promise<void> {
    const centerOn: PickerItem | null = opts.selectName
      ? { kind: "dir_entry", name: opts.selectName, is_dir: true, match_indices: [] }
      : null;
    const r = await this.client.rpc<PickerViewResult>("picker/view", {
      kind: "explorer",
      reset: true,
      offset: 0,
      limit: this.fetchLimit(),
      directory_path: opts.roots ? null : opts.directory_path ?? null,
      explorer_roots: opts.roots ?? false,
      center_on: centerOn,
      // Navigation resets query/highlight but the filter chips ride along — a `-hidden`
      // toggled while browsing keeps applying in the next directory.
      filters: this.filters(),
    });
    this.generation = r.generation;
    this.offset = r.effective_offset;
    this.requestedOffset = r.effective_offset;
    this.selected = 0;
    this.total = 0;
    this.items = [];
    const name = opts.selectName;
    this.pendingSelectMatch = name ? (it) => it.kind === "dir_entry" && it.name === name : null;
    this.input.value = "";
    this.explorerDir = r.directory_path ?? null;
    this.explorerParent = r.directory_parent ?? null;
    this.renderList();
  }

  private onQueryInput(): void {
    this.generation += 1;
    this.offset = 0;
    this.selected = 0;
    this.requestedOffset = 0;
    this.items = [];
    this.listEl.scrollTop = 0; // a new query starts at the top of the (now-empty) result list
    void this.client
      .rpc<null>("picker/query", {
        kind: this.kind,
        query: this.input.value,
        generation: this.generation,
        filters: this.filters(),
      })
      .catch(() => {});
    this.renderList(); // immediate feedback (clears list + shows the create row as you type)
  }

  private onKey(e: KeyboardEvent): void {
    // A selected chip captures the editing keys (Enter edits, Backspace/Delete removes,
    // arrows walk the row, Escape deselects, typing deselects back into the query). Anything
    // *else* — the filter chords (Alt-d, Alt-i, …), Alt-j/k result moves, paging — falls
    // through to the normal picker vocabulary, so a selected chip doesn't disable it.
    if (this.chipSelected !== null) {
      const chips = this.chips();
      if (chips.length === 0) {
        this.chipSelected = null;
      } else {
        const sel = Math.min(this.chipSelected, chips.length - 1);
        let handled = true;
        if (e.key === "ArrowLeft" && !e.altKey) {
          e.preventDefault();
          this.chipSelected = Math.max(0, sel - 1);
          this.renderChips();
        } else if (e.key === "ArrowRight" && !e.altKey) {
          e.preventDefault();
          if (sel + 1 >= chips.length) {
            this.chipSelected = null;
            this.input.setSelectionRange(0, 0);
          } else {
            this.chipSelected = sel + 1;
          }
          this.renderChips();
        } else if (e.key === "Escape") {
          e.preventDefault();
          this.chipSelected = null;
          this.renderChips();
        } else if (e.key === "Backspace" || e.key === "Delete") {
          e.preventDefault();
          this.removeChip(chips[sel].id);
          const remaining = chips.length - 1;
          this.chipSelected = remaining > 0 ? Math.min(sel, remaining - 1) : null;
          void this.applyFilterChange();
        } else if (e.key === "Enter") {
          e.preventDefault();
          this.editChip(chips[sel].id);
        } else if (e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey) {
          // Typing returns to the query — don't preventDefault, the char lands at caret 0.
          this.chipSelected = null;
          this.renderChips();
        } else {
          handled = false;
        }
        if (handled) return;
      }
    }
    const caretAtStart = this.input.selectionStart === 0 && this.input.selectionEnd === 0;
    if (e.key === "Escape") {
      e.preventDefault();
      this.onClose();
    } else if (e.key === "Enter") {
      e.preventDefault();
      this.onEnter();
    } else if (
      e.ctrlKey &&
      !e.altKey &&
      (e.key === "g" || e.key === "f") &&
      this.kind === "explorer"
    ) {
      // Switch to Grep / Files scoped to the browsed directory ("grep here"). preventDefault
      // keeps the browser's find / find-again off the screen.
      e.preventDefault();
      this.switchTo(e.key === "g" ? "grep" : "files");
    } else if (e.altKey && e.key === "c") {
      e.preventDefault();
      void this.toggleFilter({ t: "case" });
    } else if (e.altKey && e.key === "w") {
      e.preventDefault();
      void this.toggleFilter({ t: "word" });
    } else if (e.altKey && e.key === "e") {
      e.preventDefault();
      void this.toggleFilter({ t: "lit" });
    } else if (e.altKey && e.key === "i") {
      e.preventDefault();
      void this.toggleFilter({ t: "ignored" });
    } else if (e.altKey && e.key === ".") {
      e.preventDefault();
      void this.toggleFilter({ t: "hidden" });
    } else if (e.altKey && e.key === "m") {
      e.preventDefault();
      void this.toggleFilter({ t: "changed" });
    } else if (e.altKey && e.key === "g") {
      e.preventDefault();
      this.openChipEditor("glob", null);
    } else if (e.altKey && e.key === "d") {
      e.preventDefault();
      this.openChipEditor("dir", null);
    } else if (e.key === "ArrowLeft" && !e.altKey && caretAtStart && this.chips().length > 0) {
      // Step into the chip row (rightmost chip first) — the browser tag-input gesture.
      e.preventDefault();
      this.chipSelected = this.chips().length - 1;
      this.renderChips();
    } else if (e.key === "Backspace" && !e.altKey && !e.ctrlKey && caretAtStart && this.chips().length > 0) {
      // First press selects the rightmost chip; a second deletes it (two-stage, so holding
      // backspace through the query can't silently destroy a carefully typed glob).
      e.preventDefault();
      this.chipSelected = this.chips().length - 1;
      this.renderChips();
    } else if (e.altKey && e.key === "j") {
      e.preventDefault();
      this.move(1);
    } else if (e.altKey && e.key === "k") {
      e.preventDefault();
      this.move(-1);
    } else if (e.altKey && e.key === "l") {
      if (this.kind === "explorer") {
        e.preventDefault();
        void this.enterHighlightedDir();
      } else if (this.kind === "grep") {
        e.preventDefault();
        void this.grepJump("forward");
      }
    } else if (e.altKey && e.key === "h") {
      e.preventDefault();
      if (this.kind === "grep") void this.grepJump("backward");
      else void this.back();
    } else if (e.altKey && e.key === "Backspace") {
      e.preventDefault();
      void this.back();
    } else if (e.key === "Delete" || (e.ctrlKey && e.key === "d")) {
      e.preventDefault();
      void this.stageDelete();
    } else if (e.key === "PageDown") {
      e.preventDefault();
      this.move(PAGE);
    } else if (e.key === "PageUp") {
      e.preventDefault();
      this.move(-PAGE);
    }
  }

  private highlighted(): PickerItem | undefined {
    return this.items[this.selected - this.offset];
  }

  private onEnter(): void {
    // Synthetic "create" row (Explorer / Projects) sits one past the last item.
    if (this.hasCreate() && this.selected === this.total) {
      void this.doCreate();
      return;
    }
    const item = this.highlighted();
    if (!item) return;
    // In the Explorer, Enter on a directory/root navigates into it (like Alt-l); files confirm.
    if (this.kind === "explorer" && ((item.kind === "dir_entry" && item.is_dir) || item.kind === "root")) {
      void this.enterHighlightedDir();
    } else if (item.kind === "lsp_server") {
      // LSP-servers picker isn't a jump target; Enter opens the server's status in a dialog.
      void this.showLspInfo(item);
    } else {
      this.onConfirm(item);
    }
  }

  /** Build the dialog's data from an LSP-server picker item. */
  private lspInfoFromItem(item: Extract<PickerItem, { kind: "lsp_server" }>): LspInfoData {
    return {
      name: item.name,
      language: item.language,
      workspaceRoot: item.workspace_root,
      state: item.status.state,
      message: item.status.state === "crashed" ? item.status.message : null,
      progress: (item.progress ?? []).map(lspProgressLine),
    };
  }

  /** Open the highlighted LSP server's status in a modal; Restart fires lsp/restart_server. The
   *  dialog stays live: `onUpdate` refreshes it as the picker's status/progress pushes arrive. */
  private async showLspInfo(item: PickerItem): Promise<void> {
    if (item.kind !== "lsp_server") return;
    const dialog = lspInfoDialog(this.lspInfoFromItem(item));
    this.lspModal = { update: dialog.update, language: item.language, root: item.workspace_root };
    const choice = await dialog.result;
    this.lspModal = null;
    this.input.focus(); // dialog stole focus
    if (choice === "restart") {
      void this.client.rpc<null>("lsp/restart_server", { language: item.language }).catch(() => {});
    }
  }

  /** Grep file-jump (Alt-l / Alt-h): jump the selection to the next/prev file's first hit. */
  private async grepJump(direction: "forward" | "backward"): Promise<void> {
    const target = await this.client.rpc<PickerItem | null>("picker/grep_file_jump", {
      from_index: this.selected,
      direction,
    });
    if (!target) return;
    // Re-frame the window around the target hit and select it when the listing arrives.
    const r = await this.client.rpc<PickerViewResult>("picker/view", {
      kind: this.kind,
      reset: false,
      offset: 0,
      limit: this.fetchLimit(),
      center_on: target,
    });
    this.offset = r.effective_offset;
    this.requestedOffset = r.effective_offset;
    this.pendingSelectMatch = (it) => sameGrepHit(it, target);
    this.renderList();
  }

  private async enterHighlightedDir(): Promise<void> {
    const item = this.highlighted();
    if (!item) return;
    if (item.kind === "dir_entry" && item.is_dir) {
      await this.viewExplorer({ directory_path: joinPath(this.explorerDir ?? "", item.name) });
    } else if (item.kind === "root") {
      const target = this.projectPaths[item.path_index];
      if (target) await this.viewExplorer({ directory_path: target });
    }
  }

  /** Alt-h / Alt-Backspace: clear filter, else pop the rightmost chip, else step up
   *  (parent → roots mode). Holding it progressively unwinds the whole picker state. */
  private async back(): Promise<void> {
    if (this.input.value !== "") {
      this.input.value = "";
      this.onQueryInput();
      return;
    }
    const chips = this.chips();
    if (chips.length > 0) {
      this.removeChip(chips[chips.length - 1].id);
      await this.applyFilterChange();
      return;
    }
    if (this.kind !== "explorer") return;
    if (this.explorerParent) {
      // Pre-select the directory we're leaving in the parent listing.
      const leaving = this.explorerDir ? basename(this.explorerDir) : undefined;
      await this.viewExplorer({ directory_path: this.explorerParent, selectName: leaving });
    } else if (this.explorerDir && this.projectPaths.length > 1) {
      await this.viewExplorer({ roots: true });
    }
  }

  // ---- filter chips (docs/picker-filters.md) ----

  private chips(): Chip[] {
    return deriveChips(this.chipValues, this.projectPaths);
  }

  /** The wire filter set the active chips fold into — built per send. */
  private filters(): PickerFilters {
    return wireFilters(this.chipValues);
  }

  /** Explorer `Ctrl-g`/`Ctrl-f`: hand the host a seeded filter set for the target picker —
   *  the browsed directory as a dir scope, changed-only copied, and (grep only) the
   *  ignored/hidden visibility *inverted*: the explorer's listing shows those entries unless
   *  hidden, grep's walk excludes them unless included, so flipping the polarity makes the
   *  search see exactly what the listing showed. Files takes only dir + changed-only. */
  private switchTo(kind: "grep" | "files"): void {
    if (this.kind !== "explorer") return;
    const explorer = this.filters();
    const seeded: PickerFilters = {};
    const scope = this.explorerScope();
    if (scope) seeded.directories = [scope];
    if (explorer.changed_only) seeded.changed_only = true;
    if (kind === "grep") {
      if (!explorer.hide_ignored) seeded.include_ignored = true;
      if (!explorer.hide_hidden) seeded.include_hidden = true;
    }
    this.onSwitch(kind, seeded);
  }

  /** The browsed directory as a `ScopedPath` — longest project root that contains it, with
   *  the root itself mapping to an empty relative path (a whole-root scope). Null in Roots
   *  mode or when the dir sits outside every root. */
  private explorerScope(): ScopedPath | null {
    const dir = this.explorerDir;
    if (!dir) return null;
    let best: ScopedPath | null = null;
    let bestLen = -1;
    this.projectPaths.forEach((root, i) => {
      const prefix = root.endsWith("/") ? root : `${root}/`;
      if (dir === root) {
        if (root.length > bestLen) {
          bestLen = root.length;
          best = { path_index: i, relative_path: "" };
        }
      } else if (dir.startsWith(prefix) && root.length > bestLen) {
        bestLen = root.length;
        best = { path_index: i, relative_path: dir.slice(prefix.length) };
      }
    });
    return best;
  }

  /** Rebuild the chip row DOM. Cheap (a handful of spans), so it runs wholesale on any filter
   *  or selection change. Clicking a chip selects it (focus stays on the input — selection is
   *  virtual, exactly like the keyboard path). */
  private renderChips(): void {
    this.chipsEl.textContent = "";
    const chips = this.chips();
    if (this.chipSelected !== null && this.chipSelected >= chips.length) {
      this.chipSelected = chips.length > 0 ? chips.length - 1 : null;
    }
    chips.forEach((c, i) => {
      const el = document.createElement("span");
      let cls = "picker-chip";
      if (c.label.startsWith("!")) cls += " exclude";
      if (i === this.chipSelected) cls += " selected";
      if (c.id.t === "word") cls += " flag";
      el.className = cls;
      el.textContent = c.label;
      el.addEventListener("mousedown", (ev) => {
        ev.preventDefault(); // keep focus on the input
        this.chipSelected = i;
        this.renderChips();
      });
      this.chipsEl.append(el);
    });
  }

  /** Reset the filter a chip stands for to its default (the chip disappears on re-derive). */
  private removeChip(id: ChipId): void {
    if (id.t === "dir" || id.t === "glob") {
      if (id.i < this.chipValues.length) this.chipValues.splice(id.i, 1);
    } else {
      this.chipValues = this.chipValues.filter((v) => v.t !== id.t);
    }
  }

  /** Toggle/cycle the filter a chord (or Enter on a selected chip) names, then push the change.
   *  `case` cycles smart → sensitive → insensitive. Ignored/hidden map per kind: include for
   *  grep, hide for the explorer. A chord that doesn't apply to this picker kind is a no-op. */
  /** Toggle/cycle a flag chip: booleans flip (appearing appends, disappearing drops out);
   *  `case` cycles smart → sensitive → insensitive → smart *in place* while the chip stays
   *  visible. The ignored/hidden chips record the per-kind direction in the value. */
  private async toggleFilter(id: ChipId): Promise<void> {
    if (!filterApplies(this.kind, id)) return;
    const explorer = this.kind === "explorer";
    if (id.t === "glob" || id.t === "dir") return; // valued chips are edited via their prompts
    if (id.t === "case") {
      const i = this.chipValues.findIndex((v) => v.t === "case");
      if (i < 0) this.chipValues.push({ t: "case", mode: "sensitive" });
      else {
        const v = this.chipValues[i];
        if (v.t === "case" && v.mode === "sensitive") {
          this.chipValues[i] = { t: "case", mode: "insensitive" };
        } else {
          this.chipValues.splice(i, 1);
        }
      }
    } else {
      const value: ChipValue =
        id.t === "ignored" || id.t === "hidden" ? { t: id.t, hide: explorer } : { t: id.t };
      const i = this.chipValues.findIndex((v) => v.t === id.t);
      if (i >= 0) this.chipValues.splice(i, 1);
      else this.chipValues.push(value);
    }
    await this.applyFilterChange();
  }

  /** `Enter` on a selected chip: valued chips re-open their editor pre-filled; everything else
   *  toggles/cycles in place (a plain boolean's chip disappears). */
  private editChip(id: ChipId): void {
    if (id.t === "glob") {
      this.openChipEditor("glob", id.i);
    } else if (id.t === "dir") {
      this.openChipEditor("dir", id.i);
    } else {
      void this.toggleFilter(id);
    }
  }

  /** Push a filter change. For grep/files a filter change is a query change (same generation
   *  mechanics); for the explorer the filters apply when the listing is built, so re-view the
   *  current directory with the replacement set (the query survives the re-rank). */
  private async applyFilterChange(): Promise<void> {
    this.renderChips();
    if (this.kind === "grep" || this.kind === "files") {
      this.onQueryInput();
      return;
    }
    if (this.kind !== "explorer" || !this.explorerDir) return; // roots mode: nothing to filter
    const r = await this.client.rpc<PickerViewResult>("picker/view", {
      kind: "explorer",
      reset: false,
      offset: 0,
      limit: this.fetchLimit(),
      filters: this.filters(),
    });
    this.offset = r.effective_offset;
    this.requestedOffset = r.effective_offset;
    this.selected = 0;
    this.total = 0;
    this.items = [];
    this.listEl.scrollTop = 0;
    this.renderList();
  }

  /** Open the chip editor line below the input (chips + query stay visible). The dir editor
   *  reads as a single `dir:` field — in multi-root projects a root typeahead segment (type a
   *  prefix, Alt-j/k cycle the matching disambiguated root labels), a `:` separator, then the
   *  unlabelled root-relative path; single-root projects show the path alone. The path segment
   *  carries directory-only ghost suggestions in the save-as idiom (Alt-j/k cycle, Tab / Alt-l
   *  accept a segment, Alt-Backspace pops one). Enter commits from any field; Escape cancels.
   *  Globs are one plain field using rg `-g` syntax (`!` excludes, `src/**` scopes to a tree). */
  private openChipEditor(kind: "glob" | "dir", edit: number | null): void {
    const probe: ChipId = kind === "glob" ? { t: "glob", i: 0 } : { t: "dir", i: 0 };
    if (!filterApplies(this.kind, probe)) return;
    this.chipSelected = null;
    this.renderChips();
    const editedChip = edit !== null ? this.chipValues[edit] : undefined;
    const current = kind === "dir" && editedChip?.t === "dir" ? editedChip.d : null;
    const rootIndex = current?.path_index ?? 0;
    this.chipEditor = {
      kind,
      edit,
      // Fresh multi-root dir scopes compose left-to-right: start on the root segment.
      // Editing an existing chip (or any glob/single-root open) starts on the path.
      field:
        kind === "dir" && this.projectPaths.length > 1 && current === null ? "root" : "path",
      rootSelected: rootIndex,
      rootIndex,
      listing: [],
      listingDirAbs: "",
      listingState: "pending",
      suggestionIdx: 0,
    };

    this.editorRow.textContent = "";
    this.editorRootInput = null;
    this.editorGhostEl = null;
    this.editorPathGhostEl = null;
    this.editorSepEl = null;
    this.editorRootSpan = null;
    this.editorPathSpan = null;
    // The two inputs are created once per open and persist across field switches (their
    // `.value` is the editor's source of truth); renderDirEditor attaches only the *focused*
    // one to the DOM — the unfocused segment renders as a plain span, like the TUI.
    const multiRoot = kind === "dir" && this.projectPaths.length > 1;
    if (multiRoot) {
      this.editorRootInput = document.createElement("input");
      this.editorRootInput.className = "picker-editor-input picker-editor-root";
      this.editorRootInput.spellcheck = false;
      this.editorRootInput.autocomplete = "off";
      this.editorRootInput.addEventListener("keydown", (e) => this.onEditorKey(e));
      this.editorRootInput.addEventListener("input", () => {
        // The match set changed under the highlight — snap back to the best match, and
        // re-sync the path listing: the chosen root may have moved under existing path text.
        if (this.chipEditor) this.chipEditor.rootSelected = 0;
        this.renderRootGhost();
        this.syncEditorListing();
        this.updateEditorDecorations();
      });
    }
    this.editorPathInput = document.createElement("input");
    this.editorPathInput.className = "picker-editor-input";
    this.editorPathInput.spellcheck = false;
    this.editorPathInput.autocomplete = "off";
    this.editorPathInput.placeholder = kind === "glob" ? "*.rs · !*_test.rs · src/**" : "";
    this.editorPathInput.value =
      kind === "glob"
        ? editedChip?.t === "glob"
          ? editedChip.g
          : ""
        : current?.relative_path ?? "";
    this.editorPathInput.addEventListener("keydown", (e) => this.onEditorKey(e));
    if (kind === "dir") {
      this.editorPathInput.classList.add("picker-editor-root");
      this.editorPathInput.addEventListener("input", () => this.onPathInput());
      this.renderDirEditor();
      this.syncEditorListing();
      this.updateEditorDecorations();
    } else {
      const label = document.createElement("span");
      label.className = "picker-editor-label";
      label.textContent = "glob:";
      this.editorRow.append(label, this.editorPathInput);
    }
    this.editorRow.style.display = "flex";
    (this.chipEditor.field === "root" ? this.editorRootInput! : this.editorPathInput).focus();
  }

  /** (Re)build the dir editor row for the current focused field: `dir:` label, root segment,
   *  `:` separator, path segment. Only the focused segment is an `<input>` (with its ghost
   *  overlay); the other renders as a plain span — clicking it moves focus there. Called on
   *  open and on every field switch, never per keystroke (rebuilding under a live input would
   *  drop its caret). */
  private renderDirEditor(): void {
    const ed = this.chipEditor;
    if (!ed || ed.kind !== "dir" || !this.editorPathInput) return;
    const multiRoot = this.projectPaths.length > 1;
    this.editorRow.textContent = "";
    this.editorGhostEl = null;
    this.editorPathGhostEl = null;
    this.editorSepEl = null;
    this.editorRootSpan = null;
    this.editorPathSpan = null;
    const label = document.createElement("span");
    label.className = "picker-editor-label";
    label.textContent = "dir:";
    this.editorRow.append(label);
    if (multiRoot && this.editorRootInput) {
      if (ed.field === "root") {
        // The focused root segment: a transparent <input> stacked over a ghost layer that
        // re-renders the typed prefix invisibly (reserving its exact glyph metrics) followed
        // by the gray remainder of the current typeahead match. The `hug` variant makes the
        // ghost the in-flow sizer, so the segment is exactly as wide as its content.
        const wrap = document.createElement("span");
        wrap.className = "picker-editor-rootwrap hug";
        this.editorGhostEl = document.createElement("span");
        this.editorGhostEl.className = "picker-editor-ghost";
        wrap.append(this.editorGhostEl, this.editorRootInput);
        this.editorRow.append(wrap);
        this.renderRootGhost();
      } else {
        // Unfocused root: the chosen label in committed-prefix blue — or the raw filter text,
        // red, when it matches nothing (never a fallback label the commit gate would refuse).
        const labels = rootLabels(this.projectPaths);
        const invalid = rootCandidates(labels, this.editorRootInput.value).length === 0;
        const span = document.createElement("span");
        span.className = invalid ? "picker-editor-seg invalid" : "picker-editor-seg root";
        span.textContent = invalid
          ? this.editorRootInput.value
          : labels[this.editorChosenRoot()] ?? "";
        span.addEventListener("mousedown", (e) => {
          e.preventDefault();
          this.setEditorField("root");
        });
        this.editorRootSpan = span;
        this.editorRow.append(span);
      }
      this.editorSepEl = document.createElement("span");
      this.editorSepEl.className = "picker-editor-sep";
      this.editorSepEl.textContent = ":";
      this.editorRow.append(this.editorSepEl);
    }
    if (ed.field === "path" || !multiRoot) {
      // The focused path segment: same ghost layering, stretched over the rest of the row, so
      // directory suggestions render gray after the typed text.
      const wrap = document.createElement("span");
      wrap.className = "picker-editor-rootwrap";
      this.editorPathGhostEl = document.createElement("span");
      this.editorPathGhostEl.className = "picker-editor-ghost";
      wrap.append(this.editorPathGhostEl, this.editorPathInput);
      this.editorRow.append(wrap);
      this.renderPathGhost();
    } else {
      // Unfocused path: plain text (red when invalid) — no suggestion until it's focused.
      const span = document.createElement("span");
      span.className = "picker-editor-seg";
      span.textContent = this.editorPathInput.value;
      span.addEventListener("mousedown", (e) => {
        e.preventDefault();
        this.setEditorField("path");
      });
      this.editorPathSpan = span;
      this.editorRow.append(span);
    }
  }

  /** Move focus between the dir editor's segments, swapping which one renders as an input.
   *  The caret parks at the end of the newly focused input (a reattached input forgets its
   *  selection anyway). */
  private setEditorField(field: "root" | "path"): void {
    const ed = this.chipEditor;
    if (!ed || ed.kind !== "dir") return;
    if (field === "root" && !this.editorRootInput) return;
    // The path can't be entered under an invalid root (Tab, `:`, or a click on the path
    // segment) — focus stays pinned to the red root until it matches something.
    if (field === "path" && this.editorRootInvalid()) return;
    ed.field = field;
    this.renderDirEditor();
    this.updateEditorDecorations();
    const input = field === "root" ? this.editorRootInput! : this.editorPathInput!;
    input.focus();
    input.setSelectionRange(input.value.length, input.value.length);
  }

  /** Keys inside the editor line. The dir editor reads as one `dir: root: path` field: Tab /
   *  Alt-l accept the focused segment's ghost (root — adopting it and moving into the path;
   *  path — absorbing the next directory segment), `:` on a completed root value moves into
   *  the path, Alt-j/k cycle the focused segment's matches, Alt-Backspace pops a path segment
   *  (then, at an empty path, clears the root selection), and plain Backspace at an empty path
   *  steps back into the root. Enter commits, Escape cancels. */
  private onEditorKey(e: KeyboardEvent): void {
    const ed = this.chipEditor;
    if (!ed) return;
    // Only the focused segment renders an input, so the field state *is* the focus state.
    const inRoot = ed.kind === "dir" && ed.field === "root" && this.editorRootInput !== null;
    const inPath = !inRoot;
    if (e.key === "Enter") {
      // A dir editor only commits a *valid* scope — a root matching some label and a path
      // that exists (or is empty). Otherwise the editor stays open, its invalid segment red.
      e.preventDefault();
      if (ed.kind === "dir" && !this.editorScopeValid()) return;
      this.closeChipEditor(true);
    } else if (e.key === "Escape") {
      e.preventDefault();
      this.closeChipEditor(false);
    } else if (e.key === "Tab" || (e.altKey && e.key === "l")) {
      // Accept the focused segment's suggestion. Root: adopt the ghost completion (save-as
      // muscle memory) and continue right into the path — you've picked the root, keep
      // composing. Path: absorb the ghost directory segment; the next segment's suggestions
      // are fetched so repeated presses walk down the tree. Swallowed in the glob editor (Tab
      // must not move browser focus out of the line).
      e.preventDefault();
      if (ed.kind === "dir" && inRoot) this.commitRootField();
      else if (ed.kind === "dir" && inPath) this.acceptPathSuggestion();
    } else if (e.key === ":" && !e.altKey && !e.ctrlKey && inRoot) {
      // `:` on a completed root value confirms it and moves into the path — the editor reads
      // as a single `dir: root: path` field, and `:` is the separator you'd type next. On an
      // incomplete value it's swallowed (`:` can never extend a root-label prefix match).
      e.preventDefault();
      if (this.editorRootComplete()) this.commitRootField();
    } else if (e.altKey && e.key === "h") {
      e.preventDefault();
      this.setEditorField("root");
    } else if (e.altKey && e.key === "Backspace") {
      e.preventDefault();
      if (ed.kind === "dir" && inPath && this.editorPathInput) {
        // Path: delete the rightmost segment, fish-style (the save-as gesture); at an empty
        // path, clear the root selection — the next rung of the progressive unwind.
        if (this.editorPathInput.value === "") {
          if (this.editorRootInput) {
            this.editorRootInput.value = "";
            ed.rootSelected = 0;
            this.setEditorField("root");
            this.syncEditorListing();
          }
        } else {
          this.popPathSegment();
        }
        this.updateEditorDecorations();
      } else if (inRoot && this.editorRootInput) {
        this.editorRootInput.value = "";
        ed.rootSelected = 0;
        this.renderRootGhost();
        this.syncEditorListing();
        this.updateEditorDecorations();
      } else if (this.editorPathInput) {
        this.editorPathInput.value = "";
      }
    } else if (
      e.key === "Backspace" &&
      !e.ctrlKey &&
      this.editorRootInput &&
      inPath &&
      this.editorPathInput?.value === ""
    ) {
      // Backspace at an empty path steps back into the root field — the same leftward gesture
      // the chip row uses from the query.
      e.preventDefault();
      this.setEditorField("root");
    } else if (e.altKey && (e.key === "j" || e.key === "k")) {
      e.preventDefault();
      const down = e.key === "j";
      if (inRoot && this.editorRootInput) {
        const labels = rootLabels(this.projectPaths);
        const n = rootCandidates(labels, this.editorRootInput.value).length;
        if (n > 0) {
          const sel = Math.min(ed.rootSelected, n - 1);
          ed.rootSelected = down ? (sel + 1) % n : (sel + n - 1) % n;
          this.renderRootGhost();
          // The chosen root moved — any path text now resolves (and validates) under it.
          this.syncEditorListing();
          this.updateEditorDecorations();
        }
      } else if (ed.kind === "dir" && inPath) {
        // Path suggestions clamp at both ends, like the save-as prompt.
        const s = this.pathSuggestionState();
        if (s && s.matches.length > 0) {
          const sel = Math.min(ed.suggestionIdx, s.matches.length - 1);
          ed.suggestionIdx = down ? Math.min(sel + 1, s.matches.length - 1) : Math.max(sel - 1, 0);
          this.renderPathGhost();
        }
      }
    } else if (e.altKey) {
      e.preventDefault(); // swallow other chords — they're picker-level, not editor-level
    }
  }

  /** True when the root filter prefix-matches no label (an empty filter matches every root).
   *  Always false in single-root projects (no root segment to invalidate). */
  private editorRootInvalid(): boolean {
    if (!this.editorRootInput) return false;
    return rootCandidates(rootLabels(this.projectPaths), this.editorRootInput.value).length === 0;
  }

  /** True when the dir editor's scope is committable: a root that prefix-matches some label
   *  and a path that exists or is empty. */
  private editorScopeValid(): boolean {
    return !this.editorRootInvalid() && this.editorPathValid();
  }

  /** True when the path field holds a committable value: empty (whole-root scope / clear), or
   *  a path whose dir portion is vouched for by a loaded listing and whose leaf is either
   *  empty (trailing `/`) or prefixes at least one listed subdirectory — a partial leaf
   *  commits as its highlighted completion (see editorCommittedPath). A pending listing can't
   *  vouch, so a commit racing the fetch waits. */
  private editorPathValid(): boolean {
    const ed = this.chipEditor;
    if (!ed || ed.kind !== "dir" || !this.editorPathInput) return true;
    const value = this.editorPathInput.value;
    if (value === "") return true;
    if (ed.listingState !== "loaded") return false;
    const slash = value.lastIndexOf("/");
    const leaf = slash >= 0 ? value.slice(slash + 1) : value;
    return leaf === "" || prefixMatchIndices(ed.listing, leaf).length > 0;
  }

  /** The path a commit should adopt: the typed text, with a partially typed leaf completed to
   *  the highlighted suggestion — Enter on a prefix selects the completion, mirroring the root
   *  segment, and the ghost shows exactly what will commit. The text comes back as typed when
   *  the leaf is empty, nothing matches, or the listing can't vouch. */
  private editorCommittedPath(): string {
    const ed = this.chipEditor;
    if (!ed || ed.kind !== "dir" || !this.editorPathInput) return "";
    const value = this.editorPathInput.value;
    if (ed.listingState !== "loaded") return value;
    const slash = value.lastIndexOf("/");
    const dir = slash >= 0 ? value.slice(0, slash + 1) : "";
    const leaf = slash >= 0 ? value.slice(slash + 1) : value;
    if (leaf === "") return value;
    const matches = prefixMatchIndices(ed.listing, leaf);
    if (matches.length === 0) return value;
    const idx = matches[Math.min(ed.suggestionIdx, matches.length - 1)];
    return dir + ed.listing[idx];
  }

  /** True when the path is *definitely* wrong — the red-worthy condition, looser than
   *  `!editorPathValid()`: the dir portion failed to list, or the loaded listing holds no
   *  directory the leaf even prefixes. A leaf prefixing an existing directory isn't flagged
   *  (mid-segment, ghost visible) though the commit still requires an exact match; a pending
   *  listing isn't flagged either (unknown ≠ invalid). */
  private editorPathInvalid(): boolean {
    const ed = this.chipEditor;
    if (!ed || ed.kind !== "dir" || !this.editorPathInput) return false;
    const value = this.editorPathInput.value;
    if (value === "") return false;
    if (ed.listingState === "pending") return false;
    if (ed.listingState === "failed") return true;
    const slash = value.lastIndexOf("/");
    const leaf = slash >= 0 ? value.slice(slash + 1) : value;
    return leaf !== "" && prefixMatchIndices(ed.listing, leaf).length === 0;
  }

  /** Reflect validity and separator visibility into the DOM: invalid segments (root matching no
   *  label / path that doesn't exist) colour red — the visible form of "Enter will refuse this"
   *  — and the `:` separator appears once the path is in play (focused, or already holding
   *  text), so a fresh root prompt doesn't dangle a `:` off an unentered field. */
  private updateEditorDecorations(): void {
    const ed = this.chipEditor;
    if (!ed || ed.kind !== "dir") return;
    const pathText = this.editorPathInput?.value ?? "";
    if (this.editorSepEl) {
      this.editorSepEl.style.display = ed.field === "path" || pathText !== "" ? "" : "none";
    }
    if (this.editorRootInput) {
      const labels = rootLabels(this.projectPaths);
      const invalid = rootCandidates(labels, this.editorRootInput.value).length === 0;
      // Whichever element currently stands for the segment (live input or static span).
      (ed.field === "root" ? this.editorRootInput : this.editorRootSpan)?.classList.toggle(
        "invalid",
        invalid,
      );
    }
    (ed.field === "path" ? this.editorPathInput : this.editorPathSpan)?.classList.toggle(
      "invalid",
      this.editorPathInvalid(),
    );
  }

  /** True when the root field holds a complete root label (the current match's ghost suffix is
   *  empty) — what lets a typed `:` act as the root/path separator. */
  private editorRootComplete(): boolean {
    const ed = this.chipEditor;
    if (!ed || !this.editorRootInput) return false;
    const typed = this.editorRootInput.value;
    const labels = rootLabels(this.projectPaths);
    const candidates = rootCandidates(labels, typed);
    if (candidates.length === 0) return false;
    const idx = candidates[Math.min(ed.rootSelected, candidates.length - 1)];
    return labels[idx].length === typed.length;
  }

  /** Confirm the root segment (adopting the ghost completion, when one is visible) and move
   *  focus into the path. Shared by Tab, Alt-l, and `:`-on-a-complete-value. */
  private commitRootField(): void {
    const ed = this.chipEditor;
    if (!ed || !this.editorRootInput || !this.editorPathInput) return;
    const labels = rootLabels(this.projectPaths);
    const candidates = rootCandidates(labels, this.editorRootInput.value);
    // An invalid root refuses to advance — focus stays on the red root field.
    if (candidates.length === 0) return;
    const idx = candidates[Math.min(ed.rootSelected, candidates.length - 1)];
    this.editorRootInput.value = labels[idx];
    // The full label may still prefix-match several roots ("beta" vs "beta-api") — keep the
    // highlight on the adopted one.
    const after = rootCandidates(labels, labels[idx]);
    ed.rootSelected = Math.max(0, after.indexOf(idx));
    this.setEditorField("path");
    // The chosen root may have moved the dir the path resolves under.
    this.syncEditorListing();
    this.renderPathGhost();
    this.updateEditorDecorations();
  }

  /** A free-form edit to the path field: reset the suggestion highlight and resync the listing
   *  (the dir portion may have moved — typed `/`, backspaced into a previous segment). */
  private onPathInput(): void {
    const ed = this.chipEditor;
    if (!ed || ed.kind !== "dir") return;
    ed.suggestionIdx = 0;
    this.syncEditorListing();
    this.renderPathGhost();
    this.updateEditorDecorations();
  }

  /** The absolute directory the path field's suggestions should list: the dir portion of the
   *  typed path (up to the last `/`), resolved under the chosen root. */
  private editorListingPath(): string | null {
    const ed = this.chipEditor;
    if (!ed || ed.kind !== "dir" || !this.editorPathInput) return null;
    // No listing target under an invalid root — suggestions fetched beneath the fallback root
    // would read as silently defaulting to it.
    if (this.editorRootInvalid()) return null;
    const rootIdx = this.projectPaths.length > 1 ? this.editorChosenRoot() : 0;
    const root = this.projectPaths[rootIdx];
    if (root === undefined) return null;
    const value = this.editorPathInput.value;
    const slash = value.lastIndexOf("/");
    const dirPart = slash >= 0 ? value.slice(0, slash).replace(/^\/+|\/+$/g, "") : "";
    return dirPart === "" ? root : joinPath(root, dirPart);
  }

  /** Reconcile the listing key with the current (root, dir-portion) pair, refetching when they
   *  diverged. The stale listing is cleared immediately so a ghost from the old directory can't
   *  flash while the fetch is in flight. */
  private syncEditorListing(): void {
    const ed = this.chipEditor;
    const abs = this.editorListingPath();
    if (!ed || abs === null || abs === ed.listingDirAbs) return;
    ed.listingDirAbs = abs;
    ed.listing = [];
    ed.listingState = "pending";
    ed.suggestionIdx = 0;
    void this.refreshEditorListing(abs);
  }

  /** Fire `directory/list` and stash the response (subdirectories only — a file never completes
   *  a directory scope). A failure marks the dir portion nonexistent: the path renders invalid
   *  and the commit gate refuses it until the next change re-syncs. */
  private async refreshEditorListing(abs: string): Promise<void> {
    const ed = this.chipEditor;
    if (!ed) return;
    try {
      const r = await this.client.rpc<DirectoryListResult>("directory/list", { path: abs });
      if (this.chipEditor === ed && ed.listingDirAbs === abs) {
        ed.listing = r.entries.filter((en) => en.is_dir).map((en) => en.name);
        ed.listingState = "loaded";
        ed.suggestionIdx = 0;
        this.renderPathGhost();
        this.updateEditorDecorations();
      }
    } catch {
      // Typed-but-nonexistent segment, or a path outside the boundary.
      if (this.chipEditor === ed && ed.listingDirAbs === abs) {
        ed.listingState = "failed";
        this.updateEditorDecorations();
      }
    }
  }

  /** The path field's partial leaf (text after the last `/`) and the listing indices matching
   *  it as a smartcase prefix. */
  private pathSuggestionState(): { partial: string; matches: number[] } | null {
    const ed = this.chipEditor;
    if (!ed || ed.kind !== "dir" || !this.editorPathInput) return null;
    const value = this.editorPathInput.value;
    const slash = value.lastIndexOf("/");
    const partial = slash >= 0 ? value.slice(slash + 1) : value;
    return { partial, matches: prefixMatchIndices(ed.listing, partial) };
  }

  /** Re-render the path field's ghost layer: the typed text invisibly (reserving its glyph
   *  metrics), then the rest of the current directory match plus its trailing `/` in gray.
   *  Nothing matches → no ghost (unlike the root field there's no fallback to cue). */
  private renderPathGhost(): void {
    const ed = this.chipEditor;
    if (!ed || ed.kind !== "dir" || !this.editorPathGhostEl || !this.editorPathInput) return;
    this.editorPathGhostEl.textContent = "";
    const prefix = document.createElement("span");
    prefix.className = "typed";
    prefix.textContent = this.editorPathInput.value;
    const suffix = document.createElement("span");
    const s = this.pathSuggestionState();
    if (s && s.matches.length > 0) {
      const idx = s.matches[Math.min(ed.suggestionIdx, s.matches.length - 1)];
      suffix.textContent = `${ed.listing[idx].slice(s.partial.length)}/`;
    }
    this.editorPathGhostEl.append(prefix, suffix);
  }

  /** Tab / Alt-l in the path field: absorb the ghost into the input. The suffix always ends in
   *  `/` (suggestions are directories), so the dir portion grew — refetch so the next segment's
   *  suggestion appears. No-op without a ghost. */
  private acceptPathSuggestion(): void {
    const ed = this.chipEditor;
    const s = this.pathSuggestionState();
    if (!ed || !s || s.matches.length === 0 || !this.editorPathInput) return;
    const idx = s.matches[Math.min(ed.suggestionIdx, s.matches.length - 1)];
    this.editorPathInput.value += `${ed.listing[idx].slice(s.partial.length)}/`;
    ed.suggestionIdx = 0;
    this.syncEditorListing();
    this.renderPathGhost();
    this.updateEditorDecorations();
  }

  /** Alt-Backspace in a non-empty path field: drop the rightmost segment, fish-style (the
   *  save-as gesture), keeping the parent's trailing `/`. */
  private popPathSegment(): void {
    const ed = this.chipEditor;
    if (!ed || !this.editorPathInput) return;
    const v = this.editorPathInput.value;
    const stripped = v.endsWith("/") ? v.slice(0, -1) : v;
    const i = stripped.lastIndexOf("/");
    this.editorPathInput.value = i >= 0 ? stripped.slice(0, i + 1) : "";
    ed.suggestionIdx = 0;
    this.syncEditorListing();
    this.renderPathGhost();
    this.updateEditorDecorations();
  }

  /** Re-render the root field's ghost layer: an invisible copy of the typed prefix (reserving
   *  its exact glyph metrics) followed by the current match's remainder in gray, sitting under
   *  the transparent input. No match → no ghost; the typed text colouring red (see
   *  updateEditorDecorations) is the cue. */
  private renderRootGhost(): void {
    const ed = this.chipEditor;
    if (!ed || !this.editorGhostEl || !this.editorRootInput) return;
    const typed = this.editorRootInput.value;
    const labels = rootLabels(this.projectPaths);
    const candidates = rootCandidates(labels, typed);
    this.editorGhostEl.textContent = "";
    const prefix = document.createElement("span");
    prefix.className = "typed";
    prefix.textContent = typed;
    const suffix = document.createElement("span");
    if (candidates.length > 0) {
      const idx = candidates[Math.min(ed.rootSelected, candidates.length - 1)];
      suffix.textContent = labels[idx].slice(typed.length);
    }
    this.editorGhostEl.append(prefix, suffix);
  }

  /** The root the editor would commit: the highlighted typeahead candidate, falling back to
   *  the root the editor opened with when the filter matches nothing. */
  private editorChosenRoot(): number {
    const ed = this.chipEditor;
    if (!ed) return 0;
    if (!this.editorRootInput) return ed.rootIndex;
    const labels = rootLabels(this.projectPaths);
    const candidates = rootCandidates(labels, this.editorRootInput.value);
    if (candidates.length === 0) return ed.rootIndex;
    return candidates[Math.min(ed.rootSelected, candidates.length - 1)];
  }

  /** Close the editor line, committing when asked. Glob: an empty value clears the glob being
   *  edited (or cancels when adding). Dir: chosen root + trimmed path compose the ScopedPath;
   *  an empty path is a whole-root scope in multi-root projects, and clears the chip in
   *  single-root ones. Focus returns to the query input. */
  private closeChipEditor(commit: boolean): void {
    const ed = this.chipEditor;
    if (!ed) return;
    // A partially typed dir leaf commits as its highlighted completion (Enter on a prefix
    // selects the completion, like the root segment).
    const raw = ed.kind === "dir" ? this.editorCommittedPath() : this.editorPathInput?.value ?? "";
    const text = raw.trim().replace(/^\/+|\/+$/g, "");
    let changed = false;
    if (commit) {
      // Both editors commit through the same shape (mirrors the TUI's commit_*_edit): a null
      // value clears the chip being edited (or cancels when adding), duplicates collapse —
      // committing an existing value is a no-op, editing into one drops the edited entry —
      // and an in-place edit keeps its position in the row. `ed.edit` indexes the chip list.
      let value: ChipValue | null;
      if (ed.kind === "glob") {
        // Normalization: `.rs` → `*.rs`; degenerate match-everything globs (`*`, `**`,
        // negated too) come back null and behave like an empty commit.
        const glob = normalizeGlob(this.editorPathInput?.value ?? "");
        value = glob === null ? null : { t: "glob", g: glob };
      } else {
        // An empty path is a whole-root scope in multi-root projects, and clears the chip in
        // single-root ones.
        const multiRoot = this.projectPaths.length > 1;
        value =
          text === "" && !multiRoot
            ? null
            : {
                t: "dir",
                d: { path_index: multiRoot ? this.editorChosenRoot() : 0, relative_path: text },
              };
      }
      const edit = ed.edit !== null && this.chipValues[ed.edit]?.t === ed.kind ? ed.edit : null;
      if (value === null) {
        if (edit !== null) {
          this.chipValues.splice(edit, 1);
          changed = true;
        }
      } else if (edit !== null) {
        const v = value;
        if (this.chipValues.some((c, j) => j !== edit && sameChip(c, v))) {
          this.chipValues.splice(edit, 1);
        } else {
          this.chipValues[edit] = v;
        }
        changed = true;
      } else {
        const v = value;
        if (!this.chipValues.some((c) => sameChip(c, v))) {
          this.chipValues.push(v);
          changed = true;
        }
      }
    }
    this.chipEditor = null;
    this.editorRow.style.display = "none";
    this.editorRow.textContent = "";
    this.editorRootInput = null;
    this.editorPathInput = null;
    this.editorGhostEl = null;
    this.editorPathGhostEl = null;
    this.editorSepEl = null;
    this.editorRootSpan = null;
    this.editorPathSpan = null;
    this.input.focus();
    if (changed) void this.applyFilterChange();
    else this.renderChips();
  }

  /** Delete the highlighted file/dir/project after a confirm dialog, then refresh the list. */
  private async stageDelete(): Promise<void> {
    const item = this.highlighted();
    if (!item) return;
    let rpc: string;
    let params: { path: string } | { name: string };
    let label: string;
    if (item.kind === "file") {
      const root = this.projectPaths[item.path_index];
      if (!root) return;
      rpc = "path/delete";
      params = { path: joinPath(root, item.relative_path) };
      label = item.relative_path;
    } else if (item.kind === "dir_entry") {
      if (!this.explorerDir) return;
      rpc = "path/delete";
      params = { path: joinPath(this.explorerDir, item.name) };
      label = item.name;
    } else if (item.kind === "project") {
      rpc = "project/delete";
      params = { name: item.name };
      label = item.name;
    } else {
      return; // roots / grep hits / buffers / lsp servers aren't deletable here
    }
    const ok = await confirmDialog(`Delete ${label}?`, { danger: true });
    this.input.focus(); // confirm dialog stole focus
    if (!ok) return;
    try {
      await this.client.rpc<null>(rpc, params);
      this.onToast(`deleted ${label}`, "success");
      this.refresh();
    } catch (e) {
      this.onToast((e as Error).message, "error");
    }
  }

  /** Re-list after a deletion: re-run the directory listing or candidate walk. */
  private refresh(): void {
    if (this.kind === "explorer") void this.viewExplorer({ directory_path: this.explorerDir ?? undefined });
    else void this.view(true, 0);
  }

  private move(delta: number): void {
    const max = this.total - 1 + (this.hasCreate() ? 1 : 0); // synthetic create row sits at `total`
    if (max < 0) return;
    this.selected = Math.max(0, Math.min(this.selected + delta, max));
    // Scroll to the selection now if it's in the loaded window; otherwise ensureWindow will fetch
    // and we scroll once it lands.
    const inWindow =
      (this.selected >= this.offset && this.selected < this.offset + this.items.length) ||
      (this.hasCreate() && this.selected === this.total);
    if (!inWindow) this.scrollToSelOnUpdate = true;
    this.ensureWindow();
    this.renderList();
    if (inWindow) this.scrollSelectionIntoView();
  }

  private scrollSelectionIntoView(block: ScrollLogicalPosition = "nearest"): void {
    this.selectedRowEl?.scrollIntoView({ block });
  }

  /** Items per fetched window: at least LIMIT, and at least three viewports' worth of rows, so
   *  the loaded window always extends well past the visible range plus the fetch margins. (A
   *  fixed 64 became too tight once grep rows got denser — a tall picker shows ~64 rows, making
   *  the window ≈ the viewport, which kept both fetch margins permanently armed and refetched on
   *  every wheel tick.) */
  private fetchLimit(): number {
    const visible = Math.ceil(this.listEl.clientHeight / this.rowH);
    return Math.max(LIMIT, visible * 3);
  }

  /** If the selection has left the loaded window, fetch a new window centred on it. */
  private ensureWindow(): void {
    const loaded = this.selected >= this.offset && this.selected < this.offset + this.items.length;
    if (loaded) return;
    const limit = this.fetchLimit();
    const maxOffset = Math.max(0, this.total - limit);
    const target = Math.max(0, Math.min(this.selected - Math.floor(limit / 2), maxOffset));
    if (target === this.requestedOffset && this.items.length > 0) return;
    void this.view(false, target).catch(() => {});
  }

  private renderList(): void {
    // References opens empty while it resolves (an LSP round-trip), so a blank list would read as a
    // broken picker — show progress while loading and an explicit "none" once it finishes empty.
    if (this.kind === "references" && this.items.length === 0) {
      const msg = document.createElement("div");
      msg.className = "picker-empty";
      msg.textContent = this.ticking ? "Finding references…" : "No references found";
      this.listEl.classList.add("filled"); // the message itself needs the separator
      this.listEl.replaceChildren(msg);
      this.countEl.textContent = "";
      this.updatePath();
      return;
    }
    // Show the input/list separator only when the list will have visible content — without it an
    // empty list reads as a doubled-up input border.
    this.listEl.classList.toggle(
      "filled",
      this.total > 0 || this.items.length > 0 || this.hasCreate(),
    );
    // Virtual scroll: a full-height spacer (total rows) with the loaded window absolutely
    // positioned `offset` rows down, so the native scrollbar spans all results and mouse scrolling
    // reveals unloaded ranges (onListScroll fetches them).
    const win = document.createElement("div");
    win.className = "picker-window";
    const localSel = this.selected - this.offset;
    let selectedRow: HTMLElement | null = null;
    let prevGrepKey: string | null = null;
    let headersInWindow = 0;
    // Grep rows are wrapped per-file in a section so the file header can be `position: sticky` and
    // push the previous one off the top as you scroll. Non-grep rows go straight into the window.
    let section: HTMLElement | null = null;
    this.items.forEach((item, i) => {
      // Grep: a non-selectable file header before the first hit of each file in the window.
      if (item.kind === "grep_hit") {
        const key = `${item.path_index}\0${item.relative_path}`;
        if (key !== prevGrepKey) {
          prevGrepKey = key;
          headersInWindow++;
          section = document.createElement("div");
          section.className = "grep-section";
          section.style.marginTop = `${GREP_GROUP_GAP}px`;
          const h = document.createElement("div");
          h.className = "picker-row grep-header";
          // Multi-root projects prefix the root's name: `root: path`, all in the header colour.
          // The *disambiguated* label, not the raw basename — two roots sharing a basename
          // would otherwise render identical headers (the case rootLabels exists for). The
          // path segment-elides to the row budget so the filename always survives.
          if (this.projectPaths.length > 1) {
            const label = rootLabels(this.projectPaths)[item.path_index] ?? `root ${item.path_index}`;
            const budget = this.listPathBudget([...label].length + 2);
            h.textContent = `${label}: ${truncatePath(item.relative_path, undefined, budget).display}`;
          } else {
            h.textContent = truncatePath(item.relative_path, undefined, this.listPathBudget(0)).display;
          }
          section.append(h);
          win.append(section);
        }
      }
      // File-backed rows (Files / Grep / Buffers / Explorer files / Projects) render as anchors so
      // Ctrl/Cmd/middle-click opens in a new tab natively; a plain click still opens in place (below).
      const href = this.fileUrl(item, this.explorerDir);
      const row: HTMLElement = document.createElement(href ? "a" : "div");
      if (href) (row as HTMLAnchorElement).href = href;
      row.className = i === localSel ? "picker-row selected" : "picker-row";
      // Grep hits are code lines — rendered in the editor's mono font (see .grep-hit).
      if (item.kind === "grep_hit") row.classList.add("grep-hit");
      if (i === localSel) selectedRow = row;
      const { primary, primaryMatches, meta, prefix, prefixClass, dir, bullet, bulletStatus, bulletClass, bulletIcon, dim, dirtyDot, italic, suffix } = this.describe(item);
      if (bullet) {
        // Fixed-width cell (empty when clean/ignored) so entry names stay aligned across rows; the
        // `•` glyph appears only with a colour to show (a git change). LSP rows put the status
        // bar's SVG icon in the cell instead (bulletIcon).
        const cls = bulletClass ?? (bulletStatus ? `picker-bullet-${bulletStatus}` : undefined);
        const b = document.createElement("span");
        b.className = cls ? `picker-bullet ${cls}` : "picker-bullet";
        if (bulletIcon) {
          b.classList.add("icon");
          b.append(bulletIcon);
        } else {
          b.textContent = cls ? "•" : "";
        }
        row.append(b);
      }
      if (prefix) {
        const p = document.createElement("span");
        p.className = prefixClass ? `picker-prefix ${prefixClass}` : "picker-prefix";
        p.textContent = prefix;
        row.append(p);
      }
      const main = document.createElement("span");
      const mainClass = ["picker-main"];
      if (dim) mainClass.push("picker-dim");
      else if (dir) mainClass.push("picker-dir");
      if (italic) mainClass.push("picker-italic");
      main.className = mainClass.join(" ");
      main.append(matched(primary, primaryMatches));
      row.append(main);
      if (suffix) {
        const s = document.createElement("span");
        s.className = "picker-suffix";
        s.textContent = suffix;
        row.append(s);
      }
      if (meta) {
        const m = document.createElement("span");
        m.className = "picker-meta";
        m.textContent = meta;
        row.append(m);
      }
      if (dirtyDot && dirtyDot !== "clean") {
        const d = bufferStatusDot();
        d.classList.add("picker-dirty-dot", `picker-dirty-${dirtyDot}`);
        row.append(d);
      }
      row.addEventListener("mousedown", (e: MouseEvent) => {
        // On an anchor row, let Ctrl/Cmd/middle-click fall through to the browser's open-in-new-tab.
        if (href && (e.ctrlKey || e.metaKey || e.button === 1)) return;
        e.preventDefault();
        this.selected = this.offset + i;
        this.onEnter();
      });
      (section ?? win).append(row);
    });

    const spacer = document.createElement("div");
    spacer.className = "picker-spacer";
    spacer.append(win);

    // Synthetic "+ create" row at the end (Explorer / Projects with a non-empty query).
    let createRow: HTMLElement | null = null;
    if (this.hasCreate()) {
      const row = document.createElement("div");
      createRow = row;
      const isSel = this.selected === this.total;
      row.className = isSel ? "picker-row create selected" : "picker-row create";
      if (isSel) selectedRow = row;
      const main = document.createElement("span");
      main.className = "picker-main";
      main.textContent = `＋ ${this.createLabel()}`;
      row.append(main);
      row.addEventListener("mousedown", (e) => {
        e.preventDefault();
        this.selected = this.total;
        this.onEnter();
      });
      spacer.append(row);
    }
    // Coordinate rows: for grep these are DISPLAY rows (hits + file headers, server-reported) so the
    // spacer counts headers and the last file stays reachable; for other kinds they're item rows.
    // The grep window block starts one row above its first hit (the repeated file header sits there).
    const isGrep = this.grepTotalDisplayRows != null;
    const displayTotal = isGrep ? this.grepTotalDisplayRows! : this.total + (createRow ? 1 : 0);
    const displayOffset = isGrep ? Math.max(0, (this.grepDisplayOffset ?? 1) - 1) : this.offset;
    const displayLen = isGrep ? this.items.length + headersInWindow : this.items.length;
    // Grep's per-group gaps (each section's margin-top) are extra pixels the row-count geometry
    // doesn't see: the spacer adds one gap per file group in the whole result set (groups =
    // display rows minus hits), and the window shifts down by the gaps above it. The window's own
    // first section's margin supplies its gap, so it isn't counted (hence displayOffset - offset,
    // the headers *strictly above* the window block) — but it IS inside the window's px extent,
    // so the bottom bound counts all headersInWindow gaps.
    const gapsTotal = isGrep ? Math.max(0, displayTotal - this.total) : 0;
    const gapsAboveWin = isGrep ? Math.max(0, displayOffset - this.offset) : 0;
    // Set geometry from the known row height BEFORE inserting — the window/create rows are absolute,
    // so without an explicit spacer height the container would collapse to ~0 on replaceChildren and
    // the browser would clamp scrollTop back to the top.
    const applyGeometry = () => {
      this.winTopPx = displayOffset * this.rowH + gapsAboveWin * GREP_GROUP_GAP;
      this.winBottomPx =
        this.winTopPx + displayLen * this.rowH + headersInWindow * GREP_GROUP_GAP;
      this.spacerPx = displayTotal * this.rowH + gapsTotal * GREP_GROUP_GAP;
      win.style.top = `${this.winTopPx}px`;
      spacer.style.height = `${this.spacerPx}px`;
      if (createRow) createRow.style.top = `${this.total * this.rowH}px`;
    };
    applyGeometry();
    this.listEl.replaceChildren(spacer);
    // Re-measure once in the DOM; only re-apply if the row height actually changed. Use the
    // fractional rect height (offsetHeight rounds to an integer, which accumulates into a visible
    // gap below the last row over a long list).
    const probe = win.querySelector(".picker-row:not(.grep-header)") as HTMLElement | null;
    const measured = probe?.getBoundingClientRect().height ?? 0;
    if (measured > 0 && measured !== this.rowH) {
      this.rowH = measured;
      applyGeometry();
    }

    // Note: do NOT scroll to the selection here — renderList also runs on scroll-driven window
    // reloads, and scrolling then would fight the user. Callers that move the selection scroll
    // explicitly (move / scrollToSelOnUpdate).
    this.selectedRowEl = selectedRow;
    const pos = this.total === 0 ? 0 : this.selected + 1;
    this.countEl.textContent = `${pos}/${this.total}${this.ticking ? " · scanning…" : ""}`;
    this.updatePath();
  }

  /** Mouse/trackpad scroll: load the window covering the visible range when it nears a loaded edge.
   *  Bounds are compared in PIXELS against the loaded window's exact px extent (renderList computed
   *  it, group gaps included). Converting px to rows first — even via an average gap-aware row
   *  height — drifts when grep's gaps cluster unevenly, and a drift past the fetch margin either
   *  strands the viewport in unloaded blank or oscillates fetches (visible flicker). The fetch
   *  offset is still a proportional estimate (item ≈ px share of the spacer); the next scroll event
   *  re-checks against the exactly-placed result, so estimates converge instead of compounding. */
  private onListScroll(): void {
    if (this.total === 0 || this.listFetchInFlight) return;
    const viewTop = this.listEl.scrollTop;
    const viewBottom = viewTop + this.listEl.clientHeight;
    const limit = this.fetchLimit();
    const marginPx = Math.floor(limit / 4) * this.rowH;
    const needAbove = this.winTopPx > 0 && viewTop < this.winTopPx + marginPx;
    // 1px epsilon: the window-bottom and spacer heights are float sums via different expressions.
    const needBelow = this.winBottomPx < this.spacerPx - 1 && viewBottom > this.winBottomPx - marginPx;
    if (!needAbove && !needBelow) return;
    // Estimate the item offset whose row sits ~margin above the viewport, anchored to the loaded
    // window (whose top px ↔ item offset correspondence is exact): px distance → display rows →
    // items via the global hits-per-display-row ratio. Anchoring locally keeps the error
    // proportional to the gap-density deviation over the *distance jumped*, not over the whole
    // list above (the global version drifted by whole viewports on skewed grep results).
    const displayTotal = this.grepTotalDisplayRows ?? Math.max(1, this.total);
    const itemsPerRow = this.total / Math.max(1, displayTotal);
    const wantPx = Math.max(0, viewTop - marginPx);
    const maxOffset = Math.max(0, this.total - limit);
    let target = this.offset + Math.round(((wantPx - this.winTopPx) / this.rowH) * itemsPerRow);
    target = Math.max(0, Math.min(target, maxOffset));
    // Force progress in the scroll direction when the estimate lands back at the current offset
    // against a loaded edge (which would wedge) — but only when exactly one side needs more.
    // When both margins are violated at once, the unforced estimate (which covers the viewport
    // top-down) is right; the old always-on rules fought each other and oscillated.
    if (needAbove !== needBelow) {
      if (needBelow && target <= this.offset) target = Math.min(maxOffset, this.offset + Math.floor(limit / 2));
      if (needAbove && target >= this.offset) target = Math.max(0, this.offset - Math.floor(limit / 2));
    }
    // Guard against re-fetching what's already loaded / in flight (not just the last requested offset).
    if (target === this.offset || target === this.requestedOffset) return;
    this.listFetchInFlight = true;
    void this.view(false, target).finally(() => {
      this.listFetchInFlight = false;
      // Re-check once the landed window has been placed (the picker/update push carrying it is
      // processed by the next frame): if the estimate fell short of covering the viewport, the
      // next iteration corrects from the new, closer anchor — converging without waiting for
      // another scroll event. (Previously a short landing left blank rows until the user moved.)
      requestAnimationFrame(() => this.onListScroll());
    });
  }

  /** A "+ create" row is offered for Explorer / Projects when a query is typed. */
  private hasCreate(): boolean {
    return (this.kind === "explorer" || this.kind === "projects") && this.input.value.trim().length > 0;
  }

  private createLabel(): string {
    const q = this.input.value.trim();
    if (this.kind === "projects") return `Create project "${q}"`;
    return q.endsWith("/") ? `Create directory ${q}` : `Create file ${q}`;
  }

  /** Act on the synthetic create row: directories are created in place (refresh); files / projects
   *  hand off to the editor (which opens the file / activates the new project and closes the picker). */
  private async doCreate(): Promise<void> {
    const q = this.input.value.trim();
    if (!q) return;
    if (this.kind === "projects") {
      this.onCreateProject(q);
      return;
    }
    // Explorer.
    if (q.endsWith("/")) {
      try {
        await this.client.rpc<{ path: string }>("directory/create", {
          path: joinPath(this.explorerDir ?? "", q.slice(0, -1)),
        });
        this.onToast(`created ${q}`, "success");
        this.refresh();
      } catch (e) {
        this.onToast((e as Error).message, "error");
      }
    } else {
      this.onCreatePath(joinPath(this.explorerDir ?? "", q));
    }
  }

  /** Explorer breadcrumb above the input: the current directory, project-relative, in blue. */
  private updatePath(): void {
    const path = this.kind === "explorer" ? this.explorerDisplayPath() : "";
    this.pathEl.textContent = path;
    this.pathEl.style.display = path ? "block" : "none";
  }

  /** Path within the project root (empty at a root's top), matching the terminal — which shows the
   *  relative path only, plus a root label for multi-root projects. Not the root basename. */
  private explorerDisplayPath(): string {
    const dir = this.explorerDir;
    if (!dir) return ""; // roots mode — the rows already say "pick a root"
    let best = "";
    let bestIdx = -1;
    this.projectPaths.forEach((root, i) => {
      const norm = root.endsWith("/") ? root.slice(0, -1) : root;
      if ((dir === norm || dir.startsWith(norm + "/")) && norm.length > best.length) {
        best = norm;
        bestIdx = i;
      }
    });
    if (bestIdx < 0) return `${dir}/`;
    // The *disambiguated* root label, not the basename — colliding basenames would read alike.
    const label =
      this.projectPaths.length > 1 ? `${rootLabels(this.projectPaths)[bestIdx]}/` : "";
    if (dir === best) return label;
    // Segment-elide the path to the breadcrumb's share of the input row (its CSS max-width is
    // 55%); the trailing `/` is the "you're inside this dir" cue, re-appended after.
    const row = this.pathEl.parentElement;
    const style = getComputedStyle(this.pathEl);
    const px = (row ? row.clientWidth : 400) * 0.55;
    const budget = Math.max(
      8,
      charBudget(px, `${style.fontSize} ${style.fontFamily}`) - [...label].length,
    );
    const rel = truncatePath(dir.slice(best.length + 1), undefined, budget).display;
    return `${label}${rel}/`;
  }

  /** Approximate character budget for path text in the result list: the list's pixel width
   *  through the shared estimator, minus `reservedChars` for siblings (bullet, suffix, meta).
   *  CSS `text-overflow: ellipsis` stays as the safety net for the estimate's error margin. */
  private listPathBudget(reservedChars: number): number {
    const style = getComputedStyle(this.listEl);
    const px = this.listEl.clientWidth - 24; // row padding
    return Math.max(8, charBudget(px, `${style.fontSize} ${style.fontFamily}`) - reservedChars);
  }

  private describe(item: PickerItem): RowDesc {
    switch (item.kind) {
      case "file": {
        // Same left status-bullet as the explorer (ignored files never reach this picker — the
        // workspace walker skips them — so there's no `dim` case here). Multi-root projects show
        // the root's *disambiguated* label dimly after the path, matching the terminal client
        // (the raw basename would read alike for roots that share one).
        const suffix =
          this.projectPaths.length > 1
            ? rootLabels(this.projectPaths)[item.path_index] ?? `root ${item.path_index}`
            : undefined;
        const budget = this.listPathBudget(2 + (suffix ? [...suffix].length + 2 : 0));
        const { display, indices } = truncatePath(item.relative_path, item.match_indices, budget);
        return {
          primary: display,
          primaryMatches: indices,
          bullet: true,
          bulletStatus: item.git_status,
          suffix,
        };
      }
      case "buffer":
        // Buffer-state dot on the right (colour-coded), matching the editor status bar and the
        // terminal client's buffer picker. Clean buffers (`status` omitted → "clean") show nothing.
        // Transient buffers slant, like the status-bar label.
        return {
          primary: item.display,
          primaryMatches: item.match_indices,
          dirtyDot: item.status,
          italic: item.transient,
        };
      case "grep_hit": {
        // The file is shown in the group header; the row carries the line number + preview.
        // `match_indices` are char offsets into the untrimmed preview, so shift them down by the
        // stripped leading-whitespace char count (indices inside the whitespace drop out).
        const trimmed = item.preview.trimStart();
        const lead = [...item.preview].length - [...trimmed].length;
        return {
          primary: trimmed.trimEnd(),
          primaryMatches: item.match_indices?.map((i) => i - lead).filter((i) => i >= 0),
          meta: `${item.line + 1}`,
        };
      }
      case "diagnostic":
        return {
          primary: item.message.split("\n")[0],
          primaryMatches: item.match_indices,
          meta: diagRangeLabel(item.line, item.col, item.end_line ?? item.line, item.end_col ?? item.col),
          prefix: "● ",
          prefixClass: `sev-${item.severity}`,
        };
      case "dir_entry": {
        // Reserve a status-bullet column on every explorer entry (`bullet`); colour the `•` for a
        // real change (`bulletStatus`); ignored entries carry no bullet but dim their text (`dim`).
        const st = item.git_status;
        const changed = st && st !== "ignored" ? st : undefined;
        return {
          primary: item.is_dir ? `${item.name}/` : item.name,
          primaryMatches: item.match_indices,
          dir: item.is_dir,
          bullet: true,
          bulletStatus: changed,
          dim: st === "ignored",
        };
      }
      case "root": {
        const p = this.projectPaths[item.path_index];
        return { primary: `${p ? basename(p) : `root ${item.path_index}`}/`, primaryMatches: item.match_indices, dir: true };
      }
      case "project":
        return { primary: item.name, primaryMatches: item.match_indices };
      case "lsp_server": {
        // LSP state as the status bar's SVG icon in the leading bullet cell. Busy = ready with
        // active `$/progress` work, like the status bar (spinning, too);
        // starting/initializing/restarting share the busy icon. The dim meta is
        // `language · root` (root only when the server runs off the project root), followed by
        // any live progress hint.
        const busy = item.status.state === "ready" && (item.progress?.length ?? 0) > 0;
        const dot = busy ? "lsp-busy" : lspStateClass(item.status.state);
        const parts = [item.language];
        if (item.root_label) parts.push(item.root_label);
        const hint = lspProgressHint(item.progress);
        if (hint) parts.push(hint);
        return {
          primary: item.name,
          primaryMatches: item.match_indices,
          meta: parts.join(" · "),
          bullet: true,
          bulletClass: dot,
          bulletIcon: statusIcon(dot, dot === "lsp-busy"),
        };
      }
      case "reference": {
        // Dim `path:line` location prefix (the distinguishing bit, since reference previews often
        // repeat), then the preview line with the fuzzy-match highlights. No leading-whitespace
        // trim — `match_indices` index into the preview the server sent. The path takes at most
        // half the row, segment-elided so the filename + line number survive.
        const linePart = `:${item.line + 1}`;
        const pathBudget = Math.max(
          8,
          Math.floor(this.listPathBudget(0) / 2) - [...linePart].length,
        );
        const { display } = truncatePath(item.display_path, undefined, pathBudget);
        return {
          primary: item.preview,
          primaryMatches: item.match_indices,
          prefix: `${display}${linePart}`,
          prefixClass: "picker-loc",
        };
      }
    }
  }
}

interface RowDesc {
  primary: string;
  primaryMatches?: number[];
  meta?: string;
  prefix?: string;
  prefixClass?: string;
  dir?: boolean;
  /** Reserve the left status-bullet column (explorer entries), keeping text aligned across rows. */
  bullet?: boolean;
  /** Colour the bullet for a real Git change (modified/added/untracked/deleted/conflicted). */
  bulletStatus?: GitStatus;
  /** Explicit bullet colour class (LSP rows: `lsp-ready` / `lsp-busy` / …), overriding the
   *  git-status colouring. */
  bulletClass?: string;
  /** SVG status icon rendered in the bullet cell instead of the `•` glyph (LSP rows — the same
   *  icon the status bar shows, coloured via `bulletClass`). */
  bulletIcon?: SVGSVGElement;
  /** Dim the text to gray — used for `.gitignore`d entries (which carry no bullet). */
  dim?: boolean;
  /** Right-aligned buffer-state dot (buffer rows). `"clean"`/undefined → no dot. */
  dirtyDot?: BufferDirtyState;
  /** Italicise the primary text — transient buffers, matching the status-bar label. */
  italic?: boolean;
  /** Dim suffix right after the primary text (multi-root file rows: the root's name). */
  suffix?: string;
}

/** Filled-circle SVG dot for a buffer's dirty state — the same circle the tab favicon uses. The
 *  colour comes from `currentColor`, set by the caller (an inline style in the editor status bar,
 *  a `picker-dirty-*` class in the picker). Shared so both surfaces draw an identical icon. */
export function bufferStatusDot(): SVGSVGElement {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("class", "buffer-status-dot");
  svg.innerHTML = '<circle cx="8" cy="8" r="4.5" fill="currentColor"/>';
  return svg;
}

/** A diagnostic's range as a compact 1-based label: "12:5" for a point, "12:5-9" within a line,
 *  "12:5-14:2" across lines. Lets distinct diagnostics that read alike be told apart. */
function diagRangeLabel(line: number, col: number, endLine: number, endCol: number): string {
  if (line === endLine && col === endCol) return `${line + 1}:${col + 1}`;
  if (line === endLine) return `${line + 1}:${col + 1}-${endCol + 1}`;
  return `${line + 1}:${col + 1}-${endLine + 1}:${endCol + 1}`;
}

/** Full one-line description of a single progress op: "cargo check 28%  120/430". */
function lspProgressLine(p: LspProgress): string {
  let s = p.title;
  if (p.percentage != null) s += ` ${p.percentage}%`;
  if (p.message) s += `  ${p.message}`;
  return s;
}

/** Compact busy summary for a picker row: first op (with %) plus "+N" when several run. Empty when
 *  idle. */
function lspProgressHint(progress?: LspProgress[]): string {
  if (!progress || progress.length === 0) return "";
  const first = progress[0];
  let s = first.title;
  if (first.percentage != null) s += ` ${first.percentage}%`;
  if (progress.length > 1) s += ` +${progress.length - 1}`;
  return s;
}

function joinPath(dir: string, name: string): string {
  if (!dir) return name;
  return dir.endsWith("/") ? dir + name : `${dir}/${name}`;
}

function basename(path: string): string {
  const trimmed = path.endsWith("/") ? path.slice(0, -1) : path;
  const i = trimmed.lastIndexOf("/");
  return i >= 0 ? trimmed.slice(i + 1) : trimmed;
}

/** Stable identity of a picker item, so the selection can follow the *item* across a same-query
 *  re-rank (e.g. the buffers list reordering on open) instead of sticking to a now-stale index. */
function itemKey(item: PickerItem): string {
  switch (item.kind) {
    case "file":
      return `file\0${item.path_index}\0${item.relative_path}`;
    case "buffer":
      return `buffer\0${item.buffer_id}`;
    case "grep_hit":
      return `grep\0${item.path_index}\0${item.relative_path}\0${item.line}\0${item.col}`;
    case "diagnostic":
      return `diag\0${item.line}\0${item.col}\0${item.message}`;
    case "project":
      return `project\0${item.name}`;
    case "dir_entry":
      return `dir\0${item.name}`;
    case "root":
      return `root\0${item.path_index}`;
    case "lsp_server":
      return `lsp\0${item.language}\0${item.workspace_root}`;
    case "reference":
      return `reference\0${item.path}\0${item.line}\0${item.col}`;
  }
}

/** Identity of a grep hit (same line/col in the same file) — for centering + selection restore. */
function sameGrepHit(a: PickerItem, b: PickerItem): boolean {
  return (
    a.kind === "grep_hit" &&
    b.kind === "grep_hit" &&
    a.path_index === b.path_index &&
    a.relative_path === b.relative_path &&
    a.line === b.line &&
    a.col === b.col
  );
}

/** Wrap the code points at `indices` (server-provided char offsets) in <b class="match">. */
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
