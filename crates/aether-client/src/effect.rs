//! Effects — what core logic asks its shell to do. The core mutates its own state and
//! returns these; the shell executes them (spawning futures on its runtime, presenting
//! toasts, touching the clipboard) and feeds resulting events back into the core.
//!
//! Generic over the event type `E` so logic can migrate into the core piecemeal: shell code
//! mid-migration produces `Effects<Message>`, core code produces `Effects<Event>` — the
//! shell maps the latter back through its bridge variant.

use super::keymap::Action;
use super::session::{HoverText, PasteKind};
use futures_util::future::BoxFuture;

/// Web-client toast kinds; the colour of the toast's accent bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
    Warning,
    Success,
}

pub enum Effect<E> {
    /// Run a future on the shell's executor; feed the produced event back into update.
    Spawn(BoxFuture<'static, E>),
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
    Reconnect { attempt: u32 },
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
pub struct Effects<E>(pub Vec<Effect<E>>);

impl<E> Effects<E> {
    pub fn none() -> Self {
        Effects(Vec::new())
    }

    pub fn one(e: Effect<E>) -> Self {
        Effects(vec![e])
    }

    pub fn spawn(fut: impl std::future::Future<Output = E> + Send + 'static) -> Self {
        Effects::one(Effect::Spawn(Box::pin(fut)))
    }

    pub fn toast(message: impl Into<String>, kind: ToastKind) -> Self {
        Effects::one(Effect::Toast(message.into(), kind))
    }

    pub fn error(message: impl Into<String>) -> Self {
        Effects::toast(message, ToastKind::Error)
    }

    #[allow(dead_code)] // exercised as more update arms migrate into core
    pub fn push(&mut self, e: Effect<E>) {
        self.0.push(e);
    }

    /// Append `other`'s effects after this batch's (the `Task::batch` analogue).
    #[allow(dead_code)] // exercised as more update arms migrate into core
    pub fn and(mut self, other: Effects<E>) -> Self {
        self.0.extend(other.0);
        self
    }
}
