//! Surround/unsurround delimiter tables. Pure char logic with no buffer or tree-sitter
//! awareness — unlike `brackets.rs`, which walks the tree-sitter tree to match brackets, surround
//! works purely on the chars hugging the selection. The same pair table powers both directions:
//! `input/surround` maps a typed delimiter key to its open/close chars, and `input/unsurround`
//! asks whether the two chars on either side of the selection form a strippable pair.

/// The canonical (open, close) pairs recognised by unsurround. Symmetric delimiters (quotes,
/// backtick) have `open == close`.
const PAIRS: &[(char, char)] = &[
    ('(', ')'),
    ('{', '}'),
    ('[', ']'),
    ('<', '>'),
    ('"', '"'),
    ('\'', '\''),
    ('`', '`'),
];

/// Resolve the delimiter key typed after `s` to its `(open, close)` chars. Accepts either member
/// of a bracket pair (so `s )` behaves like `s (`), the vim-style aliases `b`/`B`/`r`/`a`, and the
/// symmetric quotes. Returns `None` for any other key, which makes `s <junk>` a no-op.
pub fn open_close(key: char) -> Option<(char, char)> {
    Some(match key {
        '(' | ')' | 'b' => ('(', ')'),
        '{' | '}' | 'B' => ('{', '}'),
        '[' | ']' | 'r' => ('[', ']'),
        '<' | '>' | 'a' => ('<', '>'),
        '"' => ('"', '"'),
        '\'' => ('\'', '\''),
        '`' => ('`', '`'),
        _ => return None,
    })
}

/// Whether `left`/`right` are the open/close chars of a known pair. This is the gate unsurround
/// uses on the two chars hugging the selection before stripping them.
pub fn matching_pair(left: char, right: char) -> bool {
    PAIRS.contains(&(left, right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_close_accepts_either_bracket_member() {
        assert_eq!(open_close('('), Some(('(', ')')));
        assert_eq!(open_close(')'), Some(('(', ')')));
        assert_eq!(open_close('{'), Some(('{', '}')));
        assert_eq!(open_close('}'), Some(('{', '}')));
        assert_eq!(open_close('['), Some(('[', ']')));
        assert_eq!(open_close('<'), Some(('<', '>')));
    }

    #[test]
    fn open_close_accepts_vim_aliases() {
        assert_eq!(open_close('b'), Some(('(', ')')));
        assert_eq!(open_close('B'), Some(('{', '}')));
        assert_eq!(open_close('r'), Some(('[', ']')));
        assert_eq!(open_close('a'), Some(('<', '>')));
    }

    #[test]
    fn open_close_quotes_are_symmetric() {
        assert_eq!(open_close('"'), Some(('"', '"')));
        assert_eq!(open_close('\''), Some(('\'', '\'')));
        assert_eq!(open_close('`'), Some(('`', '`')));
    }

    #[test]
    fn open_close_unknown_is_none() {
        assert_eq!(open_close('x'), None);
        assert_eq!(open_close(' '), None);
        assert_eq!(open_close('1'), None);
    }

    #[test]
    fn matching_pair_recognises_known_pairs() {
        assert!(matching_pair('(', ')'));
        assert!(matching_pair('{', '}'));
        assert!(matching_pair('[', ']'));
        assert!(matching_pair('<', '>'));
        assert!(matching_pair('"', '"'));
    }

    #[test]
    fn matching_pair_rejects_mismatches() {
        assert!(!matching_pair('(', ']'));
        assert!(!matching_pair(')', '(')); // reversed isn't a valid enclosing pair
        assert!(!matching_pair('x', 'y'));
        assert!(!matching_pair('"', '\'')); // mismatched quotes
    }
}
