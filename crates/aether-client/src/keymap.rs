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
use aether_protocol::input::SurroundTarget;
use aether_protocol::picker::PickerKind;

/// Layout-resolved key identity, normalised from the platform's key event: letters lowercase
/// (Shift is carried separately in [`Mods`]), shifted symbols as produced (`?`, `{`, …).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyCode {
    Char(char),
    Esc,
    Enter,
    Tab,
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
    const CTRL_ALT: Mods = Mods {
        ctrl: true,
        alt: true,
        shift: false,
    };
    pub const SHIFT: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: true,
    };
    const SHIFT_ALT: Mods = Mods {
        ctrl: false,
        alt: true,
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
    Leader,
    /// The `Tab`-leader chords for *revealing* information at the cursor (hover, diagnostic, blame
    /// commit) — a second leader, distinct from the `Space` [`Leader`](KeyContext::Leader) chords.
    Reveal,
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
    NavUnitEdge {
        start: bool,
    },
    BeginFind {
        dir: Direction,
        till: bool,
    },

    // ---- selection ----
    SelectWord {
        boundary: WordBoundary,
    },
    SelectLine(Direction),
    SelectAll,
    SwapAnchor,
    CollapseSelection,
    TreeExpand,
    TreeContract,
    MotionUndo,
    MotionRedo,
    RepeatMotion,
    CenterCursor,
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
    /// `Tab` — arm the reveal-leader chord (next key looked up in [`KeyContext::Reveal`]).
    BeginReveal,

    // ---- edits ----
    Backspace,
    NewlineIndent,
    InsertTab,
    DeletePoint,
    DeleteSelection,
    Undo,
    Redo,
    MoveLines(VerticalDirection),
    JoinLines,
    Indent,
    Dedent,
    ToggleComment,
    OpenLineBelow,
    OpenLineAbove,
    // Selection-scoped (Normal) vs line-scoped (Insert) clipboard/edit pairs.
    Copy,
    Cut,
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
    /// `>` / `<` — step through cached grep hits from the cursor, cross-file.
    GrepNavigate(Direction),
    /// `Esc` in Normal — drop the active search (clear highlights).
    DropSearch,

    // ---- app ----
    Quit,
    Save,
    SaveAs,
    Reload,
    NewScratch,
    CloseBuffer,

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
    /// `Space Alt-f` / `Space Alt-g` — Files/Grep pre-scoped to the active buffer's directory
    /// (seeded as a directory filter chip, removable like any chip).
    OpenPickerInBufferDir(PickerKind),
    /// `Space Alt-e` — Explorer at the buffer's project root rather than its directory.
    OpenExplorerAtRoot,

    // ---- shell-local overlays (dispatched via `Effect::ShellAction`; a shell without the
    // overlay ignores them) ----
    /// `Space ?` — the keyboard-shortcut help overlay, generated from these tables.
    OpenHelp,
    /// `Space ,` — the project-settings overlay (roots + rename). TUI-only today.
    OpenProjectSettings,
    /// `Space .` — the application-settings overlay (global preferences, e.g. soft wrap).
    OpenAppSettings,
}

impl Action {
    /// Whether this chord arms a capture (the next keystroke is data, not a binding).
    pub fn awaits_key(&self) -> bool {
        matches!(self, Action::BeginFind { .. } | Action::BeginSurround(_))
    }

    /// Whether `r`/`Shift-r` replays this action — the TUI's `is_repeatable`: every
    /// cursor/selection motion (absolute ones included) plus the selection motions; never
    /// edits, scroll, or the non-motion selection ops. (`SearchCycle` joins when search lands.)
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
                | Action::NavUnitEdge { .. }
                | Action::SelectWord { .. }
                | Action::SelectLine(_)
                | Action::TreeExpand
                | Action::TreeContract
                | Action::SearchCycle(_)
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
        match self.ctx {
            KeyContext::Leader => s.push_str("Space "),
            KeyContext::Reveal => s.push_str("Tab "),
            _ => {}
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
        KeyContext::Leader,
        KeyContext::Reveal,
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
        KeyContext::Leader => LEADER,
        KeyContext::Reveal => REVEAL,
        KeyContext::Global => GLOBAL,
    }
}

pub fn lookup(ctx: KeyContext, code: KeyCode, mods: Mods) -> Option<&'static Binding> {
    table(ctx).iter().find(|b| b.matches(code, mods))
}

/// One row of the keyboard-shortcuts help: a formatted chord + its description, filed under a tab
/// and a section group. Built straight from the binding tables so every client renders identical
/// content.
pub struct HelpEntry {
    /// `Normal` / `Insert` / `Search` / `Application`.
    pub tab: &'static str,
    /// Section heading within the tab (the binding's `group`).
    pub group: &'static str,
    /// Display chord, e.g. `Ctrl-w`, `Space f ␣`, `↑`.
    pub keys: String,
    pub desc: &'static str,
}

/// Every user-facing binding, grouped for the help dialog: the four tabs in display order, the
/// `Global` (shared Ctrl-editing) keys folded into both Normal and Insert, leader chords as the
/// Application tab. Bindings with no `group` (internal aliases) and the leader-trigger itself are
/// omitted. The single source the web and native help dialogs both render.
pub fn help_entries() -> Vec<HelpEntry> {
    const TABS: [(&str, &[KeyContext]); 4] = [
        ("Normal", &[KeyContext::Normal, KeyContext::Global]),
        ("Insert", &[KeyContext::Insert, KeyContext::Global]),
        ("Search", &[KeyContext::Search]),
        ("Application", &[KeyContext::Leader, KeyContext::Reveal]),
    ];
    let mut entries = Vec::new();
    for (tab, contexts) in TABS {
        for &cx in contexts {
            for b in table(cx) {
                if !b.group.is_empty()
                    && !matches!(b.action, Action::BeginLeader | Action::BeginReveal)
                {
                    entries.push(HelpEntry {
                        tab,
                        group: b.group,
                        keys: b.key_label(),
                        desc: b.desc,
                    });
                }
            }
        }
    }
    entries
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
use KeyContext::{Global as G, Insert as I, Leader as L, Normal as N};
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
    bind!(N, ch('q'), Exact(Mods::NONE), A::SwapAnchor, "Selection", "Swap cursor and anchor"),
    bind!(N, ch('y'), Exact(Mods::NONE), A::TreeExpand, "Selection", "Expand selection to parent syntax node"),
    bind!(N, ch('y'), Exact(Mods::ALT), A::TreeContract, "Selection", "Contract selection to child syntax node"),
    bind!(N, ch('u'), Exact(Mods::ALT), A::MotionRedo, "Selection", "Redo cursor/selection motion"),
    bind!(N, ch('u'), Exact(Mods::NONE), A::MotionUndo, "Selection", "Undo cursor/selection motion"),
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
    bind!(N, ch('e'), Any, A::MoveWordEnd { dir: Direction::Forward, boundary: WordBoundary::Word }, "Motion", "Small word end"),

    // ---- motions: find char ----
    bind!(N, ch('f'), IgnoreShift(Mods::ALT), A::BeginFind { dir: Direction::Backward, till: false }, "Motion", "Find character backward"),
    bind!(N, ch('f'), IgnoreShift(Mods::NONE), A::BeginFind { dir: Direction::Forward, till: false }, "Motion", "Find character forward"),
    bind!(N, ch('t'), IgnoreShift(Mods::ALT), A::BeginFind { dir: Direction::Backward, till: true }, "Motion", "Till character backward"),
    bind!(N, ch('t'), IgnoreShift(Mods::NONE), A::BeginFind { dir: Direction::Forward, till: true }, "Motion", "Till character forward"),

    // ---- motions: brackets / nav units / goto ----
    bind!(N, ch('m'), IgnoreShift(Mods::NONE), A::MatchBracket { inner: false }, "Motion", "Matching bracket"),
    bind!(N, ch('m'), IgnoreShift(Mods::ALT), A::MatchBracket { inner: true }, "Motion", "Inner matching bracket"),
    bind!(N, ch('o'), Exact(Mods::NONE), A::NavUnit(Direction::Forward), "Navigation", "Next symbol"),
    bind!(N, ch('o'), Exact(Mods::ALT), A::NavUnit(Direction::Backward), "Navigation", "Previous symbol"),
    bind!(N, ch('o'), Exact(Mods::SHIFT), A::NavUnitEdge { start: false }, "Navigation", "Select to end of unit"),
    bind!(N, ch('o'), Exact(Mods::SHIFT_ALT), A::NavUnitEdge { start: true }, "Navigation", "Select to start of unit"),
    bind!(N, ch('g'), IgnoreShift(Mods::ALT), A::GotoLine { last: true }, "Motion", "Go to last line"),
    bind!(N, ch('g'), IgnoreShift(Mods::NONE), A::GotoLine { last: false }, "Motion", "Go to line (count, default 1)"),
    bind!(N, KeyCode::Enter, Exact(Mods::NONE), A::GotoDefinition, "Code", "Go to definition"),

    // ---- cursor-local git / diagnostic navigation (the list pickers live under Space) ----
    bind!(N, ch('c'), Exact(Mods::NONE), A::NextHunk, "Git", "Next change (hunk)"),
    bind!(N, ch('c'), Exact(Mods::ALT), A::PrevHunk, "Git", "Previous change (hunk)"),
    bind!(N, ch('d'), Exact(Mods::NONE), A::NextDiagnostic, "Code", "Next diagnostic"),
    bind!(N, ch('d'), Exact(Mods::ALT), A::PrevDiagnostic, "Code", "Previous diagnostic"),

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
    bind!(N, KeyCode::Up, IgnoreShift(Mods::ALT), A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Half }, "Scroll", "Scroll half page up"),
    bind!(N, KeyCode::Down, IgnoreShift(Mods::ALT), A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Half }, "Scroll", "Scroll half page down"),
    bind!(N, KeyCode::Up, Any, A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Line }, "Scroll", "Scroll up one line"),
    bind!(N, KeyCode::Down, Any, A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Line }, "Scroll", "Scroll down one line"),
    bind!(N, KeyCode::Left, Any, A::Scroll { dir: ScrollDir::Left, unit: ScrollUnit::Line }, "Scroll", "Scroll left one column"),
    bind!(N, KeyCode::Right, Any, A::Scroll { dir: ScrollDir::Right, unit: ScrollUnit::Line }, "Scroll", "Scroll right one column"),
    bind!(N, ch(';'), Exact(Mods::NONE), A::CenterCursor, "Scroll", "Center cursor in window"),

    // ---- navigation history (cross-file jump list) ----
    bind!(N, KeyCode::Backspace, Exact(Mods::NONE), A::NavBack, "Navigation", "Jump back (history)"),
    bind!(N, KeyCode::Backspace, Exact(Mods::ALT), A::NavForward, "Navigation", "Jump forward (history)"),

    // ---- delete / search ----
    bind!(N, KeyCode::Delete, Any, A::DeleteSelection, "Edit", "Delete selection"),
    bind!(N, ch('/'), IgnoreShift(Mods::NONE), A::EnterSearch, "Search", "Search"),
    bind!(N, ch('/'), Exact(Mods::ALT), A::SearchFromSelection, "Search", "Search for selection"),
    bind!(N, ch('?'), IgnoreShift(Mods::NONE), A::EnterSearchToCursor, "Search", "Select from cursor to match"),
    bind!(N, ch('n'), IgnoreShift(Mods::ALT), A::SearchCycle(Direction::Backward), "Search", "Previous match"),
    bind!(N, ch('n'), IgnoreShift(Mods::NONE), A::SearchCycle(Direction::Forward), "Search", "Next match"),

    // ---- selection editing / clipboard ----
    bind!(N, ch('a'), Exact(Mods::CTRL), A::Change, "Edit", "Change selection"),
    bind!(N, ch('d'), Exact(Mods::CTRL), A::DeleteSelection, "Edit", "Delete selection"),
    bind!(N, ch('c'), Exact(Mods::CTRL), A::Copy, "Clipboard", "Copy selection"),
    bind!(N, ch('x'), Exact(Mods::CTRL), A::Cut, "Clipboard", "Cut selection"),
    bind!(N, ch('v'), Exact(Mods::CTRL), A::Paste, "Clipboard", "Paste before selection"),
    bind!(N, ch('v'), Exact(Mods::CTRL_ALT), A::ReplaceClipboard, "Clipboard", "Replace selection with clipboard"),
    bind!(N, ch('s'), Exact(Mods::CTRL_ALT), A::Unsurround(SurroundTarget::Selection), "Edit", "Unsurround selection"),
    bind!(N, ch('s'), Exact(Mods::CTRL), A::BeginSurround(SurroundTarget::Selection), "Edit", "Surround selection"),

    // ---- leaders ----
    bind!(N, ch(' '), Exact(Mods::NONE), A::BeginLeader, "Leader", "Space leader chord"),
    bind!(N, KeyCode::Tab, Exact(Mods::NONE), A::BeginReveal, "Reveal", "Tab reveal chord"),
];

#[rustfmt::skip]
static GLOBAL: &[Binding] = &[
    bind!(G, ch('u'), Exact(Mods::CTRL), A::Undo, "Edit", "Undo"),
    bind!(G, ch('u'), Exact(Mods::CTRL_ALT), A::Redo, "Edit", "Redo"),
    bind!(G, ch('j'), Exact(Mods::CTRL), A::MoveLines(VerticalDirection::Down), "Edit", "Move line(s) down"),
    bind!(G, ch('k'), Exact(Mods::CTRL), A::MoveLines(VerticalDirection::Up), "Edit", "Move line(s) up"),
    bind!(G, ch('g'), Exact(Mods::CTRL), A::JoinLines, "Edit", "Join lines"),
    bind!(G, ch('l'), Exact(Mods::CTRL), A::Indent, "Edit", "Indent"),
    bind!(G, ch('h'), Exact(Mods::CTRL), A::Dedent, "Edit", "Dedent"),
    bind!(G, ch('o'), Exact(Mods::CTRL), A::OpenLineBelow, "Edit", "Open line below"),
    bind!(G, ch('o'), Exact(Mods::CTRL_ALT), A::OpenLineAbove, "Edit", "Open line above"),
    // Mode-agnostic edits (same action in Normal and Insert) live here rather than being split
    // line-vs-selection, so one binding serves both modes.
    bind!(G, ch('y'), Exact(Mods::CTRL), A::ToggleComment, "Edit", "Toggle comment"),
    bind!(G, ch('f'), Exact(Mods::CTRL), A::Format, "Code", "Format document"),
];

#[rustfmt::skip]
static INSERT: &[Binding] = &[
    bind!(I, KeyCode::Esc, Any, A::LeaveInsert, "Mode", "Leave insert mode"),
    bind!(I, KeyCode::Backspace, Any, A::Backspace, "Edit", "Delete character before cursor"),
    bind!(I, KeyCode::Delete, Any, A::DeletePoint, "Edit", "Delete character at cursor"),
    bind!(I, KeyCode::Enter, Any, A::NewlineIndent, "Edit", "Newline and indent"),
    bind!(I, KeyCode::Tab, Any, A::InsertTab, "Edit", "Insert tab"),
    bind!(I, KeyCode::Left, Any, A::MoveChar(Direction::Backward), "Motion", "Cursor left"),
    bind!(I, KeyCode::Right, Any, A::MoveChar(Direction::Forward), "Motion", "Cursor right"),
    bind!(I, KeyCode::Up, Any, A::MoveVisualLine(VerticalDirection::Up), "Motion", "Cursor up"),
    bind!(I, KeyCode::Down, Any, A::MoveVisualLine(VerticalDirection::Down), "Motion", "Cursor down"),
    // Line-scoped editing mirrors Normal's selection-scoped Ctrl column on the same keys (Insert
    // has no selection to act on); the mode-agnostic Ctrl-y/Ctrl-f come from GLOBAL.
    bind!(I, ch('a'), Exact(Mods::CTRL), A::ChangeLine, "Edit", "Change line"),
    bind!(I, ch('d'), Exact(Mods::CTRL), A::DeleteLine, "Edit", "Delete line"),
    bind!(I, ch('c'), Exact(Mods::CTRL), A::CopyLine, "Clipboard", "Copy line"),
    bind!(I, ch('x'), Exact(Mods::CTRL), A::CutLine, "Clipboard", "Cut line"),
    bind!(I, ch('v'), Exact(Mods::CTRL), A::PasteAtCursor, "Clipboard", "Paste at cursor"),
    bind!(I, ch('v'), Exact(Mods::CTRL_ALT), A::ReplaceLineClipboard, "Clipboard", "Replace line with clipboard"),
    bind!(I, ch('s'), Exact(Mods::CTRL_ALT), A::Unsurround(SurroundTarget::Line), "Edit", "Unsurround line"),
    bind!(I, ch('s'), Exact(Mods::CTRL), A::BeginSurround(SurroundTarget::Line), "Edit", "Surround line"),
];

#[rustfmt::skip]
static SEARCH: &[Binding] = &[
    bind!(KeyContext::Search, KeyCode::Esc, Any, A::SearchAbort, "Search", "Abort search"),
    bind!(KeyContext::Search, KeyCode::Enter, Any, A::SearchCommit, "Search", "Commit search"),
    // Alt-k/j (not Up/Down) browse history — same chord as the TUI / picker inputs.
    bind!(KeyContext::Search, ch('k'), Exact(Mods::ALT), A::SearchHistoryPrev, "Search", "Previous query in history"),
    bind!(KeyContext::Search, ch('j'), Exact(Mods::ALT), A::SearchHistoryNext, "Search", "Next query in history"),
    // Match-option toggles, mirroring the grep picker's chip chords (Alt-c / Alt-w / Alt-e).
    bind!(KeyContext::Search, ch('c'), Exact(Mods::ALT), A::SearchToggleCase, "Search", "Cycle case sensitivity"),
    bind!(KeyContext::Search, ch('w'), Exact(Mods::ALT), A::SearchToggleWord, "Search", "Toggle whole-word match"),
    bind!(KeyContext::Search, ch('e'), Exact(Mods::ALT), A::SearchToggleRegex, "Search", "Toggle literal/regex"),
    // Text entry (chars, Backspace, Left/Right caret) is owned by each shell's search input, which
    // syncs the value via `search_set_query`; only the command keys above live in this table.
];

#[rustfmt::skip]
static LEADER: &[Binding] = &[
    bind!(L, ch('f'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Files), "Files", "Find files"),
    bind!(L, ch('f'), Exact(Mods::ALT), A::OpenPickerInBufferDir(PickerKind::Files), "Files", "Find files in buffer's directory"),
    bind!(L, ch('b'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Buffers), "Files", "Switch buffer"),
    bind!(L, ch('b'), Exact(Mods::ALT), A::NewScratch, "Files", "New scratch buffer"),
    bind!(L, ch('g'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Grep), "Files", "Grep workspace"),
    bind!(L, ch('g'), Exact(Mods::ALT), A::OpenPickerInBufferDir(PickerKind::Grep), "Files", "Grep buffer's directory"),
    bind!(L, ch('e'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Explorer), "Files", "File explorer"),
    bind!(L, ch('e'), Exact(Mods::ALT), A::OpenExplorerAtRoot, "Files", "File explorer at project root"),
    bind!(L, ch('p'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Projects), "Project", "Switch project"),
    bind!(L, ch('d'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Diagnostics), "Code", "Diagnostics list"),
    bind!(L, ch('l'), Exact(Mods::NONE), A::OpenPicker(PickerKind::LspServers), "Code", "LSP servers"),
    bind!(L, ch('r'), Exact(Mods::NONE), A::OpenPicker(PickerKind::References), "Code", "Go to references"),
    bind!(L, ch('o'), Exact(Mods::NONE), A::OpenPicker(PickerKind::DocumentSymbols), "Code", "Document symbols"),
    bind!(L, ch('n'), Exact(Mods::NONE), A::GrepNavigate(Direction::Forward), "Search", "Next grep hit"),
    bind!(L, ch('n'), Exact(Mods::ALT), A::GrepNavigate(Direction::Backward), "Search", "Previous grep hit"),
    bind!(L, ch('q'), Exact(Mods::NONE), A::Quit, "App", "Quit"),
    bind!(L, ch('?'), Any, A::OpenHelp, "App", "Show keyboard shortcuts"),
    bind!(L, ch(','), Exact(Mods::NONE), A::OpenProjectSettings, "Project", "Project settings"),
    bind!(L, ch('.'), Exact(Mods::NONE), A::OpenAppSettings, "App", "Application settings"),
    bind!(L, ch('w'), Exact(Mods::NONE), A::CloseBuffer, "App", "Close buffer"),
    bind!(L, ch('s'), Exact(Mods::NONE), A::Save, "App", "Save"),
    bind!(L, ch('s'), Exact(Mods::ALT), A::SaveAs, "App", "Save as"),
    bind!(L, ch('a'), Exact(Mods::NONE), A::Reload, "App", "Reload from disk"),
    bind!(L, ch('y'), Exact(Mods::NONE), A::ToggleStageHunk, "Git", "Stage/unstage change (hunk/selection)"),
    bind!(L, ch('y'), Exact(Mods::ALT), A::RevertHunk, "Git", "Revert change"),
    bind!(L, ch('i'), Exact(Mods::NONE), A::ToggleDiffView, "Git", "Toggle inline diff"),
];

/// The `Tab`-leader chords: reveal information at the cursor. A second leader keeps these
/// transient, look-here gestures (hover card, diagnostic, blame commit) off the crowded `Space`
/// map and together under one mnemonic.
#[rustfmt::skip]
static REVEAL: &[Binding] = &[
    bind!(KeyContext::Reveal, ch('h'), Exact(Mods::NONE), A::Hover, "Reveal", "Hover (type & docs)"),
    bind!(KeyContext::Reveal, ch('d'), Exact(Mods::NONE), A::ShowDiagnostic, "Reveal", "Diagnostic at cursor"),
    bind!(KeyContext::Reveal, ch('c'), Exact(Mods::NONE), A::ShowCommitInfo, "Reveal", "Blame commit details"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_entries_group_into_the_four_tabs() {
        let entries = help_entries();
        for tab in ["Normal", "Insert", "Search", "Application"] {
            assert!(entries.iter().any(|e| e.tab == tab), "tab {tab} present");
        }
        // Internal bindings are hidden: never an empty group, and the leader-trigger (bare
        // "Space", action BeginLeader) is filtered out.
        assert!(entries.iter().all(|e| !e.group.is_empty()));
        assert!(entries.iter().all(|e| e.keys != "Space"));
        // The Application tab carries both leaders: every chord is either a `Space …` (the Space
        // leader) or a `Tab …` (the reveal leader).
        assert!(entries
            .iter()
            .filter(|e| e.tab == "Application")
            .all(|e| e.keys.starts_with("Space ") || e.keys.starts_with("Tab ")));
        // The reveal chords show up there with their `Tab …` labels.
        assert!(entries
            .iter()
            .any(|e| e.keys == "Tab h" && e.desc == "Hover (type & docs)"));
        // Global (shared Ctrl-editing) keys fold into both Normal and Insert: at least one
        // description shows up under both tabs.
        let in_tab = |t: &str| {
            entries
                .iter()
                .filter(move |e| e.tab == t)
                .map(|e| e.desc)
                .collect::<Vec<_>>()
        };
        let (normal, insert) = (in_tab("Normal"), in_tab("Insert"));
        assert!(
            normal.iter().any(|d| insert.contains(d)),
            "Global bindings appear in both Normal and Insert"
        );
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
    fn tab_reveal_leader_replaces_the_space_bindings() {
        // Tab arms the reveal leader from Normal.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Tab, Mods::NONE).map(|b| b.action),
            Some(Action::BeginReveal)
        ));
        // The reveal context maps h/d/c to the cursor-reveal actions.
        assert!(matches!(
            lookup(KeyContext::Reveal, ch('h'), Mods::NONE).map(|b| b.action),
            Some(Action::Hover)
        ));
        assert!(matches!(
            lookup(KeyContext::Reveal, ch('d'), Mods::NONE).map(|b| b.action),
            Some(Action::ShowDiagnostic)
        ));
        assert!(matches!(
            lookup(KeyContext::Reveal, ch('c'), Mods::NONE).map(|b| b.action),
            Some(Action::ShowCommitInfo)
        ));
        // Hover/diagnostic/blame are reveal-only now — never on the Space leader.
        assert!(lookup(KeyContext::Leader, ch('k'), Mods::NONE).is_none());
        assert!(lookup(KeyContext::Leader, ch('j'), Mods::NONE).is_none());
        // Go-to-definition moved to Enter; the Space leader's `d` is now the diagnostics list.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Enter, Mods::NONE).map(|b| b.action),
            Some(Action::GotoDefinition)
        ));
        assert!(matches!(
            lookup(KeyContext::Leader, ch('d'), Mods::NONE).map(|b| b.action),
            Some(Action::OpenPicker(PickerKind::Diagnostics))
        ));
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
        // Ctrl-u (undo) lives in Global, not Normal (plain `u` is the motion-undo).
        assert!(lookup(KeyContext::Normal, ch('u'), Mods::CTRL).is_none());
        assert!(matches!(
            lookup(KeyContext::Global, ch('u'), Mods::CTRL).map(|b| b.action),
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
        // Backspace / Alt-Backspace drive the cross-file jump list; the arrows are now scroll-only.
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
