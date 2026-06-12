//! Data-driven keybindings — a port of `aether-tui/src/keymap.rs` onto iced key types.
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

/// Layout-resolved key identity, normalised from `iced::keyboard::Key`: letters lowercase
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
    const NONE: Mods = Mods { ctrl: false, alt: false, shift: false };
    const ALT: Mods = Mods { ctrl: false, alt: true, shift: false };
    const CTRL: Mods = Mods { ctrl: true, alt: false, shift: false };
    const CTRL_ALT: Mods = Mods { ctrl: true, alt: true, shift: false };

    fn without_shift(self) -> Mods {
        Mods { shift: false, ..self }
    }
}

impl From<iced::keyboard::Modifiers> for Mods {
    fn from(m: iced::keyboard::Modifiers) -> Self {
        Mods {
            ctrl: m.control(),
            alt: m.alt(),
            shift: m.shift(),
        }
    }
}

/// Normalise an iced key to our [`KeyCode`]. `None` for keys we don't bind (modifiers
/// themselves, function keys, …).
pub fn keycode(key: &iced::keyboard::Key) -> Option<KeyCode> {
    use iced::keyboard::key::Named;
    use iced::keyboard::Key;
    Some(match key {
        Key::Character(s) => {
            let mut chars = s.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c.to_ascii_lowercase())
        }
        Key::Named(named) => match named {
            Named::Space => KeyCode::Char(' '),
            Named::Escape => KeyCode::Esc,
            Named::Enter => KeyCode::Enter,
            Named::Tab => KeyCode::Tab,
            Named::Backspace => KeyCode::Backspace,
            Named::Delete => KeyCode::Delete,
            Named::Home => KeyCode::Home,
            Named::End => KeyCode::End,
            Named::PageUp => KeyCode::PageUp,
            Named::PageDown => KeyCode::PageDown,
            Named::ArrowLeft => KeyCode::Left,
            Named::ArrowRight => KeyCode::Right,
            Named::ArrowUp => KeyCode::Up,
            Named::ArrowDown => KeyCode::Down,
            _ => return None,
        },
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyContext {
    Normal,
    Insert,
    Search,
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
    fn matches(self, mods: Mods) -> bool {
        match self {
            ModPattern::Exact(m) => mods == m,
            ModPattern::IgnoreShift(base) => mods.without_shift() == base,
            ModPattern::Any => true,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ScrollDir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug)]
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
    MoveWord { dir: Direction, boundary: WordBoundary },
    MoveWordEnd { dir: Direction, boundary: WordBoundary },
    MoveVisualLine(VerticalDirection),
    MoveLogicalLine(Direction),
    MoveLineStart,
    MoveLineEnd,
    MoveLineFirstNonblank,
    MoveLogicalLineFirstNonblank(Direction),
    GotoLine { last: bool },
    MatchBracket { inner: bool },
    PageMotion { dir: VerticalDirection, half: bool },
    NavUnit(Direction),
    NavUnitEdge { start: bool },
    BeginFind { dir: Direction, till: bool },

    // ---- selection ----
    SelectLine(Direction),
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
    Scroll { dir: ScrollDir, unit: ScrollUnit },
    ToggleWrap,

    // ---- mode transitions ----
    EnterInsert(InsertWhere),
    LeaveInsert,
    BeginLeader,

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
    SearchCursorLeft,
    SearchCursorRight,
    SearchBackspace,
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
}

impl Action {
    /// Whether `r`/`Shift-r` replays this action — the TUI's `is_repeatable`: every
    /// cursor/selection motion (absolute ones included) plus the selection motions; never
    /// edits, scroll, or the non-motion selection ops. (`SearchCycle` joins when search lands.)
    pub fn is_repeatable(&self) -> bool {
        matches!(
            self,
            Action::MoveChar(_)
                | Action::MoveWord { .. }
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
                | Action::SelectLine(_)
                | Action::TreeExpand
                | Action::TreeContract
                | Action::SearchCycle(_)
        )
    }
}

pub struct Binding {
    /// Kept for table-shape parity with the TUI's `Binding` (and the future help overlay);
    /// `lookup` selects the table directly so it never reads this.
    #[allow(dead_code)]
    pub ctx: KeyContext,
    pub code: KeyCode,
    pub mods: ModPattern,
    pub action: Action,
}

impl Binding {
    fn matches(&self, code: KeyCode, mods: Mods) -> bool {
        self.code == code && self.mods.matches(mods)
    }
}

/// First binding in `ctx`'s table whose chord matches, scanning in declaration order.
pub fn lookup(ctx: KeyContext, code: KeyCode, mods: Mods) -> Option<&'static Binding> {
    let table: &[Binding] = match ctx {
        KeyContext::Normal => NORMAL,
        KeyContext::Insert => INSERT,
        KeyContext::Search => SEARCH,
        KeyContext::Leader => LEADER,
        KeyContext::Global => GLOBAL,
    };
    table.iter().find(|b| b.matches(code, mods))
}

use Action as A;
use KeyContext::{Global as G, Insert as I, Leader as L, Normal as N};
use ModPattern::{Any, Exact, IgnoreShift};

const fn ch(c: char) -> KeyCode {
    KeyCode::Char(c)
}

macro_rules! bind {
    ($ctx:expr, $code:expr, $mods:expr, $action:expr) => {
        Binding { ctx: $ctx, code: $code, mods: $mods, action: $action }
    };
}

#[rustfmt::skip]
static NORMAL: &[Binding] = &[
    // ---- meta / selection ----
    bind!(N, KeyCode::Esc, Any, A::DropSearch),
    bind!(N, ch('c'), Exact(Mods::NONE), A::CollapseSelection),
    bind!(N, ch('o'), Exact(Mods::NONE), A::SwapAnchor),
    bind!(N, ch('y'), Exact(Mods::NONE), A::TreeExpand),
    bind!(N, ch('y'), Exact(Mods::ALT), A::TreeContract),
    bind!(N, ch('z'), Exact(Mods::ALT), A::MotionRedo),
    bind!(N, ch('z'), Exact(Mods::NONE), A::MotionUndo),
    bind!(N, ch('r'), IgnoreShift(Mods::NONE), A::RepeatMotion),

    // ---- motions: chars / lines ----
    bind!(N, KeyCode::Home, Any, A::MoveLineStart),
    bind!(N, KeyCode::End, Any, A::MoveLineEnd),
    bind!(N, ch('h'), IgnoreShift(Mods::ALT), A::MoveLineFirstNonblank),
    bind!(N, ch('h'), IgnoreShift(Mods::NONE), A::MoveChar(Direction::Backward)),
    bind!(N, ch('l'), IgnoreShift(Mods::ALT), A::MoveLineEnd),
    bind!(N, ch('l'), IgnoreShift(Mods::NONE), A::MoveChar(Direction::Forward)),
    bind!(N, ch('k'), IgnoreShift(Mods::ALT), A::MoveVisualLine(VerticalDirection::Up)),
    bind!(N, ch('k'), IgnoreShift(Mods::NONE), A::MoveLogicalLine(Direction::Backward)),
    bind!(N, ch('j'), IgnoreShift(Mods::ALT), A::MoveVisualLine(VerticalDirection::Down)),
    bind!(N, ch('j'), IgnoreShift(Mods::NONE), A::MoveLogicalLine(Direction::Forward)),
    bind!(N, ch('0'), IgnoreShift(Mods::NONE), A::MoveLineStart),
    bind!(N, KeyCode::Enter, IgnoreShift(Mods::NONE), A::MoveLogicalLineFirstNonblank(Direction::Forward)),
    bind!(N, KeyCode::Backspace, IgnoreShift(Mods::NONE), A::MoveLogicalLineFirstNonblank(Direction::Backward)),

    // ---- motions: page / half-page ----
    bind!(N, ch('d'), IgnoreShift(Mods::NONE), A::PageMotion { dir: VerticalDirection::Down, half: false }),
    bind!(N, ch('u'), IgnoreShift(Mods::NONE), A::PageMotion { dir: VerticalDirection::Up, half: false }),
    bind!(N, ch('d'), IgnoreShift(Mods::ALT), A::PageMotion { dir: VerticalDirection::Down, half: true }),
    bind!(N, ch('u'), IgnoreShift(Mods::ALT), A::PageMotion { dir: VerticalDirection::Up, half: true }),

    // ---- motions: words ----
    bind!(N, ch('w'), IgnoreShift(Mods::ALT), A::MoveWord { dir: Direction::Forward, boundary: WordBoundary::BigWord }),
    bind!(N, ch('w'), IgnoreShift(Mods::NONE), A::MoveWord { dir: Direction::Forward, boundary: WordBoundary::Word }),
    bind!(N, ch('b'), IgnoreShift(Mods::ALT), A::MoveWord { dir: Direction::Backward, boundary: WordBoundary::BigWord }),
    bind!(N, ch('b'), IgnoreShift(Mods::NONE), A::MoveWord { dir: Direction::Backward, boundary: WordBoundary::Word }),
    bind!(N, ch('e'), IgnoreShift(Mods::ALT), A::MoveWordEnd { dir: Direction::Forward, boundary: WordBoundary::BigWord }),
    bind!(N, ch('e'), Any, A::MoveWordEnd { dir: Direction::Forward, boundary: WordBoundary::Word }),

    // ---- motions: find char ----
    bind!(N, ch('f'), IgnoreShift(Mods::ALT), A::BeginFind { dir: Direction::Backward, till: false }),
    bind!(N, ch('f'), IgnoreShift(Mods::NONE), A::BeginFind { dir: Direction::Forward, till: false }),
    bind!(N, ch('t'), IgnoreShift(Mods::ALT), A::BeginFind { dir: Direction::Backward, till: true }),
    bind!(N, ch('t'), IgnoreShift(Mods::NONE), A::BeginFind { dir: Direction::Forward, till: true }),

    // ---- motions: brackets / nav units / goto ----
    bind!(N, ch('m'), IgnoreShift(Mods::NONE), A::MatchBracket { inner: false }),
    bind!(N, ch('m'), IgnoreShift(Mods::ALT), A::MatchBracket { inner: true }),
    bind!(N, ch(']'), Exact(Mods::NONE), A::NavUnit(Direction::Forward)),
    bind!(N, ch('['), Exact(Mods::NONE), A::NavUnit(Direction::Backward)),
    bind!(N, ch('}'), Any, A::NavUnitEdge { start: false }),
    bind!(N, ch('{'), Any, A::NavUnitEdge { start: true }),
    bind!(N, ch('g'), IgnoreShift(Mods::ALT), A::GotoLine { last: true }),
    bind!(N, ch('g'), IgnoreShift(Mods::NONE), A::GotoLine { last: false }),

    // ---- line selection ----
    bind!(N, ch('x'), IgnoreShift(Mods::NONE), A::SelectLine(Direction::Forward)),
    bind!(N, ch('x'), IgnoreShift(Mods::ALT), A::SelectLine(Direction::Backward)),

    // ---- mode transitions ----
    bind!(N, ch('i'), Exact(Mods::NONE), A::EnterInsert(InsertWhere::SelectionStart)),
    bind!(N, ch('a'), Exact(Mods::NONE), A::EnterInsert(InsertWhere::SelectionEnd)),
    bind!(N, ch('i'), Exact(Mods::ALT), A::EnterInsert(InsertWhere::FirstLineStart)),
    bind!(N, ch('a'), Exact(Mods::ALT), A::EnterInsert(InsertWhere::LastLineEnd)),

    // ---- viewport scroll ----
    bind!(N, KeyCode::PageDown, Any, A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Page }),
    bind!(N, KeyCode::PageUp, Any, A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Page }),
    bind!(N, KeyCode::Up, IgnoreShift(Mods::ALT), A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Half }),
    bind!(N, KeyCode::Down, IgnoreShift(Mods::ALT), A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Half }),
    bind!(N, KeyCode::Up, Any, A::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Line }),
    bind!(N, KeyCode::Down, Any, A::Scroll { dir: ScrollDir::Down, unit: ScrollUnit::Line }),
    // Alt-Left/Right drive the cross-file jump list; they must precede the plain Left/Right
    // `Any` rows, which still scroll horizontally one column.
    bind!(N, KeyCode::Left, Exact(Mods::ALT), A::NavBack),
    bind!(N, KeyCode::Right, Exact(Mods::ALT), A::NavForward),
    bind!(N, KeyCode::Left, Any, A::Scroll { dir: ScrollDir::Left, unit: ScrollUnit::Line }),
    bind!(N, KeyCode::Right, Any, A::Scroll { dir: ScrollDir::Right, unit: ScrollUnit::Line }),
    bind!(N, ch('-'), Exact(Mods::NONE), A::CenterCursor),

    // ---- delete / search ----
    bind!(N, KeyCode::Delete, Any, A::DeleteSelection),
    bind!(N, ch('/'), IgnoreShift(Mods::NONE), A::EnterSearch),
    bind!(N, ch('/'), Exact(Mods::ALT), A::SearchFromSelection),
    bind!(N, ch('?'), IgnoreShift(Mods::NONE), A::EnterSearchToCursor),
    bind!(N, ch('n'), IgnoreShift(Mods::ALT), A::SearchCycle(Direction::Backward)),
    bind!(N, ch('n'), IgnoreShift(Mods::NONE), A::SearchCycle(Direction::Forward)),
    bind!(N, ch('>'), Any, A::GrepNavigate(Direction::Forward)),
    bind!(N, ch('<'), Any, A::GrepNavigate(Direction::Backward)),

    // ---- selection editing / clipboard ----
    bind!(N, ch('c'), Exact(Mods::CTRL), A::Change),
    bind!(N, ch('d'), Exact(Mods::CTRL), A::DeleteSelection),
    bind!(N, ch('y'), Exact(Mods::CTRL), A::Copy),
    bind!(N, ch('x'), Exact(Mods::CTRL), A::Cut),
    bind!(N, ch('v'), Exact(Mods::CTRL), A::Paste),
    bind!(N, ch('r'), Exact(Mods::CTRL), A::ReplaceClipboard),
    bind!(N, ch('s'), Exact(Mods::CTRL_ALT), A::Unsurround(SurroundTarget::Selection)),
    bind!(N, ch('s'), Exact(Mods::CTRL), A::BeginSurround(SurroundTarget::Selection)),

    // ---- leader ----
    bind!(N, ch(' '), Exact(Mods::NONE), A::BeginLeader),
];

#[rustfmt::skip]
static GLOBAL: &[Binding] = &[
    bind!(G, ch('z'), Exact(Mods::CTRL), A::Undo),
    bind!(G, ch('z'), Exact(Mods::CTRL_ALT), A::Redo),
    bind!(G, ch('j'), Exact(Mods::CTRL), A::MoveLines(VerticalDirection::Down)),
    bind!(G, ch('k'), Exact(Mods::CTRL), A::MoveLines(VerticalDirection::Up)),
    bind!(G, ch('g'), Exact(Mods::CTRL), A::JoinLines),
    bind!(G, ch('l'), Exact(Mods::CTRL), A::Indent),
    bind!(G, ch('h'), Exact(Mods::CTRL), A::Dedent),
    bind!(G, ch('/'), Exact(Mods::CTRL), A::ToggleComment),
    bind!(G, ch('o'), Exact(Mods::CTRL), A::OpenLineBelow),
    bind!(G, ch('o'), Exact(Mods::CTRL_ALT), A::OpenLineAbove),
];

#[rustfmt::skip]
static INSERT: &[Binding] = &[
    bind!(I, KeyCode::Esc, Any, A::LeaveInsert),
    bind!(I, KeyCode::Backspace, Any, A::Backspace),
    bind!(I, KeyCode::Delete, Any, A::DeletePoint),
    bind!(I, KeyCode::Enter, Any, A::NewlineIndent),
    bind!(I, KeyCode::Tab, Any, A::InsertTab),
    bind!(I, KeyCode::Left, Any, A::MoveChar(Direction::Backward)),
    bind!(I, KeyCode::Right, Any, A::MoveChar(Direction::Forward)),
    bind!(I, KeyCode::Up, Any, A::MoveVisualLine(VerticalDirection::Up)),
    bind!(I, KeyCode::Down, Any, A::MoveVisualLine(VerticalDirection::Down)),
    bind!(I, ch('c'), Exact(Mods::CTRL), A::ChangeLine),
    bind!(I, ch('d'), Exact(Mods::CTRL), A::DeleteLine),
    bind!(I, ch('y'), Exact(Mods::CTRL), A::CopyLine),
    bind!(I, ch('x'), Exact(Mods::CTRL), A::CutLine),
    bind!(I, ch('v'), Exact(Mods::CTRL), A::PasteAtCursor),
    bind!(I, ch('r'), Exact(Mods::CTRL), A::ReplaceLineClipboard),
    bind!(I, ch('s'), Exact(Mods::CTRL_ALT), A::Unsurround(SurroundTarget::Line)),
    bind!(I, ch('s'), Exact(Mods::CTRL), A::BeginSurround(SurroundTarget::Line)),
];

#[rustfmt::skip]
static SEARCH: &[Binding] = &[
    bind!(KeyContext::Search, KeyCode::Esc, Any, A::SearchAbort),
    bind!(KeyContext::Search, KeyCode::Enter, Any, A::SearchCommit),
    // Alt-k/j (not Up/Down) browse history — same chord as the TUI / picker inputs.
    bind!(KeyContext::Search, ch('k'), Exact(Mods::ALT), A::SearchHistoryPrev),
    bind!(KeyContext::Search, ch('j'), Exact(Mods::ALT), A::SearchHistoryNext),
    bind!(KeyContext::Search, KeyCode::Left, Any, A::SearchCursorLeft),
    bind!(KeyContext::Search, KeyCode::Right, Any, A::SearchCursorRight),
    bind!(KeyContext::Search, KeyCode::Backspace, Any, A::SearchBackspace),
];

#[rustfmt::skip]
static LEADER: &[Binding] = &[
    bind!(L, ch('f'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Files)),
    bind!(L, ch('f'), Exact(Mods::ALT), A::OpenPickerInBufferDir(PickerKind::Files)),
    bind!(L, ch('b'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Buffers)),
    bind!(L, ch('g'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Grep)),
    bind!(L, ch('g'), Exact(Mods::ALT), A::OpenPickerInBufferDir(PickerKind::Grep)),
    bind!(L, ch('e'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Explorer)),
    bind!(L, ch('e'), Exact(Mods::ALT), A::OpenExplorerAtRoot),
    bind!(L, ch('p'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Projects)),
    bind!(L, ch('t'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Diagnostics)),
    bind!(L, ch('l'), Exact(Mods::NONE), A::OpenPicker(PickerKind::LspServers)),
    bind!(L, ch('d'), Exact(Mods::ALT), A::OpenPicker(PickerKind::References)),
    bind!(L, ch('q'), Exact(Mods::NONE), A::Quit),
    bind!(L, ch('c'), Exact(Mods::NONE), A::CloseBuffer),
    bind!(L, ch('s'), Exact(Mods::NONE), A::Save),
    bind!(L, ch('s'), Exact(Mods::ALT), A::SaveAs),
    bind!(L, ch('r'), Exact(Mods::NONE), A::Reload),
    bind!(L, ch('n'), Exact(Mods::NONE), A::NewScratch),
    bind!(L, ch('w'), Exact(Mods::NONE), A::ToggleWrap),
    bind!(L, ch('a'), Exact(Mods::NONE), A::ToggleStageHunk),
    bind!(L, ch('v'), Exact(Mods::NONE), A::RevertHunk),
    bind!(L, ch('h'), Exact(Mods::NONE), A::NextHunk),
    bind!(L, ch('h'), Exact(Mods::ALT), A::PrevHunk),
    bind!(L, ch('i'), Exact(Mods::NONE), A::ToggleDiffView),
    bind!(L, ch('o'), Exact(Mods::NONE), A::ShowCommitInfo),
    bind!(L, ch('m'), Exact(Mods::NONE), A::Format),
    bind!(L, ch('k'), Exact(Mods::NONE), A::Hover),
    bind!(L, ch('d'), Exact(Mods::NONE), A::GotoDefinition),
    bind!(L, ch('j'), Exact(Mods::NONE), A::ShowDiagnostic),
    bind!(L, ch('x'), Exact(Mods::NONE), A::NextDiagnostic),
    bind!(L, ch('x'), Exact(Mods::ALT), A::PrevDiagnostic),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_mirror_the_tui_tables() {
        // h / Shift-h → MoveChar(Backward); Alt-h is the distinct earlier arm.
        assert!(matches!(
            lookup(KeyContext::Normal, ch('h'), Mods::NONE).map(|b| b.action),
            Some(Action::MoveChar(Direction::Backward))
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, ch('h'), Mods { shift: true, ..Mods::NONE })
                .map(|b| b.action),
            Some(Action::MoveChar(Direction::Backward))
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, ch('h'), Mods::ALT).map(|b| b.action),
            Some(Action::MoveLineFirstNonblank)
        ));
        // Ctrl-z lives in Global, not Normal.
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
            lookup(KeyContext::Normal, ch('j'), Mods { shift: true, ..Mods::ALT })
                .map(|b| b.action),
            Some(Action::MoveVisualLine(VerticalDirection::Down))
        ));
    }

    #[test]
    fn nav_history_precedes_horizontal_scroll() {
        // Alt-Left is the jump list; plain Left (any other mods) still scrolls.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Left, Mods::ALT).map(|b| b.action),
            Some(Action::NavBack)
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Right, Mods::ALT).map(|b| b.action),
            Some(Action::NavForward)
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Left, Mods::NONE).map(|b| b.action),
            Some(Action::Scroll { dir: ScrollDir::Left, .. })
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
        assert!(!Action::Scroll { dir: ScrollDir::Up, unit: ScrollUnit::Line }.is_repeatable());
        assert!(!Action::NavBack.is_repeatable());
        assert!(!Action::BeginFind { dir: Direction::Forward, till: false }.is_repeatable());
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
            lookup(KeyContext::Normal, ch('?'), Mods { shift: true, ..Mods::NONE })
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

    #[test]
    fn keycode_normalises_letters_and_named_keys() {
        use iced::keyboard::{key::Named, Key};
        assert_eq!(keycode(&Key::Character("H".into())), Some(ch('h')));
        assert_eq!(keycode(&Key::Character("?".into())), Some(ch('?')));
        assert_eq!(keycode(&Key::Named(Named::Space)), Some(ch(' ')));
        assert_eq!(keycode(&Key::Named(Named::Escape)), Some(KeyCode::Esc));
        assert_eq!(keycode(&Key::Named(Named::Shift)), None);
    }
}
