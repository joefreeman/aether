//! Syntax service messages — tree-sitter work the server does on text that isn't a buffer.

use crate::envelope::RpcMethod;
use crate::viewport::Highlight;
use serde::{Deserialize, Serialize};

// ---- syntax/highlight_snippet -------------------------------------------------------------------

pub struct SyntaxHighlightSnippet;
impl RpcMethod for SyntaxHighlightSnippet {
    const NAME: &'static str = "syntax/highlight_snippet";
    type Params = SyntaxHighlightSnippetParams;
    type Result = SyntaxHighlightSnippetResult;
}

/// Highlight a standalone snippet with the server's tree-sitter registry — the markdown reading
/// view's fenced code blocks (docs/markdown-view.md §2.8). `language` resolves through the same
/// alias table fences use (`rs`, `py`, …); an unknown language yields an empty result rather
/// than an error, so callers can fire-and-adopt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntaxHighlightSnippetParams {
    pub language: String,
    pub text: String,
}

/// Byte-offset highlight runs into the snippet, with the usual tree-sitter capture names —
/// clients style them through the same theme tables the editor uses.
#[derive(Debug, Serialize, Deserialize)]
pub struct SyntaxHighlightSnippetResult {
    pub highlights: Vec<Highlight>,
}
