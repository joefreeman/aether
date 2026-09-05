//! Effects — what core logic asks its shell to do. The core mutates its own state and returns
//! these; the shell executes them (performing RPC requests, presenting toasts, touching the
//! clipboard) and feeds outcomes back into the core. Pure data: the core is sans-IO — it never
//! constructs futures, so the whole surface is inspectable and the update loop unit-testable with
//! canned results.

use super::keymap::{ScrollDir, ScrollUnit, ViewportPlace};
use super::session::{HoverText, PasteKind};
use aether_protocol::BufferId;
use std::time::Duration;

/// An action whose execution is irreducibly shell-side — geometry (pixel scroll, cell metrics,
/// cursor placement), viewport wrap plumbing, or the help overlay. The keymap and dispatch stay in
/// the core; only the body is the shell's. Deliberately a small, closed set (not the whole `Action`
/// enum) so every shell matches it exhaustively — a new shell-action can't be silently dropped.
#[derive(Debug, Clone)]
pub enum ShellAction {
    /// Pixel/row scroll by direction and unit.
    Scroll { dir: ScrollDir, unit: ScrollUnit },
    /// Rest the cursor at a viewport position (`;` / `Alt-;`).
    PlaceCursor(ViewportPlace),
    /// Flip soft-wrap and re-render the viewport (paired with [`Effect::SaveContentAnchor`]).
    ToggleWrap,
    /// Open a URL — or an absolute file path — with the system handler (the reading view's `Enter`
    /// on an external link or image). The GUI reuses its hover-link opener (allow-listed schemes +
    /// spawn), the TUI spawns the same system opener, the web shell opens a new tab. Never a
    /// relative path: the core resolves against the buffer's directory before emitting.
    OpenUrl(String),
    /// Open a file that lives beside the buffer — a *local* image's `Enter`. Native shells open
    /// `absolute` with the system handler; a browser can't touch local paths, so the web shell
    /// opens the server's confined `/asset/{buffer_id}/{relative}` route in a new tab instead (the
    /// same route its `<img>` tags already load from).
    OpenBufferFile {
        absolute: String,
        buffer_id: BufferId,
        relative: String,
    },
    /// Open a [`WindowTarget`] in a *new* window. Two entry points build the target in the core:
    /// `Space z` ([`crate::keymap::Action::NewWindow`]) duplicates the current view, and
    /// `Ctrl-Enter` in a picker opens the highlighted item (the native sibling of the web client's
    /// Ctrl/Cmd-Enter "open in a new tab"). The GUI shell spawns a fresh detached `ae --gui` seeded
    /// from the target; the TUI ignores it (no window to spawn); the web shell opens a new browser
    /// tab on the same URL (`window.open`) — its Ctrl-Enter is handled shell-side, so the picker
    /// path never reaches here on the web.
    NewWindow(WindowTarget),
    /// Copy the web client's URL for the current view (`Space Alt-z`). `path_query` is the
    /// `?workspace=…` query (+ optional `#L:C` fragment) from [`crate::web_link`]; the shell
    /// prepends its own base and writes the clipboard — the native shells derive `http://…`
    /// from the server address they dialed, the web shell uses its own origin (which may be a
    /// port-forward the server's loopback address would misname). The confirmation toast rides
    /// alongside from the core, like every copy gesture, so the shell adds none.
    CopyWebUrl { path_query: String },
}

/// A resolved target for opening a *new* window ([`ShellAction::NewWindow`]). The core resolves
/// everything the spawning shell needs into plain strings/ids — the shell only turns it into a fresh
/// `ae` invocation. Built by [`crate::update`]'s `current_view_target` (`Space z`) or
/// `picker_item_target` (`Ctrl-Enter`), the latter's item set mirroring the web client's `pickerItemUrl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowTarget {
    /// `--workspace NAME` for the new window, or `None` to open by path alone — an ephemeral,
    /// no-workspace open, used for a file outside every workspace (an ephemeral workspace id isn't
    /// CLI-addressable).
    pub workspace: Option<String>,
    /// Which **context** of that workspace to open: repo id → worktree admin name, empty for the
    /// base. Carried as the binding *set* rather than the server's internal context id, because the
    /// id is internal — activation takes the set and derives it.
    ///
    /// Deliberately not a CLI flag. `Space z` opens a window in-process (the GUI) or a tab (the
    /// web), so this never has to survive a command line; and a user-facing `--worktree` beside a
    /// path positional would raise "is that path relative to the checkout or the tree?", which has
    /// no non-arbitrary answer. Reaching a context by hand is what the branch picker is for.
    pub worktrees: Vec<(String, String)>,
    /// What the new window lands on.
    pub open: WindowOpen,
}

/// The thing a [`WindowTarget`] opens: a file (optionally jumped to a location), an existing buffer
/// by id (a scratch, re-openable because the new window dials the same daemon), or just the
/// workspace's MRU buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowOpen {
    /// Open a file by absolute path, optionally jumping to a 0-based `(line, col)` (a grep hit).
    Path {
        path: String,
        at: Option<(u32, u32)>,
    },
    /// Present an existing view by id — a scratch with no path, addressable across clients because
    /// views are daemon-global. Stale-id-safe: the shell falls back to the MRU/scratch if the id is
    /// gone (the daemon restarted).
    View(aether_protocol::ViewId),
    /// No specific file: activate the workspace and land on its MRU buffer (the `Space z`
    /// duplicate, and the Workspaces picker's "open this workspace in a new window").
    Workspace,
}

/// A toast's intent: the colour of its accent bar and title, and (via [`ToastKind::pinned`] /
/// [`ToastKind::ttl`]) how long it stays up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
    Warning,
    Success,
}

impl ToastKind {
    /// Whether a toast of this kind stays up until it's *dismissed* (Esc, or a click in the GUI/web
    /// shells) rather than fading on its own. Only errors pin: they're the ones carrying detail
    /// worth reading, and they're rare enough that holding the corner isn't noise. Pinned toasts
    /// still expire on [`ToastKind::ttl`] as a backstop, so a forgotten one can't linger forever.
    pub fn pinned(self) -> bool {
        matches!(self, ToastKind::Error)
    }

    /// How long a toast of this kind stays before it auto-dismisses. Confirmations (a save, a copy)
    /// are read at a glance; a warning is a sentence worth finishing; an error is pinned and this is
    /// only its backstop. Shared by all three shells so their timings can't drift.
    pub fn ttl(self) -> Duration {
        match self {
            ToastKind::Info | ToastKind::Success => Duration::from_millis(3600),
            ToastKind::Warning => Duration::from_millis(6000),
            ToastKind::Error => Duration::from_millis(20_000),
        }
    }
}

/// How a cursor reveal should reposition the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealStyle {
    /// Ordinary motions/edits: keep the view stable, scrolling the minimum to bring the cursor
    /// on-screen (and nothing when it's already visible).
    Follow,
    /// A jump to a specific target (search hit, diagnostic, hunk, go-to-line, a cross-buffer open):
    /// if the cursor is already visible it stays put; otherwise the shell rests it near the top of
    /// the viewport ([`CURSOR_REST_FRACTION`](crate::keymap::CURSOR_REST_FRACTION) down), where
    /// there's more context below. Short same-buffer jumps animate the scroll there; far (and
    /// cross-buffer) ones snap.
    Jump,
}

pub enum Effect {
    /// Perform this JSON-RPC call and feed the outcome back through `Session::on_rpc_result` with
    /// the same token. Requests are performed in emission order on the single connection —
    /// sequenced flows rely on it. (The sans-IO replacement for `Spawn`-ing an RPC future)
    Request {
        token: u64,
        method: &'static str,
        params: serde_json::Value,
    },
    /// Show a transient message (styling is the shell's; lifetime comes from the `kind`, see
    /// [`ToastKind::ttl`]). `title` is the headline, shown in the kind's colour — keep it short and
    /// scannable ("Push failed", not "Push failed: remote rejected…"). The optional `body` carries
    /// the detail or the suggested next step, rendered muted underneath; it's what the reader turns
    /// to *after* the title has told them which way it went.
    ///
    /// When `group` is set, the shell replaces any existing toast carrying the same key — refreshing
    /// its lifetime — instead of stacking a new one, so a status that evolves (an LSP server's
    /// "Restarting" → "ready", the diff toggle, the reconnect lifecycle) updates a single toast in
    /// place. `None` (the default, via [`Effects::toast`]/[`Effects::error`]) always stacks a fresh
    /// toast — the right behaviour for discrete confirmations (saves, copies, deletes).
    Toast {
        title: String,
        body: Option<String>,
        kind: ToastKind,
        group: Option<String>,
    },
    /// Put text on the system clipboard.
    WriteClipboard(String),
    /// Scroll so the cursor is on-screen — geometry, so the shell owns the how (pixel
    /// reveal + window fetch for the GUI; row scrolling for a terminal). The [`RevealStyle`]
    /// distinguishes an ordinary follow from a navigation jump (rest near the top, animate if
    /// short).
    RevealCursor(RevealStyle),
    /// The session switched buffers: reset view-side presentation (scroll, hover) and
    /// subscribe a fresh viewport at the shell's grid.
    Resubscribe,
    /// Remember the current scroll position (the search prompt's Esc-restore anchor —
    /// geometry, so the shell holds the value).
    SaveScrollAnchor,
    /// Jump back to the remembered scroll position (and forget it).
    RestoreScrollAnchor,
    /// Capture a *content* scroll anchor before a wrap/diff re-layout: the shell calls
    /// [`crate::session::Session::capture_scroll_anchor`] with its current top visual row, so the
    /// view can be restored to the same content once the re-laid-out window arrives. Distinct from
    /// the geometry-based [`Effect::SaveScrollAnchor`] (correct for search, which doesn't relayout).
    /// The restore side is folded into [`Effect::WindowAdopted`] and the shells' wrap-adopt paths,
    /// which call [`crate::session::Session::resolve_scroll_anchor`].
    SaveContentAnchor,
    /// Show the hover popover with this content (the shell parses/styles it).
    ShowHover(HoverText),
    DismissHover,
    /// The core replaced the window wholesale (wrap/diff toggle): re-derive view geometry. If a
    /// content anchor is pending (see [`Effect::SaveContentAnchor`]) the shell restores the view to
    /// it; otherwise it clamps the scroll and reveals the cursor.
    WindowAdopted,
    /// Scroll the picker's results list so the highlighted row is in view (geometry — the
    /// pixel math and the scrollable live in the shell).
    RevealPickerSelection(super::picker::Reveal),
    /// The picker's results list restarted at the top (fresh open, query/filter change,
    /// explorer navigation) — zero the shell's scroll state and snap the list widget there.
    PickerScrollReset,
    /// Dial the server again after this attempt's backoff (the mechanism — discovery, the
    /// socket — is the shell's; the core owns the policy that asked for it).
    Reconnect {
        attempt: u32,
    },
    /// Call `Session::on_hint_tick` with the current wall clock, promptly. Emitted when the
    /// `hints/state` snapshot adopts: the engine is sans-IO (time reaches it only through the tick
    /// entry point), so without this the first hint would wait out the shell's periodic tick
    /// interval — the shell answers with one out-of-band tick and the first hint shows right after
    /// adoption instead of seconds later.
    HintTickNow,
    /// Quit the application. A shell with no process to quit (the web — a browser tab) maps this
    /// to a no-op; the mandatory chooser's Esc relies on that (the core keeps the picker open and
    /// emits `Exit`, so the web chooser simply stays up).
    Exit,
    /// Return to the workspace chooser, discarding the current (now buffer-less) session — used when
    /// the last buffer of an ephemeral context closes on a client that *navigated into* it rather
    /// than launching for a file (so it shouldn't quit). The shell resets to its boot-chooser state
    /// (a fresh placeholder session + the Workspaces picker, uniform across all three shells); it's
    /// an effect rather than a core mutation because the session swap is the shell's — it owns the
    /// `Session` value and its render state.
    ToChooser,
    /// Read the system clipboard; the text comes back as `Event::ClipboardRead`.
    ReadClipboard(PasteKind),
    /// An action whose execution is irreducibly shell-side (see [`ShellAction`]) — the keymap and
    /// dispatch stay core; the body doesn't.
    ShellAction(ShellAction),
}

/// An ordered batch of effects, with builder conveniences mirroring how `iced::Task` reads
/// at the call sites it replaces.
pub struct Effects(pub Vec<Effect>);

impl Effects {
    pub fn none() -> Self {
        Effects(Vec::new())
    }

    pub fn one(e: Effect) -> Self {
        Effects(vec![e])
    }

    pub fn toast(title: impl Into<String>, kind: ToastKind) -> Self {
        Effects::one(Effect::Toast {
            title: title.into(),
            body: None,
            kind,
            group: None,
        })
    }

    /// A toast with a muted detail line under its title (see [`Effect::Toast`]). Use it instead of
    /// folding the detail into the title with a `: ` — the title stays scannable and the detail gets
    /// room to wrap. An empty `body` degrades to a plain [`Effects::toast`], so a formatted error
    /// string that happens to come back blank doesn't leave a dangling line.
    pub fn toast_detail(
        title: impl Into<String>,
        body: impl Into<String>,
        kind: ToastKind,
    ) -> Self {
        Effects::one(Effect::Toast {
            title: title.into(),
            body: non_empty(body),
            kind,
            group: None,
        })
    }

    /// A toast that *replaces* any existing toast sharing `group` (see [`Effect::Toast`]). Use for a
    /// status that should update one toast in place rather than stack — keyed so distinct subjects
    /// (e.g. two LSP servers) still get their own toast.
    pub fn toast_grouped(
        title: impl Into<String>,
        kind: ToastKind,
        group: impl Into<String>,
    ) -> Self {
        Effects::one(Effect::Toast {
            title: title.into(),
            body: None,
            kind,
            group: Some(group.into()),
        })
    }

    /// [`Effects::toast_grouped`] with a detail line ([`Effects::toast_detail`]).
    pub fn toast_grouped_detail(
        title: impl Into<String>,
        body: impl Into<String>,
        kind: ToastKind,
        group: impl Into<String>,
    ) -> Self {
        Effects::one(Effect::Toast {
            title: title.into(),
            body: non_empty(body),
            kind,
            group: Some(group.into()),
        })
    }

    pub fn error(title: impl Into<String>) -> Self {
        Effects::toast(title, ToastKind::Error)
    }

    /// An error toast whose detail — the failure's own message, or the suggested fix — reads on its
    /// own muted line. The shape to reach for when the alternative is `format!("{what}: {e}")`.
    pub fn error_detail(title: impl Into<String>, body: impl Into<String>) -> Self {
        Effects::toast_detail(title, body, ToastKind::Error)
    }

    pub fn push(&mut self, e: Effect) {
        self.0.push(e);
    }

    /// Append `other`'s effects after this batch's (the `Task::batch` analogue).
    pub fn and(mut self, other: Effects) -> Self {
        self.0.extend(other.0);
        self
    }
}

/// A toast body, dropped when it's blank — see [`Effects::toast_detail`].
fn non_empty(body: impl Into<String>) -> Option<String> {
    let body = body.into();
    (!body.trim().is_empty()).then_some(body)
}
