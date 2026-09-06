//! The grammar: a line is pipelines joined by `;`, newlines, `&&` and `||`; a pipeline is simple
//! commands joined by `|`; a simple command is assignments, then words, with redirections among
//! them. Anything the grammar does not cover is a syntax error naming the place — the way to
//! the user's own shell for the rest is to run it: `sh -c "…"`.

use crate::lex::{tokenize, Part, Span, Token, TokenKind, Word};
use crate::Refusal;

/// A parsed line: pipelines in order, each joined to the one before it by its `op`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// How this pipeline follows the previous one; `None` for the first.
    pub op: Option<ListOp>,
    pub pipeline: Pipeline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOp {
    /// `;` or a newline: run regardless.
    Seq,
    /// `&&`: run only if the previous succeeded.
    And,
    /// `||`: run only if the previous failed.
    Or,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub commands: Vec<Command>,
    pub span: Span,
}

/// A simple command. `words` may be empty when the command is assignments alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub assignments: Vec<Assignment>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub name: String,
    /// What follows the `=`; a word with no parts for `NAME=`.
    pub value: Word,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub kind: RedirectKind,
    pub target: Word,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectKind {
    In,
    Out,
    Append,
}

impl RedirectKind {
    pub fn symbol(self) -> &'static str {
        match self {
            RedirectKind::In => "<",
            RedirectKind::Out => ">",
            RedirectKind::Append => ">>",
        }
    }
}

/// Parse one submitted line.
pub fn parse(src: &str) -> Result<Program, Refusal> {
    let tokens = tokenize(src)?;
    Parser { tokens, i: 0 }.program()
}

struct Parser {
    tokens: Vec<Token>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.i)
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.i).cloned();
        self.i += 1;
        t
    }

    fn skip_separators(&mut self) {
        while matches!(
            self.peek().map(|t| &t.kind),
            Some(TokenKind::Semi | TokenKind::Newline)
        ) {
            self.i += 1;
        }
    }

    fn program(&mut self) -> Result<Program, Refusal> {
        let mut items = Vec::new();
        let mut pending: Option<ListOp> = None;
        self.skip_separators();
        while let Some(next) = self.peek() {
            if let TokenKind::And | TokenKind::Or = next.kind {
                return Err(Refusal::new(
                    format!("missing command before `{}`", op_symbol(&next.kind)),
                    next.span,
                ));
            }
            let pipeline = self.pipeline()?;
            items.push(Item {
                op: pending.take(),
                pipeline,
            });
            let Some(next) = self.peek() else { break };
            match next.kind {
                TokenKind::Semi | TokenKind::Newline => {
                    self.skip_separators();
                    pending = Some(ListOp::Seq);
                }
                TokenKind::And | TokenKind::Or => {
                    let op = if next.kind == TokenKind::And {
                        ListOp::And
                    } else {
                        ListOp::Or
                    };
                    let span = next.span;
                    self.i += 1;
                    // `a &&` at the end of a line continues on the next, as it does everywhere.
                    while matches!(self.peek().map(|t| &t.kind), Some(TokenKind::Newline)) {
                        self.i += 1;
                    }
                    if self.peek().is_none() {
                        return Err(Refusal::new(
                            format!(
                                "missing command after `{}`",
                                if op == ListOp::And { "&&" } else { "||" }
                            ),
                            span,
                        ));
                    }
                    pending = Some(op);
                }
                // A pipeline ends only at one of the above; `pipeline` consumes everything else.
                _ => unreachable!("a pipeline stops only at a list operator"),
            }
        }
        Ok(Program { items })
    }

    fn pipeline(&mut self) -> Result<Pipeline, Refusal> {
        let first = self.command()?;
        let mut span = first.span;
        let mut commands = vec![first];
        while let Some(t) = self.peek() {
            if t.kind != TokenKind::Pipe {
                break;
            }
            let pipe = t.span;
            self.i += 1;
            match self.peek().map(|t| &t.kind) {
                Some(TokenKind::Word(_)) => {}
                _ => return Err(Refusal::new("missing command after `|`", pipe)),
            }
            let next = self.command()?;
            span = span.join(next.span);
            commands.push(next);
        }
        Ok(Pipeline { commands, span })
    }

    fn command(&mut self) -> Result<Command, Refusal> {
        let mut assignments = Vec::new();
        let mut words: Vec<Word> = Vec::new();
        let mut redirects = Vec::new();
        let mut span: Option<Span> = None;
        let extend = |span: &mut Option<Span>, s: Span| {
            *span = Some(span.map_or(s, |have| have.join(s)));
        };
        while let Some(t) = self.peek() {
            match &t.kind {
                TokenKind::Word(w) => {
                    let w = w.clone();
                    extend(&mut span, w.span);
                    self.i += 1;
                    match assignment(&w) {
                        // Assignments lead; after the command word, `x=y` is an argument.
                        Some(a) if words.is_empty() => assignments.push(a),
                        _ => words.push(w),
                    }
                }
                TokenKind::RedirectIn | TokenKind::RedirectOut | TokenKind::RedirectAppend => {
                    let kind = match t.kind {
                        TokenKind::RedirectIn => RedirectKind::In,
                        TokenKind::RedirectOut => RedirectKind::Out,
                        _ => RedirectKind::Append,
                    };
                    let op_span = t.span;
                    self.i += 1;
                    let target = match self.bump() {
                        Some(Token {
                            kind: TokenKind::Word(w),
                            ..
                        }) => w,
                        _ => {
                            return Err(Refusal::new(
                                format!("missing target after `{}`", kind.symbol()),
                                op_span,
                            ))
                        }
                    };
                    let rspan = op_span.join(target.span);
                    extend(&mut span, rspan);
                    redirects.push(Redirect {
                        kind,
                        target,
                        span: rspan,
                    });
                }
                _ => break,
            }
        }
        let Some(span) = span else {
            let at = self.peek().map_or(Span::new(0, 0), |t| t.span);
            return Err(Refusal::new("missing command", at));
        };
        if words.is_empty() && !redirects.is_empty() {
            return Err(Refusal::new("missing command", redirects[0].span));
        }
        Ok(Command {
            assignments,
            words,
            redirects,
            span,
        })
    }
}

fn op_symbol(kind: &TokenKind) -> &'static str {
    match kind {
        TokenKind::And => "&&",
        TokenKind::Or => "||",
        TokenKind::Pipe => "|",
        _ => "",
    }
}

/// `NAME=value` as a word: the first part is bare and starts with a name and `=`.
fn assignment(w: &Word) -> Option<Assignment> {
    let Some(Part::Bare(first)) = w.parts.first() else {
        return None;
    };
    let eq = first.find('=')?;
    let name = &first[..eq];
    let mut chars = name.chars();
    let valid = matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
    if !valid {
        return None;
    }
    let mut parts = Vec::new();
    let rest = &first[eq + 1..];
    if !rest.is_empty() {
        parts.push(Part::Bare(rest.to_string()));
    }
    parts.extend(w.parts[1..].iter().cloned());
    let value_start = w.span.start + eq + 1;
    Some(Assignment {
        name: name.to_string(),
        value: Word {
            parts,
            span: Span::new(value_start, w.span.end),
        },
        span: w.span,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(src: &str) -> Vec<Item> {
        parse(src).unwrap().items
    }

    fn texts(cmd: &Command) -> Vec<String> {
        cmd.words.iter().map(|w| w.literal().unwrap()).collect()
    }

    #[test]
    fn a_simple_command_is_its_words() {
        let items = list("ls -la src");
        assert_eq!(items.len(), 1);
        let cmd = &items[0].pipeline.commands[0];
        assert_eq!(texts(cmd), vec!["ls", "-la", "src"]);
        assert_eq!(cmd.span, Span::new(0, 10));
        assert!(cmd.assignments.is_empty() && cmd.redirects.is_empty());
    }

    #[test]
    fn pipelines_and_lists() {
        let items = list("a | b && c || d; e\nf");
        let ops: Vec<_> = items.iter().map(|i| i.op).collect();
        assert_eq!(
            ops,
            vec![
                None,
                Some(ListOp::And),
                Some(ListOp::Or),
                Some(ListOp::Seq),
                Some(ListOp::Seq)
            ]
        );
        assert_eq!(items[0].pipeline.commands.len(), 2);
        assert_eq!(texts(&items[0].pipeline.commands[1]), vec!["b"]);
        assert_eq!(items[0].pipeline.span, Span::new(0, 5));
    }

    #[test]
    fn a_trailing_list_operator_continues_on_the_next_line() {
        let items = list("a &&\nb");
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].op, Some(ListOp::And));
    }

    #[test]
    fn redirections_ride_the_command() {
        let items = list("sort < in.txt > out.txt >> log");
        let cmd = &items[0].pipeline.commands[0];
        assert_eq!(texts(cmd), vec!["sort"]);
        let kinds: Vec<_> = cmd
            .redirects
            .iter()
            .map(|r| (r.kind, r.target.literal().unwrap()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (RedirectKind::In, "in.txt".to_string()),
                (RedirectKind::Out, "out.txt".to_string()),
                (RedirectKind::Append, "log".to_string())
            ]
        );
    }

    #[test]
    fn assignments_lead_and_then_become_arguments() {
        let items = list("A=1 B=\"two words\" cmd C=3");
        let cmd = &items[0].pipeline.commands[0];
        let names: Vec<_> = cmd.assignments.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B"]);
        assert_eq!(cmd.assignments[1].value.literal().unwrap(), "two words");
        assert_eq!(cmd.assignments[0].value.span, Span::new(2, 3));
        assert_eq!(texts(cmd), vec!["cmd", "C=3"]);

        let alone = list("FOO=bar");
        let cmd = &alone[0].pipeline.commands[0];
        assert!(cmd.words.is_empty());
        assert_eq!(cmd.assignments[0].name, "FOO");
        let empty = list("FOO=");
        assert!(empty[0].pipeline.commands[0].assignments[0]
            .value
            .parts
            .is_empty());
    }

    #[test]
    fn a_misspelt_assignment_is_three_words() {
        let items = list("FOO = bar");
        let cmd = &items[0].pipeline.commands[0];
        assert!(cmd.assignments.is_empty());
        assert_eq!(texts(cmd), vec!["FOO", "=", "bar"]);
    }

    #[test]
    fn blank_and_comment_only_lines_are_empty_lists() {
        assert!(parse("").unwrap().items.is_empty());
        assert!(parse("  \n # just a note\n").unwrap().items.is_empty());
        // `!` is an ordinary character: no history expansion, no escape.
        let items = list("!x");
        assert_eq!(texts(&items[0].pipeline.commands[0]), vec!["!x"]);
    }

    #[test]
    fn the_errors_name_their_cause_and_place() {
        let err = |src: &str| parse(src).unwrap_err();
        assert_eq!(err("a |").message, "missing command after `|`");
        assert_eq!(err("a |").span, Span::new(2, 3));
        assert_eq!(err("a | | b").message, "missing command after `|`");
        assert_eq!(err("a &&").message, "missing command after `&&`");
        assert_eq!(err("&& a").message, "missing command before `&&`");
        assert_eq!(err("a; || b").message, "missing command before `||`");
        assert_eq!(err("cat >").message, "missing target after `>`");
        assert_eq!(err("cat >").span, Span::new(4, 5));
        assert_eq!(err("> out").message, "missing command");
        assert_eq!(err("FOO=1 > out").message, "missing command");
    }
}
