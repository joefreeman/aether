//! Data-driven keymap, ported from crates/aether-tui/src/keymap.rs (the subset Phase 2 implements).
//! Tables map a key chord to an abstract Action; main.ts resolves count/extend and runs the RPCs.
//! `extend` (Shift) and `count` are execution context, not table data — a motion is bound once and
//! the handler reads Shift to decide whether it grows the selection.
//!
//! Browser remap: Aether's `Ctrl-T` (toggle comment) is reserved by browsers, so it's `Ctrl-/`
//! here (the only keymap change the web client needs — see docs/web-client.md §2.4).

import type { Direction, HunkDirection, PickerKind, SurroundTarget, VerticalDirection, WordBoundary } from "./protocol";

export type InsertWhere = "selectionStart" | "selectionEnd" | "firstLineStart" | "lastLineEnd";
export type ScrollDir = "up" | "down" | "left" | "right";
export type ScrollUnit = "line" | "half" | "page";

export type Action =
  // motions (extend = Shift)
  | { t: "moveChar"; dir: Direction }
  | { t: "moveWord"; dir: Direction; boundary: WordBoundary }
  | { t: "moveWordEnd"; dir: Direction; boundary: WordBoundary }
  | { t: "moveVisualLine"; dir: VerticalDirection }
  | { t: "moveLogicalLine"; dir: Direction }
  | { t: "moveLineStart" }
  | { t: "moveLineEnd" }
  | { t: "moveLineFirstNonblank" }
  | { t: "gotoLine"; last: boolean }
  | { t: "matchBracket"; inner: boolean }
  | { t: "pageMotion"; dir: VerticalDirection; half: boolean }
  | { t: "navUnit"; dir: Direction }
  | { t: "navUnitEdge"; start: boolean }
  | { t: "beginFind"; dir: Direction; till: boolean }
  | { t: "repeatMotion" }
  | { t: "scroll"; dir: ScrollDir; unit: ScrollUnit }
  // selection / cursor history
  | { t: "selectLine"; dir: Direction }
  | { t: "swapAnchor" }
  | { t: "collapseSelection" }
  | { t: "treeExpand" }
  | { t: "treeContract" }
  | { t: "motionUndo" }
  | { t: "motionRedo" }
  | { t: "centerCursor" }
  // mode
  | { t: "enterInsert"; where: InsertWhere }
  | { t: "leaveInsert" }
  | { t: "beginLeader" }
  // search
  | { t: "enterSearch" }
  | { t: "enterSearchToCursor" }
  | { t: "searchFromSelection" }
  | { t: "searchCycle"; dir: Direction }
  | { t: "dropSearch" }
  // edits
  | { t: "backspace" }
  | { t: "newlineIndent" }
  | { t: "insertTab" }
  | { t: "deletePoint" }
  | { t: "deleteSelection" }
  | { t: "deleteLine" }
  | { t: "change" }
  | { t: "changeLine" }
  | { t: "undo" }
  | { t: "redo" }
  | { t: "moveLines"; dir: VerticalDirection }
  | { t: "joinLines" }
  | { t: "indent" }
  | { t: "dedent" }
  | { t: "toggleComment" }
  | { t: "openLineBelow" }
  | { t: "openLineAbove" }
  | { t: "beginSurround"; target: SurroundTarget }
  | { t: "unsurround"; target: SurroundTarget }
  // git
  | { t: "toggleDiffView" }
  | { t: "navigateHunk"; dir: HunkDirection }
  | { t: "grepNavigate"; dir: Direction }
  // LSP
  | { t: "hover" }
  | { t: "gotoDefinition" }
  | { t: "showDiagnostic" }
  | { t: "navigateDiagnostic"; dir: "next" | "prev" }
  | { t: "format" }
  // clipboard (system clipboard via navigator.clipboard / the native paste event)
  | { t: "copy" }
  | { t: "cut" }
  | { t: "replaceClipboard" }
  | { t: "copyLine" }
  | { t: "cutLine" }
  | { t: "replaceLineClipboard" }
  // leader / app
  | { t: "openPicker"; kind: PickerKind }
  | { t: "toggleWrap" }
  | { t: "save" }
  | { t: "saveAs" }
  | { t: "reload" }
  | { t: "newScratch" }
  | { t: "closeBuffer" }
  | { t: "openHelp" }
  | { t: "openProjectSettings" };

export type KeyContext = "normal" | "insert" | "global" | "leader";

export interface Chord {
  /** Canonical key: letters lowercased, others as KeyboardEvent.key ("Escape", "ArrowLeft", " "). */
  key: string;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  meta: boolean;
}

type ModPattern =
  | { kind: "exact"; ctrl: boolean; alt: boolean }
  | { kind: "ignoreShift"; ctrl: boolean; alt: boolean }
  | { kind: "any" };

const exact = (ctrl = false, alt = false): ModPattern => ({ kind: "exact", ctrl, alt });
const ignoreShift = (ctrl = false, alt = false): ModPattern => ({ kind: "ignoreShift", ctrl, alt });
const any: ModPattern = { kind: "any" };

interface Binding {
  key: string;
  mods: ModPattern;
  action: Action;
}
const b = (key: string, mods: ModPattern, action: Action): Binding => ({ key, mods, action });

function matchMods(p: ModPattern, c: Chord): boolean {
  if (p.kind === "any") return true;
  if (c.meta) return false; // never bind Meta (Cmd) — let the browser have it
  if (p.kind === "exact") return c.ctrl === p.ctrl && c.alt === p.alt && !c.shift;
  return c.ctrl === p.ctrl && c.alt === p.alt; // ignoreShift
}

// Tables are scanned in order; more-specific (Alt) rows precede catch-alls, exactly like the Rust
// tables. Bare keys use ignoreShift so `h` and `Shift-h` reach the same motion (Shift → extend).

const NORMAL: Binding[] = [
  b("Escape", any, { t: "dropSearch" }),
  b("c", exact(), { t: "collapseSelection" }),
  b("o", exact(), { t: "swapAnchor" }),
  b("y", exact(), { t: "treeExpand" }),
  b("y", exact(false, true), { t: "treeContract" }),
  b("z", exact(false, true), { t: "motionRedo" }),
  b("z", exact(), { t: "motionUndo" }),
  b("r", ignoreShift(), { t: "repeatMotion" }),

  // viewport scroll (cursor stays put) — arrows + page keys, Alt for half-page
  b("ArrowUp", exact(false, true), { t: "scroll", dir: "up", unit: "half" }),
  b("ArrowDown", exact(false, true), { t: "scroll", dir: "down", unit: "half" }),
  b("ArrowUp", any, { t: "scroll", dir: "up", unit: "line" }),
  b("ArrowDown", any, { t: "scroll", dir: "down", unit: "line" }),
  b("PageUp", any, { t: "scroll", dir: "up", unit: "page" }),
  b("PageDown", any, { t: "scroll", dir: "down", unit: "page" }),
  // Horizontal scroll stays on plain Left/Right. They're `exact` (no modifiers) so Alt-Left/Right
  // fall through *unhandled* to the browser, which drives back/forward through native history
  // (popstate); binding them here (or matching them with `any`) would preventDefault that away.
  b("ArrowLeft", exact(false, false), { t: "scroll", dir: "left", unit: "line" }),
  b("ArrowRight", exact(false, false), { t: "scroll", dir: "right", unit: "line" }),

  b("Home", any, { t: "moveLineStart" }),
  b("End", any, { t: "moveLineEnd" }),
  b("h", ignoreShift(false, true), { t: "moveLineFirstNonblank" }),
  b("h", ignoreShift(), { t: "moveChar", dir: "backward" }),
  b("l", ignoreShift(false, true), { t: "moveLineEnd" }),
  b("l", ignoreShift(), { t: "moveChar", dir: "forward" }),
  b("k", ignoreShift(false, true), { t: "moveVisualLine", dir: "up" }),
  b("k", ignoreShift(), { t: "moveLogicalLine", dir: "backward" }),
  b("j", ignoreShift(false, true), { t: "moveVisualLine", dir: "down" }),
  b("j", ignoreShift(), { t: "moveLogicalLine", dir: "forward" }),
  b("0", ignoreShift(), { t: "moveLineStart" }),

  b("d", ignoreShift(false, true), { t: "pageMotion", dir: "down", half: true }),
  b("d", ignoreShift(), { t: "pageMotion", dir: "down", half: false }),
  b("u", ignoreShift(false, true), { t: "pageMotion", dir: "up", half: true }),
  b("u", ignoreShift(), { t: "pageMotion", dir: "up", half: false }),

  b("w", ignoreShift(false, true), { t: "moveWord", dir: "forward", boundary: "WORD" }),
  b("w", ignoreShift(), { t: "moveWord", dir: "forward", boundary: "word" }),
  b("b", ignoreShift(false, true), { t: "moveWord", dir: "backward", boundary: "WORD" }),
  b("b", ignoreShift(), { t: "moveWord", dir: "backward", boundary: "word" }),
  b("e", ignoreShift(false, true), { t: "moveWordEnd", dir: "forward", boundary: "WORD" }),
  b("e", any, { t: "moveWordEnd", dir: "forward", boundary: "word" }),

  b("f", ignoreShift(false, true), { t: "beginFind", dir: "backward", till: false }),
  b("f", ignoreShift(), { t: "beginFind", dir: "forward", till: false }),
  b("t", ignoreShift(false, true), { t: "beginFind", dir: "backward", till: true }),
  b("t", ignoreShift(), { t: "beginFind", dir: "forward", till: true }),

  b("m", ignoreShift(false, true), { t: "matchBracket", inner: true }),
  b("m", ignoreShift(), { t: "matchBracket", inner: false }),
  b("]", exact(), { t: "navUnit", dir: "forward" }),
  b("[", exact(), { t: "navUnit", dir: "backward" }),
  b("}", any, { t: "navUnitEdge", start: false }),
  b("{", any, { t: "navUnitEdge", start: true }),
  b("g", ignoreShift(false, true), { t: "gotoLine", last: true }),
  b("g", ignoreShift(), { t: "gotoLine", last: false }),

  b("x", ignoreShift(false, true), { t: "selectLine", dir: "backward" }),
  b("x", ignoreShift(), { t: "selectLine", dir: "forward" }),

  b("i", exact(), { t: "enterInsert", where: "selectionStart" }),
  b("a", exact(), { t: "enterInsert", where: "selectionEnd" }),
  b("i", exact(false, true), { t: "enterInsert", where: "firstLineStart" }),
  b("a", exact(false, true), { t: "enterInsert", where: "lastLineEnd" }),

  b("-", exact(), { t: "centerCursor" }),
  b("Delete", any, { t: "deleteSelection" }),

  // search
  b("/", exact(false, true), { t: "searchFromSelection" }),
  b("/", ignoreShift(), { t: "enterSearch" }),
  b("?", ignoreShift(), { t: "enterSearchToCursor" }),
  b("n", ignoreShift(false, true), { t: "searchCycle", dir: "backward" }),
  b("n", ignoreShift(), { t: "searchCycle", dir: "forward" }),
  b(">", any, { t: "grepNavigate", dir: "forward" }),
  b("<", any, { t: "grepNavigate", dir: "backward" }),

  // selection-scoped Ctrl edits (Normal); clipboard ones aren't ported yet
  b("c", exact(true), { t: "change" }),
  b("d", exact(true), { t: "deleteSelection" }),
  b("y", exact(true), { t: "copy" }),
  b("x", exact(true), { t: "cut" }),
  // Ctrl-v is intentionally unbound: it flows to the native paste event (see main.ts onPaste),
  // which reads the clipboard without Firefox's readText permission prompt.
  b("r", exact(true), { t: "replaceClipboard" }),
  b("s", exact(true, true), { t: "unsurround", target: "selection" }),
  b("s", exact(true), { t: "beginSurround", target: "selection" }),

  b(" ", exact(), { t: "beginLeader" }),
];

const INSERT: Binding[] = [
  b("Escape", any, { t: "leaveInsert" }),
  b("Backspace", any, { t: "backspace" }),
  b("Delete", any, { t: "deletePoint" }),
  b("Enter", any, { t: "newlineIndent" }),
  b("Tab", any, { t: "insertTab" }),
  b("ArrowLeft", any, { t: "moveChar", dir: "backward" }),
  b("ArrowRight", any, { t: "moveChar", dir: "forward" }),
  b("ArrowUp", any, { t: "moveVisualLine", dir: "up" }),
  b("ArrowDown", any, { t: "moveVisualLine", dir: "down" }),

  b("c", exact(true), { t: "changeLine" }),
  b("d", exact(true), { t: "deleteLine" }),
  b("y", exact(true), { t: "copyLine" }),
  b("x", exact(true), { t: "cutLine" }),
  // Ctrl-v flows to the native paste event (see main.ts onPaste).
  b("r", exact(true), { t: "replaceLineClipboard" }),
  b("s", exact(true, true), { t: "unsurround", target: "line" }),
  b("s", exact(true), { t: "beginSurround", target: "line" }),
];

// Ctrl edits shared by both modes. Consulted before the per-mode table.
const GLOBAL: Binding[] = [
  b("z", exact(true, true), { t: "redo" }),
  b("z", exact(true), { t: "undo" }),
  b("j", exact(true), { t: "moveLines", dir: "down" }),
  b("k", exact(true), { t: "moveLines", dir: "up" }),
  b("g", exact(true), { t: "joinLines" }),
  b("l", exact(true), { t: "indent" }),
  b("h", exact(true), { t: "dedent" }),
  b("/", exact(true), { t: "toggleComment" }), // Aether's Ctrl-T, remapped for the browser
  b("o", exact(true, true), { t: "openLineAbove" }),
  b("o", exact(true), { t: "openLineBelow" }),
];

const LEADER: Binding[] = [
  b("f", exact(), { t: "openPicker", kind: "files" }),
  b("b", exact(), { t: "openPicker", kind: "buffers" }),
  b("g", exact(), { t: "openPicker", kind: "grep" }),
  b("e", exact(), { t: "openPicker", kind: "explorer" }),
  b("p", exact(), { t: "openPicker", kind: "projects" }),
  b("t", exact(), { t: "openPicker", kind: "diagnostics" }),
  b("l", exact(), { t: "openPicker", kind: "lsp_servers" }),
  b("s", exact(false, true), { t: "saveAs" }),
  b("s", exact(), { t: "save" }),
  b("c", exact(), { t: "closeBuffer" }),
  b("r", exact(), { t: "reload" }),
  b("n", exact(), { t: "newScratch" }),
  b("w", exact(), { t: "toggleWrap" }),
  b("i", exact(), { t: "toggleDiffView" }),
  b("h", exact(false, true), { t: "navigateHunk", dir: "prev" }),
  b("h", exact(), { t: "navigateHunk", dir: "next" }),
  b("k", exact(), { t: "hover" }),
  b("d", exact(), { t: "gotoDefinition" }),
  b("j", exact(), { t: "showDiagnostic" }),
  b("m", exact(), { t: "format" }),
  b("x", exact(false, true), { t: "navigateDiagnostic", dir: "prev" }),
  b("x", exact(), { t: "navigateDiagnostic", dir: "next" }),
  b(",", exact(), { t: "openProjectSettings" }),
  b("?", any, { t: "openHelp" }),
];

function table(ctx: KeyContext): Binding[] {
  switch (ctx) {
    case "normal":
      return NORMAL;
    case "insert":
      return INSERT;
    case "global":
      return GLOBAL;
    case "leader":
      return LEADER;
  }
}

export function lookup(ctx: KeyContext, c: Chord): Action | null {
  const hit = table(ctx).find((bd) => bd.key === c.key && matchMods(bd.mods, c));
  return hit ? hit.action : null;
}

/** Canonical chord from a KeyboardEvent: letters lowercased (Shift → the `shift` flag), other keys
 *  kept as-is. The find-char target uses the raw `event.key` instead (case-sensitive). */
export function chordOf(e: KeyboardEvent): Chord {
  const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  return { key, ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, meta: e.metaKey };
}
