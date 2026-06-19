//! Tree-sitter integration: language registry, parsing, and per-range highlight computation.

use crate::indent::{expand_inherits, CompiledIndentQuery, IndentStyle};
use aether_protocol::viewport::Highlight;
use std::ops::Range;
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor, Tree};

pub struct LanguageConfig {
    pub name: &'static str,
    pub language: Language,
    pub query: Query,
    /// Optional injection query (e.g. markdown fenced code blocks). Patterns must capture
    /// `@injection.content` for the byte range to re-parse, and either `@injection.language`
    /// (capture text names the language) or a `#set! injection.language "<name>"` directive.
    pub injection_query: Option<Query>,
    /// Optional `indents.scm` query (vendored from Helix). Drives the smart-indent engine; when
    /// absent we fall back to copying the previous non-empty line's leading whitespace.
    pub indent_query: Option<CompiledIndentQuery>,
    /// Style to use when the buffer is empty or has no detectable indent — e.g. Rust/Python
    /// default to 4 spaces (PEP 8 / rustfmt), Go to tabs, most web languages to 2 spaces.
    pub default_indent: IndentStyle,
    /// Per-language line-comment prefix (e.g. `"//"`, `"#"`, `"%"`). `None` for languages
    /// without a single-line comment form (markdown, html, css, json).
    pub line_comment: Option<&'static str>,
    /// Per-language block-comment delimiters (`(start, end)`). `None` for languages without a
    /// block form (python, bash, toml, yaml, elixir, erlang, json). Drives mid-line-selection
    /// comment toggling and provides a fallback for languages without `line_comment`.
    pub block_comment: Option<(&'static str, &'static str)>,
}

/// Shared `indents.scm` bodies referenced from per-language `; inherits` directives. Loaded
/// once via `include_str!` and resolved by [`load_indent_query`] when compiling.
fn shared_indent_body(name: &str) -> Option<&'static str> {
    match name {
        "ecma" => Some(include_str!("../queries/ecma/indents.scm")),
        "_typescript" => Some(include_str!("../queries/_typescript/indents.scm")),
        "_jsx" => Some(include_str!("../queries/_jsx/indents.scm")),
        // `_javascript` is referenced by javascript/indents.scm but doesn't exist upstream —
        // Helix's resolver silently skips missing inherits, so we do too.
        _ => None,
    }
}

fn load_indent_query(language: &Language, source: &'static str) -> Option<CompiledIndentQuery> {
    let expanded = expand_inherits(source, shared_indent_body);
    match CompiledIndentQuery::compile(language, &expanded) {
        Ok(iq) => Some(iq),
        Err(e) => {
            tracing::warn!("indent query compile failed: {e}");
            None
        }
    }
}

/// One embedded sub-language span inside a parent buffer (e.g. a `rust` fenced code block
/// inside a markdown file). The `tree` was parsed against `&source[range]`, so its node byte
/// offsets are *slice-relative* (start at 0, not at `range.start`).
pub struct InjectionLayer {
    pub config: &'static LanguageConfig,
    pub range: Range<usize>,
    pub tree: Tree,
}

static RUST: OnceLock<LanguageConfig> = OnceLock::new();
static MARKDOWN: OnceLock<LanguageConfig> = OnceLock::new();
static TOML: OnceLock<LanguageConfig> = OnceLock::new();
static HTML: OnceLock<LanguageConfig> = OnceLock::new();
static JAVASCRIPT: OnceLock<LanguageConfig> = OnceLock::new();
static TYPESCRIPT: OnceLock<LanguageConfig> = OnceLock::new();
static TSX: OnceLock<LanguageConfig> = OnceLock::new();
static PYTHON: OnceLock<LanguageConfig> = OnceLock::new();
static GO: OnceLock<LanguageConfig> = OnceLock::new();
static ELIXIR: OnceLock<LanguageConfig> = OnceLock::new();
static ERLANG: OnceLock<LanguageConfig> = OnceLock::new();
static CSS: OnceLock<LanguageConfig> = OnceLock::new();
static BASH: OnceLock<LanguageConfig> = OnceLock::new();
static JSON: OnceLock<LanguageConfig> = OnceLock::new();
static YAML: OnceLock<LanguageConfig> = OnceLock::new();

/// Everything that distinguishes one injection-free language from another: the grammar, its
/// queries, and the editing metadata copied into the resulting [`LanguageConfig`]. Named fields
/// keep the per-language table in [`get_config`] self-describing.
struct LanguageSpec<L> {
    name: &'static str,
    language: L,
    highlights: &'static str,
    indents: Option<&'static str>,
    default_indent: IndentStyle,
    line_comment: Option<&'static str>,
    block_comment: Option<(&'static str, &'static str)>,
}

fn simple<L: Into<Language>>(
    cell: &'static OnceLock<LanguageConfig>,
    spec: LanguageSpec<L>,
) -> &'static LanguageConfig {
    cell.get_or_init(move || {
        let language: Language = spec.language.into();
        let query = Query::new(&language, spec.highlights)
            .unwrap_or_else(|e| panic!("{} highlights query compiles: {e}", spec.name));
        let indent_query = spec
            .indents
            .and_then(|src| load_indent_query(&language, src));
        LanguageConfig {
            name: spec.name,
            language,
            query,
            injection_query: None,
            indent_query,
            default_indent: spec.default_indent,
            line_comment: spec.line_comment,
            block_comment: spec.block_comment,
        }
    })
}

/// TypeScript's bundled `HIGHLIGHTS_QUERY` carries only the TS-specific additions (types, TS
/// keywords like `interface`/`type`); the base constructs — `const`/`let`/`function`/`return`,
/// strings, numbers, comments, operators — live in the *JavaScript* query, since the TS grammar
/// extends JS. The crate ships them separately, so on its own the TS query leaves almost everything
/// uncoloured. Concatenate the JS base with the TS additions (the JS `highlights.scm` has no JSX
/// captures, so it compiles cleanly against the non-JSX `typescript` grammar as well as `tsx`).
/// Built once and cached; the result outlives the process so it satisfies the `&'static` query slot.
fn typescript_highlights() -> &'static str {
    static QUERY: OnceLock<String> = OnceLock::new();
    QUERY
        .get_or_init(|| {
            format!(
                "{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            )
        })
        .as_str()
}

/// Like [`typescript_highlights`] but for `.tsx`: also append the JS crate's JSX query so markup
/// (tag names, attributes, the `< > />` brackets) is coloured. Those rules reference `jsx_*` node
/// types that exist only in the `tsx` grammar — appending them to the plain `typescript` query
/// would fail to compile — so this third piece is kept TSX-only.
fn tsx_highlights() -> &'static str {
    static QUERY: OnceLock<String> = OnceLock::new();
    QUERY
        .get_or_init(|| {
            format!(
                "{}\n{}\n{}",
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            )
        })
        .as_str()
}

/// Resolve a language name (canonical or alias) to its config. Recognises file extensions
/// (`"rs"`, `"py"`) and common markdown-fence aliases (`"sh"`, `"js"`, `"yml"`) so both the
/// extension-based detection path and injection-language lookups share one table. Input is
/// lowercased; unknown names return `None`.
pub fn get_config(name: &str) -> Option<&'static LanguageConfig> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "rust" | "rs" => Some(simple(
            &RUST,
            LanguageSpec {
                name: "rust",
                language: tree_sitter_rust::LANGUAGE,
                highlights: tree_sitter_rust::HIGHLIGHTS_QUERY,
                indents: Some(include_str!("../queries/rust/indents.scm")),
                default_indent: IndentStyle::Spaces(4),
                line_comment: Some("//"),
                block_comment: Some(("/*", "*/")),
            },
        )),
        "markdown" | "md" => Some(MARKDOWN.get_or_init(|| {
            let language: Language = tree_sitter_md::LANGUAGE.into();
            let query = Query::new(&language, tree_sitter_md::HIGHLIGHT_QUERY_BLOCK)
                .expect("markdown highlights query compiles");
            let injection_query = Query::new(&language, tree_sitter_md::INJECTION_QUERY_BLOCK)
                .expect("markdown injection query compiles");
            LanguageConfig {
                name: "markdown",
                language,
                query,
                injection_query: Some(injection_query),
                indent_query: None,
                default_indent: IndentStyle::Spaces(2),
                line_comment: None,
                block_comment: Some(("<!--", "-->")),
            }
        })),
        "toml" => Some(simple(
            &TOML,
            LanguageSpec {
                name: "toml",
                language: tree_sitter_toml_ng::LANGUAGE,
                highlights: tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
                indents: None,
                default_indent: IndentStyle::Spaces(2),
                line_comment: Some("#"),
                block_comment: None,
            },
        )),
        "html" | "htm" => Some(simple(
            &HTML,
            LanguageSpec {
                name: "html",
                language: tree_sitter_html::LANGUAGE,
                highlights: tree_sitter_html::HIGHLIGHTS_QUERY,
                indents: None,
                default_indent: IndentStyle::Spaces(2),
                line_comment: None,
                block_comment: Some(("<!--", "-->")),
            },
        )),
        "javascript" | "js" | "jsx" | "mjs" | "cjs" => Some(simple(
            &JAVASCRIPT,
            LanguageSpec {
                name: "javascript",
                language: tree_sitter_javascript::LANGUAGE,
                highlights: tree_sitter_javascript::HIGHLIGHT_QUERY,
                indents: Some(include_str!("../queries/javascript/indents.scm")),
                default_indent: IndentStyle::Spaces(2),
                line_comment: Some("//"),
                block_comment: Some(("/*", "*/")),
            },
        )),
        "typescript" | "ts" => Some(simple(
            &TYPESCRIPT,
            LanguageSpec {
                name: "typescript",
                language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                highlights: typescript_highlights(),
                indents: Some(include_str!("../queries/typescript/indents.scm")),
                default_indent: IndentStyle::Spaces(2),
                line_comment: Some("//"),
                block_comment: Some(("/*", "*/")),
            },
        )),
        "tsx" => Some(simple(
            &TSX,
            LanguageSpec {
                name: "tsx",
                language: tree_sitter_typescript::LANGUAGE_TSX,
                highlights: tsx_highlights(),
                indents: Some(include_str!("../queries/tsx/indents.scm")),
                default_indent: IndentStyle::Spaces(2),
                line_comment: Some("//"),
                block_comment: Some(("/*", "*/")),
            },
        )),
        "python" | "py" => Some(simple(
            &PYTHON,
            LanguageSpec {
                name: "python",
                language: tree_sitter_python::LANGUAGE,
                highlights: tree_sitter_python::HIGHLIGHTS_QUERY,
                indents: Some(include_str!("../queries/python/indents.scm")),
                default_indent: IndentStyle::Spaces(4),
                line_comment: Some("#"),
                block_comment: None,
            },
        )),
        "go" | "golang" => Some(simple(
            &GO,
            LanguageSpec {
                name: "go",
                language: tree_sitter_go::LANGUAGE,
                highlights: tree_sitter_go::HIGHLIGHTS_QUERY,
                indents: Some(include_str!("../queries/go/indents.scm")),
                default_indent: IndentStyle::Tab,
                line_comment: Some("//"),
                block_comment: Some(("/*", "*/")),
            },
        )),
        "elixir" | "ex" | "exs" => Some(simple(
            &ELIXIR,
            LanguageSpec {
                name: "elixir",
                language: tree_sitter_elixir::LANGUAGE,
                highlights: tree_sitter_elixir::HIGHLIGHTS_QUERY,
                indents: Some(include_str!("../queries/elixir/indents.scm")),
                default_indent: IndentStyle::Spaces(2),
                line_comment: Some("#"),
                block_comment: None,
            },
        )),
        "erlang" | "erl" | "hrl" => Some(simple(
            &ERLANG,
            LanguageSpec {
                name: "erlang",
                language: tree_sitter_erlang::LANGUAGE,
                highlights: tree_sitter_erlang::HIGHLIGHTS_QUERY,
                indents: None,
                default_indent: IndentStyle::Spaces(4),
                line_comment: Some("%"),
                block_comment: None,
            },
        )),
        "css" => Some(simple(
            &CSS,
            LanguageSpec {
                name: "css",
                language: tree_sitter_css::LANGUAGE,
                highlights: tree_sitter_css::HIGHLIGHTS_QUERY,
                indents: Some(include_str!("../queries/css/indents.scm")),
                default_indent: IndentStyle::Spaces(2),
                line_comment: None,
                block_comment: Some(("/*", "*/")),
            },
        )),
        "bash" | "sh" | "shell" | "zsh" => Some(simple(
            &BASH,
            LanguageSpec {
                name: "bash",
                language: tree_sitter_bash::LANGUAGE,
                highlights: tree_sitter_bash::HIGHLIGHT_QUERY,
                indents: Some(include_str!("../queries/bash/indents.scm")),
                default_indent: IndentStyle::Spaces(2),
                line_comment: Some("#"),
                block_comment: None,
            },
        )),
        "json" => Some(simple(
            &JSON,
            LanguageSpec {
                name: "json",
                language: tree_sitter_json::LANGUAGE,
                highlights: tree_sitter_json::HIGHLIGHTS_QUERY,
                indents: Some(include_str!("../queries/json/indents.scm")),
                default_indent: IndentStyle::Spaces(2),
                line_comment: None,
                block_comment: None,
            },
        )),
        "yaml" | "yml" => Some(simple(
            &YAML,
            LanguageSpec {
                name: "yaml",
                language: tree_sitter_yaml::LANGUAGE,
                highlights: tree_sitter_yaml::HIGHLIGHTS_QUERY,
                indents: Some(include_str!("../queries/yaml/indents.scm")),
                default_indent: IndentStyle::Spaces(2),
                line_comment: Some("#"),
                block_comment: None,
            },
        )),
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

/// Run the parent's injection query and parse each captured content range with its named
/// language. Skips matches whose language is unknown to us (e.g. `markdown_inline`, `html`,
/// `yaml` aren't in our registry yet). Single-level only — injected sub-trees don't themselves
/// contribute further injections.
pub fn compute_injections(
    config: &LanguageConfig,
    tree: &Tree,
    source: &str,
) -> Vec<InjectionLayer> {
    let Some(inj_query) = config.injection_query.as_ref() else {
        return Vec::new();
    };
    let bytes = source.as_bytes();
    let capture_names = inj_query.capture_names();

    let mut layers = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(inj_query, tree.root_node(), bytes);
    while let Some(m) = matches.next() {
        let mut content_range: Option<Range<usize>> = None;
        let mut dyn_language: Option<&str> = None;
        for cap in m.captures {
            let name = capture_names[cap.index as usize];
            if name == "injection.content" {
                content_range = Some(cap.node.start_byte()..cap.node.end_byte());
            } else if name == "injection.language" {
                let s = cap.node.start_byte();
                let e = cap.node.end_byte();
                if let Ok(text) = std::str::from_utf8(&bytes[s..e]) {
                    dyn_language = Some(text.trim());
                }
            }
        }
        let static_language = inj_query
            .property_settings(m.pattern_index)
            .iter()
            .find(|p| &*p.key == "injection.language")
            .and_then(|p| p.value.as_deref());

        let lang_name = dyn_language.or(static_language);
        let (Some(content_range), Some(lang_name)) = (content_range, lang_name) else {
            continue;
        };
        if content_range.is_empty() {
            continue;
        }
        let Some(inj_config) = get_config(lang_name) else {
            continue;
        };

        let mut parser = make_parser(inj_config);
        let slice = &source[content_range.clone()];
        let Some(inj_tree) = parser.parse(slice, None) else {
            continue;
        };
        layers.push(InjectionLayer {
            config: inj_config,
            range: content_range,
            tree: inj_tree,
        });
    }
    layers
}

/// Compute non-overlapping highlight spans for the byte range `[range_start, range_end)` within
/// `source`. The returned highlights' `start`/`end` are **relative to `range_start`** (i.e. they
/// fall in `[0, range_end - range_start)`).
///
/// More-specific (shorter) captures override longer ones at the same byte. Captures of the same
/// length are last-writer-wins by query order. Injection layers whose range intersects the
/// requested window are overlaid on top of the outer captures, so an embedded `rust` block in a
/// markdown file gets rust highlighting in its content region.
pub fn highlights_for_range(
    config: &LanguageConfig,
    tree: &Tree,
    injections: &[InjectionLayer],
    source: &str,
    range_start: usize,
    range_end: usize,
) -> Vec<Highlight> {
    if range_end <= range_start {
        return vec![];
    }
    let span_len = range_end - range_start;
    let mut per_byte: Vec<Option<&'static str>> = vec![None; span_len];

    // Outer pass: query reports source-byte offsets; per_byte index = source_byte - range_start.
    overlay_captures(
        &config.query,
        tree,
        source.as_bytes(),
        range_start..range_end,
        -(range_start as isize),
        &mut per_byte,
    );

    // Injection passes: each query reports slice-local offsets (slice = source[inj.range]);
    // per_byte index = slice_byte + (inj.range.start - range_start).
    for inj in injections {
        let overlap_start = inj.range.start.max(range_start);
        let overlap_end = inj.range.end.min(range_end);
        if overlap_start >= overlap_end {
            continue;
        }
        let slice = &source.as_bytes()[inj.range.start..inj.range.end];
        let local_start = overlap_start - inj.range.start;
        let local_end = overlap_end - inj.range.start;
        overlay_captures(
            &inj.config.query,
            &inj.tree,
            slice,
            local_start..local_end,
            (inj.range.start as isize) - (range_start as isize),
            &mut per_byte,
        );
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

/// Run `query` against `tree` over `bytes_for_query` (which the query's nodes index into),
/// restricted to query-local byte range `query_range`. Each capture's byte interval `[s,e)` is
/// written into `per_byte` at index `s + per_byte_offset` (and likewise for `e`). Longer
/// captures are applied first so shorter, more-specific captures overwrite them.
fn overlay_captures(
    query: &Query,
    tree: &Tree,
    bytes_for_query: &[u8],
    query_range: Range<usize>,
    per_byte_offset: isize,
    per_byte: &mut [Option<&'static str>],
) {
    let capture_names = query.capture_names();
    // (start, end, pattern_index, name). `pattern_index` orders equal-length overlaps so the later
    // query pattern wins — the standard tree-sitter precedence query authors rely on. Match
    // *iteration* order can't stand in for it: a capture whose pattern matches on an enclosing node
    // (e.g. JSX `(jsx_opening_element (identifier) @tag)`, matched at the `<`) is yielded before a
    // bare `(identifier) @variable` on the same name, so without this the broad rule would overwrite
    // the specific one.
    let mut captures: Vec<(usize, usize, usize, &'static str)> = Vec::new();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(query_range.clone());
    let mut matches = cursor.matches(query, tree.root_node(), bytes_for_query);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let name = capture_names[cap.index as usize];
            let s = cap.node.start_byte().max(query_range.start);
            let e = cap.node.end_byte().min(query_range.end);
            if s < e {
                // The underlying string data lives in a `'static` `Query` (held in `OnceLock`);
                // the borrow checker can't see through `&Query`'s lifetime so we widen here.
                let name: &'static str = unsafe { std::mem::transmute::<&str, &'static str>(name) };
                captures.push((s, e, m.pattern_index, name));
            }
        }
    }
    if captures.is_empty() {
        return;
    }
    // Longer captures first (shorter, more-specific ones overwrite); for equal length, the
    // later-defined pattern wins (written last); `start` is a final stable tiebreak.
    captures.sort_by(|a, b| {
        let len_a = a.1 - a.0;
        let len_b = b.1 - b.0;
        len_b.cmp(&len_a).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0))
    });
    for (s, e, _, name) in &captures {
        for i in *s..*e {
            let idx = (i as isize) + per_byte_offset;
            if idx >= 0 && (idx as usize) < per_byte.len() {
                per_byte[idx as usize] = Some(*name);
            }
        }
    }
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
        let highlights = highlights_for_range(cfg, &tree, &[], source, 0, source.len());

        assert!(
            !highlights.is_empty(),
            "expected highlights for Rust source"
        );

        let fn_kw = highlights.iter().find(|h| h.start == 0 && h.end == 2);
        assert!(
            fn_kw.is_some_and(|h| h.kind.contains("keyword")),
            "expected 'fn' to be a keyword, got {:?}",
            fn_kw
        );

        let string_pos = source.find("\"hi\"").unwrap() as u32;
        let has_string = highlights
            .iter()
            .any(|h| h.start <= string_pos && h.end > string_pos && h.kind.contains("string"));
        assert!(has_string, "expected string highlight for \"hi\"");
    }

    #[test]
    fn typescript_highlights_base_js_and_ts_constructs() {
        // The combined JS-base + TS-additions query must compile against the (non-JSX) typescript
        // grammar and colour the base constructs the bundled TS-only query misses.
        let cfg = get_config("typescript").unwrap();
        let mut parser = make_parser(cfg);
        let source = "export const n: number = 42;\nfunction f(s: string) { return s; }\n";
        let tree = parser.parse(source, None).unwrap();
        let highlights = highlights_for_range(cfg, &tree, &[], source, 0, source.len());
        let kind_at = |needle: &str| {
            let pos = source.find(needle).unwrap() as u32;
            highlights
                .iter()
                .find(|h| h.start <= pos && h.end > pos)
                .map(|h| h.kind.clone())
        };
        // Base JS keywords / literals (previously uncoloured) now get captures.
        assert!(
            kind_at("const").is_some_and(|k| k.contains("keyword")),
            "const → keyword"
        );
        assert!(
            kind_at("function").is_some_and(|k| k.contains("keyword")),
            "function → keyword"
        );
        assert!(
            kind_at("return").is_some_and(|k| k.contains("keyword")),
            "return → keyword"
        );
        assert!(
            kind_at("42").is_some_and(|k| k.contains("number")),
            "42 → number"
        );
        // A string literal in a separate snippet (the first has none).
        let src2 = "const greeting = \"hello\";\n";
        let tree2 = parser.parse(src2, None).unwrap();
        let hl2 = highlights_for_range(cfg, &tree2, &[], src2, 0, src2.len());
        let sp = src2.find("\"hello\"").unwrap() as u32;
        assert!(
            hl2.iter()
                .any(|h| h.start <= sp && h.end > sp && h.kind.contains("string")),
            "string literal → string"
        );
        // TS-specific additions still work.
        assert!(
            kind_at("number").is_some_and(|k| k.contains("type")),
            "number → type.builtin"
        );
    }

    #[test]
    fn tsx_highlights_base_and_jsx_markup() {
        // The TSX query (JS base + TS additions + JSX) compiles against the JSX-bearing grammar and
        // colours both ordinary code and the markup the base query leaves plain.
        let cfg = get_config("tsx").unwrap();
        let mut parser = make_parser(cfg);
        let source = "const e = <div className=\"x\">{n}</div>;\n";
        let tree = parser.parse(source, None).unwrap();
        let hl = highlights_for_range(cfg, &tree, &[], source, 0, source.len());
        let kind_at = |needle: &str| {
            let pos = source.find(needle).unwrap() as u32;
            hl.iter()
                .find(|h| h.start <= pos && h.end > pos)
                .map(|h| h.kind.clone())
        };
        assert!(
            kind_at("const").is_some_and(|k| k.contains("keyword")),
            "base code still works"
        );
        // The lowercase HTML tag name and the attribute name get JSX captures.
        assert!(
            kind_at("div").is_some_and(|k| k.contains("tag")),
            "<div> → tag, got {:?}",
            kind_at("div")
        );
        assert!(
            kind_at("className").is_some_and(|k| k.contains("attribute")),
            "className → attribute, got {:?}",
            kind_at("className")
        );
    }

    #[test]
    fn range_filter_clips_highlights() {
        let cfg = get_config("rust").unwrap();
        let mut parser = make_parser(cfg);
        let source = "fn alpha() {}\nfn beta() {}\n";
        let tree = parser.parse(source, None).unwrap();

        let line2_start = source.find("fn beta").unwrap();
        let line2_end = source.find("\nfn beta").map_or(source.len(), |i| i + 13);
        let highlights = highlights_for_range(cfg, &tree, &[], source, line2_start, line2_end);

        for h in &highlights {
            assert!(h.end as usize <= line2_end - line2_start);
        }
        assert!(highlights.iter().any(|h| h.kind.contains("keyword")));
    }

    #[test]
    fn markdown_rust_fence_injects_rust_highlights() {
        let cfg = get_config("markdown").unwrap();
        let mut parser = make_parser(cfg);
        let source = "# Heading\n\n```rust\nfn main() {}\n```\n";
        let tree = parser.parse(source, None).unwrap();

        let injections = compute_injections(cfg, &tree, source);
        assert_eq!(injections.len(), 1, "expected one rust injection layer");
        assert_eq!(injections[0].config.name, "rust");
        let content = &source[injections[0].range.clone()];
        assert!(content.contains("fn main"));
        assert!(!content.contains("```"));

        let highlights = highlights_for_range(cfg, &tree, &injections, source, 0, source.len());

        let fn_byte = source.find("fn ").unwrap() as u32;
        let fn_kw = highlights
            .iter()
            .find(|h| h.start <= fn_byte && h.end > fn_byte && h.kind.contains("keyword"));
        assert!(
            fn_kw.is_some(),
            "expected rust keyword highlight for 'fn' in fence"
        );
    }

    #[test]
    fn unknown_injection_language_is_skipped() {
        let cfg = get_config("markdown").unwrap();
        let mut parser = make_parser(cfg);
        let source = "```nosuchlang\nblah\n```\n";
        let tree = parser.parse(source, None).unwrap();
        let injections = compute_injections(cfg, &tree, source);
        assert!(
            injections.is_empty(),
            "expected no layers for unknown language, got {}",
            injections.len()
        );
    }

    /// Every registered canonical language loads, parses, and produces at least one highlight
    /// span on a small representative snippet. Catches grammar/query ABI mismatches at test
    /// time rather than the first time a user opens a file of that type.
    #[test]
    fn every_language_produces_highlights_for_sample() {
        let cases: &[(&str, &str)] = &[
            ("rust", "fn main() {}"),
            ("markdown", "# hi"),
            ("toml", "x = 1\n"),
            ("html", "<p>hi</p>"),
            ("javascript", "const x = 1;"),
            ("typescript", "const x: number = 1;"),
            ("tsx", "const x: number = 1;"),
            ("python", "def f(): pass\n"),
            ("go", "package main\n"),
            ("elixir", "defmodule M do\nend\n"),
            ("erlang", "-module(m).\n"),
            ("css", "a { color: red; }"),
            ("bash", "echo hi\n"),
            ("json", "{\"a\": 1}"),
            ("yaml", "a: 1\n"),
        ];
        for (lang, source) in cases {
            let cfg =
                get_config(lang).unwrap_or_else(|| panic!("no config registered for `{lang}`"));
            let mut parser = make_parser(cfg);
            let tree = parser
                .parse(source, None)
                .unwrap_or_else(|| panic!("`{lang}` parser produced no tree"));
            let highlights = highlights_for_range(cfg, &tree, &[], source, 0, source.len());
            assert!(
                !highlights.is_empty(),
                "expected at least one highlight span for `{lang}` sample {source:?}",
            );
        }
    }

    /// Aliases (file extensions, markdown-fence short names) resolve to the same registered
    /// config as their canonical name. Same `LanguageConfig` pointer means the OnceLock cell
    /// is shared — important so the markdown injection path can find `rust` from `rs`, etc.
    #[test]
    fn aliases_resolve_to_canonical_config() {
        let pairs: &[(&str, &str)] = &[
            ("rs", "rust"),
            ("md", "markdown"),
            ("py", "python"),
            ("js", "javascript"),
            ("jsx", "javascript"),
            ("mjs", "javascript"),
            ("ts", "typescript"),
            ("yml", "yaml"),
            ("sh", "bash"),
            ("zsh", "bash"),
            ("golang", "go"),
            ("ex", "elixir"),
            ("exs", "elixir"),
            ("erl", "erlang"),
            ("htm", "html"),
        ];
        for (alias, canonical) in pairs {
            let a = get_config(alias).unwrap_or_else(|| panic!("alias `{alias}` not registered"));
            let c = get_config(canonical)
                .unwrap_or_else(|| panic!("canonical `{canonical}` not registered"));
            assert!(
                std::ptr::eq(a, c),
                "`{alias}` should resolve to the same config as `{canonical}`",
            );
        }
        // Case-insensitive too.
        let lower = get_config("python").unwrap();
        let upper = get_config("PYTHON").unwrap();
        assert!(std::ptr::eq(lower, upper));
    }

    #[test]
    fn markdown_fence_with_alias_injects() {
        let cfg = get_config("markdown").unwrap();
        let mut parser = make_parser(cfg);
        // `py` instead of `python`; `sh` instead of `bash`.
        let source = "```py\ndef f(): pass\n```\n\n```sh\necho hi\n```\n";
        let tree = parser.parse(source, None).unwrap();
        let layers = compute_injections(cfg, &tree, source);
        let langs: Vec<_> = layers.iter().map(|l| l.config.name).collect();
        assert!(
            langs.contains(&"python"),
            "expected python layer, got {langs:?}"
        );
        assert!(
            langs.contains(&"bash"),
            "expected bash layer, got {langs:?}"
        );
    }
}
