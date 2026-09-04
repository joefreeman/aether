//! The element vocabulary: what a view is made of, composed once by the server and painted the
//! same way by every client.
//!
//! This is deliberately tiny and deliberately closed. It is not a general UI toolkit — it is the
//! set of building blocks a view needs: somewhere to put buffer text, and somewhere to put the
//! *chrome* around it (generated content that isn't buffer text and has no grammar to highlight
//! it). A closed enum means every shell matches exhaustively, so a new kind can't silently render
//! as a blank row on one client.
//!
//! # One vocabulary, not two
//!
//! There used to be two. A vertical one — stack, chrome, editor — described the view; a horizontal
//! one — row, text, space, fill — described what a single chrome row contained, and the boundary
//! between them was hard: an editor could not sit inside a row. That foreclosed everything the
//! framework exists to allow (side-by-side diff, splits, a notebook cell with a margin) and meant
//! two enums, two walks and two TypeScript mirrors for what is one idea.
//!
//! So [`Element`] is both. `Stack` arranges top to bottom, `Row` left to right.
//!
//! **One thing the type allows that the renderers do not yet draw:** an [`Element::Editor`] inside a
//! `Row`. Every client lays a view out as a flat top-to-bottom list of rows, which has no way to
//! express "these two editors share these rows" — so side-by-side diff and splits need a different
//! row model, not merely a deeper walk. Until that exists, an editor is expected to be a child of a
//! `Stack`; the row builders assert it rather than silently drawing the editor as one chrome row,
//! because [`Element::walk`] *does* descend into rows and the two would then disagree about how many
//! lines the view has. The vocabulary being merged is what makes that future change possible; it is
//! not the claim that it has already happened.
//!
//! # What belongs here, and what doesn't
//!
//! **Structure** does: how many pieces a row has, what order they're in, which one absorbs the
//! slack. **Geometry and palette** don't: a shell decides what a cell is worth in pixels, what a
//! rail looks like, and which shade a role resolves to. That split is the same one the rest of the
//! protocol already draws — the server soft-wraps to a column width, the clients own everything
//! below that.
//!
//! Styling therefore travels as [`Highlight`] runs, exactly as it does for buffer text, and
//! resolves through each shell's existing theme table. No new palette, no new vocabulary.

use crate::viewport::{ChromeKind, Highlight, LogicalLineRender};
use serde::{Deserialize, Serialize};

/// Identifies one editor element within a view, for the messages that address a single one of them.
pub type FieldId = u32;

/// One node of a view's tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum Element {
    /// Children top to bottom.
    Stack { children: Vec<Element> },
    /// Children left to right, sharing one row. At most one [`Element::Fill`] child, which takes
    /// whatever width the others leave.
    Row { children: Vec<Element> },
    /// Generated presentation — a file heading, a rule, a spacer. Occupies a row and holds no
    /// cursor position. `kind` says what it *means*, which is what a shell keys its band and
    /// spacing off; `children` say what to draw, laid out left to right as a [`Element::Row`] does.
    Chrome {
        kind: ChromeKind,
        rail: RailJoin,
        children: Vec<Element>,
    },
    /// A window onto a buffer. `lines` are its rendered lines, starting at `first_buffer_line`.
    ///
    /// `buffer` and `rows` describe the element as a whole, not the window: `rows` is its **total**
    /// visual row count, of which `lines` is the slice currently loaded. Together they are what
    /// lets a client lay the view out and scroll it without asking the server — it knows how tall
    /// every element is, and which buffer to request more of when one scrolls into range.
    ///
    /// `first_buffer_line` is a line of **`buffer`**, not of the view. See [`crate::coords`].
    Editor {
        element: FieldId,
        buffer: crate::BufferId,
        rows: u32,
        first_buffer_line: u32,
        lines: Vec<LogicalLineRender>,
    },
    /// Literal text with role-styled runs over it. `highlights` are byte offsets into `text` and
    /// carry the same capture names buffer text does, so they resolve through the theme table a
    /// shell already has.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        highlights: Vec<Highlight>,
    },
    /// `cols` blank cells. Separating a rail from its heading is a layout fact, not a string of
    /// spaces someone has to remember to trim.
    Space { cols: u16 },
    /// Repeat `glyph` across whatever width the row has left. The file rule, and nothing else so
    /// far — but it is what a `Row` means by "one child absorbs the slack".
    ///
    /// Carries no role: the only colour a fill has ever needed is the one a shell already applies
    /// to the chrome kind it sits in, and inventing a capture name for it would have added a role
    /// to the syntax vocabulary — the one part of the palette with no cross-shell parity test.
    Fill { glyph: char },
}

impl Element {
    pub fn stack(children: Vec<Element>) -> Element {
        Element::Stack { children }
    }

    pub fn row(children: Vec<Element>) -> Element {
        Element::Row { children }
    }

    pub fn text(text: impl Into<String>, highlights: Vec<Highlight>) -> Element {
        Element::Text {
            text: text.into(),
            highlights,
        }
    }

    pub fn space(cols: u16) -> Element {
        Element::Space { cols }
    }

    pub fn fill(glyph: char) -> Element {
        Element::Fill { glyph }
    }

    pub fn chrome(kind: ChromeKind, rail: RailJoin, children: Vec<Element>) -> Element {
        Element::Chrome {
            kind,
            rail,
            children,
        }
    }

    /// The literal text this element draws, left to right. Spaces and fills contribute nothing:
    /// they have width but no content. For inspection — a shell paints from the tree itself.
    pub fn text_content(&self) -> String {
        let mut out = String::new();
        self.walk(&mut |e| {
            if let Element::Text { text, .. } = e {
                out.push_str(text);
            }
        });
        out
    }

    /// Every highlight run in the tree, in tree order. Offsets are relative to their own
    /// [`Element::Text`], so this is for inspection rather than painting.
    pub fn highlight_runs(&self) -> Vec<&Highlight> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let Element::Text { highlights, .. } = e {
                out.extend(highlights.iter());
            }
        });
        out
    }

    /// The leaves of one row, flattened left to right — text, spaces and fills, in painting order.
    /// A shell laying out a single row can walk this and never see the nesting.
    pub fn inline(&self) -> Vec<&Element> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if matches!(
                e,
                Element::Text { .. } | Element::Space { .. } | Element::Fill { .. }
            ) {
                out.push(e);
            }
        });
        out
    }

    /// Every editor element in order — the shells' painting and the shared row maths both walk
    /// this rather than recursing themselves.
    pub fn editors(&self) -> Vec<&Element> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if matches!(e, Element::Editor { .. }) {
                out.push(e);
            }
        });
        out
    }

    /// The view's rendered lines in order, flattened across its editors — for the paths that
    /// genuinely want every line and no structure.
    pub fn lines(&self) -> Vec<&LogicalLineRender> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let Element::Editor { lines, .. } = e {
                out.extend(lines.iter());
            }
        });
        out
    }

    fn walk<'a>(&'a self, f: &mut impl FnMut(&'a Element)) {
        f(self);
        match self {
            Element::Stack { children }
            | Element::Row { children }
            | Element::Chrome { children, .. } => {
                for child in children {
                    child.walk(f);
                }
            }
            _ => {}
        }
    }
}

/// Where a chrome row sits on the **file rail** — the vertical line tying one file's chrome
/// together so its headings read as belonging to the file rather than floating in the diff.
///
/// Structure, not presentation. All three shells ask this same question and answer it differently
/// — the terminal with box-drawing glyphs, the GUI with a pixel rule, the web with a border — and
/// until this existed all three re-derived it from a flat row list, in three different places, with
/// three chances to disagree. The server builds the file blocks, so the server knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RailJoin {
    /// The rail starts here: the first file's rule, with nothing above it to run into. A stub
    /// above this would read as a line to nowhere.
    Opens,
    /// The rail runs through: any row inside a file block, and a rule that a previous file's
    /// closing blank runs into from above.
    Tees,
    /// The rail ends here.
    Closes,
    /// No rail. The patch's opening summary and its blank belong to no file.
    Detached,
}
