//! Workspace-wide symbol search (`docs/workspace-symbols.md`): the LSP `workspace/symbol` fan-out
//! behind [`aether_protocol::picker::PickerKind::WorkspaceSymbols`].
//!
//! Shaped like [`crate::grep`] rather than the snapshot pickers: the query *is* the search, so each
//! `picker/query` re-asks every eligible server and the results are merged into the picker's
//! candidate list as each one answers, guarded by the picker's generation. Only servers pinned by a
//! declared *project* are asked — see [`crate::lsp::manager::LspManager::symbol_servers`] for why.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aether_protocol::picker::{PickerKind, SymbolKind};
use serde_json::Value;

use crate::lsp::manager::SymbolServer;
use crate::lsp::position::{lsp_to_byte, PositionEncoding};
use crate::picker::WorkspaceSymbolCandidate;
use crate::state::SharedState;
use aether_protocol::ClientId;

/// Most symbols to take from any one server before merging.
///
/// Capped per server rather than after the merge: an unbounded fan-out lets one chatty server crowd
/// out the rest, making the result set depend on which server is verbose rather than which symbols
/// are relevant.
pub const PER_SERVER_LIMIT: usize = 200;

/// Ask one server for `query` and convert its reply into candidates.
///
/// Returns an empty vec on any failure — a server that errors, times out, or answers with something
/// unparseable simply contributes nothing, because one broken project shouldn't empty the picker for
/// the others.
pub async fn query_server(
    server: &SymbolServer,
    query: &str,
    roots: &[PathBuf],
) -> Vec<WorkspaceSymbolCandidate> {
    // The query goes over verbatim. Some servers give it extra meaning — rust-analyzer widens into
    // dependencies with a trailing `#` and the standard library with `*` — so trimming or
    // normalising it here would silently break conventions users already have.
    let params = serde_json::json!({ "query": query });
    let reply = match server.client.request("workspace/symbol", params).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(language = %server.language, error = %e, "workspace/symbol failed");
            return Vec::new();
        }
    };
    parse_symbols(&reply, server.encoding, roots)
}

/// Parse a `workspace/symbol` reply into candidates, resolving each symbol's line/column.
///
/// Handles both response shapes: the classic `SymbolInformation[]` and LSP 3.17's `WorkspaceSymbol`
/// (whose `location` may be a bare `{ uri }` needing a `workspaceSymbol/resolve` round-trip — those
/// are dropped, see the doc's Deferred list).
pub fn parse_symbols(
    reply: &Value,
    encoding: PositionEncoding,
    roots: &[PathBuf],
) -> Vec<WorkspaceSymbolCandidate> {
    let Some(items) = reply.as_array() else {
        return Vec::new();
    };
    // Symbols cluster in a handful of files, so read each one once and reuse its lines. Converting
    // per symbol would re-read the same file for every match in it.
    let mut lines_cache: HashMap<String, Option<Vec<String>>> = HashMap::new();
    let mut out = Vec::new();
    for item in items.iter().take(PER_SERVER_LIMIT) {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(location) = item.get("location") else {
            continue;
        };
        let Some(uri) = location.get("uri").and_then(Value::as_str) else {
            continue;
        };
        // A 3.17 `WorkspaceSymbol` may carry only a `uri`; without a range there's nowhere to jump.
        let Some(range) = location.get("range") else {
            continue;
        };
        let Some(path) = crate::lsp::uri::uri_to_path(uri) else {
            continue;
        };
        let line = range
            .get("start")
            .and_then(|s| s.get("line"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let character = range
            .get("start")
            .and_then(|s| s.get("character"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;

        let abs_path = path.display().to_string();
        // The file isn't open, so there's no rope to convert against — read it (once) and convert
        // the LSP column against the actual line text. A missing/unreadable file still yields a
        // usable row at column 0 rather than being dropped.
        let col = {
            let lines = lines_cache
                .entry(abs_path.clone())
                .or_insert_with(|| read_lines(&path));
            match lines.as_ref().and_then(|l| l.get(line as usize)) {
                Some(text) => lsp_to_byte(text, character, encoding) as u32,
                None => 0,
            }
        };

        out.push(WorkspaceSymbolCandidate {
            display_path: display_path(&path, roots),
            abs_path: abs_path.clone(),
            line,
            col,
            name: clean_symbol_name(name, &abs_path),
            symbol_kind: symbol_kind(item.get("kind").and_then(Value::as_u64).unwrap_or(0) as u8),
            container: item
                .get("containerName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    out
}

fn read_lines(path: &Path) -> Option<Vec<String>> {
    std::fs::read_to_string(path)
        .ok()
        .map(|c| c.lines().map(str::to_string).collect())
}

/// Tidy an LSP symbol name for a one-row picker label (and haystack). Servers send names
/// verbatim from source: marksman reports a setext heading as `Heading\n=======` (the
/// underline included — verified against marksman 2026-02-08), and any embedded newline
/// breaks the row layout, so every name is cut to its first line. For markdown files the
/// name is also raw *markup* (`` `code` ``, `**strong**`, `[text](url)`), which the picker
/// shouldn't show — strip it down to the rendered text.
pub fn clean_symbol_name(name: &str, abs_path: &str) -> String {
    let first = name.lines().next().unwrap_or("").trim();
    let markdown = std::path::Path::new(abs_path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"));
    if !markdown {
        return first.to_string();
    }
    let stripped = strip_markdown_inline(first);
    if stripped.is_empty() {
        first.to_string()
    } else {
        stripped
    }
}

/// Render a single line of markdown down to its plain text. The line is parsed as a heading
/// (`# {line}`) so its content stays in *inline* context — parsed as a bare document,
/// a heading named "1. Introduction" would become an ordered list and lose its number.
fn strip_markdown_inline(line: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser};
    let doc = format!("# {line}");
    let mut out = String::new();
    for ev in Parser::new_ext(&doc, Options::ENABLE_STRIKETHROUGH) {
        match ev {
            Event::Text(t) | Event::Code(t) => out.push_str(&t),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Workspace-relative when the file lives inside a root, else the absolute path — symbols can come
/// from dependencies and the standard library, where no root applies.
fn display_path(path: &Path, roots: &[PathBuf]) -> String {
    roots
        .iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.components().count())
        .and_then(|r| path.strip_prefix(r).ok())
        .map(|rel| rel.display().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// The LSP `SymbolKind` enumeration (1-based), mirroring `parse_document_symbols`'s mapping.
fn symbol_kind(raw: u8) -> SymbolKind {
    use SymbolKind as K;
    match raw {
        1 => K::File,
        2 => K::Module,
        3 => K::Namespace,
        4 => K::Package,
        5 => K::Class,
        6 => K::Method,
        7 => K::Property,
        8 => K::Field,
        9 => K::Constructor,
        10 => K::Enum,
        11 => K::Interface,
        12 => K::Function,
        13 => K::Variable,
        14 => K::Constant,
        15 => K::String,
        16 => K::Number,
        17 => K::Boolean,
        18 => K::Array,
        19 => K::Object,
        20 => K::Key,
        21 => K::Null,
        22 => K::EnumMember,
        23 => K::Struct,
        24 => K::Event,
        25 => K::Operator,
        26 => K::TypeParameter,
        _ => K::Unknown,
    }
}

/// Merge one server's answer into an open workspace-symbols picker and push the update.
///
/// Late arrivals are dropped by generation, so a superseded query can't resurrect its results. New
/// candidates are deduped against everything already accumulated — not just within this batch —
/// because overlapping declared roots (a Cargo workspace and one of its member crates) mean the
/// duplicate usually arrives from a *different* server.
pub async fn merge_results(
    state: &SharedState,
    client_id: ClientId,
    generation: u64,
    found: Vec<WorkspaceSymbolCandidate>,
) {
    let mut s = state.lock().await;
    let key = (client_id, PickerKind::WorkspaceSymbols);
    let outbound = s.clients.get(&client_id).map(|c| c.outbound.clone());
    let crate::state::ServerState {
        pickers, matcher, ..
    } = &mut *s;
    let Some(picker) = pickers.get_mut(&key) else {
        return; // picker closed
    };
    if picker.generation != generation {
        return; // a newer query superseded this one
    }
    let crate::picker::PickerCandidates::WorkspaceSymbols(existing) = &mut picker.candidates else {
        return;
    };
    let mut seen: std::collections::HashSet<(String, u32, u32, String)> = existing
        .iter()
        .map(|c| (c.abs_path.clone(), c.line, c.col, c.name.clone()))
        .collect();
    let before = existing.len();
    for c in found {
        let k = (c.abs_path.clone(), c.line, c.col, c.name.clone());
        if seen.insert(k) {
            existing.push(c);
        }
    }
    if existing.len() == before {
        return; // nothing new — don't churn the client's window
    }
    // Group by file so the header spans stay contiguous, then rerank against the live query.
    existing.sort_by(|a, b| {
        a.display_path
            .cmp(&b.display_path)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.name.cmp(&b.name))
    });
    picker.rerank(matcher);
    let update = crate::picker::build_update(picker, matcher);
    drop(s);
    if let (Some(sender), Some(params)) = (outbound, update) {
        let _ = sender
            .send(crate::handlers::picker_update_notif(params))
            .await;
    }
}

/// Re-run any open workspace-symbols query against a server that has just become ready.
///
/// Pinned servers start at *activation*, so a query typed while one is still indexing simply gets
/// nothing from it — and nothing retries, because the fan-out only fires on a keystroke. Without
/// this the picker is quietly incomplete for the first minute after opening a workspace, which is
/// exactly when you reach for it.
///
/// Only the newly-ready server is asked; everything already accumulated stays, and the merge dedupes
/// against it. The picker's *current* generation is used, so a query the user has since changed
/// discards this the same way any late arrival is discarded.
pub async fn requery_ready_server(state: &SharedState, key: &crate::lsp::manager::LspServerKey) {
    let pending: Vec<(ClientId, u64, String, SymbolServer, Vec<PathBuf>)> = {
        let s = state.lock().await;
        let Some(server) = s
            .lsp
            .symbol_servers(&key.workspace)
            .into_iter()
            .find(|srv| srv.root == key.root && srv.language == key.language)
        else {
            return; // not pinned, or doesn't do workspace symbols
        };
        let Some(roots) = s.workspaces.get(&key.workspace).map(|w| w.paths.clone()) else {
            return;
        };
        s.pickers
            .iter()
            .filter(|((_, kind), _)| *kind == PickerKind::WorkspaceSymbols)
            .filter(|((client_id, _), p)| {
                // Only clients actually on this workspace, and only a live query.
                !p.query.is_empty()
                    && s.clients
                        .get(client_id)
                        .and_then(|c| c.active_workspace.as_deref())
                        == Some(key.workspace.as_str())
            })
            .map(|((client_id, _), p)| {
                (
                    *client_id,
                    p.generation,
                    p.query.clone(),
                    server.clone(),
                    roots.clone(),
                )
            })
            .collect()
    };
    for (client_id, generation, query, server, roots) in pending {
        let state = state.clone();
        tokio::spawn(async move {
            let found = query_server(&server, &query, &roots).await;
            merge_results(&state, client_id, generation, found).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, path: &str, line: u32, character: u32) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "kind": 12,
            "location": {
                "uri": crate::lsp::uri::path_to_uri(std::path::Path::new(path)),
                "range": {
                    "start": { "line": line, "character": character },
                    "end": { "line": line, "character": character + 1 },
                },
            },
        })
    }

    /// A UTF-16 column has to be converted against the *line text*, which for an unopened file means
    /// reading it. This is the case a rope would have handled for an open buffer.
    #[test]
    fn converts_utf16_columns_against_the_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        // `é` is 2 bytes but 1 UTF-16 unit, so the two encodings disagree past it.
        std::fs::write(&file, "let é = thing;\n").unwrap();
        let path = file.display().to_string();
        let reply = serde_json::json!([sym("thing", &path, 0, 8)]);

        let utf16 = parse_symbols(&reply, PositionEncoding::Utf16, &[dir.path().to_path_buf()]);
        assert_eq!(
            utf16[0].col, 9,
            "one byte further along than the UTF-16 index"
        );

        let utf8 = parse_symbols(&reply, PositionEncoding::Utf8, &[dir.path().to_path_buf()]);
        assert_eq!(utf8[0].col, 8, "already a byte offset");
    }

    /// Marksman names a setext heading with its underline included (`text\n====`); the first
    /// line is the label — an embedded newline breaks the picker row.
    #[test]
    fn setext_symbol_names_are_cut_to_the_first_line() {
        assert_eq!(
            clean_symbol_name("Setext Heading (H1)\n===================", "/w/doc.md"),
            "Setext Heading (H1)"
        );
        // Not markdown-specific: any server's multi-line name gets one row.
        assert_eq!(clean_symbol_name("fn f(\n  x: u32)", "/w/a.rs"), "fn f(");
    }

    /// Markdown symbol names arrive as raw markup; the label shows the rendered text. Other
    /// languages pass through untouched — `a__b` is an identifier, not emphasis.
    #[test]
    fn markdown_symbol_names_drop_inline_markup() {
        assert_eq!(
            clean_symbol_name("Styled `heading` with a [link](https://example.com)", "/w/d.md"),
            "Styled heading with a link"
        );
        assert_eq!(clean_symbol_name("**Bold** and *em* and ~~gone~~", "/w/d.md"), "Bold and em and gone");
        // Inline context: a numbered heading must not parse as an ordered list.
        assert_eq!(clean_symbol_name("1. Introduction", "/w/d.md"), "1. Introduction");
        assert_eq!(clean_symbol_name("__init__ and `*ptr`", "/w/mod.rs"), "__init__ and `*ptr`");
    }

    /// A 3.17 `WorkspaceSymbol` may carry a location with no range; there's nowhere to jump, so it's
    /// dropped rather than landing you at 0:0.
    #[test]
    fn drops_symbols_with_no_range() {
        let reply = serde_json::json!([{
            "name": "unresolved",
            "kind": 12,
            "location": { "uri": "file:///tmp/x.rs" },
        }]);
        assert!(parse_symbols(&reply, PositionEncoding::Utf8, &[]).is_empty());
    }

    /// One chatty server must not crowd out the others.
    #[test]
    fn caps_each_server_before_merging() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "x\n").unwrap();
        let path = file.display().to_string();
        let many: Vec<_> = (0..PER_SERVER_LIMIT + 50)
            .map(|i| sym(&format!("s{i}"), &path, 0, 0))
            .collect();
        let out = parse_symbols(&serde_json::Value::Array(many), PositionEncoding::Utf8, &[]);
        assert_eq!(out.len(), PER_SERVER_LIMIT);
    }

    /// Inside a root it's relative; outside — a dependency, the stdlib — it's absolute, because
    /// there's no root to be relative to.
    #[test]
    fn display_path_is_relative_only_inside_a_root() {
        let roots = [PathBuf::from("/w/proj")];
        assert_eq!(
            display_path(Path::new("/w/proj/src/a.rs"), &roots),
            "src/a.rs"
        );
        assert_eq!(
            display_path(Path::new("/elsewhere/dep.rs"), &roots),
            "/elsewhere/dep.rs"
        );
    }
}
