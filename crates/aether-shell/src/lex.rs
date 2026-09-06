//! Tokens: words, and the handful of operators between them.
//!
//! A word is a run of bare text, quoted text and variable references with no whitespace between
//! them — `dir/"my file".txt` is one word of three parts. The parts are kept apart rather than
//! joined here because they mean different things later: bare text is where globs and a leading
//! `~` are live; quoted text and a variable's value are taken exactly as they are.

use crate::Refusal;

/// A byte range into the source line, end exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// The smallest span covering both.
    pub fn join(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// One piece of a word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// Typed bare. Glob characters are live, and a leading `~` may expand.
    Bare(String),
    /// From inside quotes, escapes already resolved. Taken literally.
    Quoted(String),
    /// `$NAME` or `${NAME}`, bare or inside quotes. The value is taken literally either way.
    Var { name: String, span: Span },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub parts: Vec<Part>,
    pub span: Span,
}

impl Word {
    /// What was typed for this word.
    pub fn source<'a>(&self, src: &'a str) -> &'a str {
        &src[self.span.start..self.span.end]
    }

    /// The word's text when it has no variable references: bare and quoted parts joined.
    pub fn literal(&self) -> Option<String> {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                Part::Bare(s) | Part::Quoted(s) => out.push_str(s),
                Part::Var { .. } => return None,
            }
        }
        Some(out)
    }

    /// Whether the word is exactly one bare part reading `s`.
    pub fn is_bare(&self, s: &str) -> bool {
        matches!(self.parts.as_slice(), [Part::Bare(b)] if b == s)
    }

    /// The bare text the word starts with, if it starts bare.
    pub fn leading_bare(&self) -> Option<&str> {
        match self.parts.first() {
            Some(Part::Bare(s)) => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Word(Word),
    Pipe,
    And,
    Or,
    Semi,
    Newline,
    RedirectOut,
    RedirectAppend,
    RedirectIn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// Split `src` into tokens, or say where it stopped making sense.
pub fn tokenize(src: &str) -> Result<Vec<Token>, Refusal> {
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (pos, c) = chars[i];
        let next = chars.get(i + 1).map(|(_, c)| *c);
        let single = |kind: TokenKind| Token {
            kind,
            span: Span::new(pos, pos + c.len_utf8()),
        };
        let double = |kind: TokenKind| Token {
            kind,
            span: Span::new(pos, pos + 2),
        };
        match c {
            ' ' | '\t' | '\r' => i += 1,
            '\n' => {
                out.push(single(TokenKind::Newline));
                i += 1;
            }
            // Only at a token boundary: `foo#bar` is one word.
            '#' => {
                while i < chars.len() && chars[i].1 != '\n' {
                    i += 1;
                }
            }
            '|' if next == Some('|') => {
                out.push(double(TokenKind::Or));
                i += 2;
            }
            '|' => {
                out.push(single(TokenKind::Pipe));
                i += 1;
            }
            '&' if next == Some('&') => {
                out.push(double(TokenKind::And));
                i += 2;
            }
            '&' => {
                return Err(Refusal::new(
                    "background jobs aren't supported",
                    Span::new(pos, pos + 1),
                ));
            }
            ';' => {
                out.push(single(TokenKind::Semi));
                i += 1;
            }
            '>' if next == Some('>') => {
                out.push(double(TokenKind::RedirectAppend));
                i += 2;
            }
            '>' if next == Some('&') => {
                return Err(Refusal::new(
                    "stderr redirection isn't supported — stderr already lands in the transcript",
                    Span::new(pos, pos + 2),
                ));
            }
            '>' => {
                out.push(single(TokenKind::RedirectOut));
                i += 1;
            }
            '<' => {
                out.push(single(TokenKind::RedirectIn));
                i += 1;
            }
            _ => {
                let (word, after) = lex_word(src, &chars, i)?;
                let span = word.span;
                out.push(Token {
                    kind: TokenKind::Word(word),
                    span,
                });
                i = after;
            }
        }
    }
    Ok(out)
}

fn is_word_break(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n' | '|' | '&' | ';' | '<' | '>')
}

fn is_name_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_name_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// Byte offset of the character at `i`, or the end of the source past the last one.
fn at(src: &str, chars: &[(usize, char)], i: usize) -> usize {
    chars.get(i).map_or(src.len(), |(pos, _)| *pos)
}

fn lex_word(src: &str, chars: &[(usize, char)], start: usize) -> Result<(Word, usize), Refusal> {
    let mut parts = Vec::new();
    let mut bare = String::new();
    let mut i = start;
    while i < chars.len() {
        let (_, c) = chars[i];
        if is_word_break(c) {
            break;
        }
        match c {
            '"' => {
                flush_bare(&mut parts, &mut bare);
                i = lex_quoted(src, chars, i, &mut parts)?;
            }
            '$' => {
                flush_bare(&mut parts, &mut bare);
                let (part, after) = lex_var(src, chars, i)?;
                parts.push(part);
                i = after;
            }
            _ => {
                bare.push(c);
                i += 1;
            }
        }
    }
    flush_bare(&mut parts, &mut bare);
    let span = Span::new(at(src, chars, start), at(src, chars, i));
    Ok((Word { parts, span }, i))
}

fn flush_bare(parts: &mut Vec<Part>, bare: &mut String) {
    if !bare.is_empty() {
        parts.push(Part::Bare(std::mem::take(bare)));
    }
}

/// `$NAME` or `${NAME}` starting at `chars[i]`, which is the `$`.
fn lex_var(src: &str, chars: &[(usize, char)], i: usize) -> Result<(Part, usize), Refusal> {
    let dollar = at(src, chars, i);
    let no_name = || {
        Refusal::new(
            "a variable name must follow `$`",
            Span::new(dollar, dollar + 1),
        )
    };
    let mut j = i + 1;
    let braced = chars.get(j).is_some_and(|(_, c)| *c == '{');
    if braced {
        j += 1;
    }
    let name_start = j;
    if !chars.get(j).is_some_and(|(_, c)| is_name_start(*c)) {
        return Err(no_name());
    }
    while chars.get(j).is_some_and(|(_, c)| is_name_char(*c)) {
        j += 1;
    }
    let name: String = chars[name_start..j].iter().map(|(_, c)| *c).collect();
    if braced {
        if chars.get(j).is_none_or(|(_, c)| *c != '}') {
            return Err(Refusal::new(
                format!("missing `}}` after `${{{name}`"),
                Span::new(dollar, at(src, chars, j)),
            ));
        }
        j += 1;
    }
    let span = Span::new(dollar, at(src, chars, j));
    Ok((Part::Var { name, span }, j))
}

/// A quoted section starting at `chars[i]`, which is the opening quote. Pushes the parts it
/// yields — quoted text and variable references — and answers the index after the closing quote.
fn lex_quoted(
    src: &str,
    chars: &[(usize, char)],
    i: usize,
    parts: &mut Vec<Part>,
) -> Result<usize, Refusal> {
    let open = at(src, chars, i);
    let unterminated = || Refusal::new("unterminated quote", Span::new(open, src.len()));
    let mut text = String::new();
    let mut yielded = false;
    let mut j = i + 1;
    loop {
        let Some(&(pos, c)) = chars.get(j) else {
            return Err(unterminated());
        };
        match c {
            '"' => {
                if !text.is_empty() || !yielded {
                    parts.push(Part::Quoted(std::mem::take(&mut text)));
                }
                return Ok(j + 1);
            }
            '\\' => {
                let Some(&(_, e)) = chars.get(j + 1) else {
                    return Err(unterminated());
                };
                match e {
                    '"' | '\\' | '$' => {
                        text.push(e);
                        j += 2;
                    }
                    'n' => {
                        text.push('\n');
                        j += 2;
                    }
                    't' => {
                        text.push('\t');
                        j += 2;
                    }
                    'u' => {
                        let (ch, after) = lex_unicode_escape(src, chars, j)?;
                        text.push(ch);
                        j = after;
                    }
                    other => {
                        return Err(Refusal::new(
                            format!("unknown escape `\\{other}`"),
                            Span::new(pos, pos + 1 + other.len_utf8()),
                        ));
                    }
                }
            }
            '$' => {
                if !text.is_empty() {
                    parts.push(Part::Quoted(std::mem::take(&mut text)));
                }
                let (part, after) = lex_var(src, chars, j)?;
                parts.push(part);
                yielded = true;
                j = after;
            }
            _ => {
                text.push(c);
                j += 1;
            }
        }
    }
}

/// `\u{XXXX}` with `chars[j]` at the backslash.
fn lex_unicode_escape(
    src: &str,
    chars: &[(usize, char)],
    j: usize,
) -> Result<(char, usize), Refusal> {
    let start = at(src, chars, j);
    let bad = |end: usize| Refusal::new("malformed `\\u{…}` escape", Span::new(start, end));
    if chars.get(j + 2).is_none_or(|(_, c)| *c != '{') {
        return Err(bad(at(src, chars, j + 2)));
    }
    let mut k = j + 3;
    let mut hex = String::new();
    while let Some(&(_, c)) = chars.get(k) {
        if c == '}' {
            break;
        }
        hex.push(c);
        k += 1;
    }
    if chars.get(k).is_none_or(|(_, c)| *c != '}') {
        return Err(bad(at(src, chars, k)));
    }
    let ch = u32::from_str_radix(&hex, 16)
        .ok()
        .and_then(char::from_u32)
        .filter(|_| !hex.is_empty() && hex.len() <= 6)
        .ok_or_else(|| bad(at(src, chars, k + 1)))?;
    Ok((ch, k + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(src: &str) -> Vec<Vec<Part>> {
        tokenize(src)
            .unwrap()
            .into_iter()
            .filter_map(|t| match t.kind {
                TokenKind::Word(w) => Some(w.parts),
                _ => None,
            })
            .collect()
    }

    fn bare(s: &str) -> Part {
        Part::Bare(s.into())
    }

    fn quoted(s: &str) -> Part {
        Part::Quoted(s.into())
    }

    #[test]
    fn words_split_on_whitespace_and_keep_their_spans() {
        let toks = tokenize("ls  -la\tsrc").unwrap();
        let spans: Vec<_> = toks.iter().map(|t| (t.span.start, t.span.end)).collect();
        assert_eq!(spans, vec![(0, 2), (4, 7), (8, 11)]);
        assert_eq!(
            words("ls -la src"),
            vec![vec![bare("ls")], vec![bare("-la")], vec![bare("src")]]
        );
    }

    #[test]
    fn adjacent_parts_join_into_one_word() {
        assert_eq!(
            words(r#"dir/"my file".txt"#),
            vec![vec![bare("dir/"), quoted("my file"), bare(".txt")]]
        );
    }

    #[test]
    fn quotes_interpolate_and_escape() {
        assert_eq!(
            words(r#""a $x b""#),
            vec![vec![
                quoted("a "),
                Part::Var {
                    name: "x".into(),
                    span: Span::new(3, 5)
                },
                quoted(" b")
            ]]
        );
        assert_eq!(
            words(r#""say \"hi\"\n\t\\ \$5 \u{41}""#),
            vec![vec![quoted("say \"hi\"\n\t\\ $5 A")]]
        );
        assert_eq!(
            words(r#""""#),
            vec![vec![quoted("")]],
            "an empty string is a word"
        );
    }

    #[test]
    fn variables_bare_and_braced() {
        assert_eq!(
            words("$HOME/${x}y"),
            vec![vec![
                Part::Var {
                    name: "HOME".into(),
                    span: Span::new(0, 5)
                },
                bare("/"),
                Part::Var {
                    name: "x".into(),
                    span: Span::new(6, 10)
                },
                bare("y")
            ]]
        );
    }

    #[test]
    fn operators_and_comments() {
        let kinds: Vec<_> = tokenize("a | b && c || d ; e > f >> g < h # not this\nk")
            .unwrap()
            .into_iter()
            .map(|t| match t.kind {
                TokenKind::Word(w) => w.parts[0].clone(),
                other => Part::Bare(format!("{other:?}")),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                bare("a"),
                bare("Pipe"),
                bare("b"),
                bare("And"),
                bare("c"),
                bare("Or"),
                bare("d"),
                bare("Semi"),
                bare("e"),
                bare("RedirectOut"),
                bare("f"),
                bare("RedirectAppend"),
                bare("g"),
                bare("RedirectIn"),
                bare("h"),
                bare("Newline"),
                bare("k"),
            ]
        );
        assert_eq!(
            words("foo#bar"),
            vec![vec![bare("foo#bar")]],
            "`#` mid-word is literal"
        );
    }

    #[test]
    fn the_errors_name_their_cause_and_place() {
        let err = |src: &str| tokenize(src).unwrap_err();
        assert_eq!(err("echo \"abc").message, "unterminated quote");
        assert_eq!(err("echo \"abc").span, Span::new(5, 9));
        assert_eq!(err(r#"echo "\d""#).message, "unknown escape `\\d`");
        assert_eq!(err(r#"echo "\d""#).span, Span::new(6, 8));
        assert_eq!(err("echo $").message, "a variable name must follow `$`");
        assert_eq!(err("echo ${x").message, "missing `}` after `${x`");
        assert_eq!(err("sleep 5 &").message, "background jobs aren't supported");
        assert_eq!(err("sleep 5 &").span, Span::new(8, 9));
        assert!(err("cmd 2>&1")
            .message
            .starts_with("stderr redirection isn't supported"));
        assert_eq!(err(r#""\u{zz}""#).message, "malformed `\\u{…}` escape");
    }
}
