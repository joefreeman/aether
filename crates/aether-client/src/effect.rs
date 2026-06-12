//! Effects — what core logic asks its shell to do. The core mutates its own state and
//! returns these; the shell executes them (performing RPC requests, presenting toasts,
//! touching the clipboard) and feeds outcomes back into the core. Pure data: the core is
//! sans-IO — it never constructs futures, so the whole surface is inspectable and the
//! update loop unit-testable with canned results (docs/client-core.md).

use super::keymap::Action;
use super::session::{HoverText, PasteKind};

/// Web-client toast kinds; the colour of the toast's accent bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
    Warning,
    Success,
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
    /// Show a transient message (display duration and styling are the shell's).
    Toast(String, ToastKind),
    /// Put text on the system clipboard.
    WriteClipboard(String),
    /// Scroll so the cursor is on-screen — geometry, so the shell owns the how (pixel
    /// reveal + window fetch for the GUI; row scrolling for a terminal).
    RevealCursor,
    /// The session switched buffers: reset view-side presentation (scroll, hover) and
    /// subscribe a fresh viewport at the shell's grid.
    Resubscribe,
    /// Remember the current scroll position (the search prompt's Esc-restore anchor —
    /// geometry, so the shell holds the value).
    SaveScrollAnchor,
    /// Jump back to the remembered scroll position (and forget it).
    RestoreScrollAnchor,
    /// Show the hover popover with this content (the shell parses/styles it).
    ShowHover(HoverText),
    DismissHover,
    /// The core replaced the window wholesale (diff toggle): re-derive view geometry —
    /// clamp the scroll, reveal the cursor.
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
    /// Read the system clipboard; the text comes back as `Event::ClipboardRead`.
    ReadClipboard(PasteKind),
    /// An action whose execution is irreducibly shell-side (pixel scrolling, cell metrics,
    /// viewport wrap plumbing) — the keymap and dispatch stay core; the body doesn't.
    ShellAction(Action),
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
        Effects::one(Effect::Toast(message.into(), kind))
    }

    pub fn error(message: impl Into<String>) -> Self {
        Effects::toast(message, ToastKind::Error)
    }

    #[allow(dead_code)] // exercised as more update arms migrate into core
    pub fn push(&mut self, e: Effect) {
        self.0.push(e);
    }

    /// Append `other`'s effects after this batch's (the `Task::batch` analogue).
    #[allow(dead_code)] // exercised as more update arms migrate into core
    pub fn and(mut self, other: Effects) -> Self {
        self.0.extend(other.0);
        self
    }
}
