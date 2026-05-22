//! Tree-sitter-driven indent engine, modelled on Helix's `helix-core/src/indent.rs`.
//!
//! Reads an `indents.scm` query (vendored from Helix) and walks the ancestors of the cursor
//! position to compute how many indent levels apply at that point. The capture vocabulary is a
//! v1 subset of Helix's: `@indent`, `@outdent`, `@indent.always`, `@outdent.always`. Skipped
//! for now: `@extend` / `@extend.prevent-once` (matters for Python end-of-block continuations),
//! `@align` / `@anchor` (column-aligned continuations). Predicates: `#same-line?` and
//! `#not-same-line?` are honoured; unknown predicates don't filter the match.

use regex::Regex;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Query, QueryCursor, QueryPredicateArg, Tree};

/// Per-buffer indent unit. Detected on load (with [`detect_indent_style`]) and falls back to a
/// per-language default; smart-indent and the manual indent/dedent handlers both consult it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndentStyle {
    Tab,
    Spaces(u8),
}

impl IndentStyle {
    /// The exact string we'd insert for one indent level. `Cow` so the common widths (tab, 2,
    /// 4, 8 spaces) are returned as static slices without allocating.
    pub fn unit(self) -> Cow<'static, str> {
        match self {
            IndentStyle::Tab => Cow::Borrowed("\t"),
            IndentStyle::Spaces(2) => Cow::Borrowed("  "),
            IndentStyle::Spaces(4) => Cow::Borrowed("    "),
            IndentStyle::Spaces(8) => Cow::Borrowed("        "),
            IndentStyle::Spaces(n) => Cow::Owned(" ".repeat(n as usize)),
        }
    }
}

/// Scan up to the first ~1000 lines of `text` and infer the indent unit. Returns `None` when
/// the buffer has no indented lines to learn from (empty / single-line / all top-level), in
/// which case the caller should fall back to the language's default.
///
/// Heuristic: tabs win if they appear on more leading-character positions than any single space
/// width; otherwise pick the smallest space width that shows up on at least two lines (filters
/// one-off accidental indents).
pub fn detect_indent_style(text: &ropey::Rope) -> Option<IndentStyle> {
    const SCAN_LINES: usize = 1000;
    let line_count = text.len_lines().min(SCAN_LINES);

    let mut tab_count = 0usize;
    let mut space_widths: HashMap<usize, usize> = HashMap::new();
    for i in 0..line_count {
        let line = text.line(i);
        let first = line.chars().next();
        match first {
            Some('\t') => tab_count += 1,
            Some(' ') => {
                let w = 1 + line.chars().skip(1).take_while(|c| *c == ' ').count();
                *space_widths.entry(w).or_insert(0) += 1;
            }
            _ => {}
        }
    }

    let total_space_lines: usize = space_widths.values().sum();
    if tab_count > 0 && tab_count >= total_space_lines {
        return Some(IndentStyle::Tab);
    }
    if total_space_lines == 0 {
        return None;
    }

    // Smallest width that appears ≥2 times — that's the unit step, ignoring stray one-line
    // misindents and deeper levels that are multiples of it.
    let mut widths: Vec<(usize, usize)> = space_widths.into_iter().collect();
    widths.sort_by_key(|(w, _)| *w);
    widths
        .iter()
        .find(|(_, c)| *c >= 2)
        .or_else(|| widths.first())
        .map(|(w, _)| IndentStyle::Spaces(*w as u8))
}

/// Per-pattern compiled metadata derived from the raw `Query`. Resolved once at query-load time
/// so the hot path can lookup by `pattern_index` without re-parsing strings.
pub struct CompiledIndentQuery {
    pub query: Query,
    /// Index = pattern_index. Each slot maps capture-name index → `IndentCapture` if recognised.
    capture_kinds: Vec<HashMap<u32, IndentCapture>>,
    /// Per-pattern scope override (default Tail for @indent/@indent.always, All for @outdent
    /// variants). Driven by `(#set! "scope" "all" | "tail")` directives in the query.
    scope_overrides: Vec<Option<Scope>>,
    /// Per-pattern manual predicates. Built-in tree-sitter predicates (`#eq?`, `#match?`, etc.)
    /// are evaluated by tree-sitter itself; this list contains only the ones we hand-evaluate.
    predicates: Vec<Vec<CompiledPredicate>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndentCapture {
    Indent,
    Outdent,
    IndentAlways,
    OutdentAlways,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    /// Default for `@indent` / `@indent.always`: capture applies only to lines *after* the
    /// captured node's start line.
    Tail,
    /// Default for `@outdent` / `@outdent.always`: capture applies on all lines covered by the
    /// captured node, including its start line.
    All,
}

#[derive(Debug)]
enum CompiledPredicate {
    SameLine { a: u32, b: u32, negated: bool },
}

impl IndentCapture {
    fn default_scope(self) -> Scope {
        match self {
            IndentCapture::Indent | IndentCapture::IndentAlways => Scope::Tail,
            IndentCapture::Outdent | IndentCapture::OutdentAlways => Scope::All,
        }
    }
}

impl CompiledIndentQuery {
    /// Compile an `indents.scm` source against a tree-sitter language. The source may contain
    /// any number of `; inherits <lang>,<lang>...` directives at the top, which the caller is
    /// responsible for resolving (see [`expand_inherits`]).
    pub fn compile(language: &Language, source: &str) -> Result<Self, tree_sitter::QueryError> {
        let query = Query::new(language, source)?;
        let pattern_count = query.pattern_count();
        let capture_names = query.capture_names();

        let mut capture_kinds = Vec::with_capacity(pattern_count);
        let mut scope_overrides = Vec::with_capacity(pattern_count);
        let mut predicates = Vec::with_capacity(pattern_count);

        for p in 0..pattern_count {
            let mut kinds = HashMap::new();
            for (i, name) in capture_names.iter().enumerate() {
                let kind = match *name {
                    "indent" => Some(IndentCapture::Indent),
                    "outdent" => Some(IndentCapture::Outdent),
                    "indent.always" => Some(IndentCapture::IndentAlways),
                    "outdent.always" => Some(IndentCapture::OutdentAlways),
                    _ => None,
                };
                if let Some(k) = kind {
                    kinds.insert(i as u32, k);
                }
            }
            capture_kinds.push(kinds);

            let scope = query
                .property_settings(p)
                .iter()
                .find(|prop| &*prop.key == "scope")
                .and_then(|prop| match prop.value.as_deref() {
                    Some("all") => Some(Scope::All),
                    Some("tail") => Some(Scope::Tail),
                    _ => None,
                });
            scope_overrides.push(scope);

            let mut pat_preds = Vec::new();
            for pred in query.general_predicates(p) {
                let negated = match &*pred.operator {
                    "not-same-line?" => true,
                    "same-line?" => false,
                    // Unknown predicate — don't filter. (Helix supports more, e.g. one-line?,
                    // not-kind-eq?. Adding them is mechanical when a query needs one.)
                    _ => continue,
                };
                if let [QueryPredicateArg::Capture(a), QueryPredicateArg::Capture(b)] = &pred.args[..] {
                    pat_preds.push(CompiledPredicate::SameLine { a: *a, b: *b, negated });
                }
            }
            predicates.push(pat_preds);
        }

        Ok(Self { query, capture_kinds, scope_overrides, predicates })
    }
}

/// Substitute `; inherits foo,bar` directives in `source` with the bodies of the named queries
/// resolved by `resolve`. Modelled on Helix's regex-based preprocessing in
/// `tree-house/highlighter/src/config.rs`. Single-level only; the inherited bodies are not
/// themselves recursively expanded (Helix does, but our vendored set is shallow enough that we
/// don't need recursion in practice).
pub fn expand_inherits(source: &str, resolve: impl Fn(&str) -> Option<&'static str>) -> String {
    static INHERITS: OnceLock<Regex> = OnceLock::new();
    let re = INHERITS.get_or_init(|| {
        Regex::new(r";+\s*inherits\s*:?\s*([a-zA-Z_,()-]+)\s*").expect("inherits regex compiles")
    });
    re.replace_all(source, |caps: &regex::Captures<'_>| {
        let mut out = String::new();
        for lang in caps[1].split(',') {
            let lang = lang.trim();
            if let Some(body) = resolve(lang) {
                out.push('\n');
                out.push_str(body);
                out.push('\n');
            }
        }
        out
    })
    .into_owned()
}

#[derive(Default, Clone, Copy)]
struct LineContribution {
    /// 0 or 1 — multiple `@indent` captures on the same line collapse to a single level.
    indent: i32,
    /// Accumulates without collapse: each `@indent.always` contributes one level.
    indent_always: i32,
    /// 0 or 1 — same collapse rule as `indent`.
    outdent: i32,
    outdent_always: i32,
}

impl LineContribution {
    /// Per-line net level: `indent` and `outdent` *on the same line* cancel each other (rather
    /// than producing -1 or +1); the `always` variants stack regardless.
    fn level(self) -> i32 {
        let (i, o) =
            if self.indent > 0 && self.outdent > 0 { (0, 0) } else { (self.indent, self.outdent) };
        i + self.indent_always - o - self.outdent_always
    }
}

/// Number of indent levels that should apply when inserting a new line at byte `cursor_byte`.
/// `target_line` is the 0-based row where the new content will live (typically the cursor's
/// current line + 1 when called from a "press Enter" path; equal to the cursor's line when
/// re-indenting an existing line).
///
/// Algorithm: walk every ancestor of the cursor's node, bucketing capture contributions by the
/// node's start line, then apply Helix's same-line collapse rules to each line and sum.
pub fn compute_indent_levels(
    iq: &CompiledIndentQuery,
    tree: &Tree,
    source: &[u8],
    cursor_byte: usize,
    target_line: usize,
) -> i32 {
    let root = tree.root_node();
    // Look one byte back: an empty range at byte_pos often lands on the root, but the byte just
    // before the cursor is reliably inside the innermost node we care about.
    let lookup_start = cursor_byte.saturating_sub(1);
    let cursor_node = root
        .descendant_for_byte_range(lookup_start, cursor_byte)
        .unwrap_or(root);

    let mut captures_by_node: HashMap<usize, Vec<(IndentCapture, Scope)>> = HashMap::new();

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&iq.query, root, source);
    while let Some(m) = matches.next() {
        let pattern_index = m.pattern_index;
        if !evaluate_predicates(&iq.predicates[pattern_index], m, source) {
            continue;
        }
        let scope_override = iq.scope_overrides[pattern_index];
        let kinds = &iq.capture_kinds[pattern_index];
        for cap in m.captures {
            if let Some(&ic) = kinds.get(&cap.index) {
                let scope = scope_override.unwrap_or_else(|| ic.default_scope());
                captures_by_node.entry(cap.node.id()).or_default().push((ic, scope));
            }
        }
    }

    let mut per_line: HashMap<usize, LineContribution> = HashMap::new();
    let mut node = Some(cursor_node);
    while let Some(n) = node {
        let n_line = n.start_position().row;
        if let Some(caps) = captures_by_node.get(&n.id()) {
            for (ic, scope) in caps {
                let applies = match scope {
                    Scope::Tail => n_line < target_line,
                    Scope::All => n_line <= target_line,
                };
                if !applies {
                    continue;
                }
                let entry = per_line.entry(n_line).or_default();
                match ic {
                    IndentCapture::Indent => entry.indent = 1,
                    IndentCapture::IndentAlways => entry.indent_always += 1,
                    IndentCapture::Outdent => entry.outdent = 1,
                    IndentCapture::OutdentAlways => entry.outdent_always += 1,
                }
            }
        }
        node = n.parent();
    }

    let total: i32 = per_line.values().map(|c| c.level()).sum();
    total.max(0)
}

fn evaluate_predicates(
    predicates: &[CompiledPredicate],
    m: &tree_sitter::QueryMatch<'_, '_>,
    _source: &[u8],
) -> bool {
    for pred in predicates {
        match pred {
            CompiledPredicate::SameLine { a, b, negated } => {
                let node_a = m.captures.iter().find(|c| c.index == *a).map(|c| c.node);
                let node_b = m.captures.iter().find(|c| c.index == *b).map(|c| c.node);
                let (Some(na), Some(nb)) = (node_a, node_b) else {
                    // Capture missing from this match — treat predicate as undecided, let the
                    // match through. (Captures inside `?`-marked subpatterns can be absent.)
                    continue;
                };
                let same = na.start_position().row == nb.start_position().row;
                let pass = if *negated { !same } else { same };
                if !pass {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_rust(src: &str) -> Tree {
        let language: Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&language).unwrap();
        parser.parse(src, None).unwrap()
    }

    fn rust_iq() -> CompiledIndentQuery {
        let language: Language = tree_sitter_rust::LANGUAGE.into();
        let source = include_str!("../queries/rust/indents.scm");
        CompiledIndentQuery::compile(&language, source).unwrap()
    }

    #[test]
    fn indent_after_function_opener() {
        // `{` is the opener; cursor right after it should produce one level for the new line.
        let src = "fn foo() {}\n";
        let tree = parse_rust(src);
        let iq = rust_iq();
        let cursor_byte = src.find('{').unwrap() + 1;
        let levels = compute_indent_levels(&iq, &tree, src.as_bytes(), cursor_byte, 1);
        assert_eq!(levels, 1, "expected one level inside fn body");
    }

    #[test]
    fn engine_returns_zero_for_incomplete_open_brace() {
        // Documents the engine's contract: for an unmatched `{` the parser yields an ERROR
        // node, no `block` is produced, and no @indent fires. The handler layer adds a
        // heuristic floor on top — see `compute_smart_indent` and the
        // `newline_and_indent_adds_one_level_after_opening_brace` integration test.
        let src = "fn foo() {\n";
        let tree = parse_rust(src);
        let iq = rust_iq();
        let cursor_byte = src.find('{').unwrap() + 1;
        let levels = compute_indent_levels(&iq, &tree, src.as_bytes(), cursor_byte, 1);
        assert_eq!(levels, 0, "engine should not bandage incomplete parses on its own");
    }

    #[test]
    fn indent_nested_two_levels() {
        let src = "fn foo() {\n    if x {\n        bar();\n    }\n}\n";
        let tree = parse_rust(src);
        let iq = rust_iq();
        // Cursor at end of `bar();` (line 2 — `bar();` is at byte 32; the `;` is at the end).
        let cursor_byte = src.find("bar();").unwrap() + "bar();".len();
        let levels = compute_indent_levels(&iq, &tree, src.as_bytes(), cursor_byte, 3);
        assert_eq!(levels, 2);
    }

    #[test]
    fn outdent_on_closing_brace_cancels_one_level() {
        // Cursor just past `}` — its @outdent is now an ancestor of the cursor lookup, so it
        // cancels the enclosing block's @indent and the new line returns to depth 0.
        let src = "fn foo() {\n    let x = 1;\n}\n";
        let tree = parse_rust(src);
        let iq = rust_iq();
        let cursor_byte = src.rfind('}').unwrap() + 1;
        let levels = compute_indent_levels(&iq, &tree, src.as_bytes(), cursor_byte, 3);
        assert_eq!(levels, 0, "post-closing-brace line should outdent to 0");
    }

    // ---- indent style detection ----------------------------------------------------------------

    #[test]
    fn detect_four_space_indent() {
        let rope = ropey::Rope::from_str("fn foo() {\n    let x = 1;\n    let y = 2;\n}\n");
        assert_eq!(detect_indent_style(&rope), Some(IndentStyle::Spaces(4)));
    }

    #[test]
    fn detect_two_space_indent() {
        let rope = ropey::Rope::from_str("a:\n  b: 1\n  c: 2\n");
        assert_eq!(detect_indent_style(&rope), Some(IndentStyle::Spaces(2)));
    }

    #[test]
    fn detect_tab_indent() {
        let rope = ropey::Rope::from_str("func foo() {\n\treturn 1\n\treturn 2\n}\n");
        assert_eq!(detect_indent_style(&rope), Some(IndentStyle::Tab));
    }

    #[test]
    fn detect_returns_none_for_unindented_text() {
        let rope = ropey::Rope::from_str("hello world\nno indent here\n");
        assert_eq!(detect_indent_style(&rope), None);
    }

    #[test]
    fn detect_ignores_one_off_misindent() {
        // A single 3-space line shouldn't beat the consistent 2-space body.
        let rope = ropey::Rope::from_str("a:\n  b: 1\n   stray\n  c: 2\n  d: 3\n");
        assert_eq!(detect_indent_style(&rope), Some(IndentStyle::Spaces(2)));
    }

    #[test]
    fn indent_style_unit_returns_static_for_common_widths() {
        use std::borrow::Cow;
        assert!(matches!(IndentStyle::Tab.unit(), Cow::Borrowed("\t")));
        assert!(matches!(IndentStyle::Spaces(2).unit(), Cow::Borrowed("  ")));
        assert!(matches!(IndentStyle::Spaces(4).unit(), Cow::Borrowed("    ")));
        // Uncommon widths fall back to owned strings — still correct, just allocate.
        assert_eq!(IndentStyle::Spaces(3).unit(), "   ");
    }

    #[test]
    fn blank_indented_line_keeps_indent() {
        // Cursor on a blank line *inside* the block: pressing Enter should preserve the depth
        // (we're still inside the function body).
        let src = "fn foo() {\n    let x = 1;\n    \n}\n";
        let tree = parse_rust(src);
        let iq = rust_iq();
        let blank_line_start = src.rfind("    \n").unwrap();
        let cursor_byte = blank_line_start + 4; // end of the 4-space indent on the blank line
        let levels = compute_indent_levels(&iq, &tree, src.as_bytes(), cursor_byte, 3);
        assert_eq!(levels, 1);
    }
}
