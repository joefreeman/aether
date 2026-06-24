//! Pure case/word-shape transforms for `input/transform_case`. No buffer or position awareness —
//! just `&str` in, `String` out — so it's trivially unit-testable.
//!
//! The three character transforms (`Upper`/`Lower`/`Invert`) recase letters verbatim. The
//! convention transforms split the operand into words and re-render them: split on whitespace,
//! any non-alphanumeric punctuation (`_`, `-`, `.`, …), and case boundaries — a lower/digit→upper
//! transition (`fooBar` → `foo`,`bar`) and an acronym tail (`HTTPServer` → `HTTP`,`Server`).

use aether_protocol::input::CaseKind;

pub fn transform(kind: CaseKind, input: &str) -> String {
    match kind {
        CaseKind::Upper => input.to_uppercase(),
        CaseKind::Lower => input.to_lowercase(),
        CaseKind::Invert => input.chars().map(invert_char).collect(),
        // Convention transforms share the tokenizer; they differ only in the per-word casing and
        // the joining separator.
        CaseKind::Camel => join_words(input, WordCase::CamelLead, ""),
        CaseKind::Pascal => join_words(input, WordCase::Capitalized, ""),
        CaseKind::Snake => join_words(input, WordCase::Lower, "_"),
        CaseKind::Kebab => join_words(input, WordCase::Lower, "-"),
        CaseKind::Words => join_words(input, WordCase::Lower, " "),
        CaseKind::Title => join_words(input, WordCase::Capitalized, " "),
        CaseKind::Sentence => join_words(input, WordCase::SentenceLead, " "),
        CaseKind::Dot => join_words(input, WordCase::Lower, "."),
        CaseKind::Constant => join_words(input, WordCase::Upper, "_"),
    }
}

fn invert_char(c: char) -> char {
    if c.is_uppercase() {
        // `to_lowercase` can yield multiple chars (rare); for case-inversion we keep it 1:1 and
        // fall back to the original when it doesn't collapse to a single char.
        let mut it = c.to_lowercase();
        match (it.next(), it.next()) {
            (Some(l), None) => l,
            _ => c,
        }
    } else if c.is_lowercase() {
        let mut it = c.to_uppercase();
        match (it.next(), it.next()) {
            (Some(u), None) => u,
            _ => c,
        }
    } else {
        c
    }
}

/// How each word is cased when a convention transform re-renders it. `*Lead` variants treat the
/// first word specially (camelCase / Sentence case).
#[derive(Clone, Copy)]
enum WordCase {
    Lower,
    Upper,
    Capitalized,
    CamelLead,
    SentenceLead,
}

fn join_words(input: &str, case: WordCase, sep: &str) -> String {
    let words = split_words(input);
    let mut out = String::with_capacity(input.len());
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        match case {
            WordCase::Lower => out.push_str(&w.to_lowercase()),
            WordCase::Upper => out.push_str(&w.to_uppercase()),
            WordCase::Capitalized => out.push_str(&capitalize(w)),
            WordCase::CamelLead => {
                if i == 0 {
                    out.push_str(&w.to_lowercase());
                } else {
                    out.push_str(&capitalize(w));
                }
            }
            WordCase::SentenceLead => {
                if i == 0 {
                    out.push_str(&capitalize(w));
                } else {
                    out.push_str(&w.to_lowercase());
                }
            }
        }
    }
    out
}

/// First char upper, the rest lower (`fOO` → `Foo`). Unicode-aware on both ends.
fn capitalize(w: &str) -> String {
    let mut chars = w.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out: String = first.to_uppercase().collect();
    out.push_str(&chars.as_str().to_lowercase());
    out
}

/// Split into word segments, dropping separators. A new word starts on a non-alphanumeric
/// separator (consumed) or at a case boundary inside an alphanumeric run.
fn split_words(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for i in 0..chars.len() {
        let c = chars[i];
        if !c.is_alphanumeric() {
            // Separator: end the current word and drop the char.
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if !cur.is_empty() && boundary_before(&chars, i) {
            words.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Whether a new word begins *at* index `i`, given a non-empty current word. `i > 0` here.
/// Two boundaries, both standard camelCase splits:
/// - lower/digit → Upper: `fooBar` breaks before `B`.
/// - Upper → Upper followed by lower (acronym tail): `HTTPServer` breaks before `S`.
fn boundary_before(chars: &[char], i: usize) -> bool {
    let c = chars[i];
    let p = chars[i - 1];
    if c.is_uppercase() && (p.is_lowercase() || p.is_numeric()) {
        return true;
    }
    if c.is_uppercase() && p.is_uppercase() {
        if let Some(next) = chars.get(i + 1) {
            return next.is_lowercase();
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(kind: CaseKind, input: &str) -> String {
        transform(kind, input)
    }

    #[test]
    fn character_transforms() {
        assert_eq!(t(CaseKind::Upper, "aBc1"), "ABC1");
        assert_eq!(t(CaseKind::Lower, "aBc1"), "abc1");
        assert_eq!(t(CaseKind::Invert, "aBc1-D"), "AbC1-d");
        // Character transforms are non-destructive on arbitrary text (spaces/punctuation kept).
        assert_eq!(t(CaseKind::Upper, "let foo = 1;"), "LET FOO = 1;");
    }

    #[test]
    fn tokenizer_splits_every_convention() {
        // From spaced words.
        assert_eq!(t(CaseKind::Camel, "foo bar"), "fooBar");
        assert_eq!(t(CaseKind::Pascal, "foo bar"), "FooBar");
        assert_eq!(t(CaseKind::Snake, "foo bar"), "foo_bar");
        assert_eq!(t(CaseKind::Kebab, "foo bar"), "foo-bar");
        assert_eq!(t(CaseKind::Words, "foo bar"), "foo bar");
        assert_eq!(t(CaseKind::Title, "foo bar"), "Foo Bar");
        assert_eq!(t(CaseKind::Sentence, "foo bar"), "Foo bar");
        assert_eq!(t(CaseKind::Dot, "foo bar"), "foo.bar");
        assert_eq!(t(CaseKind::Constant, "foo bar"), "FOO_BAR");
    }

    #[test]
    fn converts_between_conventions() {
        assert_eq!(t(CaseKind::Camel, "FooBar"), "fooBar");
        assert_eq!(t(CaseKind::Camel, "foo_bar"), "fooBar");
        assert_eq!(t(CaseKind::Pascal, "fooBar"), "FooBar");
        assert_eq!(t(CaseKind::Pascal, "foo_bar"), "FooBar");
        assert_eq!(t(CaseKind::Snake, "FooBar"), "foo_bar");
        assert_eq!(t(CaseKind::Kebab, "FooBar"), "foo-bar");
        assert_eq!(t(CaseKind::Words, "fooBar"), "foo bar");
        assert_eq!(t(CaseKind::Words, "FooBar"), "foo bar");
        assert_eq!(t(CaseKind::Constant, "fooBar"), "FOO_BAR");
    }

    #[test]
    fn acronyms_split_at_the_tail() {
        // The all-caps run stays together; the boundary is before the word-initial capital.
        assert_eq!(t(CaseKind::Snake, "HTTPServer"), "http_server");
        assert_eq!(
            t(CaseKind::Words, "parseHTTPResponse"),
            "parse http response"
        );
        assert_eq!(t(CaseKind::Pascal, "getHTTP"), "GetHttp"); // trailing acronym, no tail
    }

    #[test]
    fn digits_stay_glued_until_a_capital() {
        assert_eq!(t(CaseKind::Snake, "foo2bar"), "foo2bar");
        assert_eq!(t(CaseKind::Snake, "foo2Bar"), "foo2_bar");
        assert_eq!(t(CaseKind::Camel, "version 2 beta"), "version2Beta");
    }

    #[test]
    fn mixed_separators_and_extra_whitespace() {
        assert_eq!(t(CaseKind::Camel, "foo-bar_baz"), "fooBarBaz");
        assert_eq!(t(CaseKind::Snake, "  foo   bar  "), "foo_bar");
        assert_eq!(t(CaseKind::Snake, "foo.bar/baz"), "foo_bar_baz");
    }

    #[test]
    fn empty_and_separator_only_collapse() {
        assert_eq!(t(CaseKind::Snake, ""), "");
        assert_eq!(t(CaseKind::Camel, "   "), "");
        assert_eq!(t(CaseKind::Snake, "___"), "");
        // No letters → character transforms leave it untouched.
        assert_eq!(t(CaseKind::Upper, "123"), "123");
    }

    #[test]
    fn single_word_round_trips_are_idempotent() {
        assert_eq!(
            t(CaseKind::Snake, t(CaseKind::Snake, "fooBar").as_str()),
            "foo_bar"
        );
        assert_eq!(t(CaseKind::Camel, "fooBar"), "fooBar");
        assert_eq!(t(CaseKind::Pascal, "FooBar"), "FooBar");
    }
}
