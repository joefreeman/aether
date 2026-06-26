//! Effects — what core logic asks its shell to do. The core mutates its own state and
//! returns these; the shell executes them (performing RPC requests, presenting toasts,
//! touching the clipboard) and feeds outcomes back into the core. Pure data: the core is
//! sans-IO — it never constructs futures, so the whole surface is inspectable and the
//! update loop unit-testable with canned results (docs/client-core.md).

use super::keymap::{ScrollDir, ScrollUnit, ViewportPlace};
use super::session::{HoverText, PasteKind};

/// An action whose execution is irreducibly shell-side — geometry (pixel scroll, cell metrics,
/// cursor placement), viewport wrap plumbing, or the help overlay. The keymap and dispatch stay in
/// the core; only the body is the shell's. Deliberately a small, closed set (not the whole `Action`
/// enum) so every shell matches it exhaustively — a new shell-action can't be silently dropped.
#[derive(Debug, Clone, Copy)]
pub enum ShellAction {
    /// Pixel/row scroll by direction and unit.
    Scroll { dir: ScrollDir, unit: ScrollUnit },
    /// Rest the cursor at a viewport position (`;` / `Alt-;`).
    PlaceCursor(ViewportPlace),
    /// Flip soft-wrap and re-render the viewport (paired with [`Effect::SaveContentAnchor`]).
    ToggleWrap,
    /// Open the shell-local help cheatsheet (rendered from the keymap tables).
    OpenHelp,
}

/// Web-client toast kinds; the colour of the toast's accent bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
    Warning,
    Success,
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
    /// Perform this JSON-RPC call and feed the outcome back through
    /// `Session::on_rpc_result` with the same token. Requests are performed in emission
    /// order on the single connection — sequenced flows rely on it. (The sans-IO
    /// replacement for `Spawn`-ing an RPC future; docs/client-core.md.)
    Request {
        token: u64,
        method: &'static str,
        params: serde_json::Value,
    },
    /// Show a transient message (display duration and styling are the shell's). When `group` is
    /// set, the shell replaces any existing toast carrying the same key — refreshing its lifetime —
    /// instead of stacking a new one, so a status that evolves (an LSP server's "Restarting" →
    /// "ready", the diff toggle, the reconnect lifecycle) updates a single toast in place. `None`
    /// (the default, via [`Effects::toast`]/[`Effects::error`]) always stacks a fresh toast — the
    /// right behaviour for discrete confirmations (saves, copies, deletes).
    Toast {
        message: String,
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
    /// Quit the application.
    Exit,
    /// Return to the workspace chooser, discarding the current (now buffer-less) session — used when
    /// the last buffer of an ephemeral context closes on a client that *navigated into* it rather
    /// than launching for a file (so it shouldn't quit). The shell resets to its boot-chooser
    /// state (a placeholder session + the Workspaces picker); the core can't do this by mutating its
    /// own fields because each shell presents its chooser differently (the TUI swaps in a
    /// placeholder session, iced has a separate `boot` state, the web rebuilds the session).
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

    pub fn toast(message: impl Into<String>, kind: ToastKind) -> Self {
        Effects::one(Effect::Toast {
            message: message.into(),
            kind,
            group: None,
        })
    }

    /// A toast that *replaces* any existing toast sharing `group` (see [`Effect::Toast`]). Use for a
    /// status that should update one toast in place rather than stack — keyed so distinct subjects
    /// (e.g. two LSP servers) still get their own toast.
    pub fn toast_grouped(
        message: impl Into<String>,
        kind: ToastKind,
        group: impl Into<String>,
    ) -> Self {
        Effects::one(Effect::Toast {
            message: message.into(),
            kind,
            group: Some(group.into()),
        })
    }

    pub fn error(message: impl Into<String>) -> Self {
        Effects::toast(message, ToastKind::Error)
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
