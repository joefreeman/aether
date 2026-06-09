//! Keyboard-shortcut help overlay (Space ?). Curated reference of the implemented bindings, split
//! into tabs by context — Normal / Insert / Search / Application — mirroring the terminal UI's help.
//! (The TUI generates its help from the keymap tables; the web keymap carries no descriptions, so
//! this is hand-maintained — keep it in sync when bindings change.)

interface Section {
  title: string;
  rows: [string, string][];
}

interface Tab {
  label: string;
  sections: Section[];
}

const TABS: Tab[] = [
  {
    label: "Normal",
    sections: [
      {
        title: "Motion (Shift extends selection)",
        rows: [
          ["h  l", "char left / right"],
          ["j  k", "line down / up"],
          ["Alt-j  Alt-k", "visual row down / up"],
          ["w  b  e", "word forward / back / end (Alt = sub-word)"],
          ["0  Home / End  Alt-l", "line start / end"],
          ["Alt-h", "first non-blank"],
          ["g  Alt-g", "go to line (count) / last line"],
          ["f  t", "find / till char (Alt = backward)"],
          ["m  Alt-m", "matching / inner bracket"],
          ["d  u", "page down / up (Alt = half)"],
          ["]  [", "next / prev navigation unit"],
          ["}  {", "select to end / start of unit"],
          ["1-9", "count prefix (e.g. 3w)"],
        ],
      },
      {
        title: "Selection",
        rows: [
          ["c", "collapse selection"],
          ["o", "swap cursor / anchor"],
          ["x  Alt-x", "select line down / up"],
          ["y  Alt-y", "expand / contract to syntax node"],
          ["z  Alt-z", "undo / redo motion"],
          ["r", "repeat last motion"],
          ["-", "center cursor"],
        ],
      },
      {
        title: "Edit",
        rows: [
          ["i  a", "insert at selection start / end (Alt = line start/end)"],
          ["Backspace  Delete", "delete before / selection"],
          ["Ctrl-d  Ctrl-c", "delete / change selection"],
          ["Ctrl-z  Ctrl-Alt-z", "undo / redo"],
          ["Ctrl-l  Ctrl-h", "indent / dedent"],
          ["Ctrl-j  Ctrl-k", "move line(s) down / up"],
          ["Ctrl-g", "join lines"],
          ["Ctrl-/", "toggle comment"],
          ["Ctrl-o  Ctrl-Alt-o", "open line below / above"],
          ["Ctrl-s  Ctrl-Alt-s", "surround / unsurround"],
        ],
      },
      {
        title: "Clipboard",
        rows: [
          ["Ctrl-y  Ctrl-x", "copy / cut"],
          ["Ctrl-v", "paste"],
          ["Ctrl-r", "replace with clipboard"],
        ],
      },
      {
        title: "Search & scroll",
        rows: [
          ["/  ?", "search / select from cursor to match"],
          ["Alt-/", "search for selection"],
          ["n  Alt-n", "next / prev match"],
          ["< >", "prev / next grep hit"],
          ["↑ ↓  PageUp/Down", "scroll line / page (Alt = half)"],
          ["← →", "scroll horizontally (no wrap)"],
        ],
      },
    ],
  },
  {
    label: "Insert",
    sections: [
      {
        title: "Insert mode",
        rows: [
          ["(type)", "insert text — IME / dead keys / accents supported"],
          ["Esc", "leave insert mode"],
          ["Enter", "newline + auto-indent"],
          ["Tab", "insert a tab"],
          ["Backspace  Delete", "delete before / after the cursor"],
        ],
      },
      {
        title: "Also available in Insert",
        rows: [
          ["Ctrl-y / x / v", "copy / cut / paste"],
          ["Ctrl-z  Ctrl-Alt-z", "undo / redo"],
          ["Ctrl-l  Ctrl-h", "indent / dedent"],
          ["Ctrl-/", "toggle comment"],
          ["Ctrl-s", "surround selection"],
        ],
      },
    ],
  },
  {
    label: "Search",
    sections: [
      {
        title: "From Normal mode",
        rows: [
          ["/", "open search"],
          ["?", "search, extending the selection to each match"],
          ["Alt-/", "search for the current selection"],
          ["n  Alt-n", "jump to next / prev match"],
        ],
      },
      {
        title: "In the search bar",
        rows: [
          ["(type)", "filter matches incrementally"],
          ["Enter", "commit the search"],
          ["Esc", "cancel and restore the cursor"],
          ["↑ ↓", "previous / next in search history"],
        ],
      },
    ],
  },
  {
    label: "Application",
    sections: [
      {
        title: "Pickers (Space …)",
        rows: [
          ["Space f  b  g", "files / buffers / grep"],
          ["Space e", "explorer"],
          ["Space p  t  l", "projects / diagnostics / LSP servers"],
          ["Alt-j/k  Enter  Esc", "move / open / close (in a picker)"],
          ["Alt-l  Alt-h", "enter dir / up (explorer); grep file jump"],
          ["Delete", "delete highlighted (files/explorer/projects)"],
        ],
      },
      {
        title: "View (Space …)",
        rows: [
          ["Space w", "toggle soft wrap"],
          ["Space i", "toggle inline diff"],
          ["Space Alt-i", "toggle diff base (HEAD/index)"],
          ["Space h  Alt-h", "next / prev hunk"],
        ],
      },
      {
        title: "Code — LSP (Space …)",
        rows: [
          ["Space k", "hover"],
          ["Space d  Alt-d", "go to definition / references"],
          ["Space j", "show diagnostic"],
          ["Space x  Alt-x", "next / prev diagnostic"],
          ["Space m", "format document"],
        ],
      },
      {
        title: "App (Space …)",
        rows: [
          ["Space s  Space Alt-s", "save / save as"],
          ["Space r  Space n", "reload / new scratch"],
          ["Space c", "close buffer"],
          ["Space ,", "project settings"],
          ["Space ?", "this help"],
        ],
      },
    ],
  },
];

/** Show the help overlay; resolves when dismissed (Esc / Space-? / click outside). Tabs switch with
 *  the tab labels, ← / → (or Tab), or 1-4. */
export function showHelp(): Promise<void> {
  return new Promise((resolve) => {
    const ov = document.createElement("div");
    ov.className = "overlay";
    const box = document.createElement("div");
    box.className = "modal help";
    box.tabIndex = -1;

    const tabBar = document.createElement("div");
    tabBar.className = "help-tabs";
    const grid = document.createElement("div");
    grid.className = "help-grid";

    let active = 0;
    const tabEls = TABS.map((tab, i) => {
      const t = document.createElement("button");
      t.className = "help-tab";
      t.textContent = tab.label;
      t.addEventListener("click", () => select(i));
      tabBar.append(t);
      return t;
    });

    const select = (i: number) => {
      active = (i + TABS.length) % TABS.length;
      tabEls.forEach((t, j) => t.classList.toggle("active", j === active));
      grid.replaceChildren(...TABS[active].sections.map(renderSection));
    };

    box.append(tabBar, grid);
    ov.append(box);
    document.body.append(ov);
    select(0);

    const finish = () => {
      ov.removeEventListener("keydown", onKey, true);
      ov.remove();
      resolve();
    };
    const onKey = (e: KeyboardEvent) => {
      e.stopPropagation();
      if (e.key === "Escape" || e.key === "?") {
        e.preventDefault();
        finish();
      } else if (e.key === "ArrowRight" || (e.key === "Tab" && !e.shiftKey)) {
        e.preventDefault();
        select(active + 1);
      } else if (e.key === "ArrowLeft" || (e.key === "Tab" && e.shiftKey)) {
        e.preventDefault();
        select(active - 1);
      } else if (e.key >= "1" && e.key <= String(TABS.length)) {
        e.preventDefault();
        select(Number(e.key) - 1);
      }
    };
    ov.addEventListener("keydown", onKey, true);
    ov.addEventListener("mousedown", (e) => {
      if (e.target === ov) finish();
    });
    box.focus();
  });
}

function renderSection(section: Section): HTMLElement {
  const sec = document.createElement("div");
  sec.className = "help-section";
  const h = document.createElement("div");
  h.className = "help-section-title";
  h.textContent = section.title;
  sec.append(h);
  for (const [key, desc] of section.rows) {
    const row = document.createElement("div");
    row.className = "help-row";
    const k = document.createElement("span");
    k.className = "help-key";
    k.textContent = key;
    const d = document.createElement("span");
    d.className = "help-desc";
    d.textContent = desc;
    row.append(k, d);
    sec.append(row);
  }
  return sec;
}
