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
    /// Tree-sitter node kinds that `[` / `]` treat as "navigation units" — the structural
    /// chunks the user wants to skip between (functions, type declarations, HTML elements,
    /// CSS rule sets, etc.). The motion walks up the tree from the cursor until it finds an
    /// ancestor with a child of one of these kinds past (or before) the cursor. Languages
    /// with an empty list have no `[` / `]` navigation.
    pub navigation_kinds: &'static [&'static str],
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

fn simple<L: Into<Language> + Copy>(
    cell: &'static OnceLock<LanguageConfig>,
    name: &'static str,
    language_fn: L,
    highlights: &'static str,
    indents: Option<&'static str>,
    default_indent: IndentStyle,
    line_comment: Option<&'static str>,
    block_comment: Option<(&'static str, &'static str)>,
    navigation_kinds: &'static [&'static str],
) -> &'static LanguageConfig {
    cell.get_or_init(move || {
        let language: Language = language_fn.into();
        let query = Query::new(&language, highlights)
            .unwrap_or_else(|e| panic!("{name} highlights query compiles: {e}"));
        let indent_query = indents.and_then(|src| load_indent_query(&language, src));
        LanguageConfig {
            name,
            language,
            query,
            injection_query: None,
            indent_query,
            default_indent,
            line_comment,
            block_comment,
            navigation_kinds,
        }
    })
}

/// Resolve a language name (canonical or alias) to its config. Recognises file extensions
/// (`"rs"`, `"py"`) and common markdown-fence aliases (`"sh"`, `"js"`, `"yml"`) so both the
/// extension-based detection path and injection-language lookups share one table. Input is
/// lowercased; unknown names return `None`.
pub fn get_config(name: &str) -> Option<&'static LanguageConfig> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "rust" | "rs" => Some(simple(
            &RUST, "rust",
            tree_sitter_rust::LANGUAGE, tree_sitter_rust::HIGHLIGHTS_QUERY,
            Some(include_str!("../queries/rust/indents.scm")),
            IndentStyle::Spaces(4), Some("//"), Some(("/*", "*/")),
            &["function_item", "struct_item", "enum_item", "impl_item", "trait_item",
              "mod_item", "const_item", "static_item", "type_item", "macro_definition"],
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
                navigation_kinds: &["section", "fenced_code_block"],
            }
        })),
        "toml" => Some(simple(
            &TOML, "toml",
            tree_sitter_toml_ng::LANGUAGE, tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            None, IndentStyle::Spaces(2), Some("#"), None,
            &["table", "table_array_element"],
        )),
        "html" | "htm" => Some(simple(
            &HTML, "html",
            tree_sitter_html::LANGUAGE, tree_sitter_html::HIGHLIGHTS_QUERY,
            None, IndentStyle::Spaces(2), None, Some(("<!--", "-->")),
            &["element", "script_element", "style_element"],
        )),
        "javascript" | "js" | "jsx" | "mjs" | "cjs" => Some(simple(
            &JAVASCRIPT, "javascript",
            tree_sitter_javascript::LANGUAGE, tree_sitter_javascript::HIGHLIGHT_QUERY,
            Some(include_str!("../queries/javascript/indents.scm")),
            IndentStyle::Spaces(2), Some("//"), Some(("/*", "*/")),
            &["function_declaration", "class_declaration", "export_statement",
              "lexical_declaration", "variable_declaration", "method_definition"],
        )),
        "typescript" | "ts" => Some(simple(
            &TYPESCRIPT, "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT, tree_sitter_typescript::HIGHLIGHTS_QUERY,
            Some(include_str!("../queries/typescript/indents.scm")),
            IndentStyle::Spaces(2), Some("//"), Some(("/*", "*/")),
            &["function_declaration", "class_declaration", "export_statement",
              "lexical_declaration", "variable_declaration", "method_definition",
              "interface_declaration", "type_alias_declaration", "enum_declaration"],
        )),
        "tsx" => Some(simple(
            &TSX, "tsx",
            tree_sitter_typescript::LANGUAGE_TSX, tree_sitter_typescript::HIGHLIGHTS_QUERY,
            Some(include_str!("../queries/tsx/indents.scm")),
            IndentStyle::Spaces(2), Some("//"), Some(("/*", "*/")),
            &["function_declaration", "class_declaration", "export_statement",
              "lexical_declaration", "variable_declaration", "method_definition",
              "interface_declaration", "type_alias_declaration", "enum_declaration",
              "jsx_element", "jsx_self_closing_element"],
        )),
        "python" | "py" => Some(simple(
            &PYTHON, "python",
            tree_sitter_python::LANGUAGE, tree_sitter_python::HIGHLIGHTS_QUERY,
            Some(include_str!("../queries/python/indents.scm")),
            IndentStyle::Spaces(4), Some("#"), None,
            &["function_definition", "class_definition", "decorated_definition"],
        )),
        "go" | "golang" => Some(simple(
            &GO, "go",
            tree_sitter_go::LANGUAGE, tree_sitter_go::HIGHLIGHTS_QUERY,
            Some(include_str!("../queries/go/indents.scm")),
            IndentStyle::Tab, Some("//"), Some(("/*", "*/")),
            &["function_declaration", "method_declaration", "type_declaration",
              "var_declaration", "const_declaration"],
        )),
        "elixir" | "ex" | "exs" => Some(simple(
            &ELIXIR, "elixir",
            tree_sitter_elixir::LANGUAGE, tree_sitter_elixir::HIGHLIGHTS_QUERY,
            Some(include_str!("../queries/elixir/indents.scm")),
            IndentStyle::Spaces(2), Some("#"), None,
            // Elixir's grammar wraps everything (incl. `def`, `defmodule`, `defp`) in `call`
            // nodes. Coarse but matches reality — refine later by filtering on call name.
            &["call"],
        )),
        "erlang" | "erl" | "hrl" => Some(simple(
            &ERLANG, "erlang",
            tree_sitter_erlang::LANGUAGE, tree_sitter_erlang::HIGHLIGHTS_QUERY,
            None, IndentStyle::Spaces(4), Some("%"), None,
            &["fun_decl", "attribute"],
        )),
        "css" => Some(simple(
            &CSS, "css",
            tree_sitter_css::LANGUAGE, tree_sitter_css::HIGHLIGHTS_QUERY,
            Some(include_str!("../queries/css/indents.scm")),
            IndentStyle::Spaces(2), None, Some(("/*", "*/")),
            &["rule_set", "at_rule", "media_statement", "keyframes_statement",
              "supports_statement"],
        )),
        "bash" | "sh" | "shell" | "zsh" => Some(simple(
            &BASH, "bash",
            tree_sitter_bash::LANGUAGE, tree_sitter_bash::HIGHLIGHT_QUERY,
            Some(include_str!("../queries/bash/indents.scm")),
            IndentStyle::Spaces(2), Some("#"), None,
            &["function_definition", "if_statement", "while_statement", "for_statement",
              "case_statement"],
        )),
        "json" => Some(simple(
            &JSON, "json",
            tree_sitter_json::LANGUAGE, tree_sitter_json::HIGHLIGHTS_QUERY,
            Some(include_str!("../queries/json/indents.scm")),
            IndentStyle::Spaces(2), None, None,
            &["pair"],
        )),
        "yaml" | "yml" => Some(simple(
            &YAML, "yaml",
            tree_sitter_yaml::LANGUAGE, tree_sitter_yaml::HIGHLIGHTS_QUERY,
            Some(include_str!("../queries/yaml/indents.scm")),
            IndentStyle::Spaces(2), Some("#"), None,
            &["block_mapping_pair", "block_sequence_item"],
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
        layers.push(InjectionLayer { config: inj_config, range: content_range, tree: inj_tree });
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
    let mut captures: Vec<(usize, usize, &'static str)> = Vec::new();
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
                captures.push((s, e, name));
            }
        }
    }
    if captures.is_empty() {
        return;
    }
    captures.sort_by(|a, b| {
        let len_a = a.1 - a.0;
        let len_b = b.1 - b.0;
        len_b.cmp(&len_a).then(a.0.cmp(&b.0))
    });
    for (s, e, name) in &captures {
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

        assert!(!highlights.is_empty(), "expected highlights for Rust source");

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
        assert!(fn_kw.is_some(), "expected rust keyword highlight for 'fn' in fence");
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
            let cfg = get_config(lang)
                .unwrap_or_else(|| panic!("no config registered for `{lang}`"));
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
        assert!(langs.contains(&"python"), "expected python layer, got {langs:?}");
        assert!(langs.contains(&"bash"), "expected bash layer, got {langs:?}");
    }
}
