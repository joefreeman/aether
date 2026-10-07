//! `search/*` — search over a view: set, clear, step, and the summary the client sees.
//!
//! The model lives in [`crate::view_search`]; this is the wire and the cursor. What is specific to
//! stepping is **seating**: every match has a place the cursor can sit beside it — the match itself
//! for buffer text, the line below a removed line, the first line under a command — and landing on
//! a match in another element moves focus there, exactly as a line motion walking out of its
//! element does.

use super::*;
use crate::view_search::{self, MatchAt, MatchKey, ViewMatch, ViewSearch};
use aether_protocol::search::SearchAnchor;
use aether_protocol::viewport::ViewportFocusElementResult;
use aether_protocol::ViewId;

/// The element holding the client's cursor in `view_id`, and the buffer it windows. The first
/// element when the client has no viewport on the view, which is where one would start.
fn focus_of(s: &ServerState, client_id: ClientId, view_id: ViewId) -> (FieldId, BufferId) {
    let view = s.view(view_id);
    match s.viewport_on(client_id, view_id) {
        Some(vp) => (vp.focused, vp.buffer_id(view)),
        None => (0, view.elements[0].buffer_id),
    }
}

type FieldId = aether_protocol::ui::FieldId;

/// The summary `client_id` sees of its search on `view_id`, or the zero summary when there is none.
fn summary_of(s: &ServerState, client_id: ClientId, view_id: ViewId) -> SearchSummary {
    match s.searches.get(&(client_id, view_id)) {
        Some(search) => view_search::summary(
            search,
            view_id,
            s.view(view_id),
            s.viewport_on(client_id, view_id),
        ),
        None => SearchSummary {
            view_id,
            total: 0,
            truncated: false,
            current_index: 0,
            folded: 0,
        },
    }
}

/// Make `key` the current match and record its index as pushed — the result carrying the summary
/// is the push.
fn set_current(s: &mut ServerState, client_id: ClientId, view_id: ViewId, key: Option<MatchKey>) {
    if let Some(search) = s.searches.get_mut(&(client_id, view_id)) {
        search.current = key;
    }
    let index = summary_of(s, client_id, view_id).current_index;
    if let Some(search) = s.searches.get_mut(&(client_id, view_id)) {
        search.last_pushed_index = index;
    }
}

/// The client's cursor in its focused element, as the response decorates it.
fn focused_cursor(s: &ServerState, client_id: ClientId, view_id: ViewId) -> CursorState {
    let (_, buffer_id) = focus_of(s, client_id, view_id);
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    wrap_for_response(s, client_id, buffer_id, cursor)
}

/// How the cursor takes a text match: which end is the anchor, which the head.
#[derive(Clone, Copy)]
enum Take {
    /// Select the match, the head leading in the travel direction.
    Select(Direction),
    /// Keep `anchor` and put the head on the match's edge in the travel direction — the `?` entry
    /// and `Shift-n`.
    Extend {
        anchor: LogicalPosition,
        direction: Direction,
    },
}

/// Seat the cursor beside `m` and move focus to its element. Returns the crossing when focus moved.
///
/// Where the cursor goes is the match's **seat**: the match itself for buffer text (selected, as
/// `n` always did), the line a removed line sits above, the first line under a command. An element
/// that cannot hold a cursor — a run that printed nothing — has no seat: the cursor and focus stay
/// where they are, and the match is current all the same.
fn seat(
    s: &mut ServerState,
    client_id: ClientId,
    view_id: ViewId,
    m: &ViewMatch,
    take: Take,
) -> Result<Option<ViewportFocusElementResult>, RpcError> {
    let vp = s.viewport_on(client_id, view_id);
    let (viewport_id, from_element) = (vp.map(|vp| vp.id), focus_of(s, client_id, view_id).0);
    let Some(binding) = s.view(view_id).elements.get(m.element as usize).cloned() else {
        return Ok(None);
    };
    // With no viewport nothing is folded open — how a fresh one starts.
    let holds = match vp {
        Some(vp) => vp.can_hold_cursor(&binding),
        None => !binding.is_empty() && !binding.prose && !binding.collapsible,
    };
    if !holds {
        return Ok(None);
    }
    let buffer_id = binding.buffer_id;
    let doc = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let point = |pos: LogicalPosition| {
        let p = motion::clamp_position(doc, pos);
        (p, p)
    };
    let (anchor, position) = match m.at {
        MatchAt::Text { start, end } => {
            // The inclusive end, by char arithmetic so a multi-byte match stays on char
            // boundaries — how a `Char` motion counts.
            let start_char = motion::pos_to_char(doc, start);
            let last_char = motion::pos_to_char(doc, end)
                .saturating_sub(1)
                .max(start_char);
            let (first, last) = (
                motion::char_to_pos(doc, start_char),
                motion::char_to_pos(doc, last_char),
            );
            match take {
                Take::Select(Direction::Forward) => (first, last),
                Take::Select(Direction::Backward) => (last, first),
                Take::Extend { anchor, direction } => {
                    let head = match direction {
                        Direction::Forward => last,
                        Direction::Backward => first,
                    };
                    (anchor, head)
                }
            }
        }
        MatchAt::Phantom { line, .. } => point(LogicalPosition { line, col: 0 }),
        MatchAt::Chrome { .. } => point(LogicalPosition {
            line: binding.start_line(),
            col: 0,
        }),
    };
    let crossed = m.element != from_element;
    if crossed {
        // Focus is a viewport's; without one there is nowhere to record that it moved.
        let Some(viewport) = viewport_id.and_then(|id| s.viewports.get_mut(&id)) else {
            return Ok(None);
        };
        viewport.focused = m.element;
    }
    let key = (client_id, buffer_id);
    let from = s.cursors.get(&key).copied().unwrap_or_default();
    let to = CursorState {
        position,
        anchor,
        match_bracket: None,
        jumplist_position: None,
    };
    // Landed like any motion — recorded for motion undo, virtual column and tree history dropped.
    // The current-match update it collects is superseded by the caller's, which knows the match.
    let (cursor, _) = commit_move(s, client_id, buffer_id, from, to, None);
    if !crossed {
        return Ok(None);
    }
    Ok(Some(ViewportFocusElementResult {
        element: m.element,
        buffer: describe_buffer(s, buffer_id, cursor)?,
        buffer_status: buffer_status_for(s, client_id, buffer_id),
    }))
}

/// The client's viewport on `view_id`, re-rendered — how a search set or cleared repaints.
fn view_refresh(s: &ServerState, client_id: ClientId, view_id: ViewId) -> PendingPushes {
    let Some(vp) = s.viewport_on(client_id, view_id) else {
        return Vec::new();
    };
    let Some(sender) = s.clients.get(&client_id).map(|c| c.outbound.clone()) else {
        return Vec::new();
    };
    vec![(
        sender,
        build_lines_changed_notif(s, vp, lines_changed_cursor(s, vp), SneakLabels::Shown),
    )]
}

pub async fn search_set(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    mut params: SearchSetParams,
) -> Result<SearchSetResult, RpcError> {
    let client_id = ctx.client_id;
    let view_id = params.view_id;
    let key = (client_id, view_id);
    let mut s = state.lock().await;
    if s.try_view(view_id).is_none() {
        return Err(RpcError::view_not_found(view_id));
    }
    let (_, focused_buffer) = focus_of(&s, client_id, view_id);

    // Composite pre-step: derive the query from the selection — `Alt-/` searches the selected text
    // literally. Empty selection = no-op.
    let mut effective_query = None;
    if params.from_selection {
        let buf = s.doc_of(focused_buffer);
        let cursor = s
            .cursors
            .get(&(client_id, focused_buffer))
            .copied()
            .unwrap_or_default();
        let (start, end) = scope_range(buf, &cursor, CopyScope::Selection);
        let text = buf.text.slice(start..end).to_string();
        if text.is_empty() {
            return Ok(SearchSetResult {
                cursor: focused_cursor(&s, client_id, view_id),
                summary: summary_of(&s, client_id, view_id),
                query: None,
                crossed: None,
            });
        }
        params.query = text;
        // Search the selection literally: clear `regex` (the default) so `build_match_regex`
        // escapes it for us. The query stored/shown is then the raw selection text, not an escaped
        // pattern. Case / whole-word still apply.
        params.options.regex = false;
        effective_query = Some(params.query.clone());
    }

    let mut crossed = None;
    if params.query.is_empty() {
        s.searches.remove(&key);
    } else {
        let regex = picker_state::build_match_regex(&params.query, &params.options)
            .map_err(|e| RpcError::new(ErrorCode::INVALID_PARAMS, format!("invalid regex: {e}")))?;
        let (matches, truncated) = view_search::find(&s, s.view(view_id), &regex);
        s.searches.insert(
            key,
            ViewSearch {
                query: params.query.clone(),
                options: params.options,
                matches,
                truncated,
                current: None,
                last_pushed_index: 0,
            },
        );
        // A real search owns the highlight layer: drop any symbol-highlight set so it can't show
        // through, and so it won't reappear stale when this search is later cleared (the client
        // re-requests highlights on search exit).
        s.symbol_highlights.remove(&(client_id, focused_buffer));
        s.symbol_highlight_gen.remove(&(client_id, focused_buffer));

        let current = match params.anchor {
            // Incremental: the first match at-or-after where `/` was pressed is current, wrapping
            // to the first when there is none below.
            // `?` grows the selection from where it was pressed through the match, so it only lands
            // on text in the element it was pressed in — a selection lives in one buffer. A match
            // past that element leaves the cursor where it was, as an extending step does.
            Some(anchor) => match first_at_or_after(&s, client_id, view_id, anchor, params.extend)
                .filter(|(target, _)| !params.extend || target.element == anchor.element)
            {
                Some((target, wrapped)) => {
                    // Not across the wrap, which would engulf everything between: just the match,
                    // as a wrapped step does.
                    let take = if params.extend && !wrapped {
                        Take::Extend {
                            anchor: anchor.position,
                            direction: Direction::Forward,
                        }
                    } else {
                        Take::Select(Direction::Forward)
                    };
                    crossed = seat(&mut s, client_id, view_id, &target, take)?;
                    Some(target.key())
                }
                None => None,
            },
            None => {
                let search = &s.searches[&key];
                view_search::derive_current(
                    &s,
                    client_id,
                    search,
                    s.view(view_id),
                    s.viewport_on(client_id, view_id),
                )
            }
        };
        set_current(&mut s, client_id, view_id, current);
    }
    let pushes = view_refresh(&s, client_id, view_id);
    // After the refresh, which carries the same summary on its window: one answer, two routes.
    let result = SearchSetResult {
        cursor: focused_cursor(&s, client_id, view_id),
        summary: summary_of(&s, client_id, view_id),
        query: effective_query,
        crossed,
    };
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(result)
}

/// The first shown match at-or-after `anchor`, falling back to the first shown match (a wrap) —
/// text matches only when `text_only`, for a `?` that ends a selection on it.
fn first_at_or_after(
    s: &ServerState,
    client_id: ClientId,
    view_id: ViewId,
    anchor: SearchAnchor,
    text_only: bool,
) -> Option<(ViewMatch, bool)> {
    let search = s.searches.get(&(client_id, view_id))?;
    let view = s.view(view_id);
    let vp = s.viewport_on(client_id, view_id);
    let from = MatchKey::text(anchor.element, anchor.position);
    let shown: Vec<ViewMatch> = view_search::shown(search, view, vp)
        .map(|(_, m)| *m)
        .filter(|m| !text_only || m.is_text())
        .collect();
    match shown.iter().find(|m| m.key() >= from) {
        Some(m) => Some((*m, false)),
        None => shown.first().map(|m| (*m, true)),
    }
}

pub async fn search_clear(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchClearParams,
) -> Result<(), RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if s.try_view(params.view_id).is_none() {
        return Err(RpcError::view_not_found(params.view_id));
    }
    s.searches.remove(&(client_id, params.view_id));
    let pushes = view_refresh(&s, client_id, params.view_id);
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(())
}

/// `search/step` — step `count` matches in `params.direction`, handling the composite params:
/// optional query revive first (skipping the step when it has no matches — same early-out the
/// clients used), then `count` steps.
pub async fn search_step(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: SearchStepParams,
) -> Result<SearchNavResult, RpcError> {
    if let Some(query) = params.set_query.clone() {
        let set = search_set(
            state,
            ctx,
            SearchSetParams {
                view_id: params.view_id,
                query,
                anchor: None,
                extend: false,
                from_selection: false,
                options: params.options,
            },
        )
        .await?;
        if set.summary.total == 0 {
            return Ok(SearchNavResult {
                cursor: set.cursor,
                summary: set.summary,
                crossed: None,
            });
        }
    }
    let mut crossed = None;
    let mut last = None;
    for _ in 0..params.count.max(1) {
        let step =
            search_navigate(state, ctx, params.view_id, params.direction, params.extend).await?;
        // Several steps can cross several times; what the client rebinds to is where it ended.
        crossed = step.crossed.clone().or(crossed);
        last = Some(step);
    }
    let mut last = last.expect("count.max(1) iterations");
    if let Some(crossed) = crossed.as_mut() {
        // A later step within the element the first one crossed into moved the cursor again.
        crossed.buffer.cursor = last.cursor;
    }
    last.crossed = crossed;
    Ok(last)
}

async fn search_navigate(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    view_id: ViewId,
    direction: Direction,
    extend: bool,
) -> Result<SearchNavResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    if s.try_view(view_id).is_none() {
        return Err(RpcError::view_not_found(view_id));
    }
    let (element, buffer_id) = focus_of(&s, client_id, view_id);
    let target = step_target(
        &s, client_id, view_id, element, buffer_id, direction, extend,
    )
    // A selection lives in one buffer, so an extending step whose next match is in another
    // element has nowhere to grow to: it does nothing, rather than throw the selection away.
    .filter(|(target, _)| !extend || target.element == element);
    let Some((target, wrapped)) = target else {
        return Ok(SearchNavResult {
            cursor: focused_cursor(&s, client_id, view_id),
            summary: summary_of(&s, client_id, view_id),
            crossed: None,
        });
    };
    let current = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    // Extend pins the anchor and lands the head on the match's near edge in the travel direction,
    // re-anchoring via `extend_anchor` so reversing direction grows the selection instead of
    // discarding the span already covered on the far side. A wrap is the exception: growing across
    // the boundary would engulf everything between, so it resets to just the match.
    let take = if extend && !wrapped {
        let MatchAt::Text { start, end } = target.at else {
            unreachable!("an extending step only visits text");
        };
        let doc = s.doc_of(buffer_id);
        let head = match direction {
            Direction::Forward => {
                let start_char = motion::pos_to_char(doc, start);
                let last = motion::pos_to_char(doc, end)
                    .saturating_sub(1)
                    .max(start_char);
                motion::char_to_pos(doc, last)
            }
            Direction::Backward => start,
        };
        Take::Extend {
            anchor: extend_anchor(&current, head),
            direction,
        }
    } else {
        Take::Select(direction)
    };
    let crossed = seat(&mut s, client_id, view_id, &target, take)?;
    set_current(&mut s, client_id, view_id, Some(target.key()));
    Ok(SearchNavResult {
        cursor: focused_cursor(&s, client_id, view_id),
        summary: summary_of(&s, client_id, view_id),
        crossed,
    })
}

/// The match a step lands on, and whether reaching it wrapped.
///
/// Stepped from the current match when that is not text — a removed line or a command, which the
/// cursor only sits *beside*. Otherwise from the selection's far edge in the travel direction: the
/// head in the usual case, but using the edge means a direction reversal off a match steps to the
/// adjacent one instead of re-selecting it, and a plain step after a multi-match `Shift`-extend
/// steps off the whole selection instead of landing back inside it.
fn step_target(
    s: &ServerState,
    client_id: ClientId,
    view_id: ViewId,
    element: FieldId,
    buffer_id: BufferId,
    direction: Direction,
    extend: bool,
) -> Option<(ViewMatch, bool)> {
    let search = s.searches.get(&(client_id, view_id))?;
    let view = s.view(view_id);
    let vp = s.viewport_on(client_id, view_id);
    // A selection ends on text, so an extending step only visits text.
    let shown: Vec<ViewMatch> = view_search::shown(search, view, vp)
        .map(|(_, m)| *m)
        .filter(|m| !extend || m.is_text())
        .collect();
    let beside = search
        .current
        .filter(|key| shown.iter().any(|m| !m.is_text() && m.key() == *key));
    let reference = beside.unwrap_or_else(|| {
        let cursor = s
            .cursors
            .get(&(client_id, buffer_id))
            .copied()
            .unwrap_or_default();
        let (lo, hi) = if pos_tuple(cursor.anchor) <= pos_tuple(cursor.position) {
            (cursor.anchor, cursor.position)
        } else {
            (cursor.position, cursor.anchor)
        };
        MatchKey::text(
            element,
            match direction {
                Direction::Forward => hi,
                Direction::Backward => lo,
            },
        )
    });
    let found = match direction {
        Direction::Forward => shown.iter().find(|m| m.key() > reference),
        Direction::Backward => shown.iter().rev().find(|m| m.key() < reference),
    };
    match found {
        Some(m) => Some((*m, false)),
        None => match direction {
            Direction::Forward => shown.first(),
            Direction::Backward => shown.last(),
        }
        .map(|m| (*m, true)),
    }
}

/// New anchor for an *extending* move whose cursor lands on `head`. Normally the anchor is kept, so
/// the selection is the usual `[anchor, head]`. But when `head` falls on the opposite side of the
/// anchor from the *current* cursor — a direction reversal across the pivot — keeping the anchor
/// would throw away the span the selection already covered on the old side. In that case we
/// re-anchor to the previous cursor position so the move grows the selection rather than collapsing
/// it across the pivot. The decision is based purely on where `head` lands relative to the current
/// selection, so it's independent of which binding (next/prev, etc.) drove the move.
pub fn extend_anchor(current: &CursorState, head: LogicalPosition) -> LogicalPosition {
    let a = pos_tuple(current.anchor);
    let c = pos_tuple(current.position);
    let h = pos_tuple(head);
    let crosses_pivot = (c < a && h > a) || (c > a && h < a);
    if crosses_pivot {
        current.position
    } else {
        current.anchor
    }
}

pub fn pos_tuple(p: LogicalPosition) -> (u32, u32) {
    (p.line, p.col)
}

/// The view the client is working in that windows `buffer_id` — the one a cursor move in that
/// buffer belongs to.
fn view_moving(s: &ServerState, client_id: ClientId, buffer_id: BufferId) -> Option<ViewId> {
    s.viewports
        .values()
        .find(|v| v.client_id == client_id && s.view_of(v).binds(buffer_id))
        .map(|v| v.view_id)
}

/// After the cursor moved in `buffer_id` (or focus moved to it): the current match is re-derived
/// from where the head now is — the text match it is inside, else none — and the summary is pushed
/// when that changed its index. A move is the one change to a search that re-renders nothing, so
/// it needs a push of its own; everything else rides the window.
pub fn collect_cursor_search_update(
    s: &mut ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Option<(mpsc::Sender<Notification>, Notification)> {
    let view_id = view_moving(s, client_id, buffer_id)?;
    let key = (client_id, view_id);
    let current = {
        let search = s.searches.get(&key)?;
        view_search::derive_current(
            s,
            client_id,
            search,
            s.view(view_id),
            s.viewport_on(client_id, view_id),
        )
    };
    s.searches.get_mut(&key)?.current = current;
    let summary = summary_of(s, client_id, view_id);
    let search = s.searches.get_mut(&key)?;
    if summary.current_index == search.last_pushed_index {
        return None;
    }
    search.last_pushed_index = summary.current_index;
    let session = s.clients.get(&client_id)?;
    Some((
        session.outbound.clone(),
        Notification {
            jsonrpc: JsonRpc,
            method: SearchStateChanged::NAME.into(),
            params: serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null),
        },
    ))
}

/// Recompute every search over a view windowing this buffer's document, after a mutation. The
/// summaries ride the window the mutation re-renders, so there is nothing to push from here.
pub fn refresh_searches_for_buffer(s: &mut ServerState, buffer_id: BufferId) {
    if !s.buffers.contains_key(&buffer_id) {
        return;
    }
    // Fan out over the document: a mutation through one buffer shifts byte positions for every
    // sibling buffer sharing the content too.
    let attached = s.doc_siblings(buffer_id);
    // A mutation shifts byte positions, so any symbol-highlight set is now stale. Drop it (and its
    // debounce generation, which invalidates any in-flight refresh); the client re-requests
    // highlights when its cursor lands after the edit.
    s.symbol_highlights
        .retain(|(_, b), _| !attached.contains(b));
    s.symbol_highlight_gen
        .retain(|(_, b), _| !attached.contains(b));
    view_search::refresh_views_binding(s, &attached);
}

/// Whether one of the client's searches covers `buffer_id` — a search on a view windowing it. A
/// search owns the highlight layer there, so symbol highlights stand down.
pub fn search_covers(s: &ServerState, client_id: ClientId, buffer_id: BufferId) -> bool {
    s.searches
        .keys()
        .any(|(c, v)| *c == client_id && s.views.get(v).is_some_and(|view| view.binds(buffer_id)))
}

/// Convert a buffer-wide byte offset to a `(line, col_bytes)` position.
pub fn byte_to_logical(buf: &Document, byte_idx: usize) -> LogicalPosition {
    let char_idx = buf.text.byte_to_char(byte_idx);
    let line_idx = buf.text.char_to_line(char_idx);
    let line_start_char = buf.text.line_to_char(line_idx);
    let char_offset = char_idx - line_start_char;
    let line_slice = buf.text.line(line_idx);
    let col_bytes = line_slice.char_to_byte(char_offset);
    LogicalPosition {
        line: line_idx as u32,
        col: col_bytes as u32,
    }
}
