//! Aether's own command language: what the shell view runs.
//!
//! Not a POSIX shell and not an imitation of one. A line is a list of pipelines of simple
//! commands, with one kind of quote, a fixed set of escapes, and the operators every shell shares
//! (`|`, `;`, `&&`, `||`, `>`, `>>`, `<`). A lone path-shaped word changes directory; a lone
//! `NAME=value` sets a variable for the runs that follow. Everything else is a syntax error with a
//! position — and the user's real shell is a command like any other, `sh -c "…"`, for the rest.
//!
//! The other half of the design is that a line is **validated before it runs**: a command that is
//! not on `PATH`, a directory that does not exist, a variable that is not set, a glob that matches
//! nothing — each is refused at `Enter`, naming the word at fault, rather than becoming a failed
//! run. That is what makes the transcript a record of things that happened.
//!
//! This crate is sans-IO. It reads the world through the [`World`] trait — what a variable is,
//! what a path is, what a directory holds — and never spawns anything. The server implements the
//! trait over the shell's own state and runs what this crate accepts.

pub mod glob;
pub mod lex;
pub mod parse;
pub mod validate;
pub mod world;

pub use lex::{Part, Span, Word};
pub use parse::{
    parse, Assignment, Command, Item, ListOp, Pipeline, Program, Redirect, RedirectKind,
};
pub use validate::{
    find_executable, validate, Accepted, Builtin, Context, Exec, Plan, PlannedItem,
    PlannedRedirect, Stage, BUILTINS,
};
pub use world::{PathKind, World};

/// Why a line was not accepted, and the word at fault.
///
/// One type for syntax errors and validation refusals alike: both are answered the same way, by
/// selecting `span` in the input and saying `message`. The span is a byte range into the source
/// line and is never empty except for an empty line, where there is nothing to select.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub message: String,
    pub span: Span,
}

impl Refusal {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        Refusal {
            message: message.into(),
            span,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refusal {}
