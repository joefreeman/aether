//! `search/*` — buffer search: set, clear, step, and the match/summary state pushed to clients.

use super::*;

// ---- search/* ----------------------------------------------------------------------------------

pub const SEARCH_MAX_MATCHES: usize = 10_000;

/// Run `query` against the buffer and produce a fresh `SearchEntry`, honouring `options`: by
/// default the query is matched literally (escaped), and `regex` opts into regex syntax;
/// `whole_word` wraps it in `\b…\b`, and `case` selects smartcase (case-insensitive unless the
/// query has an uppercase letter), forced-sensitive or forced-insensitive. `multi_line: true`
/// throughout. Zero-width matches are skipped so patterns like `^` don't pin the cursor.
pub fn compute_search_entry(
    buf: &Document,
    query: &str,
    options: &MatchOptions,
) -> Result<SearchEntry, RpcError> {
    if query.is_empty() {
        return Ok(SearchEntry {
            query: String::new(),
            options: *options,
            matches: Vec::new(),
            truncated: false,
            last_pushed_index: 0,
        });
    }
    // Shared with the Git-changes picker so query semantics (fixed-string, whole-word, smartcase)
    // stay in lock-step across content searches.
    let regex = picker_state::build_match_regex(query, options)
        .map_err(|e| RpcError::new(ErrorCode::INVALID_PARAMS, format!("invalid regex: {e}")))?;
    let mut matches: Vec<(LogicalPosition, LogicalPosition)> = Vec::new();
    let mut truncated = false;
    let len_bytes = buf.text.len_bytes();
    if len_bytes == 0 {
        return Ok(SearchEntry {
            query: query.to_string(),
            options: *options,
            matches,
            truncated,
            last_pushed_index: 0,
        });
    }
    let source: String = buf.text.chunks().collect();
    for m in regex.find_iter(&source) {
        if matches.len() >= SEARCH_MAX_MATCHES {
            truncated = true;
            break;
        }
        if m.start() == m.end() {
            continue;
        }
        matches.push((
            byte_to_logical(buf, m.start()),
            byte_to_logical(buf, m.end()),
        ));
    }
    Ok(SearchEntry {
        query: query.to_string(),
        options: *options,
        matches,
        truncated,
        last_pushed_index: 0,
    })
}

pub async fn search_set(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    mut params: SearchSetParams,
) -> Result<SearchSetResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let buf = s
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let key = (client_id, params.buffer_id);

    let mut cursor = s.cursors.get(&key).copied().unwrap_or_default();
    // Composite pre-step: derive the query from the selection — `Alt-/` searches the selected text
    // literally. Empty selection = no-op.
    let mut effective_query = None;
    if params.from_selection {
        let (start, end) = scope_range(buf, &cursor, CopyScope::Selection);
        let text = buf.text.slice(start..end).to_string();
        if text.is_empty() {
            return Ok(SearchSetResult {
                cursor: wrap_for_response(&s, client_id, params.buffer_id, cursor),
                summary: SearchSummary {
                    buffer_id: params.buffer_id,
                    total: 0,
                    truncated: false,
                    current_index: 0,
                },
                query: None,
            });
        }
        params.query = text;
        // Search the selection literally: clear `regex` (the default) so `build_match_regex`
        // escapes it for us. The query stored/shown is then the raw selection text, not an escaped
        // pattern. Case / whole-word still apply.
        params.options.regex = false;
        effective_query = Some(params.query.clone());
    }
    let (summary, pushes) = if params.query.is_empty() {
        s.searches.remove(&key);
        let summary = SearchSummary {
            buffer_id: params.buffer_id,
            total: 0,
            truncated: false,
            current_index: 0,
        };
        let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
        (summary, pushes)
    } else {
        let mut entry = compute_search_entry(buf, &params.query, &params.options)?;
        // If the caller passed an anchor, jump the cursor to the first match at-or-after it
        // (wrapping to the first match if none). This is how incremental search keeps the cursor
        // anchored at `/`-press time across keystrokes.
        if let Some(anchor_pos) = params.anchor {
            let (target, wrapped) = first_match_at_or_after_with_wrap(&entry, anchor_pos);
            if let Some((start, end_excl)) = target {
                let start_char = motion::pos_to_char(buf, start);
                let end_char_excl = motion::pos_to_char(buf, end_excl);
                let last_char = end_char_excl.saturating_sub(1).max(start_char);
                let position = motion::char_to_pos(buf, last_char);
                // `?`-search grows the selection from where `?` was pressed (`anchor_pos`) through
                // the match. A wrap is the exception — extending across the buffer boundary would
                // engulf the whole span, so on wrap we reset to selecting just the match, exactly
                // like `search/next`. Plain `/` always selects just the match.
                let anchor_p = if params.extend && !wrapped {
                    anchor_pos
                } else {
                    motion::char_to_pos(buf, start_char)
                };
                let new_cursor = CursorState {
                    position,
                    anchor: anchor_p,
                    match_bracket: None,
                    jumplist_position: None,
                };
                let prev_cursor = cursor;
                set_cursor(&mut s, key, new_cursor);
                s.record_motion(key, prev_cursor, new_cursor);
                s.virtual_col.remove(&key);
                s.clear_tree_selection_history(client_id, params.buffer_id);
                cursor = new_cursor;
            }
        }
        let buf_ref = s.doc_of(params.buffer_id);
        let summary = summary_for(buf_ref, &entry, params.buffer_id, &cursor);
        entry.last_pushed_index = summary.current_index;
        s.searches.insert(key, entry);
        // A real search owns the highlight layer: drop any symbol-highlight set so it can't show
        // through, and so it won't reappear stale when this search is later cleared (the client
        // re-requests highlights on search exit).
        s.symbol_highlights.remove(&key);
        s.symbol_highlight_gen.remove(&key);
        let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
        (summary, pushes)
    };
    // Stamp `match_bracket` + `grep_position` before sending — without this, the freshly-jumped
    // cursor would arrive at the client missing the status-bar indicators that derive from it.
    let cursor = wrap_for_response(&s, client_id, params.buffer_id, cursor);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(SearchSetResult {
        cursor,
        summary,
        query: effective_query,
    })
}

/// First match at-or-after `pos`, falling back to the first match in the buffer (a wrap). Returns
/// the match plus whether it was reached by wrapping, so `?`-search can reset its selection on a
/// wrap rather than extending across the buffer boundary.
fn first_match_at_or_after_with_wrap(
    entry: &SearchEntry,
    pos: LogicalPosition,
) -> (Option<(LogicalPosition, LogicalPosition)>, bool) {
    let found = entry
        .matches
        .iter()
        .copied()
        .find(|(start, _)| pos_tuple(*start) >= pos_tuple(pos));
    let wrapped = found.is_none();
    let target = found.or_else(|| entry.matches.first().copied());
    (target, wrapped)
}

pub async fn search_clear(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchClearParams,
) -> Result<(), RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if !s.buffers.contains_key(&params.buffer_id) {
        return Err(RpcError::buffer_not_found(params.buffer_id));
    }
    s.searches.remove(&(client_id, params.buffer_id));
    let pushes = collect_viewport_refresh(&s, client_id, params.buffer_id);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(())
}
