//! Viewport messages.

use crate::coords::ElementRow;
use crate::cursor::CursorState;
use crate::envelope::{NotificationMethod, RpcMethod};
use crate::git::GitBufferStatus;
use crate::lsp::{DiagnosticCounts, LspServerStatus, SymbolCrumb};
use crate::search::SearchMatchRange;
use crate::sneak::SneakTarget;
// The element vocabulary lives in `ui`; re-exported here because a view's tree is what
// `viewport` messages carry, and that is where callers look for it.
pub use crate::ui::{Element, FieldId, SourceLines};
use crate::{Revision, ViewportId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WrapMode {
    Soft,
    None,
}

/// Where a view is scrolled to, as **content**: an element, a line of that element's buffer, and
/// how far into that line's block of rows.
///
/// Names a place the way a bookmark does, so it survives a wrap toggle, a resize and a restore — a
/// visual row would not. It used to be a *view line*, an index into the concatenation of the
/// elements' extents: a coordinate no client could compute and no user ever saw, and the one every
/// blank-viewport bug in this area was a confusion with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ScrollPosition {
    pub element: FieldId,
    /// A line of the element's **buffer**.
    pub line: u32,
    /// Rows into the line's block — chrome above it, phantoms, then its own wrapped rows.
    /// Fractional for a pixel-scrolling client.
    pub sub_row: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogicalLineRender {
    pub logical_line: u32,
    pub visual_rows: Vec<WrappedRow>,
    /// Per-line byte ranges where the current server-side search query matches. Empty when no
    /// search is active on this buffer for this client. Multi-line matches contribute one entry
    /// to each line they touch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search_matches: Vec<SearchMatchRange>,
    /// Baseline lines this one removed or replaced, drawn above it while the inline diff view is
    /// on. They occupy screen rows and hold no cursor position — which is the whole point, since
    /// making some *buffer* lines unaddressable would need a skip rule in every motion, in search
    /// landing, in sneak, and in jumplist and nav restore.
    ///
    /// A generated patch's chrome used to share this field. It is a banded [`ui::Element::Row`] now:
    /// a separator belongs between two hunks, not to the line beneath it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub baseline_above: Vec<BaselineRow>,
    /// This line's change-state: diffed against a baseline, conflicted, or a side of a generated
    /// patch. See [`LineChange`] — the three are mutually exclusive by construction, which is why
    /// they are one field.
    #[serde(default, skip_serializing_if = "LineChange::is_none")]
    pub change: LineChange,
    /// Language-server diagnostics intersecting this logical line, as byte ranges within the line
    /// (already converted from the server's LSP position encoding). A diagnostic spanning multiple
    /// lines contributes one entry — carrying the full message — to each line it touches, so the
    /// client can underline the span and show the message wherever the cursor lands. Empty when no
    /// diagnostics apply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<DiagnosticSpan>,
    /// Active sneak (`s`/`S`) word-jump targets on this logical line: matched word-starts as byte
    /// ranges, each optionally carrying the label char painted over its first cell. Empty when no
    /// sneak session is active for this client. See [`crate::sneak`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sneak_targets: Vec<SneakTarget>,
}

/// What a line's own change-state is: changed against a baseline, part of a conflict, or one side
/// of a generated patch.
///
/// One field rather than five, because the five were never independent. A conflicted file *is*
/// diffed, but its conflict blocks are masked out of that diff (`git::mask_conflicts`), so a line
/// never carries both. A generated patch has no baseline of its own, so it never carries a marker.
/// Those two rules used to be paragraphs of prose above fields that **two different producers wrote
/// to** — the baseline diff and the patch generator both set `diff_stage` and `diff_emphasis`,
/// each assuming the other hadn't.
///
/// Deliberately *not* a flat list of decorations: the three independent overlays a line can also
/// carry — search matches, diagnostics, sneak targets — genuinely coexist and have no precedence
/// question between them, and pooling them would only cost their payloads their types.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LineChange {
    /// Unchanged, outside a repo, or a generated patch's context line.
    #[default]
    None,
    /// Changed against the buffer's baseline.
    Changed {
        marker: DiffMarker,
        stage: DiffStage,
        /// Byte ranges the change actually touched, for the stronger intra-line tint. Populated
        /// only while the viewport's inline diff view is on — unlike `marker`, which is ungated so
        /// the gutter change-bar is right whether or not the view is up. Empty when the whole line
        /// changed too much to pick sub-ranges out of.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        emphasis: Vec<EmphasisRange>,
    },
    /// Part of a merge conflict left by a stopped merge or rebase.
    ///
    /// Never gated on the diff view, for a reason the marker isn't: a conflicted file cannot be
    /// read correctly without knowing which side is which, and the markers delimiting the sides are
    /// ordinary buffer text with nothing else to distinguish them.
    Conflict { side: ConflictLine },
    /// One side of a generated patch. Both sides are ordinary buffer text here — unlike the inline
    /// diff view, where the old side is a phantom row the cursor can't reach.
    Patch {
        side: PatchLine,
        stage: DiffStage,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        emphasis: Vec<EmphasisRange>,
    },
}

impl LineChange {
    /// Whether this line has no change-state at all — the overwhelmingly common case, and what
    /// keeps it off the wire.
    pub fn is_none(&self) -> bool {
        matches!(self, LineChange::None)
    }

    /// The gutter change-bar marker, if this line has one.
    pub fn marker(&self) -> Option<DiffMarker> {
        match self {
            LineChange::Changed { marker, .. } => Some(*marker),
            _ => None,
        }
    }

    /// Which side of a generated patch this line is; `None` on context lines and ordinary buffers.
    pub fn patch_side(&self) -> Option<PatchLine> {
        match self {
            LineChange::Patch { side, .. } => Some(*side),
            _ => None,
        }
    }

    /// Which part of a conflict block this line is; `None` everywhere else.
    pub fn conflict(&self) -> Option<ConflictLine> {
        match self {
            LineChange::Conflict { side } => Some(*side),
            _ => None,
        }
    }

    /// Which layer the change sits in. `Unstaged` where the question doesn't arise, which is also
    /// what a stage-unaware renderer degrades to.
    pub fn stage(&self) -> DiffStage {
        match self {
            LineChange::Changed { stage, .. } | LineChange::Patch { stage, .. } => *stage,
            _ => DiffStage::Unstaged,
        }
    }

    /// Intra-line emphasis ranges; empty where there are none.
    pub fn emphasis(&self) -> &[EmphasisRange] {
        match self {
            LineChange::Changed { emphasis, .. } | LineChange::Patch { emphasis, .. } => emphasis,
            _ => &[],
        }
    }
}

/// One diagnostic's footprint on a single logical line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticSpan {
    /// Byte offset within the logical line where the underline starts.
    pub start: u32,
    /// Byte offset within the logical line where the underline ends (exclusive). For a zero-width
    /// diagnostic (`start == end`) the client underlines one cell so it's visible.
    pub end: u32,
    pub severity: DiagnosticSeverity,
    /// The full diagnostic message (repeated on each line the diagnostic covers).
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffMarker {
    Added,
    Modified,
    /// Lines were removed immediately above this one (a pure deletion). The line itself is
    /// unchanged — only the gutter flags it; it carries no background tint.
    Deleted,
}

/// Which side of a generated patch a line is — see [`LineChange::Patch`].
///
/// Deliberately separate from [`DiffMarker`], which decorates a *file* against its baseline: there
/// a removal is a phantom row with no cursor position, and `Deleted` flags the surviving line
/// underneath. In a patch both sides are ordinary buffer text the cursor can land on, so the two
/// carry different meanings and only ever share their theme colours. They never appear on the same
/// buffer — a patch has no baseline of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchLine {
    /// A `+` line: present in the new side only.
    Added,
    /// A `-` line: present in the old side only.
    Removed,
}

/// Which part of a conflict block a line belongs to — see [`LineChange::Conflict`].
///
/// The four marker lines are one variant rather than four: they are scenery, styled the same and
/// deleted by every resolution, and telling `<<<<<<<` from `=======` is what the line's own text is
/// for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictLine {
    /// A `<<<<<<<`, `|||||||`, `=======` or `>>>>>>>` line.
    Marker,
    /// Our side: what the branch being merged *into* has (mid-rebase, confusingly, the upstream —
    /// git's labels on the marker lines are the authority, which is why they travel too).
    Ours,
    /// The common ancestor, under `merge.conflictstyle = diff3` / `zdiff3`. Context only.
    Base,
    /// Their side: what is being merged in.
    Theirs,
}

/// Which side of the index a change sits on, in the combined staged+unstaged view. Tags both
/// per-line changes ([`LineChange`]) and phantom rows ([`BaselineRow`]).
/// `Unstaged` is the default and is omitted from the wire, so a stage-unaware renderer degrades
/// to a single-colour look. Deliberately binary: where the two layers overlap, the unstaged top
/// layer wins — bright means "`Space g s` will stage this", dim means "staged; `Space g u` pulls it
/// back out".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStage {
    /// The buffer's content here differs from the index (`B ≠ I`).
    #[default]
    Unstaged,
    /// Staged and untouched since (`B == I ≠ HEAD`).
    Staged,
}

impl DiffStage {
    pub fn is_unstaged(&self) -> bool {
        matches!(self, DiffStage::Unstaged)
    }
}

/// One intra-line diff emphasis range: byte offsets (within the logical line's or virtual row's
/// text) of a sub-line region the change actually touched. Word-grain, non-overlapping, sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmphasisRange {
    pub start: u32,
    /// Exclusive.
    pub end: u32,
}

/// A baseline line the working buffer removed or replaced, drawn above the surviving line while
/// the inline diff view is on. Occupies a screen row but holds no cursor position.
///
/// Anchored to a line rather than placed in the view's tree, which is the difference between this
/// and a patch's chrome: a phantom deletion belongs *above line N* of a particular buffer, whereas
/// a file separator belongs between two hunks. Chrome is a banded [`ui::Element::Row`]; this is
/// not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineRow {
    pub text: String,
    /// Staged (the text is HEAD's, already replaced in the index) vs unstaged (the text is the
    /// index's, still present there). At most one layer per anchor: where both would stack the
    /// server sends only the unstaged rows, so what shows as deleted is exactly what a revert
    /// would restore.
    #[serde(default, skip_serializing_if = "DiffStage::is_unstaged")]
    pub stage: DiffStage,
    /// Byte ranges of this removed line that its paired buffer line replaced — the old-side
    /// counterpart of the new side's [`LineChange::Changed::emphasis`]. Empty for whole-line
    /// changes and pure deletions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emphasis: Vec<EmphasisRange>,
}

/// The **content** of one row a logical line wrapped into: where it starts in the line, how far it
/// is indented, and its styled text.
///
/// Was `VisualRow`, which now names a *position* in the visual-row space ([`crate::coords`]). The
/// two are different kinds of thing — one is what to draw, the other is where — and having them
/// share a name was how the renaming started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WrappedRow {
    /// Byte offset within the *logical line* where this row's text starts. For the first row
    /// of a logical line this is always 0; for continuation rows it's the byte right after the
    /// preceding row's break point. Used by the client to map a cursor's logical column to the
    /// visual row + column it should render on.
    pub byte_offset: u32,
    pub continuation_indent: u32,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    pub highlights: Vec<Highlight>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Highlight {
    /// Byte offset within the containing `Segment::text`.
    pub start: u32,
    pub end: u32,
    /// Tree-sitter highlight name (e.g. `"keyword"`, `"string"`, `"comment"`).
    pub kind: String,
}

/// What a viewport shows of its view: the whole tree, with the loaded slices inside it, and the
/// view-level facts the status bar needs.
///
/// No scroll geometry. The tree carries every element's total height and every loaded slice's row
/// within its element, and the client lays the view out from that — it is the only side that can,
/// since a prose element's height is measured there. So the view's total height, where a slice
/// sits on screen and how far the view may scroll are all the client's arithmetic over the tree,
/// not numbers the server sends.
#[derive(Debug, Serialize, Deserialize)]
pub struct Window {
    /// Display width (in cols) of the widest line among the view's buffers, for sizing a native
    /// horizontal scroll container under `WrapMode::None`. `0` under soft wrap (content always
    /// fits `cols`).
    pub max_line_width: u32,
    /// Buffer-level Git status (branch + staged/unstaged counts) for the status bar. `None` outside
    /// a repo. Rides the window so it updates live on edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_status: Option<GitBufferStatus>,
    /// Whether any buffer this view windows **other than the focused element's** has unsaved
    /// changes. Always `false` for an ordinary view, which has nothing else.
    ///
    /// Deliberately excludes the focused element, and that is what makes it correct between
    /// renders. The client already knows the focused buffer's dirtiness first-hand and instantly
    /// (it holds `revision` and `saved_revision`), so the status dot is
    /// `focused_is_dirty || other_elements_dirty` — typing shows immediately, and saving clears it
    /// immediately, neither waiting for a re-render. An *inclusive* flag would go stale on exactly
    /// that save: a save pushes `buffer/state`, not a new window, so a window computed before it
    /// would still be claiming the view was dirty.
    ///
    /// Edits can only land in the focused element (the cursor is there), so the remaining staleness
    /// is another client editing one of this view's other files — which no channel corrects today
    /// either.
    #[serde(default)]
    pub other_elements_dirty: bool,
    /// What the view is composed of. A single [`Element::Editor`] for an ordinary buffer; chrome
    /// and hunks interleaved for a generated patch. Use [`Element::lines`] where the structure is
    /// irrelevant and every rendered line is what's wanted.
    pub root: Element,
}

// ---- view/save ----------------------------------------------------------------------------------

/// Save every document the view's elements window.
///
/// **Not `buffer/save` with more arguments.** One saves a named document; this saves a *set* the
/// caller cannot enumerate — the elements of a composed view are the server's business, and a
/// working-changes view routinely windows a dozen files at once. `Space s` saving only whichever
/// element happened to hold the cursor was the bug: the other files stayed dirty with nothing on
/// screen saying so beyond the view-wide dot.
///
/// An ordinary view windows exactly one document, so this is `buffer/save` for it and no client
/// needs to ask which kind of view it is looking at.
///
/// Documents that are clean, read-only, or generated are skipped rather than refused — saving a
/// view means "write what I have changed here", and a patch's own text is not that.
pub struct ViewSave;
impl RpcMethod for ViewSave {
    const NAME: &'static str = "view/save";
    const MUTATES_TEXT: bool = true;
    type Params = ViewSaveParams;
    type Result = ViewSaveResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewSaveParams {
    pub view_id: crate::ViewId,
    /// Acknowledges a divergence from disk, exactly as [`crate::buffer::BufferSaveParams`] does —
    /// and with the same two-step handshake. The first document that needs confirming aborts the
    /// call with its own error code; documents already written stay written, and the retry with
    /// `overwrite` finds them clean and skips them.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct ViewSaveResult {
    /// How many documents were written. `0` when the view had nothing dirty.
    pub saved: u32,
    /// The focused element's own result, when its document was among them — what a client folds
    /// into the buffer state it is already tracking. `None` when the focused element was clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused: Option<crate::buffer::BufferSaveResult>,
}

// ---- viewport/subscribe -------------------------------------------------------------------------

pub struct ViewportSubscribe;
impl RpcMethod for ViewportSubscribe {
    const NAME: &'static str = "view/subscribe";
    type Params = ViewportSubscribeParams;
    type Result = ViewportSubscribeResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportSubscribeParams {
    /// The **view** to show — what `view/open` answered with. For a patch that is the generated
    /// document's view, which no element windows and which nothing edits.
    pub view_id: crate::ViewId,
    pub cols: u32,
    pub rows: u32,
    pub overscan_rows: u32,
    /// Where to open: the server loads a screen of the named element around this line, and the
    /// client puts that line's block at the top of its viewport.
    pub scroll: ScrollPosition,
    /// Which element holds the cursor, when the subscriber knows — a re-subscribe (a wrap toggle,
    /// a reconnect) says where focus already is, so the server does not move it. Absent on a fresh
    /// open, where the element the scroll names is the one being looked at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<FieldId>,
    pub wrap: WrapMode,
    /// Cols the client reserves at the start of each *continuation* row for a wrap indicator
    /// glyph (e.g. "↪ "). The server subtracts this from the available width on continuation
    /// rows so the visible text + marker fit within `cols`. 0 disables.
    pub continuation_marker_width: u32,
    /// On-screen width of a tab character, in cols. The server uses this for soft-wrap math,
    /// visual-line motions, and centring so its idea of where bytes land matches what the
    /// client actually renders. Most clients will pass 4 or 8; 0 collapses tabs to zero width
    /// (don't do this unless you also strip tabs client-side).
    pub tab_width: u32,
    /// Whether to render the new viewport with the inline diff view on. The toggle is a sticky,
    /// client-wide preference but each viewport is fresh, so the client carries its current setting
    /// here and the first frame is correct without a follow-up `git/set_diff_view`. Defaults off.
    #[serde(default)]
    pub diff_view: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportSubscribeResult {
    pub viewport_id: ViewportId,
    pub window: Window,
    /// Buffer-level status, snapshotted at subscribe time. Subscribing is the act of *showing* a
    /// buffer, so it's where a client seeds the buffer-wide state it can't derive from the window:
    /// external-change flags, diagnostic counts, and language-server health. Carried in the
    /// response (not a follow-up notification) so it arrives atomically with the window, with no
    /// ordering race against the editor switch. Live updates then flow through `buffer/state`,
    /// `lsp/diagnostics_changed`, and `lsp/status_changed`.
    #[serde(default)]
    pub buffer_status: BufferStatusSnapshot,
    /// Which element of this view holds the cursor, and the buffer it windows — the same answer
    /// [`ViewportFocusElement`] gives, because it is the same question.
    ///
    /// Always present. A client mirrors the focused element, and the server decides it (from the
    /// scroll the subscribe named, today), so the mirror has to start from the server's value
    /// whichever element that is — for an ordinary view, element 0 over the buffer it *is*. It was
    /// once omitted when the focused element windowed the subscribed buffer itself, which left a
    /// subscribe landing in a patch's own text (a deleted file's block) with the server on element
    /// N and the client on element 0, and no cursor seated in either.
    ///
    /// For a composed view it is also what keeps the client out of two line spaces at once: the
    /// buffer it opened (a patch's generated document) is not the buffer its cursor is in, and a
    /// cursor held as a position in the view's document while every rendered line belongs to a file
    /// is a cursor nothing draws.
    pub focus: ViewportFocusElementResult,
}

/// The buffer-level state a client needs to start showing a buffer, beyond the rendered window —
/// see [`ViewportSubscribeResult::buffer_status`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BufferStatusSnapshot {
    /// File changed on disk while the buffer was dirty (the watcher couldn't silently reload).
    #[serde(default)]
    pub externally_modified: bool,
    /// File was removed on disk.
    #[serde(default)]
    pub externally_deleted: bool,
    /// Per-severity diagnostic counts for the status bar. Empty when none / no language server.
    #[serde(default, skip_serializing_if = "DiagnosticCounts::is_empty")]
    pub diagnostics: DiagnosticCounts,
    /// Health of the language server backing this buffer, if one is attached. `None` for an
    /// unbacked buffer (no server configured / no workspace root / not yet started).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp_status: Option<LspServerStatus>,
    /// The document-outline symbols enclosing this client's cursor, outermost first — the status
    /// bar's breadcrumb. Seeded here because subscribing is the act of *showing* a buffer, and the
    /// outline may have been cached long before (switching back to an already-open buffer pushes
    /// nothing). Live updates then flow through `lsp/symbol_path_changed`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbol_path: Vec<SymbolCrumb>,
}

// ---- viewport/resize ----------------------------------------------------------------------------

pub struct ViewportResize;
impl RpcMethod for ViewportResize {
    const NAME: &'static str = "view/resize";
    type Params = ViewportResizeParams;
    type Result = ViewportWindowResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportResizeParams {
    pub viewport_id: ViewportId,
    pub cols: u32,
    pub rows: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportWindowResult {
    pub window: Window,
}

// ---- view/window --------------------------------------------------------------------------------

/// Load the slices of the view a client's viewport reaches.
///
/// The client lays the view out from the tree — every element's height is in it, chrome is one row
/// each, prose it measures itself — so it knows which elements its viewport intersects and at what
/// row within each. What it cannot know is which *lines* those rows are, because that depends on
/// how the lines above them wrapped; so each slice is asked for by element and row within it, and
/// the server answers with the lines from there. One request loads every element a screen straddles.
///
/// Replaces the two scroll requests that addressed the view by a *view line* or an absolute row:
/// both presumed the server could sum the heights of everything above a point, which stops being
/// true the moment a view holds an element the client laid out.
pub struct ViewportWindow;
impl RpcMethod for ViewportWindow {
    const NAME: &'static str = "view/window";
    type Params = ViewportWindowParams;
    type Result = ViewportWindowResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportWindowParams {
    pub viewport_id: ViewportId,
    /// Where the viewport's top is, as content — what a reopen of this view restores. Reported
    /// here rather than through a request of its own, because every scroll that matters ends in a
    /// window request.
    pub anchor: ScrollPosition,
    /// The slices to load, replacing whatever was loaded before. An element not named is unloaded.
    pub slices: Vec<SliceRequest>,
}

/// One element's slice: `rows` rows starting `from_row` within the element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceRequest {
    pub element: FieldId,
    pub from_row: ElementRow,
    pub rows: u32,
}

// ---- viewport/focus_element ---------------------------------------------------------------------

/// Move focus to another of the view's editor elements.
///
/// Focus is *which element holds the live cursor*, and therefore which **buffer** an edit, a search
/// or an undo acts on. In a view of one element it is inert; in a patch it is how you move between
/// hunks, and once elements window different files it is what decides whose text you are editing.
///
/// The cursor lands at the element's first line, because an element you have just moved to has no
/// remembered position within it — and the alternative, keeping the old line number, would land
/// somewhere arbitrary once elements window different buffers whose line numbers merely collide.
pub struct ViewportFocusElement;
impl RpcMethod for ViewportFocusElement {
    const NAME: &'static str = "view/focus_element";
    type Params = ViewportFocusElementParams;
    type Result = ViewportFocusElementResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportFocusElementParams {
    pub viewport_id: ViewportId,
    pub target: FocusTarget,
}

/// Which element to focus.
///
/// Relative for a keystroke — `Tab` moves through what the user can see, and the server already
/// knows how many elements there are. Absolute for a **click**, which names one: without this the
/// shell could only step towards it, and clicking a line in another hunk instead set the cursor to
/// that line number *in the focused element's buffer* — a different file entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "to", rename_all = "snake_case")]
pub enum FocusTarget {
    Step { direction: FocusStep },
    Element { element: FieldId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusStep {
    Next,
    Previous,
}

/// A place *inside* a composed view: the element to focus and the buffer it currently windows.
///
/// Two fields because both are needed and neither implies the other: focusing is what makes the
/// cursor visible (a cursor in an unfocused element is not drawn), and the cursor itself is set per
/// `(client, buffer)`. The buffer rides along rather than being remembered by the caller because a
/// view's element buffers are transient — the same file can come back as a different buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSeat {
    pub element: FieldId,
    pub buffer_id: crate::BufferId,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportFocusElementResult {
    /// Where focus ended up — unchanged at the ends, which is what makes repeated presses stop
    /// rather than wrap.
    pub element: FieldId,
    /// Everything needed to bind to the buffer that element windows, carrying the cursor's landing
    /// position.
    ///
    /// The *whole* description rather than an id, because crossing into another buffer changes what
    /// the view is showing: its path, its label, whether it is read-only, which revision it is at.
    /// A client given only an id would have to keep the old buffer's label beside the new one's
    /// text. It is deliberately the buffer half of what an open returns, so the client rebinds
    /// through the path it already has — and only that half: an element's file is not a view, so
    /// there is no view id, scroll or keep flag to describe.
    pub buffer: crate::view::BufferDescription,
    /// The same buffer-level snapshot [`ViewportSubscribe`] seeds, for the buffer focus just landed
    /// in — breadcrumb, diagnostic counts, language-server health, external-change flags.
    ///
    /// Carried here for the same reason it is carried there: every field is a fact about the buffer
    /// under the cursor, and crossing an element changes which buffer that is. Without it a client
    /// can only go on showing the element it *left*, because the pushes that would correct it
    /// (`lsp/symbol_path_changed`, `lsp/diagnostics_changed`) are keyed to a buffer and only fire on
    /// a change — so a `Tab` between two files' hunks left the breadcrumb, the counts and the
    /// server glyph describing the previous file until something unrelated moved.
    #[serde(default)]
    pub buffer_status: BufferStatusSnapshot,
}

// ---- view/set_expanded --------------------------------------------------------------------------

/// Fold one of a view's elements shut, or open it up.
///
/// **Per viewport, not per view** — the same reason `git/set_diff_view` is: two clients reading one
/// conversation fold different blocks, and neither should move the other's. It is a fact about what
/// *this* screen is showing, so it rides the viewport and dies with it.
///
/// Only the elements a view marked collapsible answer to this; everything else refuses. That is
/// what keeps folding a property of the machinery a view builds — an agent's tool calls, its diffs,
/// its plan — rather than something a client can do to any element it can name. An agent's own
/// reply is not foldable, because the conversation is the thing you came to read.
///
/// Returns the rebuilt window rather than an acknowledgement: folding changes every row below the
/// fold, so a client would have to ask for one immediately anyway.
pub struct ViewportSetExpanded;
impl RpcMethod for ViewportSetExpanded {
    const NAME: &'static str = "view/set_expanded";
    type Params = ViewportSetExpandedParams;
    type Result = ViewportWindowResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportSetExpandedParams {
    pub viewport_id: ViewportId,
    /// Which element to fold. Absolute, never relative: the key that presses this names the
    /// focused element, and focus is the client's to report rather than the server's to re-derive.
    pub element: FieldId,
    /// `None` toggles, which is what a key press means. `Some` is for the caller that knows which
    /// state it wants — expand-on-reveal, when something scrolls a folded block into view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
}

// ---- viewport/navigate_change -------------------------------------------------------------------

/// Step to the next or previous **change** in a view — `c` / `Alt-c`, whatever the view shows.
///
/// View-scoped rather than buffer-scoped, and that is the whole point: a patch's changes are spread
/// across its elements, each windowing a different file. Asking one of those files what changed in
/// it answers a different question — for a commit it answers nothing at all, since a blob at a
/// revision has no baseline to diff against. The view knows, because its elements carry the diff's
/// own account of their lines.
///
/// **Total**: an ordinary view's changes are its one buffer's own hunks, and its outline is that
/// buffer's document symbols, so the same two keys go here whatever kind of view they are pressed
/// in. A client that routed them by view kind had to know what a view *was*; one that routes them
/// here does not.
///
/// Stepping past the last change stops rather than wrapping, as hunk navigation does in a file.
pub struct ViewportNavigateChange;
impl RpcMethod for ViewportNavigateChange {
    const NAME: &'static str = "view/navigate_change";
    type Params = ViewportNavigateChangeParams;
    /// The same shape focus returns: crossing into another element may cross into another buffer,
    /// and the client has to rebind to it.
    type Result = ViewportFocusElementResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportNavigateChangeParams {
    pub viewport_id: ViewportId,
    pub direction: FocusStep,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// How coarsely to step. Both grains are the same operation over the same index — "move focus
    /// to the next anchor in the view" — so they share a method rather than growing a second one
    /// whose result type would be identical.
    #[serde(default, skip_serializing_if = "NavigateGrain::is_default")]
    pub grain: NavigateGrain,
    /// Grow the selection to the landing change (Shift) rather than collapsing to a point there:
    /// the anchor is kept and the cursor jumps. Within one element only — a selection cannot span
    /// buffers — so a step that crosses into another element lands as a point there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub extend: bool,
}

/// The unit `view/navigate_change` steps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigateGrain {
    /// One change block — `c`/`Alt-c`, and what the changes picker lists.
    #[default]
    Change,
    /// One **outline entry** — `o`/`Alt-o`, and one stop per row the outline picker lists.
    ///
    /// The reason it belongs here rather than in a motion: a view's outline is known to the view,
    /// not to any buffer. `o` over an ordinary buffer steps document symbols, which is the same
    /// idea — the structure of what you are reading, above the grain of its lines.
    ///
    /// A composed view's outline is its *changes*, so this steps hunk to hunk. It stepped file to
    /// file while the outline's rows were files; the rows moved and this followed them, which is
    /// the point of the two reading one source.
    Outline,
}

impl NavigateGrain {
    fn is_default(&self) -> bool {
        matches!(self, NavigateGrain::Change)
    }
}

// ---- viewport/window_at_cursor ------------------------------------------------------------------

/// Return (and scroll to) a window containing **this client's cursor** in the view's focused
/// element.
///
/// The one thing a client cannot ask for in coordinates of its own. A slice is addressed by row
/// within its element, and when the cursor's line has scrolled out of the loaded slice the client
/// does not know its row: that depends on how the lines above the cursor wrapped, which the server
/// holds and the client can only guess at. Guessing is what it used to do, by fetching around the
/// focused *element's* start instead, which answers a different question the moment the cursor is
/// more than a screen into a large element: the window comes back without the cursor's line in it,
/// the reveal or placement waiting on it still cannot be performed, and the view has been dragged to
/// the top of the element for nothing. That is what "`;` has to be pressed twice" was.
///
/// No coordinates cross the wire, because both halves — where the cursor is, and how its element
/// wraps — live here.
pub struct ViewportWindowAtCursor;
impl RpcMethod for ViewportWindowAtCursor {
    const NAME: &'static str = "view/window_at_cursor";
    type Params = ViewportWindowAtCursorParams;
    type Result = ViewportWindowResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportWindowAtCursorParams {
    pub viewport_id: ViewportId,
}

// ---- viewport/set_wrap --------------------------------------------------------------------------

pub struct ViewportSetWrap;
impl RpcMethod for ViewportSetWrap {
    const NAME: &'static str = "view/set_wrap";
    type Params = ViewportSetWrapParams;
    type Result = ViewportWindowResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportSetWrapParams {
    pub viewport_id: ViewportId,
    pub wrap: WrapMode,
}

// ---- viewport/lines_changed (notification) ------------------------------------------------------

pub struct ViewportLinesChanged;
impl NotificationMethod for ViewportLinesChanged {
    const NAME: &'static str = "view/lines_changed";
    type Params = ViewportLinesChangedParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ViewportLinesChangedParams {
    pub viewport_id: ViewportId,
    /// Whose revision `revision` is, and whose lines these are.
    ///
    /// A view is not one buffer: a patch windows a file per hunk, so "the view's revision" is not a
    /// well-formed idea. Without this a client would file one buffer's revision against another and
    /// then discard the next push for that buffer as stale — the failure that makes any scheme
    /// redirecting a buffer id behind the client's back unworkable.
    pub buffer: crate::BufferId,
    pub revision: Revision,
    /// The window after the change: the same shape a geometry request answers with, re-rendered
    /// over the slices the viewport had loaded. A whole replacement rather than a splice — the
    /// client rebuilds from it, and a patch regenerates wholesale anyway.
    pub window: Window,
    /// The authoritative cursor for the receiving client after the change, decorated like an RPC
    /// response (`match_bracket`, `jumplist_position`). Lets the client adopt server-side cursor
    /// moves that have no request in flight — e.g. the clamp a watcher reload applies when the
    /// file shrank under the cursor. `None` when the client has no cursor on the buffer; the
    /// client then keeps its local state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<CursorState>,
}
