//! The three vertical coordinate spaces a composed view has, and which of them the type system
//! polices.
//!
//! A view is a stack of *elements*, each a window onto some buffer's lines. That gives three
//! different things a "line number" can mean, and mixing them is not a hypothetical mistake — it is
//! the one that produced blank viewports, cursors drawn rows off from where they were painted,
//! clicks resolving to the wrong line, and two separate `index past end of Rope` panics.
//!
//! | Space | Meaning | Type |
//! | --- | --- | --- |
//! | **Buffer line** | a line of some buffer's text | plain `u32` |
//! | **View line** | an index into the concatenation of a view's element extents | [`ViewLine`] |
//! | **Visual row** | a painted screen row — wrapped rows, phantoms and chrome included | [`VisualRow`] |
//!
//! # Why only two of the three are newtypes
//!
//! Buffer lines stay a bare `u32` deliberately. They are the *default* space: a cursor, a motion, a
//! diff hunk, a diagnostic and a rendered line all live there, and `LogicalPosition::line` — 400-odd
//! source references and twice that in tests — is one. Wrapping them would be an enormous change
//! that mostly re-states what is already consistent.
//!
//! The two derived spaces are the ones that must never leak into it, so those are the ones that
//! carry a type. The rule this buys is worth stating plainly:
//!
//! > **Two bare `u32` line numbers in the same expression are both buffer lines.** Anything else is
//! > a compile error.
//!
//! Which means a view line can no longer be handed to something that indexes a rope, and a screen
//! row can no longer be added to a line count. Crossing between spaces is possible — it has to be —
//! but only by calling something named for the crossing, and for view↔buffer there is exactly one
//! such thing, server-side: `ViewLayout`, which owns the walk over a view's element extents.

use serde::{Deserialize, Serialize};

/// A line of a **view**: an index into the concatenation of its elements' extents.
///
/// Only meaningful against the view it came from. It is not a line of any buffer, and for a view of
/// several elements it usually is not even close to one — element 3's first view line might be 40
/// while the file it windows calls that same line 7. Resolve it with
/// [`crate::viewport::ViewLayout::resolve`].
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ViewLine(pub u32);

impl ViewLine {
    pub const ZERO: ViewLine = ViewLine(0);

    /// The last line of a view holding `count` of them, or line 0 for an empty view — the clamp
    /// target for a scroll position, which is why it saturates rather than returning `None`.
    pub fn last_of(count: u32) -> ViewLine {
        ViewLine(count.saturating_sub(1))
    }

    pub fn get(self) -> u32 {
        self.0
    }

    pub fn saturating_add(self, n: u32) -> ViewLine {
        ViewLine(self.0.saturating_add(n))
    }

    pub fn saturating_sub(self, n: u32) -> ViewLine {
        ViewLine(self.0.saturating_sub(n))
    }

    /// How many lines from `self` to `other`, or 0 when `other` is above. A count, so it leaves the
    /// space — which is the point: an offset into an element is a buffer-line delta.
    pub fn distance_to(self, other: ViewLine) -> u32 {
        other.0.saturating_sub(self.0)
    }
}

/// Prints as the bare number. These name positions, not units, so a message reading `12..40` is
/// what a reader wants; the type is there to stop them being *mixed*, not to decorate output.
impl std::fmt::Display for ViewLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A painted **screen row**.
///
/// Distinct from a line because a line is not a row: soft wrap turns one line into several, the
/// inline diff's phantom baseline rows and a patch's chrome occupy rows while belonging to no line
/// at all. Summing lines where rows were meant is what silently shortened the scrollbar and put a
/// view's last lines out of reach.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct VisualRow(pub u32);

impl VisualRow {
    pub const ZERO: VisualRow = VisualRow(0);

    pub fn get(self) -> u32 {
        self.0
    }

    pub fn saturating_add(self, n: u32) -> VisualRow {
        VisualRow(self.0.saturating_add(n))
    }

    pub fn saturating_sub(self, n: u32) -> VisualRow {
        VisualRow(self.0.saturating_sub(n))
    }

    /// How many rows from `self` down to `other`, or 0 when `other` is above.
    pub fn distance_to(self, other: VisualRow) -> u32 {
        other.0.saturating_sub(self.0)
    }
}

/// See [`ViewLine`]'s `Display`.
impl std::fmt::Display for VisualRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both spaces ride the wire as bare numbers. They are newtypes to stop them being *confused*,
    /// not to change the protocol — a transparent representation is what keeps this an internal
    /// discipline that costs clients nothing.
    #[test]
    fn both_spaces_are_transparent_on_the_wire() {
        assert_eq!(serde_json::to_string(&ViewLine(7)).unwrap(), "7");
        assert_eq!(serde_json::to_string(&VisualRow(7)).unwrap(), "7");
        assert_eq!(serde_json::from_str::<ViewLine>("7").unwrap(), ViewLine(7));
        assert_eq!(
            serde_json::from_str::<VisualRow>("7").unwrap(),
            VisualRow(7)
        );
    }

    /// The clamp target for a scroll position. An empty view has no last line, and answering `None`
    /// would push the saturation out to every caller.
    #[test]
    fn the_last_line_of_an_empty_view_is_line_zero() {
        assert_eq!(ViewLine::last_of(0), ViewLine(0));
        assert_eq!(ViewLine::last_of(1), ViewLine(0));
        assert_eq!(ViewLine::last_of(9), ViewLine(8));
    }
}
