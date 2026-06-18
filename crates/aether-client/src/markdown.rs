//! Markdown → AST for hover popovers.
//!
//! Hover content is parsed once here (with `pulldown-cmark`, a real CommonMark parser) into a small
//! serializable tree, so every shell renders the *same* structure — the web to DOM, the native
//! client to iced widgets, the terminal to styled lines — instead of each re-implementing a parser.
//! Only the subset that LSP hover actually uses is modelled; anything fancier (tables, footnotes,
//! HTML, math) degrades to its text content rather than erroring.
//!
//! `Serialize` is for the wasm boundary (the web shell renders the AST as JSON); the native and
//! terminal shells consume the Rust values directly.

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag};
use serde::Serialize;

/// A block-level node.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    Paragraph {
        content: Vec<Inline>,
    },
    Code {
        language: Option<String>,
        code: String,
    },
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    Quote {
        content: Vec<Block>,
    },
    Rule,
}

/// An inline (span-level) node.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Inline {
    Text { text: String },
    Code { text: String },
    Emphasis { content: Vec<Inline> },
    Strong { content: Vec<Inline> },
    Link { href: String, content: Vec<Inline> },
}

/// Parse Markdown into the hover AST.
pub fn parse(md: &str) -> Vec<Block> {
    let mut b = Builder::default();
    for ev in Parser::new(md) {
        match ev {
            Event::Start(tag) => b.start(tag),
            Event::End(_) => b.end(),
            Event::Text(s) => b.text(&s),
            Event::Code(s) => b.push_inline(Inline::Code {
                text: s.to_string(),
            }),
            // A soft or hard break inside a paragraph just separates words for our wrapped layout.
            Event::SoftBreak | Event::HardBreak => b.text(" "),
            Event::Rule => b.push_block(Block::Rule),
            // Html / math / footnote refs / task markers: ignored (their text, if any, still flows).
            _ => {}
        }
    }
    b.out
}

/// An in-progress container on the parse stack.
enum Frame {
    Paragraph(Vec<Inline>),
    Heading(u8, Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strong(Vec<Inline>),
    Link(String, Vec<Inline>),
    /// Transparent inline container — strikethrough, image alt, or any unmodelled inline tag. Its
    /// children flow into the parent on close.
    Span(Vec<Inline>),
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    Item(Vec<Block>),
    Quote(Vec<Block>),
    Code(Option<String>, String),
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
                Frame::Paragraph(v)
                | Frame::Heading(_, v)
                | Frame::Emphasis(v)
                | Frame::Strong(v)
                | Frame::Link(_, v)
                | Frame::Span(v),
            ) => Some(v),
            _ => None,
        }
    }

    /// The inline list to append to: the innermost inline frame, or a trailing paragraph opened in
    /// the current block context (a tight list item / blockquote emits inline text with no wrapping
    /// paragraph, so we synthesise one).
    fn inline_target(&mut self) -> &mut Vec<Inline> {
        let has_inline_frame = matches!(
            self.stack.last(),
            Some(
                Frame::Paragraph(_)
                    | Frame::Heading(..)
                    | Frame::Emphasis(_)
                    | Frame::Strong(_)
                    | Frame::Link(..)
                    | Frame::Span(_)
            )
        );
        if has_inline_frame {
            return self.inlines_mut().expect("inline frame present");
        }
        let blocks = match self.stack.last_mut() {
            Some(Frame::Item(v) | Frame::Quote(v)) => v,
            _ => &mut self.out,
        };
        if !matches!(blocks.last(), Some(Block::Paragraph { .. })) {
            blocks.push(Block::Paragraph {
                content: Vec::new(),
            });
        }
        match blocks.last_mut() {
            Some(Block::Paragraph { content }) => content,
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

    /// Append a block to the innermost block-collecting frame (a list item or quote), else the root.
    fn push_block(&mut self, blk: Block) {
        match self.stack.last_mut() {
            Some(Frame::Item(v) | Frame::Quote(v)) => v.push(blk),
            _ => self.out.push(blk),
        }
    }

    fn text(&mut self, s: &str) {
        if let Some(Frame::Code(_, code)) = self.stack.last_mut() {
            code.push_str(s);
        } else {
            self.push_inline(Inline::Text {
                text: s.to_string(),
            });
        }
    }

    fn start(&mut self, tag: Tag) {
        let frame = match tag {
            Tag::Paragraph => Frame::Paragraph(Vec::new()),
            Tag::Heading { level, .. } => Frame::Heading(level as u8, Vec::new()),
            Tag::Emphasis => Frame::Emphasis(Vec::new()),
            Tag::Strong => Frame::Strong(Vec::new()),
            Tag::Link { dest_url, .. } => Frame::Link(dest_url.to_string(), Vec::new()),
            Tag::List(start) => Frame::List {
                ordered: start.is_some(),
                items: Vec::new(),
            },
            Tag::Item => Frame::Item(Vec::new()),
            Tag::BlockQuote(_) => Frame::Quote(Vec::new()),
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(l) if !l.is_empty() => Some(l.to_string()),
                    _ => None,
                };
                Frame::Code(lang, String::new())
            }
            // Strikethrough, images, tables, … — keep their inline text, drop the wrapper.
            _ => Frame::Span(Vec::new()),
        };
        self.stack.push(frame);
    }

    fn end(&mut self) {
        let Some(frame) = self.stack.pop() else {
            return;
        };
        match frame {
            Frame::Paragraph(content) => self.push_block(Block::Paragraph { content }),
            Frame::Heading(level, content) => self.push_block(Block::Heading { level, content }),
            Frame::Emphasis(content) => self.push_inline(Inline::Emphasis { content }),
            Frame::Strong(content) => self.push_inline(Inline::Strong { content }),
            Frame::Link(href, content) => self.push_inline(Inline::Link { href, content }),
            // Transparent: flow children to the parent (push_inline opens a paragraph if needed).
            Frame::Span(content) => {
                for inl in content {
                    self.push_inline(inl);
                }
            }
            Frame::List { ordered, items } => self.push_block(Block::List { ordered, items }),
            Frame::Item(blocks) => {
                if let Some(Frame::List { items, .. }) = self.stack.last_mut() {
                    items.push(blocks);
                }
            }
            Frame::Quote(content) => self.push_block(Block::Quote { content }),
            Frame::Code(language, code) => {
                // pulldown emits a trailing newline after the last code line — drop it.
                let code = code.strip_suffix('\n').map(str::to_string).unwrap_or(code);
                self.push_block(Block::Code { language, code });
            }
        }
    }
}

/// Flatten a parsed document back to plain text, for "copy whole popover" (the AST is the only form
/// the shells retain — the original Markdown source is gone after `parse`). Blocks are separated by
/// a blank line; lists keep their bullets/numbers, code its lines, links their visible text.
pub fn to_plain(blocks: &[Block]) -> String {
    let mut out = String::new();
    push_blocks_plain(blocks, &mut out, "");
    out.trim_end().to_string()
}

fn push_blocks_plain(blocks: &[Block], out: &mut String, indent: &str) {
    for block in blocks {
        match block {
            Block::Heading { content, .. } | Block::Paragraph { content } => {
                out.push_str(indent);
                push_inlines_plain(content, out);
                out.push_str("\n\n");
            }
            Block::Code { code, .. } => {
                for line in code.split('\n') {
                    out.push_str(indent);
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }
            Block::List { ordered, items } => {
                for (i, item) in items.iter().enumerate() {
                    let marker = if *ordered {
                        format!("{}. ", i + 1)
                    } else {
                        "- ".to_string()
                    };
                    // Render the item, then graft the marker onto its first line and indent the rest.
                    let mut item_text = String::new();
                    push_blocks_plain(item, &mut item_text, "");
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
            Block::Quote { content } => {
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
            Block::Rule => {
                out.push_str(indent);
                out.push_str("---\n\n");
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
            | Inline::Link { content, .. } => push_inlines_plain(content, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Inline {
        Inline::Text { text: s.into() }
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
                    },
                    text("."),
                ],
            }]
        );
    }

    #[test]
    fn fenced_code_block_keeps_language_and_strips_trailing_newline() {
        let blocks = parse("```rust\nfn x() {}\n```");
        assert_eq!(
            blocks,
            vec![Block::Code {
                language: Some("rust".into()),
                code: "fn x() {}".into(),
            }]
        );
    }

    #[test]
    fn list_items_fold_soft_wrapped_lines() {
        // The continuation line (no marker) belongs to the same item.
        let md = "- first item that is\n  wrapped\n- second";
        let blocks = parse(md);
        let Block::List { ordered, items } = &blocks[0] else {
            panic!("expected list, got {blocks:?}");
        };
        assert!(!ordered);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            vec![Block::Paragraph {
                content: vec![text("first item that is wrapped")],
            }]
        );
    }

    #[test]
    fn heading_level_and_rule() {
        let blocks = parse("# Title\n\n---");
        assert_eq!(
            blocks,
            vec![
                Block::Heading {
                    level: 1,
                    content: vec![text("Title")],
                },
                Block::Rule,
            ]
        );
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
}
