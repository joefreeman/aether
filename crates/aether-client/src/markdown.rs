//! Markdown → AST for the reading view and hover popovers.
//!
//! Markdown is parsed once here (with `pulldown-cmark`, a real CommonMark parser) into a small
//! serializable tree, so every shell renders the *same* structure — the web to DOM, the native
//! client to iced widgets, the terminal to styled lines — instead of each re-implementing a
//! parser. Two consumers share the AST:
//!
//! - **LSP hover popovers**: the original user; content arrives as markdown text, source
//!   positions are irrelevant.
//! - **The markdown reading view** (docs/markdown-view.md): parses whole buffers. Every block
//!   and interactive inline carries its **source byte span**, the foundation of the read view's
//!   source map — focus derivation, outline jumps and edit-toggle fidelity all resolve through
//!   those spans. The flattened [`Element`] list built by [`elements`] is the navigable form.
//!
//! `Serialize` is for the wasm boundary (the web shell renders the AST as JSON); the native and
//! terminal shells consume the Rust values directly.
//!
//! Offset fidelity: pulldown's offset iterator was probed against the awkward node kinds (table
//! cells, tight lists, task markers, setext headings, footnotes, front matter, smart
//! punctuation) before this design was committed — ranges are exact for everything the source
//! map needs. The one quirk: an *indented* code block's span starts after the first line's
//! indent. Harmless at element grain.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde::Serialize;
use std::collections::HashMap;

/// A byte range into the source the AST was parsed from. Start-inclusive, end-exclusive.
/// Hover ASTs carry spans too (the parse is shared) — they're just never queried there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn contains(&self, pos: u32) -> bool {
        pos >= self.start && pos < self.end
    }

    fn len(&self) -> u32 {
        self.end.saturating_sub(self.start)
    }
}

/// A block-level node.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Heading {
        level: u8,
        content: Vec<Inline>,
        span: Span,
    },
    Paragraph {
        content: Vec<Inline>,
        span: Span,
    },
    Code {
        language: Option<String>,
        code: String,
        span: Span,
    },
    List {
        ordered: bool,
        /// First item's number for an ordered list (`3.` lists start at 3); 1 for unordered.
        start: u64,
        items: Vec<ListItem>,
        span: Span,
    },
    Quote {
        /// GFM alert kind (`> [!NOTE]` …); `None` for a plain blockquote.
        #[serde(skip_serializing_if = "Option::is_none")]
        alert: Option<AlertKind>,
        content: Vec<Block>,
        span: Span,
    },
    Rule {
        span: Span,
    },
    Table {
        alignments: Vec<ColAlign>,
        head: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
        span: Span,
    },
    /// A paragraph that is exactly one image, promoted to block form (a display image).
    Image {
        src: String,
        alt: String,
        span: Span,
        /// The image markup itself (the promoted inline's span — no trailing whitespace):
        /// the Enter-target span, leaving the block a *rest byte* so `l` opts into the image
        /// like it opts into links (docs/markdown-view.md §2.3).
        inner_span: Span,
    },
    /// YAML front matter (`---` fenced, document start). Raw text; not interpreted.
    FrontMatter {
        text: String,
        span: Span,
    },
    FootnoteDef {
        label: String,
        content: Vec<Block>,
        span: Span,
    },
    /// A raw HTML block. Rendered as a literal panel — never interpreted as markup.
    Html {
        raw: String,
        span: Span,
    },
}

impl Block {
    /// The block's source span (every variant carries one).
    pub fn span(&self) -> Span {
        match self {
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
}

/// One item of a [`Block::List`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListItem {
    /// `Some(done)` for a task-list item (`- [x]` / `- [ ]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    pub blocks: Vec<Block>,
    pub span: Span,
}

/// GFM blockquote alert kinds (`> [!NOTE]` …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

/// Table column alignment, from the delimiter row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ColAlign {
    None,
    Left,
    Center,
    Right,
}

/// An inline (span-level) node. Interactive inlines (link, image, footnote ref) carry source
/// spans — they're focusable elements in the reading view; plain text runs don't (match
/// painting, which needs text-run spans, is a later phase — docs/markdown-view.md §10 step 5).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Inline {
    Text { text: String },
    Code { text: String },
    Emphasis { content: Vec<Inline> },
    Strong { content: Vec<Inline> },
    Strikethrough { content: Vec<Inline> },
    Link { href: String, content: Vec<Inline>, span: Span },
    Image { src: String, alt: String, span: Span },
    FootnoteRef { label: String, span: Span },
    /// An explicit line break (trailing `  ` or `\`). Soft breaks flow as spaces.
    HardBreak,
}

/// The extension set both consumers parse with (docs/markdown-view.md §2.2). Smart punctuation
/// is display-only prettiness — the replacement text still carries exact source ranges.
fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_SMART_PUNCTUATION
        | Options::ENABLE_GFM
}

/// Parse Markdown into the AST.
pub fn parse(md: &str) -> Vec<Block> {
    let mut b = Builder::default();
    for (ev, range) in Parser::new_ext(md, options()).into_offset_iter() {
        let span = Span {
            start: range.start as u32,
            end: range.end as u32,
        };
        match ev {
            Event::Start(tag) => b.start(tag, span),
            Event::End(te) => b.end(te),
            Event::Text(s) => b.text(&s),
            Event::Code(s) => b.push_inline(Inline::Code {
                text: s.to_string(),
            }),
            // A soft break separates words in flowed text; a hard break is kept explicit.
            Event::SoftBreak => b.text(" "),
            Event::HardBreak => b.push_inline(Inline::HardBreak),
            Event::Rule => b.push_block(Block::Rule { span }),
            Event::TaskListMarker(done) => b.task_marker(done),
            Event::FootnoteReference(label) => b.push_inline(Inline::FootnoteRef {
                label: label.to_string(),
                span,
            }),
            // Raw HTML: block-level chunks collect into the enclosing Html block; inline HTML
            // degrades to its literal text (never interpreted).
            Event::Html(s) => b.html(&s),
            Event::InlineHtml(s) => b.text(&s),
            // Math is not enabled; anything else degrades to nothing.
            _ => {}
        }
    }
    promote_lone_images(&mut b.out);
    b.out
}

/// Replace any paragraph whose content is exactly one image with [`Block::Image`], recursively —
/// a display image rather than a run of text (docs/markdown-view.md §2.2).
fn promote_lone_images(blocks: &mut Vec<Block>) {
    for block in blocks.iter_mut() {
        match block {
            Block::Paragraph { content, span } => {
                if let [Inline::Image {
                    src,
                    alt,
                    span: inner,
                }] = content.as_slice()
                {
                    *block = Block::Image {
                        src: src.clone(),
                        alt: alt.clone(),
                        span: *span,
                        inner_span: *inner,
                    };
                }
            }
            Block::Quote { content, .. } | Block::FootnoteDef { content, .. } => {
                promote_lone_images(content)
            }
            Block::List { items, .. } => {
                for item in items {
                    promote_lone_images(&mut item.blocks);
                }
            }
            _ => {}
        }
    }
}

/// An in-progress container on the parse stack.
enum Frame {
    Paragraph(Vec<Inline>, Span),
    Heading(u8, Vec<Inline>, Span),
    Emphasis(Vec<Inline>),
    Strong(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Link(String, Vec<Inline>, Span),
    /// Image alt text collects as inlines and flattens to a string on close.
    Image(String, Vec<Inline>, Span),
    /// Transparent inline container for any unmodelled inline tag. Its children flow into the
    /// parent on close.
    Transparent(Vec<Inline>),
    List {
        ordered: bool,
        start: u64,
        items: Vec<ListItem>,
        span: Span,
    },
    Item {
        blocks: Vec<Block>,
        checked: Option<bool>,
        span: Span,
    },
    Quote {
        alert: Option<AlertKind>,
        content: Vec<Block>,
        span: Span,
    },
    Code(Option<String>, String, Span),
    Table {
        alignments: Vec<ColAlign>,
        head: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
        span: Span,
    },
    /// A table head or body row (cells collect in order). Which one is decided by the closing
    /// `TagEnd` — pulldown emits head cells directly under `TableHead`.
    TableRow(Vec<Vec<Inline>>),
    TableCell(Vec<Inline>),
    FootnoteDef(String, Vec<Block>, Span),
    Html(String, Span),
    FrontMatter(String, Span),
}

#[derive(Default)]
struct Builder {
    stack: Vec<Frame>,
    out: Vec<Block>,
}

impl Builder {
    /// The inline list of the innermost inline-collecting frame, if any.
    fn inlines_mut(&mut self) -> Option<&mut Vec<Inline>> {
        match self.stack.last_mut() {
            Some(
                Frame::Paragraph(v, _)
                | Frame::Heading(_, v, _)
                | Frame::Emphasis(v)
                | Frame::Strong(v)
                | Frame::Strikethrough(v)
                | Frame::Link(_, v, _)
                | Frame::Image(_, v, _)
                | Frame::Transparent(v)
                | Frame::TableCell(v),
            ) => Some(v),
            _ => None,
        }
    }

    /// The inline list to append to: the innermost inline frame, or a trailing paragraph opened
    /// in the current block context (a tight list item / blockquote emits inline text with no
    /// wrapping paragraph, so we synthesise one — spanned to its container, which is the
    /// element-grain unit anyway).
    fn inline_target(&mut self) -> &mut Vec<Inline> {
        if self.inlines_mut().is_some() {
            return self.inlines_mut().expect("inline frame present");
        }
        let (blocks, container_span) = match self.stack.last_mut() {
            Some(
                Frame::Item {
                    blocks, span: sp, ..
                }
                | Frame::Quote {
                    content: blocks,
                    span: sp,
                    ..
                }
                | Frame::FootnoteDef(_, blocks, sp),
            ) => (blocks, *sp),
            _ => (&mut self.out, Span::default()),
        };
        if !matches!(blocks.last(), Some(Block::Paragraph { .. })) {
            blocks.push(Block::Paragraph {
                content: Vec::new(),
                span: container_span,
            });
        }
        match blocks.last_mut() {
            Some(Block::Paragraph { content, .. }) => content,
            _ => unreachable!(),
        }
    }

    fn push_inline(&mut self, inl: Inline) {
        let target = self.inline_target();
        // Coalesce adjacent text (soft breaks split it into runs) for a tidier tree.
        if let Inline::Text { text } = &inl {
            if let Some(Inline::Text { text: prev }) = target.last_mut() {
                prev.push_str(text);
                return;
            }
        }
        target.push(inl);
    }

    /// Append a block to the innermost block-collecting frame, else the root.
    fn push_block(&mut self, blk: Block) {
        match self.stack.last_mut() {
            Some(
                Frame::Item { blocks, .. }
                | Frame::Quote {
                    content: blocks, ..
                }
                | Frame::FootnoteDef(_, blocks, _),
            ) => blocks.push(blk),
            _ => self.out.push(blk),
        }
    }

    fn text(&mut self, s: &str) {
        match self.stack.last_mut() {
            Some(Frame::Code(_, code, _)) => code.push_str(s),
            Some(Frame::FrontMatter(text, _)) => text.push_str(s),
            Some(Frame::Html(raw, _)) => raw.push_str(s),
            _ => self.push_inline(Inline::Text {
                text: s.to_string(),
            }),
        }
    }

    fn html(&mut self, s: &str) {
        match self.stack.last_mut() {
            Some(Frame::Html(raw, _)) => raw.push_str(s),
            // A stray HTML chunk outside an HtmlBlock: keep its text.
            _ => self.text(s),
        }
    }

    fn task_marker(&mut self, done: bool) {
        if let Some(Frame::Item { checked, .. }) = self.stack.last_mut() {
            *checked = Some(done);
        }
    }

    fn start(&mut self, tag: Tag, span: Span) {
        let frame = match tag {
            Tag::Paragraph => Frame::Paragraph(Vec::new(), span),
            Tag::Heading { level, .. } => Frame::Heading(level as u8, Vec::new(), span),
            Tag::Emphasis => Frame::Emphasis(Vec::new()),
            Tag::Strong => Frame::Strong(Vec::new()),
            Tag::Strikethrough => Frame::Strikethrough(Vec::new()),
            Tag::Link { dest_url, .. } => Frame::Link(dest_url.to_string(), Vec::new(), span),
            Tag::Image { dest_url, .. } => Frame::Image(dest_url.to_string(), Vec::new(), span),
            Tag::List(start) => Frame::List {
                ordered: start.is_some(),
                start: start.unwrap_or(1),
                items: Vec::new(),
                span,
            },
            Tag::Item => Frame::Item {
                blocks: Vec::new(),
                checked: None,
                span,
            },
            Tag::BlockQuote(kind) => Frame::Quote {
                alert: kind.map(|k| match k {
                    pulldown_cmark::BlockQuoteKind::Note => AlertKind::Note,
                    pulldown_cmark::BlockQuoteKind::Tip => AlertKind::Tip,
                    pulldown_cmark::BlockQuoteKind::Important => AlertKind::Important,
                    pulldown_cmark::BlockQuoteKind::Warning => AlertKind::Warning,
                    pulldown_cmark::BlockQuoteKind::Caution => AlertKind::Caution,
                }),
                content: Vec::new(),
                span,
            },
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(l) if !l.is_empty() => Some(l.to_string()),
                    _ => None,
                };
                Frame::Code(lang, String::new(), span)
            }
            Tag::Table(aligns) => Frame::Table {
                alignments: aligns
                    .iter()
                    .map(|a| match a {
                        pulldown_cmark::Alignment::None => ColAlign::None,
                        pulldown_cmark::Alignment::Left => ColAlign::Left,
                        pulldown_cmark::Alignment::Center => ColAlign::Center,
                        pulldown_cmark::Alignment::Right => ColAlign::Right,
                    })
                    .collect(),
                head: Vec::new(),
                rows: Vec::new(),
                span,
            },
            Tag::TableHead | Tag::TableRow => Frame::TableRow(Vec::new()),
            Tag::TableCell => Frame::TableCell(Vec::new()),
            Tag::FootnoteDefinition(label) => {
                Frame::FootnoteDef(label.to_string(), Vec::new(), span)
            }
            Tag::HtmlBlock => Frame::Html(String::new(), span),
            Tag::MetadataBlock(_) => Frame::FrontMatter(String::new(), span),
            // Anything unmodelled: keep its inline text, drop the wrapper.
            _ => Frame::Transparent(Vec::new()),
        };
        self.stack.push(frame);
    }

    fn end(&mut self, te: TagEnd) {
        let Some(frame) = self.stack.pop() else {
            return;
        };
        match frame {
            Frame::Paragraph(content, span) => self.push_block(Block::Paragraph { content, span }),
            Frame::Heading(level, content, span) => self.push_block(Block::Heading {
                level,
                content,
                span,
            }),
            Frame::Emphasis(content) => self.push_inline(Inline::Emphasis { content }),
            Frame::Strong(content) => self.push_inline(Inline::Strong { content }),
            Frame::Strikethrough(content) => self.push_inline(Inline::Strikethrough { content }),
            Frame::Link(href, content, span) => self.push_inline(Inline::Link {
                href,
                content,
                span,
            }),
            Frame::Image(src, content, span) => {
                let alt = inlines_text(&content);
                self.push_inline(Inline::Image { src, alt, span });
            }
            // Transparent: flow children to the parent (push_inline opens a paragraph if needed).
            Frame::Transparent(content) => {
                for inl in content {
                    self.push_inline(inl);
                }
            }
            Frame::List {
                ordered,
                start,
                items,
                span,
            } => self.push_block(Block::List {
                ordered,
                start,
                items,
                span,
            }),
            Frame::Item {
                blocks,
                checked,
                span,
            } => {
                if let Some(Frame::List { items, .. }) = self.stack.last_mut() {
                    items.push(ListItem {
                        checked,
                        blocks,
                        span,
                    });
                }
            }
            Frame::Quote {
                alert,
                content,
                span,
            } => self.push_block(Block::Quote {
                alert,
                content,
                span,
            }),
            Frame::Code(language, code, span) => {
                // pulldown emits a trailing newline after the last code line — drop it.
                let code = code.strip_suffix('\n').map(str::to_string).unwrap_or(code);
                self.push_block(Block::Code {
                    language,
                    code,
                    span,
                });
            }
            Frame::Table {
                alignments,
                head,
                rows,
                span,
            } => self.push_block(Block::Table {
                alignments,
                head,
                rows,
                span,
            }),
            Frame::TableRow(cells) => {
                if let Some(Frame::Table { head, rows, .. }) = self.stack.last_mut() {
                    if te == TagEnd::TableHead {
                        *head = cells;
                    } else {
                        rows.push(cells);
                    }
                }
            }
            Frame::TableCell(cells) => {
                if let Some(Frame::TableRow(row)) = self.stack.last_mut() {
                    row.push(cells);
                }
            }
            Frame::FootnoteDef(label, content, span) => self.push_block(Block::FootnoteDef {
                label,
                content,
                span,
            }),
            Frame::Html(raw, span) => {
                let raw = raw.strip_suffix('\n').map(str::to_string).unwrap_or(raw);
                self.push_block(Block::Html { raw, span });
            }
            Frame::FrontMatter(text, span) => {
                let text = text.trim().to_string();
                self.push_block(Block::FrontMatter { text, span });
            }
        }
    }
}

/// Flatten inlines to their plain text (image alt text, heading slugs, plain copies).
fn inlines_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    push_inlines_plain(inlines, &mut out);
    out
}

// ---- the element list ---------------------------------------------------------------------------

/// A navigable element of the rendered document, in document order (outer before inner at equal
/// starts). The reading view's focus model runs entirely over this list: block-grain elements
/// are `j`/`k` stops, interactive ones are `Tab` stops and `Enter` targets, headings serve the
/// `o`/`Alt-o` motion and anchor-link resolution (docs/markdown-view.md §1.3, §2.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Element {
    /// A non-heading block-grain stop: paragraph, code block, rule, table, quote, footnote
    /// definition, front matter, HTML block.
    Block { span: Span },
    Heading {
        span: Span,
        level: u8,
        /// GitHub-style anchor slug, deduplicated document-wide (`#foo`, `#foo-1`, …).
        slug: String,
        text: String,
    },
    /// A list item (nested items are their own elements; a loose item's inner paragraphs are not).
    Item {
        span: Span,
        #[serde(skip_serializing_if = "Option::is_none")]
        checked: Option<bool>,
    },
    Link { span: Span, href: String },
    /// An image target — always interactive-grain, never a `j`/`k` stop: a *display* image's
    /// block identity is a separate [`Element::Block`] over the paragraph span, so `l` opts
    /// into the image exactly like it opts into a link.
    Image { span: Span, src: String },
    FootnoteRef { span: Span, label: String },
}

impl Element {
    pub fn span(&self) -> Span {
        match self {
            Element::Block { span }
            | Element::Heading { span, .. }
            | Element::Item { span, .. }
            | Element::Link { span, .. }
            | Element::Image { span, .. }
            | Element::FootnoteRef { span, .. } => *span,
        }
    }

    /// A `j`/`k` reading stop.
    pub fn is_block(&self) -> bool {
        matches!(
            self,
            Element::Block { .. } | Element::Heading { .. } | Element::Item { .. }
        )
    }

    /// A `Tab` stop / `Enter` target.
    pub fn is_interactive(&self) -> bool {
        matches!(
            self,
            Element::Link { .. } | Element::Image { .. } | Element::FootnoteRef { .. }
        )
    }
}

/// Flatten the AST into the element list. Construction order is document order: containers
/// before their contents, inline interactives inside their block, in source order.
pub fn elements(blocks: &[Block]) -> Vec<Element> {
    let mut out = Vec::new();
    let mut slugs: HashMap<String, u32> = HashMap::new();
    walk_blocks(blocks, true, &mut out, &mut slugs);
    out
}

/// `top` selects whether blocks at this level are elements themselves. List items and their
/// nested items always are (the reading grain of a list is the item); a container's inner
/// paragraphs are not (the container is the grain) — but their interactive inlines always
/// collect, so a link inside a quote stays a `Tab` stop.
fn walk_blocks(blocks: &[Block], top: bool, out: &mut Vec<Element>, slugs: &mut HashMap<String, u32>) {
    for block in blocks {
        match block {
            Block::Heading {
                level,
                content,
                span,
            } => {
                // Headings are elements at any depth — the outline should see a heading inside
                // a quote (rare, but legal markdown).
                let text = inlines_text(content).trim().to_string();
                let slug = unique_slug(&text, slugs);
                out.push(Element::Heading {
                    span: *span,
                    level: *level,
                    slug,
                    text,
                });
                walk_inlines(content, out);
            }
            Block::Paragraph { content, span } => {
                if top {
                    out.push(Element::Block { span: *span });
                }
                walk_inlines(content, out);
            }
            Block::Code { span, .. }
            | Block::Rule { span }
            | Block::FrontMatter { span, .. }
            | Block::Html { span, .. } => {
                if top {
                    out.push(Element::Block { span: *span });
                }
            }
            Block::Table {
                head, rows, span, ..
            } => {
                if top {
                    out.push(Element::Block { span: *span });
                }
                for cell in head.iter().chain(rows.iter().flatten()) {
                    walk_inlines(cell, out);
                }
            }
            Block::Image {
                src,
                span,
                inner_span,
                ..
            } => {
                // The paragraph is the `j`/`k` stop (bar host); the image markup inside it is
                // the Enter target — leaving the trailing whitespace as the rest byte, so a
                // display image joins the `l`-opts-in model like links do.
                out.push(Element::Block { span: *span });
                out.push(Element::Image {
                    span: *inner_span,
                    src: src.clone(),
                });
            }
            Block::Quote { content, span, .. } => {
                if top {
                    out.push(Element::Block { span: *span });
                }
                walk_blocks(content, false, out, slugs);
            }
            Block::FootnoteDef { content, span, .. } => {
                if top {
                    out.push(Element::Block { span: *span });
                }
                walk_blocks(content, false, out, slugs);
            }
            Block::List { items, .. } => {
                for item in items {
                    out.push(Element::Item {
                        span: item.span,
                        checked: item.checked,
                    });
                    walk_blocks(&item.blocks, false, out, slugs);
                }
            }
        }
    }
}

fn walk_inlines(inlines: &[Inline], out: &mut Vec<Element>) {
    for inl in inlines {
        match inl {
            Inline::Link {
                href,
                content,
                span,
            } => {
                out.push(Element::Link {
                    span: *span,
                    href: href.clone(),
                });
                walk_inlines(content, out);
            }
            Inline::Image { src, span, .. } => {
                out.push(Element::Image {
                    span: *span,
                    src: src.clone(),
                });
            }
            Inline::FootnoteRef { label, span } => {
                out.push(Element::FootnoteRef {
                    span: *span,
                    label: label.clone(),
                });
            }
            Inline::Emphasis { content }
            | Inline::Strong { content }
            | Inline::Strikethrough { content } => walk_inlines(content, out),
            Inline::Text { .. } | Inline::Code { .. } | Inline::HardBreak => {}
        }
    }
}

/// GitHub-style anchor slug: lowercase, alphanumerics/`-`/`_` kept, spaces → `-`, everything
/// else dropped; document-wide duplicates get `-1`, `-2`, ….
fn unique_slug(text: &str, seen: &mut HashMap<String, u32>) -> String {
    let base: String = text
        .chars()
        .flat_map(|c| c.to_lowercase())
        .filter_map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                Some(c)
            } else if c == ' ' {
                Some('-')
            } else {
                None
            }
        })
        .collect();
    let n = seen.entry(base.clone()).or_insert(0);
    let slug = if *n == 0 {
        base.clone()
    } else {
        format!("{base}-{n}")
    };
    *n += 1;
    slug
}

/// The element the reading cursor at byte `pos` focuses: the **innermost** element containing
/// `pos`, else the first element starting after it, else the last element. `None` only for an
/// empty document. This is the pure "focus = f(cursor)" derivation (docs/markdown-view.md §1.3).
pub fn element_at(elements: &[Element], pos: u32) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, el) in elements.iter().enumerate() {
        if el.span().contains(pos) {
            best = Some(match best {
                Some(b) if elements[b].span().len() < el.span().len() => b,
                _ => i,
            });
        }
    }
    best.or_else(|| elements.iter().position(|e| e.span().start >= pos))
        .or(if elements.is_empty() {
            None
        } else {
            Some(elements.len() - 1)
        })
}

/// The innermost element matching `pred` whose span contains `pos` — the class-relative anchor
/// for stepping. Stepping blocks while an inline (a link) holds the focus must start from the
/// link's *containing block*: a lone-link paragraph derives focus back to the link after every
/// Goto to the paragraph start, so stepping backward from the link's own index would land on the
/// containing paragraph again and again, never past it.
pub fn containing_element(
    elements: &[Element],
    pos: u32,
    pred: impl Fn(&Element) -> bool,
) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, el) in elements.iter().enumerate() {
        if el.span().contains(pos) && pred(el) {
            best = Some(match best {
                Some(b) if elements[b].span().len() < el.span().len() => b,
                _ => i,
            });
        }
    }
    best
}

/// [`element_at`] restricted to elements matching `pred`: the innermost matching element
/// containing `pos`, else the first matching element starting after it, else the last matching
/// one. The reading-position (block-grain) derivation — `Some` whenever any matching element
/// exists, so the position marker never vanishes.
pub fn element_at_matching(
    elements: &[Element],
    pos: u32,
    pred: impl Fn(&Element) -> bool,
) -> Option<usize> {
    containing_element(elements, pos, &pred)
        .or_else(|| {
            elements
                .iter()
                .position(|e| pred(e) && e.span().start >= pos)
        })
        .or_else(|| elements.iter().rposition(pred))
}

/// Indices of the interactive elements (links, images, footnote refs) whose spans nest inside
/// `container`, in document order — the reading view's within-block link ring
/// (docs/markdown-view.md §2.3: `h`/`l` step the Enter target inside the focused block).
pub fn interactive_within(elements: &[Element], container: Span) -> Vec<usize> {
    elements
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            let s = e.span();
            e.is_interactive() && s.start >= container.start && s.end <= container.end
        })
        .map(|(i, _)| i)
        .collect()
}

/// The byte a block-grain step lands on to select `block` *without* auto-targeting a leading
/// interactive: the first byte of its span outside every interactive child, so a lone-link
/// paragraph shows the position bar alone and `l` opts into the link
/// (docs/markdown-view.md §2.3). Falls back to the span start when no such byte exists — a
/// block image IS its own target, and staying targeted is the honest state there.
pub fn block_rest_byte(elements: &[Element], block: usize) -> u32 {
    let span = elements[block].span();
    let mut pos = span.start;
    while pos < span.end {
        let covering = elements
            .iter()
            .enumerate()
            .find(|(i, e)| *i != block && e.is_interactive() && e.span().contains(pos));
        match covering {
            Some((_, e)) => pos = e.span().end,
            None => return pos,
        }
    }
    span.start
}

/// The nearest element matching `pred` strictly after (`forward`) or before `from` in list
/// order. `from` may be any element index (e.g. the focused link when stepping blocks).
pub fn step_element(
    elements: &[Element],
    from: usize,
    forward: bool,
    pred: impl Fn(&Element) -> bool,
) -> Option<usize> {
    if forward {
        elements
            .iter()
            .enumerate()
            .skip(from + 1)
            .find(|(_, e)| pred(e))
            .map(|(i, _)| i)
    } else {
        elements
            .iter()
            .enumerate()
            .take(from)
            .rev()
            .find(|(_, e)| pred(e))
            .map(|(i, _)| i)
    }
}

/// Resolve an in-document anchor (`#some-heading`) against the heading slugs.
pub fn heading_by_slug(elements: &[Element], slug: &str) -> Option<usize> {
    elements
        .iter()
        .position(|e| matches!(e, Element::Heading { slug: s, .. } if s == slug))
}

/// Every fenced code block that names a language, recursively, as `(span, language, code)` —
/// the reading view's highlight fan-out (docs/markdown-view.md §2.8).
pub fn fenced_code_blocks(blocks: &[Block]) -> Vec<(Span, String, String)> {
    let mut out = Vec::new();
    collect_fences(blocks, &mut out);
    out
}

fn collect_fences(blocks: &[Block], out: &mut Vec<(Span, String, String)>) {
    for block in blocks {
        match block {
            Block::Code {
                language: Some(lang),
                code,
                span,
            } => out.push((*span, lang.clone(), code.clone())),
            Block::Quote { content, .. } | Block::FootnoteDef { content, .. } => {
                collect_fences(content, out)
            }
            Block::List { items, .. } => {
                for item in items {
                    collect_fences(&item.blocks, out);
                }
            }
            _ => {}
        }
    }
}

/// The source span of the footnote definition with `label`, for `Enter` on a footnote reference.
pub fn footnote_def_span(blocks: &[Block], label: &str) -> Option<Span> {
    for block in blocks {
        match block {
            Block::FootnoteDef {
                label: l, span, ..
            } if l == label => return Some(*span),
            Block::Quote { content, .. } | Block::FootnoteDef { content, .. } => {
                if let Some(s) = footnote_def_span(content, label) {
                    return Some(s);
                }
            }
            Block::List { items, .. } => {
                for item in items {
                    if let Some(s) = footnote_def_span(&item.blocks, label) {
                        return Some(s);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

// ---- plain flattening ---------------------------------------------------------------------------

/// Flatten a parsed document back to plain text, for "copy whole popover" (the AST is the only
/// form the shells retain — the original Markdown source is gone after `parse`). Blocks are
/// separated by a blank line; lists keep their bullets/numbers, code its lines, links their
/// visible text.
pub fn to_plain(blocks: &[Block]) -> String {
    let mut out = String::new();
    push_blocks_plain(blocks, &mut out, "");
    out.trim_end().to_string()
}

fn push_blocks_plain(blocks: &[Block], out: &mut String, indent: &str) {
    for block in blocks {
        match block {
            Block::Heading { content, .. } | Block::Paragraph { content, .. } => {
                out.push_str(indent);
                push_inlines_plain(content, out);
                out.push_str("\n\n");
            }
            Block::Code { code, .. } | Block::Html { raw: code, .. } => {
                for line in code.split('\n') {
                    out.push_str(indent);
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }
            Block::List {
                ordered,
                start,
                items,
                ..
            } => {
                for (i, item) in items.iter().enumerate() {
                    let mut marker = if *ordered {
                        format!("{}. ", start + i as u64)
                    } else {
                        "- ".to_string()
                    };
                    if let Some(done) = item.checked {
                        marker.push_str(if done { "[x] " } else { "[ ] " });
                    }
                    // Render the item, then graft the marker onto its first line and indent the
                    // rest.
                    let mut item_text = String::new();
                    push_blocks_plain(&item.blocks, &mut item_text, "");
                    let item_text = item_text.trim_end();
                    let pad: String = " ".repeat(marker.chars().count());
                    for (j, line) in item_text.split('\n').enumerate() {
                        out.push_str(indent);
                        out.push_str(if j == 0 { &marker } else { &pad });
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                out.push('\n');
            }
            Block::Quote { content, .. } => {
                let mut inner = String::new();
                push_blocks_plain(content, &mut inner, "");
                for line in inner.trim_end().split('\n') {
                    out.push_str(indent);
                    out.push_str("> ");
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }
            Block::Rule { .. } => {
                out.push_str(indent);
                out.push_str("---\n\n");
            }
            Block::Table { head, rows, .. } => {
                for row in std::iter::once(head).chain(rows.iter()) {
                    if row.is_empty() {
                        continue;
                    }
                    out.push_str(indent);
                    let cells: Vec<String> = row.iter().map(|c| inlines_text(c)).collect();
                    out.push_str(&cells.join(" | "));
                    out.push('\n');
                }
                out.push('\n');
            }
            Block::Image { alt, .. } => {
                out.push_str(indent);
                out.push_str(alt);
                out.push_str("\n\n");
            }
            // Front matter is metadata, not prose — skipped in plain copies.
            Block::FrontMatter { .. } => {}
            Block::FootnoteDef { label, content, .. } => {
                let mut inner = String::new();
                push_blocks_plain(content, &mut inner, "");
                out.push_str(indent);
                out.push_str(&format!("[{label}]: "));
                out.push_str(inner.trim_end());
                out.push_str("\n\n");
            }
        }
    }
}

fn push_inlines_plain(inlines: &[Inline], out: &mut String) {
    for inl in inlines {
        match inl {
            Inline::Text { text } | Inline::Code { text } => out.push_str(text),
            Inline::Emphasis { content }
            | Inline::Strong { content }
            | Inline::Strikethrough { content }
            | Inline::Link { content, .. } => push_inlines_plain(content, out),
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::FootnoteRef { label, .. } => {
                out.push('[');
                out.push_str(label);
                out.push(']');
            }
            Inline::HardBreak => out.push('\n'),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Inline {
        Inline::Text { text: s.into() }
    }

    /// The span of `needle`'s (first) occurrence in `md` — tests state spans by content, not by
    /// hand-counted offsets.
    fn span_of(md: &str, needle: &str) -> Span {
        let start = md.find(needle).expect("needle present") as u32;
        Span {
            start,
            end: start + needle.len() as u32,
        }
    }

    #[test]
    fn paragraph_with_inline_code_and_link() {
        let md = "See `foo` and [docs](https://x.y).";
        let blocks = parse(md);
        assert_eq!(
            blocks,
            vec![Block::Paragraph {
                content: vec![
                    text("See "),
                    Inline::Code { text: "foo".into() },
                    text(" and "),
                    Inline::Link {
                        href: "https://x.y".into(),
                        content: vec![text("docs")],
                        span: span_of(md, "[docs](https://x.y)"),
                    },
                    text("."),
                ],
                span: span_of(md, md),
            }]
        );
    }

    #[test]
    fn fenced_code_block_keeps_language_and_strips_trailing_newline() {
        let md = "```rust\nfn x() {}\n```";
        let blocks = parse(md);
        assert_eq!(
            blocks,
            vec![Block::Code {
                language: Some("rust".into()),
                code: "fn x() {}".into(),
                span: span_of(md, md),
            }]
        );
    }

    #[test]
    fn list_items_fold_soft_wrapped_lines() {
        // The continuation line (no marker) belongs to the same item.
        let md = "- first item that is\n  wrapped\n- second";
        let blocks = parse(md);
        let Block::List { ordered, items, .. } = &blocks[0] else {
            panic!("expected list, got {blocks:?}");
        };
        assert!(!ordered);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].checked, None);
        // The tight item's synthesized paragraph carries the item's span (the element grain).
        assert_eq!(
            items[0].blocks,
            vec![Block::Paragraph {
                content: vec![text("first item that is wrapped")],
                span: items[0].span,
            }]
        );
        assert_eq!(items[0].span, span_of(md, "- first item that is\n  wrapped\n"));
    }

    #[test]
    fn heading_level_and_rule() {
        let md = "# Title\n\n---";
        let blocks = parse(md);
        assert_eq!(
            blocks,
            vec![
                Block::Heading {
                    level: 1,
                    content: vec![text("Title")],
                    span: span_of(md, "# Title\n"),
                },
                Block::Rule {
                    span: span_of(md, "---"),
                },
            ]
        );
    }

    #[test]
    fn setext_heading_span_covers_underline() {
        let md = "Title Line\n==========\n\nBody.\n";
        let blocks = parse(md);
        let Block::Heading { level, span, .. } = &blocks[0] else {
            panic!("expected heading, got {blocks:?}");
        };
        assert_eq!(*level, 1);
        assert_eq!(*span, span_of(md, "Title Line\n==========\n"));
    }

    #[test]
    fn table_shape_alignments_and_span() {
        let md = "| Name | Age |\n|:-----|----:|\n| Ada  | 36  |\n| Bo | 7 |\n";
        let blocks = parse(md);
        let Block::Table {
            alignments,
            head,
            rows,
            span,
        } = &blocks[0]
        else {
            panic!("expected table, got {blocks:?}");
        };
        assert_eq!(alignments, &vec![ColAlign::Left, ColAlign::Right]);
        assert_eq!(head, &vec![vec![text("Name")], vec![text("Age")]]);
        assert_eq!(
            rows,
            &vec![
                vec![vec![text("Ada")], vec![text("36")]],
                vec![vec![text("Bo")], vec![text("7")]],
            ]
        );
        assert_eq!(span.start, 0);
        assert_eq!(span.end as usize, md.len());
    }

    #[test]
    fn task_list_markers() {
        let md = "- [x] done item\n- [ ] open item\n";
        let blocks = parse(md);
        let Block::List { items, .. } = &blocks[0] else {
            panic!("expected list, got {blocks:?}");
        };
        assert_eq!(items[0].checked, Some(true));
        assert_eq!(items[1].checked, Some(false));
        assert_eq!(items[0].blocks[0], Block::Paragraph {
            content: vec![text("done item")],
            span: items[0].span,
        });
    }

    #[test]
    fn ordered_list_keeps_start_number() {
        let md = "3. third\n4. fourth\n";
        let blocks = parse(md);
        let Block::List { ordered, start, .. } = &blocks[0] else {
            panic!("expected list, got {blocks:?}");
        };
        assert!(*ordered);
        assert_eq!(*start, 3);
        assert_eq!(to_plain(&blocks), "3. third\n4. fourth");
    }

    #[test]
    fn footnote_ref_and_definition() {
        let md = "Uses a note[^a] here.\n\n[^a]: The note body.\n";
        let blocks = parse(md);
        let Block::Paragraph { content, .. } = &blocks[0] else {
            panic!("expected paragraph, got {blocks:?}");
        };
        assert_eq!(
            content[1],
            Inline::FootnoteRef {
                label: "a".into(),
                span: span_of(md, "[^a]"),
            }
        );
        let Block::FootnoteDef { label, span, .. } = &blocks[1] else {
            panic!("expected footnote def, got {blocks:?}");
        };
        assert_eq!(label, "a");
        assert_eq!(*span, span_of(md, "[^a]: The note body.\n"));
    }

    #[test]
    fn front_matter_collects_raw_text() {
        let md = "---\ntitle: Test\ntags: [a, b]\n---\n\nBody para.\n";
        let blocks = parse(md);
        assert_eq!(
            blocks[0],
            Block::FrontMatter {
                text: "title: Test\ntags: [a, b]".into(),
                span: span_of(md, "---\ntitle: Test\ntags: [a, b]\n---"),
            }
        );
        assert!(matches!(&blocks[1], Block::Paragraph { .. }));
    }

    #[test]
    fn gfm_alert_kind() {
        let md = "> [!NOTE]\n> Careful reader.\n";
        let blocks = parse(md);
        let Block::Quote { alert, content, .. } = &blocks[0] else {
            panic!("expected quote, got {blocks:?}");
        };
        assert_eq!(*alert, Some(AlertKind::Note));
        assert_eq!(content.len(), 1);
    }

    #[test]
    fn lone_image_paragraph_promotes_to_block_image() {
        let md = "![diagram](d.png)\n\nText after.\n";
        let blocks = parse(md);
        assert_eq!(
            blocks[0],
            Block::Image {
                src: "d.png".into(),
                alt: "diagram".into(),
                span: span_of(md, "![diagram](d.png)\n"),
                // The markup itself, no trailing newline — the Enter-target span, leaving
                // the block a rest byte for the `l`-opts-in model.
                inner_span: span_of(md, "![diagram](d.png)"),
            }
        );
        // An image with surrounding text stays inline.
        let md2 = "See ![icon](i.png) here.\n";
        let blocks2 = parse(md2);
        let Block::Paragraph { content, .. } = &blocks2[0] else {
            panic!("expected paragraph, got {blocks2:?}");
        };
        assert_eq!(
            content[1],
            Inline::Image {
                src: "i.png".into(),
                alt: "icon".into(),
                span: span_of(md2, "![icon](i.png)"),
            }
        );
    }

    #[test]
    fn hard_break_and_strikethrough() {
        let md = "line one  \nline two has ~~gone~~ text\n";
        let blocks = parse(md);
        let Block::Paragraph { content, .. } = &blocks[0] else {
            panic!("expected paragraph, got {blocks:?}");
        };
        assert_eq!(content[0], text("line one"));
        assert_eq!(content[1], Inline::HardBreak);
        assert!(content
            .iter()
            .any(|i| matches!(i, Inline::Strikethrough { content } if content == &vec![text("gone")])));
    }

    #[test]
    fn html_block_kept_raw() {
        let md = "<div class=\"x\">\nraw <b>html</b>\n</div>\n\nAfter.\n";
        let blocks = parse(md);
        assert_eq!(
            blocks[0],
            Block::Html {
                raw: "<div class=\"x\">\nraw <b>html</b>\n</div>".into(),
                span: span_of(md, "<div class=\"x\">\nraw <b>html</b>\n</div>\n"),
            }
        );
    }

    #[test]
    fn smart_punctuation_keeps_source_spans_on_links() {
        // Smart punctuation rewrites text but must not disturb interactive spans.
        let md = "\"Quoted\" -- see [docs](https://x.y) now...\n";
        let blocks = parse(md);
        let Block::Paragraph { content, .. } = &blocks[0] else {
            panic!("expected paragraph, got {blocks:?}");
        };
        let link = content
            .iter()
            .find(|i| matches!(i, Inline::Link { .. }))
            .expect("link present");
        let Inline::Link { span, .. } = link else {
            unreachable!()
        };
        assert_eq!(*span, span_of(md, "[docs](https://x.y)"));
    }

    #[test]
    fn element_list_document_order_and_kinds() {
        let md = "# Title\n\nA [link](https://x.y) here.\n\n- one\n- two\n\n> quoted [inner](https://q.z)\n";
        let els = elements(&parse(md));
        // Document order, outer before inner.
        let kinds: Vec<&str> = els
            .iter()
            .map(|e| match e {
                Element::Heading { .. } => "heading",
                Element::Block { .. } => "block",
                Element::Item { .. } => "item",
                Element::Link { .. } => "link",
                Element::Image { .. } => "image",
                Element::FootnoteRef { .. } => "ref",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["heading", "block", "link", "item", "item", "block", "link"]
        );
        // Starts are non-decreasing (construction order is document order).
        let starts: Vec<u32> = els.iter().map(|e| e.span().start).collect();
        let mut sorted = starts.clone();
        sorted.sort();
        assert_eq!(starts, sorted);
        // The quote is the block element; the link inside it is still a Tab stop.
        let Element::Link { href, .. } = &els[6] else {
            panic!("expected link, got {:?}", els[6]);
        };
        assert_eq!(href, "https://q.z");
    }

    #[test]
    fn element_at_prefers_innermost_then_next() {
        let md = "Intro para.\n\nA [link](https://x.y) here.\n";
        let els = elements(&parse(md));
        // Inside the link → the link, not its paragraph.
        let link_pos = md.find("[link]").unwrap() as u32 + 1;
        let idx = element_at(&els, link_pos).unwrap();
        assert!(matches!(els[idx], Element::Link { .. }));
        // In paragraph text → the paragraph.
        let para_pos = md.find("here").unwrap() as u32;
        let idx = element_at(&els, para_pos).unwrap();
        assert!(matches!(els[idx], Element::Block { .. }));
        // On the blank line between blocks → the next block.
        let gap = md.find("\n\nA").unwrap() as u32 + 1;
        let idx = element_at(&els, gap).unwrap();
        assert_eq!(els[idx].span(), els[1].span());
        // Past the end → the last element.
        let idx = element_at(&els, md.len() as u32 + 100).unwrap();
        assert_eq!(idx, els.len() - 1);
    }

    #[test]
    fn step_element_filters_by_class() {
        let md = "# H\n\nPara with [a](https://a.a) and [b](https://b.b).\n\nNext.\n";
        let els = elements(&parse(md));
        // From the heading, the next block skips over the links.
        let next = step_element(&els, 0, true, Element::is_block).unwrap();
        assert!(matches!(els[next], Element::Block { .. }));
        // From that paragraph, the next interactive is link a, then link b.
        let a = step_element(&els, next, true, Element::is_interactive).unwrap();
        let b = step_element(&els, a, true, Element::is_interactive).unwrap();
        let (Element::Link { href: ha, .. }, Element::Link { href: hb, .. }) = (&els[a], &els[b])
        else {
            panic!("expected links");
        };
        assert_eq!((ha.as_str(), hb.as_str()), ("https://a.a", "https://b.b"));
        // Stepping back from link b lands on link a; no interactive before the first link.
        assert_eq!(step_element(&els, b, false, Element::is_interactive), Some(a));
        assert_eq!(step_element(&els, a, false, Element::is_interactive), None);
    }

    #[test]
    fn interactive_within_scopes_the_link_ring_to_a_block() {
        let md = "Para with [a](https://a.a) and [b](https://b.b).\n\nAnother [c](https://c.c).\n";
        let els = elements(&parse(md));
        // The first paragraph's ring holds its two links, in order — not the third one.
        let para = els
            .iter()
            .position(|e| matches!(e, Element::Block { .. }))
            .unwrap();
        let ring = interactive_within(&els, els[para].span());
        let hrefs: Vec<&str> = ring
            .iter()
            .map(|&i| match &els[i] {
                Element::Link { href, .. } => href.as_str(),
                other => panic!("expected link, got {other:?}"),
            })
            .collect();
        assert_eq!(hrefs, vec!["https://a.a", "https://b.b"]);
        // A container with no interactives yields an empty ring.
        let md2 = "# Plain\n\nNo links here.\n";
        let els2 = elements(&parse(md2));
        assert!(interactive_within(&els2, els2[0].span()).is_empty());
    }

    #[test]
    fn block_rest_byte_lands_outside_leading_interactives() {
        // A lone-link paragraph: the rest byte sits after the link (the trailing newline), so
        // block steps select the paragraph without auto-targeting the link.
        let md = "[docs](https://x.y)\n\nSee [a](https://a.a) here.\n";
        let els = elements(&parse(md));
        let lone = els
            .iter()
            .position(|e| matches!(e, Element::Block { .. }))
            .unwrap();
        let rest = block_rest_byte(&els, lone);
        assert_eq!(rest, "[docs](https://x.y)".len() as u32);
        assert!(element_at(&els, rest) == Some(lone), "rest byte derives the block itself");
        // An ordinary paragraph: the rest byte is simply its start.
        let para = els
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, Element::Block { .. }))
            .nth(1)
            .unwrap()
            .0;
        assert_eq!(block_rest_byte(&els, para), els[para].span().start);
        // A block image now splits into Block (paragraph span) + Image target (markup span),
        // so it has a rest byte too — the trailing newline — and joins the l-opts-in model.
        let md2 = "![d](i.png)\n";
        let els2 = elements(&parse(md2));
        assert!(matches!(els2[0], Element::Block { .. }));
        assert!(matches!(els2[1], Element::Image { .. }));
        let rest = block_rest_byte(&els2, 0);
        assert_eq!(rest, "![d](i.png)".len() as u32);
        assert_eq!(
            containing_element(&els2, rest, Element::is_interactive),
            None,
            "no auto-target at the rest byte"
        );
    }

    #[test]
    fn containing_element_filters_by_class() {
        let md = "# H\n\n[docs](https://x.y)\n";
        let els = elements(&parse(md));
        // At the link's first byte the innermost element is the link, but the innermost *block*
        // is its containing paragraph — the class-relative step anchor.
        let pos = md.find("[docs]").unwrap() as u32;
        let link = element_at(&els, pos).unwrap();
        assert!(matches!(els[link], Element::Link { .. }));
        let block = containing_element(&els, pos, Element::is_block).unwrap();
        assert!(matches!(els[block], Element::Block { .. }));
        assert!(els[block].span().contains(pos));
        // No heading contains that byte.
        assert_eq!(
            containing_element(&els, pos, |e| matches!(e, Element::Heading { .. })),
            None
        );
    }

    #[test]
    fn element_at_matching_falls_forward_then_back() {
        let md = "# H\n\nA [x](https://x.y) para.\n";
        let els = elements(&parse(md));
        // In the gap between blocks → the next block, never the link inside it… well, the
        // paragraph *is* first in document order; the point is the pred filter applies.
        let gap = md.find("\n\nA").unwrap() as u32 + 1;
        let idx = element_at_matching(&els, gap, Element::is_block).unwrap();
        assert!(matches!(els[idx], Element::Block { .. }));
        // Past the end → the last matching element (the paragraph, skipping the link).
        let idx = element_at_matching(&els, md.len() as u32 + 50, Element::is_block).unwrap();
        assert!(matches!(els[idx], Element::Block { .. }));
        // No interactive contains the heading position, and none precedes it.
        assert_eq!(
            element_at_matching(&els, 0, Element::is_interactive),
            els.iter().position(|e| e.is_interactive()),
            "falls forward to the first interactive"
        );
    }

    #[test]
    fn heading_slugs_github_style_and_deduped() {
        let md = "# Hello World!\n\n## Hello World!\n\n## With `code` & more\n";
        let els = elements(&parse(md));
        let slugs: Vec<&str> = els
            .iter()
            .filter_map(|e| match e {
                Element::Heading { slug, .. } => Some(slug.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(slugs, vec!["hello-world", "hello-world-1", "with-code--more"]);
        assert_eq!(heading_by_slug(&els, "hello-world-1"), Some(1));
        assert_eq!(heading_by_slug(&els, "missing"), None);
    }

    #[test]
    fn to_plain_flattens_headings_lists_and_code() {
        let md =
            "# Title\n\nSome `inline` and [docs](https://x.y).\n\n- one\n- two\n\n```\ncode\n```\n";
        let plain = to_plain(&parse(md));
        assert_eq!(
            plain,
            "Title\n\nSome inline and docs.\n\n- one\n- two\n\ncode"
        );
    }

    #[test]
    fn to_plain_tables_and_tasks() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n\n- [x] done\n";
        let plain = to_plain(&parse(md));
        assert_eq!(plain, "A | B\n1 | 2\n\n- [x] done");
    }
}
