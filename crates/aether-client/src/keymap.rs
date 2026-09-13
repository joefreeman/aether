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
use aether_protocol::git::{ApplyScope, ConflictSide};
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
    /// The markdown reading view.
    ///
    /// Editing here is **block-grain**: the table binds block delete / change / open / paste, block
    /// depth, task toggle and undo/redo, all of which resolve against the markdown parse rather
    /// than against lines. What it deliberately has no way to do is edit *characters* — `i`/`a`
    /// leave the reading view for the source editor (`read_exit_for_edit`) rather than inserting
    /// in place.
    ///
    /// That is also why `Global` is not consulted in Read mode: its edit chords are line-grain
    /// (join, indent, move lines) and the reading view acts on blocks, so this table opts in
    /// binding by binding instead of inheriting a keymap written for the editor.
    Read,
    Leader,
    /// The `Space g` sub-leader: git verbs and the repo-wide git pickers. A second table rather
    /// than `Alt`-variants on the leader because git is the one area with more operations than a
    /// single key row can hold. Cursor-local git *navigation* deliberately stays out of it —
    /// `c`/`Alt-c` (next/prev hunk) in Normal, `Space c`/`Space Alt-c` (the changes pickers,
    /// mirroring `Space d`'s diagnostics) and `Space m` (blame at the cursor, the third reveal next
    /// to `Tab` and `Space n`).
    LeaderGit,
    /// The `Space v` sub-leader: the verbs of the **view** you are in — stop what it is running,
    /// and answer what an agent is asking. A second sub-leader rather than `Alt`-variants on the
    /// leader because answering must never be one key away from a typo, and because the leader's
    /// twenty-six letters were spent.
    LeaderView,
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

    /// Reading-view placement gap: the space between the view's edge and the focused *block's*
    /// matching edge — `Upper` leaves this above the block's top, `Lower` leaves it below the
    /// block's bottom. Edge-matched (unlike the editor's top-anchored line placement) so a tall
    /// block placed "near the bottom" actually ends there instead of hanging mostly off-screen. The
    /// gap is the editor's rest fraction, so `;` feels identical in both views (a cursor line is
    /// its own top *and* bottom edge).
    pub const READ_GAP: f32 = CURSOR_REST_FRACTION;
}

/// Abstract intent, mirroring the TUI's `Action` (subset). `count`/`extend` are execution
/// context resolved by the app.
#[derive(Clone, Copy, Debug)]
pub enum Action {
    // ---- motions (extend = Shift) ----
    MoveChar(Direction),
    /// Move to the next/previous word start. Normal mode binds only the backward direction
    /// (`b` / `Alt-b`) because `w` there selects words via [`Action::SelectWord`]; Insert mode,
    /// which has no selection, binds both on `Alt-←` / `Alt-→`.
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
    /// `Space g` — arm the git sub-leader ([`KeyContext::LeaderGit`]): the next keystroke names a
    /// git operation. Like the leader itself, an unbound key just cancels.
    BeginGitLeader,
    /// `Space v` — arm the view sub-leader ([`KeyContext::LeaderView`]).
    BeginViewLeader,

    // ---- edits ----
    Backspace,
    /// `Alt-Backspace` / `Alt-Delete` in Insert mode — delete the word to one side of the caret.
    /// The span is the matching [`Action::MoveWord`] motion's, resolved server-side, so the delete
    /// and the motion can never disagree about where a word starts.
    DeleteWord {
        dir: Direction,
        boundary: WordBoundary,
    },
    /// `Enter` in Insert. A newline everywhere except an input element, where the dispatch
    /// re-routes it to [`Action::SubmitInput`] — see that action for why the choice is made there
    /// and not in the table.
    NewlineIndent,
    /// `Alt-Enter` — the newline that is **never** re-routed, so a multi-line command or prompt
    /// can be typed in an input element where plain `Enter` submits.
    ///
    /// A separate action rather than a second binding onto [`Action::NewlineIndent`], because the
    /// resolved action is all the dispatch sees of which row fired: sharing one would put both
    /// keys on the same side of the input-role guard and leave no way to type a newline there.
    /// Identical to `NewlineIndent` in every other respect, and in every other element.
    NewlineIndentLiteral,
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
    /// `Alt-Backspace` in the search bar — drop the query's last word and re-run the incremental
    /// search. The search bar has no unwind ladder behind it (its option chips have their own
    /// toggle chords), so this rung is all there is.
    SearchDeleteWord,
    /// `Alt-e` in the search prompt: toggle literal (fixed-string) vs. regex matching.
    SearchToggleRegex,
    /// `]` / `[` — step through the jumplist from the cursor, cross-file, stopping at the ends.
    /// Populated by `Ctrl-j` in a picker.
    JumplistStep(Direction),
    /// `}` / `{` — like [`Action::JumplistStep`] but restricted to entries in the current buffer's
    /// file, so you walk one file's hits without jumping away. Uses Shift-bracket, not Alt-bracket,
    /// because Alt-bracket collides with terminal escape introducers (`ESC [` / `ESC ]`) — see the
    /// binding site.
    JumplistStepInFile(Direction),
    /// `Space Alt-j` — discard the captured list, so `]`/`[` go back to reporting it empty. The
    /// Alt sibling of `Space j` (open the Jumplist picker), and the counterpart of a picker's
    /// `Ctrl-j`. Clears it for every client in the context, which is who the list belongs to.
    ClearJumplist,
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
    /// analogue of [`Action::SaveAndQuit`], with the same confirm-deferral). On the tethered buffer
    /// the close also exits the client — the one-chord finish for an `ae file` quick edit (write
    /// the commit message, `Space Alt-x`, done).
    SaveAndClose,
    /// `Space Alt-w` — open a file by typing its absolute path (a leading `~/` is fine),
    /// regardless of the active workspace. Outside any workspace root the file opens as an external
    /// buffer; with no workspace active it lands in a fresh ephemeral context. Pairs with `Space w`
    /// (switch workspace). Opens the open-from-path overlay; submit calls `workspace/open_path`.
    OpenPath,
    Reload,
    /// Toggle the active buffer's transient ("keep") state — pin a preview permanent, or release a
    /// permanent buffer back to transient. Refused for unsaved buffers (auto-close would discard).
    /// On the tethered buffer, un-keeping additionally *releases* the tether — the client stops
    /// exiting when the buffer closes; one-way, a re-keep is just a plain keep.
    ToggleKeep,
    /// Copy the active buffer's workspace-relative path to the system clipboard.
    CopyRelativePath,
    /// Copy the active buffer's absolute (canonical) path to the system clipboard.
    CopyAbsolutePath,
    NewScratch,
    /// `Space Alt-t` — a **new** shell. Always creates: `Space t` lists the ones you have, so the
    /// open key has one meaning.
    ShellOpen,
    /// `Space Alt-a` — a **new** agent conversation, with the first agent found on `PATH`. Always
    /// creates, for the reason [`Action::ShellOpen`] does.
    AgentOpen,
    /// `Space v c` — stop whatever the focused view is running: a shell's command, an agent's turn.
    /// **Total** — the server decides what the view is, and a view running nothing answers so,
    /// which is the one toast this can produce ("Nothing is running here"). Same shape as
    /// `Space g x`.
    Interrupt,
    /// `Space v e` — fold the focused element shut, or open it up.
    ///
    /// Addresses the **focused** element and no other, which is what makes `Tab` the movement and
    /// this the verb. Whether that element folds at all is the view's to say, so a press on an
    /// agent's reply comes back refused rather than doing nothing visible.
    ToggleExpand,
    /// `Space v a` / `Space v d` — answer the pending permission request with the agent's first
    /// allowing or rejecting option. The wording is the agent's; this only says which way.
    AgentAnswer {
        allow: bool,
    },
    /// Submit what is typed in a composed view's **input** element.
    ///
    /// Not bound to a key of its own: `Enter` resolves to [`Action::Activate`] in Normal and
    /// [`Action::NewlineIndent`] in Insert, and the dispatch routes *either* here when the focused
    /// element is an input — a keymap row cannot see which element holds the cursor, so the choice
    /// is made where that is known. The same override the input role already applies to `Up`/`Down`
    /// (history recall), and the reason the keymap needs no notion of a shell.
    ///
    /// Both modes, deliberately: which mode you are in decides how text is *edited*, never whether
    /// a command runs, so requiring `Esc` first would be a mode distinction with nothing behind it.
    /// The newline moves to `Alt-Enter` ([`Action::NewlineIndentLiteral`]) inside an input, which
    /// is how a multi-line command or prompt is typed.
    ///
    /// What submitting *means* is the server's to decide (`view/submit_input`): a shell runs the
    /// line, an agent view sends the prompt, and the client never learns which sort of view it is
    /// in — the window marks the input by role and carries no kind at all.
    SubmitInput,
    CloseView,
    /// `Space z` — open another window onto the same workspace: the GUI spawns a fresh detached `ae
    /// --gui` process dialling the same daemon; the web shell opens a new browser tab on the same
    /// URL. A new client lands on the workspace's MRU buffer (the one you're on), so it
    /// "duplicates" the current view; the two windows are independent thereafter (own
    /// cursor/selection/viewport, shared buffers server-side). The TUI has no window to spawn, so
    /// it ignores the [`ShellAction::NewWindow`] it emits. The spawn names the workspace explicitly
    /// (`--workspace`), so the sibling never tethers to the file it lands on.
    NewWindow,
    /// `Space Alt-z` — the share-link sibling of `Space z`: copy the web client's URL for the
    /// current buffer to the clipboard (`?workspace=&root=&file=` with the cursor as its `#L:C`
    /// fragment; `?view=` for a scratch). The shell prepends its own base
    /// ([`ShellAction::CopyWebUrl`]).
    CopyWebUrl,

    // ---- git (the verbs live on the `Space g` sub-leader; see [`KeyContext::LeaderGit`]) ----
    /// `Space i` — toggle the inline diff. On the leader rather than the git sub-leader because
    /// it's a *view* of the buffer you're in, like `Space u`'s reading view, not an operation on
    /// the repo: nothing about it writes, and it reads as "inline" rather than as a git verb.
    ToggleDiffView,
    /// `c` / `Alt-c` in Normal — cursor-local hunk navigation, so *not* behind `Space g`: they're
    /// repeatable motions ([`Action::is_repeatable`]) and a three-key prefix would ruin them.
    NextHunk,
    PrevHunk,
    /// Stage a change: `Space g s` takes the hunk under the cursor (or the selected lines),
    /// `Space g Alt-s` the whole file — the more common gesture, and the reason the scope is a
    /// parameter rather than a separate action.
    StageChange {
        scope: ApplyScope,
    },
    /// Unstage, on the same two scopes: `Space g u` and `Space g Alt-u`.
    ///
    /// A separate action from [`Action::StageChange`] rather than one toggling key, because a
    /// toggle's effect depends on index state the user would have to read off the gutter before
    /// pressing — and the two directions aren't equally cheap to get wrong.
    UnstageChange {
        scope: ApplyScope,
    },
    /// The same two scopes again, reverting instead: `Space g r` and `Space g Alt-r`. Undoable
    /// (it's an ordinary buffer edit), which is why it needs no confirm.
    RevertChange {
        scope: ApplyScope,
    },
    /// Take a side in the merge conflict under the cursor: `Space g <` ours, `Space g >` theirs,
    /// `Space g =` both. The keys name the *marker glyphs* rather than the sides, because the
    /// position of a side is invariant while its meaning is not — during a rebase "ours" is the
    /// commit you're replaying onto, not your work, and a key reading `o` would be lying in the
    /// one situation where the distinction matters most.
    ///
    /// A selection covering several blocks takes them all — the same addressing
    /// [`Action::StageChange`] uses, which is why neither needs a file-scope key.
    ResolveConflict {
        side: ConflictSide,
    },
    /// `Space g d` — abandon a stopped merge/rebase (`<op> --abort`).
    ///
    /// The one git verb behind a confirm: the reset rewrites the working tree from disk, so every
    /// conflict resolution made in it is discarded and the undo stack cannot reach any of it. The
    /// confirm also stands in for the key's history — `d` was the diff toggle until the inline
    /// diff moved to `Space i`, and old muscle memory must not be able to throw work away.
    GitAbortOperation,
    /// `Space g c` — start a commit: prepare the message file server-side and open it as a buffer.
    /// `amend` (`Space g Alt-c`) rewrites the previous commit instead of adding one.
    GitCommit {
        amend: bool,
    },
    /// `Space g z` — take back the last commit, leaving its changes staged
    /// (`git reset --soft HEAD^`). No confirm: nothing is discarded, and the commit stays in the
    /// reflog either way.
    GitUncommit,
    /// `Space g f` — fetch from the remote, refreshing the ahead/behind counts in the status bar.
    /// Touches no file, so unlike the other git verbs it needs no unsaved-work pre-flight. The
    /// periodic fetcher (the `git_auto_fetch` app setting) runs the same operation on a timer.
    GitFetch,
    /// `Space g w` — everything not yet committed ("working" changes), as one read-only patch
    /// buffer: the same view a commit gets, over the changes you haven't made into one yet.
    ShowWorkingChanges,
    /// `Space g Alt-p` — publish the current branch's commits (`↑ahead`). Never force-pushes: the
    /// Alt slot here is the *outward* sibling of pull, not an escalation of it, and force-push has
    /// no key at all.
    GitPush,
    /// `Space g p` — bring the branch up to date with its upstream (`↓behind`). Refuses when
    /// buffers are unsaved (it moves the working tree, which fetch doesn't).
    ///
    /// Runs plain `git pull`, so the user's own `pull.rebase` decides between merge and rebase.
    GitPull,
    /// `Space g x` — stop the fetch, push or pull in flight. A no-op when nothing is running.
    ///
    /// Deliberately **not** `Esc`: an unbound second key cancels the sub-leader, so binding `Esc`
    /// to a verb would make it the one key on `Space g` that does something instead of backing
    /// out. Escape belongs to the chord.
    GitCancel,
    /// `Space g t` — shelve the working tree (`git stash push`), taking git's own
    /// `WIP on <branch>` message. `staged` (`Space g Alt-t`) narrows it to the index
    /// (`--staged`, git 2.35+), the "set this half aside" gesture. The stash *picker*
    /// (`Space g a`) is where entries are previewed, applied, popped and dropped.
    GitStashPush {
        staged: bool,
    },

    /// `Tab` / `Shift-Tab` — move the live cursor to the next/previous editor element of the view.
    ///
    /// Inert in a view of one element, which is most of them. In a patch it steps between hunks,
    /// and once elements window different files it is how you choose which file you are editing.
    FocusNextElement,
    FocusPrevElement,

    // ---- LSP ----
    /// `Enter` — **follow what is under the cursor**, resolved against what the cursor is *in*
    /// rather than against a view kind.
    ///
    /// One verb with several resolvers, which is what it always was: over source it is the language
    /// server's definition; on a patch's generated text it is the file that line came from; in a
    /// composed view it is the file the focused element windows. Naming it `Activate` rather than
    /// `GotoDefinition` says that out loud — the old name described one of its three answers and
    /// made the other two read as special cases.
    Activate,
    NextDiagnostic,
    PrevDiagnostic,
    Hover,
    ShowDiagnostic,
    Format,

    // ---- git (popovers) ----
    /// `Space m` — blame details for the cursor's line. Stays on the leader (not `Space g`) as the
    /// third cursor-local *reveal*, beside `Space n` (hover) and `Space Alt-n` (diagnostic at cursor).
    ShowCommitInfo,

    // ---- pickers ----
    OpenPicker(PickerKind),
    /// `Space Alt-f` — open Files pre-scoped to the active buffer's directory, seeded as an
    /// ordinary directory filter chip (editable, composable, removable). The buffer-locked
    /// changes/diagnostics *modes* use a dedicated kind instead (see [`PickerKind::GitChangesFile`]).
    OpenFilesInFileDir,
    /// `Space Alt-/` — open Grep with the query seeded from the buffer's selection: the
    /// workspace-scoped echo of Normal mode's `Alt-/` (search for selection), just as `Space /`
    /// echoes `/`. A fresh open, so the chip row starts empty like any other; an empty
    /// selection just opens grep.
    OpenGrepFromSelection,
    /// `Space Alt-e` — Explorer at the buffer's workspace root rather than its directory.
    OpenExplorerAtRoot,

    // ---- shell-local overlays (dispatched via `Effect::ShellAction`; a shell without the
    // overlay ignores them) ----
    /// `Space y` — the keyboard-shortcut reference (the Keybindings picker), generated from these
    /// tables. An unshifted letter rather than punctuation: the punctuation slots are taken by the
    /// settings pair, and `Space /` is grep (mirroring Normal mode's `/`).
    OpenHelp,
    /// `Space .` — the workspace-settings overlay (roots + rename). The neighbour of the app-wide
    /// settings on `Space ,`: same overlay family, narrower scope. Was `Space Alt-,`, which
    /// terminal emulators tend to swallow before we see it.
    OpenWorkspaceSettings,
    /// `Space,` — the application-settings overlay (global preferences, e.g. soft wrap). Font size
    /// lives here too (a stepped value row), not on a keybinding.
    OpenAppSettings,
    /// `Space ?` — the application-info dialog: build identity, the daemon we're connected to, and
    /// where this profile's state lives. Keeps its key through every `,`/`.`//` reshuffle: `?` is a
    /// strong enough "what is this thing?" mnemonic to stand on its own.
    ShowAppInfo,

    // ---- hints ----
    /// `Space h` — dismiss the corner hint: down-weight it (a deliberate "not now") and show
    /// another. No-op when the corner is empty.
    DismissHint,
    /// `Space Alt-h` — toggle hints on/off (the same switch as the settings-overlay row),
    /// persisted app-wide.
    ToggleHints,

    // ---- markdown reading view ----
    /// `Space u` — toggle the markdown reading view on the current buffer (markdown only;
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
    /// `x`/`Alt-x` — the editor's line-select at block grain: plain presses walk block to block
    /// (whole-line normal form), Shift grows the selection.
    ReadSelectBlock(Direction),
    /// `i`/`a` — to the editor, inserting at the selection's start / end: an extended selection
    /// uses the editor's own Insert-entry motions; a bare reading position enters at the focused
    /// block's start / append position.
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
    /// aliases so editor muscle memory lands too), `Ctrl-Alt-j`/`k` in the editor (blank-line
    /// paragraphs, any file type). One atomic server edit.
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

    /// Render the chord for the help overlay, e.g. `Alt-h`, `Ctrl-z`, `Space f`, `Space g s`, `↑`.
    /// Chords that arm a capture get a trailing `␣` placeholder (`f ␣`) to signal one more
    /// keystroke is expected.
    pub fn key_label(&self) -> String {
        let mut s = String::new();
        match self.ctx {
            KeyContext::Leader => s.push_str("Space "),
            KeyContext::LeaderGit => s.push_str("Space g "),
            KeyContext::LeaderView => s.push_str("Space v "),
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
        KeyContext::LeaderGit,
        KeyContext::LeaderView,
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
        KeyContext::LeaderGit => LEADER_GIT,
        KeyContext::LeaderView => LEADER_VIEW,
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
    "Agent",
    "App", // app-level
];

/// Every user-facing binding as a Keybindings-picker row: one entry per binding, bucketed by
/// group — the picker renders one section header per group (grep-style), so a group's rows must
/// be a contiguous run. Groups follow [`GROUP_ORDER`]; within a group, rows keep mode-major
/// order (Normal, the shared `Any` keys, Insert, Search, Application — so unlike the old tabbed
/// help dialog the `Global` keys appear *once*, as mode `Any`, rather than folded into both
/// Normal and Insert). Bindings with no `group` (internal aliases) and the leader-triggers
/// themselves are omitted. Built straight from the binding tables and shipped on `picker/view`, so
/// every client's picker shows exactly its own keymap.
pub fn keybinding_entries() -> Vec<aether_protocol::picker::KeybindingEntry> {
    // The `Space g` sub-leader lists as "Application" too: mode is the editor mode a chord is
    // reachable from, and both leaders are reached from Normal. Its rows are told apart by the
    // `Git` group and the `Space g …` label, not by a mode of their own.
    const MODES: [(&str, KeyContext); 8] = [
        ("Normal", KeyContext::Normal),
        ("Any", KeyContext::Global),
        ("Insert", KeyContext::Insert),
        ("Search", KeyContext::Search),
        ("Read", KeyContext::Read),
        ("Application", KeyContext::Leader),
        ("Application", KeyContext::LeaderGit),
        ("Application", KeyContext::LeaderView),
    ];
    // One bucket per group, filled in scan order; reordered to GROUP_ORDER just before flattening.
    // A Vec scan beats a map: ~15 groups, built once per open.
    let mut groups: Vec<(&str, Vec<aether_protocol::picker::KeybindingEntry>)> = Vec::new();
    for (mode, cx) in MODES {
        for b in table(cx) {
            if !b.group.is_empty()
                && !matches!(
                    b.action,
                    Action::BeginLeader | Action::BeginGitLeader | Action::BeginViewLeader
                )
            {
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
use KeyContext::{
    Global as G, Insert as I, Leader as L, LeaderGit as LG, LeaderView as LV, Normal as N,
    Read as R,
};
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
    bind!(N, ch('b'), IgnoreShift(Mods::ALT), A::MoveWord { dir: Direction::Backward, boundary: WordBoundary::BigWord }, "Motion", "Big word backward"),
    bind!(N, ch('b'), IgnoreShift(Mods::NONE), A::MoveWord { dir: Direction::Backward, boundary: WordBoundary::Word }, "Motion", "Small word backward"),
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
    bind!(N, KeyCode::Enter, Exact(Mods::NONE), A::Activate, "Code", "Go to definition"),
    // Reserved for this since the element tree landed; `Tab` still indents in Insert, where there
    // is no element to move between.
    bind!(N, KeyCode::Tab, Exact(Mods::NONE), A::FocusNextElement, "Motion", "Focus the next editor element"),
    bind!(N, KeyCode::BackTab, Any, A::FocusPrevElement, "Motion", "Focus the previous editor element"),

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
    bind!(N, ch('%'), IgnoreShift(Mods::NONE), A::SelectAll, "Selection", "Select all"),

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
    // selection with its neighbour, gap and all — any file type.
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
    // The Alt tier of the editing keys deletes/moves by word — the one modifier every terminal
    // delivers intact (legacy encoding sends ESC + the key, which reads back as Alt). Each must be
    // declared *before* its `Any` sibling below: lookup takes the first matching row, so an `Any`
    // seen first would swallow the chord. `IgnoreShift` because Shift means "extend" and Insert has
    // no selection to extend — holding it must not silently drop the chord back to char grain.
    bind!(I, KeyCode::Backspace, IgnoreShift(Mods::ALT), A::DeleteWord { dir: Direction::Backward, boundary: WordBoundary::Word }, "Edit", "Delete word before cursor"),
    bind!(I, KeyCode::Backspace, Any, A::Backspace, "Edit", "Delete character before cursor"),
    bind!(I, KeyCode::Delete, IgnoreShift(Mods::ALT), A::DeleteWord { dir: Direction::Forward, boundary: WordBoundary::Word }, "Edit", "Delete word after cursor"),
    bind!(I, KeyCode::Delete, Any, A::DeletePoint, "Edit", "Delete character at cursor"),
    // `Enter` submits in a shell's or agent's input element, so the newline needs a key that is
    // never re-routed — the routing is by focused element and happens in the dispatch, which sees
    // only the resolved action. `IgnoreShift` for the reason the Alt rows above give.
    bind!(I, KeyCode::Enter, IgnoreShift(Mods::ALT), A::NewlineIndentLiteral, "Edit", "Newline — never submits (multi-line commands and prompts)"),
    bind!(I, KeyCode::Enter, Any, A::NewlineIndent, "Edit", "Newline and indent — submits in a shell or agent input"),
    bind!(I, KeyCode::Tab, Any, A::InsertTab, "Edit", "Indent to next tab stop"),
    // Insert has no selection, so it can't borrow Normal's `w`-selects-a-word trick: both word
    // directions are plain motions here.
    bind!(I, KeyCode::Left, IgnoreShift(Mods::ALT), A::MoveWord { dir: Direction::Backward, boundary: WordBoundary::Word }, "Motion", "Word left"),
    bind!(I, KeyCode::Left, Any, A::MoveChar(Direction::Backward), "Motion", "Cursor left"),
    bind!(I, KeyCode::Right, IgnoreShift(Mods::ALT), A::MoveWord { dir: Direction::Forward, boundary: WordBoundary::Word }, "Motion", "Word right"),
    bind!(I, KeyCode::Right, Any, A::MoveChar(Direction::Forward), "Motion", "Cursor right"),
    bind!(I, KeyCode::Up, Any, A::MoveVisualLine(VerticalDirection::Up), "Motion", "Cursor up"),
    bind!(I, KeyCode::Down, Any, A::MoveVisualLine(VerticalDirection::Down), "Motion", "Cursor down"),
    // Normal binds these too — the line ends are the same place in either mode, and typing is
    // exactly when you reach for them. Insert doesn't fall through to Normal's table, so they have
    // to be declared here to exist at all.
    bind!(I, KeyCode::Home, Any, A::MoveLineStart, "Motion", "Logical line start"),
    bind!(I, KeyCode::End, Any, A::MoveLineEnd, "Motion", "Logical line end"),
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
    // Up/Down browse the query history — the same chord in every overlay text input (the grep
    // query, the glob/path chip editors). They're safe here and there because no shell's text input
    // claims a bare arrow-up, and because the *list* keys in the pickers are Alt-k/j. Alt-k/j stay
    // as an unlisted alias for the muscle memory that predates this.
    bind!(KeyContext::Search, KeyCode::Up, Exact(Mods::NONE), A::SearchHistoryPrev, "Search", "Previous query in history"),
    bind!(KeyContext::Search, KeyCode::Down, Exact(Mods::NONE), A::SearchHistoryNext, "Search", "Next query in history"),
    bind!(KeyContext::Search, ch('k'), Exact(Mods::ALT), A::SearchHistoryPrev, "", ""),
    bind!(KeyContext::Search, ch('j'), Exact(Mods::ALT), A::SearchHistoryNext, "", ""),
    // The one editing key the core owns here: word-grain delete, as in a buffer and in the pickers.
    // Plain Backspace stays with the shell's input (and, at the query start, steps into the chip
    // row) — see the note below.
    bind!(KeyContext::Search, KeyCode::Backspace, Exact(Mods::ALT), A::SearchDeleteWord, "Search", "Delete word in query"),
    // Match-option toggles, mirroring the grep picker's chip chords (Alt-c / Alt-w / Alt-e).
    bind!(KeyContext::Search, ch('c'), Exact(Mods::ALT), A::SearchToggleCase, "Search", "Cycle case sensitivity"),
    bind!(KeyContext::Search, ch('w'), Exact(Mods::ALT), A::SearchToggleWord, "Search", "Toggle whole-word match"),
    bind!(KeyContext::Search, ch('e'), Exact(Mods::ALT), A::SearchToggleRegex, "Search", "Toggle regex"),
    // Text entry (chars, Backspace, Left/Right caret) is owned by each shell's search input, which
    // syncs the value via `search_set_query`; only the command keys above live in this table.
];

/// The markdown reading view's keys. Where the editor already has a key for the concept, Read
/// reuses it — `o` *is* symbol nav (same action, same outline), `g`/`Alt-g` are the ends pair, `j`/`k` move the
/// (reading) cursor while the arrows scroll, `Ctrl-c` copies (the editor's clipboard chord — acting
/// on the focused element, since Read has no selection), search and jumplist keys are verbatim.
/// Deliberately contains no editing action (see [`KeyContext::Read`]).
#[rustfmt::skip]
static READ: &[Binding] = &[
    bind!(R, KeyCode::Esc, Any, A::DropSearch, "Search", "Clear the active search"),

    // ---- the reading cursor ----
    bind!(R, ch('j'), IgnoreShift(Mods::NONE), A::ReadStep(Direction::Forward), "Read", "Focus next element"),
    bind!(R, ch('k'), IgnoreShift(Mods::NONE), A::ReadStep(Direction::Backward), "Read", "Focus previous element"),
    // Unlisted muscle-memory aliases (the Ctrl-Alt-j/k pattern): the editor's other
    // line-step motions — `p`/`Alt-p`'s first-non-blank step, `Alt-j`/`k`'s visual-row step —
    // all collapse into the element step at block grain, so the keys land where the hand
    // expects. IgnoreShift keeps Shift as the extend modifier, exactly as on `j`/`k`.
    bind!(R, ch('p'), IgnoreShift(Mods::NONE), A::ReadStep(Direction::Forward)),
    bind!(R, ch('p'), IgnoreShift(Mods::ALT), A::ReadStep(Direction::Backward)),
    bind!(R, ch('j'), IgnoreShift(Mods::ALT), A::ReadStep(Direction::Forward)),
    bind!(R, ch('k'), IgnoreShift(Mods::ALT), A::ReadStep(Direction::Backward)),
    bind!(R, ch('l'), IgnoreShift(Mods::NONE), A::ReadStepLink(Direction::Forward), "Read", "Focus next link in block"),
    bind!(R, ch('h'), IgnoreShift(Mods::NONE), A::ReadStepLink(Direction::Backward), "Read", "Focus previous link in block"),
    // The **same** action Normal mode binds, not a reading-flavoured twin: one outline, whatever
    // view is looking at it. A markdown file's document symbols *are* its headings, so this lands
    // where the old AST walk did — and the breadcrumb, `Space o` and this key now agree by
    // construction rather than by three implementations happening to say the same thing.
    bind!(R, ch('o'), IgnoreShift(Mods::NONE), A::NavUnit(Direction::Forward), "Read", "Next heading"),
    bind!(R, ch('o'), IgnoreShift(Mods::ALT), A::NavUnit(Direction::Backward), "Read", "Previous heading"),
    bind!(R, ch('g'), IgnoreShift(Mods::NONE), A::ReadEnds { last: false }, "Read", "First element"),
    bind!(R, ch('g'), IgnoreShift(Mods::ALT), A::ReadEnds { last: true }, "Read", "Last element"),
    bind!(R, KeyCode::Enter, Exact(Mods::NONE), A::ReadActivate, "Read", "Follow link / open image / jump to footnote"),
    bind!(R, KeyCode::Enter, Exact(Mods::CTRL), A::ReadActivateNewWindow, "Read", "Open link in a new window/tab"),
    bind!(R, ch('c'), Exact(Mods::CTRL), A::ReadCopy, "Read", "Copy selection, link URL, or element source"),

    // ---- block selection (the editor's `x` line-select machine at block grain: plain presses
    // walk, Shift grows — and Shift-j/k extend through read_step) ----
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
    // whose other chords are edits; the curated-edit discipline) ----
    bind!(R, ch('z'), Exact(Mods::CTRL), A::Undo, "Edit", "Undo"),
    bind!(R, ch('z'), Exact(Mods::CTRL_ALT), A::Redo, "Edit", "Redo"),
    // The editor's adjust-the-value pair, re-declared because Read skips Global. Same action, so
    // the same key does the same thing on either side of `Space u`.
    bind!(R, ch('a'), Exact(Mods::CTRL), A::IncrementNumber, "Edit", "Check task item"),
    bind!(R, ch('a'), Exact(Mods::CTRL_ALT), A::DecrementNumber, "Edit", "Uncheck task item"),

    // ---- to the editor (transitions; deliberately NOT recording a read-vs-source
    // preference — Space u remains the "I prefer source" signal) ----
    bind!(R, ch('i'), Exact(Mods::NONE), A::ReadInsert { at_end: false }, "Mode", "Edit: insert at block/selection start"),
    bind!(R, ch('a'), Exact(Mods::NONE), A::ReadInsert { at_end: true }, "Mode", "Edit: insert at block/selection end"),
    bind!(R, ch('e'), Exact(Mods::CTRL), A::ReadChange, "Edit", "Edit: rewrite selected block(s)"),
    bind!(R, ch('o'), Exact(Mods::CTRL), A::ReadOpenBlock { above: false }, "Edit", "Edit: open block below (list item in a list)"),
    bind!(R, ch('o'), Exact(Mods::CTRL_ALT), A::ReadOpenBlock { above: true }, "Edit", "Edit: open block above (list item in a list)"),

    // ---- structural edits (phase 3: selection-relative server ops, atomic, one undo
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
    // *editor's* wrap geometry (best-effort in read space), but the landing
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
    bind!(L, ch('f'), Exact(Mods::ALT), A::OpenFilesInFileDir, "Files", "Find files in this file's directory"),
    // The three view-listing pickers share one rule with their `Alt` siblings: plain **lists** what
    // you have, `Alt` **makes** a new one. `b` buffers, `t` shells (terminals), `a` agents.
    bind!(L, ch('b'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Buffers), "Files", "Switch buffer"),
    bind!(L, ch('b'), Exact(Mods::ALT), A::NewScratch, "Files", "New scratch"),
    bind!(L, ch('t'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Shells), "App", "Switch shell"),
    bind!(L, ch('t'), Exact(Mods::ALT), A::ShellOpen, "App", "New shell (run a command)"),
    bind!(L, ch('a'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Agents), "Agent", "Switch agent conversation"),
    bind!(L, ch('a'), Exact(Mods::ALT), A::AgentOpen, "Agent", "New agent conversation"),
    // `g` is the git sub-leader's prefix, so grep moved to `/` (and its selection-seeded sibling to
    // `Alt-/`) — the workspace-scoped echo of Normal's `/` and `Alt-/`. `Space Alt-g` is left
    // unbound on purpose: `g` should read as "git" with no exception to remember.
    bind!(L, ch('g'), Exact(Mods::NONE), A::BeginGitLeader, "Leader", "Git sub-leader chord"),
    bind!(L, ch('/'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Grep), "Files", "Grep workspace"),
    bind!(L, ch('/'), Exact(Mods::ALT), A::OpenGrepFromSelection, "Files", "Grep for selection"),
    bind!(L, ch('e'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Explorer), "Files", "File explorer"),
    bind!(L, ch('e'), Exact(Mods::ALT), A::OpenExplorerAtRoot, "Files", "File explorer at workspace root"),
    bind!(L, ch('w'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Workspaces), "Workspace", "Switch workspace"),
    bind!(L, ch('d'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Diagnostics), "Code", "Diagnostics in this file"),
    bind!(L, ch('d'), Exact(Mods::ALT), A::OpenPicker(PickerKind::DiagnosticsWorkspace), "Code", "Workspace diagnostics"),
    bind!(L, ch('j'), Exact(Mods::NONE), A::OpenPicker(PickerKind::Jumplist), "Navigation", "Jumplist"),
    bind!(L, ch('j'), Exact(Mods::ALT), A::ClearJumplist, "Navigation", "Clear jumplist"),
    // The cursor-reveals sit together: `n` type & docs, `Alt-n` diagnostic, `m` blame. Hover used to
    // be `Tab` — the odd one out of the three — and moved to the leader to free `Tab`/`Shift-Tab`
    // for moving between the editors of a multi-element view. Bare letters are motions; a reveal is
    // not one.
    //
    // The trio sat on `t`/`Alt-t`/`m` until the three view pickers took the whole `a`/`b`/`t` row;
    // `n` came free at the same moment, when the agent sub-leader moved to `Space v`.
    //
    // One binding covers the reading view too — `A::Hover` resolves to the focused link's target
    // there — because "what is this thing?" is the same question either way.
    bind!(L, ch('n'), Exact(Mods::NONE), A::Hover, "Code", "Hover: type & docs, or link target"),
    bind!(L, ch('n'), Exact(Mods::ALT), A::ShowDiagnostic, "Code", "Diagnostic at cursor"),
    bind!(L, ch('m'), Exact(Mods::NONE), A::ShowCommitInfo, "Git", "Blame commit details"),
    bind!(L, ch('v'), Exact(Mods::NONE), A::BeginViewLeader, "Leader", "View sub-leader chord"),
    bind!(L, ch('l'), Exact(Mods::NONE), A::OpenPicker(PickerKind::LspServers), "Code", "LSP servers"),
    bind!(L, ch('r'), Exact(Mods::NONE), A::OpenPicker(PickerKind::References), "Code", "Go to references"),
    bind!(L, ch('o'), Exact(Mods::NONE), A::OpenPicker(PickerKind::DocumentSymbols), "Code", "Document symbols"),
    bind!(L, ch('o'), Exact(Mods::ALT), A::OpenPicker(PickerKind::WorkspaceSymbols), "Code", "Workspace symbols"),
    // The changes pickers stay on the leader rather than moving under `Space g`: they're the list
    // form of the cursor-local hunk navigation on `c`/`Alt-c`, exactly as `Space d`/`Space Alt-d`
    // are for `d`/`Alt-d`'s diagnostics. Plain is buffer-scoped, Alt widens to the workspace.
    bind!(L, ch('c'), Exact(Mods::NONE), A::OpenPicker(PickerKind::GitChangesFile), "Git", "Git changes in current file"),
    bind!(L, ch('c'), Exact(Mods::ALT), A::OpenPicker(PickerKind::GitChanges), "Git", "Workspace git changes (hunks)"),
    bind!(L, ch('q'), Exact(Mods::NONE), A::Quit, "App", "Quit"),
    bind!(L, ch('q'), Exact(Mods::ALT), A::SaveAndQuit, "App", "Save and quit"),
    // `?` is a shifted `/` on every layout we care about, so the terminal reports it with SHIFT set
    // while the GUI/web report the resolved character — `IgnoreShift` accepts both. It keeps its
    // key now `/` is grep: "?" asks about the install, and the shortcut list is one key away on `.`.
    bind!(L, ch('?'), IgnoreShift(Mods::NONE), A::ShowAppInfo, "App", "About / diagnostics"),
    bind!(L, ch(','), Exact(Mods::NONE), A::OpenAppSettings, "App", "Application settings"),
    bind!(L, ch('.'), Exact(Mods::NONE), A::OpenWorkspaceSettings, "Workspace", "Workspace settings"),
    bind!(L, ch('y'), Exact(Mods::NONE), A::OpenHelp, "App", "Show keyboard shortcuts"),
    bind!(L, ch('x'), Exact(Mods::NONE), A::CloseView, "App", "Close view"),
    bind!(L, ch('x'), Exact(Mods::ALT), A::SaveAndClose, "App", "Save and close view"),
    bind!(L, ch('z'), Exact(Mods::NONE), A::NewWindow, "App", "Open another window"),
    bind!(L, ch('z'), Exact(Mods::ALT), A::CopyWebUrl, "App", "Copy web URL"),
    bind!(L, ch('w'), Exact(Mods::ALT), A::OpenPath, "App", "Open file by absolute path"),
    bind!(L, ch('s'), Exact(Mods::NONE), A::Save, "App", "Save"),
    bind!(L, ch('s'), Exact(Mods::ALT), A::SaveAs, "App", "Save as"),
    bind!(L, ch('k'), Exact(Mods::NONE), A::ToggleKeep, "App", "Keep this document (in a review, the file under the cursor)"),
    bind!(L, ch('k'), Exact(Mods::ALT), A::Reload, "App", "Reload from disk"),
    bind!(L, ch('p'), Exact(Mods::NONE), A::CopyRelativePath, "App", "Copy relative path"),
    bind!(L, ch('p'), Exact(Mods::ALT), A::CopyAbsolutePath, "App", "Copy absolute path"),
    bind!(L, ch('u'), Exact(Mods::NONE), A::ToggleReadView, "Read", "Toggle Markdown reader / editor"),
    // The inline diff sits beside the reading view, not under `Space g`: both are ways of looking
    // at the buffer you're already in, and neither writes anything. `i` for *inline* — `d` on the
    // git sub-leader now abandons a stopped merge, which is not a key to leave a view toggle's
    // muscle memory pointing at.
    bind!(L, ch('i'), Exact(Mods::NONE), A::ToggleDiffView, "Git", "Toggle inline diff"),
    // The Alt sibling names the same verb one level down: plain toggles the view, Alt chooses what
    // it shows. Toggling is a many-times-a-session gesture and re-baselining a rare one, so the
    // cheap chord stays with the toggle.
    bind!(L, ch('i'), Exact(Mods::ALT), A::OpenPicker(PickerKind::GitBaseline), "Git", "Diff against…"),
    bind!(L, ch('h'), Exact(Mods::NONE), A::DismissHint, "App", "Dismiss the current hint"),
    bind!(L, ch('h'), Exact(Mods::ALT), A::ToggleHints, "App", "Toggle hints on/off"),
];

/// The `Space g` sub-leader: git operations on the repo. Same plain/Alt sibling convention as the
/// leader — plain is the common gesture, Alt its variant, and the pair always names *one* verb at
/// two scopes or in two directions (`s`/`Alt-s` stage hunk/file, `l`/`Alt-l` log repo/file).
///
/// Three keys don't spell their verb, and each buys something for it:
/// - `<`/`>`/`=` name the conflict markers themselves. Ours/theirs is not a stable idea — during a
///   rebase "ours" is the branch you're replaying *onto* — but the marker order is: git writes
///   stage 2 above the `=======` and stage 3 below it for every operation that can conflict. The
///   keys point at what's on screen. (`|` is free for the diff3 base section, if taking it ever
///   becomes a verb.)
/// - `d` abandons a stopped merge. It reads as *discard*, and it is the only key here behind a
///   confirm — which is also what makes it safe to have taken over the old diff-toggle key.
/// - `a` is the stash *picker*, one letter off its verb on `t`, because three stash operations
///   (push, push-staged, browse) don't fit one plain/Alt pair.
///
/// Still reserved, so the shape is decided once rather than key by key: `m` the full-file blame
/// column, `y` copy commit permalink, `Alt-g` (free — the repo picker's likely home now that `r`
/// is revert). Choosing what the gutter diffs against went where this said it would, on
/// `Space Alt-i` beside the diff toggle, and is not a key here. The reflog is a filter chip on the
/// log picker rather than a key: it's the same rows over a different ref walk.
///
/// One consequence worth naming, since it is the only place a `Space g` key does something with no
/// git in it: while the baseline is the saved file, `r` reverts a hunk to *disk*, i.e. discards its
/// unsaved edits. Staging and unstaging are refused there (and under a pinned revision) — see
/// `ApplyHunkStatus::NotAgainstHead`.
/// The `Space v` sub-leader: the verbs of the **view** you are in.
///
/// `c` stops whatever it is running — one key for a shell's command and an agent's turn, because
/// the client cannot tell the two apart and does not need to (see [`Action::Interrupt`]).
///
/// `a` and `d` answer the pending permission request an agent is blocked on — allow and decline.
/// Two keys rather than one toggle for the reason staging and unstaging are two: an answer cannot
/// be taken back, so pressing the same key twice must never mean the opposite of the first press.
/// The *wording* of the options is always the agent's own; these only say which way. They stay
/// chords rather than a modal prompt because answering usually means scrolling the transcript
/// first, which an overlay would block.
///
/// `e` folds the focused element shut or opens it up. One key rather than two, unlike `a`/`d`
/// above: a fold is a way of looking at something and is taken back by pressing it again, which is
/// the exact opposite of an answer.
///
/// `Esc` is deliberately unbound here, as in every leader table: it cancels the pending chord.
#[rustfmt::skip]
static LEADER_VIEW: &[Binding] = &[
    bind!(LV, ch('c'), Exact(Mods::NONE), A::Interrupt, "Agent", "Stop what this view is running"),
    bind!(LV, ch('a'), Exact(Mods::NONE), A::AgentAnswer { allow: true }, "Agent", "Allow what the agent is asking to do"),
    bind!(LV, ch('d'), Exact(Mods::NONE), A::AgentAnswer { allow: false }, "Agent", "Decline what the agent is asking to do"),
    bind!(LV, ch('e'), Exact(Mods::NONE), A::ToggleExpand, "Agent", "Expand or collapse the focused block"),
];

#[rustfmt::skip]
static LEADER_GIT: &[Binding] = &[
    bind!(LG, ch('s'), Exact(Mods::NONE), A::StageChange { scope: ApplyScope::Cursor }, "Git", "Stage change (hunk/selection)"),
    bind!(LG, ch('s'), Exact(Mods::ALT), A::StageChange { scope: ApplyScope::File }, "Git", "Stage whole file (mark conflict resolved)"),
    bind!(LG, ch('u'), Exact(Mods::NONE), A::UnstageChange { scope: ApplyScope::Cursor }, "Git", "Unstage change (hunk/selection)"),
    bind!(LG, ch('u'), Exact(Mods::ALT), A::UnstageChange { scope: ApplyScope::File }, "Git", "Unstage whole file"),
    bind!(LG, ch('r'), Exact(Mods::NONE), A::RevertChange { scope: ApplyScope::Cursor }, "Git", "Revert change (hunk/selection)"),
    bind!(LG, ch('r'), Exact(Mods::ALT), A::RevertChange { scope: ApplyScope::File }, "Git", "Revert whole file"),
    // `IgnoreShift` on all three: the char already encodes the Shift, and shells differ on whether
    // they also report the modifier (`<`/`>` are shifted on US layouts, `=` on German and French).
    // Mirrors the `?` and `}`/`{` bindings.
    bind!(LG, ch('<'), IgnoreShift(Mods::NONE), A::ResolveConflict { side: ConflictSide::Ours }, "Git", "Conflict: keep the top section (<<<<<<<)"),
    bind!(LG, ch('>'), IgnoreShift(Mods::NONE), A::ResolveConflict { side: ConflictSide::Theirs }, "Git", "Conflict: keep the bottom section (>>>>>>>)"),
    bind!(LG, ch('='), IgnoreShift(Mods::NONE), A::ResolveConflict { side: ConflictSide::Both }, "Git", "Conflict: keep both sections"),
    bind!(LG, ch('c'), Exact(Mods::NONE), A::GitCommit { amend: false }, "Git", "Commit staged changes"),
    bind!(LG, ch('c'), Exact(Mods::ALT), A::GitCommit { amend: true }, "Git", "Amend previous commit"),
    bind!(LG, ch('z'), Exact(Mods::NONE), A::GitUncommit, "Git", "Uncommit (keep changes staged)"),
    bind!(LG, ch('w'), Exact(Mods::NONE), A::ShowWorkingChanges, "Git", "Working changes (uncommitted diff)"),
    bind!(LG, ch('f'), Exact(Mods::NONE), A::GitFetch, "Git", "Fetch from remote"),
    bind!(LG, ch('p'), Exact(Mods::NONE), A::GitPull, "Git", "Pull from remote"),
    bind!(LG, ch('p'), Exact(Mods::ALT), A::GitPush, "Git", "Push commits to remote"),
    bind!(LG, ch('x'), Exact(Mods::NONE), A::GitCancel, "Git", "Stop the fetch, push or pull in progress"),
    bind!(LG, ch('d'), Exact(Mods::NONE), A::GitAbortOperation, "Git", "Abandon the stopped merge/rebase"),
    bind!(LG, ch('b'), Exact(Mods::NONE), A::OpenPicker(PickerKind::GitBranches), "Git", "Branches and worktrees"),
    bind!(LG, ch('l'), Exact(Mods::NONE), A::OpenPicker(PickerKind::GitLog), "Git", "History"),
    bind!(LG, ch('l'), Exact(Mods::ALT), A::OpenPicker(PickerKind::GitLogFile), "Git", "History of current file"),
    bind!(LG, ch('a'), Exact(Mods::NONE), A::OpenPicker(PickerKind::GitStash), "Git", "Stashes"),
    bind!(LG, ch('t'), Exact(Mods::NONE), A::GitStashPush { staged: false }, "Git", "Stash working tree"),
    bind!(LG, ch('t'), Exact(Mods::ALT), A::GitStashPush { staged: true }, "Git", "Stash staged changes"),
];

/// README ↔ keymap parity.
///
/// The README's keybinding tables are the only user-facing record of the keymap, and nothing but
/// this module has ever checked them against it — the tables drifted at 0.4.0 prep and were swept
/// by hand. Both directions are checked, because the two failures look nothing alike: a binding
/// that changes chord leaves a *stale* README row, while a new binding leaves a *missing* one.
#[cfg(test)]
mod readme_parity {
    use super::*;
    use std::collections::BTreeSet;

    const README: &str = include_str!("../../../README.md");

    /// Chords the README lists that are deliberately not `Binding`s. Each needs a reason, because
    /// the cheap way to make this test pass is to add a line here.
    const README_ONLY: &[(&str, &str)] = &[
        // Shift is not a binding: it is read separately as "extend" (`ModPattern::IgnoreShift`),
        // so `Shift-j` is the `j` binding with a modifier the table never sees.
        ("Shift-j", "Shift means extend; read outside the table"),
        ("Shift-k", "Shift means extend; read outside the table"),
    ];

    /// Bindings deliberately absent from the README: aliases and internals already carry `group:
    /// ""`, so anything here is a binding that *is* user-facing but documented in prose instead.
    const KEYMAP_ONLY: &[(&str, &str)] = &[];

    /// Backticked spans in the `## Keybindings` section, split by where they sit.
    ///
    /// The two halves answer different questions and are deliberately not one set:
    ///
    /// - **`rows`** — the first cell of each table row. A row *asserts* that a chord is bound, so
    ///   it can go stale, and that is the strict direction.
    /// - **`prose`** — the paragraphs between the tables, which document real bindings the tables
    ///   do not itemise (insert-mode word motions, the search-prompt toggles). They document
    ///   without asserting a table shape, so they count as coverage but are never checked for
    ///   staleness — a prose backtick is as likely to be a filename as a chord.
    ///
    /// Table *descriptions* are in neither: the jumplist row cites `Ctrl-j` as a cross-reference,
    /// not as a claim that `Ctrl-j` is bound here.
    fn readme_chords() -> (BTreeSet<String>, BTreeSet<String>) {
        let (mut rows, mut prose) = (BTreeSet::new(), BTreeSet::new());
        let mut in_keybindings = false;
        for line in README.lines() {
            if let Some(h) = line.strip_prefix("## ") {
                in_keybindings = h.trim() == "Keybindings";
                continue;
            }
            if !in_keybindings {
                continue;
            }
            // Header (`| Key | Action |`) and separator (`| --- |`) rows carry no backticks, so
            // they contribute nothing and need no special case.
            let (text, sink) = if line.starts_with('|') {
                (
                    line.trim_start_matches('|').split('|').next().unwrap_or(""),
                    &mut rows,
                )
            } else {
                (line, &mut prose)
            };
            for span in text.split('`').skip(1).step_by(2) {
                let chord = span.trim();
                if !chord.is_empty() {
                    sink.insert(chord.to_string());
                }
            }
        }
        (rows, prose)
    }

    /// Every chord the keymap binds and expects to be documented. `group: ""` marks a binding as
    /// hidden from the help overlay — an alias or an internal — so it is not README material
    /// either, and the two lists stay in agreement about what is user-facing.
    fn keymap_chords() -> BTreeSet<String> {
        all()
            .filter(|b| !b.group.is_empty())
            // `awaits_key` appends a `␣` placeholder for the overlay; the README writes the chord
            // and explains the extra keystroke in prose.
            .map(|b| b.key_label().trim_end_matches(" ␣").to_string())
            .collect()
    }

    #[test]
    fn every_readme_row_is_bound() {
        let bound = keymap_chords();
        let allowed: BTreeSet<&str> = README_ONLY.iter().map(|(c, _)| *c).collect();
        let stale: Vec<String> = readme_chords()
            .0
            .into_iter()
            .filter(|c| !bound.contains(c) && !allowed.contains(c.as_str()))
            .collect();
        assert!(
            stale.is_empty(),
            "README rows document chords the keymap does not bind (stale rows, or a chord that \
             moved): {stale:?}"
        );
    }

    #[test]
    fn every_binding_is_in_the_readme() {
        let (rows, prose) = readme_chords();
        let allowed: BTreeSet<&str> = KEYMAP_ONLY.iter().map(|(c, _)| *c).collect();
        let undocumented: Vec<String> = keymap_chords()
            .into_iter()
            .filter(|c| !rows.contains(c) && !prose.contains(c) && !allowed.contains(c.as_str()))
            .collect();
        assert!(
            undocumented.is_empty(),
            "keymap binds chords the README does not document (add a row, or `group: \"\"` if the \
             binding is an alias): {undocumented:?}"
        );
    }

    /// The allowlists are the failure mode of this test, so they get their own guard: an entry
    /// that no longer applies must be deleted rather than left to rot.
    #[test]
    fn the_allowlists_are_still_needed() {
        let bound = keymap_chords();
        let (rows, prose) = readme_chords();
        let documented: BTreeSet<&String> = rows.iter().chain(prose.iter()).collect();
        for (chord, why) in README_ONLY {
            assert!(
                documented.contains(&chord.to_string()),
                "`{chord}` is allowlisted as README-only ({why}) but the README no longer lists it"
            );
            assert!(
                !bound.contains(*chord),
                "`{chord}` is allowlisted as README-only ({why}) but it is now a real binding"
            );
        }
        for (chord, why) in KEYMAP_ONLY {
            assert!(
                bound.contains(*chord),
                "`{chord}` is allowlisted as keymap-only ({why}) but nothing binds it"
            );
            assert!(
                !documented.contains(&chord.to_string()),
                "`{chord}` is allowlisted as keymap-only ({why}) but the README now documents it"
            );
        }
    }

    /// Guards the parser, not the keymap: a README restructure that stops the table rows being
    /// found would make both directions pass vacuously.
    #[test]
    fn the_readme_parse_is_not_vacuous() {
        let chords = readme_chords().0;
        assert!(
            chords.len() > 100,
            "expected the README's keybinding tables to yield >100 chords, got {} — the parser \
             has probably lost the tables",
            chords.len()
        );
        for expected in ["h", "Alt-j", "Ctrl-Alt-z", "Space f", "Space g s", "Enter"] {
            assert!(chords.contains(expected), "parser missed `{expected}`");
        }
    }
}

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
        // Hover is a leader chord, and one binding serves both source and the reading view.
        assert!(entries.iter().any(|e| e.mode == "Application"
            && e.keys == "Space n"
            && e.desc == "Hover: type & docs, or link target"));
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
    fn reveal_bindings_are_space_n_alt_n_m() {
        // All three cursor-reveals are leader chords. Hover was a bare `Tab` until `Tab` was needed
        // for moving between a multi-element view's editors — and a bare letter is a motion here,
        // so the leader is where a reveal belongs anyway.
        assert!(matches!(
            lookup(KeyContext::Leader, ch('n'), Mods::NONE).map(|b| b.action),
            Some(Action::Hover)
        ));
        // `Tab` was reserved for element focus when hover moved off it; that reservation is now
        // taken up, which is the whole reason hover had to move.
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::Tab, Mods::NONE).map(|b| b.action),
            Some(Action::FocusNextElement)
        ));
        assert!(matches!(
            lookup(KeyContext::Normal, KeyCode::BackTab, Mods::NONE).map(|b| b.action),
            Some(Action::FocusPrevElement)
        ));
        // Read still answers neither: block editing has its own navigation, and a reader is one
        // element until the markdown ladder gives it more.
        assert!(lookup(KeyContext::Read, KeyCode::Tab, Mods::NONE).is_none());
        // Diagnostic-at-cursor and blame live on the Space leader (`Alt-n` / `m`); `Space j` is
        // the jumplist picker. The pair moved from `t`/`Alt-t` to `n`/`Alt-n` when the three view
        // pickers took `a`/`b`/`t`; plain answers "what is this", Alt "what is wrong with it".
        assert!(matches!(
            lookup(KeyContext::Leader, ch('n'), Mods::ALT).map(|b| b.action),
            Some(Action::ShowDiagnostic)
        ));
        // `Space t` is now the shells picker, not a reveal.
        assert!(matches!(
            lookup(KeyContext::Leader, ch('t'), Mods::NONE).map(|b| b.action),
            Some(Action::OpenPicker(PickerKind::Shells))
        ));
        assert!(matches!(
            lookup(KeyContext::Leader, ch('j'), Mods::NONE).map(|b| b.action),
            Some(Action::OpenPicker(PickerKind::Jumplist))
        ));
        // …and its Alt sibling discards the list rather than showing it.
        assert!(matches!(
            lookup(KeyContext::Leader, ch('j'), Mods::ALT).map(|b| b.action),
            Some(Action::ClearJumplist)
        ));
        assert!(matches!(
            lookup(KeyContext::Leader, ch('m'), Mods::NONE).map(|b| b.action),
            Some(Action::ShowCommitInfo)
        ));
        // `Space ?` is the info dialog. A terminal reports the shifted `/` with SHIFT held while
        // the GUI/web hand over the resolved character, so both must resolve — the whole point of
        // binding it `IgnoreShift`. It must not be shadowed by `Space /`'s grep, which is `Exact`.
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
            Some(Action::Activate)
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
    fn space_g_is_a_prefix_and_the_git_verbs_live_behind_it() {
        // `Space g` arms the sub-leader rather than running anything itself.
        assert!(matches!(
            lookup(KeyContext::Leader, ch('g'), Mods::NONE).map(|b| b.action),
            Some(Action::BeginGitLeader)
        ));
        // `Space Alt-g` is deliberately unbound: `g` reads as "git" with no exception.
        assert!(lookup(KeyContext::Leader, ch('g'), Mods::ALT).is_none());

        let git = |code, mods| lookup(KeyContext::LeaderGit, code, mods).map(|b| b.action);
        // The index verbs: plain is the hunk under the cursor, Alt the whole file. Stage and
        // unstage are *separate keys* — pressing `s` twice must never quietly undo the first press,
        // which is exactly what a toggle on one key would do.
        assert!(matches!(
            git(ch('s'), Mods::NONE),
            Some(Action::StageChange {
                scope: ApplyScope::Cursor
            })
        ));
        assert!(matches!(
            git(ch('s'), Mods::ALT),
            Some(Action::StageChange {
                scope: ApplyScope::File
            })
        ));
        assert!(matches!(
            git(ch('u'), Mods::NONE),
            Some(Action::UnstageChange {
                scope: ApplyScope::Cursor
            })
        ));
        assert!(matches!(
            git(ch('u'), Mods::ALT),
            Some(Action::UnstageChange {
                scope: ApplyScope::File
            })
        ));
        assert!(matches!(
            git(ch('r'), Mods::NONE),
            Some(Action::RevertChange {
                scope: ApplyScope::Cursor
            })
        ));
        assert!(matches!(
            git(ch('r'), Mods::ALT),
            Some(Action::RevertChange {
                scope: ApplyScope::File
            })
        ));
        assert!(matches!(
            git(ch('c'), Mods::NONE),
            Some(Action::GitCommit { amend: false })
        ));
        assert!(matches!(
            git(ch('c'), Mods::ALT),
            Some(Action::GitCommit { amend: true })
        ));
        assert!(matches!(
            git(ch('z'), Mods::NONE),
            Some(Action::GitUncommit)
        ));
        assert!(matches!(
            git(ch('b'), Mods::NONE),
            Some(Action::OpenPicker(PickerKind::GitBranches))
        ));
        // `g` was the branch picker's key until it moved to the letter that names it; nothing took
        // its place, so a doubled `Space g g` does nothing rather than something else.
        assert!(git(ch('g'), Mods::NONE).is_none());
        // Pull is plain, push is its outward Alt sibling — not the other way round, and neither is
        // a force variant.
        assert!(matches!(git(ch('p'), Mods::NONE), Some(Action::GitPull)));
        assert!(matches!(git(ch('p'), Mods::ALT), Some(Action::GitPush)));
        // Stash: the verb on `t`, `--staged` on its Alt, the picker one letter away on `a`.
        assert!(matches!(
            git(ch('t'), Mods::NONE),
            Some(Action::GitStashPush { staged: false })
        ));
        assert!(matches!(
            git(ch('t'), Mods::ALT),
            Some(Action::GitStashPush { staged: true })
        ));
        assert!(matches!(
            git(ch('a'), Mods::NONE),
            Some(Action::OpenPicker(PickerKind::GitStash))
        ));
        // `d` abandons the stopped operation. It was the diff toggle until the inline diff moved
        // to `Space i`, which is why the action behind it is confirm-gated in the core.
        assert!(matches!(
            git(ch('d'), Mods::NONE),
            Some(Action::GitAbortOperation)
        ));
        // The inline diff is on the leader, and nothing on the git sub-leader answers `i`.
        assert!(matches!(
            lookup(KeyContext::Leader, ch('i'), Mods::NONE).map(|b| b.action),
            Some(Action::ToggleDiffView)
        ));
        assert!(git(ch('i'), Mods::NONE).is_none());
        // `w` for *working* changes. Free since worktrees folded into the branch picker.
        assert!(matches!(
            git(ch('w'), Mods::NONE),
            Some(Action::ShowWorkingChanges)
        ));
        // A key with no git meaning resolves to nothing, so the chord just cancels.
        assert!(git(ch('j'), Mods::NONE).is_none());

        // The old single-key homes stay free — a stale reflex does nothing rather than something
        // else. `t`, `u` and `Alt-t` are the exceptions, spent on the shells picker, the reader
        // toggle and a new shell: **inert landings**, the same argument that let the keybindings
        // picker reclaim `y` below. `Alt-t` was the old git commit, and opening a shell writes
        // nothing — it hands you a prompt, which is where a stale reflex stops.
        assert!(matches!(
            lookup(KeyContext::Leader, ch('t'), Mods::ALT).map(|b| b.action),
            Some(Action::ShellOpen)
        ));
        // `y` (once branches) has since been reclaimed by the keybindings picker. Acceptable
        // because the landing is inert: a stale reflex opens a searchable list of every binding,
        // which answers the question a stale reflex is really asking.
        assert!(matches!(
            lookup(KeyContext::Leader, ch('y'), Mods::NONE).map(|b| b.action),
            Some(Action::OpenHelp)
        ));

        // Sub-leader rows render with their prefix, so the keybindings picker reads `Space g s`.
        assert_eq!(
            lookup(KeyContext::LeaderGit, ch('s'), Mods::ALT)
                .map(|b| b.key_label())
                .as_deref(),
            Some("Space g Alt-s")
        );
    }

    /// The conflict keys name the marker glyphs, and every one of them is a shifted character on
    /// some layout (`<`/`>` on US, `=` on German and French). Terminals report the resolved char
    /// *with* SHIFT set while the GUI and web shells report it without — so both must resolve, or
    /// the keys work in one shell and vanish in another.
    #[test]
    fn conflict_keys_are_the_marker_glyphs_and_survive_a_reported_shift() {
        let git = |code, mods| lookup(KeyContext::LeaderGit, code, mods).map(|b| b.action);
        for mods in [Mods::NONE, Mods::SHIFT] {
            assert!(
                matches!(
                    git(ch('<'), mods),
                    Some(Action::ResolveConflict {
                        side: ConflictSide::Ours
                    })
                ),
                "`<` must take the top section with mods {mods:?}"
            );
            assert!(
                matches!(
                    git(ch('>'), mods),
                    Some(Action::ResolveConflict {
                        side: ConflictSide::Theirs
                    })
                ),
                "`>` must take the bottom section with mods {mods:?}"
            );
            assert!(
                matches!(
                    git(ch('='), mods),
                    Some(Action::ResolveConflict {
                        side: ConflictSide::Both
                    })
                ),
                "`=` must keep both sections with mods {mods:?}"
            );
        }
        // Alt is not a variant of these: the sides are exhausted by three keys, and an Alt-chord
        // here would only be a mis-press away from a resolution nobody asked for.
        assert!(git(ch('<'), Mods::ALT).is_none());
        assert!(git(ch('>'), Mods::ALT).is_none());
        assert!(git(ch('='), Mods::ALT).is_none());
        // The letters the sides used to live on are free — `o`/`t` said "ours"/"theirs", which is
        // the very naming these keys exist to stop using.
        assert!(git(ch('o'), Mods::NONE).is_none());
        assert!(git(ch('o'), Mods::ALT).is_none());
    }

    #[test]
    fn leader_punctuation_is_settings_and_grep() {
        let l = |code, mods| lookup(KeyContext::Leader, code, mods).map(|b| b.action);
        // `,` app-wide, `.` this workspace: same overlay family, adjacent keys, narrower scope on
        // the second. Neither may move onto an Alt-chord — terminals eat `Alt-,`.
        assert!(matches!(
            l(ch(','), Mods::NONE),
            Some(Action::OpenAppSettings)
        ));
        assert!(matches!(
            l(ch('.'), Mods::NONE),
            Some(Action::OpenWorkspaceSettings)
        ));
        assert!(l(ch(','), Mods::ALT).is_none());
        // The shortcut reference sits on `y`; `/` is grep, mirroring Normal mode's `/` and `Alt-/`.
        assert!(matches!(l(ch('y'), Mods::NONE), Some(Action::OpenHelp)));
        assert!(matches!(
            l(ch('/'), Mods::NONE),
            Some(Action::OpenPicker(PickerKind::Grep))
        ));
        assert!(matches!(
            l(ch('/'), Mods::ALT),
            Some(Action::OpenGrepFromSelection)
        ));
        // Grep is `Exact(NONE)`, so the shifted `/` still reaches the info dialog behind it.
        assert!(matches!(l(ch('?'), Mods::SHIFT), Some(Action::ShowAppInfo)));
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

    /// Insert mode's Alt tier: word-grain versions of the char-grain editing keys. Each `Exact(ALT)`
    /// row must beat its `Any` sibling, which only holds while it's declared first — hence the
    /// paired assertions.
    #[test]
    fn insert_alt_tier_is_word_grain() {
        let alt_shift = Mods {
            alt: true,
            shift: true,
            ..Mods::NONE
        };
        for (mods, expected_word) in [(Mods::ALT, true), (alt_shift, true), (Mods::NONE, false)] {
            let back = lookup(KeyContext::Insert, KeyCode::Backspace, mods).map(|b| b.action);
            assert_eq!(
                matches!(
                    back,
                    Some(Action::DeleteWord {
                        dir: Direction::Backward,
                        ..
                    })
                ),
                expected_word,
                "Backspace with {mods:?}"
            );
            let del = lookup(KeyContext::Insert, KeyCode::Delete, mods).map(|b| b.action);
            assert_eq!(
                matches!(
                    del,
                    Some(Action::DeleteWord {
                        dir: Direction::Forward,
                        ..
                    })
                ),
                expected_word,
                "Delete with {mods:?}"
            );
            let left = lookup(KeyContext::Insert, KeyCode::Left, mods).map(|b| b.action);
            assert_eq!(
                matches!(
                    left,
                    Some(Action::MoveWord {
                        dir: Direction::Backward,
                        ..
                    })
                ),
                expected_word,
                "Left with {mods:?}"
            );
            let right = lookup(KeyContext::Insert, KeyCode::Right, mods).map(|b| b.action);
            assert_eq!(
                matches!(
                    right,
                    Some(Action::MoveWord {
                        dir: Direction::Forward,
                        ..
                    })
                ),
                expected_word,
                "Right with {mods:?}"
            );
        }
        // Unmodified, the same keys keep their char-grain meaning.
        assert!(matches!(
            lookup(KeyContext::Insert, KeyCode::Backspace, Mods::NONE).map(|b| b.action),
            Some(Action::Backspace)
        ));
        assert!(matches!(
            lookup(KeyContext::Insert, KeyCode::Delete, Mods::NONE).map(|b| b.action),
            Some(Action::DeletePoint)
        ));
        assert!(matches!(
            lookup(KeyContext::Insert, KeyCode::Left, Mods::NONE).map(|b| b.action),
            Some(Action::MoveChar(Direction::Backward))
        ));
        assert!(matches!(
            lookup(KeyContext::Insert, KeyCode::Right, Mods::NONE).map(|b| b.action),
            Some(Action::MoveChar(Direction::Forward))
        ));
    }

    /// Insert doesn't fall through to Normal's table, so the line ends have to be declared in both
    /// to mean the same thing in both.
    #[test]
    fn line_ends_are_bound_in_normal_and_insert() {
        for ctx in [KeyContext::Normal, KeyContext::Insert] {
            assert!(
                matches!(
                    lookup(ctx, KeyCode::Home, Mods::NONE).map(|b| b.action),
                    Some(Action::MoveLineStart)
                ),
                "Home in {ctx:?}"
            );
            assert!(
                matches!(
                    lookup(ctx, KeyCode::End, Mods::NONE).map(|b| b.action),
                    Some(Action::MoveLineEnd)
                ),
                "End in {ctx:?}"
            );
        }
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
        // `p`/`Alt-p` and `Alt-j`/`Alt-k` alias the element step: the editor's line-step variants
        // collapse into one motion at block grain. IgnoreShift keeps Shift as the extend modifier,
        // as on `j`/`k`.
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
