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

use crate::viewport::{Highlight, LogicalLineRender};
use serde::{Deserialize, Serialize};

/// Identifies one editor element within a view, for the messages that address a single one of them.
pub type FieldId = u32;

/// One node of a view's tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum Element {
    /// Children top to bottom. Was `Stack`, which named the same thing without pairing with
    /// [`Element::Row`] — there is no third arrangement for it to have meant.
    Column {
        #[serde(default, skip_serializing_if = "Edges::is_zero")]
        edges: Edges,
        #[serde(default, skip_serializing_if = "Band::is_none")]
        band: Band,
        /// What the box's **top border** says, drawn on the border row itself: a rule cell after
        /// the corner, a space, these nodes, a space, then the rule on to the far corner.
        ///
        /// Inline nodes — the kinds a [`Element::Row`] holds — so a title is styled by the same
        /// [`Highlight`] runs everything else is, through the theme table a shell already has.
        /// It costs the box **no rows**: it rides the border row the box was already spending, and
        /// [`crate::ui`]'s row walk therefore says nothing about it — each painter reads it off
        /// the top edge row's owner. A title needs somewhere to sit, so `edges.border.top` must be
        /// at least 1; see [`Element::titled`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        title: Vec<Element>,
        children: Vec<Element>,
    },
    /// Children left to right, sharing one row. At most one [`Element::Fill`] child, which takes
    /// whatever width the others leave.
    Row {
        #[serde(default, skip_serializing_if = "Edges::is_zero")]
        edges: Edges,
        #[serde(default, skip_serializing_if = "Band::is_none")]
        band: Band,
        children: Vec<Element>,
    },
    /// A window onto a buffer. `lines` are its rendered lines, starting at `first_buffer_line`.
    ///
    /// `buffer` and `rows` describe the element as a whole, not the window: `rows` is its **total**
    /// row count, of which `lines` is the slice currently loaded, and `first_row` is where that
    /// slice starts within the element — the rows of its lines above `first_buffer_line`, wrapped
    /// rows and phantoms alike. Together they are what lets a client lay the view out and scroll it
    /// without asking the server: it knows how tall every element is, where each loaded slice sits
    /// inside its element, and which element to request more of when one scrolls into range.
    ///
    /// `first_buffer_line` is a line of **`buffer`**, not of the view. See [`crate::coords`].
    Editor {
        element: FieldId,
        buffer: crate::BufferId,
        rows: u32,
        first_row: crate::coords::ElementRow,
        first_buffer_line: u32,
        lines: Vec<LogicalLineRender>,
        /// Whose arithmetic `rows` is — see [`LayoutOwner`]. Off the wire for the ordinary case.
        #[serde(default, skip_serializing_if = "LayoutOwner::is_server")]
        laid_out_by: LayoutOwner,
        /// What this element is *for* — see [`ElementRole`]. Off the wire for the ordinary case.
        #[serde(default, skip_serializing_if = "ElementRole::is_field")]
        role: ElementRole,
    },
    /// **Rendered prose** — a span of a buffer as markdown, not as lines.
    ///
    /// The vocabulary's answer to "this is a document to read", as [`Element::Editor`] is its
    /// answer to "this is text to edit". A shell renders it with real typography: headings that
    /// are headings, lists, quotes, code panels. What that costs each shell is one renderer,
    /// written once — which is the point. Before this existed the reading view had to be a whole
    /// *view kind* with a painter of its own in every shell, and prose anywhere else (an agent's
    /// reply inside a conversation) had nowhere to live but an `Editor` element pretending.
    ///
    /// `blocks` is the **tree**, not a flattened list: a list item holds blocks, a quote holds
    /// blocks, a table cell holds inlines. Rendering recurses, and so must anything that walks it.
    ///
    /// Its height is the shell's to report ([`LayoutOwner::Client`] is implied — proportional type
    /// cannot be measured anywhere else), through the same `Measured` table an editor element the
    /// client lays out uses.
    ///
    /// **The parse, and the shape of the text it came from — no source and no wire rows.** A prose
    /// element carries its own content, so nothing fetches it and no row addresses it. What
    /// `source` adds is the one thing a parse loses: the reading view *is* addressed by position,
    /// because the server owns the cursor and reports it as a line and a byte column, and
    /// resolving that against a block means knowing where each line starts. The line table is what
    /// a client wanted the source for; the source itself stays where the buffer is.
    ///
    /// Still no buffer id: the one path that resolves an element to a buffer — a pointer press —
    /// walks the editors, and which of a block's inlines a *pixel* is in is unanswered, so it is
    /// not guessed at here in the meantime.
    Prose {
        element: FieldId,
        blocks: Vec<aether_markdown::Block>,
        source: SourceLines,
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

/// Where a prose element's lines begin, and how long its text is — the shape of the text a parse
/// came from, without the text.
///
/// A reading view is addressed by **position**: the server owns the cursor and reports it as a
/// line and a byte column, while everything resolved against it client-side — which block the
/// cursor is in, which line a rendered block starts at, where a measured block's lines sit — is a
/// byte offset into the same text. Converting between the two is all a client ever wanted the
/// source for, so this is what it is sent instead: the two numbers that conversion needs.
///
/// Offsets are into the **element's own text**, exactly as the spans in `blocks` are, so both
/// sides of any comparison speak one set of coordinates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLines {
    /// The byte offset each line starts at, ascending, one entry per line. Always begins with 0:
    /// text with no newline in it is one line, and empty text is still one line.
    pub starts: Vec<u32>,
    /// The text's length in bytes — where the last line ends, and what a byte offset is clamped
    /// against. Derivable from neither `starts` (which says where the last line begins, not where
    /// it ends) nor the parse (trailing blank lines belong to no block).
    pub byte_len: u32,
}

impl SourceLines {
    /// The table for a piece of text. One definition of where a line begins, used by the server
    /// that sends it and by every test that fakes one — the alternative is two rules that agree
    /// until a document ends without a newline.
    ///
    /// Counts lines as a rope does: a trailing newline opens a final empty line, so `"a\nb\n"` is
    /// three lines. That is the count the cursor's line numbers are in.
    pub fn of(text: &str) -> Self {
        SourceLines {
            starts: std::iter::once(0)
                .chain(
                    text.char_indices()
                        .filter_map(|(i, c)| (c == '\n').then_some(i as u32 + 1)),
                )
                .collect(),
            byte_len: text.len() as u32,
        }
    }

    /// How many lines the text has — the length of the table, by construction.
    pub fn line_count(&self) -> u32 {
        self.starts.len() as u32
    }
}

/// Who lays an editor element's lines out — whose arithmetic its height is.
///
/// The server wraps monospace text and knows exactly how many rows that made; that is every
/// ordinary editor, and the tree carries its height. An editor the *client* wraps has no height
/// the server could know, so its lines go out unwrapped — one row per line on the wire, `rows` the
/// line count — and the shell measures the rest. The grid reads the tree for the first and the
/// shell's measurements for the second, and nothing else about scrolling changes between them.
///
/// Rendered markdown is not one of these: it is [`Element::Prose`], which carries no lines at all
/// and is always the shell's to measure. The reading view was the one client-laid-out editor there
/// was — a whole document sent as unwrapped lines for the client to parse — until it became prose
/// like any other.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutOwner {
    /// Wrapped by the server to the viewport's columns; `rows` counts the result.
    #[default]
    Server,
    /// Laid out by the client; a row on the wire is a line, and the true height is the client's
    /// (`aether_client::grid::Measured`).
    Client,
}

impl LayoutOwner {
    pub fn is_server(&self) -> bool {
        matches!(self, LayoutOwner::Server)
    }
}

/// What an editor element is *for*, where a view has more than one kind of them.
///
/// Every element of every view so far has been a field of content: a file, a hunk, a slice of a
/// generated document. A shell view has one that is not — the line you type the next command into
/// — and a handful of keys mean something different there (`Enter` submits; `Alt-Enter` is the
/// newline; `Up`/`Down` recall history on a single line). The client has to know *which* element
/// that is, and this is how the window says so.
///
/// **A role, not a view kind.** The client derives "this view is a shell" from the presence of an
/// input element rather than from a tag on the view, which keeps the shells kind-blind: they
/// already paint an editor element, and this only says which of them holds the caret's special
/// meanings. A view that later grows a second kind of field says so here too, in one place, rather
/// than by each client re-deriving it from the shape of the tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementRole {
    /// Content. Every element of an ordinary or composed view.
    #[default]
    Field,
    /// The line a shell's next command is typed into — the view's last element.
    Input,
}

impl ElementRole {
    pub fn is_field(&self) -> bool {
        matches!(self, ElementRole::Field)
    }

    pub fn is_input(&self) -> bool {
        matches!(self, ElementRole::Input)
    }
}

/// The two views a client can ask for over one file: its source in the editor, or the document
/// its source describes, laid out by the client and read at block grain.
///
/// What an open *asks for* (`view/open { kind }`), and what a view then is for its whole life:
/// a file's editor and its reader are two views, each with its own scroll, sharing the cursor.
/// A driver's view — a patch — is neither and ignores the request. Which kind a view is shows in
/// its window as the element's [`LayoutOwner`]. A client that has no opinion sends nothing and
/// gets the file's most recently used view, or the app setting's kind for a file with none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewKind {
    Editor,
    Reader,
}

impl Element {
    pub fn column(children: Vec<Element>) -> Element {
        Element::Column {
            edges: Edges::NONE,
            band: Band::None,
            title: Vec::new(),
            children,
        }
    }

    pub fn row(children: Vec<Element>) -> Element {
        Element::Row {
            edges: Edges::NONE,
            band: Band::None,
            children,
        }
    }

    /// A column that draws a box: `edges` around `children`, with `band` behind the cells the
    /// border and padding occupy.
    pub fn framed(edges: Edges, band: Band, children: Vec<Element>) -> Element {
        Element::Column {
            edges,
            band,
            title: Vec::new(),
            children,
        }
    }

    /// [`Element::framed`] with a name on its top border — see the `title` field.
    ///
    /// The title is drawn *on* the top border row, so there has to be one: a box with no top
    /// border has nowhere to put it and would silently drop it.
    pub fn titled(
        edges: Edges,
        band: Band,
        title: Vec<Element>,
        children: Vec<Element>,
    ) -> Element {
        debug_assert!(
            title.is_empty() || edges.border.top >= 1,
            "a title is drawn on the top border row; this box has no top border"
        );
        Element::Column {
            edges,
            band,
            title,
            children,
        }
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

    /// A row of generated presentation — a file heading, a spacer — on the chrome band.
    ///
    /// Was `Element::Chrome`, a variant of its own carrying a `kind` no shell branched on.
    /// What it actually delivered was the band, so that is what it says now; "holds no cursor
    /// position" needs no tag, because a row is cursor-bearing iff it came from an
    /// [`Element::Editor`]'s lines.
    pub fn chrome(children: Vec<Element>) -> Element {
        Element::Row {
            edges: Edges::NONE,
            band: Band::Chrome,
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

    /// What this container's top border says, if anything — empty for every node that is not a
    /// titled [`Element::Column`].
    ///
    /// Deliberately **not** part of [`Element::walk`]: a title is not a row and stands for none, so
    /// nothing that counts rows, lines or editors should find it. The painters read it here, off
    /// the owner an edge row already names.
    pub fn title(&self) -> &[Element] {
        match self {
            Element::Column { title, .. } => title,
            _ => &[],
        }
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

    /// Every **content** element: the editors and the prose, in view order.
    ///
    /// What anything asking "which elements does this view have, and where do they sit" wants —
    /// measurement, focus order, the row walk. Distinct from [`Self::editors`], whose callers want
    /// an editor's *fields* (lines, rows, the buffer slice) and would have to skip prose anyway.
    pub fn content(&self) -> Vec<&Element> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if matches!(e, Element::Editor { .. } | Element::Prose { .. }) {
                out.push(e);
            }
        });
        out
    }

    /// The field id of a content element — an editor's or prose's.
    pub fn field_id(&self) -> Option<FieldId> {
        match self {
            Element::Editor { element, .. } | Element::Prose { element, .. } => Some(*element),
            _ => None,
        }
    }

    /// The [`ElementRole::Input`] element of this tree, if it has one — which is also the answer
    /// to "is this view a shell?".
    ///
    /// Derived from the tree rather than carried as a view kind, so a client never branches on
    /// what a view *is*: it asks whether the thing in front of it has an input, and the keys that
    /// belong to one follow from that.
    pub fn input_element(&self) -> Option<FieldId> {
        self.editors().into_iter().find_map(|e| match e {
            Element::Editor { element, role, .. } if role.is_input() => Some(*element),
            _ => None,
        })
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
            Element::Column { children, .. } | Element::Row { children, .. } => {
                for child in children {
                    child.walk(f);
                }
            }
            _ => {}
        }
    }
}

/// The cells a container spends on itself: a border, and padding inside it.
///
/// **Structure, not geometry.** How many cells the box costs changes how many its content gets, and
/// the server must wrap to that — so it belongs here, exactly as [`Element::Space`]'s `cols` does.
/// What a border *looks like* is the shell's: the terminal draws box-drawing glyphs, the GUI a
/// hairline, the web a CSS border.
///
/// Counted in **cells**, not pixels or fractions. A terminal border is one column or one row; a
/// pixel shell draws its hairline inside the cell it is given, which is what `aether-iced` already
/// does for the file rail. Prose the client lays out is measured in ems rather than cells, so a
/// reader that grows frames will need the horizontal analogue of `grid::Measured::units_per_row` —
/// a widening of how a shell *reads* these, not a change to what is sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edges {
    #[serde(default, skip_serializing_if = "Sides::is_zero")]
    pub border: Sides,
    #[serde(default, skip_serializing_if = "Sides::is_zero")]
    pub padding: Sides,
    /// Whether adjacent bordered children of this container **share** an edge rather than each
    /// drawing its own — `border-collapse`, and the reason a run of file blocks reads as one
    /// ruled list rather than as a stack of separate boxes.
    ///
    /// On the container, not the children: it is a fact about how two siblings meet, and only
    /// their parent can see both. What the shared edge is *drawn* as follows from it — a join with
    /// something above and below is a tee, one with nothing above is a corner — which is the
    /// question [`RailJoin`] answers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collapse: bool,
}

impl Edges {
    pub const NONE: Edges = Edges {
        border: Sides::ZERO,
        padding: Sides::ZERO,
        collapse: false,
    };

    /// Nothing on any side, and nothing to collapse — the ordinary case, kept off the wire.
    ///
    /// Deliberately includes `collapse`: a value that serialises to nothing must deserialise back
    /// to itself, and `collapse: true` with no borders would not.
    pub fn is_zero(&self) -> bool {
        *self == Edges::NONE
    }

    /// Cells lost to the left and right — what an element's wrap width is reduced by.
    pub fn horizontal(&self) -> u16 {
        self.border.left + self.border.right + self.padding.left + self.padding.right
    }

    /// Cells the left side spends, which is where the content starts.
    pub fn left(&self) -> u16 {
        self.border.left + self.padding.left
    }

    /// Rows the top spends, and the bottom — border and padding alike occupy whole rows.
    pub fn top(&self) -> u16 {
        self.border.top + self.padding.top
    }

    pub fn bottom(&self) -> u16 {
        self.border.bottom + self.padding.bottom
    }
}

/// Cells per side, in the order a stylesheet names them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sides {
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub top: u16,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub right: u16,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub bottom: u16,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub left: u16,
}

fn is_zero_u16(n: &u16) -> bool {
    *n == 0
}

impl Sides {
    pub const ZERO: Sides = Sides {
        top: 0,
        right: 0,
        bottom: 0,
        left: 0,
    };

    pub fn is_zero(&self) -> bool {
        *self == Sides::ZERO
    }

    /// The same count on every side.
    pub fn all(n: u16) -> Sides {
        Sides {
            top: n,
            right: n,
            bottom: n,
            left: n,
        }
    }
}

/// The fill a container paints behind itself — behind its border and padding cells, with children
/// painting over their own content area, as a box model has it.
///
/// A **closed** enum rather than a colour or an open role name, for the reason the whole vocabulary
/// is closed (see this module's header): every shell matches exhaustively, so a new band cannot
/// render as nothing on one client. A colour would be palette, which does not travel here; an open
/// role string would land in the syntax vocabulary, the one part of the palette with no cross-shell
/// parity test — the same reason [`Element::Fill`] declines to carry a role.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    /// Paints nothing of its own; whatever is behind it shows through.
    #[default]
    None,
    /// Generated presentation — the shade a patch's separators and the space around its hunks sit
    /// on. Rows on it hold no cursor position, which is what the band exists to make legible.
    Chrome,
}

impl Band {
    pub fn is_none(&self) -> bool {
        matches!(self, Band::None)
    }
}

/// How a box's horizontal border meets the rails down its sides — `┌`, `├`, `└`, or a bare rule
/// with no rail to meet.
///
/// Structure, not presentation. All three shells ask this same question and answer it differently
/// — the terminal with box-drawing glyphs, the GUI with a pixel rule, the web with a background
/// gradient — and before this existed all three re-derived it from a flat row list, in three
/// different places, with three chances to disagree.
///
/// **Not on the wire.** It rode on `Chrome` while a file block's boundary was a chrome row; it is a
/// box's own border now, so `grid::resolve_joins` reads it off the tree — one derivation, in the
/// shared core, that cannot disagree with the structure it describes.
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
