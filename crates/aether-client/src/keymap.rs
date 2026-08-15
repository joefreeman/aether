//! Data-driven keybindings — a port of `aether-tui/src/keymap.rs` onto the core's own
//! key types (shells map their native key events in at the edge — see `input.rs`).
//!
//! The chords and their semantics are copied verbatim from the TUI so the clients stay
//! consistent; this file should never invent a binding the TUI doesn't have. It currently
//! carries the milestone-1 subset (motions, edits, clipboard, save/quit) — search, pickers,
//! git/LSP chords arrive with their UI surfaces. Once a shared client-core crate exists, both
//! this and the TUI table collapse into it.
//!
//! Same structural rules as the TUI: count accumulation and the `f`/`t` find-char capture stay
//! out of the table (they're stateful lexing), `extend` is derived from Shift at execution
//! time, and tables are scanned in order so more-specific chords precede catch-alls.

use aether_protocol::cursor::{Direction, VerticalDirection, WordBoundary};
use aether_protocol::input::{BlockUnit, CommentStyle, SurroundTarget};
use aether_protocol::picker::PickerKind;

/// Layout-resolved key identity, normalised from the platform's key event: letters lowercase
/// (Shift is carried separately in [`Mods`]), shifted symbols as produced (`?`, `{`, …).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyCode {
    Char(char),
    Esc,
    Enter,
    Tab,
    /// Shift-Tab (`CSI Z` in a terminal). Distinct from `Tab` because the two are opposite
    /// directions of the same gesture — "next field" / "previous field" — and folding them
    /// together (as the TUI once did) makes reverse traversal unexpressible.
    BackTab,
    Backspace,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
}

/// Fold Shift-Tab onto [`KeyCode::BackTab`].
///
/// Terminals send a distinct `BackTab` (`CSI Z`), while GUI and browser report a plain `Tab` with
/// Shift held. Each shell calls this at its input boundary so the core sees one key either way, and
/// "previous field" doesn't have to be spelled differently per client.
pub fn apply_backtab(code: KeyCode, mods: Mods) -> KeyCode {
    if code == KeyCode::Tab && mods.shift {
        KeyCode::BackTab
    } else {
        code
    }
}

/// Pick which normalised key a binding lookup should resolve against.
///
/// Normally we use the *modified* key, so layout/Shift composition is honoured (Shift-`/` → `?`,
/// etc.). But macOS applies Option(Alt)-composition to the modified key — Option-`f` arrives as
/// `ƒ`, Option-`j` as `∆` — which would never match an `Alt-f` binding. When Alt is held, fall back
/// to the *base* (unmodified) key, which is the raw `f` on every platform. On Linux/Windows the two
/// keys are equal under Alt, so this is a no-op there and a fix on macOS.
///
/// Shells own producing the two `KeyCode`s from their native key events (iced's `key` /
/// `modified_key`; the web's `e.code` / `e.key`) and pass them here — the rule itself lives in the
/// core so every shell resolves Alt-chords identically. The "base" key each shell can produce
/// differs slightly (winit's layout-aware `key_without_modifiers` vs the browser's physical
/// `e.code`), but that only matters for exotic non-QWERTY layouts.
pub fn keycode_for_binding(
    base: Option<KeyCode>,
    modified: Option<KeyCode>,
    alt: bool,
) -> Option<KeyCode> {
    if alt {
        base
    } else {
        modified
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    pub const NONE: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
    };
    pub const ALT: Mods = Mods {
        ctrl: false,
        alt: true,
        shift: false,
    };
    pub const CTRL: Mods = Mods {
        ctrl: true,
        alt: false,
        shift: false,
    };
    pub const CTRL_ALT: Mods = Mods {
        ctrl: true,
        alt: true,
        shift: false,
    };
    pub const SHIFT: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: true,
    };
    fn without_shift(self) -> Mods {
        Mods {
            shift: false,
            ..self
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyContext {
    Normal,
    Insert,
    Search,
    /// The markdown reading view (docs/markdown-view.md §2.3). Read-only by construction: this
    /// table contains no editing action, and the `Global` edit chords are not consulted in Read
    /// mode — the read-only invariant is the table itself.
    Read,
    Leader,
    Global,
}

/// How a binding matches modifiers — same three patterns as the TUI table.
#[derive(Clone, Copy)]
pub enum ModPattern {
    Exact(Mods),
    /// Equal ignoring Shift (Shift means "extend" and is read separately).
    IgnoreShift(Mods),
    Any,
}

impl ModPattern {
    /// The modifiers the help overlay displays for this pattern.
    fn display_mods(self) -> Mods {
        match self {
            ModPattern::Exact(m) | ModPattern::IgnoreShift(m) => m,
            ModPattern::Any => Mods::NONE,
        }
    }

    fn matches(self, mods: Mods) -> bool {
        match self {
            ModPattern::Exact(m) => mods == m,
            ModPattern::IgnoreShift(base) => mods.without_shift() == base,
            ModPattern::Any => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollUnit {
    Line,
    Half,
    Page,
}

#[derive(Clone, Copy, Debug)]
pub enum InsertWhere {
    SelectionStart,
    SelectionEnd,
    FirstLineStart,
    LastLineEnd,
}

/// The fraction of the viewport that sits *above* a cursor that's been jumped to or placed near the
/// top (search/diagnostic/hunk/go-to-line reveals, a cross-buffer open, and `;`). One source of
/// truth so those rest positions stay aligned; the shells apply it in their own units (rows / px).
pub const CURSOR_REST_FRACTION: f32 = 0.2;

/// Where to put the cursor's line vertically when the user explicitly repositions the view
/// (`;` / `Alt-;`). The shell scrolls so the line lands this far down the viewport.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewportPlace {
    /// Near the top — leaves more context below (matches a jump's rest position, `;`).
    Upper,
    /// Near the bottom — keeps the preceding context on screen (`Alt-;`).
    Lower,
}

impl ViewportPlace {
    /// The fraction of the viewport that sits *above* the cursor's line at this placement.
    pub fn fraction(self) -> f32 {
        match self {
            ViewportPlace::Upper => CURSOR_REST_FRACTION,
            ViewportPlace::Lower => 1.0 - CURSOR_REST_FRACTION,
        }
    }

    /// Reading-view placement gap (docs/markdown-view.md §2.3): the space between the view's
    /// edge and the focused *block's* matching edge — `Upper` leaves this above the block's
    /// top, `Lower` leaves it below the block's bottom. Edge-matched (unlike the editor's
    /// top-anchored line placement) so a tall block placed "near the bottom" actually ends
    /// there instead of hanging mostly off-screen. The gap is the editor's rest fraction, so
    /// `;` feels identical in both views (a cursor line is its own top *and* bottom edge).
    pub const READ_GAP: f32 = CURSOR_REST_FRACTION;
}

/// Abstract intent, mirroring the TUI's `Action` (subset). `count`/`extend` are execution
/// context resolved by the app.
#[derive(Clone, Copy, Debug)]
pub enum Action {
    // ---- motions (extend = Shift) ----
    MoveChar(Direction),
    /// `b` / `Alt-b` — move to the previous word start. (`w` selects words via
    /// [`Action::SelectWord`], so this is backward-only.)
    MoveWordBack {
        boundary: WordBoundary,
    },
    MoveWordEnd {
        dir: Direction,
        boundary: WordBoundary,
    },
    MoveVisualLine(VerticalDirection),
    MoveLogicalLine(Direction),
    MoveLineStart,
    MoveLineEnd,
    MoveLineFirstNonblank,
    MoveLogicalLineFirstNonblank(Direction),
    GotoLine {
        last: bool,
    },
    MatchBracket {
        inner: bool,
    },
    PageMotion {
        dir: VerticalDirection,
        half: bool,
    },
    NavUnit(Direction),
    BeginFind {
        dir: Direction,
        till: bool,
    },
    /// `s` / `S` / `Alt-s` / `Shift-Alt-s` — arm sneak word-jump. The next keystrokes build a
    /// word-prefix query; the server labels matching words and the label keystroke jumps. `big`
    /// targets whitespace-delimited "big" words (`Alt-s`, like `Alt-w`); `extend` (Shift) is read
    /// from the key event, like `BeginFind`.
    BeginSneak {
        big: bool,
    },

    // ---- selection ----
    SelectWord {
        boundary: WordBoundary,
    },
    SelectLine(Direction),
    SelectAll,
    /// Swap cursor and anchor (`r`). With `forward_only` (`Alt-r`), only a backward selection
    /// swaps — normalize to forward orientation instead of toggling.
    SwapAnchor {
        forward_only: bool,
    },
    CollapseSelection,
    TreeExpand,
    TreeContract,
    MotionUndo,
    MotionRedo,
    RepeatMotion,
    /// Reposition the view so the cursor's line sits at a fixed fraction down the viewport
    /// (`;` / `Alt-;`). Shell-owned (geometry).
    PlaceCursor(ViewportPlace),
    NavBack,
    NavForward,

    // ---- viewport ----
    Scroll {
        dir: ScrollDir,
        unit: ScrollUnit,
    },
    ToggleWrap,

    // ---- mode transitions ----
    EnterInsert(InsertWhere),
    LeaveInsert,
    BeginLeader,

    // ---- edits ----
    Backspace,
    NewlineIndent,
    /// Join's dual: insert a line break at the cursor, cursor staying *before* it (so a
    /// following join re-joins the same pair). Distinct from [`Action::NewlineIndent`], whose
    /// cursor advances onto the new line (Enter's typing flow).
    UnjoinLines,
    InsertTab,
    DeletePoint,
    DeleteSelection,
    Undo,
    Redo,
    MoveLines(VerticalDirection),
    JoinLines,
    Indent,
    Dedent,
    IncrementNumber,
    DecrementNumber,
    /// `Ctrl-y` (line) / `Ctrl-Alt-y` (block). The style is explicit per chord; the target is
    /// `Selection` in Normal mode and `Line` in Insert, mirroring surround/unsurround.
    ToggleComment(CommentStyle, SurroundTarget),
    OpenLineBelow,
    OpenLineAbove,
    // Selection-scoped (Normal) vs line-scoped (Insert) clipboard/edit pairs.
    Copy,
    Cut,
    /// `Ctrl-Alt-x` — cut the selection (like [`Action::Cut`]) and then enter Insert mode at the
    /// gap left behind, mirroring [`Action::Change`] but keeping the removed text on the clipboard.
    CutChange,
    Paste,
    Change,
    ReplaceClipboard,
    CopyLine,
    CutLine,
    PasteAtCursor,
    ChangeLine,
    DeleteLine,
    ReplaceLineClipboard,
    /// `Ctrl-s ␣` — the next keystroke names the delimiter to wrap the target with.
    BeginSurround(SurroundTarget),
    Unsurround(SurroundTarget),
    /// `Ctrl-r ␣` — the next keystroke names the case transform (see [`CaseKind::from_char`]).
    /// Operand: the selection, or the identifier under a point cursor.
    BeginTransform,

    // ---- search ----
    EnterSearch,
    /// `?` — enter search, growing the selection from the cursor to each incremental match.
    EnterSearchToCursor,
    SearchFromSelection,
    SearchCycle(Direction),
    SearchAbort,
    SearchCommit,
    SearchHistoryPrev,
    SearchHistoryNext,
    /// `Alt-c` in the search prompt: cycle case mode (smart → sensitive → insensitive → smart).
    SearchToggleCase,
    /// `Alt-w` in the search prompt: toggle whole-word matching.
    SearchToggleWord,
    /// `Alt-e` in the search prompt: toggle literal (fixed-string) vs. regex matching.
    SearchToggleRegex,
    /// `]` / `[` — step through the jumplist from the cursor, cross-file, stopping
    /// at the ends (docs/jumplist.md). Populated by `Ctrl-j` in a picker.
    JumplistStep(Direction),
    /// `}` / `{` — like [`Action::JumplistStep`] but restricted to entries in the current
    /// buffer's file, so you walk one file's hits without jumping away (docs/jumplist.md). Uses
    /// Shift-bracket, not Alt-bracket, because Alt-bracket collides with terminal escape
    /// introducers (`ESC [` / `ESC ]`) — see the binding site.
    JumplistStepInFile(Direction),
    /// `Esc` in Normal — drop the active search (clear highlights).
    DropSearch,

    // ---- app ----
    Quit,
    Save,
    SaveAs,
    /// `Space Alt-q` — save the current buffer, then quit if the save succeeds. An overwrite /
    /// external-change confirm defers the quit until the retry lands; a failed or cancelled save
    /// doesn't quit. Sequences `Save` then `Quit`.
    SaveAndQuit,
    /// `Space Alt-x` — save the current buffer, then close it if the save succeeds (the close
    /// analogue of [`Action::SaveAndQuit`], with the same confirm-deferral). On the tethered
    /// buffer (docs/tether.md) the close also exits the client — the one-chord finish for an
    /// `ae file` quick edit (write the commit message, `Space Alt-x`, done).
    SaveAndClose,
    /// `Space Alt-w` — open a file by typing its absolute path (a leading `~/` is fine),
    /// regardless of the active workspace. Outside any workspace root the file opens as an external
    /// buffer; with no workspace active it lands in a fresh ephemeral context. Pairs with `Space w`
    /// (switch workspace). Opens the open-from-path overlay; submit calls `workspace/open_path`.
    OpenPath,
    Reload,
    /// Toggle the active buffer's transient ("keep") state — pin a preview permanent, or release a
    /// permanent buffer back to transient. Refused for unsaved buffers (auto-close would discard).
    /// On the tethered buffer (docs/tether.md), un-keeping additionally *releases* the tether —
    /// the client stops exiting when the buffer closes; one-way, a re-keep is just a plain keep.
    ToggleKeep,
    /// Copy the active buffer's workspace-relative path to the system clipboard.
    CopyRelativePath,
    /// Copy the active buffer's absolute (canonical) path to the system clipboard.
    CopyAbsolutePath,
    NewScratch,
    CloseBuffer,
    /// `Space z` — open another window onto the same workspace: the GUI spawns a fresh detached
    /// `ae --gui` process dialling the same daemon; the web shell opens a new browser tab on the same
    /// URL. A new client lands on the workspace's MRU buffer (the one you're on), so it "duplicates"
    /// the current view; the two windows are independent thereafter (own cursor/selection/viewport,
    /// shared buffers server-side). The TUI has no window to spawn, so it ignores the
    /// [`ShellAction::NewWindow`] it emits. The spawn names the workspace explicitly (`--workspace`),
    /// so the sibling never tethers to the file it lands on (docs/tether.md).
    NewWindow,
    /// `Space Alt-z` — the share-link sibling of `Space z`: copy the web client's URL for the
    /// current buffer to the clipboard (`?workspace=&root=&file=` with the cursor as its `#L:C`
    /// fragment; `?buffer=` for a scratch). The shell prepends its own base
    /// ([`ShellAction::CopyWebUrl`]).
    CopyWebUrl,

    // ---- git ----
    ToggleDiffView,
    NextHunk,
    PrevHunk,
    ToggleStageHunk,
    RevertHunk,

    // ---- LSP ----
    GotoDefinition,
    NextDiagnostic,
    PrevDiagnostic,
    Hover,
    ShowDiagnostic,
    Format,

    // ---- git (popovers) ----
    ShowCommitInfo,

    // ---- pickers ----
    OpenPicker(PickerKind),
    /// `Space Alt-f` — open Files pre-scoped to the active buffer's directory, seeded as an
    /// ordinary directory filter chip (editable, composable, removable). The buffer-locked
    /// changes/diagnostics *modes* use a dedicated kind instead (see [`PickerKind::GitChangesFile`]).
    OpenFilesInBufferDir,
    /// `Space Alt-g` — open Grep with the query seeded from the buffer's selection (the grep
    /// equivalent of `Alt-/`). A fresh open, so the chip row starts empty like any other; an empty
    /// selection just opens grep.
    OpenGrepFromSelection,
    /// `Space Alt-e` — Explorer at the buffer's workspace root rather than its directory.
    OpenExplorerAtRoot,

    // ---- shell-local overlays (dispatched via `Effect::ShellAction`; a shell without the
    // overlay ignores them) ----
    /// `Space /` — the keyboard-shortcut help overlay, generated from these tables.
    OpenHelp,
    /// `Space ,` — the workspace-settings overlay (roots + rename). TUI-only today.
    OpenWorkspaceSettings,
    /// `Space .` — the application-settings overlay (global preferences, e.g. soft wrap). Font size
    /// lives here too (a stepped value row), not on a keybinding.
    OpenAppSettings,
    /// `Space ?` — the application-info dialog: build identity, the daemon we're connected to, and
    /// where this profile's state lives. Sits next to `Space /` (the shortcut reference) because
    /// both answer "what is this thing doing?" — one about keys, one about the install.
    ShowAppInfo,

    // ---- hints (docs/hints.md) ----
    /// `Space h` — dismiss the corner hint: down-weight it (a deliberate "not now") and show
    /// another. No-op when the corner is empty.
    DismissHint,
    /// `Space Alt-h` — toggle hints on/off (the same switch as the settings-overlay row),
    /// persisted app-wide.
    ToggleHints,

    // ---- markdown reading view (docs/markdown-view.md) ----
    /// `Space v` — toggle the markdown reading view on the current buffer (markdown only;
    /// remembered per buffer for the session).
    ToggleReadView,
    /// `j`/`k` — focus the next/previous block-grain element (the reading cursor; sends a
    /// `Goto` to the element's source start, so the server cursor *is* the reading position).
    ReadStep(Direction),
    /// `h`/`l` — step the Enter target among the links/images *inside the focused block* (the
    /// fine-grain axis to `j`/`k`'s coarse one, mirroring the editor's split).
    ReadStepLink(Direction),
    /// `Tab` — show the focused element's target without following it: a link's URL, an
    /// image's source, a footnote's definition text (the editor's Tab-reveals-hover, at
    /// reading grain).
    ReadShowTarget,
    /// `o`/`Alt-o` — next/previous heading (AST-resolved; the reading sibling of symbol nav).
    ReadStepHeading(Direction),
    /// `g`/`Alt-g` — first/last element (the reading form of the editor's buffer-start/end pair).
    ReadEnds {
        last: bool,
    },
    /// `Enter` — follow the focused element: open a link, an image, or jump to a footnote's
    /// definition. No-op on non-interactive blocks.
    ReadActivate,
    /// `Ctrl-Enter` — the picker's open-in-new-window, at reading grain: a relative-path link
    /// opens in a new window (GUI) / tab (web); anything else behaves like `Enter`.
    ReadActivateNewWindow,
    /// `Ctrl-c` — copy: an extended selection's source, else the focused element (a link's
    /// URL, otherwise its markdown source).
    ReadCopy,
    /// `x`/`Alt-x` — the editor's line-select at block grain (docs/markdown-view.md §12):
    /// plain presses walk block to block (whole-line normal form), Shift grows the selection.
    ReadSelectBlock(Direction),
    /// `i`/`a` — to the editor, inserting at the selection's start / end: an extended
    /// selection uses the editor's own Insert-entry motions; a bare reading position enters
    /// at the focused block's start / append position (docs/markdown-view.md §12).
    ReadInsert {
        at_end: bool,
    },
    /// `Ctrl-e` — rewrite the selected block(s): a content-only change (the trailing newline
    /// and separators survive), landing in Insert on the emptied line.
    ReadChange,
    /// `Ctrl-o`/`Ctrl-Alt-o` — open a new block below / above the focused one and enter
    /// Insert (the editor's open-line at block grain). What gets opened is read off the
    /// focused block: a sibling item inside a list, a paragraph elsewhere.
    ReadOpenBlock {
        above: bool,
    },
    /// Move the selection past a sibling: `Ctrl-j`/`k` in Read (block grain, with `Ctrl-Alt`
    /// aliases so editor muscle memory lands too), `Ctrl-Alt-j`/`k` in the editor
    /// (blank-line paragraphs, any file type). One atomic server edit
    /// (docs/markdown-view.md §12).
    MoveBlock {
        down: bool,
        unit: BlockUnit,
    },
    /// `Ctrl-x` in Read — cut the selected block(s): around-block removal, the blocks'
    /// source to the clipboard.
    ReadCutBlock,
    /// `Ctrl-d` in Read — delete the focused/selected block(s): the same around-block
    /// removal as `Ctrl-x` with the clipboard left alone, which is exactly how the editor's
    /// `Ctrl-d` (delete selection) stands to its `Ctrl-x` (cut selection).
    ReadDeleteBlock,
    /// `Ctrl-v`/`Ctrl-Alt-v` in Read — paste the clipboard as its own block before the
    /// selection / in place of the selected block(s).
    ReadPasteBlock {
        replace: bool,
    },
    /// `Ctrl-l`/`Ctrl-h` in Read — the indent chords at block grain: demote/promote a
    /// heading, nest/un-nest a list item (subtree riding along); toasts elsewhere.
    ReadBlockDepth {
        deeper: bool,
    },
}

impl Action {
    /// Whether this chord arms a capture (the next keystroke is data, not a binding).
    pub fn awaits_key(&self) -> bool {
        matches!(
            self,
            Action::BeginFind { .. }
                | Action::BeginSneak { .. }
                | Action::BeginSurround(_)
                | Action::BeginTransform
        )
    }

    /// Whether `.` replays this action: every cursor/selection motion (absolute ones included)
    /// plus the selection motions and the cursor-jumping navigations (symbol / hunk / diagnostic
    /// next-prev); never edits, scroll, or the non-motion selection ops. (`SearchCycle` joins when
    /// search lands.) The hunk/diagnostic jumps re-key off the live cursor, so a repeat steps to
    /// the next one each press.
    pub fn is_repeatable(&self) -> bool {
        matches!(
            self,
            Action::MoveChar(_)
                | Action::MoveWordBack { .. }
                | Action::MoveWordEnd { .. }
                | Action::MoveVisualLine(_)
                | Action::MoveLogicalLine(_)
                | Action::MoveLineStart
                | Action::MoveLineEnd
                | Action::MoveLineFirstNonblank
                | Action::MoveLogicalLineFirstNonblank(_)
                | Action::GotoLine { .. }
                | Action::MatchBracket { .. }
                | Action::PageMotion { .. }
                | Action::NavUnit(_)
                | Action::SelectWord { .. }
                | Action::SelectLine(_)
                | Action::TreeExpand
                | Action::TreeContract
                | Action::SearchCycle(_)
                | Action::JumplistStep(_)
                | Action::JumplistStepInFile(_)
                | Action::NextHunk
                | Action::PrevHunk
                | Action::NextDiagnostic
                | Action::PrevDiagnostic
        )
    }
}

pub struct Binding {
    /// Kept for table-shape parity; `lookup` selects the table directly so it never reads
    /// this — the help overlay does.
    pub ctx: KeyContext,
    pub code: KeyCode,
    pub mods: ModPattern,
    pub action: Action,
    /// Help-overlay section this binding lists under. Empty = hidden from help (an alias
    /// or internal binding).
    pub group: &'static str,
    /// One-line help description.
    pub desc: &'static str,
}

impl Binding {
    fn matches(&self, code: KeyCode, mods: Mods) -> bool {
        self.code == code && self.mods.matches(mods)
    }

    pub fn is_alt(&self) -> bool {
        self.mods.display_mods().alt
    }

    /// Whether `self` and `other` are the same key differing by *exactly* the Alt modifier —
    /// the pairing the help overlay folds into one "X / Alt-X" row (e.g. `h`/`Alt-h`,
    /// `Ctrl-z`/`Ctrl-Alt-z`). Same code but a *different* modifier is not a pair.
    pub fn is_alt_pair(&self, other: &Binding) -> bool {
        let (a, b) = (self.mods.display_mods(), other.mods.display_mods());
        self.code == other.code && a.ctrl == b.ctrl && a.shift == b.shift && a.alt != b.alt
    }

    /// Render the chord for the help overlay, e.g. `Alt-h`, `Ctrl-z`, `Space f`, `↑`. Chords
    /// that arm a capture get a trailing `␣` placeholder (`f ␣`) to signal one more
    /// keystroke is expected.
    pub fn key_label(&self) -> String {
        let mut s = String::new();
        if self.ctx == KeyContext::Leader {
            s.push_str("Space ");
        }
        let m = self.mods.display_mods();
        if m.ctrl {
            s.push_str("Ctrl-");
        }
        if m.alt {
            s.push_str("Alt-");
        }
        s.push_str(&code_label(self.code));
        if self.action.awaits_key() {
            // U+2423 OPEN BOX — an empty "a key goes here" slot.
            s.push_str(" ␣");
        }
        s
    }
}

fn code_label(code: KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "Shift-Tab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
    }
}

/// Every binding, in context order — for the help overlay.
pub fn all() -> impl Iterator<Item = &'static Binding> {
    [
        KeyContext::Normal,
        KeyContext::Global,
        KeyContext::Insert,
        KeyContext::Search,
        KeyContext::Read,
        KeyContext::Leader,
    ]
    .into_iter()
    .flat_map(|cx| table(cx).iter())
}

/// First binding in `ctx`'s table whose chord matches, scanning in declaration order.
/// The binding table for a context, in declaration (lookup) order.
pub fn table(ctx: KeyContext) -> &'static [Binding] {
    match ctx {
        KeyContext::Normal => NORMAL,
        KeyContext::Insert => INSERT,
        KeyContext::Search => SEARCH,
        KeyContext::Read => READ,
        KeyContext::Leader => LEADER,
        KeyContext::Global => GLOBAL,
    }
}

pub fn lookup(ctx: KeyContext, code: KeyCode, mods: Mods) -> Option<&'static Binding> {
    table(ctx).iter().find(|b| b.matches(code, mods))
}

/// Curated section order for the keybindings picker: getting-around → changing-text → finding →
/// tools → app. This is deliberately independent of the binding tables' declaration order, so
/// reordering `bind!` lines only shuffles rows *within* a group, never the section order here.
/// Every group produced by the tables must appear exactly once below — the
/// `keybinding_sections_follow_the_curated_group_order` test enforces both directions (no missing
/// group, no stale entry).
const GROUP_ORDER: &[&str] = &[
    "Motion",
    "Navigation",
    "Scroll", // getting around
    "Selection",
    "Mode",
    "Read", // the markdown reading view
    "Edit",
    "Clipboard", // changing text
    "Search",    // finding
    "Files",
    "Code",
    "Git", // tools
    "Workspace",
    "App", // app-level
];

/// Every user-facing binding as a Keybindings-picker row: one entry per binding, bucketed by
/// group — the picker renders one section header per group (grep-style), so a group's rows must
/// be a contiguous run. Groups follow [`GROUP_ORDER`]; within a group, rows keep mode-major
/// order (Normal, the shared `Any` keys, Insert, Search, Application — so unlike the old tabbed
/// help dialog the `Global` keys appear *once*, as mode `Any`, rather than folded into both
/// Normal and Insert). Bindings with no `group` (internal aliases) and the leader-trigger itself
/// are omitted. Built straight from the binding tables and shipped on `picker/view`, so every
/// client's picker shows exactly its own keymap.
pub fn keybinding_entries() -> Vec<aether_protocol::picker::KeybindingEntry> {
    const MODES: [(&str, KeyContext); 6] = [
        ("Normal", KeyContext::Normal),
        ("Any", KeyContext::Global),
        ("Insert", KeyContext::Insert),
        ("Search", KeyContext::Search),
        ("Read", KeyContext::Read),
        ("Application", KeyContext::Leader),
    ];
    // One bucket per group, filled in scan order; reordered to GROUP_ORDER just before flattening.
    // A Vec scan beats a map: ~15 groups, built once per open.
    let mut groups: Vec<(&str, Vec<aether_protocol::picker::KeybindingEntry>)> = Vec::new();
    for (mode, cx) in MODES {
        for b in table(cx) {
            if !b.group.is_empty() && !matches!(b.action, Action::BeginLeader) {
                let entry = aether_protocol::picker::KeybindingEntry {
                    group: b.group.to_string(),
                    desc: b.desc.to_string(),
                    mode: mode.to_string(),
                    keys: b.key_label(),
                };
                match groups.iter_mut().find(|(g, _)| *g == b.group) {
                    Some((_, rows)) => rows.push(entry),
                    None => groups.push((b.group, vec![entry])),
                }
            }
        }
    }
    // Section order follows GROUP_ORDER, not the tables. A group absent from GROUP_ORDER sorts
    // last; the guard test forbids that, so in practice every group has an explicit position.
    groups.sort_by_key(|(g, _)| {
        GROUP_ORDER
            .iter()
            .position(|x| x == g)
            .unwrap_or(usize::MAX)
    });
    groups.into_iter().flat_map(|(_, rows)| rows).collect()
}

/// What a key does to an *open hover popover*.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HoverAction {
    /// Pan the popover (vertical only).
    Scroll { dir: ScrollDir, unit: ScrollUnit },
    /// Copy the whole popover to the clipboard.
    Copy,
}

/// Resolve a key for an open hover popover, reusing the *same* Normal-context bindings the editor
/// uses — `Ctrl-y` → [`Action::Copy`], the arrow / page keys → [`Action::Scroll`]. This keeps the
/// popover's keys in lockstep with the real keymap (change a binding once and every client's popover
/// follows) instead of each shell hardcoding the chords. Returns `None` for any other key, on which
/// the shell dismisses the popover. Only vertical scrolls apply (a popover has no horizontal pan).
pub fn hover_action(code: KeyCode, mods: Mods) -> Option<HoverAction> {
    match lookup(KeyContext::Normal, code, mods).map(|b| &b.action) {
        Some(Action::Scroll {
            dir: dir @ (ScrollDir::Up | ScrollDir::Down),
            unit,
        }) => Some(HoverAction::Scroll {
            dir: *dir,
            unit: *unit,
        }),
        Some(Action::Copy) => Some(HoverAction::Copy),
        _ => None,
    }
}

use Action as A;
use KeyContext::{Global as G, Insert as I, Leader as L, Normal as N, Read as R};
use ModPattern::{Any, Exact, IgnoreShift};

const fn ch(c: char) -> KeyCode {
    KeyCode::Char(c)
}

macro_rules! bind {
    ($ctx:expr, $code:expr, $mods:expr, $action:expr) => {
        bind!($ctx, $code, $mods, $action, "", "")
    };
    ($ctx:expr, $code:expr, $mods:expr, $action:expr, $group:literal, $desc:literal) => {
        Binding {
            ctx: $ctx,
            code: $code,
            mods: $mods,
            action: $action,
            group: $group,
            desc: $desc,
        }
    };
}

#[rustfmt::skip]
static NORMAL: &[Binding] = &[
    // ---- meta / selection ----
    bind!(N, KeyCode::Esc, Any, A::DropSearch, "Search", "Clear the active search"),
    bind!(N, ch(','), Exact(Mods::NONE), A::CollapseSelection, "Selection", "Collapse selection"),
    bind!(N, ch('r'), Exact(Mods::NONE), A::SwapAnchor { forward_only: false }, "Selection", "Reverse selection (swap cursor and anchor)"),
    bind!(N, ch('r'), Exact(Mods::ALT), A::SwapAnchor { forward_only: true }, "Selection", "Orient selection forward (cursor to end)"),
    bind!(N, ch('q'), Exact(Mods::NONE), A::TreeExpand, "Selection", "Expand selection to parent syntax node"),
    bind!(N, ch('q'), Exact(Mods::ALT), A::TreeContract, "Selection", "Contract selection to child syntax node"),
    bind!(N, ch('z'), Exact(Mods::ALT), A::MotionRedo, "Selection", "Redo cursor/selection motion"),
    bind!(N, ch('z'), Exact(Mods::NONE), A::MotionUndo, "Selection", "Undo cursor/selection motion"),
    bind!(N, ch('.'), Exact(Mods::NONE), A::RepeatMotion, "Selection", "Repeat last motion"),

    // ---- motions: chars / lines ----
    bind!(N, KeyCode::Home, Any, A::MoveLineStart, "Motion", "Logical line start"),
    bind!(N, KeyCode::End, Any, A::MoveLineEnd, "Motion", "Logical line end"),
    bind!(N, ch('h'), IgnoreShift(Mods::ALT), A::MoveLineFirstNonblank, "Motion", "First non-blank of line"),
    bind!(N, ch('h'), IgnoreShift(Mods::NONE), A::MoveChar(Direction::Backward), "Motion", "Character left"),
    bind!(N, ch('l'), IgnoreShift(Mods::ALT), A::MoveLineEnd, "Motion", "End of line"),
    bind!(N, ch('l'), IgnoreShift(Mods::NONE), A::MoveChar(Direction::Forward), "Motion", "Character right"),
    bind!(N, ch('k'), IgnoreShift(Mods::ALT), A::MoveVisualLine(VerticalDirection::Up), "Motion", "Visual row up"),
    bind!(N, ch('k'), IgnoreShift(Mods::NONE), A::MoveLogicalLine(Direction::Backward), "Motion", "Logical line up"),
    bind!(N, ch('j'), IgnoreShift(Mods::ALT), A::MoveVisualLine(VerticalDirection::Down), "Motion", "Visual row down"),
    bind!(N, ch('j'), IgnoreShift(Mods::NONE), A::MoveLogicalLine(Direction::Forward), "Motion", "Logical line down"),
    bind!(N, ch('p'), IgnoreShift(Mods::ALT), A::MoveLogicalLineFirstNonblank(Direction::Backward), "Motion", "First non-blank of previous line"),
    bind!(N, ch('p'), IgnoreShift(Mods::NONE), A::MoveLogicalLineFirstNonblank(Direction::Forward), "Motion", "First non-blank of next line"),
    bind!(N, ch('0'), IgnoreShift(Mods::NONE), A::MoveLineStart, "Motion", "Logical line start"),

    // ---- motions: cursor half-page ----
    bind!(N, ch('v'), IgnoreShift(Mods::NONE), A::PageMotion { dir: VerticalDirection::Down, half: true }, "Motion", "Cursor down half a page"),
    bind!(N, ch('v'), IgnoreShift(Mods::ALT), A::PageMotion { dir: VerticalDirection::Up, half: true }, "Motion", "Cursor up half a page"),

    // ---- motions: words ----
    bind!(N, ch('w'), IgnoreShift(Mods::ALT), A::SelectWord { boundary: WordBoundary::BigWord }, "Selection", "Select big word"),
    bind!(N, ch('w'), IgnoreShift(Mods::NONE), A::SelectWord { boundary: WordBoundary::Word }, "Selection", "Select word"),
    bind!(N, ch('b'), IgnoreShift(Mods::ALT), A::MoveWordBack { boundary: WordBoundary::BigWord }, "Motion", "Big word backward"),
    bind!(N, ch('b'), IgnoreShift(Mods::NONE), A::MoveWordBack { boundary: WordBoundary::Word }, "Motion", "Small word backward"),
    bind!(N, ch('e'), IgnoreShift(Mods::ALT), A::MoveWordEnd { dir: Direction::Forward, boundary: WordBoundary::BigWord }, "Motion", "Big word end"),
    bind!(N, ch('e'), IgnoreShift(Mods::NONE), A::MoveWordEnd { dir: Direction::Forward, boundary: WordBoundary::Word }, "Motion", "Small word end"),

    // ---- motions: find char ----
    bind!(N, ch('f'), IgnoreShift(Mods::ALT), A::BeginFind { dir: Direction::Backward, till: false }, "Motion", "Find character backward"),
    bind!(N, ch('f'), IgnoreShift(Mods::NONE), A::BeginFind { dir: Direction::Forward, till: false }, "Motion", "Find character forward"),
    bind!(N, ch('t'), IgnoreShift(Mods::ALT), A::BeginFind { dir: Direction::Backward, till: true }, "Motion", "Till character backward"),
    bind!(N, ch('t'), IgnoreShift(Mods::NONE), A::BeginFind { dir: Direction::Forward, till: true }, "Motion", "Till character forward"),
    bind!(N, ch('s'), IgnoreShift(Mods::NONE), A::BeginSneak { big: false }, "Motion", "Sneak to word"),
    bind!(N, ch('s'), IgnoreShift(Mods::ALT), A::BeginSneak { big: true }, "Motion", "Sneak to big word"),

    // ---- motions: brackets / nav units / goto ----
    bind!(N, ch('m'), IgnoreShift(Mods::NONE), A::MatchBracket { inner: false }, "Motion", "Matching bracket"),
    bind!(N, ch('m'), IgnoreShift(Mods::ALT), A::MatchBracket { inner: true }, "Motion", "Inner matching bracket"),
    bind!(N, ch('o'), IgnoreShift(Mods::NONE), A::NavUnit(Direction::Forward), "Navigation", "Next symbol"),
    bind!(N, ch('o'), IgnoreShift(Mods::ALT), A::NavUnit(Direction::Backward), "Navigation", "Previous symbol"),
    bind!(N, ch('g'), IgnoreShift(Mods::ALT), A::GotoLine { last: true }, "Motion", "Go to line from end (count, default last)"),
    bind!(N, ch('g'), IgnoreShift(Mods::NONE), A::GotoLine { last: false }, "Motion", "Go to line (count, default 1)"),
    bind!(N, KeyCode::Enter, Exact(Mods::NONE), A::GotoDefinition, "Code", "Go to definition"),

    // ---- cursor-local git / diagnostic navigation (the list pickers live under Space) ----
    bind!(N, ch('c'), IgnoreShift(Mods::NONE), A::NextHunk, "Git", "Next change (hunk)"),
    bind!(N, ch('c'), IgnoreShift(Mods::ALT), A::PrevHunk, "Git", "Previous change (hunk)"),
    bind!(N, ch('d'), IgnoreShift(Mods::NONE), A::NextDiagnostic, "Code", "Next diagnostic"),
    bind!(N, ch('d'), IgnoreShift(Mods::ALT), A::PrevDiagnostic, "Code", "Previous diagnostic"),

    // ---- line selection ----
    bind!(N, ch('x'), IgnoreShift(Mods::NONE), A::SelectLine(Direction::Forward), "Selection", "Select line downward"),
    bind!(N, ch('x'), IgnoreShift(Mods::ALT), A::SelectLine(Direction::Backward), "Selection", "Select line upward"),
    // `%` is Shift-5, so the Shift modifier rides along (like `?`); IgnoreShift matches it in all
    // three clients (iced/web report `shift: true`, some terminals do too).
    bind!(N, ch('%'), IgnoreShift(Mods::NONE), A::SelectAll, "Selection", "Select whole buffer"),

    // ---- mode transitions ----
    bind!(N, ch('i'), Exact(Mods::NONE), A::EnterInsert(InsertWhere::SelectionStart), "Mode", "Insert at selection start"),
    bind!(N, ch('a'), Exact(Mods::NONE), A::EnterInsert(InsertWhere::SelectionEnd), "Mode", "Insert at selection end"),
    bind!(N, ch('i'), Exact(Mods::ALT), A::EnterInsert(InsertWhere::FirstLineStart), "Mode", "Insert at first non-blank of line"),
    bind!(N, ch('a'), Exact(Mods::ALT), A::EnterInsert(InsertWhere::LastLineEnd), "Mode", "Insert at last line end"),

    // ---- viewport scroll ----
    bind!(N, KeyCode::PageDown, Any, A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Page }, "Scroll", "Scroll page down"),
    bind!(N, KeyCode::PageUp, Any, A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Page }, "Scroll", "Scroll page up"),
    // Only a bare arrow (one line) and Alt-arrow (half page) scroll; Shift/Ctrl arrows do nothing.
    // Exact patterns keep these disjoint, so declaration order here doesn't affect dispatch.
    bind!(N, KeyCode::Up, Exact(Mods::ALT), A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Half }, "Scroll", "Scroll half page up"),
    bind!(N, KeyCode::Down, Exact(Mods::ALT), A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Half }, "Scroll", "Scroll half page down"),
    bind!(N, KeyCode::Up, Exact(Mods::NONE), A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Line }, "Scroll", "Scroll up one line"),
    bind!(N, KeyCode::Down, Exact(Mods::NONE), A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Line }, "Scroll", "Scroll down one line"),
    bind!(N, KeyCode::Left, Any, A::Scroll { dir: ScrollDir::Left, unit: ScrollUnit::Line }, "Scroll", "Scroll left one column"),
    bind!(N, KeyCode::Right, Any, A::Scroll { dir: ScrollDir::Right, unit: ScrollUnit::Line }, "Scroll", "Scroll right one column"),
    bind!(N, ch(';'), Exact(Mods::NONE), A::PlaceCursor(ViewportPlace::Upper), "Scroll", "Cursor near top"),
    bind!(N, ch(';'), Exact(Mods::ALT), A::PlaceCursor(ViewportPlace::Lower), "Scroll", "Cursor near bottom"),

    // ---- navigation history (cross-file back/forward) ----
    bind!(N, KeyCode::Backspace, Exact(Mods::NONE), A::NavBack, "Navigation", "Jump back (history)"),
    bind!(N, KeyCode::Backspace, Exact(Mods::ALT), A::NavForward, "Navigation", "Jump forward (history)"),
    bind!(N, ch(']'), Exact(Mods::NONE), A::JumplistStep(Direction::Forward), "Navigation", "Next jumplist entry"),
    bind!(N, ch('['), Exact(Mods::NONE), A::JumplistStep(Direction::Backward), "Navigation", "Previous jumplist entry"),
    // `}`/`{` (Shift-bracket) rather than `Alt-]`/`Alt-[`: an Alt-bracket sends the bytes `ESC [` /
    // `ESC ]` — the CSI / OSC introducers — so on terminals without the kitty keyboard protocol
    // (Terminal.app, xterm, tmux, …) `Alt-[` is swallowed and `Alt-]` loses its Alt. `}`/`{` are
    // literal bytes, reliable on every terminal. `IgnoreShift` because the char already encodes the
    // Shift; shells differ on whether they also report the modifier (mirrors the `?` binding).
    bind!(N, ch('}'), IgnoreShift(Mods::NONE), A::JumplistStepInFile(Direction::Forward), "Navigation", "Next jumplist entry in this file"),
    bind!(N, ch('{'), IgnoreShift(Mods::NONE), A::JumplistStepInFile(Direction::Backward), "Navigation", "Previous jumplist entry in this file"),

    // ---- delete / search ----
    bind!(N, KeyCode::Delete, Any, A::DeleteSelection, "Edit", "Delete selection"),
    bind!(N, ch('/'), IgnoreShift(Mods::NONE), A::EnterSearch, "Search", "Search"),
    bind!(N, ch('/'), Exact(Mods::ALT), A::SearchFromSelection, "Search", "Search for selection"),
    bind!(N, ch('?'), IgnoreShift(Mods::NONE), A::EnterSearchToCursor, "Search", "Select from cursor to match"),
    bind!(N, ch('n'), IgnoreShift(Mods::ALT), A::SearchCycle(Direction::Backward), "Search", "Previous match"),
    bind!(N, ch('n'), IgnoreShift(Mods::NONE), A::SearchCycle(Direction::Forward), "Search", "Next match"),

    // ---- selection editing / clipboard ----
    bind!(N, ch('e'), Exact(Mods::CTRL), A::Change, "Edit", "Change selection"),
    bind!(N, ch('d'), Exact(Mods::CTRL), A::DeleteSelection, "Edit", "Delete selection"),
    bind!(N, ch('c'), Exact(Mods::CTRL), A::Copy, "Clipboard", "Copy selection"),
    bind!(N, ch('x'), Exact(Mods::CTRL), A::Cut, "Clipboard", "Cut selection"),
    bind!(N, ch('x'), Exact(Mods::CTRL_ALT), A::CutChange, "Clipboard", "Cut selection and insert"),
    bind!(N, ch('v'), Exact(Mods::CTRL), A::Paste, "Clipboard", "Paste before selection"),
    bind!(N, ch('v'), Exact(Mods::CTRL_ALT), A::ReplaceClipboard, "Clipboard", "Replace selection with clipboard"),
    bind!(N, ch('s'), Exact(Mods::CTRL_ALT), A::Unsurround(SurroundTarget::Selection), "Edit", "Unsurround selection"),
    bind!(N, ch('s'), Exact(Mods::CTRL), A::BeginSurround(SurroundTarget::Selection), "Edit", "Surround selection"),
    bind!(N, ch('r'), Exact(Mods::CTRL), A::BeginTransform, "Edit", "Transform selection (u/l/i/r/m/c/p/s/k/w/t/n/d/x)"),
    bind!(N, ch('y'), Exact(Mods::CTRL), A::ToggleComment(CommentStyle::Line, SurroundTarget::Selection), "Edit", "Toggle line comment"),
    bind!(N, ch('y'), Exact(Mods::CTRL_ALT), A::ToggleComment(CommentStyle::Block, SurroundTarget::Selection), "Edit", "Toggle block comment"),

    // ---- reveal ----
    bind!(N, KeyCode::Tab, Exact(Mods::NONE), A::Hover, "Code", "Hover (type & docs)"),

    // ---- leaders ----
    bind!(N, ch(' '), Exact(Mods::NONE), A::BeginLeader, "Leader", "Space leader chord"),
];

#[rustfmt::skip]
static GLOBAL: &[Binding] = &[
    bind!(G, ch('z'), Exact(Mods::CTRL), A::Undo, "Edit", "Undo"),
    bind!(G, ch('z'), Exact(Mods::CTRL_ALT), A::Redo, "Edit", "Redo"),
    bind!(G, ch('j'), Exact(Mods::CTRL), A::MoveLines(VerticalDirection::Down), "Edit", "Move line(s) down"),
    bind!(G, ch('k'), Exact(Mods::CTRL), A::MoveLines(VerticalDirection::Up), "Edit", "Move line(s) up"),
    // The paragraph-grain sibling of Ctrl-j/k: swap the blank-line-delimited chunk under the
    // selection with its neighbour, gap and all — any file type (docs/markdown-view.md §12).
    bind!(G, ch('j'), Exact(Mods::CTRL_ALT), A::MoveBlock { down: true, unit: BlockUnit::Paragraph }, "Edit", "Move paragraph down"),
    bind!(G, ch('k'), Exact(Mods::CTRL_ALT), A::MoveBlock { down: false, unit: BlockUnit::Paragraph }, "Edit", "Move paragraph up"),
    // Join/un-join are exact mirrors on `g`: join deletes "\n"+indent parking the cursor on the
    // seam; un-join inserts them back, cursor staying before the break — so the pair ping-pongs.
    // `Ctrl-Alt-g` survives legacy key encoding (ESC + 0x07 — `g` is not a sequence introducer,
    // unlike the Alt-bracket chords).
    bind!(G, ch('g'), Exact(Mods::CTRL), A::JoinLines, "Edit", "Join lines"),
    bind!(G, ch('g'), Exact(Mods::CTRL_ALT), A::UnjoinLines, "Edit", "Un-join lines"),
    bind!(G, ch('l'), Exact(Mods::CTRL), A::Indent, "Edit", "Indent"),
    bind!(G, ch('h'), Exact(Mods::CTRL), A::Dedent, "Edit", "Dedent"),
    // Mode-agnostic (Global so they fire in Insert too); the mode-specific Change/ChangeLine
    // pair sits on Ctrl-e in NORMAL/INSERT. Global is checked before Normal and Insert, so these
    // win there; Read skips Global and re-declares them below, so the pair means the same thing
    // in every mode. The *value* they adjust is whatever the buffer has — a number, or a task
    // checkbox in markdown: one pair of keys, one meaning, "adjust what's under the cursor".
    bind!(G, ch('a'), Exact(Mods::CTRL), A::IncrementNumber, "Edit", "Increment number / check task"),
    bind!(G, ch('a'), Exact(Mods::CTRL_ALT), A::DecrementNumber, "Edit", "Decrement number / uncheck task"),
    bind!(G, ch('o'), Exact(Mods::CTRL), A::OpenLineBelow, "Edit", "Open line below"),
    bind!(G, ch('o'), Exact(Mods::CTRL_ALT), A::OpenLineAbove, "Edit", "Open line above"),
    // Mode-agnostic edits (same action in Normal and Insert) live here rather than being split
    // line-vs-selection, so one binding serves both modes.
    bind!(G, ch('f'), Exact(Mods::CTRL), A::Format, "Code", "Format document"),
];

#[rustfmt::skip]
static INSERT: &[Binding] = &[
    bind!(I, KeyCode::Esc, Any, A::LeaveInsert, "Mode", "Leave insert mode"),
    bind!(I, KeyCode::Backspace, Any, A::Backspace, "Edit", "Delete character before cursor"),
    bind!(I, KeyCode::Delete, Any, A::DeletePoint, "Edit", "Delete character at cursor"),
    bind!(I, KeyCode::Enter, Any, A::NewlineIndent, "Edit", "Newline and indent"),
    bind!(I, KeyCode::Tab, Any, A::InsertTab, "Edit", "Indent to next tab stop"),
    bind!(I, KeyCode::Left, Any, A::MoveChar(Direction::Backward), "Motion", "Cursor left"),
    bind!(I, KeyCode::Right, Any, A::MoveChar(Direction::Forward), "Motion", "Cursor right"),
    bind!(I, KeyCode::Up, Any, A::MoveVisualLine(VerticalDirection::Up), "Motion", "Cursor up"),
    bind!(I, KeyCode::Down, Any, A::MoveVisualLine(VerticalDirection::Down), "Motion", "Cursor down"),
    // Line-scoped editing mirrors Normal's selection-scoped Ctrl column on the same keys (Insert
    // has no selection to act on); the mode-agnostic Ctrl-f comes from GLOBAL.
    bind!(I, ch('e'), Exact(Mods::CTRL), A::ChangeLine, "Edit", "Change line"),
    bind!(I, ch('d'), Exact(Mods::CTRL), A::DeleteLine, "Edit", "Delete line"),
    bind!(I, ch('c'), Exact(Mods::CTRL), A::CopyLine, "Clipboard", "Copy line"),
    bind!(I, ch('x'), Exact(Mods::CTRL), A::CutLine, "Clipboard", "Cut line"),
    bind!(I, ch('v'), Exact(Mods::CTRL), A::PasteAtCursor, "Clipboard", "Paste at cursor"),
    bind!(I, ch('v'), Exact(Mods::CTRL_ALT), A::ReplaceLineClipboard, "Clipboard", "Replace line with clipboard"),
    bind!(I, ch('s'), Exact(Mods::CTRL_ALT), A::Unsurround(SurroundTarget::Line), "Edit", "Unsurround line"),
    bind!(I, ch('s'), Exact(Mods::CTRL), A::BeginSurround(SurroundTarget::Line), "Edit", "Surround line"),
    bind!(I, ch('r'), Exact(Mods::CTRL), A::BeginTransform, "Edit", "Transform identifier (u/l/i/r/m/c/p/s/k/w/t/n/d/x)"),
    bind!(I, ch('y'), Exact(Mods::CTRL), A::ToggleComment(CommentStyle::Line, SurroundTarget::Line), "Edit", "Toggle line comment"),
    bind!(I, ch('y'), Exact(Mods::CTRL_ALT), A::ToggleComment(CommentStyle::Block, SurroundTarget::Line), "Edit", "Toggle block comment on line"),
];

#[rustfmt::skip]
static SEARCH: &[Binding] = &[
    bind!(KeyContext::Search, KeyCode::Esc, Any, A::SearchAbort, "Search", "Abort search"),
    bind!(KeyContext::Search, KeyCode::Enter, Any, A::SearchCommit, "Search", "Commit search"),
    // Up/Down browse the query history (docs/input-history.md) — the same chord in every overlay
    // text input (the grep query, the glob/path chip editors). They're safe here and there because
    // no shell's text input claims a bare arrow-up, and because the *list* keys in the pickers are
    // Alt-k/j. Alt-k/j stay as an unlisted alias for the muscle memory that predates this.
    bind!(KeyContext::Search, KeyCode::Up, Exact(Mods::NONE), A::SearchHistoryPrev, "Search", "Previous query in history"),
    bind!(KeyContext::Search, KeyCode::Down, Exact(Mods::NONE), A::SearchHistoryNext, "Search", "Next query in history"),
    bind!(KeyContext::Search, ch('k'), Exact(Mods::ALT), A::SearchHistoryPrev, "", ""),
    bind!(KeyContext::Search, ch('j'), Exact(Mods::ALT), A::SearchHistoryNext, "", ""),
    // Match-option toggles, mirroring the grep picker's chip chords (Alt-c / Alt-w / Alt-e).
    bind!(KeyContext::Search, ch('c'), Exact(Mods::ALT), A::SearchToggleCase, "Search", "Cycle case sensitivity"),
    bind!(KeyContext::Search, ch('w'), Exact(Mods::ALT), A::SearchToggleWord, "Search", "Toggle whole-word match"),
    bind!(KeyContext::Search, ch('e'), Exact(Mods::ALT), A::SearchToggleRegex, "Search", "Toggle regex"),
    // Text entry (chars, Backspace, Left/Right caret) is owned by each shell's search input, which
    // syncs the value via `search_set_query`; only the command keys above live in this table.
];

/// The markdown reading view's keys (docs/markdown-view.md §2.3). Where the editor already has a
/// key for the concept, Read reuses it — `o` heading-steps like symbol nav, `g`/`Alt-g` are the
/// ends pair, `j`/`k` move the (reading) cursor while the arrows scroll, `Ctrl-c` copies (the
/// editor's clipboard chord — acting on the focused element, since Read has no selection), search
/// and jumplist keys are verbatim. Deliberately contains no editing action (see
/// [`KeyContext::Read`]).
#[rustfmt::skip]
static READ: &[Binding] = &[
    bind!(R, KeyCode::Esc, Any, A::DropSearch, "Search", "Clear the active search"),

    // ---- the reading cursor ----
    bind!(R, ch('j'), IgnoreShift(Mods::NONE), A::ReadStep(Direction::Forward), "Read", "Focus next element"),
    bind!(R, ch('k'), IgnoreShift(Mods::NONE), A::ReadStep(Direction::Backward), "Read", "Focus previous element"),
    // Unlisted muscle-memory aliases (the Ctrl-Alt-j/k pattern, §12.1.7): the editor's other
    // line-step motions — `p`/`Alt-p`'s first-non-blank step, `Alt-j`/`k`'s visual-row step —
    // all collapse into the element step at block grain, so the keys land where the hand
    // expects. IgnoreShift keeps Shift as the extend modifier, exactly as on `j`/`k`.
    bind!(R, ch('p'), IgnoreShift(Mods::NONE), A::ReadStep(Direction::Forward)),
    bind!(R, ch('p'), IgnoreShift(Mods::ALT), A::ReadStep(Direction::Backward)),
    bind!(R, ch('j'), IgnoreShift(Mods::ALT), A::ReadStep(Direction::Forward)),
    bind!(R, ch('k'), IgnoreShift(Mods::ALT), A::ReadStep(Direction::Backward)),
    bind!(R, ch('l'), IgnoreShift(Mods::NONE), A::ReadStepLink(Direction::Forward), "Read", "Focus next link in block"),
    bind!(R, ch('h'), IgnoreShift(Mods::NONE), A::ReadStepLink(Direction::Backward), "Read", "Focus previous link in block"),
    bind!(R, KeyCode::Tab, Exact(Mods::NONE), A::ReadShowTarget, "Read", "Show link/image target"),
    bind!(R, ch('o'), IgnoreShift(Mods::NONE), A::ReadStepHeading(Direction::Forward), "Read", "Next heading"),
    bind!(R, ch('o'), IgnoreShift(Mods::ALT), A::ReadStepHeading(Direction::Backward), "Read", "Previous heading"),
    bind!(R, ch('g'), IgnoreShift(Mods::NONE), A::ReadEnds { last: false }, "Read", "First element"),
    bind!(R, ch('g'), IgnoreShift(Mods::ALT), A::ReadEnds { last: true }, "Read", "Last element"),
    bind!(R, KeyCode::Enter, Exact(Mods::NONE), A::ReadActivate, "Read", "Follow link / open image / jump to footnote"),
    bind!(R, KeyCode::Enter, Exact(Mods::CTRL), A::ReadActivateNewWindow, "Read", "Open link in a new window/tab"),
    bind!(R, ch('c'), Exact(Mods::CTRL), A::ReadCopy, "Read", "Copy selection, link URL, or element source"),

    // ---- block selection (docs/markdown-view.md §12; the editor's `x` line-select machine
    // at block grain: plain presses walk, Shift grows — and Shift-j/k extend through
    // read_step) ----
    bind!(R, ch('x'), IgnoreShift(Mods::NONE), A::ReadSelectBlock(Direction::Forward), "Read", "Select block downward (Shift extends)"),
    bind!(R, ch('x'), IgnoreShift(Mods::ALT), A::ReadSelectBlock(Direction::Backward), "Read", "Select block upward (Shift extends)"),
    // The editor's own reverse/orient pair, unchanged: swapping the ends moves the bar to the
    // other edge of the block range, and every extension key already grows from the cursor's
    // end — so `r` is what re-aims `x`/`Shift-j`/`Shift-k` at the top of a selection.
    bind!(R, ch('r'), Exact(Mods::NONE), A::SwapAnchor { forward_only: false }, "Read", "Reverse selection (swap cursor and anchor)"),
    bind!(R, ch('r'), Exact(Mods::ALT), A::SwapAnchor { forward_only: true }, "Read", "Orient selection forward (cursor to end)"),
    // The editor's whole-buffer / collapse pair at block grain. A whole-buffer selection is
    // already whole-line normal form, so `%` needs no read-side math — every block selected,
    // front matter included (structural ops on it still refuse server-side, as they do for an
    // `x` selection swept over it). `,` drops a multi-block selection back to the cursor-end
    // block without moving — the only collapse that doesn't also step (`j`/`k`) or need a
    // whole-block span (`x`).
    bind!(R, ch('%'), IgnoreShift(Mods::NONE), A::SelectAll, "Read", "Select all blocks"),
    bind!(R, ch(','), Exact(Mods::NONE), A::CollapseSelection, "Read", "Collapse selection to the cursor's block"),

    // ---- undo/redo (the Global table's chords, whitelisted here — Read still skips Global,
    // whose other chords are edits; §12's curated-edit discipline) ----
    bind!(R, ch('z'), Exact(Mods::CTRL), A::Undo, "Edit", "Undo"),
    bind!(R, ch('z'), Exact(Mods::CTRL_ALT), A::Redo, "Edit", "Redo"),
    // The editor's adjust-the-value pair, re-declared because Read skips Global. Same action, so
    // the same key does the same thing on either side of `Space v`.
    bind!(R, ch('a'), Exact(Mods::CTRL), A::IncrementNumber, "Edit", "Check task item"),
    bind!(R, ch('a'), Exact(Mods::CTRL_ALT), A::DecrementNumber, "Edit", "Uncheck task item"),

    // ---- to the editor (§12 transitions; deliberately NOT recording a read-vs-source
    // preference — Space v remains the "I prefer source" signal) ----
    bind!(R, ch('i'), Exact(Mods::NONE), A::ReadInsert { at_end: false }, "Mode", "Edit: insert at block/selection start"),
    bind!(R, ch('a'), Exact(Mods::NONE), A::ReadInsert { at_end: true }, "Mode", "Edit: insert at block/selection end"),
    bind!(R, ch('e'), Exact(Mods::CTRL), A::ReadChange, "Edit", "Edit: rewrite selected block(s)"),
    bind!(R, ch('o'), Exact(Mods::CTRL), A::ReadOpenBlock { above: false }, "Edit", "Edit: open block below (list item in a list)"),
    bind!(R, ch('o'), Exact(Mods::CTRL_ALT), A::ReadOpenBlock { above: true }, "Edit", "Edit: open block above (list item in a list)"),

    // ---- structural edits (§12 phase 3: selection-relative server ops, atomic, one undo
    // entry each; the grain-relative reading of the editor's chords — Ctrl-j/k move the
    // block the way they move a line, Ctrl-h/l change depth the way they change indent) ----
    bind!(R, ch('j'), Exact(Mods::CTRL), A::MoveBlock { down: true, unit: BlockUnit::Block }, "Edit", "Move block(s) down"),
    bind!(R, ch('k'), Exact(Mods::CTRL), A::MoveBlock { down: false, unit: BlockUnit::Block }, "Edit", "Move block(s) up"),
    bind!(R, ch('j'), Exact(Mods::CTRL_ALT), A::MoveBlock { down: true, unit: BlockUnit::Block }),
    bind!(R, ch('k'), Exact(Mods::CTRL_ALT), A::MoveBlock { down: false, unit: BlockUnit::Block }),
    bind!(R, ch('x'), Exact(Mods::CTRL), A::ReadCutBlock, "Edit", "Cut block(s)"),
    bind!(R, ch('d'), Exact(Mods::CTRL), A::ReadDeleteBlock, "Edit", "Delete block(s)"),
    // The Delete key follows Normal's Delete → delete-selection at block grain: an unlisted
    // alias of Ctrl-d (`Any`, matching Normal's pattern for the key).
    bind!(R, KeyCode::Delete, Any, A::ReadDeleteBlock),
    bind!(R, ch('v'), Exact(Mods::CTRL), A::ReadPasteBlock { replace: false }, "Edit", "Paste as block"),
    bind!(R, ch('v'), Exact(Mods::CTRL_ALT), A::ReadPasteBlock { replace: true }, "Edit", "Paste replacing selected block(s)"),
    bind!(R, ch('l'), Exact(Mods::CTRL), A::ReadBlockDepth { deeper: true }, "Edit", "Deepen: demote heading / nest item / quote"),
    bind!(R, ch('h'), Exact(Mods::CTRL), A::ReadBlockDepth { deeper: false }, "Edit", "Shallow: promote heading / un-nest item / unquote"),

    // ---- coarse reading-position jumps + view placement (the editor's own keys) ----
    // `v` rides the editor's visual-line page motion: the jump distance is measured in the
    // *editor's* wrap geometry (best-effort in read space — §2.7's contract), but the landing
    // is always framed by the focus reveal.
    bind!(R, ch('v'), IgnoreShift(Mods::NONE), A::PageMotion { dir: VerticalDirection::Down, half: true }, "Motion", "Reading position down half a page"),
    bind!(R, ch('v'), IgnoreShift(Mods::ALT), A::PageMotion { dir: VerticalDirection::Up, half: true }, "Motion", "Reading position up half a page"),
    bind!(R, ch(';'), Exact(Mods::NONE), A::PlaceCursor(ViewportPlace::Upper), "Scroll", "Focused element near top"),
    bind!(R, ch(';'), Exact(Mods::ALT), A::PlaceCursor(ViewportPlace::Lower), "Scroll", "Focused element near bottom"),

    // ---- scroll (without moving focus; mirrors the editor's scroll rows) ----
    bind!(R, KeyCode::PageDown, Any, A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Page }, "Scroll", "Scroll page down"),
    bind!(R, KeyCode::PageUp, Any, A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Page }, "Scroll", "Scroll page up"),
    bind!(R, KeyCode::Up, Exact(Mods::ALT), A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Half }, "Scroll", "Scroll half page up"),
    bind!(R, KeyCode::Down, Exact(Mods::ALT), A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Half }, "Scroll", "Scroll half page down"),
    bind!(R, KeyCode::Up, Exact(Mods::NONE), A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Line }, "Scroll", "Scroll up one line"),
    bind!(R, KeyCode::Down, Exact(Mods::NONE), A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Line }, "Scroll", "Scroll down one line"),
    bind!(R, KeyCode::Left, Exact(Mods::NONE), A::Scroll { dir: ScrollDir::Left, unit: ScrollUnit::Line }, "Scroll", "Scroll focused code block left"),
    bind!(R, KeyCode::Right, Exact(Mods::NONE), A::Scroll { dir: ScrollDir::Right, unit: ScrollUnit::Line }, "Scroll", "Scroll focused code block right"),

    // Reading-position history: after following an anchor, a `g`, or an outline jump, `z`
    // returns to where you were (the in-file complement to Backspace's cross-file history).
    // The server's cursor-motion history, verbatim — the returned cursor derives focus.
    bind!(R, ch('z'), IgnoreShift(Mods::NONE), A::MotionUndo, "Navigation", "Undo reading-position move"),
    bind!(R, ch('z'), IgnoreShift(Mods::ALT), A::MotionRedo, "Navigation", "Redo reading-position move"),

    // ---- search / navigation, verbatim from Normal ----
    bind!(R, ch('/'), IgnoreShift(Mods::NONE), A::EnterSearch, "Search", "Search"),
    bind!(R, ch('n'), IgnoreShift(Mods::ALT), A::SearchCycle(Direction::Backward), "Search", "Previous match"),
    bind!(R, ch('n'), IgnoreShift(Mods::NONE), A::SearchCycle(Direction::Forward), "Search", "Next match"),
    bind!(R, KeyCode::Backspace, Exact(Mods::NONE), A::NavBack, "Navigation", "Jump back (history)"),
    bind!(R, KeyCode::Backspace, Exact(Mods::ALT), A::NavForward, "Navigation", "Jump forward (history)"),
    bind!(R, ch(']'), Exact(Mods::NONE), A::JumplistStep(Direction::Forward), "", ""),
    bind!(R, ch('['), Exact(Mods::NONE), A::JumplistStep(Direction::Backward), "", ""),
    bind!(R, ch('}'), IgnoreShift(Mods::NONE), A::JumplistStepInFile(Direction::Forward), "", ""),
    bind!(R, ch('{'), IgnoreShift(Mods::NONE), A::JumplistStepInFile(Direction::Backward), "", ""),

    // ---- leader ----
    bind!(R, ch(' '), Exact(Mods::NONE), A::BeginLeader, "Leader", "Space leader chord"),
];

#[rustfmt::skip]
static LEADER: &[Binding] = &[
    bind!(L, ch('f'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Files), "Files", "Find files"),
    bind!(L, ch('f'), Exact(Mods::ALT), A::OpenFilesInBufferDir, "Files", "Find files in buffer's directory"),
    bind!(L, ch('b'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Buffers), "Files", "Switch buffer"),
    bind!(L, ch('b'), Exact(Mods::ALT), A::NewScratch, "Files", "New scratch buffer"),
    bind!(L, ch('g'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Grep), "Files", "Grep workspace"),
    bind!(L, ch('g'), Exact(Mods::ALT), A::OpenGrepFromSelection, "Files", "Grep for selection"),
    bind!(L, ch('e'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Explorer), "Files", "File explorer"),
    bind!(L, ch('e'), Exact(Mods::ALT), A::OpenExplorerAtRoot, "Files", "File explorer at workspace root"),
    bind!(L, ch('w'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Workspaces), "Workspace", "Switch workspace"),
    bind!(L, ch('d'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Diagnostics), "Code", "Diagnostics in current buffer"),
    bind!(L, ch('d'), Exact(Mods::ALT), A::OpenPicker(PickerKind::DiagnosticsWorkspace), "Code", "Workspace diagnostics"),
    bind!(L, ch('j'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Jumplist), "Navigation", "Jumplist"),
    bind!(L, ch('n'), Exact(Mods::NONE), A::ShowDiagnostic, "Code", "Diagnostic at cursor"),
    bind!(L, ch('m'), Exact(Mods::NONE), A::ShowCommitInfo, "Git", "Blame commit details"),
    bind!(L, ch('l'), Exact(Mods::NONE), A::OpenPicker(PickerKind::LspServers), "Code", "LSP servers"),
    bind!(L, ch('r'), Exact(Mods::NONE), A::OpenPicker(PickerKind::References), "Code", "Go to references"),
    bind!(L, ch('o'), Exact(Mods::NONE), A::OpenPicker(PickerKind::DocumentSymbols), "Code", "Document symbols"),
    bind!(L, ch('o'), Exact(Mods::ALT), A::OpenPicker(PickerKind::WorkspaceSymbols), "Code", "Workspace symbols"),
    bind!(L, ch('c'), Exact(Mods::NONE), A::OpenPicker(PickerKind::GitChangesFile), "Git", "Git changes in current file"),
    bind!(L, ch('c'), Exact(Mods::ALT), A::OpenPicker(PickerKind::GitChanges), "Git", "Workspace git changes (hunks)"),
    bind!(L, ch('q'), Exact(Mods::NONE), A::Quit, "App", "Quit"),
    bind!(L, ch('q'), Exact(Mods::ALT), A::SaveAndQuit, "App", "Save and quit"),
    bind!(L, ch('/'), Exact(Mods::NONE), A::OpenHelp, "App", "Show keyboard shortcuts"),
    // `?` is a shifted `/` on every layout we care about, so the terminal reports it with SHIFT set
    // while the GUI/web report the resolved character — `IgnoreShift` accepts both. Deliberately
    // adjacent to `Space /`.
    bind!(L, ch('?'), IgnoreShift(Mods::NONE), A::ShowAppInfo, "App", "About / diagnostics"),
    bind!(L, ch(','), Exact(Mods::NONE), A::OpenWorkspaceSettings, "Workspace", "Workspace settings"),
    bind!(L, ch('.'), Exact(Mods::NONE), A::OpenAppSettings, "App", "Application settings"),
    bind!(L, ch('x'), Exact(Mods::NONE), A::CloseBuffer, "App", "Close buffer"),
    bind!(L, ch('x'), Exact(Mods::ALT), A::SaveAndClose, "App", "Save and close buffer"),
    bind!(L, ch('z'), Exact(Mods::NONE), A::NewWindow, "App", "Open another window"),
    bind!(L, ch('z'), Exact(Mods::ALT), A::CopyWebUrl, "App", "Copy web URL"),
    bind!(L, ch('w'), Exact(Mods::ALT), A::OpenPath, "App", "Open file by absolute path"),
    bind!(L, ch('s'), Exact(Mods::NONE), A::Save, "App", "Save"),
    bind!(L, ch('s'), Exact(Mods::ALT), A::SaveAs, "App", "Save as"),
    bind!(L, ch('k'), Exact(Mods::NONE), A::ToggleKeep, "App", "Keep buffer (toggle transient)"),
    bind!(L, ch('k'), Exact(Mods::ALT), A::Reload, "App", "Reload from disk"),
    bind!(L, ch('p'), Exact(Mods::NONE), A::CopyRelativePath, "App", "Copy relative path"),
    bind!(L, ch('p'), Exact(Mods::ALT), A::CopyAbsolutePath, "App", "Copy absolute path"),
    bind!(L, ch('a'), Exact(Mods::NONE), A::ToggleStageHunk, "Git", "Stage/unstage change (hunk/selection)"),
    bind!(L, ch('a'), Exact(Mods::ALT), A::RevertHunk, "Git", "Revert change"),
    bind!(L, ch('i'), Exact(Mods::NONE), A::ToggleDiffView, "Git", "Toggle inline diff"),
    bind!(L, ch('v'), Exact(Mods::NONE), A::ToggleReadView, "Read", "Toggle Markdown reading view"),
    bind!(L, ch('h'), Exact(Mods::NONE), A::DismissHint, "App", "Dismiss the current hint"),
    bind!(L, ch('h'), Exact(Mods::ALT), A::ToggleHints, "App", "Toggle hints on/off"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycode_for_binding_prefers_base_under_alt_and_modified_otherwise() {
        // macOS delivers Option-f as base `f` + modified `ƒ`. With Alt held we must resolve on the
        // base key, or the Alt-chord never matches.
        assert_eq!(
            keycode_for_binding(Some(KeyCode::Char('f')), Some(KeyCode::Char('ƒ')), true),
            Some(KeyCode::Char('f'))
        );
        // No Alt: honour composition so Shift-/ resolves to `?`, not the base `/`.
        assert_eq!(
            keycode_for_binding(Some(KeyCode::Char('/')), Some(KeyCode::Char('?')), false),
            Some(KeyCode::Char('?'))
        );
    }

    #[test]
    fn bracket_keys_resolve_to_full_vs_file_scoped_jumplist_steps() {
        // `]`/`[` step the whole list; `}`/`{` step within the current file. The file-scoped keys
        // are Shift-bracket (reliable on every terminal, unlike Alt-bracket) and match whether or
        // not the shell also reports the Shift modifier.
        let action = |code, mods| lookup(KeyContext::Normal, code, mods).map(|b| b.action);
        assert!(matches!(
            action(ch(']'), Mods::NONE),
            Some(Action::JumplistStep(Direction::Forward))
        ));
        assert!(matches!(
            action(ch('['), Mods::NONE),
            Some(Action::JumplistStep(Direction::Backward))
        ));
        // `}` = forward, with Shift reported (web/iced) and without (TUI folds it into the char).
        for mods in [Mods::NONE, Mods::SHIFT] {
            assert!(matches!(
                action(ch('}'), mods),
                Some(Action::JumplistStepInFile(Direction::Forward))
            ));
            assert!(matches!(
                action(ch('{'), mods),
                Some(Action::JumplistStepInFile(Direction::Backward))
            ));
        }
    }

    #[test]
    fn keybinding_entries_cover_the_five_modes_once_each() {
        let entries = keybinding_entries();
        for mode in ["Normal", "Any", "Insert", "Search", "Application"] {
            assert!(
                entries.iter().any(|e| e.mode == mode),
                "mode {mode} present"
            );
        }
        // Internal bindings are hidden: never an empty group, and the leader-trigger (bare
        // "Space", action BeginLeader) is filtered out.
        assert!(entries.iter().all(|e| !e.group.is_empty()));
        assert!(entries.iter().all(|e| e.keys != "Space"));
        // Application rows carry the Space leader: every chord is a `Space …` label.
        assert!(entries
            .iter()
            .filter(|e| e.mode == "Application")
            .all(|e| e.keys.starts_with("Space ")));
        // Hover is a direct `Tab` in Normal mode.
        assert!(entries
            .iter()
            .any(|e| e.mode == "Normal" && e.keys == "Tab" && e.desc == "Hover (type & docs)"));
        // The flat list dedupes the shared Ctrl-editing keys: each (mode, keys, desc) row —
        // the picker item identity — appears exactly once.
        let mut seen = std::collections::HashSet::new();
        for e in &entries {
            assert!(
                seen.insert((e.mode.clone(), e.keys.clone(), e.desc.clone())),
                "duplicate row: {} {} ({})",
                e.keys,
                e.desc,
                e.mode
            );
        }
        // Groups are contiguous runs — the picker emits one section header per run, so a group
        // reappearing later would split into duplicate headers.
        let mut seen_groups: Vec<&str> = Vec::new();
        for e in &entries {
            match seen_groups.last() {
                Some(g) if *g == e.group => {}
                _ => {
                    assert!(
                        !seen_groups.contains(&e.group.as_str()),
                        "group {:?} appears in two separate runs",
                        e.group
                    );
                    seen_groups.push(&e.group);
                }
            }
        }
    }

    #[test]
    fn hover_action_reuses_normal_copy_and_scroll_bindings() {
        // Ctrl-c is the Normal-mode Copy binding; the popover reuses it.
        assert_eq!(hover_action(ch('c'), Mods::CTRL), Some(HoverAction::Copy));
        // Arrow / page keys resolve to the same Scroll units the editor uses.
        assert_eq!(
            hover_action(KeyCode::Down, Mods::NONE),
            Some(HoverAction::Scroll {
                dir: ScrollDir::Down,
                unit: ScrollUnit::Line
            })
        );
        assert_eq!(
            hover_action(KeyCode::Up, Mods::ALT),
            Some(HoverAction::Scroll {
                dir: ScrollDir::Up,
                unit: ScrollUnit::Half
            })
        );
        assert_eq!(
            hover_action(KeyCode::PageDown, Mods::NONE),
            Some(HoverAction::Scroll {
                dir: ScrollDir::Down,
                unit: ScrollUnit::Page
            })
        );
        // Horizontal scrolls and unrelated keys aren't popover actions (→ dismiss).
        assert_eq!(hover_action(KeyCode::Left, Mods::NONE), None);
        assert_eq!(hover_action(ch('a'), Mods::NONE), None);
    }

    #[test]
    fn keybinding_sections_follow_the_curated_group_order() {
        let entries = keybinding_entries();
        // Distinct groups in the order their section headers appear (each is one contiguous run).
        let mut sections: Vec<&str> = Vec::new();
        for e in &entries {
            if sections.last().copied() != Some(e.group.as_str()) {
                sections.push(e.group.as_str());
            }
        }
        // Sections appear in exactly GROUP_ORDER's sequence (filtered to groups that have rows),
        // which also proves no emitted group is missing from GROUP_ORDER (else it sorts last and
        // the vectors diverge).
        let expected: Vec<&str> = GROUP_ORDER
            .iter()
            .copied()
            .filter(|g| sections.contains(g))
            .collect();
        assert_eq!(sections, expected, "picker sections must match GROUP_ORDER");
        // GROUP_ORDER carries no stale / misspelled group that never renders.
        for g in GROUP_ORDER {
            assert!(sections.contains(g), "GROUP_ORDER lists unused group {g:?}");
        }
    }

    #[test]
    fn arrow_scroll_binds_only_bare_and_alt() {
        let scroll = |code, mods| lookup(KeyContext::Normal, code, mods).map(|b| b.action);
        // Bare arrow scrolls one line; Alt-arrow scrolls half a page.
        assert!(matches!(
            scroll(KeyCode::Up, Mods::NONE),
            Some(Action::Scroll {
                dir: ScrollDir::Up,
                unit: ScrollUnit::Line
            })
        ));
        assert!(matches!(
            scroll(KeyCode::Down, Mods::NONE),
            Some(Action::Scroll {
                dir: ScrollDir::Down,
                unit: ScrollUnit::Line
            })
        ));
        assert!(matches!(
            scroll(KeyCode::Up, Mods::ALT),
            Some(Action::Scroll {
                dir: ScrollDir::Up,
                unit: ScrollUnit::Half
            })
        ));
        assert!(matches!(
            scroll(KeyCode::Down, Mods::ALT),
            Some(Action::Scroll {
                dir: ScrollDir::Down,
                unit: ScrollUnit::Half
            })
        ));
        // Shift/Ctrl (and Ctrl-Alt) arrows do nothing now the `Any` catch-all is gone.
        for mods in [Mods::SHIFT, Mods::CTRL, Mods::CTRL_ALT] {
            assert!(scroll(KeyCode::Up, mods).is_none());
            assert!(scroll(KeyCode::Down, mods).is_none());
        }
    }

    #[test]
    fn word_end_e_mirrors_w_and_b_shape() {
        let e = |mods| lookup(KeyContext::Normal, ch('e'), mods).map(|b| b.action);
        // Alt-e is big-word end; bare/Shift-e is small-word end (Shift ignored, like `w`/`b`).
        assert!(matches!(
            e(Mods::ALT),
            Some(Action::MoveWordEnd {
                dir: Direction::Forward,
                boundary: WordBoundary::BigWord
            })
        ));
        assert!(matches!(
            e(Mods::NONE),
            Some(Action::MoveWordEnd {
                dir: Direction::Forward,
                boundary: WordBoundary::Word
            })
        ));
        assert!(matches!(
            e(Mods::SHIFT),
            Some(Action::MoveWordEnd {
                dir: Direction::Forward,
                boundary: WordBoundary::Word
            })
        ));
        // Ctrl-e is the Normal-mode Change binding (selection-editing sibling of Ctrl-d);
        // increment/decrement now live on Ctrl-a in GLOBAL.
        assert!(matches!(e(Mods::CTRL), Some(Action::Change)));
    }

    #[test]
    fn reveal_bindings_are_tab_hover_and_space_n_m() {
        // Tab triggers hover directly — no leader chord.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Tab, Mods::NONE).map(|b| b.action),
            Some(Action::Hover)
        ));
        // Diagnostic-at-cursor and blame live on the Space leader (`n` / `m`); `Space j` is
        // the jumplist picker.
        assert!(matches!(
            lookup(KeyContext::Leader, ch('n'), Mods::NONE).map(|b| b.action),
            Some(Action::ShowDiagnostic)
        ));
        assert!(matches!(
            lookup(KeyContext::Leader, ch('j'), Mods::NONE).map(|b| b.action),
            Some(Action::OpenPicker(PickerKind::Jumplist))
        ));
        assert!(matches!(
            lookup(KeyContext::Leader, ch('m'), Mods::NONE).map(|b| b.action),
            Some(Action::ShowCommitInfo)
        ));
        // `Space ?` is the info dialog, next to `Space /`'s shortcut reference. A terminal reports
        // the shifted `/` with SHIFT held while the GUI/web hand over the resolved character, so
        // both must resolve — the whole point of binding it `IgnoreShift`.
        for mods in [Mods::NONE, Mods::SHIFT] {
            assert!(
                matches!(
                    lookup(KeyContext::Leader, ch('?'), mods).map(|b| b.action),
                    Some(Action::ShowAppInfo)
                ),
                "Space ? must resolve with mods {mods:?}"
            );
        }
        // It renders as a plain `Space ?` in the shortcut list — `IgnoreShift` must not leak a
        // "Shift-" into the label.
        assert_eq!(
            lookup(KeyContext::Leader, ch('?'), Mods::SHIFT)
                .map(|b| b.key_label())
                .as_deref(),
            Some("Space ?")
        );
        // Go-to-definition is on Enter; the Space leader's `d` is the workspace diagnostics list, and
        // `Alt-d` the current buffer's.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Enter, Mods::NONE).map(|b| b.action),
            Some(Action::GotoDefinition)
        ));
        // Plain leader is buffer-scoped, Alt widens to the workspace (diagnostics + git changes).
        assert!(matches!(
            lookup(KeyContext::Leader, ch('d'), Mods::NONE).map(|b| b.action),
            Some(Action::OpenPicker(PickerKind::Diagnostics))
        ));
        assert!(matches!(
            lookup(KeyContext::Leader, ch('d'), Mods::ALT).map(|b| b.action),
            Some(Action::OpenPicker(PickerKind::DiagnosticsWorkspace))
        ));
        assert!(matches!(
            lookup(KeyContext::Leader, ch('c'), Mods::NONE).map(|b| b.action),
            Some(Action::OpenPicker(PickerKind::GitChangesFile))
        ));
        assert!(matches!(
            lookup(KeyContext::Leader, ch('c'), Mods::ALT).map(|b| b.action),
            Some(Action::OpenPicker(PickerKind::GitChanges))
        ));
    }

    #[test]
    fn place_cursor_bindings_are_semicolon_upper_and_alt_semicolon_lower() {
        assert!(matches!(
            lookup(KeyContext::Normal, ch(';'), Mods::NONE).map(|b| b.action),
            Some(Action::PlaceCursor(ViewportPlace::Upper))
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, ch(';'), Mods::ALT).map(|b| b.action),
            Some(Action::PlaceCursor(ViewportPlace::Lower))
        ));
        // Upper rests at the shared jump fraction; Lower is its mirror.
        assert_eq!(ViewportPlace::Upper.fraction(), CURSOR_REST_FRACTION);
        assert_eq!(ViewportPlace::Lower.fraction(), 1.0 - CURSOR_REST_FRACTION);
    }

    #[test]
    fn lookups_mirror_the_tui_tables() {
        // h / Shift-h → MoveChar(Backward); Alt-h is the distinct earlier arm.
        assert!(matches!(
            lookup(KeyContext::Normal, ch('h'), Mods::NONE).map(|b| b.action),
            Some(Action::MoveChar(Direction::Backward))
        ));
        assert!(matches!(
            lookup(
                KeyContext::Normal,
                ch('h'),
                Mods {
                    shift: true,
                    ..Mods::NONE
                }
            )
            .map(|b| b.action),
            Some(Action::MoveChar(Direction::Backward))
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, ch('h'), Mods::ALT).map(|b| b.action),
            Some(Action::MoveLineFirstNonblank)
        ));
        // Ctrl-z (undo) lives in Global, not Normal (plain `z` is the motion-undo).
        assert!(lookup(KeyContext::Normal, ch('z'), Mods::CTRL).is_none());
        assert!(matches!(
            lookup(KeyContext::Global, ch('z'), Mods::CTRL).map(|b| b.action),
            Some(Action::Undo)
        ));
        // Mode-divergent Ctrl-d: selection-scoped in Normal, line-scoped in Insert.
        assert!(matches!(
            lookup(KeyContext::Normal, ch('d'), Mods::CTRL).map(|b| b.action),
            Some(Action::DeleteSelection)
        ));
        assert!(matches!(
            lookup(KeyContext::Insert, ch('d'), Mods::CTRL).map(|b| b.action),
            Some(Action::DeleteLine)
        ));
        // Alt-Shift motions still resolve (IgnoreShift on the Alt arm).
        assert!(matches!(
            lookup(
                KeyContext::Normal,
                ch('j'),
                Mods {
                    shift: true,
                    ..Mods::ALT
                }
            )
            .map(|b| b.action),
            Some(Action::MoveVisualLine(VerticalDirection::Down))
        ));
    }

    #[test]
    fn nav_history_on_backspace() {
        // Backspace / Alt-Backspace drive the cross-file nav history; the arrows are now scroll-only.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Backspace, Mods::NONE).map(|b| b.action),
            Some(Action::NavBack)
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Backspace, Mods::ALT).map(|b| b.action),
            Some(Action::NavForward)
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Left, Mods::NONE).map(|b| b.action),
            Some(Action::Scroll {
                dir: ScrollDir::Left,
                ..
            })
        ));
    }

    #[test]
    fn surround_chords_split_by_mode_and_modifier() {
        // Ctrl-Alt-s (unsurround) must precede Ctrl-s (surround); Normal targets the
        // selection, Insert the line.
        assert!(matches!(
            lookup(KeyContext::Normal, ch('s'), Mods::CTRL_ALT).map(|b| b.action),
            Some(Action::Unsurround(SurroundTarget::Selection))
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, ch('s'), Mods::CTRL).map(|b| b.action),
            Some(Action::BeginSurround(SurroundTarget::Selection))
        ));
        assert!(matches!(
            lookup(KeyContext::Insert, ch('s'), Mods::CTRL).map(|b| b.action),
            Some(Action::BeginSurround(SurroundTarget::Line))
        ));
    }

    #[test]
    fn repeatable_covers_motions_only() {
        assert!(Action::MoveChar(Direction::Backward).is_repeatable());
        assert!(Action::SelectLine(Direction::Forward).is_repeatable());
        assert!(Action::TreeExpand.is_repeatable());
        assert!(Action::GotoLine { last: false }.is_repeatable());
        // The cursor-jumping navigations repeat too (symbol / hunk / diagnostic).
        assert!(Action::NavUnit(Direction::Forward).is_repeatable());
        assert!(Action::NextHunk.is_repeatable());
        assert!(Action::PrevHunk.is_repeatable());
        assert!(Action::NextDiagnostic.is_repeatable());
        assert!(Action::PrevDiagnostic.is_repeatable());
        // Edits, scroll, nav history, and the find *arming* never repeat.
        assert!(!Action::DeleteSelection.is_repeatable());
        assert!(!Action::Scroll {
            dir: ScrollDir::Up,
            unit: ScrollUnit::Line
        }
        .is_repeatable());
        assert!(!Action::NavBack.is_repeatable());
        assert!(!Action::BeginFind {
            dir: Direction::Forward,
            till: false
        }
        .is_repeatable());
        assert!(!Action::RepeatMotion.is_repeatable());
    }

    #[test]
    fn p_moves_to_line_first_nonblank_and_q_resizes_tree_selection() {
        // `p` / `Alt-p` step to the first non-blank char of the next / previous line; Shift is the
        // extend modifier (resolved at dispatch via `mods.shift`), so the binding still resolves
        // under Shift to the same motion.
        assert!(matches!(
            lookup(KeyContext::Normal, ch('p'), Mods::NONE).map(|b| b.action),
            Some(Action::MoveLogicalLineFirstNonblank(Direction::Forward))
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, ch('p'), Mods::ALT).map(|b| b.action),
            Some(Action::MoveLogicalLineFirstNonblank(Direction::Backward))
        ));
        assert!(matches!(
            lookup(
                KeyContext::Normal,
                ch('p'),
                Mods {
                    shift: true,
                    ..Mods::NONE
                }
            )
            .map(|b| b.action),
            Some(Action::MoveLogicalLineFirstNonblank(Direction::Forward))
        ));
        assert!(matches!(
            lookup(
                KeyContext::Normal,
                ch('p'),
                Mods {
                    shift: true,
                    ..Mods::ALT
                }
            )
            .map(|b| b.action),
            Some(Action::MoveLogicalLineFirstNonblank(Direction::Backward))
        ));
        // Tree expand / contract moved off `p` onto `q` / `Alt-q`.
        assert!(matches!(
            lookup(KeyContext::Normal, ch('q'), Mods::NONE).map(|b| b.action),
            Some(Action::TreeExpand)
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, ch('q'), Mods::ALT).map(|b| b.action),
            Some(Action::TreeContract)
        ));
    }

    #[test]
    fn read_aliases_cover_editor_muscle_memory() {
        // `p`/`Alt-p` and `Alt-j`/`Alt-k` alias the element step: the editor's line-step
        // variants collapse into one motion at block grain (docs/markdown-view.md §2.3).
        // IgnoreShift keeps Shift as the extend modifier, as on `j`/`k`.
        let shifted = |base: Mods| Mods {
            shift: true,
            ..base
        };
        for (code, mods, dir) in [
            (ch('p'), Mods::NONE, Direction::Forward),
            (ch('p'), Mods::ALT, Direction::Backward),
            (ch('j'), Mods::ALT, Direction::Forward),
            (ch('k'), Mods::ALT, Direction::Backward),
            (ch('p'), shifted(Mods::NONE), Direction::Forward),
            (ch('j'), shifted(Mods::ALT), Direction::Forward),
        ] {
            assert!(matches!(
                lookup(KeyContext::Read, code, mods).map(|b| b.action),
                Some(Action::ReadStep(d)) if d == dir
            ));
        }
        // The Ctrl rows stay the structural block moves.
        assert!(matches!(
            lookup(KeyContext::Read, ch('j'), Mods::CTRL).map(|b| b.action),
            Some(Action::MoveBlock { down: true, .. })
        ));
        // `%` selects every block (the char already encodes Shift), `,` collapses the block
        // selection, and the Delete key deletes block(s) exactly like Ctrl-d.
        assert!(matches!(
            lookup(KeyContext::Read, ch('%'), shifted(Mods::NONE)).map(|b| b.action),
            Some(Action::SelectAll)
        ));
        assert!(matches!(
            lookup(KeyContext::Read, ch(','), Mods::NONE).map(|b| b.action),
            Some(Action::CollapseSelection)
        ));
        assert!(matches!(
            lookup(KeyContext::Read, KeyCode::Delete, Mods::NONE).map(|b| b.action),
            Some(Action::ReadDeleteBlock)
        ));
    }

    #[test]
    fn search_bindings_mirror_the_tui() {
        // `/` enters search (Shift-tolerant); `?` is the extend-to-cursor variant; Alt-/ seeds
        // from the selection.
        assert!(matches!(
            lookup(KeyContext::Normal, ch('/'), Mods::NONE).map(|b| b.action),
            Some(Action::EnterSearch)
        ));
        assert!(matches!(
            lookup(
                KeyContext::Normal,
                ch('?'),
                Mods {
                    shift: true,
                    ..Mods::NONE
                }
            )
            .map(|b| b.action),
            Some(Action::EnterSearchToCursor)
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, ch('/'), Mods::ALT).map(|b| b.action),
            Some(Action::SearchFromSelection)
        ));
        // Esc in Normal drops the search; in the prompt it aborts.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Esc, Mods::NONE).map(|b| b.action),
            Some(Action::DropSearch)
        ));
        assert!(matches!(
            lookup(KeyContext::Search, KeyCode::Esc, Mods::NONE).map(|b| b.action),
            Some(Action::SearchAbort)
        ));
        // Alt-k browses history inside the prompt; plain `k` is not a control key there.
        assert!(matches!(
            lookup(KeyContext::Search, ch('k'), Mods::ALT).map(|b| b.action),
            Some(Action::SearchHistoryPrev)
        ));
        assert!(lookup(KeyContext::Search, ch('k'), Mods::NONE).is_none());
        // `n` cycles and is repeatable via `r`.
        let n = lookup(KeyContext::Normal, ch('n'), Mods::NONE).unwrap();
        assert!(matches!(n.action, Action::SearchCycle(Direction::Forward)));
        assert!(n.action.is_repeatable());
    }
}
