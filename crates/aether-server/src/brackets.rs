//! Tree-sitter-driven bracket matching: given a buffer + cursor byte, find the matching
//! bracket pair. Used both to render a "match-bracket" overlay and to power the `m` motion.

use tree_sitter::{Node, Tree};

/// Bracket token kinds. tree-sitter grammars uniformly name these tokens by their literal
/// character, so the same set works across rust / js / python / etc.
fn is_bracket(kind: &str) -> bool {
    matches!(kind, "{" | "}" | "(" | ")" | "[" | "]")
}

/// Find the matching bracket pair for the cursor position `byte`. Returns the byte offsets of
/// the two brackets as `(open_byte, close_byte)` regardless of which one the cursor was on.
///
/// Two paths:
/// - Cursor *on* a bracket token: walk the parent's children to find the sibling whose kind
///   is the mirror character.
/// - Cursor *inside* a bracket-bounded construct (e.g. between `{` and `}`): walk ancestors
///   looking for one whose first and last children are both bracket tokens.
pub fn find_match_bracket(tree: &Tree, byte: usize) -> Option<(usize, usize)> {
    let root = tree.root_node();
    let here = root.descendant_for_byte_range(byte, byte + 1)?;

    if is_bracket(here.kind()) {
        return pair_for_bracket_token(here);
    }

    let mut node = Some(here);
    while let Some(n) = node {
        if let Some(pair) = enclosing_pair(n) {
            return Some(pair);
        }
        node = n.parent();
    }
    None
}

fn pair_for_bracket_token(bracket: Node<'_>) -> Option<(usize, usize)> {
    let parent = bracket.parent()?;
    let want = mirror(bracket.kind())?;
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.kind() == want {
            let (a, b) = (bracket.start_byte(), child.start_byte());
            return Some(if a < b { (a, b) } else { (b, a) });
        }
    }
    None
}

/// Scan all of `node`'s direct children for a bracket pair. Returns the *first* opener and
/// the *last* matching closer, so a node like Go's `index_expression`
/// (`[receiver, "[", index, "]"]`) is recognised even though the brackets aren't its first
/// and last children. Limiting the search to direct children (not transitively descended
/// brackets) keeps the walk anchored on the smallest enclosing construct.
fn enclosing_pair(node: Node<'_>) -> Option<(usize, usize)> {
    let mut cursor = node.walk();
    let mut opener: Option<Node<'_>> = None;
    let mut closer: Option<Node<'_>> = None;
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if opener.is_none() && matches!(kind, "{" | "(" | "[") {
            opener = Some(child);
        } else if let Some(o) = opener {
            if mirror(o.kind()) == Some(kind) {
                closer = Some(child);
            }
        }
    }
    let (o, c) = (opener?, closer?);
    Some((o.start_byte(), c.start_byte()))
}

fn mirror(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "{" => "}",
        "}" => "{",
        "(" => ")",
        ")" => "(",
        "[" => "]",
        "]" => "[",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_rust(src: &str) -> Tree {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&language).unwrap();
        parser.parse(src, None).unwrap()
    }

    #[test]
    fn cursor_on_opening_brace_finds_closing() {
        let src = "fn foo() { let x = 1; }";
        let tree = parse_rust(src);
        let open = src.find('{').unwrap();
        let close = src.rfind('}').unwrap();
        assert_eq!(find_match_bracket(&tree, open), Some((open, close)));
    }

    #[test]
    fn cursor_on_closing_brace_finds_opening() {
        let src = "fn foo() { let x = 1; }";
        let tree = parse_rust(src);
        let open = src.find('{').unwrap();
        let close = src.rfind('}').unwrap();
        assert_eq!(find_match_bracket(&tree, close), Some((open, close)));
    }

    #[test]
    fn cursor_inside_block_finds_enclosing_pair() {
        let src = "fn foo() { let x = 1; }";
        let tree = parse_rust(src);
        let open = src.find('{').unwrap();
        let close = src.rfind('}').unwrap();
        let inside = src.find("let").unwrap();
        assert_eq!(find_match_bracket(&tree, inside), Some((open, close)));
    }

    #[test]
    fn nested_brackets_pick_the_innermost_enclosing_pair() {
        let src = "fn foo() { if true { bar(); } }";
        let tree = parse_rust(src);
        let inner_open = src.find("{ bar").unwrap(); // the inner `{`
        let inner_close = src.find("} }").unwrap(); // the inner `}`
        let inside_inner = src.find("bar").unwrap();
        assert_eq!(
            find_match_bracket(&tree, inside_inner),
            Some((inner_open, inner_close))
        );
    }

    #[test]
    fn cursor_outside_any_pair_returns_none() {
        // Top-level identifier; not inside any bracket. The walk should reach source_file
        // without finding a bracket-bounded ancestor.
        let src = "fn foo() {}";
        let tree = parse_rust(src);
        let cursor = src.find("fn").unwrap();
        assert_eq!(find_match_bracket(&tree, cursor), None);
    }

    #[test]
    fn cursor_in_index_brackets_finds_brackets_not_outer_block() {
        // Regression: a node like `index_expression` has children
        // [receiver, "[", index, "]"], so its brackets aren't at first/last positions. The
        // walk must still stop here rather than continuing up to the enclosing `{}`.
        let src = "fn foo() { let x = arr[42]; }";
        let tree = parse_rust(src);
        let open = src.find('[').unwrap();
        let close = src.find(']').unwrap();
        let inside = src.find("42").unwrap();
        assert_eq!(find_match_bracket(&tree, inside), Some((open, close)));
    }

    #[test]
    fn matches_parens_too() {
        let src = "fn foo(a: u32) {}";
        let tree = parse_rust(src);
        let open = src.find('(').unwrap();
        let close = src.find(')').unwrap();
        assert_eq!(find_match_bracket(&tree, open), Some((open, close)));
        assert_eq!(find_match_bracket(&tree, close), Some((open, close)));
    }
}
