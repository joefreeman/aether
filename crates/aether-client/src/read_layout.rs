//! Grid layout for the markdown reading view (docs/markdown-view.md §2.8) — the glow-inspired
//! character-cell rendering the terminal client paints, built in the core so the layout (wrap,
//! tables, panels, markers) is written once and unit-tested off-screen. The shell maps each
//! [`SpanStyle`] to its theme and paints rows at a scroll offset; focus painting keys off each
//! row's element index.
//!
//! Coordinates are character cells (`unicode-width`), matching the editor grid. The layout is a
//! pure function of `(blocks, elements, cols)` — shells cache it by `(buffer, revision, cols)`.

use crate::markdown::{AlertKind, Block, ColAlign, Element, Inline, ListItem, Span};
use unicode_width::UnicodeWidthStr;

/// Maximum content columns — the reading measure. Wider viewports center the column
/// (docs/markdown-view.md §2.8); the shell computes the margin via [`measure`].
pub const READ_MEASURE: u16 = 92;

/// Content width and left margin for a viewport `area_cols` wide.
pub fn measure(area_cols: u16) -> (u16, u16) {
    let content = area_cols.min(READ_MEASURE).max(10);
    let margin = area_cols.saturating_sub(content) / 2;
    (content, margin)
}

/// One rendered row: styled spans plus the element it belongs to (focus painting + reveal).
#[derive(Debug, Clone, PartialEq)]
pub struct ReadRow {
    pub spans: Vec<ReadSpan>,
    /// Index into the document's element list of the block-grain element this row renders;
    /// `None` for pure separators (blank rows).
    pub element: Option<usize>,
}

impl ReadRow {
    fn blank() -> Self {
        ReadRow {
            spans: Vec::new(),
            element: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadSpan {
    pub text: String,
    pub style: SpanStyle,
    /// The *interactive* element this span belongs to (a link/image/footnote ref), for
    /// focused-link painting — distinct from the row's block-grain element.
    pub element: Option<usize>,
    /// Tree-sitter capture name for a code-block token (`"keyword"`, `"string"`, …) — the shell
    /// styles it through the same theme table the editor uses. `None` everywhere else.
    pub syntax: Option<String>,
}

/// Colour/render family of a span; the shell maps these to its theme. Font-ish attributes ride
/// alongside in [`SpanStyle`] so e.g. bold-inside-link needs no kind explosion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    Text,
    /// Heading text, by level (1-6).
    Heading(u8),
    /// Inline code (`code`-spans).
    Code,
    /// Code-block body text (the shell gives rows a panel background).
    CodeBlock,
    /// The code panel's language tag, pinned in the top pad row (it never pans with the code).
    CodeFrame,
    Link,
    /// Secondary text: image placeholders, footnote labels, front matter, raw HTML.
    Dim,
    /// List bullets / numbers / task checkboxes.
    Marker,
    /// The quote bar (`┃ `) — coloured by the alert kind when present.
    QuoteBar(Option<AlertKind>),
    /// The alert's label row text ("Note", "Warning", …).
    AlertLabel(AlertKind),
    /// Horizontal rules and heading underlines.
    Rule,
    TableBorder,
    TableHead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanStyle {
    pub kind: SpanKind,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
}

impl SpanStyle {
    fn plain(kind: SpanKind) -> Self {
        SpanStyle {
            kind,
            bold: false,
            italic: false,
            strike: false,
            underline: false,
        }
    }
}

/// Fenced-code highlights by block span start (see
/// [`crate::session::ReadView::code_highlights`]); offsets index each block's `code` string.
pub type CodeHighlights = std::collections::HashMap<u32, Vec<aether_protocol::viewport::Highlight>>;

/// Lay the document out at `cols` content columns. `elements` must be the
/// [`crate::markdown::elements`] list of the same `blocks` — rows carry indices into it;
/// `code_highlights` colours fenced code (pass an empty map for monochrome fences).
pub fn layout(
    blocks: &[Block],
    elements: &[Element],
    cols: u16,
    code_highlights: &CodeHighlights,
) -> Vec<ReadRow> {
    let mut out = Vec::new();
    let cols = cols.max(10) as usize;
    layout_blocks(
        blocks,
        elements,
        None,
        cols,
        code_highlights,
        false,
        &mut out,
    );
    // Trim the trailing separator so the document ends on content.
    while out.last().is_some_and(|r| r.spans.is_empty()) {
        out.pop();
    }
    out
}

/// The first row rendering element `idx`, for scroll reveal.
pub fn first_row_of_element(rows: &[ReadRow], idx: usize) -> Option<usize> {
    rows.iter()
        .position(|r| r.element == Some(idx) || r.spans.iter().any(|s| s.element == Some(idx)))
}

/// The widest *pannable* row of `element` — the basis for horizontal-scroll clamping and the
/// panel scrollbars. Code rows (a CodeBlock span, but not the CodeFrame tag row — that's
/// chrome, and counting it once made every tagged panel measure as overflowing) and table
/// rows (any TableBorder span) count; prose wraps and never pans.
pub fn hscroll_content_width(rows: &[ReadRow], element: usize) -> usize {
    rows.iter()
        .filter(|r| r.element == Some(element))
        .filter(|r| {
            let code = r
                .spans
                .iter()
                .any(|s| matches!(s.style.kind, SpanKind::CodeBlock))
                && !r
                    .spans
                    .iter()
                    .any(|s| matches!(s.style.kind, SpanKind::CodeFrame));
            let table = r
                .spans
                .iter()
                .any(|s| matches!(s.style.kind, SpanKind::TableBorder));
            code || table
        })
        .map(|r| r.spans.iter().map(|s| s.text.width()).sum())
        .max()
        .unwrap_or(0)
}

// ---- internals ----------------------------------------------------------------------------------

/// Exact-span lookup into the element list (both lists derive from the same parse, so a block
/// that *is* an element matches exactly).
fn element_index(elements: &[Element], span: Span) -> Option<usize> {
    elements.iter().position(|e| e.span() == span)
}

/// `in_item` = laying out a list item's blocks: a nested list then hugs its introducing line
/// instead of getting the blank separator (§2.8's "tighter inside lists" — the blank read as
/// the item ending); paragraphs of a loose item keep their separation.
fn layout_blocks(
    blocks: &[Block],
    elements: &[Element],
    inherit: Option<usize>,
    cols: usize,
    code_hl: &CodeHighlights,
    in_item: bool,
    out: &mut Vec<ReadRow>,
) {
    for block in blocks {
        let tight = in_item && matches!(block, Block::List { .. });
        if !tight && !out.is_empty() && !out.last().is_some_and(|r| r.spans.is_empty()) {
            out.push(ReadRow::blank());
        }
        let own = element_index(elements, block_span(block)).or(inherit);
        layout_block(block, elements, own, cols, code_hl, out);
    }
}

fn block_span(block: &Block) -> Span {
    match block {
        Block::Heading { span, .. }
        | Block::Paragraph { span, .. }
        | Block::Code { span, .. }
        | Block::List { span, .. }
        | Block::Quote { span, .. }
        | Block::Rule { span }
        | Block::Table { span, .. }
        | Block::Image { span, .. }
        | Block::FrontMatter { span, .. }
        | Block::FootnoteDef { span, .. }
        | Block::Html { span, .. } => *span,
    }
}

fn layout_block(
    block: &Block,
    elements: &[Element],
    own: Option<usize>,
    cols: usize,
    code_hl: &CodeHighlights,
    out: &mut Vec<ReadRow>,
) {
    match block {
        Block::Heading { level, content, .. } => {
            // Top- and second-level headings get an extra blank row above (two total, with
            // the ordinary separator) — section starts read at a glance.
            if *level <= 2 && !out.is_empty() {
                out.push(ReadRow::blank());
            }
            let style = SpanStyle {
                kind: SpanKind::Heading(*level),
                bold: true,
                ..SpanStyle::plain(SpanKind::Text)
            };
            for line in wrap_segments(&flatten(content, style, elements), cols) {
                out.push(ReadRow {
                    spans: line,
                    element: own,
                });
            }
            // H1/H2 get an underline rule, so document structure reads at a glance.
            if *level <= 2 {
                let ch = if *level == 1 { '═' } else { '─' };
                out.push(ReadRow {
                    spans: vec![ReadSpan {
                        text: ch.to_string().repeat(cols),
                        style: SpanStyle::plain(SpanKind::Rule),
                        element: None,
                        syntax: None,
                    }],
                    element: own,
                });
            }
        }
        Block::Paragraph { content, .. } => {
            let base = SpanStyle::plain(SpanKind::Text);
            for line in wrap_segments(&flatten(content, base, elements), cols) {
                out.push(ReadRow {
                    spans: line,
                    element: own,
                });
            }
        }
        Block::Code {
            language,
            code,
            span,
        } => {
            // The panel opens on its top pad row, which pins the language tag (a CodeFrame
            // span — the painter styles it and holds it fixed while the code pans; no header
            // rule, matching the GUI/web panels) and closes on the bottom pad row (which
            // hosts the horizontal scrollbar when the block overflows). The pad rows' empty
            // CodeBlock span keeps the panel background.
            let pad_span = || ReadSpan {
                text: String::new(),
                style: SpanStyle::plain(SpanKind::CodeBlock),
                element: None,
                syntax: None,
            };
            let has_tag = language.as_deref().is_some_and(|t| !t.is_empty());
            let mut top = ReadRow {
                spans: Vec::new(),
                element: own,
            };
            if let Some(tag) = language.as_deref().filter(|t| !t.is_empty()) {
                top.spans.push(ReadSpan {
                    text: format!(" {tag}"),
                    style: SpanStyle::plain(SpanKind::CodeFrame),
                    element: None,
                    syntax: None,
                });
            }
            top.spans.push(pad_span());
            out.push(top);
            // A breathing row between the tag and the code, so the tag reads as a caption
            // rather than the first line.
            if has_tag {
                out.push(ReadRow {
                    spans: vec![pad_span()],
                    element: own,
                });
            }
            // Tree-sitter tokens, when the server's snippet highlights have landed for this
            // fence — split each source line into styled runs. One row per logical line, NOT
            // width-chunked: rows may exceed the measure, and the shell clips them to its
            // per-block horizontal scroll ([`clip_spans`]).
            let hls = code_hl.get(&span.start).map(Vec::as_slice).unwrap_or(&[]);
            let mut line_start = 0usize;
            let mut widest = 0usize;
            for raw in code.split('\n') {
                widest = widest.max(raw.width());
                let spans: Vec<ReadSpan> = code_line_segments(raw, line_start, hls)
                    .into_iter()
                    .map(|(text, kind)| ReadSpan {
                        text,
                        style: SpanStyle::plain(SpanKind::CodeBlock),
                        element: None,
                        syntax: kind,
                    })
                    .collect();
                out.push(ReadRow {
                    spans,
                    element: own,
                });
                line_start += raw.len() + 1;
            }
            // A breathing row above the scrollbar, which rides the bottom pad row — only when
            // the block will overflow the painter's window (the measure minus the gutter and
            // the pad/indicator columns), i.e. exactly when a bar will be drawn.
            if widest > cols.saturating_sub(4) {
                out.push(ReadRow {
                    spans: vec![pad_span()],
                    element: own,
                });
            }
            out.push(ReadRow {
                spans: vec![pad_span()],
                element: own,
            });
        }
        Block::List {
            ordered,
            start,
            items,
            ..
        } => layout_list(*ordered, *start, items, elements, own, cols, code_hl, out),
        Block::Quote { alert, content, .. } => {
            let bar = ReadSpan {
                text: "┃ ".into(),
                style: SpanStyle::plain(SpanKind::QuoteBar(*alert)),
                element: None,
                syntax: None,
            };
            if let Some(kind) = alert {
                let label = match kind {
                    AlertKind::Note => "Note",
                    AlertKind::Tip => "Tip",
                    AlertKind::Important => "Important",
                    AlertKind::Warning => "Warning",
                    AlertKind::Caution => "Caution",
                };
                out.push(ReadRow {
                    spans: vec![
                        bar.clone(),
                        ReadSpan {
                            text: label.into(),
                            style: SpanStyle {
                                bold: true,
                                ..SpanStyle::plain(SpanKind::AlertLabel(*kind))
                            },
                            element: None,
                            syntax: None,
                        },
                    ],
                    element: own,
                });
            }
            let mut inner = Vec::new();
            layout_blocks(
                content,
                elements,
                own,
                cols.saturating_sub(2).max(8),
                code_hl,
                false,
                &mut inner,
            );
            for mut row in inner {
                let mut spans = vec![bar.clone()];
                spans.append(&mut row.spans);
                out.push(ReadRow {
                    spans,
                    element: row.element.or(own),
                });
            }
        }
        Block::Rule { .. } => out.push(ReadRow {
            spans: vec![ReadSpan {
                text: "─".repeat(cols),
                style: SpanStyle::plain(SpanKind::Rule),
                element: None,
                syntax: None,
            }],
            element: own,
        }),
        Block::Table {
            alignments,
            head,
            rows,
            ..
        } => layout_table(alignments, head, rows, elements, own, cols, out),
        Block::Image {
            alt, inner_span, ..
        } => {
            // No source path in the placeholder (inline images and links don't show theirs
            // either; `y` copies it, `Tab` reveals it). The span carries the image *target*
            // element (the inner markup span): a display image joins the `l`-opts-in model,
            // so the invert appears only once armed — the row bar alone marks position.
            let label = if alt.is_empty() { "image" } else { alt };
            out.push(ReadRow {
                spans: vec![ReadSpan {
                    text: format!("▨ [{label}]"),
                    style: SpanStyle::plain(SpanKind::Dim),
                    element: element_index(elements, *inner_span),
                    syntax: None,
                }],
                element: own,
            });
        }
        Block::FrontMatter { text, .. } => {
            for line in text.lines() {
                out.push(ReadRow {
                    spans: vec![ReadSpan {
                        text: line.to_string(),
                        style: SpanStyle {
                            italic: true,
                            ..SpanStyle::plain(SpanKind::Dim)
                        },
                        element: None,
                        syntax: None,
                    }],
                    element: own,
                });
            }
        }
        Block::FootnoteDef { label, content, .. } => {
            out.push(ReadRow {
                spans: vec![ReadSpan {
                    text: format!("[{label}]:"),
                    style: SpanStyle::plain(SpanKind::Dim),
                    element: None,
                    syntax: None,
                }],
                element: own,
            });
            let mut inner = Vec::new();
            layout_blocks(
                content,
                elements,
                own,
                cols.saturating_sub(2).max(8),
                code_hl,
                false,
                &mut inner,
            );
            for mut row in inner {
                let mut spans = vec![ReadSpan {
                    text: "  ".into(),
                    style: SpanStyle::plain(SpanKind::Text),
                    element: None,
                    syntax: None,
                }];
                spans.append(&mut row.spans);
                out.push(ReadRow {
                    spans,
                    element: row.element.or(own),
                });
            }
        }
        Block::Html { raw, .. } => {
            for line in raw.lines() {
                for chunk in chunk_width(line, cols) {
                    out.push(ReadRow {
                        spans: vec![ReadSpan {
                            text: chunk,
                            style: SpanStyle::plain(SpanKind::Dim),
                            element: None,
                            syntax: None,
                        }],
                        element: own,
                    });
                }
            }
        }
    }
}

fn layout_list(
    ordered: bool,
    start: u64,
    items: &[ListItem],
    elements: &[Element],
    inherit: Option<usize>,
    cols: usize,
    code_hl: &CodeHighlights,
    out: &mut Vec<ReadRow>,
) {
    for (i, item) in items.iter().enumerate() {
        let mut marker = if ordered {
            format!("{}. ", start + i as u64)
        } else {
            "• ".to_string()
        };
        if let Some(done) = item.checked {
            marker.push_str(if done { "☑ " } else { "☐ " });
        }
        let own = element_index(elements, item.span).or(inherit);
        let indent = " ".repeat(marker.width());
        let inner_cols = cols.saturating_sub(marker.width()).max(8);
        let mut inner = Vec::new();
        layout_blocks(
            &item.blocks,
            elements,
            own,
            inner_cols,
            code_hl,
            true,
            &mut inner,
        );
        let mut first_content = true;
        for mut row in inner {
            // Items are tight by default: skip the blank separators between an item's blocks
            // only when the item is a single paragraph tall anyway.
            if row.spans.is_empty() && first_content {
                continue;
            }
            let prefix = if first_content {
                first_content = false;
                marker.clone()
            } else {
                indent.clone()
            };
            let mut spans = vec![ReadSpan {
                text: prefix,
                style: SpanStyle::plain(SpanKind::Marker),
                element: None,
                syntax: None,
            }];
            spans.append(&mut row.spans);
            out.push(ReadRow {
                spans,
                element: row.element.or(own),
            });
        }
    }
}

fn layout_table(
    alignments: &[ColAlign],
    head: &[Vec<Inline>],
    rows: &[Vec<Vec<Inline>>],
    elements: &[Element],
    own: Option<usize>,
    cols: usize,
    out: &mut Vec<ReadRow>,
) {
    let ncols = rows
        .iter()
        .map(|r| r.len())
        .chain(std::iter::once(head.len()))
        .max()
        .unwrap_or(0);
    if ncols == 0 {
        return;
    }
    // Natural column widths from cell content, capped so one huge cell wraps inside its
    // column rather than exploding the row. No table-level shrinking: a table wider than the
    // measure renders at full width and the shell pans it horizontally, like a code block.
    const COL_CAP: usize = 48;
    let cell_style = SpanStyle::plain(SpanKind::Text);
    let head_style = SpanStyle {
        bold: true,
        ..SpanStyle::plain(SpanKind::TableHead)
    };
    let cell_segments = |cell: &Vec<Inline>, style: SpanStyle| flatten(cell, style, elements);
    let mut widths = vec![1usize; ncols];
    for (ci, w) in widths.iter_mut().enumerate() {
        let mut max = 1;
        if let Some(cell) = head.get(ci) {
            max = max.max(segments_width(&cell_segments(cell, head_style)));
        }
        for row in rows {
            if let Some(cell) = row.get(ci) {
                max = max.max(segments_width(&cell_segments(cell, cell_style)));
            }
        }
        *w = max.min(COL_CAP);
    }
    let _ = cols; // panning replaced shrink-to-fit; the measure no longer constrains tables

    let border = |l: char, m: char, r: char| -> ReadRow {
        let mut text = String::new();
        text.push(l);
        for (i, w) in widths.iter().enumerate() {
            text.push_str(&"─".repeat(w + 2));
            text.push(if i + 1 == ncols { r } else { m });
        }
        ReadRow {
            spans: vec![ReadSpan {
                text,
                style: SpanStyle::plain(SpanKind::TableBorder),
                element: None,
                syntax: None,
            }],
            element: own,
        }
    };
    let bar = |out: &mut Vec<ReadSpan>| {
        out.push(ReadSpan {
            text: "│".into(),
            style: SpanStyle::plain(SpanKind::TableBorder),
            element: None,
            syntax: None,
        })
    };

    // Lay one logical row: wrap each cell to its column, pad to the tallest.
    let emit_row = |cells: &[Vec<Inline>], style: SpanStyle, out: &mut Vec<ReadRow>| {
        let wrapped: Vec<Vec<Vec<ReadSpan>>> = (0..ncols)
            .map(|ci| {
                let segs = cells
                    .get(ci)
                    .map(|c| cell_segments(c, style))
                    .unwrap_or_default();
                let lines = wrap_segments(&segs, widths[ci].max(1));
                if lines.is_empty() {
                    vec![Vec::new()]
                } else {
                    lines
                }
            })
            .collect();
        let height = wrapped.iter().map(|c| c.len()).max().unwrap_or(1);
        for line_i in 0..height {
            let mut spans = Vec::new();
            bar(&mut spans);
            for (ci, cell_lines) in wrapped.iter().enumerate() {
                let line = cell_lines.get(line_i).cloned().unwrap_or_default();
                let used: usize = line.iter().map(|s| s.text.width()).sum();
                let pad = widths[ci].saturating_sub(used);
                let (left, right) = match alignments.get(ci) {
                    Some(ColAlign::Right) => (pad, 0),
                    Some(ColAlign::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                spans.push(ReadSpan {
                    text: " ".repeat(left + 1),
                    style: SpanStyle::plain(SpanKind::Text),
                    element: None,
                    syntax: None,
                });
                spans.extend(line);
                spans.push(ReadSpan {
                    text: " ".repeat(right + 1),
                    style: SpanStyle::plain(SpanKind::Text),
                    element: None,
                    syntax: None,
                });
                bar(&mut spans);
            }
            out.push(ReadRow {
                spans,
                element: own,
            });
        }
    };

    out.push(border('┌', '┬', '┐'));
    if !head.is_empty() {
        emit_row(head, head_style, out);
        out.push(border('├', '┼', '┤'));
    }
    for row in rows {
        emit_row(row, cell_style, out);
    }
    out.push(border('└', '┴', '┘'));
}

// ---- inline flattening + wrapping ---------------------------------------------------------------

#[derive(Debug, Clone)]
struct Segment {
    text: String,
    style: SpanStyle,
    element: Option<usize>,
}

fn segments_width(segs: &[Segment]) -> usize {
    segs.iter().map(|s| s.text.width()).sum()
}

/// Flatten inline nodes to styled segments, resolving interactive spans to element indices.
fn flatten(inlines: &[Inline], base: SpanStyle, elements: &[Element]) -> Vec<Segment> {
    let mut out = Vec::new();
    collect(inlines, base, None, elements, &mut out);
    out
}

fn collect(
    inlines: &[Inline],
    base: SpanStyle,
    element: Option<usize>,
    elements: &[Element],
    out: &mut Vec<Segment>,
) {
    for inl in inlines {
        match inl {
            Inline::Text { text } => out.push(Segment {
                text: text.clone(),
                style: base,
                element,
            }),
            Inline::Code { text } => out.push(Segment {
                text: text.clone(),
                style: SpanStyle {
                    kind: SpanKind::Code,
                    ..base
                },
                element,
            }),
            Inline::Emphasis { content } => collect(
                content,
                SpanStyle {
                    italic: true,
                    ..base
                },
                element,
                elements,
                out,
            ),
            Inline::Strong { content } => collect(
                content,
                SpanStyle { bold: true, ..base },
                element,
                elements,
                out,
            ),
            Inline::Strikethrough { content } => collect(
                content,
                SpanStyle {
                    strike: true,
                    ..base
                },
                element,
                elements,
                out,
            ),
            Inline::Link { content, span, .. } => {
                let idx = element_index(elements, *span);
                collect(
                    content,
                    SpanStyle {
                        kind: SpanKind::Link,
                        underline: true,
                        ..base
                    },
                    idx.or(element),
                    elements,
                    out,
                );
            }
            Inline::Image { alt, span, .. } => {
                let idx = element_index(elements, *span);
                let label = if alt.is_empty() { "image" } else { alt };
                out.push(Segment {
                    text: format!("▨ [{label}]"),
                    style: SpanStyle {
                        kind: SpanKind::Dim,
                        ..base
                    },
                    element: idx.or(element),
                });
            }
            Inline::FootnoteRef { label, span } => {
                let idx = element_index(elements, *span);
                out.push(Segment {
                    text: format!("[{label}]"),
                    style: SpanStyle {
                        kind: SpanKind::Dim,
                        ..base
                    },
                    element: idx.or(element),
                });
            }
            // A hard break forces a wrap point; encode it as a zero-width sentinel the wrapper
            // understands.
            Inline::HardBreak => out.push(Segment {
                text: "\n".into(),
                style: base,
                element,
            }),
        }
    }
}

/// Greedy word-wrap over styled segments, preserving styles; words longer than `width` are
/// hard-broken; `\n` segments (hard breaks) force a new line.
fn wrap_segments(segs: &[Segment], width: usize) -> Vec<Vec<ReadSpan>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<ReadSpan>> = Vec::new();
    let mut cur: Vec<ReadSpan> = Vec::new();
    let mut cur_w = 0usize;

    let flush = |cur: &mut Vec<ReadSpan>, cur_w: &mut usize, lines: &mut Vec<Vec<ReadSpan>>| {
        // Trim the trailing space run so lines don't end in padding.
        while let Some(last) = cur.last_mut() {
            let trimmed = last.text.trim_end().to_string();
            if trimmed.is_empty() {
                cur.pop();
            } else {
                last.text = trimmed;
                break;
            }
        }
        lines.push(std::mem::take(cur));
        *cur_w = 0;
    };

    for seg in segs {
        if seg.text == "\n" {
            flush(&mut cur, &mut cur_w, &mut lines);
            continue;
        }
        // Split into words, keeping the whitespace runs as separate tokens.
        let mut rest = seg.text.as_str();
        while !rest.is_empty() {
            let split = if rest.starts_with(char::is_whitespace) {
                rest.find(|c: char| !c.is_whitespace())
                    .unwrap_or(rest.len())
            } else {
                rest.find(char::is_whitespace).unwrap_or(rest.len())
            };
            let (token, tail) = rest.split_at(split);
            rest = tail;
            let is_space = token.starts_with(char::is_whitespace);
            let mut token_w = token.width();
            if cur_w + token_w > width && !is_space && cur_w > 0 {
                flush(&mut cur, &mut cur_w, &mut lines);
            }
            // A word longer than the whole width hard-breaks.
            let mut token = token.to_string();
            while token_w > width {
                let mut take = String::new();
                let mut w = 0;
                for c in token.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    if w + cw > width {
                        break;
                    }
                    take.push(c);
                    w += cw;
                }
                cur.push(ReadSpan {
                    text: take.clone(),
                    style: seg.style,
                    element: seg.element,
                    syntax: None,
                });
                flush(&mut cur, &mut cur_w, &mut lines);
                token = token[take.len()..].to_string();
                token_w = token.width();
            }
            if token.is_empty() {
                continue;
            }
            if is_space && cur_w == 0 && !cur.is_empty() {
                continue; // don't start a wrapped line with the separator
            }
            if is_space && cur.is_empty() {
                continue; // nor the very first line
            }
            cur.push(ReadSpan {
                text: token.clone(),
                style: seg.style,
                element: seg.element,
                syntax: None,
            });
            cur_w += token.width();
        }
    }
    if !cur.is_empty() {
        flush(&mut cur, &mut cur_w, &mut lines);
    }
    lines
}

/// Split one code line into `(text, capture)` runs from the fence's snippet highlights.
/// `line_start` is the line's byte offset within the snippet; highlights are sorted and
/// non-overlapping (the server run-length-encodes), gaps come back uncaptured.
fn code_line_segments(
    line: &str,
    line_start: usize,
    hls: &[aether_protocol::viewport::Highlight],
) -> Vec<(String, Option<String>)> {
    if line.is_empty() {
        return vec![(String::new(), None)];
    }
    let line_end = line_start + line.len();
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut pos = line_start;
    for h in hls {
        let (s, e) = (h.start as usize, h.end as usize);
        if e <= line_start || s >= line_end {
            continue;
        }
        let (s, e) = (s.max(line_start), e.min(line_end));
        if s > pos {
            out.push((line[pos - line_start..s - line_start].to_string(), None));
        }
        out.push((
            line[s - line_start..e - line_start].to_string(),
            Some(h.kind.clone()),
        ));
        pos = e;
    }
    if pos < line_end {
        out.push((line[pos - line_start..].to_string(), None));
    }
    out
}

/// The visible slice of a styled row: skip `skip` display columns, keep the next `width` —
/// the shell's per-block horizontal scroll window over unchunked code rows. Styles and
/// tree-sitter captures survive the cut; a wide glyph straddling either edge renders as
/// space padding for its visible columns, so alignment never shifts.
pub fn clip_spans(spans: &[ReadSpan], skip: usize, width: usize) -> Vec<ReadSpan> {
    let end = skip + width;
    let mut out = Vec::new();
    let mut pos = 0usize;
    for span in spans {
        if pos >= end {
            break;
        }
        let mut piece = String::new();
        for c in span.text.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            let (start, stop) = (pos, pos + cw);
            pos = stop;
            if stop <= skip {
                continue; // fully left of the window
            }
            if start >= end {
                break; // fully right — nothing more of this span is visible
            }
            if start < skip || stop > end {
                let visible = stop.min(end) - start.max(skip);
                piece.extend(std::iter::repeat(' ').take(visible));
            } else {
                piece.push(c);
            }
        }
        if !piece.is_empty() {
            out.push(ReadSpan {
                text: piece,
                style: span.style,
                element: span.element,
                syntax: span.syntax.clone(),
            });
        }
    }
    out
}

/// Hard chunk a line into width-sized pieces (code/HTML bodies — no word wrap).
fn chunk_width(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut w = 0;
    for c in line.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(c);
        w += cw;
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{elements, parse};

    fn rows_text(rows: &[ReadRow]) -> Vec<String> {
        rows.iter()
            .map(|r| r.spans.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn heading_gets_underline_and_paragraph_wraps() {
        let md = "# Title\n\naaa bbb ccc ddd eee\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 11, &Default::default());
        let text = rows_text(&rows);
        assert_eq!(text[0], "Title");
        assert_eq!(text[1], "═".repeat(11));
        assert!(text[2].is_empty(), "blank separator");
        assert_eq!(text[3], "aaa bbb ccc");
        assert_eq!(text[4], "ddd eee");
        // The paragraph rows carry the paragraph's element; the heading rows the heading's.
        assert_eq!(rows[0].element, Some(0));
        assert_eq!(rows[3].element, Some(1));
        assert_eq!(rows[4].element, Some(1));
    }

    #[test]
    fn list_markers_and_hanging_indent() {
        let md = "- first item wraps here\n- [x] done\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 14, &Default::default());
        let text = rows_text(&rows);
        assert_eq!(text[0], "• first item");
        assert_eq!(text[1], "  wraps here");
        assert_eq!(text[2], "• ☑ done");
        // Item rows carry item elements (0 and 1 — the items are the only elements).
        assert_eq!(rows[0].element, Some(0));
        assert_eq!(rows[2].element, Some(1));
    }

    #[test]
    fn table_draws_box_borders_and_aligns() {
        let md = "| Name | N |\n|:-----|--:|\n| Ada | 36 |\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 40, &Default::default());
        let text = rows_text(&rows);
        assert_eq!(text[0], "┌──────┬────┐");
        assert_eq!(text[1], "│ Name │  N │"); // N right-aligned
        assert_eq!(text[2], "├──────┼────┤");
        assert_eq!(text[3], "│ Ada  │ 36 │");
        assert_eq!(text[4], "└──────┴────┘");
    }

    #[test]
    fn wide_table_keeps_natural_width_for_panning() {
        let md = "| words |\n|---|\n| several words stay on one line |\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 16, &Default::default());
        let text = rows_text(&rows);
        // No shrink-to-fit: the table renders at its natural width (wider than the measure)
        // as single-line cells, and the shell pans it like a code block.
        assert!(
            text[0].width() > 16,
            "table overflows the measure: {}",
            text[0]
        );
        assert_eq!(text.len(), 5, "cells stay single rows");
        let element = rows[0].element.expect("table rows carry their element");
        assert_eq!(hscroll_content_width(&rows, element), text[0].width());
    }

    #[test]
    fn huge_table_cell_wraps_within_column_cap() {
        // A single cell longer than the 48-col column cap hard-wraps inside its column
        // instead of stretching the table without bound.
        let long = "x".repeat(60);
        let md = format!("| head |\n|---|\n| {long} |\n");
        let blocks = parse(&md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 30, &Default::default());
        let text = rows_text(&rows);
        assert!(
            text.iter().all(|t| t.width() <= 48 + 4),
            "column capped at 48 plus frame overhead"
        );
        // 2 borders + head + separator + 2 wrapped body rows + bottom border.
        assert!(text.len() > 5, "long cell wrapped over multiple rows");
    }

    #[test]
    fn quote_bar_and_alert_label() {
        let md = "> [!WARNING]\n> Careful now.\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 30, &Default::default());
        let text = rows_text(&rows);
        assert_eq!(text[0], "┃ Warning");
        assert_eq!(text[1], "┃ Careful now.");
        assert!(matches!(
            rows[0].spans[0].style.kind,
            SpanKind::QuoteBar(Some(AlertKind::Warning))
        ));
    }

    #[test]
    fn code_block_tag_rides_the_top_pad_row() {
        let md = "```rust\nfn x() {}\n```\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 20, &Default::default());
        let text = rows_text(&rows);
        // No header rule: the panel opens on its top pad row, which pins the language tag
        // as a CodeFrame span the painter holds fixed while the code pans, with a breathing
        // row between the tag and the code.
        assert_eq!(text[0], " rust");
        assert_eq!(rows[0].spans[0].style.kind, SpanKind::CodeFrame);
        assert_eq!(rows[0].spans[1].style.kind, SpanKind::CodeBlock);
        assert_eq!(text[1], "");
        assert_eq!(rows[1].spans[0].style.kind, SpanKind::CodeBlock);
        assert_eq!(text[2], "fn x() {}");
        assert_eq!(rows[2].spans[0].style.kind, SpanKind::CodeBlock);
        // Fitting block: no pre-bar breathing row — the panel closes on its bottom pad.
        assert_eq!(text[3], "");
        assert_eq!(rows.len(), 4);
        // The tag row is chrome, not content: it doesn't count toward the scroll basis, so a
        // fitting block draws no scrollbar (the old header rule spanned the full measure and
        // made every tagged panel "overflow").
        let element = rows[2].element.expect("code rows carry their element");
        assert_eq!(hscroll_content_width(&rows, element), "fn x() {}".len());
    }

    #[test]
    fn long_code_lines_stay_single_rows() {
        // A 60-col line at a 20-col measure: ONE row wider than the measure — the shell
        // scrolls it horizontally instead of chunking it into stacked rows.
        let line = "x".repeat(60);
        let md = format!("```\n{line}\nshort\n```\n");
        let blocks = parse(&md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 20, &Default::default());
        let text = rows_text(&rows);
        // Overflowing block: pad, the two code lines, a breathing row above the scrollbar,
        // then the bottom pad the bar rides.
        assert_eq!(text.len(), 5, "got {text:?}");
        assert_eq!(text[1], line);
        assert_eq!(text[2], "short");
        assert_eq!(text[3], "");
        assert_eq!(text[4], "");
    }

    #[test]
    fn clip_spans_windows_styled_rows() {
        let span = |t: &str, syntax: Option<&str>| ReadSpan {
            text: t.into(),
            style: SpanStyle::plain(SpanKind::CodeBlock),
            element: None,
            syntax: syntax.map(str::to_string),
        };
        let row = vec![span("let ", Some("keyword")), span("x = 1;", None)];
        // Window over the middle: styles and captures survive the cut.
        let clipped = clip_spans(&row, 2, 5);
        let text: Vec<&str> = clipped.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(text, vec!["t ", "x ="]);
        assert_eq!(clipped[0].syntax.as_deref(), Some("keyword"));
        assert_eq!(clipped[1].syntax, None);
        // Fully left / fully right of the window.
        assert!(clip_spans(&row, 20, 5).is_empty());
        assert_eq!(clip_spans(&row, 0, 100).len(), 2);
        // A wide glyph straddling the left edge pads with a space instead of shifting.
        let wide = vec![span("界x", None)];
        let clipped = clip_spans(&wide, 1, 2);
        assert_eq!(clipped[0].text, " x");
    }

    #[test]
    fn link_spans_carry_their_element_for_focus_painting() {
        let md = "See [docs](https://x.y) now.\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 40, &Default::default());
        let link_span = rows[0]
            .spans
            .iter()
            .find(|s| s.text == "docs")
            .expect("link text present");
        assert_eq!(link_span.style.kind, SpanKind::Link);
        assert!(link_span.style.underline);
        // Element 1 is the link (0 = the paragraph).
        assert_eq!(link_span.element, Some(1));
        assert_eq!(first_row_of_element(&rows, 1), Some(0));
    }

    #[test]
    fn hard_break_forces_a_line_break() {
        let md = "one two  \nthree\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 40, &Default::default());
        let text = rows_text(&rows);
        assert_eq!(text, vec!["one two", "three"]);
    }

    #[test]
    fn fence_highlights_split_code_rows_into_tokens() {
        let md = "```rust\nfn x() {}\n```\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let mut hl = CodeHighlights::default();
        // The fence starts at byte 0; "fn" is a keyword in its `code` string.
        hl.insert(
            0,
            vec![aether_protocol::viewport::Highlight {
                start: 0,
                end: 2,
                kind: "keyword".into(),
            }],
        );
        let rows = layout(&blocks, &els, 40, &hl);
        // Row 0 is the top pad row (carrying the tag), row 1 its breathing row; row 2 the
        // code line, split at the token boundary.
        let code_row = &rows[2];
        assert_eq!(code_row.spans[0].text, "fn");
        assert_eq!(code_row.spans[0].syntax.as_deref(), Some("keyword"));
        assert_eq!(code_row.spans[1].text, " x() {}");
        assert_eq!(code_row.spans[1].syntax, None);
    }

    #[test]
    fn measure_centers_wide_viewports() {
        assert_eq!(measure(200), (92, 54));
        assert_eq!(measure(80), (80, 0));
    }

    #[test]
    fn h2_gets_extra_blank_above_and_image_span_carries_element() {
        let md = "Intro.\n\n## Section\n\n![d](i.png)\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 40, &Default::default());
        let text = rows_text(&rows);
        // Intro, ordinary separator, the heading's extra blank, then the heading.
        assert_eq!(text[0], "Intro.");
        assert!(
            text[1].is_empty() && text[2].is_empty(),
            "two blank rows above the H2, got {text:?}"
        );
        assert_eq!(text[3], "Section");
        // The display image's placeholder: alt only (no source path — links don't show
        // theirs either). The row carries the Block element (bar); the span carries the
        // image *target* element, distinct — the invert appears only once `l` arms it.
        let img_row = rows
            .iter()
            .find(|r| r.spans.iter().any(|s| s.text.contains('▨')))
            .expect("image placeholder row");
        assert!(img_row.element.is_some());
        assert_eq!(img_row.spans[0].text, "▨ [d]");
        assert!(img_row.spans[0].element.is_some());
        assert_ne!(img_row.spans[0].element, img_row.element);
    }

    #[test]
    fn nested_list_hugs_its_parent_item() {
        let md = "- parent intro\n  - child one\n  - child two\n- next\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 40, &Default::default());
        assert_eq!(
            rows_text(&rows),
            vec!["• parent intro", "  • child one", "  • child two", "• next"],
            "no blank separator before a nested list"
        );
        // A loose item's paragraphs keep their separation.
        let md2 = "- first\n\n  second para\n- next\n";
        let blocks2 = parse(md2);
        let els2 = elements(&blocks2);
        let rows2 = layout(&blocks2, &els2, 40, &Default::default());
        let text2 = rows_text(&rows2);
        assert_eq!(text2[0], "• first");
        assert!(
            text2[1].trim().is_empty(),
            "loose paragraphs stay separated"
        );
        assert_eq!(text2[2], "  second para");
    }

    #[test]
    fn untagged_code_block_has_no_header_rule() {
        let md = "```\nplain text\n```\n";
        let blocks = parse(md);
        let els = elements(&blocks);
        let rows = layout(&blocks, &els, 20, &Default::default());
        let text = rows_text(&rows);
        // No header row without a language tag — the panel opens on its top pad row.
        assert_eq!(text[0], "");
        assert_eq!(rows[0].spans[0].style.kind, SpanKind::CodeBlock);
        assert_eq!(text[1], "plain text");
    }
}
