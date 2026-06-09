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
import { confirmDialog, lspInfoDialog } from "./modal";
import type { GitStatus, PickerItem, PickerKind, PickerUpdateParams, PickerViewResult } from "./protocol";

type ToastKind = "info" | "error" | "warning" | "success";

const LIMIT = 64; // window size fetched per view
const PAGE = 10; // PageUp/PageDown step

const PLACEHOLDER: Record<string, string> = {
  files: "Find files",
  buffers: "Switch buffer",
  grep: "Grep workspace",
};

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
  /** Diagnostics: the buffer whose diagnostics to list. */
  diagnosticsBufferId?: number;
  /** Grep: the active buffer, so the picker opens centered on the cursor's nearest hit. */
  activeBufferId?: number;
  /** Surface a transient message (deletion result / errors). */
  onToast?: (message: string, kind?: ToastKind) => void;
  /** Create + open a file at this absolute path (Explorer "+ create file"). */
  onCreatePath?: (absPath: string) => void;
  /** Create + activate a new project (Projects "+ create project"). */
  onCreateProject?: (name: string) => void;
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
  private diagnosticsBufferId: number | undefined;
  private activeBufferId: number | undefined;
  private onToast: (message: string, kind?: ToastKind) => void;
  private onCreatePath: (absPath: string) => void;
  private onCreateProject: (name: string) => void;
  private fileUrl: (item: PickerItem, explorerDir: string | null) => string | null;

  private overlay: HTMLElement;
  private pathEl: HTMLElement;
  private input: HTMLInputElement;
  private listEl: HTMLElement;
  private countEl: HTMLElement;

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
  // Loaded-window geometry in coordinate rows (display rows for grep, item rows otherwise), set by
  // renderList and read by onListScroll.
  private displayTotal = 0;
  private displayOffset = 0;
  private displayLen = 0;

  constructor(opts: PickerOptions) {
    this.client = opts.client;
    this.kind = opts.kind;
    this.onConfirm = opts.onConfirm;
    this.onClose = opts.onClose;
    this.projectPaths = opts.projectPaths ?? [];
    this.diagnosticsBufferId = opts.diagnosticsBufferId;
    this.activeBufferId = opts.activeBufferId;
    this.onToast = opts.onToast ?? (() => {});
    this.onCreatePath = opts.onCreatePath ?? (() => {});
    this.onCreateProject = opts.onCreateProject ?? (() => {});
    this.fileUrl = opts.fileUrl ?? (() => null);

    this.overlay = document.createElement("div");
    this.overlay.className = "overlay";
    const box = document.createElement("div");
    box.className = "picker";
    const inputRow = document.createElement("div");
    inputRow.className = "picker-input-row";
    this.pathEl = document.createElement("span");
    this.pathEl.className = "picker-path";
    this.pathEl.style.display = "none";
    this.input = document.createElement("input");
    this.input.className = "picker-input";
    this.input.placeholder = PLACEHOLDER[this.kind] ?? "";
    this.input.spellcheck = false;
    this.input.autocomplete = "off";
    inputRow.append(this.pathEl, this.input);
    this.listEl = document.createElement("div");
    this.listEl.className = "picker-list";
    this.countEl = document.createElement("div");
    this.countEl.className = "picker-count";
    box.append(inputRow, this.listEl, this.countEl);
    this.overlay.append(box);
    document.body.append(this.overlay);

    this.input.addEventListener("input", () => this.onQueryInput());
    this.input.addEventListener("keydown", (e) => this.onKey(e));
    this.listEl.addEventListener("scroll", () => this.onListScroll(), { passive: true });
    this.overlay.addEventListener("mousedown", (e) => {
      if (e.target === this.overlay) this.onClose();
    });

    this.input.focus();
    if (this.kind === "explorer") {
      void this.viewExplorer({ directory_path: opts.explorerInitialDir ?? null, selectName: opts.explorerSelectName });
    } else if (this.kind === "grep") {
      void this.viewGrepOpen();
    } else {
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
      limit: LIMIT,
      center_on_cursor_grep_hit: this.activeBufferId,
    });
    this.generation = r.generation; // adopt the persisted query's generation baseline
    this.offset = r.effective_offset;
    this.requestedOffset = r.effective_offset;
    this.input.value = r.query; // restore the persisted query text
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
    // Scroll to the selection only for selection-driven updates (centering / a pending move) — never
    // for plain scroll-fetches, which would yank the view back to the selection.
    if (resolved || this.scrollToSelOnUpdate) {
      this.scrollSelectionIntoView();
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
      limit: LIMIT,
      // Diagnostics are scoped to a buffer, required when (re)opening the candidate set.
      buffer_id: reset && this.kind === "diagnostics" ? this.diagnosticsBufferId : undefined,
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
      limit: LIMIT,
      directory_path: opts.roots ? null : opts.directory_path ?? null,
      explorer_roots: opts.roots ?? false,
      center_on: centerOn,
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
      .rpc<null>("picker/query", { kind: this.kind, query: this.input.value, generation: this.generation })
      .catch(() => {});
    this.renderList(); // immediate feedback (clears list + shows the create row as you type)
  }

  private onKey(e: KeyboardEvent): void {
    if (e.key === "Escape") {
      e.preventDefault();
      this.onClose();
    } else if (e.key === "Enter") {
      e.preventDefault();
      this.onEnter();
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

  /** Open the highlighted LSP server's status in a modal; Restart fires lsp/restart_server. */
  private async showLspInfo(item: PickerItem): Promise<void> {
    if (item.kind !== "lsp_server") return;
    const choice = await lspInfoDialog({
      name: item.name,
      language: item.language,
      workspaceRoot: item.workspace_root,
      state: item.status.state,
      message: item.status.state === "crashed" ? item.status.message : null,
    });
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
      limit: LIMIT,
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

  /** Alt-h / Alt-Backspace: clear filter, else step up (parent → roots mode). */
  private async back(): Promise<void> {
    if (this.input.value !== "") {
      this.input.value = "";
      this.onQueryInput();
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

  private scrollSelectionIntoView(): void {
    this.selectedRowEl?.scrollIntoView({ block: "nearest" });
  }

  /** If the selection has left the loaded window, fetch a new window centred on it. */
  private ensureWindow(): void {
    const loaded = this.selected >= this.offset && this.selected < this.offset + this.items.length;
    if (loaded) return;
    const maxOffset = Math.max(0, this.total - LIMIT);
    const target = Math.max(0, Math.min(this.selected - Math.floor(LIMIT / 2), maxOffset));
    if (target === this.requestedOffset && this.items.length > 0) return;
    void this.view(false, target).catch(() => {});
  }

  private renderList(): void {
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
          const h = document.createElement("div");
          h.className = "picker-row grep-header";
          h.textContent = this.grepFileLabel(item.path_index, item.relative_path);
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
      if (i === localSel) selectedRow = row;
      const { primary, primaryMatches, meta, prefix, prefixClass, dir, bullet, bulletStatus, dim } = this.describe(item);
      if (bullet) {
        // Fixed-width cell (empty when clean/ignored) so entry names stay aligned across rows; the
        // `•` glyph and its colour appear only on a real change.
        const b = document.createElement("span");
        b.className = bulletStatus ? `picker-bullet picker-bullet-${bulletStatus}` : "picker-bullet";
        b.textContent = bulletStatus ? "•" : "";
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
      main.className = mainClass.join(" ");
      main.append(matched(primary, primaryMatches));
      row.append(main);
      if (meta) {
        const m = document.createElement("span");
        m.className = "picker-meta";
        m.textContent = meta;
        row.append(m);
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
    this.displayTotal = isGrep ? this.grepTotalDisplayRows! : this.total + (createRow ? 1 : 0);
    this.displayOffset = isGrep ? Math.max(0, (this.grepDisplayOffset ?? 1) - 1) : this.offset;
    this.displayLen = isGrep ? this.items.length + headersInWindow : this.items.length;
    // Set geometry from the known row height BEFORE inserting — the window/create rows are absolute,
    // so without an explicit spacer height the container would collapse to ~0 on replaceChildren and
    // the browser would clamp scrollTop back to the top.
    const applyGeometry = () => {
      win.style.top = `${this.displayOffset * this.rowH}px`;
      spacer.style.height = `${this.displayTotal * this.rowH}px`;
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
   *  Works in coordinate rows (display rows for grep); the fetch is by item offset, so the visible
   *  coordinate row is converted back to an item index (proportionally for grep). */
  private onListScroll(): void {
    if (this.total === 0 || this.listFetchInFlight) return;
    const visible = Math.max(1, Math.round(this.listEl.clientHeight / this.rowH));
    const topRow = Math.round(this.listEl.scrollTop / this.rowH);
    const margin = Math.floor(LIMIT / 4);
    const loadedEnd = this.displayOffset + this.displayLen;
    const needAbove = this.displayOffset > 0 && topRow < this.displayOffset + margin;
    const needBelow = loadedEnd < this.displayTotal && topRow + visible > loadedEnd - margin;
    if (!needAbove && !needBelow) return;
    const targetRow = Math.max(0, topRow - margin);
    // Coordinate row → item offset. For grep, display rows include headers, so scale by the
    // hit/display-row ratio; the window the server returns is then positioned exactly.
    const isGrep = this.grepTotalDisplayRows != null;
    const maxOffset = Math.max(0, this.total - LIMIT);
    let target = isGrep ? Math.round((targetRow * this.total) / Math.max(1, this.displayTotal)) : targetRow;
    target = Math.max(0, Math.min(target, maxOffset));
    // Force progress in the scroll direction: the proportional grep estimate can round back to the
    // current offset even when we're against the loaded edge, which would wedge (never fetch).
    if (needBelow && target <= this.offset) target = Math.min(maxOffset, this.offset + Math.floor(LIMIT / 2));
    if (needAbove && target >= this.offset) target = Math.max(0, this.offset - Math.floor(LIMIT / 2));
    // Guard against re-fetching what's already loaded / in flight (not just the last requested offset).
    if (target === this.offset || target === this.requestedOffset) return;
    this.listFetchInFlight = true;
    void this.view(false, target).finally(() => {
      this.listFetchInFlight = false;
    });
  }

  /** Header label for a grep file group: `root: path` for multi-root projects, else just the path. */
  private grepFileLabel(pathIndex: number, relativePath: string): string {
    if (this.projectPaths.length > 1) {
      const root = this.projectPaths[pathIndex];
      return `${root ? basename(root) : `root ${pathIndex}`}: ${relativePath}`;
    }
    return relativePath;
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
    for (const root of this.projectPaths) {
      const norm = root.endsWith("/") ? root.slice(0, -1) : root;
      if ((dir === norm || dir.startsWith(norm + "/")) && norm.length > best.length) best = norm;
    }
    if (!best) return `${dir}/`;
    const rel = dir === best ? "" : `${dir.slice(best.length + 1)}/`;
    const label = this.projectPaths.length > 1 ? `${basename(best)}/` : "";
    return label + rel;
  }

  private describe(item: PickerItem): RowDesc {
    switch (item.kind) {
      case "file":
        // Same left status-bullet as the explorer (ignored files never reach this picker — the
        // workspace walker skips them — so there's no `dim` case here).
        return {
          primary: item.relative_path,
          primaryMatches: item.match_indices,
          bullet: true,
          bulletStatus: item.git_status,
        };
      case "buffer":
        return { primary: item.display, primaryMatches: item.match_indices, prefix: item.dirty ? "● " : "  " };
      case "grep_hit":
        return {
          // The file is shown in the group header; the row carries the line number + preview.
          primary: item.preview.trim(),
          primaryMatches: item.match_indices,
          meta: `${item.line + 1}`,
        };
      case "diagnostic":
        return {
          primary: item.message.split("\n")[0],
          primaryMatches: item.match_indices,
          meta: `:${item.line + 1}`,
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
        const where = item.root_label ? ` (${item.root_label})` : "";
        return { primary: item.name, primaryMatches: item.match_indices, meta: `${item.language} · ${item.status.state}${where}` };
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
  /** Dim the text to gray — used for `.gitignore`d entries (which carry no bullet). */
  dim?: boolean;
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
