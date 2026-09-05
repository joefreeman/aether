//! The vertical coordinate spaces a composed view has, and which of them the type system polices.
//!
//! A view is a stack of *elements*, each a window onto some buffer's lines, with chrome between
//! them. That gives three different things a "row number" or "line number" can mean, and mixing
//! them is not a hypothetical mistake — it is the one that produced blank viewports, cursors drawn
//! rows off from where they were painted, clicks resolving to the wrong line, and two separate
//! `index past end of Rope` panics.
//!
//! | Space | Meaning | Type |
//! | --- | --- | --- |
//! | **Buffer line** | a line of some buffer's text | plain `u32` |
//! | **Element row** | a painted row **within one element** — its lines' wrapped rows and phantom rows, counted from the element's first line | [`ElementRow`] |
//! | **Visual row** | a painted screen row of the **whole view**, chrome and every element included: the client's scroll coordinate | [`VisualRow`] |
//!
//! There used to be a fourth, the *view line*: an index into the concatenation of a view's element
//! extents. It existed because the server owned the scroll position and needed one number for
//! "where the view is scrolled to", and every crossing between it and a buffer line was a chance to
//! get it wrong. The client owns the scroll now — it is the only side that knows every element's
//! height, since prose elements are laid out there — and content is fetched per element by row
//! within it, so the space is gone.
//!
//! # Why buffer lines stay bare
//!
//! They are the *default* space: a cursor, a motion, a diff hunk, a diagnostic and a rendered line
//! all live there, and `LogicalPosition::line` — 400-odd source references and twice that in
//! tests — is one. The rule this buys is worth stating plainly:
//!
//! > **Two bare `u32` line numbers in the same expression are both buffer lines.** Anything else is
//! > a compile error.
//!
//! An element row crosses the wire (a request names one, a loaded slice reports one) and carries a
//! type so it cannot be added to a line. A visual row never crosses the wire at all: the client
//! computes it from the tree and its own measurements, and only the client scrolls by it.

use serde::{Deserialize, Serialize};

/// A painted row **within one element**, counted from the top of the element's own content: its
/// lines' wrapped rows and phantom rows, chrome excluded (chrome is a sibling of the element, not
/// part of it).
///
/// Meaningful to both sides. The server derives it from its own wrapping, which is why a client
/// asks for content by it — "this element, from this row" — rather than by a line it cannot know
/// the row of. The client places a loaded slice at the element's start plus the slice's first row.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ElementRow(pub u32);

impl ElementRow {
    pub const ZERO: ElementRow = ElementRow(0);

    pub fn get(self) -> u32 {
        self.0
    }

    pub fn saturating_add(self, n: u32) -> ElementRow {
        ElementRow(self.0.saturating_add(n))
    }

    pub fn saturating_sub(self, n: u32) -> ElementRow {
        ElementRow(self.0.saturating_sub(n))
    }
}

/// Prints as the bare number. These name positions, not units, so a message reading `12..40` is
/// what a reader wants; the type is there to stop them being *mixed*, not to decorate output.
impl std::fmt::Display for ElementRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A painted **screen row** of the whole view — the client's scroll coordinate — at the shell's
/// resolution: a terminal counts whole rows, a pixel shell thousandths of one (the client's
/// `grid::Measured::units_per_row`). The server never sees one.
///
/// Distinct from a line because a line is not a row: soft wrap turns one line into several, the
/// inline diff's phantom baseline rows and a patch's chrome occupy rows while belonging to no line
/// at all. Summing lines where rows were meant is what silently shortened the scrollbar and put a
/// view's last lines out of reach. Distinct from an [`ElementRow`] because it counts from the top
/// of the view, chrome and every element above included — a number only the client can compute.
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

/// See [`ElementRow`]'s `Display`.
impl std::fmt::Display for VisualRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The element row rides the wire as a bare number. It is a newtype to stop it being
    /// *confused* with a line, not to change the protocol — a transparent representation is what
    /// keeps this an internal discipline that costs clients nothing.
    #[test]
    fn an_element_row_is_transparent_on_the_wire() {
        assert_eq!(serde_json::to_string(&ElementRow(7)).unwrap(), "7");
        assert_eq!(
            serde_json::from_str::<ElementRow>("7").unwrap(),
            ElementRow(7)
        );
    }
}
