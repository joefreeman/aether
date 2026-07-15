//! Hints (docs/hints.md): a quiet corner suggestion that walks a curated curriculum as
//! the user demonstrates each binding. This module is the whole engine — curriculum, usage
//! observation, scoring, temperature sampling, per-context display slots — kept sans-IO like the
//! rest of the core: the clock arrives stamped on ticks from the shell, randomness is a seeded
//! generator, and persistence rides `hints/record` / `hints/state` effects.
//!
//! Two principles from the design doc shape everything here:
//!
//! - **Learning is global; display is contextual.** Each hint has one usage record no matter
//!   where its trigger fires; a hint declares the contexts it may *display* in, and the sampler
//!   only draws from hints eligible in the current context.
//! - **Completion tracking runs for every hint, all the time.** A user who already knows a
//!   binding retires its hint without ever seeing it — that's the familiarity suppression.

use crate::keymap::Action;
use aether_protocol::hints::{
    day_from_unix_ms, HintEvent, HintRecord, HintsStateResult, DISMISS_WEIGHT, LEARNED_DAYS,
    LEARNED_USES,
};
use aether_protocol::picker::PickerKind;
use std::collections::{HashMap, HashSet};

/// How long one hint holds the corner before the engine re-samples (while the user is active).
pub const ROTATE_MS: u64 = 180_000;

/// No key input for this long ⇒ the user is idle: the display timer freezes and shows stop
/// accruing, so a window left open overnight fatigues nothing.
pub const IDLE_MS: u64 = 60_000;

/// Sampling temperature. Weights are `score^(1/T)`, so `T → 0` is argmax, `T → ∞` is uniform over
/// the pool, and a score of zero (just-used, or fully fatigued) is never sampled at any
/// temperature. A constant, not a setting — tuned by feel.
const TEMPERATURE: f32 = 0.5;

/// Recency down-rank half-life: a binding used this recently is redundant to hint *right now*,
/// recovering as the memory fades.
const USE_HALFLIFE_HOURS: f32 = 8.0;

/// Fatigue decay half-life in days — must agree with the server's fold-in decay
/// (`aether-server::config::FATIGUE_HALFLIFE_DAYS`, docs/hints.md §1.11).
const FATIGUE_HALFLIFE_DAYS: f32 = 3.0;

/// Shows-without-follow per halving of a hint's score.
const FATIGUE_SCALE: f32 = 2.0;

/// Fraction of a tier that must be demonstrated before the next tier joins the pool.
const TIER_UNLOCK: f32 = 0.75;

/// Total sampling weight below which the corner stays empty instead of drawing. Without a floor,
/// a pool where everything was just used (all scores ≈ 0) still samples *something*, and a fresh
/// session right after a hint-following one reads as "the tutorial started over". An empty corner
/// is the honest answer; recency decay restocks it within an hour or two.
const MIN_POOL_WEIGHT: f32 = 0.01;

/// Where a hint may display. Learning state ignores this — the trigger firing in *any* context
/// counts — but the sampler only draws hints eligible where the user currently is. The Space
/// leader (a sub-second pending state) is deliberately not a context; it reads as Normal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContextId {
    Normal,
    Insert,
    Search,
    Picker(PickerKind),
    Settings,
    SaveAs,
    /// Curriculum-only wildcard: a hint listing this in its `contexts` is eligible in **every**
    /// picker (the shared picker vocabulary — highlight movement, paging). Never a live context:
    /// the session always reports a concrete `Picker(kind)`, so this is never a slot key.
    AnyPicker,
}

/// Picker-vocabulary commands referenced by the curriculum. `on_picker_key` is a hard-coded match
/// with no `Action` identity, so instrumented arms report one of these instead. Grown on demand —
/// this is not a mirror of the picker vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerCmd {
    /// `Ctrl-d` in the Buffers picker.
    CloseBuffer,
    /// `Alt-p` in the Files/Grep pickers.
    AddPathScope,
    /// `Alt-j` / `Alt-k` in any picker — move the highlight.
    MoveSelection,
    /// `Esc` in any picker — dismiss it. (Not the mandatory chooser's Esc, which exits the app
    /// without closing; that arm is deliberately uninstrumented.)
    Dismiss,
    /// Enter on the Workspaces picker's synthetic "+ Create workspace …" row.
    CreateWorkspace,
    /// Enter on a workspace row in the Workspaces picker.
    OpenWorkspace,
    /// `Ctrl-j` in a capturable picker — snapshot its results into the jumplist
    /// (docs/jumplist.md).
    CaptureJumplist,
}

/// Session facts that condition a hint's display eligibility beyond the context id — the engine
/// is sans-IO and can't see the picker's contents, so the session stamps these in
/// ([`HintEngine::set_facts`]) alongside every context sync. Conditions are keyed by hint id in
/// [`HintEngine::cond_holds`]; promote to a `HintDef` field if they multiply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HintFacts {
    /// Workspace rows the open Workspaces picker is showing (`None` when it isn't open). Drives
    /// the chooser hints: zero ⇒ teach creating a workspace, some ⇒ teach opening one.
    pub workspaces_listed: Option<u32>,
    /// The session is the boot placeholder (no workspace picked yet), where the Workspaces
    /// picker is the mandatory chooser: its Esc *exits* rather than closing the picker, so the
    /// picker-dismiss hint would mislead there and is suppressed.
    pub mandatory_chooser: bool,
}

/// What marks a hint as demonstrated. Editor bindings match on the resolved [`Action`] (observed
/// at `run_action` — and at `on_search_key`, where search-mode bindings resolve without passing
/// through it); picker commands are reported explicitly from the instrumented `on_picker_key`
/// arms. Capture-style bindings (find-char, surround, transforms) resolve before `run_action`
/// and are deliberately absent from the curriculum.
pub enum Trigger {
    Action(fn(&Action) -> bool),
    Picker(PickerCmd),
}

/// One curriculum entry. `keys` is an authored label (an action can have several bindings, and
/// hints are curated anyway); a keymap test cross-checks the trigger still matches a real binding
/// so a renamed action can't leave a stale hint behind. Label style: spaces separate the presses
/// of a *sequence* (`Space h`, `s ␣`); slashes separate *alternatives* (`h/j/k/l`, `Alt-j/k`).
pub struct HintDef {
    pub id: &'static str,
    /// Curriculum tier: survival → files → editing → workspace/code → git & picker deep-cuts.
    /// Tier N+1 enters the pool once ≥ [`TIER_UNLOCK`] of every lower tier is demonstrated.
    pub tier: u8,
    pub contexts: &'static [ContextId],
    pub trigger: Trigger,
    pub keys: &'static str,
    /// Display template; its `{}` slot takes the emphasized `keys` label. See [`HintView::parts`].
    pub text: &'static str,
}

use ContextId as C;

/// The tutorial opening, in display order: hints on this list that have **never been shown**
/// display before anything is sampled — each in the first context it's eligible in. The
/// workspace-chooser pair leads (the chooser is the first thing a fresh install sees, and only
/// one of the two is eligible at a time — their conditions are complementary); in Normal mode the
/// opening is "how to dismiss these" then "how to turn these off" — consent first, curriculum
/// second. Shown-ness is derived from the records (`last_shown_at`), so the intro resumes across
/// sessions and never repeats once seen.
const INTRO: &[&str] = &["workspace-create", "workspace-open", "dismiss", "toggle"];

/// The dismiss hint's id — [`HintEngine::dismiss`] keys its follow-vs-dismiss distinction on it.
const DISMISS_HINT_ID: &str = "dismiss";

/// Consent/meta hints about the hint system itself, exempt from the tier ladder's progress
/// accounting: they're offered (the intro leads with them) but never *required* — a user who
/// never dismisses or toggles hints must still progress past tier 0, or the whole curriculum
/// wedges behind two bindings the editor never needs.
const LADDER_EXEMPT: &[&str] = &["dismiss", "toggle"];

/// The curriculum, in tier order (order within a tier carries no meaning — sampling decides,
/// except for the deterministic [`INTRO`] opening). `text` is a template whose `{}` slot takes
/// the emphasized `keys` label at render time; a template without a slot is legal (a future
/// keyless tip renders as plain text).
#[rustfmt::skip]
pub static CURRICULUM: &[HintDef] = &[
    // ---- tier 0: survival (the first two are the INTRO pair) ----
    // The workspace-chooser pair: eligibility is conditioned on the picker's contents
    // (`cond_holds`), so exactly one applies at a time. Intro-listed — the chooser is the first
    // thing a fresh install sees, and these outrank the generic picker hints there.
    HintDef { id: "workspace-create", tier: 0, contexts: &[C::Picker(PickerKind::Workspaces)],
        keys: "Enter",
        trigger: Trigger::Picker(PickerCmd::CreateWorkspace),
        text: "Type a name, then {} creates that workspace" },
    HintDef { id: "workspace-open", tier: 0, contexts: &[C::Picker(PickerKind::Workspaces)],
        keys: "Enter",
        trigger: Trigger::Picker(PickerCmd::OpenWorkspace),
        text: "Use {} to open the selected workspace" },
    HintDef { id: "dismiss", tier: 0, contexts: &[C::Normal], keys: "Space h",
        trigger: Trigger::Action(|a| matches!(a, Action::DismissHint)),
        text: "Use {} to dismiss a hint" },
    HintDef { id: "toggle", tier: 0, contexts: &[C::Normal], keys: "Space Alt-h",
        trigger: Trigger::Action(|a| matches!(a, Action::ToggleHints)),
        text: "Use {} to toggle hints off/on" },
    HintDef { id: "help", tier: 0, contexts: &[C::Normal], keys: "Space /",
        trigger: Trigger::Action(|a| matches!(a, Action::OpenHelp)),
        text: "Use {} to browse all keybindings" },
    HintDef { id: "quit", tier: 0, contexts: &[C::Normal], keys: "Space q",
        trigger: Trigger::Action(|a| matches!(a, Action::Quit)),
        text: "Use {} to quit" },
    HintDef { id: "insert", tier: 0, contexts: &[C::Normal], keys: "i",
        trigger: Trigger::Action(|a| matches!(a, Action::EnterInsert(_))),
        text: "Use {} to insert text" },
    HintDef { id: "leave-insert", tier: 0, contexts: &[C::Insert], keys: "Esc",
        trigger: Trigger::Action(|a| matches!(a, Action::LeaveInsert)),
        text: "Use {} to return to Normal mode" },
    HintDef { id: "motion-hjkl", tier: 0, contexts: &[C::Normal], keys: "h/j/k/l",
        trigger: Trigger::Action(|a| matches!(a, Action::MoveChar(_) | Action::MoveLogicalLine(_))),
        text: "Use {} to move the cursor" },

    // ---- tier 1: files & saving ----
    HintDef { id: "save", tier: 1, contexts: &[C::Normal], keys: "Space s",
        trigger: Trigger::Action(|a| matches!(a, Action::Save)),
        text: "Use {} to save the buffer" },
    HintDef { id: "picker-files", tier: 1, contexts: &[C::Normal], keys: "Space f",
        trigger: Trigger::Action(|a| matches!(a, Action::OpenPicker(PickerKind::Files))),
        text: "Use {} to fuzzy-find a file" },
    HintDef { id: "picker-buffers", tier: 1, contexts: &[C::Normal], keys: "Space b",
        trigger: Trigger::Action(|a| matches!(a, Action::OpenPicker(PickerKind::Buffers))),
        text: "Use {} to switch between open buffers" },
    HintDef { id: "search", tier: 1, contexts: &[C::Normal], keys: "/",
        trigger: Trigger::Action(|a| matches!(a, Action::EnterSearch)),
        text: "Use {} to search the buffer" },
    HintDef { id: "undo", tier: 1, contexts: &[C::Normal, C::Insert], keys: "Ctrl-z",
        trigger: Trigger::Action(|a| matches!(a, Action::Undo)),
        text: "Use {} to undo" },

    // ---- tier 2: selection-first editing ----
    HintDef { id: "select-word", tier: 2, contexts: &[C::Normal], keys: "w",
        trigger: Trigger::Action(|a| matches!(a, Action::SelectWord { .. })),
        text: "Use {} to select the word under the cursor" },
    HintDef { id: "select-line", tier: 2, contexts: &[C::Normal], keys: "x",
        trigger: Trigger::Action(|a| matches!(a, Action::SelectLine(_))),
        text: "Use {} to select the line" },
    HintDef { id: "copy", tier: 2, contexts: &[C::Normal], keys: "Ctrl-c",
        trigger: Trigger::Action(|a| matches!(a, Action::Copy)),
        text: "Use {} to copy the selection" },
    HintDef { id: "paste", tier: 2, contexts: &[C::Normal], keys: "Ctrl-v",
        trigger: Trigger::Action(|a| matches!(a, Action::Paste)),
        text: "Use {} to paste before the selection" },
    HintDef { id: "change", tier: 2, contexts: &[C::Normal], keys: "Ctrl-e",
        trigger: Trigger::Action(|a| matches!(a, Action::Change)),
        text: "Use {} to replace the selection and insert" },
    HintDef { id: "select-tree", tier: 2, contexts: &[C::Normal], keys: "q",
        trigger: Trigger::Action(|a| matches!(a, Action::TreeExpand)),
        text: "Use {} to grow the selection by syntax node" },

    // ---- tier 3: workspace & code intelligence ----
    HintDef { id: "picker-grep", tier: 3, contexts: &[C::Normal], keys: "Space g",
        trigger: Trigger::Action(|a| matches!(a, Action::OpenPicker(PickerKind::Grep))),
        text: "Use {} to grep the workspace" },
    HintDef { id: "explorer", tier: 3, contexts: &[C::Normal], keys: "Space e",
        trigger: Trigger::Action(|a| matches!(a, Action::OpenPicker(PickerKind::Explorer))),
        text: "Use {} to browse files as a tree" },
    HintDef { id: "goto-def", tier: 3, contexts: &[C::Normal], keys: "Enter",
        trigger: Trigger::Action(|a| matches!(a, Action::GotoDefinition)),
        text: "Use {} to go to the definition" },
    HintDef { id: "hover", tier: 3, contexts: &[C::Normal], keys: "Tab",
        trigger: Trigger::Action(|a| matches!(a, Action::Hover)),
        text: "Use {} to see types & docs under the cursor" },
    HintDef { id: "diagnostics", tier: 3, contexts: &[C::Normal], keys: "d",
        trigger: Trigger::Action(|a| matches!(a, Action::NextDiagnostic | Action::PrevDiagnostic)),
        text: "Use {} to jump to the next diagnostic" },
    HintDef { id: "sneak", tier: 3, contexts: &[C::Normal], keys: "s ␣",
        trigger: Trigger::Action(|a| matches!(a, Action::BeginSneak { .. })),
        text: "Use {} to jump to any word on screen" },
    HintDef { id: "search-case", tier: 3, contexts: &[C::Search], keys: "Alt-c",
        trigger: Trigger::Action(|a| matches!(a, Action::SearchToggleCase)),
        text: "Use {} to cycle case sensitivity" },
    HintDef { id: "search-word", tier: 3, contexts: &[C::Search], keys: "Alt-w",
        trigger: Trigger::Action(|a| matches!(a, Action::SearchToggleWord)),
        text: "Use {} to toggle whole-word matching" },
    HintDef { id: "search-regex", tier: 3, contexts: &[C::Search], keys: "Alt-e",
        trigger: Trigger::Action(|a| matches!(a, Action::SearchToggleRegex)),
        text: "Use {} to toggle regex matching" },

    // ---- tier 4: git, picker deep-cuts, and the off switch ----
    HintDef { id: "diff", tier: 4, contexts: &[C::Normal], keys: "Space i",
        trigger: Trigger::Action(|a| matches!(a, Action::ToggleDiffView)),
        text: "Use {} to toggle the inline diff" },
    HintDef { id: "hunk-nav", tier: 4, contexts: &[C::Normal], keys: "c",
        trigger: Trigger::Action(|a| matches!(a, Action::NextHunk | Action::PrevHunk)),
        text: "Use {} to jump to the next change" },
    HintDef { id: "changes", tier: 4, contexts: &[C::Normal], keys: "Space c",
        trigger: Trigger::Action(|a| matches!(a, Action::OpenPicker(PickerKind::GitChangesFile))),
        text: "Use {} to review this file's changes" },
    HintDef { id: "picker-nav", tier: 4, contexts: &[C::AnyPicker], keys: "Alt-j/k",
        trigger: Trigger::Picker(PickerCmd::MoveSelection),
        text: "Use {} to move the selection" },
    // Sequenced after the picker basics ("help" + "picker-nav" — see `cond_holds`): learn to
    // open and move around a picker before learning the way out.
    HintDef { id: "picker-dismiss", tier: 4, contexts: &[C::AnyPicker], keys: "Esc",
        trigger: Trigger::Picker(PickerCmd::Dismiss),
        text: "Use {} to close the picker" },
    HintDef { id: "picker-close", tier: 4, contexts: &[C::Picker(PickerKind::Buffers)], keys: "Ctrl-d",
        trigger: Trigger::Picker(PickerCmd::CloseBuffer),
        text: "Use {} to close the selected buffer" },
    HintDef { id: "picker-scope", tier: 4,
        contexts: &[C::Picker(PickerKind::Files), C::Picker(PickerKind::Grep)], keys: "Alt-p",
        trigger: Trigger::Picker(PickerCmd::AddPathScope),
        text: "Use {} to scope results to a path" },
    // The jumplist trio (docs/jumplist.md): capture from a result-shaped picker, step the
    // captured entries, reopen the list as a picker.
    HintDef { id: "jumplist-capture", tier: 4,
        contexts: &[
            C::Picker(PickerKind::Grep),
            C::Picker(PickerKind::Diagnostics),
            C::Picker(PickerKind::DiagnosticsWorkspace),
            C::Picker(PickerKind::References),
            C::Picker(PickerKind::DocumentSymbols),
            C::Picker(PickerKind::GitChanges),
            C::Picker(PickerKind::GitChangesFile),
            C::Picker(PickerKind::Jumplist),
        ], keys: "Ctrl-j",
        trigger: Trigger::Picker(PickerCmd::CaptureJumplist),
        text: "Use {} to capture these results into the jumplist" },
    HintDef { id: "jumplist-step", tier: 4, contexts: &[C::Normal], keys: "]",
        trigger: Trigger::Action(|a| matches!(a, Action::JumplistStep(_))),
        text: "Use {} to step through the jumplist" },
    HintDef { id: "jumplist-picker", tier: 4, contexts: &[C::Normal], keys: "Space j",
        trigger: Trigger::Action(|a| matches!(a, Action::OpenPicker(PickerKind::Jumplist))),
        text: "Use {} to reopen the jumplist" },
    HintDef { id: "settings", tier: 4, contexts: &[C::Normal], keys: "Space .",
        trigger: Trigger::Action(|a| matches!(a, Action::OpenAppSettings)),
        text: "Use {} to open the app settings" },
];

fn curriculum_index(id: &str) -> Option<usize> {
    CURRICULUM.iter().position(|h| h.id == id)
}

/// What a shell renders in the corner: a "Hint: …" line whose key label is emphasized. Shells
/// read [`Self::parts`] rather than the raw template, so the split logic lives once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HintView {
    pub keys: &'static str,
    pub text: &'static str,
}

impl HintView {
    /// The display text split around its `{}` key slot: `(before, keys, after)` — the shell
    /// renders `before` and `after` dim and `keys` emphasized, prefixed by its "Hint: " chrome.
    /// A template without a slot is a keyless tip: all text in `before`, empty `keys`.
    pub fn parts(&self) -> (&'static str, &'static str, &'static str) {
        match self.text.split_once("{}") {
            Some((before, after)) => (before, self.keys, after),
            None => (self.text, "", ""),
        }
    }
}

/// A `hints/record` increment for the session to put on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireEvent {
    pub hint_id: &'static str,
    pub event: HintEvent,
}

/// One context's display slot: which hint holds the corner there and how much of its display
/// period remains. Frozen while the context is hidden — only the *current* context's slot ticks —
/// so flipping a picker open and closed restores the previous hint rather than churning it.
#[derive(Debug, Clone, Copy)]
struct Slot {
    hint: usize,
    remaining_ms: u64,
}

/// The engine. Inert until the `hints/state` snapshot is adopted (so pre-snapshot activity can't
/// double-count against a record that's about to arrive) and while the app setting is off — both
/// gates are the session's, passed down as `enabled`.
#[derive(Default)]
pub struct HintEngine {
    /// Snapshot adopted — the "server connection succeeded" gate for the very first hint.
    adopted: bool,
    /// Local mirror of the per-hint learning records, updated optimistically on each observed
    /// event so retirement and recency scoring don't wait on a round-trip. The server's copy
    /// (which stamps days authoritatively) reconciles at the next connect.
    records: HashMap<&'static str, HintRecord>,
    retired: HashSet<&'static str>,
    slots: HashMap<ContextId, Slot>,
    /// The context whose slot is live (ticking); `None` when hints have nowhere to display
    /// (confirm prompts, the boot chooser…).
    ctx: Option<ContextId>,
    /// Wall clock as of the last tick/sync, stamped by the shell. The engine never reads a clock.
    now_ms: u64,
    /// `now_ms` at the last key input; `now - last_input > IDLE_MS` freezes display accounting.
    last_input_ms: u64,
    /// xorshift64* state; seeded from the first stamped clock (deterministic under a fake clock
    /// in tests). 0 = not yet seeded.
    rng: u64,
    /// Session facts conditioning display eligibility (see [`HintFacts`]); stamped by the
    /// session alongside every context sync/tick.
    facts: HintFacts,
}

impl HintEngine {
    /// Adopt the `hints/state` snapshot (once per connection). Ids that no longer exist in the
    /// curriculum are dropped silently — an old `hints.json` must never wedge a new build.
    pub fn adopt(&mut self, snap: HintsStateResult) {
        self.records.clear();
        self.retired.clear();
        for (id, rec) in &snap.active {
            if let Some(idx) = curriculum_index(id) {
                self.records.insert(CURRICULUM[idx].id, *rec);
            }
        }
        for id in &snap.retired {
            if let Some(idx) = curriculum_index(id) {
                self.retired.insert(CURRICULUM[idx].id);
            }
        }
        self.adopted = true;
        // Any slot sampled before adoption (there shouldn't be one, but be safe) is stale.
        self.slots.clear();
    }

    /// Stamp key activity. Called on every key the session dispatches, before observation.
    pub fn note_input(&mut self) {
        self.last_input_ms = self.now_ms;
    }

    /// Stamp the session facts that condition display eligibility. Called alongside every
    /// context sync/tick — the engine never reads session state itself.
    pub fn set_facts(&mut self, facts: HintFacts) {
        self.facts = facts;
    }

    fn user_active(&self) -> bool {
        self.now_ms.saturating_sub(self.last_input_ms) <= IDLE_MS
    }

    /// The periodic tick (every couple of seconds, shell-driven): advance the clock, run the
    /// current context's display timer, rotate on expiry, and (re)fill an empty slot. Returns the
    /// wire events to emit (Shown records for fresh samples).
    pub fn on_tick(
        &mut self,
        ctx: Option<ContextId>,
        now_ms: u64,
        enabled: bool,
    ) -> Vec<WireEvent> {
        // The first stamped clock is a start, not an elapse — before it the engine sat at 0 and
        // a wall-clock-sized "elapsed" would instantly rotate whatever a pre-tick sync sampled.
        let elapsed = if self.now_ms == 0 {
            0
        } else {
            now_ms.saturating_sub(self.now_ms)
        };
        if self.rng == 0 && now_ms > 0 {
            // First stamped clock: seed the sampler, and treat boot as activity (the user just
            // launched the app — the very first hint should display and count).
            self.rng = now_ms | 1;
            self.last_input_ms = now_ms;
        }
        self.now_ms = now_ms;
        if !self.adopted || !enabled {
            self.ctx = ctx;
            return Vec::new();
        }

        let mut out = Vec::new();
        self.ctx = ctx;
        let Some(ctx) = ctx else {
            return out;
        };

        // Tick the live slot only while the user is around; idle time neither rotates nor shows.
        if self.user_active() {
            if let Some(slot) = self.slots.get_mut(&ctx) {
                if slot.remaining_ms <= elapsed {
                    let exclude = Some(slot.hint);
                    self.slots.remove(&ctx);
                    out.extend(self.fill_slot(ctx, exclude));
                } else {
                    slot.remaining_ms -= elapsed;
                }
            }
        }
        out.extend(self.validate_or_fill(ctx));
        out
    }

    /// Cheap context re-sync between ticks (after key dispatch / RPC events): if the user moved to
    /// a context with no valid slot, sample one now rather than waiting for the next tick.
    pub fn sync_context(&mut self, ctx: Option<ContextId>, enabled: bool) -> Vec<WireEvent> {
        self.ctx = ctx;
        if !self.adopted || !enabled {
            return Vec::new();
        }
        match ctx {
            Some(ctx) => self.validate_or_fill(ctx),
            None => Vec::new(),
        }
    }

    /// An editor action resolved through `run_action`. `ctx_before` is the context *before* the
    /// action's dispatch (the context its hint displayed in — dispatch may have opened a picker).
    pub fn observe_action(
        &mut self,
        action: &Action,
        ctx_before: Option<ContextId>,
        enabled: bool,
    ) -> Vec<WireEvent> {
        self.observe(
            |h| matches!(&h.trigger, Trigger::Action(m) if m(action)),
            ctx_before,
            enabled,
        )
    }

    /// An instrumented picker command fired.
    pub fn observe_picker(
        &mut self,
        cmd: PickerCmd,
        ctx: Option<ContextId>,
        enabled: bool,
    ) -> Vec<WireEvent> {
        self.observe(
            |h| matches!(&h.trigger, Trigger::Picker(c) if *c == cmd),
            ctx,
            enabled,
        )
    }

    /// The user explicitly dismissed the displayed hint (`Space h`): bump its fatigue by
    /// [`DISMISS_WEIGHT`] (a deliberate "not now" outweighs a lapsed display period), rotate to
    /// another hint, and report the dismissal so the server's counter matches.
    ///
    /// This owns the *learning* of the dismiss binding too (the generic `run_action` observation
    /// skips [`Action::DismissHint`] — it would rotate a followed intro hint before the dismissal
    /// ran, dismissing its replacement): every `Space h` press demonstrates the binding, and
    /// pressing it while the dismiss hint itself is on screen is that hint's *follow* — the
    /// intro's "try it now" moment — not a dismissal to hold against it.
    pub fn dismiss(&mut self, ctx: Option<ContextId>, enabled: bool) -> Vec<WireEvent> {
        if !self.adopted || !enabled {
            return Vec::new();
        }
        let Some(ctx) = ctx else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let slot = self.slots.remove(&ctx);
        let dismiss_idx = curriculum_index(DISMISS_HINT_ID);
        let followed_self = slot.map(|s| Some(s.hint) == dismiss_idx).unwrap_or(false);
        if let Some(idx) = dismiss_idx {
            out.extend(self.record_use(idx, followed_self));
        }
        let Some(slot) = slot else {
            return out; // empty corner: the press still demonstrated the binding
        };
        if !followed_self {
            let id = CURRICULUM[slot.hint].id;
            let now = self.now_ms;
            let rec = self.records.entry(id).or_default();
            rec.shows_without_follow =
                decayed_shows(rec.shows_without_follow, rec.last_shown_at, now) + DISMISS_WEIGHT;
            rec.last_shown_at = now;
            out.push(WireEvent {
                hint_id: id,
                event: HintEvent::Dismissed,
            });
        }
        out.extend(self.fill_slot(ctx, Some(slot.hint)));
        out
    }

    /// What the corner shows right now, if anything.
    pub fn view(&self, ctx: Option<ContextId>, enabled: bool) -> Option<HintView> {
        if !self.adopted || !enabled {
            return None;
        }
        let slot = self.slots.get(&ctx?)?;
        let h = &CURRICULUM[slot.hint];
        Some(HintView {
            keys: h.keys,
            text: h.text,
        })
    }

    // ---- internals -------------------------------------------------------------------------

    fn observe(
        &mut self,
        matches: impl Fn(&HintDef) -> bool,
        ctx: Option<ContextId>,
        enabled: bool,
    ) -> Vec<WireEvent> {
        if !self.adopted || !enabled {
            return Vec::new();
        }
        let mut out = Vec::new();
        let displayed = ctx.and_then(|c| self.slots.get(&c).map(|s| s.hint));
        let mut followed_any = false;
        for (idx, h) in CURRICULUM.iter().enumerate() {
            if !matches(h) {
                continue;
            }
            let followed = displayed == Some(idx);
            followed_any |= followed;
            out.extend(self.record_use(idx, followed));
        }
        // A followed hint rotates immediately — the suggestion landed; move on.
        if followed_any {
            if let Some(c) = ctx {
                if let Some(slot) = self.slots.remove(&c) {
                    out.extend(self.fill_slot(c, Some(slot.hint)));
                }
            }
        }
        // A retirement may have emptied the live context's corner; restock it.
        if let Some(c) = self.ctx {
            out.extend(self.validate_or_fill(c));
        }
        out
    }

    /// Apply one demonstrated use of hint `idx` (`followed` = it was on screen at the time):
    /// counters, same-day suppression, and local retirement, mirroring the server's apply
    /// (`config::HintsState::apply`) so neither waits on the round-trip. Returns the wire event,
    /// or `None` when suppressed. Rotation is the caller's job — [`Self::observe`] and
    /// [`Self::dismiss`] each have their own follow-up.
    fn record_use(&mut self, idx: usize, followed: bool) -> Option<WireEvent> {
        let h = &CURRICULUM[idx];
        if self.retired.contains(h.id) {
            return None;
        }
        let day = day_from_unix_ms(self.now_ms);
        let rec = self.records.entry(h.id).or_default();
        // Suppression: once today's use can't change the record (uses saturated, day already
        // counted), stop sending — this is what lets an unlearned-but-hammered binding (hjkl)
        // go quiet on the wire. A follow always goes through: its fatigue reset matters.
        let saturated_today = rec.uses >= LEARNED_USES && rec.last_used_day == day;
        if saturated_today && !followed {
            return None;
        }
        if rec.uses == 0 {
            rec.use_days = 1;
        } else if day != rec.last_used_day && rec.use_days < LEARNED_DAYS {
            rec.use_days += 1;
        }
        rec.last_used_day = day;
        rec.last_used_at = self.now_ms;
        rec.uses = (rec.uses + 1).min(LEARNED_USES);
        if followed {
            rec.shows_without_follow = 0.0;
        }
        if rec.uses >= LEARNED_USES && rec.use_days >= LEARNED_DAYS {
            self.records.remove(h.id);
            self.retired.insert(h.id);
            // Any frozen slot holding the retired hint is now invalid; drop it so re-entry
            // re-validates. The live context re-fills via the callers' restock.
            self.slots.retain(|_, s| s.hint != idx);
        }
        Some(WireEvent {
            hint_id: h.id,
            event: if followed {
                HintEvent::Followed
            } else {
                HintEvent::Used
            },
        })
    }

    /// Ensure `ctx` has a valid slot: keep a frozen one whose hint is still in the pool, resample
    /// otherwise. A pending intro hint of *higher* rank preempts the slot (the chooser's
    /// workspace hint must displace a picker-nav sampled before the list loaded); an intro hint
    /// already displaying is never preempted by a later-ranked one — rotation advances the
    /// sequence. Emits the Shown for a fresh sample.
    fn validate_or_fill(&mut self, ctx: ContextId) -> Vec<WireEvent> {
        if let Some(slot) = self.slots.get(&ctx) {
            let pool = self.pool(ctx);
            let preempted = self
                .pending_intro(&pool)
                .is_some_and(|i| intro_rank(i) < intro_rank(slot.hint));
            if pool.contains(&slot.hint) && !preempted {
                return Vec::new();
            }
            self.slots.remove(&ctx);
        }
        self.fill_slot(ctx, None)
    }

    /// The first [`INTRO`] hint eligible in `pool` that has never been shown — the next step of
    /// the tutorial opening, if any remains for this pool.
    fn pending_intro(&self, pool: &[usize]) -> Option<usize> {
        INTRO
            .iter()
            .filter_map(|id| curriculum_index(id))
            .find(|i| {
                pool.contains(i)
                    && self
                        .records
                        .get(CURRICULUM[*i].id)
                        .is_none_or(|r| r.last_shown_at == 0)
            })
    }

    /// Sample a hint for `ctx` (excluding `exclude`, the hint just rotated away from) and record
    /// the show. An empty pool — or one where every score is zero — leaves the corner empty; the
    /// next tick retries, so recency decay quietly restocks it.
    fn fill_slot(&mut self, ctx: ContextId, exclude: Option<usize>) -> Vec<WireEvent> {
        // No sampling before the first tick stamps a clock (records would carry epoch-zero
        // timestamps), and none while the user is idle: a show against an empty room is fatigue
        // noise. The first hint appears on the first post-adopt tick — snapshot adoption asks the
        // shell for one immediately (`Effect::HintTickNow`), so that's moments after boot.
        if self.now_ms == 0 || !self.user_active() {
            return Vec::new();
        }
        let pool: Vec<usize> = self
            .pool(ctx)
            .into_iter()
            .filter(|i| Some(*i) != exclude)
            .collect();
        // The tutorial opening: a never-shown INTRO hint takes the corner deterministically, in
        // list order, before anything is sampled. Rotation/dismissal excludes the outgoing hint,
        // so the sequence advances rather than sticking.
        let Some(hint) = self.pending_intro(&pool).or_else(|| self.sample(&pool)) else {
            return Vec::new();
        };
        self.slots.insert(
            ctx,
            Slot {
                hint,
                remaining_ms: ROTATE_MS,
            },
        );
        // Fold the decay into the local counter exactly like the server will (its stamp wins on
        // reconcile; the drift within one session is negligible).
        let now = self.now_ms;
        let rec = self.records.entry(CURRICULUM[hint].id).or_default();
        rec.shows_without_follow =
            decayed_shows(rec.shows_without_follow, rec.last_shown_at, now) + 1.0;
        rec.last_shown_at = now;
        vec![WireEvent {
            hint_id: CURRICULUM[hint].id,
            event: HintEvent::Shown,
        }]
    }

    /// A hint the user has already proven out: permanently retired, **or** its uses counter is
    /// saturated. Saturation is the same-day form of "learned" — it excludes the hint from
    /// display and counts toward tier unlock immediately, while permanent retirement (record
    /// collapse, observation stops) still waits for the distinct-days rule. Without this, day one
    /// can't progress at all and every session restarts the tutorial.
    fn demonstrated(&self, id: &str) -> bool {
        self.retired.contains(id) || self.records.get(id).is_some_and(|r| r.uses >= LEARNED_USES)
    }

    /// The hints eligible to display in `ctx` right now: not demonstrated, context-eligible,
    /// condition holding, and — for main-track (Normal-context) hints only — tier unlocked.
    /// Context-local hints (picker vocabulary, the Insert/Search hints) skip the tier ladder:
    /// being *in* the context is the gate, and a locked Buffers picker corner teaches nothing.
    fn pool(&self, ctx: ContextId) -> Vec<usize> {
        let frontier = self.frontier_tier();
        CURRICULUM
            .iter()
            .enumerate()
            .filter(|(_, h)| {
                let in_ctx = h.contexts.contains(&ctx)
                    || (matches!(ctx, ContextId::Picker(_))
                        && h.contexts.contains(&ContextId::AnyPicker));
                in_ctx
                    && !self.demonstrated(h.id)
                    && self.cond_holds(h.id)
                    && (!h.contexts.contains(&ContextId::Normal) || h.tier <= frontier)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Per-hint display conditions over the session [`HintFacts`] and the learning records —
    /// the rare hints whose relevance depends on more than the context id. Keyed by id while
    /// there are a few; promote to a `HintDef` field if they multiply.
    fn cond_holds(&self, id: &str) -> bool {
        match id {
            "workspace-create" => self.facts.workspaces_listed == Some(0),
            "workspace-open" => self.facts.workspaces_listed.is_some_and(|n| n > 0),
            // Sequenced after the picker basics — opening one ("help") and moving the highlight
            // ("picker-nav") come before the way out. Suppressed in the mandatory chooser, where
            // Esc exits the app instead of closing the picker and the text would mislead.
            "picker-dismiss" => {
                !self.facts.mandatory_chooser && self.past("help") && self.past("picker-nav")
            }
            _ => true,
        }
    }

    /// Whether a hint's teaching moment has passed — shown at least once — or can never come:
    /// demonstrated/retired, which excludes it from the pool, so a sequence waiting on its show
    /// would deadlock. The sequencing gate for hints that build on another hint's *presentation*.
    /// Deliberately not mere use: a binding touched once isn't a hint that has been taught, and
    /// its hint is still poolable — recency down-ranks a just-used binding's hint, so counting
    /// use here would let the follow-up hint leapfrog the very hint it builds on.
    fn past(&self, id: &str) -> bool {
        self.demonstrated(id) || self.records.get(id).is_some_and(|r| r.last_shown_at > 0)
    }

    /// The deepest unlocked tier of the **main track** (hints that display in Normal — the only
    /// ones the ladder gates): tier T is unlocked when every tier below it is ≥ `TIER_UNLOCK`
    /// demonstrated. The [`LADDER_EXEMPT`] meta pair doesn't count for or against progress.
    /// Always at least tier 0.
    fn frontier_tier(&self) -> u8 {
        let mut tier = 0u8;
        loop {
            let track: Vec<&HintDef> = CURRICULUM
                .iter()
                .filter(|h| {
                    h.tier == tier
                        && h.contexts.contains(&ContextId::Normal)
                        && !LADDER_EXEMPT.contains(&h.id)
                })
                .collect();
            if track.is_empty() {
                // Past the last tier — everything below is mostly learned; stay here.
                return tier;
            }
            let done = track.iter().filter(|h| self.demonstrated(h.id)).count();
            if (done as f32) < (track.len() as f32) * TIER_UNLOCK {
                return tier;
            }
            tier += 1;
        }
    }

    /// `score = recency × fatigue`, both in [0, 1]. The tier gate is pool membership, not a term.
    fn score(&self, idx: usize) -> f32 {
        let Some(rec) = self.records.get(CURRICULUM[idx].id) else {
            return 1.0; // never used, never shown: maximally worth suggesting
        };
        let recency = if rec.last_used_at == 0 {
            1.0
        } else {
            let hours = self.now_ms.saturating_sub(rec.last_used_at) as f32 / 3_600_000.0;
            1.0 - (-hours / USE_HALFLIFE_HOURS).exp2()
        };
        let eff = decayed_shows(rec.shows_without_follow, rec.last_shown_at, self.now_ms);
        let fatigue = (-eff / FATIGUE_SCALE).exp2();
        recency * fatigue
    }

    /// Temperature sampling: weight each pool entry `score^(1/T)` and draw proportionally. `None`
    /// when the pool is empty or the total weight sits under [`MIN_POOL_WEIGHT`] — everything was
    /// just used or is fully fatigued, and an empty corner beats recycling known material.
    fn sample(&mut self, pool: &[usize]) -> Option<usize> {
        let weights: Vec<f32> = pool
            .iter()
            .map(|&i| self.score(i).max(0.0).powf(1.0 / TEMPERATURE))
            .collect();
        let total: f32 = weights.iter().sum();
        if total <= MIN_POOL_WEIGHT {
            return None;
        }
        let mut x = self.rand_f32() * total;
        for (&idx, &w) in pool.iter().zip(&weights) {
            x -= w;
            if x <= 0.0 {
                return Some(idx);
            }
        }
        pool.last().copied()
    }

    fn rand_f32(&mut self) -> f32 {
        // xorshift64* — tiny, deterministic under a seeded clock, and plenty for picking hints.
        let mut x = self.rng.max(1);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (r >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// A hint's position in the [`INTRO`] sequence (non-intro hints rank last) — preemption order
/// for [`HintEngine::validate_or_fill`].
fn intro_rank(idx: usize) -> usize {
    INTRO
        .iter()
        .position(|id| *id == CURRICULUM[idx].id)
        .unwrap_or(usize::MAX)
}

/// The fatigue counter decayed from its last fold to `now_ms` — the same curve the server applies
/// when accumulating (`aether-server::config::decayed_shows`); keep in sync via docs/hints.md.
fn decayed_shows(shows: f32, last_shown_at: u64, now_ms: u64) -> f32 {
    if shows <= 0.0 || last_shown_at == 0 || now_ms <= last_shown_at {
        return shows;
    }
    let days = (now_ms - last_shown_at) as f32 / 86_400_000.0;
    shows * (-days / FATIGUE_HALFLIFE_DAYS).exp2()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: u64 = 86_400_000;
    const T0: u64 = 100 * DAY_MS; // an arbitrary "boot" wall-clock

    /// An adopted, empty-history engine as of `T0`, displaying in Normal.
    fn engine() -> (HintEngine, Vec<WireEvent>) {
        let mut e = HintEngine::default();
        e.adopt(HintsStateResult::default());
        let evs = e.on_tick(Some(C::Normal), T0, true);
        (e, evs)
    }

    fn displayed(e: &HintEngine, ctx: ContextId) -> Option<&'static str> {
        e.slots.get(&ctx).map(|s| CURRICULUM[s.hint].id)
    }

    #[test]
    fn first_tick_samples_a_tier_zero_hint_and_records_the_show() {
        let (e, evs) = engine();
        let id = displayed(&e, C::Normal).expect("a hint holds the corner");
        let def = &CURRICULUM[curriculum_index(id).unwrap()];
        assert_eq!(def.tier, 0, "fresh profile starts on the survival tier");
        assert!(def.contexts.contains(&C::Normal));
        assert_eq!(
            evs,
            vec![WireEvent {
                hint_id: id,
                event: HintEvent::Shown
            }]
        );
        assert!(e.view(Some(C::Normal), true).is_some());
        assert!(
            e.view(Some(C::Normal), false).is_none(),
            "setting off hides it"
        );
    }

    #[test]
    fn engine_is_inert_until_the_snapshot_arrives() {
        let mut e = HintEngine::default();
        let evs = e.on_tick(Some(C::Normal), T0, true);
        assert!(evs.is_empty());
        assert!(e.view(Some(C::Normal), true).is_none());
        let evs = e.observe_action(&Action::Quit, Some(C::Normal), true);
        assert!(evs.is_empty(), "pre-snapshot activity is not recorded");
    }

    #[test]
    fn following_the_displayed_hint_emits_followed_and_rotates() {
        let (mut e, _) = engine();
        let before = displayed(&e, C::Normal).unwrap();
        let action = trigger_action_for(before);
        let evs = e.observe_action(&action, Some(C::Normal), true);
        assert!(
            evs.contains(&WireEvent {
                hint_id: before,
                event: HintEvent::Followed
            }),
            "the on-screen hint's action is a follow: {evs:?}"
        );
        let after = displayed(&e, C::Normal);
        assert_ne!(after, Some(before), "a followed hint rotates immediately");
        // The rotation itself recorded a Shown for the replacement.
        if let Some(after) = after {
            assert!(evs.contains(&WireEvent {
                hint_id: after,
                event: HintEvent::Shown
            }));
        }
    }

    #[test]
    fn off_screen_use_emits_used_and_saturates_within_a_day() {
        let (mut e, _) = engine();
        // Find a tier-0 hint that is NOT currently displayed.
        let shown = displayed(&e, C::Normal).unwrap();
        let other = CURRICULUM
            .iter()
            .find(|h| h.tier == 0 && h.id != shown && h.contexts.contains(&C::Normal))
            .unwrap();
        let action = trigger_action_for(other.id);
        for _ in 0..LEARNED_USES {
            let evs = e.observe_action(&action, Some(C::Normal), true);
            assert!(evs
                .iter()
                .any(|ev| ev.hint_id == other.id && ev.event == HintEvent::Used));
        }
        // Saturated for today: further uses go quiet on the wire.
        let evs = e.observe_action(&action, Some(C::Normal), true);
        assert!(
            !evs.iter().any(|ev| ev.hint_id == other.id),
            "saturated same-day uses are suppressed: {evs:?}"
        );
        assert!(!e.retired.contains(other.id), "one day never retires");
    }

    #[test]
    fn uses_across_two_days_retire_and_drop_the_hint_from_the_pool() {
        let (mut e, _) = engine();
        let shown = displayed(&e, C::Normal).unwrap();
        let other = CURRICULUM
            .iter()
            .find(|h| h.tier == 0 && h.id != shown && h.contexts.contains(&C::Normal))
            .unwrap();
        let action = trigger_action_for(other.id);
        e.observe_action(&action, Some(C::Normal), true);
        e.observe_action(&action, Some(C::Normal), true);
        // Next calendar day (the engine clock only moves via ticks).
        e.on_tick(Some(C::Normal), T0 + DAY_MS, true);
        e.observe_action(&action, Some(C::Normal), true);
        assert!(
            e.retired.contains(other.id),
            "3 uses across 2 days = learned"
        );
        let idx = curriculum_index(other.id).unwrap();
        assert!(
            !e.pool(C::Normal).contains(&idx),
            "retired hints leave the pool"
        );
        // And observation goes fully quiet for it.
        let evs = e.observe_action(&action, Some(C::Normal), true);
        assert!(!evs.iter().any(|ev| ev.hint_id == other.id));
    }

    #[test]
    fn context_flip_freezes_and_restores_the_previous_hint() {
        let (mut e, _) = engine();
        let normal_hint = displayed(&e, C::Normal).unwrap();
        // Open the Buffers picker: its own context, its own slot (tier 4 might not be unlocked, so
        // possibly an empty corner — either way Normal's slot must survive untouched).
        e.on_tick(Some(C::Picker(PickerKind::Buffers)), T0 + 10_000, true);
        // Two minutes pass inside the picker — more than none, less than ROTATE_MS.
        e.on_tick(Some(C::Picker(PickerKind::Buffers)), T0 + 130_000, true);
        // Back to Normal: the same hint returns, with no fresh Shown for it.
        let evs = e.on_tick(Some(C::Normal), T0 + 131_000, true);
        assert_eq!(displayed(&e, C::Normal), Some(normal_hint));
        assert!(
            !evs.iter().any(|ev| ev.hint_id == normal_hint),
            "restoring a frozen slot is not a new show"
        );
        // The frozen slot did not tick while hidden: 121s elapsed, but well under ROTATE_MS
        // remains only if the timer paused. Let the display run to just short of ROTATE_MS from
        // *visible* time and check it still hasn't rotated.
        e.on_tick(Some(C::Normal), T0 + 131_000 + ROTATE_MS - 15_000, true);
        assert_eq!(
            displayed(&e, C::Normal),
            Some(normal_hint),
            "hidden time must not count toward rotation"
        );
    }

    #[test]
    fn display_rotates_after_rotate_ms_of_visible_active_time() {
        let (mut e, _) = engine();
        let first = displayed(&e, C::Normal).unwrap();
        let mut now = T0;
        // Keep the user active (input every 30s) while the display period runs out.
        while now < T0 + ROTATE_MS + 30_000 {
            now += 30_000;
            e.note_input();
            e.on_tick(Some(C::Normal), now, true);
        }
        let second = displayed(&e, C::Normal).unwrap();
        assert_ne!(second, first, "the corner rotates after ROTATE_MS");
    }

    #[test]
    fn idle_time_freezes_rotation() {
        let (mut e, _) = engine();
        let first = displayed(&e, C::Normal).unwrap();
        // No input for two full rotation periods: idle kicks in after IDLE_MS, freezing the timer.
        let mut now = T0;
        while now < T0 + 2 * ROTATE_MS {
            now += 30_000;
            e.on_tick(Some(C::Normal), now, true);
        }
        assert_eq!(
            displayed(&e, C::Normal),
            Some(first),
            "an unattended window keeps its hint"
        );
    }

    #[test]
    fn insert_context_offers_only_insert_hints() {
        let (mut e, _) = engine();
        e.note_input();
        e.on_tick(Some(C::Insert), T0 + 1_000, true);
        let id = displayed(&e, C::Insert).expect("insert has a survival hint (Esc)");
        assert!(CURRICULUM[curriculum_index(id).unwrap()]
            .contexts
            .contains(&C::Insert));
    }

    #[test]
    fn higher_tiers_stay_locked_until_the_frontier_mostly_retires() {
        let (mut e, _) = engine();
        assert_eq!(e.frontier_tier(), 0);
        for i in e.pool(C::Normal) {
            assert_eq!(CURRICULUM[i].tier, 0, "only tier 0 in the fresh pool");
        }
        // Retire 5 of the 6 main-track tier-0 hints (83% ≥ 75%): tier 1 unlocks.
        for id in ["dismiss", "toggle", "help", "quit", "insert"] {
            e.records.remove(id);
            e.retired.insert(id);
        }
        assert_eq!(e.frontier_tier(), 1);
        assert!(
            e.pool(C::Normal).iter().any(|&i| CURRICULUM[i].tier == 1),
            "tier-1 hints join the pool once tier 0 is mostly learned"
        );
        assert!(
            !e.pool(C::Normal).iter().any(|&i| CURRICULUM[i].tier >= 2),
            "tier 2 stays locked"
        );
    }

    #[test]
    fn tier_ladder_is_not_gated_on_the_meta_pair() {
        let (mut e, _) = engine();
        // Demonstrate tier 0's editor skills — but never dismiss/toggle, which a user who
        // simply ignores the hint system will never press. Tier 1 must still unlock, or the
        // whole curriculum wedges at tier 0 forever.
        for id in ["help", "quit", "insert", "motion-hjkl"] {
            e.records.remove(id);
            e.retired.insert(id);
        }
        assert_eq!(
            e.frontier_tier(),
            1,
            "the meta pair must not gate progression"
        );
        assert!(
            e.pool(C::Normal).iter().any(|&i| CURRICULUM[i].tier == 1),
            "tier-1 hints join the pool"
        );
    }

    #[test]
    fn recent_use_zeroes_the_score_and_it_recovers_over_hours() {
        let (mut e, _) = engine();
        let shown = displayed(&e, C::Normal).unwrap();
        let other = CURRICULUM
            .iter()
            .find(|h| h.tier == 0 && h.id != shown && h.contexts.contains(&C::Normal))
            .unwrap();
        let idx = curriculum_index(other.id).unwrap();
        assert!((e.score(idx) - 1.0).abs() < 1e-6, "untouched hint scores 1");
        e.observe_action(&trigger_action_for(other.id), Some(C::Normal), true);
        assert!(
            e.score(idx) < 0.01,
            "a just-used binding is not worth hinting"
        );
        // A day later the memory has faded most of the way back.
        e.on_tick(Some(C::Normal), T0 + DAY_MS, true);
        assert!(e.score(idx) > 0.8, "recency down-rank recovers");
    }

    #[test]
    fn ignored_shows_fatigue_the_hint() {
        let (mut e, _) = engine();
        let shown = displayed(&e, C::Normal).unwrap();
        let idx = curriculum_index(shown).unwrap();
        let fresh = e.score(idx);
        // Simulate three more display periods that the user sat through without following.
        for _ in 0..3 {
            let rec = e.records.entry(shown).or_default();
            rec.shows_without_follow += 1.0;
            rec.last_shown_at = e.now_ms;
        }
        assert!(
            e.score(idx) < fresh / 2.0,
            "shows without follows halve the score at the fatigue scale"
        );
    }

    #[test]
    fn picker_contexts_surface_their_own_hints_immediately() {
        let (mut e, _) = engine();
        // No tier grinding needed: context-local hints skip the ladder — being *in* the picker
        // is the gate. A fresh profile's Buffers picker offers its Ctrl-d or the shared
        // navigation hint straight away.
        e.note_input();
        e.on_tick(Some(C::Picker(PickerKind::Buffers)), T0 + 1_000, true);
        let id = displayed(&e, C::Picker(PickerKind::Buffers))
            .expect("a picker hint shows without tier progress");
        assert!(matches!(id, "picker-close" | "picker-nav"), "got {id}");
        // A picker with no dedicated hint still gets the shared vocabulary via AnyPicker.
        e.on_tick(Some(C::Picker(PickerKind::Workspaces)), T0 + 2_000, true);
        assert_eq!(
            displayed(&e, C::Picker(PickerKind::Workspaces)),
            Some("picker-nav")
        );
        // And the picker-command follow path rotates whichever hint the Buffers corner shows.
        let cmd = if id == "picker-close" {
            PickerCmd::CloseBuffer
        } else {
            PickerCmd::MoveSelection
        };
        let evs = e.observe_picker(cmd, Some(C::Picker(PickerKind::Buffers)), true);
        assert!(evs.contains(&WireEvent {
            hint_id: id,
            event: HintEvent::Followed
        }));
    }

    #[test]
    fn saturation_hides_a_hint_the_same_day_and_advances_the_tier() {
        let (mut e, _) = engine();
        // Demonstrate five of the six main-track tier-0 hints (3 uses each, all today).
        for id in ["dismiss", "toggle", "help", "quit", "insert"] {
            let action = trigger_action_for(id);
            for _ in 0..LEARNED_USES {
                e.observe_action(&action, Some(C::Normal), true);
            }
            assert!(!e.retired.contains(id), "one day never *retires*");
            let idx = curriculum_index(id).unwrap();
            assert!(
                !e.pool(C::Normal).contains(&idx),
                "a saturated hint stops displaying the same day"
            );
        }
        // 5 of the 6 main-track tier-0 hints demonstrated (83% ≥ 75%): tier 1 unlocks today, so
        // the curriculum progresses within a single sitting even though nothing has retired yet.
        assert_eq!(e.frontier_tier(), 1);
    }

    #[test]
    fn an_exhausted_context_pool_leaves_the_corner_empty() {
        let (mut e, _) = engine();
        // Insert's only eligible hint is Esc (undo is main-track and still tier-locked). One use
        // zeroes its recency score; the corner must stay empty rather than recycle it — this is
        // what stops a fresh session from replaying hints the user just demonstrated.
        e.observe_action(&Action::LeaveInsert, Some(C::Normal), true);
        e.note_input();
        let evs = e.on_tick(Some(C::Insert), T0 + 1_000, true);
        assert!(evs.is_empty(), "no Shown for an under-floor pool: {evs:?}");
        assert_eq!(displayed(&e, C::Insert), None);
        assert!(e.view(Some(C::Insert), true).is_none());
    }

    #[test]
    fn intro_shows_dismiss_then_toggle_before_sampling() {
        let (e, evs) = engine();
        // A fresh profile's very first hint is deterministically the dismiss hint.
        assert_eq!(displayed(&e, C::Normal), Some("dismiss"));
        assert_eq!(
            evs,
            vec![WireEvent {
                hint_id: "dismiss",
                event: HintEvent::Shown
            }]
        );

        // Pressing Space h on it is the follow that advances the intro — not a dismissal.
        let (mut e, _) = engine();
        let evs = e.dismiss(Some(C::Normal), true);
        assert!(evs.contains(&WireEvent {
            hint_id: "dismiss",
            event: HintEvent::Followed
        }));
        assert!(
            !evs.iter().any(|ev| ev.event == HintEvent::Dismissed),
            "trying the dismiss binding on its own hint is success, not rejection: {evs:?}"
        );
        assert_eq!(
            displayed(&e, C::Normal),
            Some("toggle"),
            "the intro's second hint follows deterministically"
        );

        // Rotation walks the intro too: an ignored dismiss hint still yields to the toggle hint.
        let (mut e, _) = engine();
        let mut now = T0;
        while displayed(&e, C::Normal) == Some("dismiss") {
            now += 30_000;
            assert!(now < T0 + 2 * ROTATE_MS, "rotation must advance the intro");
            e.note_input();
            e.on_tick(Some(C::Normal), now, true);
        }
        assert_eq!(displayed(&e, C::Normal), Some("toggle"));
    }

    #[test]
    fn dismissal_downweights_and_rotates() {
        let (mut e, _) = engine();
        // Advance past the dismiss hint (following it is special-cased); the toggle hint is an
        // ordinary dismissal target.
        e.dismiss(Some(C::Normal), true);
        let target = displayed(&e, C::Normal).unwrap();
        assert_eq!(target, "toggle");
        let idx = curriculum_index(target).unwrap();
        let score_before = e.score(idx);
        let evs = e.dismiss(Some(C::Normal), true);
        assert!(evs.contains(&WireEvent {
            hint_id: target,
            event: HintEvent::Dismissed
        }));
        // The press also demonstrated the dismiss binding itself (learning is global).
        assert!(evs.contains(&WireEvent {
            hint_id: "dismiss",
            event: HintEvent::Used
        }));
        assert_ne!(displayed(&e, C::Normal), Some(target), "dismissal rotates");
        // DISMISS_WEIGHT (2) at FATIGUE_SCALE (2) is exactly one halving of the score — twice
        // what a lapsed display period costs.
        assert!(
            e.score(idx) <= score_before / 2.0 + 1e-6,
            "a dismissal halves the score: {} vs {}",
            e.score(idx),
            score_before
        );
        // Dismissing an empty corner records the binding use but dismisses nothing.
        let evs = e.dismiss(Some(C::Settings), true);
        assert!(!evs.iter().any(|ev| ev.event == HintEvent::Dismissed));
    }

    #[test]
    fn hint_view_splits_its_template_around_the_key_slot() {
        let v = HintView {
            keys: "Ctrl-z",
            text: "Use {} to undo",
        };
        assert_eq!(v.parts(), ("Use ", "Ctrl-z", " to undo"));
        // A keyless tip renders as plain text.
        let v = HintView {
            keys: "",
            text: "Selections are the object of every edit",
        };
        assert_eq!(
            v.parts(),
            ("Selections are the object of every edit", "", "")
        );
    }

    #[test]
    fn workspace_chooser_hint_tracks_the_list_and_preempts() {
        let (mut e, _) = engine();
        let ws = C::Picker(PickerKind::Workspaces);
        // Chooser open, list not yet loaded (facts None): neither workspace hint is eligible, so
        // the generic picker-nav samples.
        e.note_input();
        e.on_tick(Some(ws), T0 + 1_000, true);
        assert_eq!(displayed(&e, ws), Some("picker-nav"));

        // The (empty) list loads: the create hint preempts the sampled slot — it's intro-listed
        // and the chooser is the first thing a fresh install sees.
        e.set_facts(HintFacts {
            workspaces_listed: Some(0),
            ..Default::default()
        });
        let evs = e.sync_context(Some(ws), true);
        assert_eq!(displayed(&e, ws), Some("workspace-create"));
        assert!(evs.contains(&WireEvent {
            hint_id: "workspace-create",
            event: HintEvent::Shown
        }));

        // Workspaces appear (created one / typed a matching query): the open hint takes over —
        // the conditions are complementary, so the two never compete.
        e.set_facts(HintFacts {
            workspaces_listed: Some(2),
            ..Default::default()
        });
        e.sync_context(Some(ws), true);
        assert_eq!(displayed(&e, ws), Some("workspace-open"));

        // Accepting a workspace row is the open hint's follow.
        let evs = e.observe_picker(PickerCmd::OpenWorkspace, Some(ws), true);
        assert!(evs.contains(&WireEvent {
            hint_id: "workspace-open",
            event: HintEvent::Followed
        }));
    }

    #[test]
    fn picker_dismiss_hint_waits_for_the_picker_basics() {
        let (mut e, _) = engine();
        let files = C::Picker(PickerKind::Files);
        e.note_input();
        e.retired.insert("picker-scope"); // isolate the Files pool to nav + dismiss

        // The help hint has displayed, and Alt-j/k has been *used* once (long ago — its hint
        // scores full recency) but never shown and not demonstrated. Mere use must not count
        // as "past": the corner teaches the movement hint first, not the way out.
        e.records.insert(
            "help",
            HintRecord {
                last_shown_at: T0 - DAY_MS,
                ..Default::default()
            },
        );
        e.records.insert(
            "picker-nav",
            HintRecord {
                uses: 1,
                use_days: 1,
                last_used_day: day_from_unix_ms(T0 - 30 * DAY_MS),
                last_used_at: T0 - 30 * DAY_MS,
                ..Default::default()
            },
        );
        e.on_tick(Some(files), T0 + 1_000, true);
        assert_eq!(
            displayed(&e, files),
            Some("picker-nav"),
            "a used-but-never-shown picker-nav still teaches before the dismiss hint"
        );

        // Now demonstrated (uses saturated — it leaves the pool, so waiting on a show would
        // deadlock): the sequence unblocks and the dismiss hint takes the resampled slot.
        e.records.get_mut("picker-nav").unwrap().uses = LEARNED_USES;
        let evs = e.sync_context(Some(files), true);
        assert_eq!(displayed(&e, files), Some("picker-dismiss"));
        assert!(evs.contains(&WireEvent {
            hint_id: "picker-dismiss",
            event: HintEvent::Shown
        }));

        // Esc while it's up is the follow.
        let evs = e.observe_picker(PickerCmd::Dismiss, Some(files), true);
        assert!(evs.contains(&WireEvent {
            hint_id: "picker-dismiss",
            event: HintEvent::Followed
        }));
    }

    #[test]
    fn picker_dismiss_hint_suppressed_in_the_mandatory_chooser() {
        let (mut e, _) = engine();
        let ws = C::Picker(PickerKind::Workspaces);
        e.note_input();
        // Prerequisites met and every pool-mate out of the way — the only reason the corner can
        // stay empty is the suppression itself.
        for id in ["workspace-create", "workspace-open", "picker-nav", "help"] {
            e.retired.insert(id);
        }
        // The boot chooser: Esc exits the app there instead of closing the picker, so the
        // dismiss hint would mislead.
        e.set_facts(HintFacts {
            workspaces_listed: Some(1),
            mandatory_chooser: true,
        });
        e.on_tick(Some(ws), T0 + 1_000, true);
        assert_eq!(
            displayed(&e, ws),
            None,
            "no dismiss hint in the mandatory chooser"
        );

        // The same picker mid-session (Space w over a real workspace): Esc closes it, teach away.
        e.set_facts(HintFacts {
            workspaces_listed: Some(1),
            mandatory_chooser: false,
        });
        e.on_tick(Some(ws), T0 + 3_000, true);
        assert_eq!(displayed(&e, ws), Some("picker-dismiss"));
    }

    #[test]
    fn intro_does_not_preempt_an_earlier_intro_hint() {
        let (mut e, _) = engine();
        // The dismiss hint (intro rank 2) is displaying in Normal; the toggle hint (rank 3) is
        // also pending. A later-ranked pending intro must wait for rotation, not preempt.
        assert_eq!(displayed(&e, C::Normal), Some("dismiss"));
        let evs = e.sync_context(Some(C::Normal), true);
        assert!(evs.is_empty());
        assert_eq!(displayed(&e, C::Normal), Some("dismiss"));
    }

    #[test]
    fn search_option_hints_are_split_per_chord() {
        let (mut e, _) = engine();
        // Each toggle demonstrates only its own hint.
        let evs = e.observe_action(&Action::SearchToggleWord, Some(C::Search), true);
        assert!(evs
            .iter()
            .any(|ev| ev.hint_id == "search-word" && ev.event == HintEvent::Used));
        assert!(!evs
            .iter()
            .any(|ev| ev.hint_id == "search-case" || ev.hint_id == "search-regex"));
    }

    #[test]
    fn every_curriculum_id_is_unique() {
        let mut seen = HashSet::new();
        for h in CURRICULUM {
            assert!(seen.insert(h.id), "duplicate hint id {}", h.id);
        }
    }

    #[test]
    fn every_keyed_hint_templates_its_key_slot() {
        for h in CURRICULUM {
            assert!(
                h.keys.is_empty() || h.text.contains("{}"),
                "hint '{}' has keys but its text never places them",
                h.id
            );
        }
    }

    /// The keymap cross-check (docs/hints.md §1.10): every action-triggered hint must still match
    /// a real binding, and that binding's rendered chord must appear in the hint's authored `keys`
    /// label — so a renamed action or a moved chord can't leave a stale hint behind. (Picker-cmd
    /// hints have no table to check; their arms are instrumented by hand.)
    #[test]
    fn curriculum_matches_the_live_keymap() {
        for h in CURRICULUM {
            let Trigger::Action(m) = &h.trigger else {
                continue;
            };
            let matching: Vec<&crate::keymap::Binding> =
                crate::keymap::all().filter(|b| m(&b.action)).collect();
            assert!(
                !matching.is_empty(),
                "hint '{}' matches no binding in the keymap tables",
                h.id
            );
            assert!(
                matching.iter().any(|b| h.keys.contains(&b.key_label())),
                "hint '{}' advertises keys '{}', but no matching binding renders a chord \
                 contained in it (candidates: {:?})",
                h.id,
                h.keys,
                matching.iter().map(|b| b.key_label()).collect::<Vec<_>>()
            );
        }
    }

    /// An action that satisfies the given hint's matcher — the test-side inverse of the
    /// curriculum's matchers, so tests can "press" a hint's binding.
    fn trigger_action_for(id: &str) -> Action {
        use aether_protocol::cursor::Direction;
        match id {
            "dismiss" => Action::DismissHint,
            "toggle" => Action::ToggleHints,
            "help" => Action::OpenHelp,
            "quit" => Action::Quit,
            "insert" => Action::EnterInsert(crate::keymap::InsertWhere::SelectionStart),
            "leave-insert" => Action::LeaveInsert,
            "motion-hjkl" => Action::MoveChar(Direction::Forward),
            "save" => Action::Save,
            "picker-files" => Action::OpenPicker(PickerKind::Files),
            "picker-buffers" => Action::OpenPicker(PickerKind::Buffers),
            "search" => Action::EnterSearch,
            "undo" => Action::Undo,
            other => panic!("no test action mapped for hint {other}"),
        }
    }
}
