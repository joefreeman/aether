//! Tree-sitter integration: language registry, parsing, and per-range highlight computation.

use aether_protocol::viewport::Highlight;
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor, Tree};

pub struct LanguageConfig {
    pub name: &'static str,
    pub language: Language,
    pub query: Query,
}

static RUST: OnceLock<LanguageConfig> = OnceLock::new();
static MARKDOWN: OnceLock<LanguageConfig> = OnceLock::new();
static TOML: OnceLock<LanguageConfig> = OnceLock::new();

pub fn get_config(name: &str) -> Option<&'static LanguageConfig> {
    match name {
        "rust" => Some(RUST.get_or_init(|| {
            let language: Language = tree_sitter_rust::LANGUAGE.into();
            let query = Query::new(&language, tree_sitter_rust::HIGHLIGHTS_QUERY)
                .expect("rust highlights query compiles");
            LanguageConfig { name: "rust", language, query }
        })),
        "markdown" => Some(MARKDOWN.get_or_init(|| {
            let language: Language = tree_sitter_md::LANGUAGE.into();
            let query = Query::new(&language, tree_sitter_md::HIGHLIGHT_QUERY_BLOCK)
                .expect("markdown highlights query compiles");
            LanguageConfig { name: "markdown", language, query }
        })),
        "toml" => Some(TOML.get_or_init(|| {
            let language: Language = tree_sitter_toml_ng::LANGUAGE.into();
            let query = Query::new(&language, tree_sitter_toml_ng::HIGHLIGHTS_QUERY)
                .expect("toml highlights query compiles");
            LanguageConfig { name: "toml", language, query }
        })),
        _ => None,
    }
}

pub fn make_parser(config: &LanguageConfig) -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&config.language)
        .expect("language is ABI-compatible with the tree-sitter runtime");
    parser
}

/// Compute non-overlapping highlight spans for the byte range `[range_start, range_end)` within
/// `source`. The returned highlights' `start`/`end` are **relative to `range_start`** (i.e. they
/// fall in `[0, range_end - range_start)`).
///
/// More-specific (shorter) captures override longer ones at the same byte. Captures of the same
/// length are last-writer-wins by query order.
pub fn highlights_for_range(
    config: &LanguageConfig,
    tree: &Tree,
    source: &str,
    range_start: usize,
    range_end: usize,
) -> Vec<Highlight> {
    if range_end <= range_start {
        return vec![];
    }
    let span_len = range_end - range_start;
    let bytes = source.as_bytes();
    let capture_names = config.query.capture_names();

    let mut captures: Vec<(usize, usize, &str)> = Vec::new();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(range_start..range_end);
    let mut matches = cursor.matches(&config.query, tree.root_node(), bytes);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let name = capture_names[cap.index as usize];
            let s = cap.node.start_byte().max(range_start);
            let e = cap.node.end_byte().min(range_end);
            if s < e {
                captures.push((s, e, name));
            }
        }
    }
    if captures.is_empty() {
        return vec![];
    }

    // Apply longest captures first so shorter, more-specific captures overwrite them.
    captures.sort_by(|a, b| {
        let len_a = a.1 - a.0;
        let len_b = b.1 - b.0;
        len_b.cmp(&len_a).then(a.0.cmp(&b.0))
    });

    let mut per_byte: Vec<Option<&str>> = vec![None; span_len];
    for (s, e, name) in &captures {
        for i in *s..*e {
            per_byte[i - range_start] = Some(*name);
        }
    }

    let mut spans = Vec::new();
    let mut current_start = 0usize;
    let mut current_name: Option<&str> = None;
    for (i, name) in per_byte.iter().enumerate() {
        if *name != current_name {
            if let Some(n) = current_name {
                spans.push(Highlight {
                    start: current_start as u32,
                    end: i as u32,
                    kind: n.to_string(),
                });
            }
            current_start = i;
            current_name = *name;
        }
    }
    if let Some(n) = current_name {
        spans.push(Highlight {
            start: current_start as u32,
            end: span_len as u32,
            kind: n.to_string(),
        });
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_keyword_and_string_are_highlighted() {
        let cfg = get_config("rust").unwrap();
        let mut parser = make_parser(cfg);
        let source = "fn main() { let s = \"hi\"; }";
        let tree = parser.parse(source, None).unwrap();
        let highlights = highlights_for_range(cfg, &tree, source, 0, source.len());

        // Should produce some non-empty highlights.
        assert!(!highlights.is_empty(), "expected highlights for Rust source");

        // 'fn' is a keyword.
        let fn_kw = highlights.iter().find(|h| h.start == 0 && h.end == 2);
        assert!(
            fn_kw.is_some_and(|h| h.kind.contains("keyword")),
            "expected 'fn' to be a keyword, got {:?}",
            fn_kw
        );

        // The string '"hi"' should have a string kind somewhere in its span.
        let string_pos = source.find("\"hi\"").unwrap() as u32;
        let has_string = highlights
            .iter()
            .any(|h| h.start <= string_pos && h.end > string_pos && h.kind.contains("string"));
        assert!(has_string, "expected string highlight for \"hi\"");
    }

    #[test]
    fn range_filter_clips_highlights() {
        let cfg = get_config("rust").unwrap();
        let mut parser = make_parser(cfg);
        let source = "fn alpha() {}\nfn beta() {}\n";
        let tree = parser.parse(source, None).unwrap();

        let line2_start = source.find("fn beta").unwrap();
        let line2_end = source.find("\nfn beta").map_or(source.len(), |i| i + 13);
        let highlights = highlights_for_range(cfg, &tree, source, line2_start, line2_end);

        // Should reference 'beta' but not 'alpha'.
        for h in &highlights {
            assert!(h.end as usize <= line2_end - line2_start);
        }
        assert!(highlights.iter().any(|h| h.kind.contains("keyword")));
    }
}
