//! The cursor reveal a shell still owes its viewport.
//!
//! Shells own their own scroll — rows in the terminal, pixels in the GUI — so the *act* of
//! revealing the cursor lives in each shell. What they share, and what this owns, is the rule for
//! when a reveal may be forgotten: **only once it has actually happened.**
//!
//! A reveal is owed when a cursor move lands outside the loaded window: the cursor's visual row is
//! unknown until the server sends a window carrying that line, so the shell asks for one and
//! reveals when it arrives. The trap is that window responses do not answer one-for-one to the
//! request that armed the debt — the shells also fetch windows to chase their *scroll* position,
//! and either fetch can answer first. Clearing the debt on a window that doesn't carry the
//! cursor's line is clearing a reveal that never happened: the scroll stays where it was, the next
//! scroll-driven fetch pulls the window back to *it*, and the two settle into a stable
//! disagreement — the viewport parked somewhere the cursor isn't, every later motion finding the
//! cursor "already in the window" and revealing nothing. Only a buffer switch escapes it.
//!
//! So the debt has no `take`. It is cleared by [`PendingReveal::settle`], and only when the shell's
//! own reveal reports that it could be performed; [`Settled::Unpaid`] tells the shell to go and ask
//! for a window around the cursor, which is the only thing that can pay it.

use crate::effect::RevealStyle;
use crate::keymap::ViewportPlace;

/// A shell's ability to act on an owed reveal. Each method performs the shell's own scrolling and
/// reports whether it could — `false` means the cursor's visual row isn't known, because the
/// loaded window doesn't carry the cursor's line.
pub trait RevealTarget {
    fn reveal(&mut self, style: RevealStyle) -> bool;
    fn place(&mut self, place: ViewportPlace) -> bool;
}

/// What [`PendingReveal::settle`] managed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// Nothing was owed. The caller's own default reveal, if it has one, is free to run.
    Nothing,
    /// Everything owed was performed.
    Paid,
    /// Something was owed and this window couldn't answer for it. It stays owed, and the caller
    /// must fetch a window around the cursor — nothing else will, since the shell's other fetch
    /// path chases the scroll position, which is exactly what hasn't caught up.
    Unpaid,
    /// The window fetched **for the cursor** couldn't place it either, so nothing can: the debt has
    /// been dropped. The caller must not fetch again — see [`PendingReveal::settle_chase`].
    Unplaceable,
}

/// The reveal (and/or placement) a shell owes its viewport. See the module docs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PendingReveal {
    reveal: Option<RevealStyle>,
    place: Option<ViewportPlace>,
}

impl PendingReveal {
    /// Owe a reveal in this style. A later cursor move replacing an earlier style is correct: the
    /// reveal is always performed against wherever the cursor has *reached*, so the style that
    /// should win is the one belonging to the move that put it there.
    pub fn owe_reveal(&mut self, style: RevealStyle) {
        self.reveal = Some(style);
    }

    /// Owe a fixed-fraction placement (`;` / `Alt-;`), which can be owed alongside a reveal.
    pub fn owe_place(&mut self, place: ViewportPlace) {
        self.place = Some(place);
    }

    pub fn is_owed(&self) -> bool {
        self.reveal.is_some() || self.place.is_some()
    }

    /// Drop the debt unperformed. Only for the cases where the reveal has been *superseded* rather
    /// than paid: a fresh subscribe (new viewport, new window, new everything) and a relayout
    /// anchor, which positions the view itself and would be fought by a reveal.
    pub fn abandon(&mut self) {
        *self = Self::default();
    }

    /// Pay what's owed against the window the shell has now loaded.
    pub fn settle(&mut self, target: &mut impl RevealTarget) -> Settled {
        if !self.is_owed() {
            return Settled::Nothing;
        }
        if let Some(style) = self.reveal {
            if target.reveal(style) {
                self.reveal = None;
            }
        }
        if let Some(place) = self.place {
            if target.place(place) {
                self.place = None;
            }
        }
        if self.is_owed() {
            Settled::Unpaid
        } else {
            Settled::Paid
        }
    }

    /// [`Self::settle`] against the window that was fetched **for the cursor** — the answer to the
    /// chase [`Settled::Unpaid`] asked for.
    ///
    /// If *that* window can't place the cursor, nothing can: the cursor is not in this view at all
    /// (its line belongs to a different buffer than the focused element windows, say), and asking
    /// again fetches the same window forever. Which is what the terminal did: every reply re-seated
    /// the scroll to the element's start row, so the viewport flickered, refused to scroll, and
    /// showed no cursor — because there was no cursor in it to show. The debt is dropped here, and
    /// the caller is told so rather than being sent back round.
    ///
    /// This is the *only* forgiveness the debt has beyond being paid, and it is deliberately
    /// narrow: it applies to the answer to a chase and nothing else. A window that arrived for some
    /// other reason proves nothing about the cursor (see the module docs on out-of-order answers)
    /// and still leaves the reveal owed.
    pub fn settle_chase(&mut self, target: &mut impl RevealTarget) -> Settled {
        match self.settle(target) {
            Settled::Unpaid => {
                self.abandon();
                Settled::Unplaceable
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell stub standing in for the two window cases: `answers` is whether the loaded window
    /// carries the cursor's line, which is exactly what decides whether a reveal can happen.
    #[derive(Default)]
    struct Stub {
        answers: bool,
        reveals: Vec<RevealStyle>,
        places: Vec<ViewportPlace>,
    }

    impl Stub {
        /// A window that carries the cursor's line.
        fn good() -> Self {
            Stub {
                answers: true,
                ..Stub::default()
            }
        }
        /// A window that doesn't — the stale answer that used to swallow the reveal.
        fn stale() -> Self {
            Stub::default()
        }
    }

    impl RevealTarget for Stub {
        fn reveal(&mut self, style: RevealStyle) -> bool {
            self.reveals.push(style);
            self.answers
        }
        fn place(&mut self, place: ViewportPlace) -> bool {
            self.places.push(place);
            self.answers
        }
    }

    #[test]
    fn nothing_owed_does_nothing() {
        let mut p = PendingReveal::default();
        let mut s = Stub::good();
        assert_eq!(p.settle(&mut s), Settled::Nothing);
        assert!(
            s.reveals.is_empty(),
            "no reveal attempted when none is owed"
        );
    }

    /// The regression this type exists for: a window that can't answer for the cursor must not
    /// consume the reveal. Dropping it here is what strands the viewport — the scroll never
    /// catches up, and the scroll-driven fetch then drags the window back, leaving the view
    /// frozen away from the cursor with nothing outstanding to correct it.
    #[test]
    fn a_reveal_that_could_not_be_performed_stays_owed() {
        let mut p = PendingReveal::default();
        p.owe_reveal(RevealStyle::Jump);

        let mut stale = Stub::stale();
        assert_eq!(p.settle(&mut stale), Settled::Unpaid);
        assert!(
            p.is_owed(),
            "still owed after a window that couldn't answer"
        );

        let mut good = Stub::good();
        assert_eq!(p.settle(&mut good), Settled::Paid);
        assert!(!p.is_owed(), "cleared once actually performed");
        assert_eq!(
            good.reveals,
            vec![RevealStyle::Jump],
            "and in the style owed"
        );
    }

    /// Windows that arrive for other reasons keep re-reporting `Unpaid`, so the caller keeps
    /// re-fetching rather than asking once and giving up on a reveal that is merely waiting for the
    /// right window.
    #[test]
    fn an_unpayable_debt_keeps_asking() {
        let mut p = PendingReveal::default();
        p.owe_reveal(RevealStyle::Follow);
        let mut stale = Stub::stale();
        for _ in 0..3 {
            assert_eq!(p.settle(&mut stale), Settled::Unpaid);
        }
        assert!(p.is_owed());
    }

    /// …but the answer to the chase is different: the window fetched *for the cursor* is the one
    /// that was supposed to carry it, so if it doesn't, nothing will.
    ///
    /// The regression: the shell chased on every unpaid settle, including the chase's own answer,
    /// so a cursor with no row in this view — its line belonging to a buffer the focused element
    /// doesn't window — produced fetch → reply → fetch forever, each reply re-seating the scroll.
    /// That is what a flickering viewport that refuses to scroll and shows no cursor *is*.
    #[test]
    fn a_chase_that_cannot_place_the_cursor_ends_the_debt() {
        let mut p = PendingReveal::default();
        p.owe_reveal(RevealStyle::Follow);
        let mut stale = Stub::stale();
        assert_eq!(
            p.settle_chase(&mut stale),
            Settled::Unplaceable,
            "the chase came back and still couldn't place it"
        );
        assert!(!p.is_owed(), "so the debt is dropped rather than re-chased");
        // And a later move owes a fresh one — giving up on this cursor doesn't disable reveals.
        p.owe_reveal(RevealStyle::Jump);
        let mut good = Stub::good();
        assert_eq!(p.settle(&mut good), Settled::Paid);
    }

    /// A chase that *can* be paid is an ordinary settle — the give-up path is only for failure.
    #[test]
    fn a_chase_that_lands_pays_the_debt() {
        let mut p = PendingReveal::default();
        p.owe_reveal(RevealStyle::Jump);
        let mut good = Stub::good();
        assert_eq!(p.settle_chase(&mut good), Settled::Paid);
        assert!(!p.is_owed());
        assert_eq!(good.reveals, vec![RevealStyle::Jump]);
    }

    /// A motion arriving while a reveal is outstanding replaces the style rather than queueing:
    /// the reveal happens against wherever the cursor ended up, so the newest move's style is the
    /// one that should apply. (Pre-fix, the *first* response performed the *newest* style and the
    /// rest did nothing — the same style, but against a window chosen for a different request.)
    #[test]
    fn a_later_move_replaces_the_style() {
        let mut p = PendingReveal::default();
        p.owe_reveal(RevealStyle::Jump);
        p.owe_reveal(RevealStyle::Follow);
        let mut s = Stub::good();
        assert_eq!(p.settle(&mut s), Settled::Paid);
        assert_eq!(s.reveals, vec![RevealStyle::Follow]);
    }

    /// `;` / `Alt-;` can be owed at the same time as a reveal, and each clears on its own terms.
    #[test]
    fn reveal_and_place_clear_independently() {
        let mut p = PendingReveal::default();
        p.owe_reveal(RevealStyle::Follow);
        p.owe_place(ViewportPlace::Upper);

        // A window good enough for neither.
        let mut stale = Stub::stale();
        assert_eq!(p.settle(&mut stale), Settled::Unpaid);
        assert_eq!(stale.reveals.len(), 1);
        assert_eq!(stale.places.len(), 1);

        let mut good = Stub::good();
        assert_eq!(p.settle(&mut good), Settled::Paid);
        assert!(!p.is_owed());
    }

    #[test]
    fn abandon_drops_the_debt_unperformed() {
        let mut p = PendingReveal::default();
        p.owe_reveal(RevealStyle::Jump);
        p.owe_place(ViewportPlace::Lower);
        p.abandon();
        assert!(!p.is_owed());
        let mut s = Stub::good();
        assert_eq!(p.settle(&mut s), Settled::Nothing);
        assert!(s.reveals.is_empty() && s.places.is_empty());
    }
}
