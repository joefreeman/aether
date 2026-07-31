//! Tree-sitter integration: language registry, parsing, and per-range highlight computation.

use crate::indent::{expand_inherits, CompiledIndentQuery, IndentStyle};
use aether_protocol::viewport::Highlight;
use std::ops::Range;
use std::path::Path;
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
static MARKDOWN_INLINE: OnceLock<LanguageConfig> = OnceLock::new();
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
static QUIVER: OnceLock<LanguageConfig> = OnceLock::new();
static SQL: OnceLock<LanguageConfig> = OnceLock::new();
static TERRAFORM: OnceLock<LanguageConfig> = OnceLock::new();
static DOCKERFILE: OnceLock<LanguageConfig> = OnceLock::new();

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

/// Resolve a language name (canonical or alias) to its config. The alias arms below **are** the
/// extension table: every extension we detect (`"rs"`, `"py"`, `"tfvars"`) is listed as an alias
/// alongside the markdown-fence short names (`"sh"`, `"js"`, `"yml"`), so extension-based
/// detection ([`config_for_path`]), injection-language lookups, and an explicit `language` on
/// `buffer/open` all resolve through this one table. Input is lowercased; unknown names return
/// `None`.
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
        // `tree-sitter-md` is two grammars: the block grammar above parses document structure
        // (headings, fenced-code fences, lists), and its injection query injects this inline
        // grammar over every `(inline)` node — that's where emphasis, strong, code spans, and
        // links come from. Registered here so [`compute_injections`] can resolve the
        // `markdown_inline` injection language rather than silently skipping it. Its own
        // injection query (html/latex over inline spans) is wired up too, though it only fires
        // for languages we also have registered.
        "markdown_inline" => Some(MARKDOWN_INLINE.get_or_init(|| {
            let language: Language = tree_sitter_md::INLINE_LANGUAGE.into();
            let query = Query::new(&language, tree_sitter_md::HIGHLIGHT_QUERY_INLINE)
                .expect("markdown inline highlights query compiles");
            let injection_query = Query::new(&language, tree_sitter_md::INJECTION_QUERY_INLINE)
                .expect("markdown inline injection query compiles");
            LanguageConfig {
                name: "markdown_inline",
                language,
                query,
                injection_query: Some(injection_query),
                indent_query: None,
                default_indent: IndentStyle::Spaces(2),
                line_comment: None,
                block_comment: None,
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
        "quiver" | "qv" => Some(simple(
            &QUIVER,
            LanguageSpec {
                name: "quiver",
                language: tree_sitter_quiver::LANGUAGE,
                highlights: tree_sitter_quiver::HIGHLIGHTS_QUERY,
                // First-party grammar: highlights and indents both ship from its crate
                // (rather than the vendored `queries/<lang>/` copies used for third-party
                // grammars), so they stay in lock-step with the grammar.
                indents: Some(tree_sitter_quiver::INDENTS_QUERY),
                default_indent: IndentStyle::Spaces(2),
                line_comment: Some("//"),
                block_comment: None,
            },
        )),
        "sql" => Some(simple(
            &SQL,
            LanguageSpec {
                name: "sql",
                language: tree_sitter_sequel::LANGUAGE,
                highlights: tree_sitter_sequel::HIGHLIGHTS_QUERY,
                // The crate's bundled `indents.scm` uses the `@indent.begin/branch/end` dialect,
                // which our Helix-style indent engine doesn't recognise — leave it off and fall
                // back to copying the previous line's indent.
                indents: None,
                default_indent: IndentStyle::Spaces(2),
                line_comment: Some("--"),
                block_comment: Some(("/*", "*/")),
            },
        )),
        "terraform" | "hcl" | "tf" | "tfvars" => Some(simple(
            &TERRAFORM,
            LanguageSpec {
                name: "terraform",
                language: tree_sitter_hcl::LANGUAGE,
                // The `tree-sitter-hcl` crate ships no queries; both are vendored under
                // `queries/terraform/` (highlights adapted from nvim-treesitter, indents from Helix).
                highlights: include_str!("../queries/terraform/highlights.scm"),
                indents: Some(include_str!("../queries/terraform/indents.scm")),
                default_indent: IndentStyle::Spaces(2),
                line_comment: Some("#"),
                block_comment: Some(("/*", "*/")),
            },
        )),
        // Registered by hand rather than via [`simple`] because the grammar ships an injections
        // query: `RUN` bodies and `RUN <<EOF` heredocs parse as bash, and `COPY <<EOF file.json`
        // heredocs as json/yaml/toml. (Its `comment`/`xml` injections name languages we don't
        // register, so [`compute_injections`] skips them.)
        "dockerfile" | "docker" | "containerfile" => Some(DOCKERFILE.get_or_init(|| {
            let language: Language = tree_sitter_containerfile::LANGUAGE.into();
            let query = Query::new(&language, tree_sitter_containerfile::HIGHLIGHTS_QUERY)
                .expect("dockerfile highlights query compiles");
            let injection_query =
                Query::new(&language, tree_sitter_containerfile::INJECTIONS_QUERY)
                    .expect("dockerfile injection query compiles");
            LanguageConfig {
                name: "dockerfile",
                language,
                query,
                injection_query: Some(injection_query),
                indent_query: None,
                default_indent: IndentStyle::Spaces(2),
                line_comment: Some("#"),
                block_comment: None,
            }
        })),
        _ => None,
    }
}

/// How a file *name* selects a language, for the cases an extension can't express. Matched
/// against the lowercased name, so the literals in [`FILE_RULES`] must themselves be lowercase.
enum FileRule {
    /// The complete file name — the rule for names that carry a dot of their own (`go.mod`,
    /// `cargo.lock`). Nothing needs it yet, so only the tests construct it.
    #[allow(dead_code)]
    Name(&'static str),
    /// The part before the first `.`, so per-target variants come along: `dockerfile` matches
    /// `Dockerfile`, `Dockerfile.dev` and `Dockerfile.prod` alike.
    Stem(&'static str),
}

impl FileRule {
    /// Does this rule select `name`? `name` must already be lowercased (see [`FILE_RULES`]).
    fn matches(&self, name: &str) -> bool {
        match self {
            FileRule::Name(n) => name == *n,
            FileRule::Stem(s) => name.split('.').next() == Some(*s),
        }
    }
}

/// File names that select a language regardless of extension, each paired with a name
/// [`get_config`] accepts. Ordered — the first matching rule wins.
///
/// Deliberately short: this is only for names the extension table *can't* express. A Dockerfile
/// has no extension in its canonical spelling, and `Dockerfile.dev`'s extension is `dev`; the
/// mirrored `web.Dockerfile` spelling needs no rule because `dockerfile` is a normal alias.
const FILE_RULES: &[(FileRule, &str)] = &[
    (FileRule::Stem("dockerfile"), "dockerfile"),
    (FileRule::Stem("containerfile"), "dockerfile"),
];

/// The language config for a file on disk: its name against [`FILE_RULES`] first, then its
/// extension through [`get_config`]. `None` when neither is registered.
///
/// The single detection path — callers wanting just the language *name* take `.name` off the
/// result, which keeps detection and the registry from drifting apart.
pub fn config_for_path(path: &Path) -> Option<&'static LanguageConfig> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    for (rule, language) in FILE_RULES {
        if rule.matches(&name) {
            return get_config(language);
        }
    }
    get_config(path.extension()?.to_str()?)
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
/// A `#set! "priority"` directive wins first (default 100); otherwise more-specific (shorter)
/// captures override longer ones at the same byte, and captures of the same length are
/// last-writer-wins by query order. Injection layers whose range intersects the requested window
/// are overlaid on top of the outer captures, so an embedded `rust` block in a markdown file gets
/// rust highlighting in its content region.
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
/// written into `per_byte` at index `s + per_byte_offset` (and likewise for `e`). Resolution is
/// by `#set! "priority"` first (default 100), then — among equal priority — longer captures are
/// applied first so shorter, more-specific captures overwrite them.
fn overlay_captures(
    query: &Query,
    tree: &Tree,
    bytes_for_query: &[u8],
    query_range: Range<usize>,
    per_byte_offset: isize,
    per_byte: &mut [Option<&'static str>],
) {
    let capture_names = query.capture_names();
    // Per-pattern highlight priority from a `#set! "priority" <n>` directive (default 100), the
    // standard tree-sitter escape hatch. A higher priority wins regardless of span length, letting
    // a query keep a broad capture (e.g. a whole `%mod/path` import) from being clobbered by a
    // narrower generic fallback (`(identifier) @variable`) nested inside it.
    let priorities: Vec<i32> = (0..query.pattern_count())
        .map(|i| {
            query
                .property_settings(i)
                .iter()
                .find(|p| &*p.key == "priority")
                .and_then(|p| p.value.as_deref())
                .and_then(|v| v.parse().ok())
                .unwrap_or(100)
        })
        .collect();
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
            // `@spell`/`@nospell` mark prose for an editor's spell checker; they name no colour and
            // ride along *after* the real capture on the same node (`(comment) @comment @spell` in
            // the dockerfile grammar). Left in, the equal-length tiebreak would let them overwrite
            // the capture they accompany and the node would render unstyled.
            if matches!(name, "spell" | "nospell") {
                continue;
            }
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
    // Lower priority first so higher priority is written last (and wins). Within equal priority:
    // longer captures first (shorter, more-specific ones overwrite); for equal length, the
    // later-defined pattern wins (written last); `start` is a final stable tiebreak.
    captures.sort_by(|a, b| {
        let len_a = a.1 - a.0;
        let len_b = b.1 - b.0;
        priorities[a.2]
            .cmp(&priorities[b.2])
            .then(len_b.cmp(&len_a))
            .then(a.2.cmp(&b.2))
            .then(a.0.cmp(&b.0))
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
    fn quiver_module_import_is_one_priority_span() {
        // `%mathx/vec` must render as a single `module` span: the grammar's `(import) @module`
        // carries `#set! "priority" 110`, so it beats the narrower `(identifier) @variable`
        // fallback on each segment and the `/` operator rule that overlap inside it. Without
        // priority support these would clobber the segments, leaving only the bare `%` coloured.
        let cfg = get_config("quiver").unwrap();
        let mut parser = make_parser(cfg);
        let source = "x = %mathx/vec";
        let tree = parser.parse(source, None).unwrap();
        let highlights = highlights_for_range(cfg, &tree, &[], source, 0, source.len());

        let import_start = source.find('%').unwrap() as u32;
        let import_end = source.len() as u32;
        let module = highlights
            .iter()
            .find(|h| h.start == import_start && h.kind.contains("module"));
        assert!(
            module.is_some_and(|h| h.end == import_end),
            "expected one module span over `%mathx/vec`, got {:?}",
            highlights
        );
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
    fn quiver_highlights_types_strings_and_constructors() {
        let cfg = get_config("quiver").unwrap();
        let mut parser = make_parser(cfg);
        let source = "// a point\n'p = Point[x: 'int]\ngreet = #{ \"hi\" }\n";
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "Quiver sample parses cleanly"
        );
        let highlights = highlights_for_range(cfg, &tree, &[], source, 0, source.len());
        let kind_at = |needle: &str| {
            let pos = source.find(needle).unwrap() as u32;
            highlights
                .iter()
                .find(|h| h.start <= pos && h.end > pos)
                .map(|h| h.kind.clone())
        };
        assert!(
            kind_at("// a point").is_some_and(|k| k.contains("comment")),
            "line comment → comment"
        );
        assert!(
            kind_at("'p").is_some_and(|k| k.contains("type")),
            "'p type name → type"
        );
        assert!(
            kind_at("Point").is_some_and(|k| k.contains("constructor")),
            "Point → constructor"
        );
        assert!(
            kind_at("'int").is_some_and(|k| k.contains("type")),
            "'int → type"
        );
        assert!(
            kind_at("\"hi\"").is_some_and(|k| k.contains("string")),
            "string literal → string"
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
        // The heading text also injects a `markdown_inline` layer, so filter to the rust fence.
        let rust = injections
            .iter()
            .find(|l| l.config.name == "rust")
            .expect("expected a rust injection layer");
        let content = &source[rust.range.clone()];
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
    fn config_for_path_detects_by_extension_then_name() {
        let lang = |p: &str| config_for_path(Path::new(p)).map(|c| c.name);
        // Extensions resolve through the registry's alias arms — no separate table.
        assert_eq!(lang("/w/src/main.rs"), Some("rust"));
        assert_eq!(lang("/w/a.py"), Some("python"));
        assert_eq!(lang("/w/vars.tfvars"), Some("terraform"));
        assert_eq!(lang("/w/component.tsx"), Some("tsx"));
        assert_eq!(lang("/w/README.MD"), Some("markdown"));
        // File-name rules win over the extension, and cover the extensionless spelling.
        assert_eq!(lang("/w/Dockerfile"), Some("dockerfile"));
        assert_eq!(lang("/w/Dockerfile.dev"), Some("dockerfile"));
        assert_eq!(lang("/w/Containerfile"), Some("dockerfile"));
        assert_eq!(lang("/w/web.Dockerfile"), Some("dockerfile"));
        // Neither rule nor alias: no grammar.
        assert_eq!(lang("/w/.dockerignore"), None);
        assert_eq!(lang("/w/notes.xyz"), None);
        assert_eq!(lang("/w/LICENSE"), None);
        assert_eq!(lang("/w/some/dir/"), None);
    }

    #[test]
    fn file_rule_matching() {
        // `Name` is exact — the rule for names carrying a dot of their own.
        assert!(FileRule::Name("go.mod").matches("go.mod"));
        assert!(!FileRule::Name("go.mod").matches("go.sum"));
        assert!(!FileRule::Name("makefile").matches("makefile.inc"));
        // `Stem` ignores everything from the first dot on.
        assert!(FileRule::Stem("dockerfile").matches("dockerfile"));
        assert!(FileRule::Stem("dockerfile").matches("dockerfile.dev"));
        assert!(!FileRule::Stem("dockerfile").matches("web.dockerfile"));
        assert!(!FileRule::Stem("dockerfile").matches("dockerfiles"));
    }

    /// [`FILE_RULES`] is matched against a lowercased file name and resolved through
    /// [`get_config`], so a mixed-case literal or a typo'd language name would silently never
    /// fire.
    #[test]
    fn file_rules_are_lowercase_and_name_registered_languages() {
        for (rule, language) in FILE_RULES {
            let literal = match rule {
                FileRule::Name(n) | FileRule::Stem(n) => *n,
            };
            assert_eq!(
                literal,
                literal.to_ascii_lowercase(),
                "`{literal}` can never match: rules are compared against a lowercased file name",
            );
            assert!(
                get_config(language).is_some(),
                "rule for `{literal}` names unregistered language `{language}`",
            );
        }
    }

    #[test]
    fn dockerfile_highlights_instructions_and_comments() {
        let cfg = get_config("dockerfile").unwrap();
        let mut parser = make_parser(cfg);
        let source = "# base image\nFROM rust:1 AS build\nENV RUST_LOG=debug\nEXPOSE 8080\n";
        let tree = parser.parse(source, None).unwrap();
        let highlights = highlights_for_range(cfg, &tree, &[], source, 0, source.len());
        let kind_at = |needle: &str| {
            let pos = source.find(needle).unwrap() as u32;
            highlights
                .iter()
                .find(|h| h.start <= pos && h.end > pos)
                .map(|h| h.kind.clone())
        };
        assert!(
            kind_at("FROM").is_some_and(|k| k.contains("keyword")),
            "FROM → keyword, got {:?}",
            kind_at("FROM")
        );
        assert!(
            kind_at("AS").is_some_and(|k| k.contains("keyword")),
            "AS → keyword"
        );
        // The grammar's rule is `(comment) @comment @spell`: two captures over the identical span,
        // so whichever is applied last wins. `spell` is an annotation no theme colours, so if it
        // won here comments would silently render unstyled.
        assert!(
            kind_at("# base").is_some_and(|k| k.contains("comment")),
            "comment → comment, got {:?}",
            kind_at("# base")
        );
        assert!(
            kind_at("8080").is_some_and(|k| k.contains("number")),
            "exposed port → number"
        );
    }

    #[test]
    fn dockerfile_run_body_injects_bash() {
        // The grammar injects bash over each `RUN` body, so shell keywords inside an instruction
        // get shell colouring rather than being one flat run of text.
        let cfg = get_config("dockerfile").unwrap();
        let mut parser = make_parser(cfg);
        let source = "FROM alpine\nRUN if [ -f /tmp/x ]; then echo hi; fi\n";
        let tree = parser.parse(source, None).unwrap();
        let injections = compute_injections(cfg, &tree, source);
        assert!(
            injections.iter().any(|l| l.config.name == "bash"),
            "expected a bash injection layer, got {:?}",
            injections.iter().map(|l| l.config.name).collect::<Vec<_>>()
        );
        let highlights = highlights_for_range(cfg, &tree, &injections, source, 0, source.len());
        let then_byte = source.find("then").unwrap() as u32;
        assert!(
            highlights
                .iter()
                .any(|h| h.start <= then_byte && h.end > then_byte && h.kind.contains("keyword")),
            "expected bash keyword highlight for `then` inside the RUN body",
        );
    }

    #[test]
    fn markdown_inline_injects_emphasis_strong_and_code_span() {
        // The block grammar emits an `(inline)` injection over running text; resolving it to the
        // `markdown_inline` grammar is what produces emphasis / strong / code-span captures.
        let cfg = get_config("markdown").unwrap();
        let mut parser = make_parser(cfg);
        let source = "Some *italic*, **bold**, and `code` here.\n";
        let tree = parser.parse(source, None).unwrap();

        let injections = compute_injections(cfg, &tree, source);
        assert!(
            injections
                .iter()
                .any(|l| l.config.name == "markdown_inline"),
            "expected a markdown_inline injection layer, got {:?}",
            injections.iter().map(|l| l.config.name).collect::<Vec<_>>()
        );

        let highlights = highlights_for_range(cfg, &tree, &injections, source, 0, source.len());
        let kind_at = |needle: &str| {
            let pos = source.find(needle).unwrap() as u32;
            highlights
                .iter()
                .find(|h| h.start <= pos && h.end > pos)
                .map(|h| h.kind.clone())
        };
        assert!(
            kind_at("italic").is_some_and(|k| k == "text.emphasis"),
            "*italic* → text.emphasis, got {:?}",
            kind_at("italic")
        );
        assert!(
            kind_at("bold").is_some_and(|k| k == "text.strong"),
            "**bold** → text.strong, got {:?}",
            kind_at("bold")
        );
        assert!(
            kind_at("code").is_some_and(|k| k == "text.literal"),
            "`code` → text.literal, got {:?}",
            kind_at("code")
        );
    }

    #[test]
    fn markdown_inline_resolves_as_its_own_language() {
        // Registered so the injection path finds it; standalone it highlights inline constructs.
        let cfg = get_config("markdown_inline").unwrap();
        let mut parser = make_parser(cfg);
        let source = "**bold**";
        let tree = parser.parse(source, None).unwrap();
        let highlights = highlights_for_range(cfg, &tree, &[], source, 0, source.len());
        assert!(
            highlights.iter().any(|h| h.kind == "text.strong"),
            "standalone markdown_inline should highlight strong, got {highlights:?}"
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
            ("markdown_inline", "*emphasis* and `code`\n"),
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
            ("quiver", "double = #'int { [~, 2] %math.mul }\n"),
            ("sql", "SELECT id FROM users WHERE id = 1;"),
            (
                "terraform",
                "resource \"aws_instance\" \"web\" {\n  count = 1\n}\n",
            ),
            ("dockerfile", "FROM alpine:3\nRUN echo hi\n"),
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
            ("qv", "quiver"),
            ("tf", "terraform"),
            ("tfvars", "terraform"),
            ("hcl", "terraform"),
            ("docker", "dockerfile"),
            ("containerfile", "dockerfile"),
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
