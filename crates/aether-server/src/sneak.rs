//! Pure sneak (`s`/`S`) word-jump logic: finding the matching word-starts in a viewport's visible
//! range, and numbering each. Kept free of `Buffer`/`ServerState` so it can be unit-tested against a
//! bare `Rope`; the handler in `handlers.rs` converts char indices to `LogicalPosition` and stores
//! the result.
//!
//! Labels are letters `a`–`z`, assigned top-to-bottom each update (so `a` is the topmost match) —
//! minus any letter that's a *next-refinement char* (the character a candidate word would extend
//! with), so a keystroke is unambiguous: a key matching a shown label jumps, anything else narrows.
//! Labels are positional, not sticky: as the match set shrinks the remaining words relabel `a`,
//! `b`, `c`… in order. Going alphabetical (rather than home-row) is the concession that they move;
//! the exclusion is what keeps each press unambiguous. When there are more matches than available
//! letters, as many as fit are labelled (top-to-bottom) and the overflow stays highlighted but
//! unlabelled — so labels always appear after the first char; reach the rest by narrowing.

use std::collections::HashSet;

use ropey::Rope;

/// Label characters: the letters `a`–`z`, handed out in document order.
pub const LABEL_ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz";

/// A matched word-start, in absolute char indices. `next_char` is the character that would extend
/// this word past the typed query (i.e. a key that *narrows* rather than jumps) — excluded from the
/// label alphabet so labels never alias a refinement key. `None` when the query already spans the
/// whole word.
#[derive(Debug, Clone)]
pub struct RawCandidate {
    pub start_char: usize,
    pub end_char_excl: usize,
    pub next_char: Option<char>,
}

/// A "word" char for jump purposes. Normal (`s`): alphanumerics and `_` (matches
/// `WordBoundary::Word`). Big (`Alt-s`): any non-whitespace (matches `WordBoundary::BigWord`), so a
/// whole `foo.bar()` run is one target.
fn is_word_char(c: char, big: bool) -> bool {
    if big {
        !c.is_whitespace()
    } else {
        c.is_alphanumeric() || c == '_'
    }
}

/// Smartcase: a query with any uppercase char matches case-sensitively, otherwise
/// case-insensitively.
fn prefix_matches(word: &str, query: &str) -> bool {
    if query.chars().any(|c| c.is_uppercase()) {
        word.starts_with(query)
    } else {
        word.to_lowercase().starts_with(&query.to_lowercase())
    }
}

/// All word-starts on logical lines `[first_line, last_line_excl)` whose word begins with `query`
/// (smartcase). An empty query yields no candidates — the caller shows nothing until the first
/// char is typed.
pub fn compute_candidates(
    text: &Rope,
    first_line: usize,
    last_line_excl: usize,
    query: &str,
    big: bool,
) -> Vec<RawCandidate> {
    let mut out = Vec::new();
    if query.is_empty() {
        return out;
    }
    let total = text.len_chars();
    let line_count = text.len_lines();
    let start_char = text.line_to_char(first_line.min(line_count));
    let end_char = if last_line_excl >= line_count {
        total
    } else {
        text.line_to_char(last_line_excl)
    };

    let mut i = start_char;
    while i < end_char {
        let c = text.char(i);
        if is_word_char(c, big) && (i == 0 || !is_word_char(text.char(i - 1), big)) {
            // Extend to the end of the word run (words never cross a newline, so `total` is a safe
            // upper bound even past `end_char`).
            let mut e = i;
            while e + 1 < total && is_word_char(text.char(e + 1), big) {
                e += 1;
            }
            let word: String = text.slice(i..=e).chars().collect();
            if prefix_matches(&word, query) {
                // The char just past the typed query — a key that would narrow this candidate.
                let next_char = word.chars().nth(query.chars().count());
                out.push(RawCandidate {
                    start_char: i,
                    end_char_excl: e + 1,
                    next_char,
                });
            }
            i = e + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Label the candidates top-to-bottom with letters, skipping any that's a next-refinement char (so a
/// keypress is unambiguously jump-or-narrow). Labels as many as the alphabet allows; candidates
/// beyond that get `None` (highlighted but unlabelled, reached by narrowing).
pub fn assign_labels(candidates: &[RawCandidate]) -> Vec<Option<char>> {
    let n = candidates.len();
    // Letters a candidate would extend with — excluded so a label never aliases a refinement key.
    let next_chars: HashSet<char> = candidates
        .iter()
        .filter_map(|c| c.next_char)
        .flat_map(|c| c.to_lowercase())
        .collect();
    let available: Vec<char> = LABEL_ALPHABET
        .chars()
        .filter(|c| !next_chars.contains(c))
        .collect();
    // Label as many as fit, top-to-bottom; any overflow stays highlighted but unlabelled (reachable
    // by typing another char). `available` excludes every candidate's next char, so a label never
    // aliases a refinement key even when only some candidates are labelled.
    (0..n).map(|i| available.get(i).copied()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    /// The matched words for `cands`, reconstructed from the rope (the candidate stores only char
    /// indices now).
    fn words(t: &Rope, cands: &[RawCandidate]) -> Vec<String> {
        cands
            .iter()
            .map(|c| t.slice(c.start_char..c.end_char_excl).chars().collect())
            .collect()
    }

    #[test]
    fn alphabet_is_distinct_letters() {
        let set: HashSet<char> = LABEL_ALPHABET.chars().collect();
        assert_eq!(set.len(), LABEL_ALPHABET.chars().count(), "no duplicates");
        assert!(LABEL_ALPHABET.chars().all(|c| c.is_ascii_lowercase()));
    }

    #[test]
    fn empty_query_no_candidates() {
        let t = rope("foo bar baz\n");
        assert!(compute_candidates(&t, 0, 1, "", false).is_empty());
    }

    #[test]
    fn prefix_match_word_starts_only() {
        let t = rope("foo food barfoo foam\n");
        let cands = compute_candidates(&t, 0, 1, "fo", false);
        // "foo", "food", "foam" start with fo; "barfoo" contains foo but isn't a word-start match.
        assert_eq!(words(&t, &cands), vec!["foo", "food", "foam"]);
    }

    #[test]
    fn smartcase() {
        let t = rope("Foo foo FOO\n");
        // Lowercase query is case-insensitive: matches all three.
        assert_eq!(compute_candidates(&t, 0, 1, "f", false).len(), 3);
        // Uppercase anywhere in the query forces case-sensitive: only "Foo" and "FOO".
        let cands = compute_candidates(&t, 0, 1, "F", false);
        assert_eq!(words(&t, &cands), vec!["Foo", "FOO"]);
    }

    #[test]
    fn big_word_spans_punctuation() {
        let t = rope("foo.bar() baz\n");
        // Normal: "foo" is its own word-start; the next word-start is "bar".
        let normal = compute_candidates(&t, 0, 1, "f", false);
        assert_eq!(words(&t, &normal), vec!["foo"]);
        // Big: the whole non-whitespace run is one target.
        let big = compute_candidates(&t, 0, 1, "f", true);
        assert_eq!(words(&t, &big), vec!["foo.bar()"]);
    }

    #[test]
    fn range_scopes_to_visible_lines() {
        let t = rope("alpha\nbeta\ngamma\n");
        // Only line 1 ("beta") is visible.
        let cands = compute_candidates(&t, 1, 2, "b", false);
        assert_eq!(words(&t, &cands), vec!["beta"]);
    }

    #[test]
    fn labels_letter_candidates_top_to_bottom() {
        let t = rope("fee fie foe fum\n");
        let cands = compute_candidates(&t, 0, 1, "f", false);
        let labels = assign_labels(&cands);
        assert_eq!(
            labels,
            vec![Some('a'), Some('b'), Some('c'), Some('d')],
            "letters in document order"
        );
    }

    #[test]
    fn labels_skip_ambiguous_next_chars() {
        // "xa xb xc" after query "x": the next chars are a, b, c, so labels must skip them and
        // start at d — otherwise pressing `a` would be both "jump to xa" and "narrow to xa".
        let t = rope("xa xb xc\n");
        let cands = compute_candidates(&t, 0, 1, "x", false);
        assert_eq!(assign_labels(&cands), vec![Some('d'), Some('e'), Some('f')]);
    }

    #[test]
    fn more_matches_than_letters_labels_what_fits() {
        // More than 26 matches: the first batch gets labels; the overflow is unlabelled (but still
        // highlighted), reachable by narrowing. So labels always appear after one char.
        let many: String = (0..30).map(|i| format!("a{i} ")).collect();
        let t = rope(&format!("{many}\n"));
        let cands = compute_candidates(&t, 0, 1, "a", false);
        assert!(cands.len() > 26);
        let labels = assign_labels(&cands);
        let labelled = labels.iter().filter(|l| l.is_some()).count();
        assert!(labelled > 0, "some labels shown, not deferred to none");
        assert!(labelled <= 26, "no more than the alphabet");
        assert!(labels[labels.len() - 1].is_none(), "overflow unlabelled");
    }
}
