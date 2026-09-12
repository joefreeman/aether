//! Generated patch documents: the read-only buffers behind `git/show` and, later, the working-tree
//! and index diff views.
//!
//! Two things make this module rather than a corner of [`crate::git`]. It is the *renderer* — a
//! `git2::Diff` in, a buffer's worth of text and decoration out — so every diff source (a commit, a
//! stash, the index, the working tree) shares one presentation. And a patch is the one document
//! kind the editor generates rather than loads, which means it also has to hand back the structure
//! it knew while generating: [`PatchIndex`].
//!
//! # Chrome is not buffer text
//!
//! File and hunk separators render as [`Element`]s hung above the first content line they
//! introduce, exactly like the inline diff view's phantom deleted rows. The cursor therefore cannot
//! reach them **by construction**, which is the whole reason for the arrangement: the alternative —
//! buffer lines the cursor refuses to land on — would need a skip rule in cursor clamping, in every
//! motion, in where a search match lands, in sneak, and in jumplist and nav restore.
//!
//! The buffer's text is therefore only the parts you would want to select, copy, or search: the
//! commit's metadata and message, and the files' content. Content lines carry **no `+`/`-` prefix**
//! — the side travels as [`PatchLine`] and is drawn in the gutter — so a line of the patch is
//! byte-identical to that line of the real file. That is what lets syntax highlights be projected
//! from a parse of the whole blob at zero offset, and what makes copied text paste as code.

use std::collections::HashMap;

use aether_protocol::ui::Element;
use aether_protocol::viewport::{DiffStage, EmphasisRange, FieldId, Highlight, PatchLine};

use crate::syntax::{InjectionLayer, LanguageConfig};

/// Largest blob to parse for highlighting, matching the threshold `view/open` defers a parse at.
/// Past it the file's lines render unhighlighted rather than stalling the whole patch.
///
/// Both limits apply only to the files whose lines *stay in the generated document* — see
/// [`PlannedFile::bound_path`]. A file the driver windows over a real buffer is highlighted from
/// that buffer's own tree, and parsing its blobs here as well was, at three hundred files, the
/// larger part of what generating a working-changes view cost.
const MAX_HIGHLIGHT_BLOB_BYTES: usize = 128 * 1024;
/// Most files in one patch to parse. A sweeping commit is a legitimate thing to look at, and it
/// should open now and read plainly rather than open late and read beautifully.
const MAX_HIGHLIGHTED_FILES: usize = 64;

// ---- highlight kinds ----------------------------------------------------------------------------

/// Muted chrome: field names in the metadata block, a hunk header's trailing function context, a
/// placeholder line's prose. A dedicated role rather than `comment` — the closest existing one —
/// because `comment` is italic, which reads badly on a hash or a path.
pub const META: &str = "diff.meta";
/// The `@@ -a,b +c,d @@` landmark: with no line numbers in the gutter, this is where a patch's line
/// numbers live, so it takes an accent that can't be misread as added or removed.
pub const HUNK: &str = "diff.hunk";
/// A file separator's path — the heaviest boundary in the buffer.
pub const FILE: &str = "diff.file";
/// The `+N` half of a file separator's change counts.
pub const ADDED: &str = "diff.added";
/// The `-N` half of a file separator's change counts.
pub const REMOVED: &str = "diff.removed";

// ---- the model ----------------------------------------------------------------------------------

/// Everything a generated patch carries alongside its text: what to draw, and what it means.
///
/// One value rather than two independent fields so the render-facing decorations and the
/// logic-facing index cannot drift apart — both are built in a single pass over the same diff.
#[derive(Debug, Clone)]
pub struct GeneratedPatch {
    pub decorations: StaticDecorations,
    pub index: PatchIndex,
    /// The diff's own shape — which files, which hunks, which lines each added and removed. Kept
    /// beside the rendered text because a driver binding elements to real files needs the *diff's*
    /// account of them, and deriving it back out of the rendered text would be reading tea leaves.
    pub plan: PatchPlan,
}

/// Per-line decorations for a document whose text was *generated* rather than parsed.
///
/// All three vectors are indexed by logical line and may be shorter than the document (the rope's
/// trailing empty line has no entry) — read them with `get`. This is the escape hatch for content
/// tree-sitter can't describe as one language: a patch is chrome, a message, and interleaved
/// fragments of two versions of a file. The generator knows exactly what each line is, so it says
/// so here instead of emitting text for a grammar to guess at.
///
/// Only ever set on a read-only document — nothing maintains these across an edit.
#[derive(Debug, Clone, Default)]
pub struct StaticDecorations {
    /// Which side of the patch each line is, or `None` for context, the metadata block and the
    /// message. Context lines are in *both* versions, so they belong to neither side.
    pub patch: Vec<Option<PatchLine>>,
    /// Syntax spans per line, byte offsets within the line. Consumed exactly where a live parse
    /// tree's highlights would be, so they ride the ordinary `Segment::highlights` wire field.
    pub highlights: Vec<Vec<Highlight>>,
    /// Chrome rendered *after* the final line: the rule that closes the patch. Held apart from
    /// [`Self::chrome`] because it belongs to no line — the text ends without a trailing
    /// newline, so there is no empty last line for it to sit above.
    pub trailing_chrome: Vec<Element>,
    /// Chrome rendered above each line — file and hunk separators. Rides the same wire field as
    /// the inline diff view's phantom deleted rows; the two can never appear on one buffer, since
    /// a generated patch has no baseline of its own to be diffed against.
    pub chrome: Vec<Vec<Element>>,
    /// Which layer each line's change sits in — the bright/dim split the inline diff view already
    /// uses. Always `Unstaged` in a commit's diff (nothing there is pending); resolved per change
    /// block in the working-tree diff, where it is the only visible effect of staging.
    pub stage: Vec<DiffStage>,
    /// Intra-line emphasis per line: the byte ranges a paired removal/addition actually differ
    /// over. Unlike the inline diff view — where only the new side is a buffer line and the old
    /// side is a phantom row — both sides are ordinary lines here, so both carry their own spans.
    pub emphasis: Vec<Vec<EmphasisRange>>,
    /// The regions the chrome divides this document into, in line order and contiguous.
    pub elements: Vec<ElementSpan>,
}

/// One editor region of a generated patch: a maximal run of lines with no chrome between them.
///
/// **Identity, assigned once over the whole document.** The renderer used to number these as it
/// walked the *visible* window, so the same region was element 0 on one frame and element 2 after a
/// scroll. That was harmless while every element windowed the same generated document and nothing
/// downstream kept an id — but an element id is what names the buffer a region shows, and a name
/// that changes when you scroll cannot do that job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementSpan {
    pub id: FieldId,
    /// Document lines the region covers: `start_line..end_line`.
    pub start_line: u32,
    pub end_line: u32,
}

/// Split a document into elements at every line carrying chrome above it.
///
/// The boundary rule is the renderer's, moved here: chrome is what separates one editor from the
/// next, so the two agree by construction rather than by both remembering the same condition.
fn element_spans(chrome: &[Vec<Element>], line_count: u32) -> Vec<ElementSpan> {
    let mut spans: Vec<ElementSpan> = Vec::new();
    for i in 0..line_count {
        let starts = i == 0 || chrome.get(i as usize).is_some_and(|c| !c.is_empty());
        if starts {
            let id = spans.len() as FieldId;
            if let Some(last) = spans.last_mut() {
                last.end_line = i;
            }
            spans.push(ElementSpan {
                id,
                start_line: i,
                end_line: line_count,
            });
        }
    }
    spans
}

/// The structure a generated patch knew while it was being built, in **buffer-line coordinates**.
///
/// Kept because every feature over a patch buffer is a lookup against it: stepping between changes,
/// listing them in a picker, staging the one under the cursor, and following a line back to the
/// file it came from all need to get from a cursor position to `(file, hunk, side, source line)`.
#[derive(Debug, Clone, Default)]
pub struct PatchIndex {
    pub files: Vec<PatchFile>,
    /// Parallel to buffer lines; `None` on the metadata block and the message, which belong to no
    /// file. Read with `get` — the trailing empty line has no entry.
    pub lines: Vec<Option<PatchLineInfo>>,
}

#[derive(Debug, Clone)]
pub struct PatchFile {
    /// Repo-relative path on each side. Both are set for a modification; a rename has two
    /// different ones, an addition has no old path and a deletion no new one.
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: PatchFileStatus,
    /// Language name for the *new* side, detected from the path, and for the old side where a
    /// rename changed the extension. Drives the whole-blob parse the highlights are projected from.
    pub new_language: Option<String>,
    pub old_language: Option<String>,
    pub added: u32,
    pub removed: u32,
    /// Buffer lines this file's content occupies: `start_line..end_line`. Always non-empty — a
    /// delta with no textual content still gets one placeholder line, which is what keeps it
    /// visible, navigable and stageable.
    pub start_line: u32,
    pub end_line: u32,
    pub hunks: Vec<PatchHunk>,
    /// The file's individual changes, in buffer order — the unit `c`/`Alt-c` steps and the changes
    /// picker lists. Flat across the file rather than nested under [`Self::hunks`], because a hunk
    /// is a *display* region and a change is a thing you act on; one hunk routinely holds several.
    pub changes: Vec<PatchChangeBlock>,
}

/// One run of changed lines: a maximal block of `+`/`-` lines bounded by context.
///
/// A run of removals immediately followed by its replacements is **one** block, not two — that is a
/// modification, and stopping on it twice would make `c` stutter over a single edit.
///
/// Deliberately not the hunk. A hunk starts with the context lines that make its change readable,
/// so landing on `hunk.start_line` puts the cursor several lines *above* anything that changed.
#[derive(Debug, Clone)]
pub struct PatchChangeBlock {
    /// Buffer lines the block occupies: `start_line..end_line`. Every line in the range is a `+`
    /// or `-` line (or, for a delta with no hunks, its placeholder).
    pub start_line: u32,
    pub end_line: u32,
    pub added: u32,
    pub removed: u32,
    /// Whether this change is already in the index. Always `Unstaged` in a commit's diff, where
    /// the distinction has no meaning; resolved per block in the working-tree diff.
    pub stage: DiffStage,
}

/// Which of a file's working-tree changes are already staged.
///
/// Needed because staging does **not** change `git diff HEAD` — the composed view's text is
/// identical before and after. What changes is which layer each block sits in, so without this the
/// view couldn't show you that anything had happened.
///
/// Built from the index→worktree diff, which is precisely the *unstaged* layer: a block that
/// overlaps it is unstaged, and one that doesn't is already in the index.
struct StageIndex {
    unstaged: Vec<crate::git::DiffHunk>,
}

impl StageIndex {
    /// The stage split for `path` — or `None` when the index holds exactly what the diff's left
    /// side (`left`) does, so nothing is staged and every change is unstaged without a diff to
    /// prove it. That is most files most of the time, and the working-changes view is rebuilt on
    /// every save under the repo: the index→worktree diff of every *unstaged* file was the larger
    /// part of what each rebuild cost.
    fn for_file(repo: &git2::Repository, path: &str, left: git2::Oid) -> Option<Self> {
        let entry = repo.index().ok()?.get_path(std::path::Path::new(path), 0);
        if entry.as_ref().is_some_and(|e| e.id == left) {
            return None;
        }
        let index_blob = entry
            .and_then(|e| repo.find_blob(e.id).ok())
            .map(|b| b.content().to_vec())
            .unwrap_or_default();
        let worktree = std::fs::read(repo.workdir()?.join(path)).unwrap_or_default();
        Some(StageIndex {
            unstaged: crate::git::hunks_from_buffers(&index_blob, &worktree),
        })
    }

    /// `new_lines` are the block's 1-based worktree line numbers; `deletion_at` the worktree line a
    /// pure removal sits above (a removal occupies no line of its own).
    fn stage_of(&self, new_lines: &[u32], deletion_at: Option<u32>) -> DiffStage {
        let overlaps = |h: &crate::git::DiffHunk| {
            if h.new_lines == 0 {
                // A pure deletion in the unstaged layer covers no line — it sits *at* its anchor.
                return deletion_at.is_some_and(|at| at.saturating_sub(1) == h.anchor_line);
            }
            new_lines
                .iter()
                .any(|&n| (h.anchor_line..h.anchor_line + h.new_lines).contains(&(n - 1)))
        };
        if self.unstaged.iter().any(overlaps) {
            DiffStage::Unstaged
        } else {
            DiffStage::Staged
        }
    }
}

impl PatchFile {
    /// The path to show and to act on: the new side normally, the old side for a deletion.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("(unknown)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchFileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    /// The file's mode changed with its content untouched (`chmod +x`).
    ModeChanged,
    /// Git declined to diff the content. Carries no hunks, so it renders as a placeholder line.
    Binary,
}

#[derive(Debug, Clone)]
pub struct PatchHunk {
    /// Buffer lines this hunk's content occupies: `start_line..end_line`.
    pub start_line: u32,
    pub end_line: u32,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// The enclosing signature git prints after the second `@@` — "where you are" in the file.
    ///
    /// Already rendered as this hunk's heading row; kept here as well because it is
    /// the **outline's** label for every change in the hunk, and an outline that read it back out of
    /// the rendered chrome would be parsing its own output. Empty when git offers none (the top of
    /// a file, a non-code file), which the outline falls back from.
    pub signature: String,
}

#[derive(Debug, Clone, Copy)]
pub struct PatchLineInfo {
    /// Index into [`PatchIndex::files`].
    pub file: u32,
    /// Index into that file's `hunks`. `None` on a placeholder line, which stands for a delta with
    /// no hunks at all.
    pub hunk: Option<u32>,
    pub side: Option<PatchLine>,
    /// 1-based line number on each side, as libgit2 reports it. A context line has both; an
    /// addition has only `new`, a deletion only `old`. Both are `None` on a placeholder.
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
}

// ---- the builder --------------------------------------------------------------------------------

/// Accumulates a patch document: buffer text, the decorations parallel to it, and the index.
///
/// Chrome is pushed to [`Self::chrome`] and sits pending until the next content line claims it —
/// that is what keeps separators out of the text. Every content line therefore has to go through
/// [`Self::content_line`], and a file with nothing to show still emits one placeholder, or its
/// chrome would have nothing to attach to and the file would silently vanish from the view.
#[derive(Default)]
pub struct PatchBuilder {
    text: String,
    sides: Vec<Option<PatchLine>>,
    highlights: Vec<Vec<Highlight>>,
    chrome: Vec<Vec<Element>>,
    emphasis: Vec<Vec<EmphasisRange>>,
    stage: Vec<DiffStage>,
    lines: Vec<Option<PatchLineInfo>>,
    pending: Vec<Element>,
    files: Vec<PatchFile>,
    plan: PatchPlan,
}

/// A highlight span given as byte offsets within the line or row it applies to.
pub type Span = (usize, usize, &'static str);

fn spans_to_highlights(spans: &[Span]) -> Vec<Highlight> {
    spans
        .iter()
        .map(|&(start, end, kind)| Highlight {
            start: start as u32,
            end: end as u32,
            kind: kind.to_string(),
        })
        .collect()
}

impl PatchBuilder {
    /// The next buffer line index that will be written.
    pub fn next_line(&self) -> u32 {
        self.sides.len() as u32
    }

    /// A line of the metadata block or the commit message: real, focusable buffer text belonging to
    /// no file.
    pub fn header_line(&mut self, text: &str, spans: &[Span]) {
        self.push(
            text,
            spans_to_highlights(spans),
            Vec::new(),
            DiffStage::Unstaged,
            None,
            None,
        );
    }

    /// Queue a heading to render above the next content line — a file's path, a hunk's signature.
    ///
    /// Flush with the code under it. The lead-in space this used to carry was clearance from the
    /// rail drawn in the gutter cell; the file block's box holds that line now, so a second one
    /// only reads as a heading indented out of step with the lines it names.
    pub fn heading(&mut self, text: impl Into<String>, spans: &[Span]) {
        let content = Element::row(vec![Element::text(text.into(), spans_to_highlights(spans))]);
        self.pending.push(Element::chrome(vec![content]));
    }

    /// Queue a blank row above the next content line. The band is all it is.
    pub fn blank(&mut self) {
        self.pending
            .push(Element::chrome(vec![Element::row(Vec::new())]));
    }

    /// A line of file content, which also flushes any queued chrome above itself. `highlights` are
    /// projected from a parse of the whole blob this line came from, so they arrive already in
    /// line-local byte offsets — the same shape a live tree's would be.
    pub fn content_line(
        &mut self,
        text: &str,
        highlights: Vec<Highlight>,
        emphasis: Vec<EmphasisRange>,
        stage: DiffStage,
        info: PatchLineInfo,
    ) {
        self.push(text, highlights, emphasis, stage, info.side, Some(info));
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        text: &str,
        highlights: Vec<Highlight>,
        emphasis: Vec<EmphasisRange>,
        stage: DiffStage,
        side: Option<PatchLine>,
        info: Option<PatchLineInfo>,
    ) {
        self.text.push_str(text);
        self.text.push('\n');
        self.sides.push(side);
        self.highlights.push(highlights);
        self.emphasis.push(emphasis);
        self.stage.push(stage);
        let group = std::mem::take(&mut self.pending);
        self.chrome.push(group);
        self.lines.push(info);
    }

    pub fn push_file(&mut self, file: PatchFile) -> u32 {
        self.files.push(file);
        self.files.len() as u32 - 1
    }

    /// Record where a file's content ended, now that it has all been written.
    pub fn close_file(&mut self, idx: u32) {
        let end = self.next_line();
        if let Some(f) = self.files.get_mut(idx as usize) {
            f.end_line = end;
        }
    }

    pub fn file_mut(&mut self, idx: u32) -> &mut PatchFile {
        &mut self.files[idx as usize]
    }

    pub fn finish(mut self) -> (String, GeneratedPatch) {
        // No rule closing the patch: the last file block's box draws its own bottom border now
        // (`close_last_box`), so the figure is one frame rather than a frame plus a node standing
        // in for its missing edge. Whatever chrome is still pending has nowhere to sit *above* —
        // the text ends at its last content line — so it becomes the trailing block instead.
        // No trailing newline: the empty last line it would create is one the cursor can land on,
        // below everything the patch has to show. The closing chrome hangs off the final content
        // line instead, which is what `trailing_chrome` exists for.
        if self.text.ends_with('\n') {
            self.text.pop();
        }
        let PatchBuilder {
            plan,
            text,
            sides,
            highlights,
            chrome,
            emphasis,
            stage,
            lines,
            files,
            pending,
            ..
        } = self;
        let elements = element_spans(&chrome, lines.len() as u32);
        (
            text,
            GeneratedPatch {
                decorations: StaticDecorations {
                    patch: sides,
                    highlights,
                    chrome,
                    trailing_chrome: pending,
                    emphasis,
                    stage,
                    elements,
                },
                index: PatchIndex { files, lines },
                plan,
            },
        )
    }
}

// ---- projecting syntax highlights from the blobs ------------------------------------------------

/// One side of a file, parsed **in full**, with the line index needed to project per-line
/// highlights out of it.
///
/// Parsing whole blobs rather than the patch's own text is the point. Tree-sitter is error
/// tolerant, so it *would* return a tree for a lone hunk — but the wrong one: highlight queries key
/// on parent context, so a method body lifted out of its `impl` stops matching the patterns that
/// classify it, and the same code would highlight differently here and in the file. Both sides are
/// real blobs in the object database, so there is no need to guess: parse each in full, then use
/// libgit2's per-line `old_lineno`/`new_lineno` to copy each line's spans across.
struct ParsedBlob {
    text: String,
    config: &'static LanguageConfig,
    tree: tree_sitter::Tree,
    injections: Vec<InjectionLayer>,
    /// Byte offset where each 0-based line starts.
    line_starts: Vec<usize>,
}

impl ParsedBlob {
    fn parse(repo: &git2::Repository, id: git2::Oid, path: Option<&String>) -> Option<Self> {
        if id.is_zero() {
            return None;
        }
        let config = crate::syntax::config_for_path(std::path::Path::new(path?))?;
        let blob = repo.find_blob(id).ok()?;
        if blob.is_binary() || blob.content().len() > MAX_HIGHLIGHT_BLOB_BYTES {
            return None;
        }
        let text = String::from_utf8(blob.content().to_vec()).ok()?;
        let tree = crate::syntax::make_parser(config).parse(&text, None)?;
        let injections = crate::syntax::compute_injections(config, &tree, &text);

        let mut line_starts = vec![0usize];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter(|&(_, b)| b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Some(ParsedBlob {
            text,
            config,
            tree,
            injections,
            line_starts,
        })
    }

    /// Byte range of a 1-based line, excluding its line terminator.
    fn line_range(&self, lineno: u32) -> Option<(usize, usize)> {
        let idx = lineno.checked_sub(1)? as usize;
        let start = *self.line_starts.get(idx)?;
        let mut end = self
            .line_starts
            .get(idx + 1)
            .copied()
            .unwrap_or(self.text.len());
        let bytes = self.text.as_bytes();
        if end > start && bytes[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        Some((start, end))
    }

    /// Per-line highlights for the 1-based line range `start..start + count`, in line-local byte
    /// offsets.
    ///
    /// One query over the whole range rather than one per line: the tree is whole-file (so the
    /// parse is right) but the query is range-restricted (so the cost is proportional to the hunk,
    /// not the file). A span crossing a line — a block comment, a multi-line string — is split at
    /// the line boundaries.
    fn spans_for_lines(&self, start: u32, count: u32) -> HashMap<u32, Vec<Highlight>> {
        let mut out = HashMap::new();
        if count == 0 || start == 0 {
            return out;
        }
        let last = start + count - 1;
        let (Some((range_start, _)), Some((_, range_end))) =
            (self.line_range(start), self.line_range(last))
        else {
            return out;
        };
        let spans = crate::syntax::highlights_for_range(
            self.config,
            &self.tree,
            &self.injections,
            &self.text,
            range_start,
            range_end,
        );

        // Spans are ascending and non-overlapping, so one walk in step with the lines suffices.
        let mut si = 0usize;
        for lineno in start..=last {
            let Some((ls, le)) = self.line_range(lineno) else {
                break;
            };
            // Skip spans that finish before this line; a span *straddling* the boundary is kept,
            // since it still has to paint its tail here.
            while si < spans.len() && range_start + spans[si].end as usize <= ls {
                si += 1;
            }
            let mut line_spans = Vec::new();
            for span in &spans[si..] {
                let s = range_start + span.start as usize;
                if s >= le {
                    break;
                }
                let e = range_start + span.end as usize;
                let (cs, ce) = (s.max(ls) - ls, e.min(le) - ls);
                if ce > cs {
                    line_spans.push(Highlight {
                        start: cs as u32,
                        end: ce as u32,
                        kind: span.kind.clone(),
                    });
                }
            }
            if !line_spans.is_empty() {
                out.insert(lineno, line_spans);
            }
        }
        out
    }
}

// ---- rendering a diff ---------------------------------------------------------------------------

/// Human-readable byte size for a binary file's placeholder line.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Git's octal spelling of a file mode, for a mode-change placeholder.
fn mode_string(mode: git2::FileMode) -> &'static str {
    match mode {
        git2::FileMode::Blob => "100644",
        git2::FileMode::BlobGroupWritable => "100664",
        git2::FileMode::BlobExecutable => "100755",
        git2::FileMode::Link => "120000",
        git2::FileMode::Commit => "160000",
        git2::FileMode::Tree => "040000",
        _ => "000000",
    }
}

fn path_of(file: &git2::DiffFile<'_>) -> Option<String> {
    file.path().map(|p| p.to_string_lossy().into_owned())
}

fn language_of(path: Option<&String>) -> Option<String> {
    let path = path?;
    crate::syntax::config_for_path(std::path::Path::new(path)).map(|c| c.name.to_string())
}

/// Render every delta of `diff` into `b`, appending to whatever header lines are already there.
///
/// Deltas are walked explicitly rather than through libgit2's patch printer: the printer's callback
/// stream can't distinguish "this file has no hunks" from "this file hasn't started yet", and a
/// delta with no hunks — a binary file, a bare `chmod`, a pure rename — is exactly the case that
/// needs a placeholder line inventing for it.
/// What a patch view is composed of, derived from the diff **and nothing else**.
///
/// No repository, no blobs, no filesystem — the signature is the guarantee. That matters because it
/// is the claim the whole lazy-binding design rests on: a forty-file patch has to know how many
/// regions it has and how tall each one is *before* deciding which files to open, or laying the view
/// out would cost forty file reads and forty tree-sitter parses (measured at 96–446ms).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatchPlan {
    pub files: Vec<PlannedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: PatchFileStatus,
    pub added: u32,
    pub removed: u32,
    /// One per hunk, in order. **Empty** for a delta with no textual content — a binary swap, a
    /// mode change — which renders as a single placeholder rather than as an editor.
    pub regions: Vec<PlannedRegion>,
}

impl PlannedFile {
    /// The file the driver windows this delta's hunks over, if it windows one: a new side to open,
    /// and hunks to window it with. `None` for a deletion, whose lines have nowhere to go but the
    /// generated document, and for a hunkless delta, which is a placeholder line.
    ///
    /// The one place this is decided. [`layout_over_files`] binds by it, the open path binds only
    /// what it names, and [`render_diff`] reads it to know which of the lines it emits will ever
    /// be shown — a bound file's are replaced by the real buffer's before anyone sees them, so
    /// its blobs are not parsed for highlights.
    ///
    /// Decided by status rather than by `new_path` alone: libgit2 gives a deleted delta's new
    /// side the same path as its old one, with nothing behind it.
    pub fn bound_path(&self) -> Option<&str> {
        if self.regions.is_empty() || self.status == PatchFileStatus::Deleted {
            return None;
        }
        self.new_path.as_deref()
    }
}

/// One hunk, as line ranges on both sides. 1-based, as libgit2 reports them.
///
/// `new_lines == 0` is a pure deletion: the hunk shows entirely as phantom rows above
/// `new_start`, and windows no line of the new side at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRegion {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// New-side line numbers this hunk **added** (1-based). Context lines are absent, so a line of
    /// the range not listed here is one the commit left alone.
    pub added: Vec<u32>,
    /// Removed lines, each anchored to the new-side line it sat **above** (1-based) — the same
    /// arrangement the inline diff's phantom rows already use. A removal at the very end of a hunk
    /// anchors one past its last line.
    ///
    /// The text comes from the diff itself, not from any file: `git2` carries hunk content, which
    /// is what lets a patch describe its own removed lines without opening the old blob.
    pub removed: Vec<(u32, String)>,
}

/// A patch view's elements, over whichever of its files `resolve` can supply a buffer for.
///
/// Pure: a function of the diff plus that map. That is what lets the two callers differ only in how
/// they resolve — the open path opens files asynchronously, while the save/stage/commit refresh
/// looks up buffers already open, since opening files *there* would be both surprising and async.
/// Keeping the element logic here means those two cannot drift.
///
/// A region `resolve` declines keeps windowing the generated document. That is not only an error
/// path: a **deleted** file has no new side to window, and a delta with no hunks at all (a binary
/// swap, a mode change) has only a placeholder line. Those are why the generated text — and
/// `LineChange::Patch` with it — is still load-bearing.
///
/// It is also why the two kinds of element answer the inline diff toggle differently. A bound
/// element's removed lines become phantoms, which collapse onto the gutter marker when the diff is
/// off. A generated slice's are ordinary buffer text with nowhere to collapse *to* — a file that is
/// wholly gone has no surviving line to mark — so they stay. Removals collapse wherever there is a
/// line left to collapse onto, and a deleted file's content is the only readable thing it has.
pub fn layout_over_files(
    generated: &GeneratedPatch,
    mut resolve: impl FnMut(&str) -> Option<aether_protocol::BufferId>,
) -> Vec<crate::state::ElementLayout> {
    use aether_protocol::viewport::{BaselineRow, DiffMarker, DiffStage};

    let mut layout = Vec::with_capacity(generated.decorations.elements.len());
    for span in &generated.decorations.elements {
        let chrome_above = std::sync::Arc::new(
            generated
                .decorations
                .chrome
                .get(span.start_line as usize)
                .cloned()
                .unwrap_or_default(),
        );
        // The box around this element's file. One per file, so consecutive elements of the same
        // file share it and the next file's opens a new one — which is what `collapse` then draws
        // as a tee rather than as two rules.
        //
        // A left border and a top border: the rail down the file's side, and the rule that opens
        // it. The rule the file header used to emit is this border now, which is why it is not
        // drawn twice.
        let file_of = |line: u32| -> Option<u32> {
            generated
                .index
                .lines
                .get(line as usize)
                .copied()
                .flatten()
                .map(|i| i.file)
        };
        let (box_group, edges, band) = match file_of(span.start_line) {
            Some(file) => (
                Some(file),
                aether_protocol::ui::Edges {
                    // Rails down both sides and a rule across the top. No bottom border: with
                    // `collapse` the next file's top rule *is* this file's closing one, so the run
                    // reads as one ruled list rather than as a stack of separate boxes. The last
                    // file's block is closed by the patch's trailing rule.
                    border: aether_protocol::ui::Sides {
                        top: 1,
                        left: 1,
                        right: 1,
                        bottom: 0,
                    },
                    // No padding. The gutter cell already sits between the left rail and the
                    // text — blank on an unchanged line — and the right rail stands off the ragged
                    // end of the code, so a padding cell either side only widened the block
                    // without separating anything that was touching.
                    padding: aether_protocol::ui::Sides::ZERO,
                    collapse: true,
                },
                aether_protocol::ui::Band::Chrome,
            ),
            // The patch's opening caption belongs to no file, so it sits in no box.
            None => (
                None,
                aether_protocol::ui::Edges::NONE,
                aether_protocol::ui::Band::None,
            ),
        };
        let generated_slice = || crate::state::ElementLayout {
            extent: crate::state::ElementExtent::OwnDocument {
                lines: span.start_line..span.end_line,
            },
            chrome_before: Default::default(),
            chrome_above: chrome_above.clone(),
            decorations: None,
            edges,
            box_group,
            title: Default::default(),
            band,
            role: aether_protocol::ui::ElementRole::Field,
        };

        // Which file and hunk this element's first line belongs to — read from the index that
        // produced the text, never guessed from position.
        let info = generated
            .index
            .lines
            .get(span.start_line as usize)
            .copied()
            .flatten();
        let Some((info, hunk)) = info.and_then(|i| Some((i, i.hunk?))) else {
            layout.push(generated_slice());
            continue;
        };
        let Some(file) = generated.plan.files.get(info.file as usize) else {
            layout.push(generated_slice());
            continue;
        };
        let (Some(path), Some(region)) = (file.bound_path(), file.regions.get(hunk as usize))
        else {
            layout.push(generated_slice());
            continue;
        };
        let Some(buffer_id) = resolve(path) else {
            layout.push(generated_slice());
            continue;
        };

        // Two sources, each for what it alone knows. The **plan** carries the removed lines' text,
        // already anchored to the new-side line they sat above, and it comes from the diff, so no
        // file is read for it. The **generated record** carries each line's stage, and staged
        // varies *within* a hunk — it is the only visible effect of staging in this view, while the
        // plan holds one stage per region, which would flatten it.
        let stage_of: HashMap<u32, DiffStage> = (span.start_line..span.end_line)
            .filter_map(|line| {
                let at = generated
                    .index
                    .lines
                    .get(line as usize)
                    .copied()
                    .flatten()?;
                let stage = generated
                    .decorations
                    .stage
                    .get(line as usize)
                    .copied()
                    .unwrap_or_default();
                Some((at.new_lineno?.saturating_sub(1), stage))
            })
            .collect();
        let stage_at = |line: u32| stage_of.get(&line).copied().unwrap_or_default();

        // libgit2 counts lines from 1 and buffers from 0: the shift happens here, once.
        let mut decorations = crate::state::ElementDecorations::default();
        for (anchor, text) in &region.removed {
            let line = anchor.saturating_sub(1);
            decorations
                .baseline_above
                .entry(line)
                .or_default()
                .push(BaselineRow {
                    text: text.clone(),
                    stage: stage_at(line),
                    emphasis: Vec::new(),
                });
        }
        for &added in &region.added {
            let line = added.saturating_sub(1);
            let marker = if decorations.baseline_above.contains_key(&line) {
                DiffMarker::Modified
            } else {
                DiffMarker::Added
            };
            decorations.markers.insert(line, (marker, stage_at(line)));
        }
        // A context line with removals above it is a pure deletion: flagged, not tinted.
        for line in decorations
            .baseline_above
            .keys()
            .copied()
            .collect::<Vec<_>>()
        {
            decorations
                .markers
                .entry(line)
                .or_insert((DiffMarker::Deleted, stage_at(line)));
        }

        // File lines, and 1-based from libgit2 — the `- 1` is the only place that conversion
        // happens, and the extent says which buffer they are lines of so nothing downstream has to
        // guess.
        let first = region.new_start.saturating_sub(1);
        layout.push(crate::state::ElementLayout {
            chrome_before: Default::default(),
            extent: crate::state::ElementExtent::Bound {
                buffer: buffer_id,
                lines: first..first + region.new_lines,
            },
            chrome_above,
            decorations: Some(std::sync::Arc::new(decorations)),
            edges,
            box_group,
            title: Default::default(),
            band,
            role: aether_protocol::ui::ElementRole::Field,
        });
    }
    close_last_box(&mut layout);
    layout
}

/// Give the patch's last file block a bottom border, so the box closes itself.
///
/// `collapse` means a block draws no bottom edge — the next file's top rule *is* its closing one,
/// which is what makes a run of files read as one ruled list rather than a stack of boxes. The last
/// block has no next file, and a trailing rule node used to stand in for the edge it was
/// missing. It is a border now, so the frame owns the figure from `┌` to `┘`, and the rule closes
/// rail to rail rather than running the whole width of the pane.
///
/// Set on the run's **first** element, because that is the one `compose_tree` reads a box's edges
/// from — the others in the run carry theirs only for their own wrap inset, which a bottom border
/// does not change.
fn close_last_box(layout: &mut [crate::state::ElementLayout]) {
    let Some(last) = layout.iter().rev().find_map(|e| e.box_group) else {
        return; // a patch with no file blocks at all — nothing to close
    };
    if let Some(first) = layout.iter_mut().find(|e| e.box_group == Some(last)) {
        first.edges.border.bottom = 1;
    }
}

/// The shape of a diff: which files it touches, and which line ranges of each.
#[cfg(test)]
pub fn plan_diff(diff: &git2::Diff<'_>) -> Result<PatchPlan, String> {
    plan_diff_keeping_patches(diff).map(|(plan, _)| plan)
}

/// [`plan_diff`], handing back the per-delta [`git2::Patch`] it walked as well.
///
/// Materialising a patch is where libgit2 runs the file's line diff, so it is the expensive half
/// of planning — and rendering needs the same patches for the same deltas, in the same order.
/// Planning keeps them rather than have rendering diff every file a second time.
fn plan_diff_keeping_patches<'d>(
    diff: &'d git2::Diff<'_>,
) -> Result<(PatchPlan, Vec<Option<git2::Patch<'d>>>), String> {
    let mut files = Vec::new();
    let mut patches = Vec::new();
    for (idx, delta) in diff.deltas().enumerate() {
        let patch = git2::Patch::from_diff(diff, idx).map_err(|e| e.message().to_string())?;
        let hunk_count = patch.as_ref().map(|p| p.num_hunks()).unwrap_or(0);
        let (_, added, removed) = patch
            .as_ref()
            .and_then(|p| p.line_stats().ok())
            .unwrap_or((0, 0, 0));
        let binary = delta.flags().is_binary();

        let mut status = match delta.status() {
            git2::Delta::Added => PatchFileStatus::Added,
            git2::Delta::Deleted => PatchFileStatus::Deleted,
            git2::Delta::Renamed => PatchFileStatus::Renamed,
            git2::Delta::Copied => PatchFileStatus::Copied,
            _ => PatchFileStatus::Modified,
        };
        // Refined by what the diff actually produced, which the delta's own status doesn't say: a
        // `chmod` and a binary swap both arrive as plain `Modified` with nothing to show.
        if binary {
            status = PatchFileStatus::Binary;
        } else if hunk_count == 0
            && delta.old_file().mode() != delta.new_file().mode()
            && matches!(status, PatchFileStatus::Modified)
        {
            status = PatchFileStatus::ModeChanged;
        }

        let mut regions = Vec::with_capacity(hunk_count);
        if let Some(patch) = patch.as_ref() {
            for h in 0..hunk_count {
                let (hunk, _) = patch.hunk(h).map_err(|e| e.message().to_string())?;
                let mut added = Vec::new();
                let mut removed: Vec<(u32, String)> = Vec::new();
                // Removals have no line of their own on the new side, so each waits for the next
                // line that does and anchors above it.
                let mut pending: Vec<String> = Vec::new();
                let line_count = patch
                    .num_lines_in_hunk(h)
                    .map_err(|e| e.message().to_string())?;
                for l in 0..line_count {
                    let line = patch
                        .line_in_hunk(h, l)
                        .map_err(|e| e.message().to_string())?;
                    let text = || {
                        String::from_utf8_lossy(line.content())
                            .trim_end_matches('\n')
                            .to_string()
                    };
                    match line.origin() {
                        '+' => {
                            if let Some(n) = line.new_lineno() {
                                added.push(n);
                                removed.extend(pending.drain(..).map(|t| (n, t)));
                            }
                        }
                        ' ' => {
                            if let Some(n) = line.new_lineno() {
                                removed.extend(pending.drain(..).map(|t| (n, t)));
                            }
                        }
                        '-' => pending.push(text()),
                        // `=`, `>`, `<` are "\ No newline at end of file" markers — an artefact of
                        // the patch *format*, which this view deliberately isn't.
                        _ => {}
                    }
                }
                // Anything still pending was removed from the end of the hunk, so it anchors one
                // past the last new-side line it could sit above.
                let tail = hunk.new_start() + hunk.new_lines();
                removed.extend(pending.into_iter().map(|t| (tail, t)));

                regions.push(PlannedRegion {
                    old_start: hunk.old_start(),
                    old_lines: hunk.old_lines(),
                    new_start: hunk.new_start(),
                    new_lines: hunk.new_lines(),
                    added,
                    removed,
                });
            }
        }

        files.push(PlannedFile {
            old_path: path_of(&delta.old_file()),
            new_path: path_of(&delta.new_file()),
            status,
            added: added as u32,
            removed: removed as u32,
            regions,
        });
        patches.push(patch);
    }
    Ok((PatchPlan { files }, patches))
}

/// The patch's opening caption — `N files changed  +A  −B`, plus `since <rev>` when the working
/// changes are being measured against something other than HEAD.
///
/// Chrome, so it holds no cursor position: nothing in it can be staged, followed or navigated to.
/// The counts take the same green/red as a file separator's, which is what makes the number you
/// scan for readable at a glance instead of a run of muted text.
///
/// Summed off the plan, which already counted every file's lines: `git2::Diff::stats` would count
/// them again, and at generation that meant diffing every file in the tree a second time.
///
/// The baseline rides here rather than in the buffer's title because the caption is regenerated
/// with the content and the title isn't — a caption can't be left saying `since main` after the
/// baseline went back to the index.
///
/// No blank below it: the first file's rule follows immediately, and a gap there left the caption
/// floating between the top of the buffer and the diff instead of sitting on it.
fn emit_summary(b: &mut PatchBuilder, plan: &PatchPlan, since: Option<&str>) {
    let mut text = String::new();
    let mut spans: Vec<Span> = Vec::new();

    let files = format!(
        "{} file{} changed",
        plan.files.len(),
        if plan.files.len() == 1 { "" } else { "s" }
    );
    spans.push((0, files.len(), META));
    text.push_str(&files);
    let added = format!("  +{}", plan.files.iter().map(|f| f.added).sum::<u32>());
    spans.push((text.len(), text.len() + added.len(), ADDED));
    text.push_str(&added);
    let removed = format!("  −{}", plan.files.iter().map(|f| f.removed).sum::<u32>());
    spans.push((text.len(), text.len() + removed.len(), REMOVED));
    text.push_str(&removed);
    if let Some(label) = since {
        let since = format!("  since {label}");
        spans.push((text.len(), text.len() + since.len(), META));
        text.push_str(&since);
    }

    b.heading(text, &spans);
}

/// Render `diff` into `b`: the opening caption, then every file's block.
///
/// `against_worktree` says the right-hand side is the working tree, whose changes split into a
/// staged and an unstaged layer; a commit's never do. `since` names the left-hand side in the
/// caption when it is something other than HEAD.
///
/// An empty diff renders nothing at all — not even the caption — so a caller with something to say
/// about the emptiness says it in place of the diff.
pub fn render_diff(
    repo: &git2::Repository,
    diff: &git2::Diff<'_>,
    b: &mut PatchBuilder,
    against_worktree: bool,
    since: Option<&str>,
) -> Result<(), String> {
    // The shape first, then the text. One source of truth for what each delta *is*: the driver will
    // build its elements from this same plan, and a second copy of the status refinement here is
    // exactly the kind of thing that drifts.
    let (plan, patches) = plan_diff_keeping_patches(diff)?;
    if !plan.files.is_empty() {
        emit_summary(b, &plan, since);
    }
    let mut parsed_files = 0usize;
    for ((idx, delta), patch) in diff.deltas().enumerate().zip(patches) {
        let planned = &plan.files[idx];
        let old_path = planned.old_path.clone();
        let new_path = planned.new_path.clone();
        let hunk_count = planned.regions.len();
        let (status, added, removed) = (planned.status, planned.added, planned.removed);

        let file_idx = b.push_file(PatchFile {
            old_language: language_of(old_path.as_ref()),
            new_language: language_of(new_path.as_ref()),
            old_path: old_path.clone(),
            new_path: new_path.clone(),
            status,
            added,
            removed,
            start_line: b.next_line(),
            end_line: b.next_line(),
            hunks: Vec::new(),
            changes: Vec::new(),
        });

        emit_file_header(b, &delta, status, added, removed);

        if hunk_count == 0 {
            emit_placeholder(b, &delta, status, file_idx);
        } else {
            // Highlights, for the lines that will be read here. A file the driver binds shows the
            // real buffer's lines under that buffer's own tree, and what is generated for it is
            // never seen; only a file with no new side to open — a deletion — keeps its text, so
            // only its blobs are parsed. Both sides are, and each from *its own* path: a rename
            // can change the extension, and the old side has to highlight as what it was.
            let (old_blob, new_blob) =
                if planned.bound_path().is_none() && parsed_files < MAX_HIGHLIGHTED_FILES {
                    parsed_files += 1;
                    (
                        ParsedBlob::parse(repo, delta.old_file().id(), old_path.as_ref()),
                        ParsedBlob::parse(repo, delta.new_file().id(), new_path.as_ref()),
                    )
                } else {
                    (None, None)
                };
            // Only the working-tree diff has a staged/unstaged split to resolve; in a commit's
            // diff every line is equally history.
            let stages = (against_worktree)
                .then(|| {
                    new_path
                        .as_deref()
                        .and_then(|p| StageIndex::for_file(repo, p, delta.old_file().id()))
                })
                .flatten();
            let patch = patch.as_ref().expect("hunks imply a patch");
            for h in 0..hunk_count {
                emit_hunk(
                    b,
                    patch,
                    h,
                    file_idx,
                    old_blob.as_ref(),
                    new_blob.as_ref(),
                    stages.as_ref(),
                )?;
            }
        }
        b.close_file(file_idx);
    }
    b.plan = plan;
    Ok(())
}

/// The file block's opening: a full-width rule, then `path  +N −M` (or `old → new` for a rename).
fn emit_file_header(
    b: &mut PatchBuilder,
    delta: &git2::DiffDelta<'_>,
    status: PatchFileStatus,
    added: u32,
    removed: u32,
) {
    // No rule here: the file block's box draws it as its top border, and `collapse` makes the
    // boundary between two files one rule rather than two. Emitting one as well would double it.

    let old_path = path_of(&delta.old_file());
    let new_path = path_of(&delta.new_file());
    let mut text = String::new();
    let mut spans: Vec<Span> = Vec::new();

    if matches!(status, PatchFileStatus::Renamed | PatchFileStatus::Copied) {
        let from = old_path.clone().unwrap_or_default();
        let to = new_path.clone().unwrap_or_default();
        spans.push((0, from.len(), META));
        text.push_str(&from);
        let arrow = " → ";
        spans.push((text.len(), text.len() + arrow.len(), META));
        text.push_str(arrow);
        spans.push((text.len(), text.len() + to.len(), FILE));
        text.push_str(&to);
    } else {
        let path = new_path.clone().or(old_path.clone()).unwrap_or_default();
        spans.push((0, path.len(), FILE));
        text.push_str(&path);
    }

    let note = match status {
        PatchFileStatus::Added => Some("new file"),
        PatchFileStatus::Deleted => Some("deleted"),
        PatchFileStatus::Binary => Some("binary"),
        PatchFileStatus::ModeChanged => Some("mode"),
        _ => None,
    };
    if let Some(note) = note {
        let s = format!("  ({note})");
        spans.push((text.len(), text.len() + s.len(), META));
        text.push_str(&s);
    }
    if added > 0 {
        let s = format!("  +{added}");
        spans.push((text.len(), text.len() + s.len(), ADDED));
        text.push_str(&s);
    }
    if removed > 0 {
        let s = format!("  −{removed}");
        spans.push((text.len(), text.len() + s.len(), REMOVED));
        text.push_str(&s);
    }

    b.heading(text, &spans);
}

/// The one content line standing in for a delta git gave no hunks: a binary file, a bare mode
/// change, a pure rename, an empty file.
///
/// Real (focusable) buffer text rather than more chrome, and that is the point: without a cursor
/// position the file could not be staged with `Space g Alt-s`, could not be followed into with
/// `Enter`, and would have no row in the changes picker.
fn emit_placeholder(
    b: &mut PatchBuilder,
    delta: &git2::DiffDelta<'_>,
    status: PatchFileStatus,
    file_idx: u32,
) {
    let old_mode = delta.old_file().mode();
    let new_mode = delta.new_file().mode();
    let text = if delta.flags().is_binary() {
        let old_size = delta.old_file().size();
        let new_size = delta.new_file().size();
        match status {
            PatchFileStatus::Added => format!("Binary file · {}", human_size(new_size)),
            PatchFileStatus::Deleted => format!("Binary file · {}", human_size(old_size)),
            _ => format!(
                "Binary file · {} → {}",
                human_size(old_size),
                human_size(new_size)
            ),
        }
    } else if old_mode != new_mode {
        format!("mode {} → {}", mode_string(old_mode), mode_string(new_mode))
    } else if matches!(status, PatchFileStatus::Renamed | PatchFileStatus::Copied) {
        "no content change".to_string()
    } else {
        "empty file".to_string()
    };

    // Tinted only where the whole file arrived or left — a modified binary belongs to neither side.
    let side = match status {
        PatchFileStatus::Added => Some(PatchLine::Added),
        PatchFileStatus::Deleted => Some(PatchLine::Removed),
        _ => None,
    };
    // The placeholder is the file's one change, so it gets a block like any other — otherwise a
    // binary swap or a bare `chmod` would be unreachable by `c` and absent from the picker.
    let line = b.next_line();
    b.file_mut(file_idx).changes.push(PatchChangeBlock {
        start_line: line,
        end_line: line + 1,
        added: u32::from(side == Some(PatchLine::Added)),
        removed: u32::from(side == Some(PatchLine::Removed)),
        // Nothing textual to locate in the index, so it reads as the top layer.
        stage: DiffStage::Unstaged,
    });
    b.blank();
    b.content_line(
        &text,
        spans_to_highlights(&[(0, text.len(), META)]),
        Vec::new(),
        DiffStage::Unstaged,
        PatchLineInfo {
            file: file_idx,
            hunk: None,
            side,
            old_lineno: None,
            new_lineno: None,
        },
    );
    b.blank();
}

/// One content line of a hunk, gathered before anything is pushed.
///
/// Buffered rather than pushed as it arrives because intra-line emphasis has to pair a removal
/// with the addition that replaced it, and the addition hasn't been seen yet when the removal
/// arrives.
struct HunkLine {
    text: String,
    side: Option<PatchLine>,
    old_lineno: Option<u32>,
    new_lineno: Option<u32>,
}

/// Intra-line emphasis for a hunk's lines: the byte ranges each paired removal and addition
/// actually differ over, parallel to `lines`.
///
/// Pairs are positional within a *change block* — a run of removals immediately followed by a run
/// of additions — the k-th removal with the k-th addition, which is the same rule the inline diff
/// view uses. Unpaired lines in a lopsided block (three removed, one added) keep the whole-line
/// tint and nothing more, which is the honest rendering: there is no counterpart to point at.
fn intraline_for_hunk(lines: &[HunkLine]) -> Vec<Vec<EmphasisRange>> {
    let to_ranges = |spans: crate::git::EmphasisSpans| -> Vec<EmphasisRange> {
        spans
            .into_iter()
            .map(|(start, end)| EmphasisRange { start, end })
            .collect()
    };
    let mut out = vec![Vec::new(); lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].side != Some(PatchLine::Removed) {
            i += 1;
            continue;
        }
        let removed_start = i;
        while i < lines.len() && lines[i].side == Some(PatchLine::Removed) {
            i += 1;
        }
        let added_start = i;
        while i < lines.len() && lines[i].side == Some(PatchLine::Added) {
            i += 1;
        }
        let pairs = (added_start - removed_start).min(i - added_start);
        for k in 0..pairs {
            let (o, n) = (removed_start + k, added_start + k);
            // A bail (`None`: the pair was rewritten wholesale, or is too long to analyse) renders
            // the same as an identical pair — no emphasis — so both fold to empty here.
            let Some((old_spans, new_spans)) =
                crate::git::intraline_emphasis(&lines[o].text, &lines[n].text)
            else {
                continue;
            };
            out[o] = to_ranges(old_spans);
            out[n] = to_ranges(new_spans);
        }
    }
    out
}

fn emit_hunk(
    b: &mut PatchBuilder,
    patch: &git2::Patch<'_>,
    h: usize,
    file_idx: u32,
    old_blob: Option<&ParsedBlob>,
    new_blob: Option<&ParsedBlob>,
    stages: Option<&StageIndex>,
) -> Result<(), String> {
    let (hunk, _) = patch.hunk(h).map_err(|e| e.message().to_string())?;
    let header = String::from_utf8_lossy(hunk.header());
    let header = header.trim_end_matches('\n');

    // git prints `@@ -a,b +c,d @@ <enclosing signature>`. Only the signature is kept: it says
    // *where you are*, which is what a section heading is for. The line ranges were the one place
    // a patch showed line numbers, and they go with it — deliberately, since the gutter has none
    // either. The shells trail this with a muted rule out to the right edge.
    let signature = header
        .match_indices("@@")
        .nth(1)
        .map(|(i, m)| header[i + m.len()..].trim())
        .unwrap_or("");
    let spans: Vec<Span> = if signature.is_empty() {
        Vec::new()
    } else {
        vec![(0, signature.len(), HUNK)]
    };
    // A blank above the heading and none below, so the gap reads as belonging to the boundary
    // rather than to the heading: the signature sits directly on the code it names, and what sets
    // it apart from the hunk before is the space over it. Emitted here rather than at the end of
    // the previous hunk so the first heading in a file gets one too, and so the last hunk leaves
    // no trailing blank to flush into the next file's block.
    //
    // The blank is the boundary; the heading is what the boundary is *called*, and it is only
    // drawn when it has something new to say. Git offers no signature for a hunk that starts at
    // the top of its file, and a heading with no text is a blank row impersonating a heading;
    // two hunks inside the same function repeat one signature, and the second is a label for
    // somewhere the reader already is. Either way the blank alone says "new hunk", which is the
    // part that was carrying the meaning.
    //
    // Against the previous *hunk's* signature rather than the last one shown: the two cases
    // cannot overlap, since only a hunk containing line 1 lacks a signature and only the first
    // hunk can contain line 1.
    let repeats_previous = b
        .file_mut(file_idx)
        .hunks
        .last()
        .is_some_and(|h| h.signature == signature);
    b.blank();
    if !signature.is_empty() && !repeats_previous {
        b.heading(signature, &spans);
    }

    let start_line = b.next_line();
    let line_count = patch
        .num_lines_in_hunk(h)
        .map_err(|e| e.message().to_string())?;
    let mut lines: Vec<HunkLine> = Vec::with_capacity(line_count);
    for l in 0..line_count {
        let line = patch
            .line_in_hunk(h, l)
            .map_err(|e| e.message().to_string())?;
        let side = match line.origin() {
            '+' => Some(PatchLine::Added),
            '-' => Some(PatchLine::Removed),
            ' ' => None,
            // `=`, `>`, `<` are the "\ No newline at end of file" markers. Dropped: they are an
            // artefact of the patch *format*, and this view deliberately isn't one.
            _ => continue,
        };
        // The origin character is not part of `content` for these origins, so the text is already
        // exactly the file's own line — which is the property the whole design rests on.
        let content = String::from_utf8_lossy(line.content());
        lines.push(HunkLine {
            text: content.trim_end_matches('\n').to_string(),
            side,
            old_lineno: line.old_lineno(),
            new_lineno: line.new_lineno(),
        });
    }

    let emphasis = intraline_for_hunk(&lines);
    let old_spans = old_blob
        .map(|p| p.spans_for_lines(hunk.old_start(), hunk.old_lines()))
        .unwrap_or_default();
    let new_spans = new_blob
        .map(|p| p.spans_for_lines(hunk.new_start(), hunk.new_lines()))
        .unwrap_or_default();

    // Blocks first, so each line can be pushed already knowing which layer it sits in: the block
    // is the unit staging acts on, so it is also the unit that is staged or not.
    let mut blocks: Vec<(usize, usize, u32, u32, DiffStage)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].side.is_none() {
            i += 1;
            continue;
        }
        let block_start = i;
        let (mut added, mut removed) = (0u32, 0u32);
        while i < lines.len() && lines[i].side.is_some() {
            match lines[i].side {
                Some(PatchLine::Added) => added += 1,
                Some(PatchLine::Removed) => removed += 1,
                None => unreachable!("guarded by the loop condition"),
            }
            i += 1;
        }
        let stage = match stages {
            Some(stages) => {
                let new_lines: Vec<u32> = lines[block_start..i]
                    .iter()
                    .filter_map(|l| l.new_lineno)
                    .collect();
                // A block with no new-side line is a pure removal: it occupies no worktree line, so
                // it is located by the line it sits above — the next surviving one.
                let deletion_at = new_lines
                    .is_empty()
                    .then(|| lines.get(i).and_then(|l| l.new_lineno))
                    .flatten();
                stages.stage_of(&new_lines, deletion_at)
            }
            None => DiffStage::Unstaged,
        };
        blocks.push((block_start, i, added, removed, stage));
    }
    let stage_of_line = |i: usize| {
        blocks
            .iter()
            .find(|(s, e, ..)| (*s..*e).contains(&i))
            .map_or(DiffStage::Unstaged, |&(.., stage)| stage)
    };

    for (i, line) in lines.iter().enumerate() {
        // Context is in both blobs; take the new side, which is the one that still exists.
        let highlights = match (line.new_lineno, line.old_lineno) {
            (Some(n), _) => new_spans.get(&n).cloned(),
            (None, Some(o)) => old_spans.get(&o).cloned(),
            (None, None) => None,
        };
        b.content_line(
            &line.text,
            highlights.unwrap_or_default(),
            emphasis[i].clone(),
            stage_of_line(i),
            PatchLineInfo {
                file: file_idx,
                hunk: Some(h as u32),
                side: line.side,
                old_lineno: line.old_lineno,
                new_lineno: line.new_lineno,
            },
        );
    }

    // `lines[k]` sits at buffer line `start_line + k`, so the ranges fall straight out.
    for (block_start, block_end, added, removed, stage) in blocks {
        b.file_mut(file_idx).changes.push(PatchChangeBlock {
            start_line: start_line + block_start as u32,
            end_line: start_line + block_end as u32,
            added,
            removed,
            stage,
        });
    }

    let end_line = b.next_line();
    b.file_mut(file_idx).hunks.push(PatchHunk {
        start_line,
        end_line,
        signature: signature.to_string(),
        old_start: hunk.old_start(),
        old_lines: hunk.old_lines(),
        new_start: hunk.new_start(),
        new_lines: hunk.new_lines(),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo whose HEAD commit changes two files, one of them in two places.
    fn repo_with_a_two_file_commit(root: &std::path::Path) -> git2::Repository {
        let repo = git2::Repository::init(root).unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        let commit = |repo: &git2::Repository, files: &[(&str, &str)]| {
            for (name, body) in files {
                std::fs::write(root.join(name), body).unwrap();
            }
            let mut index = repo.index().unwrap();
            for (name, _) in files {
                index.add_path(std::path::Path::new(name)).unwrap();
            }
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let parents: Vec<git2::Commit> = repo
                .head()
                .ok()
                .and_then(|h| h.peel_to_commit().ok())
                .into_iter()
                .collect();
            let refs: Vec<&git2::Commit> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, "c", &tree, &refs)
                .unwrap();
        };
        let long = (1..=40).map(|i| format!("line {i}\n")).collect::<String>();
        commit(&repo, &[("a.txt", long.as_str()), ("b.txt", "one\n")]);
        // Two separate edits to a.txt, far enough apart to be distinct hunks.
        let edited = long
            .replace("line 2\n", "LINE 2\n")
            .replace("line 38\n", "LINE 38\n");
        commit(&repo, &[("a.txt", edited.as_str()), ("b.txt", "ONE\n")]);
        repo
    }

    fn head_diff(repo: &git2::Repository) -> git2::Diff<'_> {
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let parent = head.parent(0).unwrap();
        repo.diff_tree_to_tree(
            Some(&parent.tree().unwrap()),
            Some(&head.tree().unwrap()),
            None,
        )
        .unwrap()
    }

    /// Every shape of delta in one commit: a modified Rust file (bound by the driver), a deleted
    /// one (left in the generated text), a rename with no edits (a placeholder), and a binary
    /// swap (another placeholder).
    fn repo_with_every_delta_shape(root: &std::path::Path) -> git2::Repository {
        let repo = git2::Repository::init(root).unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        let commit = |repo: &git2::Repository, add: &[&str], remove: &[&str]| {
            let mut index = repo.index().unwrap();
            for name in add {
                index.add_path(std::path::Path::new(name)).unwrap();
            }
            for name in remove {
                index.remove_path(std::path::Path::new(name)).unwrap();
            }
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let parents: Vec<git2::Commit> = repo
                .head()
                .ok()
                .and_then(|h| h.peel_to_commit().ok())
                .into_iter()
                .collect();
            let refs: Vec<&git2::Commit> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, "c", &tree, &refs)
                .unwrap();
        };
        std::fs::write(root.join("a.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        std::fs::write(root.join("gone.rs"), "fn gone() {}\nlet x = 1;\n").unwrap();
        std::fs::write(root.join("old.txt"), "same\n").unwrap();
        std::fs::write(root.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        commit(&repo, &["a.rs", "gone.rs", "old.txt", "bin.dat"], &[]);

        std::fs::write(
            root.join("a.rs"),
            "fn a() {}\nfn B() {}\nfn c() {}\nfn d() {}\n",
        )
        .unwrap();
        std::fs::remove_file(root.join("gone.rs")).unwrap();
        std::fs::rename(root.join("old.txt"), root.join("new.txt")).unwrap();
        std::fs::write(root.join("bin.dat"), [0u8, 9, 9, 9]).unwrap();
        commit(
            &repo,
            &["a.rs", "new.txt", "bin.dat"],
            &["gone.rs", "old.txt"],
        );
        repo
    }

    /// The caption used to come from `git2::Diff::stats`, which diffs every file a second time.
    /// It comes from the plan now, and has to say exactly what libgit2 would have said — across a
    /// modification, a deletion, a rename and a binary swap, which each count differently.
    #[test]
    fn the_caption_counts_what_libgit2_counts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = repo_with_every_delta_shape(&root);
        let mut diff = head_diff(&repo);
        let _ = diff.find_similar(None);
        let stats = diff.stats().unwrap();
        let plan = plan_diff(&diff).expect("plans");

        assert_eq!(plan.files.len(), 4, "the fixture has every shape");
        assert_eq!(plan.files.len(), stats.files_changed());
        assert_eq!(
            plan.files.iter().map(|f| f.added).sum::<u32>() as usize,
            stats.insertions()
        );
        assert_eq!(
            plan.files.iter().map(|f| f.removed).sum::<u32>() as usize,
            stats.deletions()
        );
        // Named, so agreement can't be a mutual zero.
        assert_eq!(stats.insertions(), 2, "`fn B` and `fn d`");
        assert_eq!(stats.deletions(), 3, "`fn b` and both lines of gone.rs");
    }

    /// Blobs are parsed for highlights only where the generated lines will be *read*: a file the
    /// driver binds shows the real buffer's lines under that buffer's own tree, so parsing its
    /// blobs here would style text nobody sees — at three hundred files, most of what a
    /// working-changes rebuild cost. A deletion has no buffer to bind and keeps its highlights.
    #[test]
    fn blobs_are_parsed_only_for_files_left_in_the_generated_text() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = repo_with_every_delta_shape(&root);
        let mut diff = head_diff(&repo);
        let _ = diff.find_similar(None);
        let mut b = PatchBuilder::default();
        render_diff(&repo, &diff, &mut b, false, None).expect("renders");
        let (text, generated) = b.finish();

        let line_at = |want: &str| {
            text.lines()
                .position(|l| l == want)
                .unwrap_or_else(|| panic!("no line {want:?} in:\n{text}"))
        };
        let bound = generated
            .plan
            .files
            .iter()
            .find(|f| f.new_path.as_deref() == Some("a.rs"));
        assert_eq!(bound.and_then(|f| f.bound_path()), Some("a.rs"));
        assert!(
            generated.decorations.highlights[line_at("fn B() {}")].is_empty(),
            "a bound file's generated lines are never shown, so they are not styled"
        );

        let deleted = generated
            .plan
            .files
            .iter()
            .find(|f| f.old_path.as_deref() == Some("gone.rs"))
            .expect("the deletion");
        assert_eq!(
            deleted.bound_path(),
            None,
            "nothing to window a deletion over"
        );
        let kinds: Vec<&str> = generated.decorations.highlights[line_at("fn gone() {}")]
            .iter()
            .map(|h| h.kind.as_str())
            .collect();
        assert!(
            kinds.contains(&"keyword"),
            "a deletion reads highlighted: {kinds:?}"
        );

        // Placeholders bind nothing either — the rename and the binary swap have no regions.
        for name in ["new.txt", "bin.dat"] {
            let f = generated
                .plan
                .files
                .iter()
                .find(|f| f.new_path.as_deref() == Some(name))
                .unwrap();
            assert!(f.regions.is_empty(), "{name} is a placeholder");
            assert_eq!(f.bound_path(), None);
        }
    }

    /// The stage split is only computed for a file that has something staged. Most files in a
    /// working tree don't, and the view is rebuilt on every save under the repo — so the
    /// index→worktree diff of every *other* file was the larger part of what a rebuild cost.
    #[test]
    fn the_stage_index_skips_files_with_nothing_staged() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = repo_with_a_two_file_commit(&root);
        let head_blob = |name: &str| {
            repo.head()
                .unwrap()
                .peel_to_tree()
                .unwrap()
                .get_path(std::path::Path::new(name))
                .unwrap()
                .id()
        };

        // An unstaged edit: the index still holds HEAD's blob, so there is nothing to split.
        std::fs::write(root.join("b.txt"), "ONE\nTWO\n").unwrap();
        assert!(
            StageIndex::for_file(&repo, "b.txt", head_blob("b.txt")).is_none(),
            "index == HEAD: every change is unstaged without a diff"
        );

        // Stage it, then edit again: now the split has to be computed, and it says the second
        // edit is the unstaged one.
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("b.txt")).unwrap();
        index.write().unwrap();
        std::fs::write(root.join("b.txt"), "ONE\nTWO\nTHREE\n").unwrap();
        let split = StageIndex::for_file(&repo, "b.txt", head_blob("b.txt"))
            .expect("index != HEAD: something is staged");
        assert_eq!(split.stage_of(&[2], None), DiffStage::Staged, "`TWO`");
        assert_eq!(split.stage_of(&[3], None), DiffStage::Unstaged, "`THREE`");
    }

    /// A patch's shape comes from the diff alone — one region per hunk, with the new-side range
    /// each windows.
    ///
    /// The whole lazy-binding design rests on this: a forty-file patch has to know how many regions
    /// it has and how tall each is *before* choosing which files to open. `plan_diff` takes only a
    /// `&git2::Diff` — no repository, no blobs, no filesystem — so the guarantee is in the
    /// signature, and this pins the numbers it produces.
    #[test]
    fn a_diff_plans_one_region_per_hunk_without_opening_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = repo_with_a_two_file_commit(&root);
        let plan = plan_diff(&head_diff(&repo)).expect("the diff plans");

        let names: Vec<&str> = plan
            .files
            .iter()
            .map(|f| f.new_path.as_deref().unwrap_or("?"))
            .collect();
        assert_eq!(names, vec!["a.txt", "b.txt"]);

        let a = &plan.files[0];
        assert_eq!(a.status, PatchFileStatus::Modified);
        assert_eq!(
            a.regions.len(),
            2,
            "two edits far apart are two hunks: {:?}",
            a.regions
        );
        // Each region windows the new side around its edit — line 2 and line 38, with context.
        assert!(
            a.regions[0].new_start <= 2 && a.regions[0].new_lines > 0,
            "the first hunk covers line 2: {:?}",
            a.regions[0]
        );
        assert!(
            a.regions[1].new_start <= 38 && a.regions[1].new_start + a.regions[1].new_lines > 38,
            "the second hunk covers line 38: {:?}",
            a.regions[1]
        );
        assert_eq!(a.added, 2, "one added line per hunk");
        assert_eq!(a.removed, 2);

        let b = &plan.files[1];
        assert_eq!(b.regions.len(), 1, "one edit is one hunk");
        assert_eq!((b.added, b.removed), (1, 1));
    }

    /// A hunk knows which new-side lines it added, and where its removed lines sat.
    ///
    /// This is what a patch element's decorations are built from: the `+` lines become
    /// `DiffMarker::Added`, and the `-` lines become phantom rows anchored above the line that
    /// replaced them — the arrangement the inline diff already uses, which is why a patch can share
    /// its decoration path. All of it comes from the diff; no file is read.
    #[test]
    fn a_hunk_knows_its_added_lines_and_where_its_removals_sat() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = repo_with_a_two_file_commit(&root);
        let plan = plan_diff(&head_diff(&repo)).expect("plans");

        // b.txt: the single line "one" became "ONE" — one addition, one removal above it.
        let b = &plan.files[1];
        let region = &b.regions[0];
        assert_eq!(region.added, vec![1], "line 1 of the new side was added");
        assert_eq!(
            region.removed,
            vec![(1, "one".to_string())],
            "and the line it replaced sat above it"
        );

        // a.txt: two separate single-line edits, each a removal replaced by an addition.
        let a = &plan.files[0];
        assert_eq!(a.regions[0].added, vec![2]);
        assert_eq!(a.regions[0].removed, vec![(2, "line 2".to_string())]);
        assert_eq!(a.regions[1].added, vec![38]);
        assert_eq!(a.regions[1].removed, vec![(38, "line 38".to_string())]);

        // Context lines are absent from `added` — that is what makes the marker map meaningful.
        assert!(
            !a.regions[0].added.contains(&1),
            "line 1 is context and was not added: {:?}",
            a.regions[0].added
        );
    }

    /// The plan agrees with what the renderer actually emitted: one hunk per planned region.
    ///
    /// Cheap cross-check, and the thing that would catch the two walks drifting apart — the reason
    /// `render_diff` consumes the plan rather than recomputing the status refinement itself.
    ///
    /// Counted off the **index**, not off the hunk headings. The headings used to be one per hunk
    /// and were the obvious proxy; they are a presentation choice now — one is dropped when git
    /// gives no signature or when it would repeat the heading above it — so counting them would
    /// make this test fail for a reason that has nothing to do with the two walks agreeing.
    #[test]
    fn the_plan_matches_what_the_renderer_emits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = repo_with_a_two_file_commit(&root);
        let diff = head_diff(&repo);
        let plan = plan_diff(&diff).expect("plans");

        let mut b = PatchBuilder::default();
        render_diff(&repo, &diff, &mut b, false, None).expect("renders");
        let (_, generated) = b.finish();

        let planned_regions: usize = plan.files.iter().map(|f| f.regions.len()).sum();
        // Named, so a mutual zero can't pass as agreement.
        assert_eq!(planned_regions, 3, "two hunks in a.txt and one in b.txt");
        let rendered_hunks: usize = generated.index.files.iter().map(|f| f.hunks.len()).sum();
        assert_eq!(
            planned_regions, rendered_hunks,
            "every planned region should have produced exactly one hunk"
        );
        assert_eq!(
            plan.files.len(),
            generated.index.files.len(),
            "the plan and the index should agree on how many files there are"
        );
    }

    fn rule() -> Element {
        Element::chrome(vec![Element::fill('─')])
    }

    /// Chrome above lines 2 and 5 makes three regions, and line 0 opens one whether or not it has
    /// chrome of its own.
    #[test]
    fn element_spans_split_on_chrome() {
        let chrome: Vec<Vec<Element>> = (0..6)
            .map(|i| {
                if i == 2 || i == 5 {
                    vec![rule()]
                } else {
                    vec![]
                }
            })
            .collect();
        let spans = element_spans(&chrome, 6);
        assert_eq!(
            spans
                .iter()
                .map(|s| (s.id, s.start_line, s.end_line))
                .collect::<Vec<_>>(),
            vec![(0, 0, 2), (1, 2, 5), (2, 5, 6)]
        );
    }

    /// The property the bindings rely on: every line belongs to exactly one element, so resolving
    /// a cursor to its region can never come up empty or ambiguous.
    #[test]
    fn element_spans_cover_every_line_exactly_once() {
        for pattern in [vec![0usize], vec![1], vec![0, 1, 2, 3], vec![3], vec![]] {
            let chrome: Vec<Vec<Element>> = (0..4)
                .map(|i| {
                    if pattern.contains(&i) {
                        vec![rule()]
                    } else {
                        vec![]
                    }
                })
                .collect();
            let spans = element_spans(&chrome, 4);
            let covered: Vec<u32> = spans
                .iter()
                .flat_map(|s| s.start_line..s.end_line)
                .collect();
            assert_eq!(covered, (0..4).collect::<Vec<_>>(), "pattern {pattern:?}");
        }
    }

    fn line(text: &str, side: Option<PatchLine>) -> HunkLine {
        HunkLine {
            text: text.to_string(),
            side,
            old_lineno: None,
            new_lineno: None,
        }
    }

    fn removed(text: &str) -> HunkLine {
        line(text, Some(PatchLine::Removed))
    }
    fn added(text: &str) -> HunkLine {
        line(text, Some(PatchLine::Added))
    }
    fn context(text: &str) -> HunkLine {
        line(text, None)
    }

    /// The emphasised substrings of each line, so the assertions read as "this bit changed"
    /// rather than as byte arithmetic.
    fn emphasised(lines: &[HunkLine]) -> Vec<Vec<String>> {
        intraline_for_hunk(lines)
            .into_iter()
            .zip(lines)
            .map(|(ranges, l)| {
                ranges
                    .into_iter()
                    .map(|r| l.text[r.start as usize..r.end as usize].to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn pairs_a_removal_with_the_addition_that_replaced_it() {
        let lines = [
            context("fn main() {"),
            removed("    let x = 1;"),
            added("    let x = 2;"),
            context("}"),
        ];
        assert_eq!(
            emphasised(&lines),
            vec![
                Vec::<String>::new(),
                vec!["1".to_string()],
                vec!["2".to_string()],
                Vec::<String>::new(),
            ],
            "only the digit that actually changed is emphasised, and context is never touched"
        );
    }

    #[test]
    fn pairs_are_positional_within_the_change_block() {
        let lines = [
            removed("alpha one"),
            removed("beta two"),
            added("alpha ONE"),
            added("beta TWO"),
        ];
        let e = emphasised(&lines);
        assert_eq!(e[0], vec!["one"], "first removal pairs with first addition");
        assert_eq!(e[2], vec!["ONE"]);
        assert_eq!(e[1], vec!["two"], "second with second");
        assert_eq!(e[3], vec!["TWO"]);
    }

    /// A lopsided block has no counterpart for the extra lines, so they keep the whole-line tint
    /// and nothing more rather than being paired with something arbitrary.
    #[test]
    fn unpaired_lines_in_a_lopsided_block_get_no_emphasis() {
        // The pair differs on *both* sides, so "was not paired" is distinguishable from "was
        // paired and turned out identical" — both of which render as no emphasis.
        let lines = [
            removed("keep A"),
            removed("two"),
            removed("three"),
            added("keep B"),
        ];
        let e = emphasised(&lines);
        assert_eq!(e[0], vec!["A"], "the one pair that exists is analysed");
        assert_eq!(e[3], vec!["B"]);
        assert!(e[1].is_empty(), "no addition to compare `two` against");
        assert!(e[2].is_empty(), "nor `three`");
    }

    #[test]
    fn a_pure_deletion_or_addition_has_nothing_to_compare() {
        let deleted = [context("keep"), removed("gone"), removed("also gone")];
        assert!(emphasised(&deleted).iter().all(|e| e.is_empty()));

        let inserted = [context("keep"), added("new"), added("also new")];
        assert!(emphasised(&inserted).iter().all(|e| e.is_empty()));
    }

    /// Context between two change blocks separates them: the second block pairs from its own
    /// start, not against leftovers from the first.
    #[test]
    fn context_separates_change_blocks() {
        let lines = [
            removed("a 1"),
            added("a 2"),
            context("unchanged"),
            removed("b 3"),
            added("b 4"),
        ];
        let e = emphasised(&lines);
        assert_eq!(e[0], vec!["1"]);
        assert_eq!(e[1], vec!["2"]);
        assert!(e[2].is_empty());
        assert_eq!(e[3], vec!["3"]);
        assert_eq!(e[4], vec!["4"]);
    }

    /// An addition run that comes *before* its removals is not a change block — that shape is a
    /// deletion following an insertion, and pairing across it would emphasise unrelated text.
    #[test]
    fn additions_before_removals_are_not_paired() {
        let lines = [added("new line"), removed("old line")];
        assert!(emphasised(&lines).iter().all(|e| e.is_empty()));
    }

    /// A pair whose text is identical (whitespace-only move, say) folds to no emphasis rather than
    /// emphasising the whole line.
    #[test]
    fn an_identical_pair_is_not_emphasised() {
        let lines = [removed("same"), added("same")];
        assert!(emphasised(&lines).iter().all(|e| e.is_empty()));
    }

    #[test]
    fn human_size_switches_units_at_1024_and_keeps_bytes_exact() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GB");
        // Past the last unit it keeps scaling GB rather than inventing one.
        assert_eq!(human_size(2 * 1024 * 1024 * 1024 * 1024), "2048.0 GB");
    }

    #[test]
    fn mode_string_is_gits_octal_spelling() {
        assert_eq!(mode_string(git2::FileMode::Blob), "100644");
        assert_eq!(mode_string(git2::FileMode::BlobExecutable), "100755");
        assert_eq!(mode_string(git2::FileMode::Link), "120000");
        assert_eq!(mode_string(git2::FileMode::Commit), "160000");
        assert_eq!(mode_string(git2::FileMode::Tree), "040000");
        assert_eq!(mode_string(git2::FileMode::Unreadable), "000000");
    }

    #[test]
    fn language_of_reads_the_extension_and_shrugs_at_the_unknown() {
        assert_eq!(
            language_of(Some(&"src/main.rs".to_string())).as_deref(),
            Some("rust")
        );
        assert_eq!(language_of(None), None);
        assert_eq!(language_of(Some(&"notes.unknownext".to_string())), None);
    }
}

#[cfg(test)]
mod chrome_tests {
    use super::*;

    /// A heading starts in the same column as the code under it.
    ///
    /// It used to be set in one space from the rail the *gutter* carried. The file block's box
    /// draws that rail now, a padding cell clear of the content, so a lead-in here is a second
    /// helping of the same clearance — and every shell rendered the heading one column right of
    /// the lines it names. The column itself is each shell's arithmetic; what is pinned here is
    /// that the server sends nothing in front of the text for them to add it to.
    #[test]
    fn a_heading_carries_no_lead_in_before_its_text() {
        let mut b = PatchBuilder::default();
        b.heading("a.rs", &[]);
        let Some(Element::Row { children, band, .. }) = b.pending.first() else {
            panic!("a chrome row was queued");
        };
        assert_eq!(
            *band,
            aether_protocol::ui::Band::Chrome,
            "on the chrome band"
        );
        let Some(Element::Row { children, .. }) = children.first() else {
            panic!("chrome lays its content out as a row");
        };
        assert!(
            matches!(children.first(), Some(Element::Text { text, .. }) if text == "a.rs"),
            "the row opens with its text, not with a space: {children:?}"
        );
    }
}
