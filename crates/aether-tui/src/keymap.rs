//! Data-driven keybindings.
//!
//! The editor's key dispatch used to live as ~580 lines of inline `match (code, mods)` arms in
//! `app.rs`. This module turns the *binding* half of that into data: a flat, ordered table per
//! [`KeyContext`] mapping a key chord to an [`Action`] (an abstract description of intent), plus a
//! human-readable description. `app.rs` keeps the *execution* half — `run_action` resolves an
//! `Action` against live state (count, viewport, selection) and performs the RPCs.
//!
//! Two things deliberately stay out of the table because they're stateful capture/lexing, not
//! chord lookups (so forcing them into the model would be dishonest):
//! - count accumulation (`3w`) — a digit lexer that runs before lookup,
//! - the `f`/`t` find-char continuation — the *next* keystroke is literal data, not a binding.
//!
//! `count` and `extend` (Shift) are execution context, never table data: a motion is bound once
//! (e.g. `h → MoveChar(Backward)`) and the handler passes whether Shift was held. This is what
//! collapses the old `m == NONE || m == SHIFT_ONLY` duplication.
//!
//! The single source of truth means the help overlay (`ui::draw_help_overlay`) is a pure `filter`
//! over these tables — descriptions can't drift from behaviour. The shape is also config-*ready*
//! (give `Action` a stable string name and parse it) but, per the project's "no config system"
//! rule, no loader exists: the tables are in-code `static` data.

use aether_protocol::cursor::{Direction, VerticalDirection, WordBoundary};
use aether_protocol::input::SurroundTarget;
use aether_protocol::picker::PickerKind;
use crossterm::event::{KeyCode, KeyModifiers};

/// Where a binding is active. Each context owns one ordered table; `Global` holds the Ctrl-modified
/// editing shortcuts that behave *identically* in Normal and Insert (undo, indent, move lines, …),
/// which both consult before their own table. Shortcuts that differ by mode — the selection-vs-line
/// clipboard/edit keys — live in the per-mode tables instead. `Leader` holds the second key of a
/// `Space` chord.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyContext {
    Normal,
    Insert,
    Search,
    Leader,
    Global,
}

/// How a binding matches the modifier state of a key event.
///
/// Matching (this enum) and selection-extension (`extend`, derived from Shift and applied per
/// `Action`) are deliberately separate concerns — see the module docs.
#[derive(Clone, Copy)]
pub enum ModPattern {
    /// Modifiers must equal exactly (e.g. `Alt-h` is `Exact(ALT)`).
    Exact(KeyModifiers),
    /// Modifiers must equal `base` ignoring Shift, so one row covers `h` and `Shift-h`. The
    /// handler reads Shift separately to decide `extend`. Used for the old `NONE || SHIFT` arms.
    IgnoreShift(KeyModifiers),
    /// Match regardless of modifiers — the old wildcard `_` arms (`}`, `Delete`, plain `e`).
    Any,
}

impl ModPattern {
    fn matches(self, mods: KeyModifiers) -> bool {
        match self {
            ModPattern::Exact(m) => mods == m,
            ModPattern::IgnoreShift(base) => mods.difference(KeyModifiers::SHIFT) == base,
            ModPattern::Any => true,
        }
    }

    /// The representative modifiers to display in help (Shift/wildcards collapse to none).
    fn display_mods(self) -> KeyModifiers {
        match self {
            ModPattern::Exact(m) | ModPattern::IgnoreShift(m) => m,
            ModPattern::Any => KeyModifiers::NONE,
        }
    }
}

/// Vertical/horizontal scroll direction for [`Action::Scroll`].
#[derive(Clone, Copy)]
pub enum ScrollDir {
    Up,
    Down,
    Left,
    Right,
}

/// How far a [`Action::Scroll`] moves: one line/column, half a viewport, or a full viewport.
#[derive(Clone, Copy)]
pub enum ScrollUnit {
    Line,
    Half,
    Page,
}

/// Where `i`/`a`/`Alt-i`/`Alt-a` drop the cursor when entering Insert mode.
#[derive(Clone, Copy, Debug)]
pub enum InsertWhere {
    /// `i` — at the cursor (or at the lower end of the selection).
    SelectionStart,
    /// `a` — after the cursor (or at the upper end of the selection).
    SelectionEnd,
    /// `Alt-i` — column 0 of the first line of the selection (or the cursor's line).
    FirstLineStart,
    /// `Alt-a` — end of the last line of the selection (or the cursor's line).
    LastLineEnd,
}

/// An abstract description of what a binding does. Carries *intent*, not runtime values: `count`
/// and `extend` and the viewport id are resolved by `run_action` against live `AppState`, so an
/// `Action` is plain `Copy` data and the tables can be `static`. The big match that turns an
/// `Action` into RPCs lives in `app.rs::run_action`.
#[derive(Clone, Copy)]
pub enum Action {
    // ---- motions (extend = Shift) ----
    MoveChar(Direction),
    MoveWord {
        dir: Direction,
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
    /// `g` (line `count`, 1-indexed) / `Alt-g` (last line).
    GotoLine {
        last: bool,
    },
    MatchBracket {
        inner: bool,
    },
    /// `u`/`d` move the cursor by `count` full (or half) viewports of visual rows.
    PageMotion {
        dir: VerticalDirection,
        half: bool,
    },
    /// `]`/`[` — step between navigation units (never extends).
    NavUnit(Direction),
    /// `}`/`{` — jump to end/start of the enclosing unit (always extends).
    NavUnitEdge {
        start: bool,
    },

    // ---- selection / cursor history ----
    SelectLine(Direction),
    SwapAnchor,
    CollapseSelection,
    TreeExpand,
    TreeContract,
    MotionUndo,
    MotionRedo,
    RepeatMotion,
    CenterCursor,
    /// `f`/`t` (+ Alt for backward) — arm the find-char capture; the next keystroke is the target.
    BeginFind {
        dir: Direction,
        till: bool,
    },
    /// `Ctrl-s` — arm the surround capture; the next keystroke names the delimiter to wrap the
    /// target with. The target distinguishes Normal (selection) from Insert (line).
    BeginSurround(SurroundTarget),
    /// `Ctrl-Alt-s` — strip the delimiter pair hugging the target (inverse of surround).
    Unsurround(SurroundTarget),

    // ---- viewport scroll ----
    Scroll {
        dir: ScrollDir,
        unit: ScrollUnit,
    },

    // ---- mode transitions ----
    EnterInsert(InsertWhere),
    LeaveInsert,
    BeginLeader,

    // ---- edits (Global Ctrl table + a few Insert keys) ----
    Backspace,
    NewlineIndent,
    InsertTab,
    /// Insert-mode `Delete` — remove the 1-char point at the cursor.
    DeletePoint,
    /// Normal `Delete` / `Ctrl-d` — delete the selection, `count` times.
    DeleteSelection,
    Undo,
    Redo,
    ToggleWrap,
    MoveLines(VerticalDirection),
    JoinLines,
    Indent,
    Dedent,
    ToggleComment,
    OpenLineBelow,
    OpenLineAbove,
    // Selection-scoped editing/clipboard (Normal mode). Their Insert-mode counterparts below act on
    // the current line instead, since Insert has no selection — so each mode binds its own action
    // rather than sharing one that branches on mode at runtime.
    Copy,
    Cut,
    Paste,
    Change,
    ReplaceClipboard,
    // Line-scoped counterparts (Insert mode): copy/cut/replace/delete/blank the current line, and
    // paste at the caret.
    CopyLine,
    CutLine,
    PasteAtCursor,
    ChangeLine,
    DeleteLine,
    ReplaceLineClipboard,

    // ---- search ----
    EnterSearch,
    SearchFromSelection,
    SearchCycle(Direction),
    /// `Esc` — abort search and revert.
    SearchAbort,
    /// `Enter` — commit the search.
    SearchCommit,
    SearchHistoryPrev,
    SearchHistoryNext,
    SearchCursorLeft,
    SearchCursorRight,
    SearchBackspace,
    GrepNavigate(Direction),
    /// `Esc` in Normal — drop the active search (clear highlights).
    DropSearch,

    // ---- pickers / app-level ----
    OpenPicker(PickerKind),
    OpenProjectSettings,
    OpenHelp,
    Quit,
    CloseBuffer,
    Save,
    SaveAs,
    Reload,
    NewScratch,
}

impl Action {
    /// Leader actions that only make sense with an open buffer. The leader handler drops these
    /// (matching the old "Esc cancels a chord" behaviour) when there's no editor, so the
    /// pre-activation screen only surfaces the editor-free actions (`Space p/q/,/?`).
    pub fn needs_editor(self) -> bool {
        matches!(
            self,
            Action::OpenPicker(PickerKind::Files)
                | Action::OpenPicker(PickerKind::Buffers)
                | Action::OpenPicker(PickerKind::Grep)
                | Action::OpenPicker(PickerKind::Explorer)
                | Action::CloseBuffer
                | Action::Save
                | Action::SaveAs
                | Action::Reload
                | Action::NewScratch
        )
    }

    /// Whether running this action arms a capture for one more keystroke — the next key is consumed
    /// as data (a find target char or a surround delimiter), not a fresh binding. The help overlay
    /// marks these with a trailing placeholder glyph so "another key follows" is visible without
    /// spelling it out in the description. (`Space`/leader isn't included: its second key is already
    /// shown as the `Space x` chord on each leader row.)
    pub fn awaits_key(&self) -> bool {
        matches!(self, Action::BeginFind { .. } | Action::BeginSurround(_))
    }
}

/// One row of a keymap table: a chord (`code` + `mods` pattern) in a context, the action it runs,
/// and the help text. `group` is a soft sub-heading within a context in the help overlay.
pub struct Binding {
    pub ctx: KeyContext,
    pub code: KeyCode,
    pub mods: ModPattern,
    pub action: Action,
    pub group: &'static str,
    pub desc: &'static str,
}

impl Binding {
    fn matches(&self, code: KeyCode, mods: KeyModifiers) -> bool {
        self.code == code && self.mods.matches(mods)
    }

    /// Whether this chord includes Alt. The help overlay uses it to fold a key's Alt variant onto
    /// the same line as its base binding (they're separate bindings — Alt isn't one uniform
    /// semantic the way Shift is — but they read better paired).
    pub fn is_alt(&self) -> bool {
        self.mods.display_mods().contains(KeyModifiers::ALT)
    }

    /// Whether `self` and `other` are the same key differing by *exactly* the Alt modifier — the
    /// pairing the help overlay folds into one "X / Alt-X" row (e.g. `h`/`Alt-h`, `Ctrl-z`/
    /// `Ctrl-Alt-z`). Same code but a *different* modifier — `c` vs `Ctrl-c` — is not a pair.
    pub fn is_alt_pair(&self, other: &Binding) -> bool {
        self.code == other.code
            && self.mods.display_mods() ^ other.mods.display_mods() == KeyModifiers::ALT
    }

    /// Render the chord for the help overlay, e.g. `Alt-h`, `Ctrl-z`, `Space f`, `↑`. Chords that
    /// arm a capture get a trailing `␣` placeholder (`f ␣`, `Ctrl-s ␣`) to signal that one more
    /// keystroke is expected.
    pub fn key_label(&self) -> String {
        let mut s = String::new();
        if self.ctx == KeyContext::Leader {
            s.push_str("Space ");
        }
        let m = self.mods.display_mods();
        if m.contains(KeyModifiers::CONTROL) {
            s.push_str("Ctrl-");
        }
        if m.contains(KeyModifiers::ALT) {
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
        other => format!("{other:?}"),
    }
}

/// First binding in `ctx`'s table whose chord matches. Tables are scanned in declaration order, so
/// more-specific arms (e.g. `Alt-h`) must precede catch-all ones (e.g. `h`) — exactly the ordering
/// the old `match` relied on.
pub fn lookup(ctx: KeyContext, code: KeyCode, mods: KeyModifiers) -> Option<&'static Binding> {
    table(ctx).iter().find(|b| b.matches(code, mods))
}

/// Every binding, in context order — for the help overlay.
pub fn all() -> impl Iterator<Item = &'static Binding> {
    [
        KeyContext::Normal,
        KeyContext::Global,
        KeyContext::Insert,
        KeyContext::Search,
        KeyContext::Leader,
    ]
    .into_iter()
    .flat_map(|c| table(c).iter())
}

fn table(ctx: KeyContext) -> &'static [Binding] {
    match ctx {
        KeyContext::Normal => NORMAL,
        KeyContext::Insert => INSERT,
        KeyContext::Search => SEARCH,
        KeyContext::Leader => LEADER,
        KeyContext::Global => GLOBAL,
    }
}

// Shorthands to keep the tables legible.
use Action as A;
use KeyContext::{
    Global as GLOBAL_CTX, Insert as INSERT_CTX, Leader as LEADER_CTX, Normal as NORMAL_CTX,
    Search as SEARCH_CTX,
};
use ModPattern::{Any, Exact, IgnoreShift};

const NONE: KeyModifiers = KeyModifiers::NONE;
const ALT: KeyModifiers = KeyModifiers::ALT;
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
const CTRL_ALT: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);

const fn ch(c: char) -> KeyCode {
    KeyCode::Char(c)
}

macro_rules! bind {
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
    bind!(NORMAL_CTX, KeyCode::Esc, Any, A::DropSearch, "Search", "Clear the active search"),
    bind!(NORMAL_CTX, ch('c'), Exact(NONE), A::CollapseSelection, "Selection", "Collapse selection"),
    bind!(NORMAL_CTX, ch('o'), Exact(NONE), A::SwapAnchor, "Selection", "Swap cursor and anchor"),
    bind!(NORMAL_CTX, ch('y'), Exact(NONE), A::TreeExpand, "Selection", "Expand selection to parent syntax node"),
    bind!(NORMAL_CTX, ch('y'), Exact(ALT), A::TreeContract, "Selection", "Contract selection to child syntax node"),
    bind!(NORMAL_CTX, ch('z'), Exact(ALT), A::MotionRedo, "Selection", "Redo cursor/selection motion"),
    bind!(NORMAL_CTX, ch('z'), Exact(NONE), A::MotionUndo, "Selection", "Undo cursor/selection motion"),
    bind!(NORMAL_CTX, ch('r'), IgnoreShift(NONE), A::RepeatMotion, "Selection", "Repeat last motion"),

    // ---- motions: chars / lines ----
    bind!(NORMAL_CTX, KeyCode::Home, Any, A::MoveLineStart, "Motion", "Logical line start"),
    bind!(NORMAL_CTX, KeyCode::End, Any, A::MoveLineEnd, "Motion", "Logical line end"),
    bind!(NORMAL_CTX, ch('h'), IgnoreShift(ALT), A::MoveLineFirstNonblank, "Motion", "First non-blank of line"),
    bind!(NORMAL_CTX, ch('h'), IgnoreShift(NONE), A::MoveChar(Direction::Backward), "Motion", "Character left"),
    bind!(NORMAL_CTX, ch('l'), IgnoreShift(ALT), A::MoveLineEnd, "Motion", "End of line"),
    bind!(NORMAL_CTX, ch('l'), IgnoreShift(NONE), A::MoveChar(Direction::Forward), "Motion", "Character right"),
    bind!(NORMAL_CTX, ch('k'), IgnoreShift(ALT), A::MoveVisualLine(VerticalDirection::Up), "Motion", "Visual row up"),
    bind!(NORMAL_CTX, ch('k'), IgnoreShift(NONE), A::MoveLogicalLine(Direction::Backward), "Motion", "Logical line up"),
    bind!(NORMAL_CTX, ch('j'), IgnoreShift(ALT), A::MoveVisualLine(VerticalDirection::Down), "Motion", "Visual row down"),
    bind!(NORMAL_CTX, ch('j'), IgnoreShift(NONE), A::MoveLogicalLine(Direction::Forward), "Motion", "Logical line down"),
    bind!(NORMAL_CTX, ch('0'), IgnoreShift(NONE), A::MoveLineStart, "Motion", "Logical line start"),

    // ---- motions: page / half-page (cursor) ----
    bind!(NORMAL_CTX, ch('d'), IgnoreShift(NONE), A::PageMotion { dir: VerticalDirection::Down, half: false }, "Motion", "Cursor down a page"),
    bind!(NORMAL_CTX, ch('u'), IgnoreShift(NONE), A::PageMotion { dir: VerticalDirection::Up, half: false }, "Motion", "Cursor up a page"),
    bind!(NORMAL_CTX, ch('d'), IgnoreShift(ALT), A::PageMotion { dir: VerticalDirection::Down, half: true }, "Motion", "Cursor down half a page"),
    bind!(NORMAL_CTX, ch('u'), IgnoreShift(ALT), A::PageMotion { dir: VerticalDirection::Up, half: true }, "Motion", "Cursor up half a page"),

    // ---- motions: words ----
    bind!(NORMAL_CTX, ch('w'), IgnoreShift(ALT), A::MoveWord { dir: Direction::Forward, boundary: WordBoundary::Word }, "Motion", "Small word forward"),
    bind!(NORMAL_CTX, ch('w'), IgnoreShift(NONE), A::MoveWord { dir: Direction::Forward, boundary: WordBoundary::BigWord }, "Motion", "Big word forward"),
    bind!(NORMAL_CTX, ch('b'), IgnoreShift(ALT), A::MoveWord { dir: Direction::Backward, boundary: WordBoundary::Word }, "Motion", "Small word backward"),
    bind!(NORMAL_CTX, ch('b'), IgnoreShift(NONE), A::MoveWord { dir: Direction::Backward, boundary: WordBoundary::BigWord }, "Motion", "Big word backward"),
    bind!(NORMAL_CTX, ch('e'), IgnoreShift(ALT), A::MoveWordEnd { dir: Direction::Forward, boundary: WordBoundary::Word }, "Motion", "Small word end"),
    bind!(NORMAL_CTX, ch('e'), Any, A::MoveWordEnd { dir: Direction::Forward, boundary: WordBoundary::BigWord }, "Motion", "Big word end"),

    // ---- motions: find char ----
    bind!(NORMAL_CTX, ch('f'), IgnoreShift(ALT), A::BeginFind { dir: Direction::Backward, till: false }, "Motion", "Find character backward"),
    bind!(NORMAL_CTX, ch('f'), IgnoreShift(NONE), A::BeginFind { dir: Direction::Forward, till: false }, "Motion", "Find character forward"),
    bind!(NORMAL_CTX, ch('t'), IgnoreShift(ALT), A::BeginFind { dir: Direction::Backward, till: true }, "Motion", "Till character backward"),
    bind!(NORMAL_CTX, ch('t'), IgnoreShift(NONE), A::BeginFind { dir: Direction::Forward, till: true }, "Motion", "Till character forward"),

    // ---- motions: brackets / nav units / goto ----
    bind!(NORMAL_CTX, ch('m'), IgnoreShift(NONE), A::MatchBracket { inner: false }, "Motion", "Matching bracket"),
    bind!(NORMAL_CTX, ch('m'), IgnoreShift(ALT), A::MatchBracket { inner: true }, "Motion", "Inner matching bracket"),
    bind!(NORMAL_CTX, ch(']'), Exact(NONE), A::NavUnit(Direction::Forward), "Navigation", "Next navigation unit"),
    bind!(NORMAL_CTX, ch('['), Exact(NONE), A::NavUnit(Direction::Backward), "Navigation", "Previous navigation unit"),
    bind!(NORMAL_CTX, ch('}'), Any, A::NavUnitEdge { start: false }, "Navigation", "Select to end of unit"),
    bind!(NORMAL_CTX, ch('{'), Any, A::NavUnitEdge { start: true }, "Navigation", "Select to start of unit"),
    bind!(NORMAL_CTX, ch('g'), IgnoreShift(ALT), A::GotoLine { last: true }, "Motion", "Go to last line"),
    bind!(NORMAL_CTX, ch('g'), IgnoreShift(NONE), A::GotoLine { last: false }, "Motion", "Go to line (count, default 1)"),

    // ---- line selection ----
    bind!(NORMAL_CTX, ch('x'), IgnoreShift(NONE), A::SelectLine(Direction::Forward), "Selection", "Select line downward"),
    bind!(NORMAL_CTX, ch('x'), IgnoreShift(ALT), A::SelectLine(Direction::Backward), "Selection", "Select line upward"),

    // ---- mode transitions ----
    bind!(NORMAL_CTX, ch('i'), Exact(NONE), A::EnterInsert(InsertWhere::SelectionStart), "Mode", "Insert at selection start"),
    bind!(NORMAL_CTX, ch('a'), Exact(NONE), A::EnterInsert(InsertWhere::SelectionEnd), "Mode", "Insert at selection end"),
    bind!(NORMAL_CTX, ch('i'), Exact(ALT), A::EnterInsert(InsertWhere::FirstLineStart), "Mode", "Insert at first line start"),
    bind!(NORMAL_CTX, ch('a'), Exact(ALT), A::EnterInsert(InsertWhere::LastLineEnd), "Mode", "Insert at last line end"),

    // ---- viewport scroll ----
    bind!(NORMAL_CTX, KeyCode::PageDown, Any, A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Page }, "Scroll", "Scroll page down"),
    bind!(NORMAL_CTX, KeyCode::PageUp, Any, A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Page }, "Scroll", "Scroll page up"),
    bind!(NORMAL_CTX, KeyCode::Up, IgnoreShift(ALT), A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Half }, "Scroll", "Scroll half page up"),
    bind!(NORMAL_CTX, KeyCode::Down, IgnoreShift(ALT), A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Half }, "Scroll", "Scroll half page down"),
    bind!(NORMAL_CTX, KeyCode::Up, Any, A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Line }, "Scroll", "Scroll up one line"),
    bind!(NORMAL_CTX, KeyCode::Down, Any, A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Line }, "Scroll", "Scroll down one line"),
    bind!(NORMAL_CTX, KeyCode::Left, IgnoreShift(ALT), A::Scroll { dir: ScrollDir::Left, unit: ScrollUnit::Half }, "Scroll", "Scroll half page left"),
    bind!(NORMAL_CTX, KeyCode::Right, IgnoreShift(ALT), A::Scroll { dir: ScrollDir::Right, unit: ScrollUnit::Half }, "Scroll", "Scroll half page right"),
    bind!(NORMAL_CTX, KeyCode::Left, Any, A::Scroll { dir: ScrollDir::Left, unit: ScrollUnit::Line }, "Scroll", "Scroll left one column"),
    bind!(NORMAL_CTX, KeyCode::Right, Any, A::Scroll { dir: ScrollDir::Right, unit: ScrollUnit::Line }, "Scroll", "Scroll right one column"),
    bind!(NORMAL_CTX, ch('-'), Exact(NONE), A::CenterCursor, "Scroll", "Center cursor in window"),

    // ---- delete / search / grep ----
    bind!(NORMAL_CTX, KeyCode::Delete, Any, A::DeleteSelection, "Edit", "Delete selection"),
    bind!(NORMAL_CTX, ch('/'), IgnoreShift(NONE), A::EnterSearch, "Search", "Search"),
    bind!(NORMAL_CTX, ch('/'), Exact(ALT), A::SearchFromSelection, "Search", "Search for selection"),
    bind!(NORMAL_CTX, ch('n'), IgnoreShift(ALT), A::SearchCycle(Direction::Backward), "Search", "Previous match"),
    bind!(NORMAL_CTX, ch('n'), IgnoreShift(NONE), A::SearchCycle(Direction::Forward), "Search", "Next match"),
    bind!(NORMAL_CTX, ch('>'), Any, A::GrepNavigate(Direction::Forward), "Search", "Next grep hit"),
    bind!(NORMAL_CTX, ch('<'), Any, A::GrepNavigate(Direction::Backward), "Search", "Previous grep hit"),

    // ---- selection editing / clipboard (Ctrl; shared keys with Insert, but selection-scoped here) ----
    bind!(NORMAL_CTX, ch('c'), Exact(CTRL), A::Change, "Edit", "Change selection"),
    bind!(NORMAL_CTX, ch('d'), Exact(CTRL), A::DeleteSelection, "Edit", "Delete selection"),
    bind!(NORMAL_CTX, ch('y'), Exact(CTRL), A::Copy, "Clipboard", "Copy selection"),
    bind!(NORMAL_CTX, ch('x'), Exact(CTRL), A::Cut, "Clipboard", "Cut selection"),
    bind!(NORMAL_CTX, ch('v'), Exact(CTRL), A::Paste, "Clipboard", "Paste before selection"),
    bind!(NORMAL_CTX, ch('r'), Exact(CTRL), A::ReplaceClipboard, "Clipboard", "Replace selection with clipboard"),
    bind!(NORMAL_CTX, ch('s'), Exact(CTRL_ALT), A::Unsurround(SurroundTarget::Selection), "Edit", "Unsurround selection"),
    bind!(NORMAL_CTX, ch('s'), Exact(CTRL), A::BeginSurround(SurroundTarget::Selection), "Edit", "Surround selection"),

    // ---- leader ----
    bind!(NORMAL_CTX, ch(' '), Exact(NONE), A::BeginLeader, "Leader", "Space leader chord"),
];

#[rustfmt::skip]
static GLOBAL: &[Binding] = &[
    bind!(GLOBAL_CTX, ch('p'), Exact(CTRL), A::ToggleWrap, "View", "Toggle soft wrap"),
    bind!(GLOBAL_CTX, ch('z'), Exact(CTRL), A::Undo, "Edit", "Undo"),
    bind!(GLOBAL_CTX, ch('z'), Exact(CTRL_ALT), A::Redo, "Edit", "Redo"),
    bind!(GLOBAL_CTX, ch('j'), Exact(CTRL), A::MoveLines(VerticalDirection::Down), "Edit", "Move line(s) down"),
    bind!(GLOBAL_CTX, ch('k'), Exact(CTRL), A::MoveLines(VerticalDirection::Up), "Edit", "Move line(s) up"),
    bind!(GLOBAL_CTX, ch('g'), Exact(CTRL), A::JoinLines, "Edit", "Join lines"),
    bind!(GLOBAL_CTX, ch('l'), Exact(CTRL), A::Indent, "Edit", "Indent"),
    bind!(GLOBAL_CTX, ch('h'), Exact(CTRL), A::Dedent, "Edit", "Dedent"),
    bind!(GLOBAL_CTX, ch('t'), Exact(CTRL), A::ToggleComment, "Edit", "Toggle comment"),
    bind!(GLOBAL_CTX, ch('o'), Exact(CTRL), A::OpenLineBelow, "Edit", "Open line below"),
    bind!(GLOBAL_CTX, ch('o'), Exact(CTRL_ALT), A::OpenLineAbove, "Edit", "Open line above"),
    // The selection/clipboard Ctrl keys (Ctrl-y/x/v/c/d/r) are *not* here: they act on the
    // selection in Normal and on the line in Insert, so each mode binds its own action (see the
    // tails of the NORMAL and INSERT tables) instead of sharing one with a runtime mode-branch.
];

#[rustfmt::skip]
static INSERT: &[Binding] = &[
    bind!(INSERT_CTX, KeyCode::Esc, Any, A::LeaveInsert, "Mode", "Leave insert mode"),
    bind!(INSERT_CTX, KeyCode::Backspace, Any, A::Backspace, "Edit", "Delete character before cursor"),
    bind!(INSERT_CTX, KeyCode::Delete, Any, A::DeletePoint, "Edit", "Delete character at cursor"),
    bind!(INSERT_CTX, KeyCode::Enter, Any, A::NewlineIndent, "Edit", "Newline and indent"),
    bind!(INSERT_CTX, KeyCode::Tab, Any, A::InsertTab, "Edit", "Insert tab"),
    bind!(INSERT_CTX, KeyCode::Left, Any, A::MoveChar(Direction::Backward), "Motion", "Cursor left"),
    bind!(INSERT_CTX, KeyCode::Right, Any, A::MoveChar(Direction::Forward), "Motion", "Cursor right"),
    bind!(INSERT_CTX, KeyCode::Up, Any, A::MoveVisualLine(VerticalDirection::Up), "Motion", "Cursor up"),
    bind!(INSERT_CTX, KeyCode::Down, Any, A::MoveVisualLine(VerticalDirection::Down), "Motion", "Cursor down"),

    // ---- line editing / clipboard (Ctrl; same keys as Normal, but line-scoped — Insert has no selection) ----
    bind!(INSERT_CTX, ch('c'), Exact(CTRL), A::ChangeLine, "Edit", "Change line"),
    bind!(INSERT_CTX, ch('d'), Exact(CTRL), A::DeleteLine, "Edit", "Delete line"),
    bind!(INSERT_CTX, ch('y'), Exact(CTRL), A::CopyLine, "Clipboard", "Copy line"),
    bind!(INSERT_CTX, ch('x'), Exact(CTRL), A::CutLine, "Clipboard", "Cut line"),
    bind!(INSERT_CTX, ch('v'), Exact(CTRL), A::PasteAtCursor, "Clipboard", "Paste at cursor"),
    bind!(INSERT_CTX, ch('r'), Exact(CTRL), A::ReplaceLineClipboard, "Clipboard", "Replace line with clipboard"),
    bind!(INSERT_CTX, ch('s'), Exact(CTRL_ALT), A::Unsurround(SurroundTarget::Line), "Edit", "Unsurround line"),
    bind!(INSERT_CTX, ch('s'), Exact(CTRL), A::BeginSurround(SurroundTarget::Line), "Edit", "Surround line"),
];

#[rustfmt::skip]
static SEARCH: &[Binding] = &[
    bind!(SEARCH_CTX, KeyCode::Esc, Any, A::SearchAbort, "Search", "Abort search"),
    bind!(SEARCH_CTX, KeyCode::Enter, Any, A::SearchCommit, "Search", "Commit search"),
    bind!(SEARCH_CTX, KeyCode::Up, Any, A::SearchHistoryPrev, "Search", "Previous query in history"),
    bind!(SEARCH_CTX, KeyCode::Down, Any, A::SearchHistoryNext, "Search", "Next query in history"),
    bind!(SEARCH_CTX, KeyCode::Left, Any, A::SearchCursorLeft, "Search", "Move cursor left"),
    bind!(SEARCH_CTX, KeyCode::Right, Any, A::SearchCursorRight, "Search", "Move cursor right"),
    bind!(SEARCH_CTX, KeyCode::Backspace, Any, A::SearchBackspace, "Search", "Delete character"),
];

#[rustfmt::skip]
static LEADER: &[Binding] = &[
    bind!(LEADER_CTX, ch('f'), Exact(NONE), A::OpenPicker(PickerKind::Files), "Files", "Find files"),
    bind!(LEADER_CTX, ch('b'), Exact(NONE), A::OpenPicker(PickerKind::Buffers), "Files", "Switch buffer"),
    bind!(LEADER_CTX, ch('g'), Exact(NONE), A::OpenPicker(PickerKind::Grep), "Files", "Grep workspace"),
    bind!(LEADER_CTX, ch('e'), Exact(NONE), A::OpenPicker(PickerKind::Explorer), "Files", "File explorer"),
    bind!(LEADER_CTX, ch('p'), Exact(NONE), A::OpenPicker(PickerKind::Projects), "Project", "Switch project"),
    bind!(LEADER_CTX, ch(','), Exact(NONE), A::OpenProjectSettings, "Project", "Project settings"),
    bind!(LEADER_CTX, ch('q'), Exact(NONE), A::Quit, "App", "Quit"),
    bind!(LEADER_CTX, ch('w'), Exact(NONE), A::CloseBuffer, "App", "Close buffer"),
    bind!(LEADER_CTX, ch('s'), Exact(NONE), A::Save, "App", "Save"),
    bind!(LEADER_CTX, ch('s'), Exact(ALT), A::SaveAs, "App", "Save as"),
    bind!(LEADER_CTX, ch('r'), Exact(NONE), A::Reload, "App", "Reload from disk"),
    bind!(LEADER_CTX, ch('n'), Exact(NONE), A::NewScratch, "App", "New scratch buffer"),
    bind!(LEADER_CTX, ch('?'), Any, A::OpenHelp, "App", "Show keyboard shortcuts"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every binding must have help text — the overlay is generated from these, so a blank would
    /// show up as a gap. Guards against forgetting `desc` on a new binding.
    #[test]
    fn every_binding_is_documented() {
        for b in all() {
            assert!(
                !b.desc.is_empty(),
                "binding {} has no description",
                b.key_label()
            );
            assert!(
                !b.group.is_empty(),
                "binding {} has no group",
                b.key_label()
            );
        }
    }

    /// Capture-arming chords (`f`/`t` find, `Ctrl-s` surround) render with a trailing `␣`
    /// placeholder; immediate actions (incl. `Ctrl-Alt-s` unsurround) don't.
    #[test]
    fn awaiting_chords_render_placeholder() {
        let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;

        let f = lookup(KeyContext::Normal, KeyCode::Char('f'), KeyModifiers::NONE).unwrap();
        assert!(f.action.awaits_key());
        assert_eq!(f.key_label(), "f ␣");

        let surround =
            lookup(KeyContext::Normal, KeyCode::Char('s'), KeyModifiers::CONTROL).unwrap();
        assert!(surround.action.awaits_key());
        assert_eq!(surround.key_label(), "Ctrl-s ␣");

        // Unsurround acts immediately — no placeholder.
        let unsurround = lookup(KeyContext::Normal, KeyCode::Char('s'), ctrl_alt).unwrap();
        assert!(!unsurround.action.awaits_key());
        assert_eq!(unsurround.key_label(), "Ctrl-Alt-s");

        // A plain motion never awaits.
        let left = lookup(KeyContext::Normal, KeyCode::Char('h'), KeyModifiers::NONE).unwrap();
        assert!(!left.action.awaits_key());
        assert_eq!(left.key_label(), "h");
    }

    /// No two bindings in a context should match the *exact same* event, which would make the
    /// later one dead (first-match wins). We can't enumerate every KeyModifiers combo, but exact
    /// duplicates on (code, representative mods) are the common mistake.
    #[test]
    fn no_shadowed_duplicates() {
        for ctx in [
            KeyContext::Normal,
            KeyContext::Insert,
            KeyContext::Search,
            KeyContext::Leader,
            KeyContext::Global,
        ] {
            let rows = table(ctx);
            for (i, a) in rows.iter().enumerate() {
                for b in &rows[i + 1..] {
                    if a.code != b.code {
                        continue;
                    }
                    // Two rows on the same key conflict only if an earlier row's pattern already
                    // subsumes the later one. `Any` subsumes everything; identical patterns clash.
                    let earlier_swallows =
                        matches!(a.mods, ModPattern::Any) || same_pattern(a.mods, b.mods);
                    assert!(
                        !earlier_swallows,
                        "{:?} binding on {:?} is shadowed by an earlier row",
                        ctx, b.code
                    );
                }
            }
        }
    }

    fn same_pattern(a: ModPattern, b: ModPattern) -> bool {
        match (a, b) {
            (ModPattern::Exact(x), ModPattern::Exact(y)) => x == y,
            (ModPattern::IgnoreShift(x), ModPattern::IgnoreShift(y)) => x == y,
            (ModPattern::Any, ModPattern::Any) => true,
            _ => false,
        }
    }

    #[test]
    fn representative_lookups_resolve() {
        // Plain `h` and `Shift-h` both reach MoveChar(Backward) via IgnoreShift.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Char('h'), KeyModifiers::NONE).map(|b| b.action),
            Some(Action::MoveChar(Direction::Backward))
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Char('h'), KeyModifiers::SHIFT).map(|b| b.action),
            Some(Action::MoveChar(Direction::Backward))
        ));
        // `Alt-h` is a distinct, earlier arm.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Char('h'), KeyModifiers::ALT).map(|b| b.action),
            Some(Action::MoveLineFirstNonblank)
        ));
        // Mode-identical Ctrl shortcuts live in the Global table, not Normal.
        assert!(lookup(
            KeyContext::Normal,
            KeyCode::Char('z'),
            KeyModifiers::CONTROL
        )
        .is_none());
        assert!(matches!(
            lookup(
                KeyContext::Global,
                KeyCode::Char('z'),
                KeyModifiers::CONTROL
            )
            .map(|b| b.action),
            Some(Action::Undo)
        ));
        // Leader `Space ?` opens help.
        assert!(matches!(
            lookup(KeyContext::Leader, KeyCode::Char('?'), KeyModifiers::NONE).map(|b| b.action),
            Some(Action::OpenHelp)
        ));
    }

    /// The selection/clipboard Ctrl keys are *mode-divergent*: each mode binds its own action
    /// (selection-scoped in Normal, line-scoped in Insert), so they live in those tables — not
    /// Global — and never collide with the bare-key bindings on the same letter.
    #[test]
    fn mode_divergent_ctrl_bindings_split_by_mode() {
        let ctrl = KeyModifiers::CONTROL;
        // Not in the shared Global table.
        for code in ['c', 'd', 'y', 'x', 'v', 'r'] {
            assert!(
                lookup(KeyContext::Global, KeyCode::Char(code), ctrl).is_none(),
                "Ctrl-{code} should not be in Global"
            );
        }
        // Normal → selection actions; Insert → the line counterparts.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Char('d'), ctrl).map(|b| b.action),
            Some(Action::DeleteSelection)
        ));
        assert!(matches!(
            lookup(KeyContext::Insert, KeyCode::Char('d'), ctrl).map(|b| b.action),
            Some(Action::DeleteLine)
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Char('c'), ctrl).map(|b| b.action),
            Some(Action::Change)
        ));
        assert!(matches!(
            lookup(KeyContext::Insert, KeyCode::Char('c'), ctrl).map(|b| b.action),
            Some(Action::ChangeLine)
        ));
        // The bare letter is unshadowed: `c` is still Collapse, not Change.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Char('c'), KeyModifiers::NONE).map(|b| b.action),
            Some(Action::CollapseSelection)
        ));
    }

    /// `is_alt_pair` folds only a key and its *Alt* variant — not same-letter chords under a
    /// different modifier (the bug that put `Ctrl-c` next to bare `c` in the help overlay).
    #[test]
    fn is_alt_pair_matches_only_alt_variants() {
        let find = |ctx, code, mods| lookup(ctx, code, mods).unwrap();
        let h = find(KeyContext::Normal, KeyCode::Char('h'), KeyModifiers::NONE);
        let alt_h = find(KeyContext::Normal, KeyCode::Char('h'), KeyModifiers::ALT);
        let ctrl_z = find(
            KeyContext::Global,
            KeyCode::Char('z'),
            KeyModifiers::CONTROL,
        );
        let ctrl_alt_z = find(
            KeyContext::Global,
            KeyCode::Char('z'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        let c = find(KeyContext::Normal, KeyCode::Char('c'), KeyModifiers::NONE);
        let ctrl_c = find(
            KeyContext::Normal,
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        );
        // True Alt pairs (base/Alt, and Ctrl/Ctrl-Alt) fold.
        assert!(h.is_alt_pair(alt_h) && alt_h.is_alt_pair(h));
        assert!(ctrl_z.is_alt_pair(ctrl_alt_z));
        // Same letter, *different* modifier (None vs Ctrl) is not a pair.
        assert!(!c.is_alt_pair(ctrl_c));
    }

    /// Alt variants whose original guard was `m.contains(ALT)` must still match when Shift is also
    /// held (Shift there means "extend the selection"). Regression test for translating those arms
    /// to `Exact(ALT)`, which silently dropped `Alt-Shift-<motion>`.
    #[test]
    fn alt_shift_motions_still_resolve() {
        let alt = KeyModifiers::ALT;
        let alt_shift = KeyModifiers::ALT | KeyModifiers::SHIFT;
        for code in ['h', 'l', 'j', 'k', 'w', 'b', 'e', 'd', 'u', 'g', 'x', 'n'] {
            let plain = lookup(KeyContext::Normal, KeyCode::Char(code), alt);
            let shifted = lookup(KeyContext::Normal, KeyCode::Char(code), alt_shift);
            assert!(plain.is_some(), "Alt-{code} should resolve");
            assert!(
                shifted.is_some(),
                "Alt-Shift-{code} should resolve to the same action as Alt-{code}"
            );
            // Same binding row reached either way.
            assert!(
                std::ptr::eq(plain.unwrap(), shifted.unwrap()),
                "Alt-{code} and Alt-Shift-{code} should hit the same binding"
            );
        }
    }
}
