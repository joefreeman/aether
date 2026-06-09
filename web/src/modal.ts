//! Native modal dialogs (confirm + save-as). Each shows an overlay and resolves a promise on the
//! user's choice, so callers can `await` them. The caller is responsible for suspending the editor
//! keymap while one is open (Editor.modalOpen); these handle their own keys (Enter/Esc/buttons).

function overlay(): HTMLElement {
  const el = document.createElement("div");
  el.className = "overlay"; // same top offset as pickers (confirmDialog adds confirm-overlay to sit lower)
  return el;
}

function button(label: string, variant?: "primary" | "danger"): HTMLButtonElement {
  const b = document.createElement("button");
  b.className =
    variant === "danger" ? "modal-btn primary danger" : variant === "primary" ? "modal-btn primary" : "modal-btn";
  b.textContent = label;
  return b;
}

/** Yes/No confirmation. Enter = confirm, Escape / backdrop = cancel. `danger` reddens the primary
 *  button for destructive actions. */
export function confirmDialog(message: string, opts?: { danger?: boolean }): Promise<boolean> {
  return new Promise((resolve) => {
    const ov = overlay();
    ov.classList.add("confirm-overlay"); // confirmations sit lower than pickers/dialogs
    const box = document.createElement("div");
    box.className = "modal";
    const msg = document.createElement("div");
    msg.className = "modal-message";
    msg.textContent = message;
    const row = document.createElement("div");
    row.className = "modal-buttons";
    const no = button("No");
    const yes = button("Yes", opts?.danger ? "danger" : "primary");
    row.append(no, yes);
    box.append(msg, row);
    ov.append(box);
    document.body.append(ov);

    const finish = (v: boolean) => {
      ov.removeEventListener("keydown", onKey, true);
      ov.remove();
      resolve(v);
    };
    const onKey = (e: KeyboardEvent) => {
      e.stopPropagation();
      if (e.key === "Enter") {
        e.preventDefault();
        finish(true);
      } else if (e.key === "Escape") {
        e.preventDefault();
        finish(false);
      }
    };
    ov.addEventListener("keydown", onKey, true);
    yes.addEventListener("click", () => finish(true));
    no.addEventListener("click", () => finish(false));
    ov.addEventListener("mousedown", (e) => {
      if (e.target === ov) finish(false);
    });
    // Focus the box so it captures keys even before the user clicks a button.
    box.tabIndex = -1;
    box.focus();
  });
}

export interface LspInfoData {
  name: string;
  language: string;
  workspaceRoot: string;
  state: string;
  message?: string | null;
  /** Active `$/progress` work, pre-formatted one line per operation (e.g. "cargo check 28%"). */
  progress?: string[];
}

/** A showing LSP-info dialog: `result` resolves with the chosen action; `update` re-renders it in
 *  place with fresh status/progress (so the caller can keep it live as `lsp/status_changed` lands). */
export interface LspInfoHandle {
  result: Promise<"restart" | "close">;
  update: (info: LspInfoData) => void;
}

/** Read-only LSP-server detail with a Restart action. Escape / Enter / backdrop = close, `r` =
 *  restart. Resolves the chosen action so the caller can fire the restart RPC. Live-updatable via
 *  the returned `update` while it stays open. */
export function lspInfoDialog(info: LspInfoData): LspInfoHandle {
  let update: (info: LspInfoData) => void = () => {};
  const result = new Promise<"restart" | "close">((resolve) => {
    const ov = overlay();
    ov.classList.add("confirm-overlay"); // same top offset as the confirmation dialogs
    const box = document.createElement("div");
    box.className = "modal lsp-info";
    const title = document.createElement("div");
    title.className = "modal-message";

    const rows = document.createElement("div");
    rows.className = "lsp-info-rows";
    // Rebuild the title + rows from `info`; called once at open and again on each live update.
    const render = (info: LspInfoData) => {
      title.textContent = info.name;
      rows.replaceChildren();
      const addRow = (label: string, value: string, valueCls?: string) => {
        const k = document.createElement("div");
        k.className = "lsp-info-key";
        k.textContent = label;
        const v = document.createElement("div");
        v.className = valueCls ? `lsp-info-val ${valueCls}` : "lsp-info-val";
        v.textContent = value;
        rows.append(k, v);
      };
      addRow("Language", info.language);
      addRow("Workspace", info.workspaceRoot);
      addRow("Status", info.state, `lsp-${info.state}`);
      if (info.message) addRow("Error", info.message, "sev-error");
      for (const [i, p] of (info.progress ?? []).entries()) {
        addRow(i === 0 ? "Working" : "", p, "lsp-busy");
      }
    };
    render(info);
    update = render;

    const row = document.createElement("div");
    row.className = "modal-buttons";
    const restart = button("Restart");
    const close = button("Close", "primary");
    row.append(restart, close);
    box.append(title, rows, row);
    ov.append(box);
    document.body.append(ov);

    const finish = (v: "restart" | "close") => {
      ov.removeEventListener("keydown", onKey, true);
      ov.remove();
      resolve(v);
    };
    const onKey = (e: KeyboardEvent) => {
      e.stopPropagation();
      if (e.key === "Escape" || e.key === "Enter") {
        e.preventDefault();
        finish("close");
      } else if (e.key === "r") {
        e.preventDefault();
        finish("restart");
      }
    };
    ov.addEventListener("keydown", onKey, true);
    restart.addEventListener("click", () => finish("restart"));
    close.addEventListener("click", () => finish("close"));
    ov.addEventListener("mousedown", (e) => {
      if (e.target === ov) finish("close");
    });
    box.tabIndex = -1;
    box.focus();
  });
  return { result, update };
}

export interface SaveAsResult {
  pathIndex: number;
  relativePath: string;
}

/** Path prompt for save-as / new file. `roots` are project-root labels; `initial` prefills the
 *  input. Resolves the chosen (pathIndex, relativePath), or null on cancel. */
export function saveAsDialog(opts: {
  roots: string[];
  initialPath: string;
  initialRootIndex: number;
}): Promise<SaveAsResult | null> {
  return new Promise((resolve) => {
    const ov = overlay();
    const box = document.createElement("div");
    box.className = "modal";
    const title = document.createElement("div");
    title.className = "modal-message";
    title.textContent = "Save as — path relative to the project root";

    const fieldRow = document.createElement("div");
    fieldRow.className = "modal-field";

    let select: HTMLSelectElement | null = null;
    if (opts.roots.length > 1) {
      select = document.createElement("select");
      select.className = "modal-select";
      opts.roots.forEach((label, i) => {
        const o = document.createElement("option");
        o.value = String(i);
        o.textContent = `${label}/`;
        select!.append(o);
      });
      select.value = String(opts.initialRootIndex);
      fieldRow.append(select);
    }

    const input = document.createElement("input");
    input.className = "modal-input";
    input.spellcheck = false;
    input.autocomplete = "off";
    input.placeholder = "src/example.rs";
    input.value = opts.initialPath;
    fieldRow.append(input);

    const row = document.createElement("div");
    row.className = "modal-buttons";
    const cancel = button("Cancel");
    const save = button("Save", "primary");
    row.append(cancel, save);
    box.append(title, fieldRow, row);
    ov.append(box);
    document.body.append(ov);

    const finish = (v: SaveAsResult | null) => {
      ov.removeEventListener("keydown", onKey, true);
      ov.remove();
      resolve(v);
    };
    const submit = () => {
      const rel = input.value.trim();
      if (!rel) return; // nothing to save to
      finish({ pathIndex: select ? Number(select.value) : opts.initialRootIndex, relativePath: rel });
    };
    const onKey = (e: KeyboardEvent) => {
      e.stopPropagation();
      if (e.key === "Enter") {
        e.preventDefault();
        submit();
      } else if (e.key === "Escape") {
        e.preventDefault();
        finish(null);
      }
    };
    ov.addEventListener("keydown", onKey, true);
    save.addEventListener("click", submit);
    cancel.addEventListener("click", () => finish(null));
    ov.addEventListener("mousedown", (e) => {
      if (e.target === ov) finish(null);
    });
    input.focus();
    input.select();
  });
}
