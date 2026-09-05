//! `viewport/*` — subscribe, scroll, resize, wrap, and the window rendering behind them.
//!
//! Also owns the per-line decoration that feeds a window: diff markers and phantom deleted rows,
//! intra-line emphasis, conflict bands, and the git-hunk recomputation they read from. Those live
//! here rather than with the git handlers because they exist to build a `LogicalLineRender`, not to
//! operate on a repository.

use super::*;
use aether_protocol::coords::ElementRow;

pub async fn viewport_subscribe(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportSubscribeParams,
) -> Result<ViewportSubscribeResult, RpcError> {
    let client_id = ctx.client_id;
    // One named crossing, at the boundary: everything below renders the view's own document and its
    // elements, all of which are buffer questions. `params.buffer_id` stays a `ViewId` for the two
    // places that want the view's identity — the `Viewport` it builds and the render it drives.
    let buffer_id = params.buffer_id.presenting_buffer();

    let mut s = state.lock().await;
    s.try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    // The buffer may have been mutated while nothing was viewing it — an edit through a sibling
    // buffer in another workspace, or a reload — and the per-mutation refresh skips buffers with
    // no viewport. This is the moment that stops being true, so re-diff before rendering, or the
    // first frame shows a clean gutter for a modified file and stays wrong until the next edit.
    rediff_git_for_buffer(&mut s, buffer_id);

    // The view exists from the first presentation on, as the kind this presentation asks for; a
    // driver's view is already there and is what it is.
    s.present_view(params.buffer_id, params.kind);
    let geom = wrap::WrapGeometry {
        wrap: params.wrap,
        cols: params.cols,
        marker_width: params.continuation_marker_width,
        tab_width: params.tab_width,
    };
    let (focused, loaded, anchor) = {
        let view = s.view(params.buffer_id);
        let last = view.elements.len().saturating_sub(1) as aether_protocol::viewport::FieldId;
        // The element the scroll names is the one being looked at — a patch opened on the file
        // you picked, a session restored where you left it — and so, on a fresh open, the one the
        // cursor is in. A re-subscribe says where focus already is instead, so a wrap toggle does
        // not move the cursor to whichever element happens to be at the top of the screen.
        let anchor_element = params.scroll.element.min(last);
        let focused = params.focus.unwrap_or(anchor_element).min(last);
        let layout = s.layout_of(&view.elements);
        let binding = &view.elements[anchor_element as usize];
        let range = layout.element_range(anchor_element);
        let (geom, phantoms) = element_geometry(&s, binding, geom, params.diff_view);
        let doc = s.doc_of(binding.buffer_id);
        let line = params.scroll.line.clamp(
            range.start(),
            range.end_exclusive().saturating_sub(1).max(range.start()),
        );
        // A screen of the anchor's element from its line, less the overscan above — the same slice
        // the client would ask for once it has laid the view out, so the first frame needs no
        // second round trip. An element the client lays out is loaded whole: see
        // `whole_if_client_laid_out`.
        let from_row = element_rows_before(doc, geom, &phantoms, range, line)
            .saturating_sub(params.overscan_rows);
        let slice = whole_if_client_laid_out(binding, range, || {
            slice_from(
                doc,
                geom,
                &phantoms,
                range,
                from_row,
                params.rows + 2 * params.overscan_rows,
            )
        });
        let mut loaded = vec![None; view.elements.len()];
        if !slice.is_empty() {
            loaded[anchor_element as usize] = Some(slice);
        }
        (
            focused,
            loaded,
            ScrollPosition {
                element: anchor_element,
                line,
                sub_row: params.scroll.sub_row,
            },
        )
    };

    // The viewport exists before its first render: the render reads everything off it, as every
    // later one does, so subscribe cannot describe a geometry the viewport does not then hold.
    let viewport_id = s.allocate_viewport_id();
    s.viewports.insert(
        viewport_id,
        Viewport {
            id: viewport_id,
            view_id: params.buffer_id,
            focused,
            client_id,
            rows: params.rows,
            overscan_rows: params.overscan_rows,
            wrap: params.wrap,
            tab_width: params.tab_width,
            diff_view: params.diff_view,
            cols: params.cols,
            continuation_marker_width: params.continuation_marker_width,
            loaded,
            anchor,
        },
    );
    // Gutter markers ride `hunks` regardless of the diff toggle; the inline view honours the
    // client's sticky setting. Hunks are seeded on open (`load_baseline`) and kept fresh per edit,
    // so they're accurate here without the recompute `git_set_diff_view` does.
    let window = render_viewport(&s, viewport_id, SneakLabels::Shown);
    s.last_scroll.insert((client_id, params.buffer_id), anchor);
    tracing::debug!(%client_id, viewport_id, buffer_id, element = anchor.element, line = anchor.line, "viewport subscribed");

    // One logical viewport per client: a new subscribe supersedes the client's previous
    // viewport(s), which the clients historically never unsubscribed. Dropping the stale
    // entries here keeps "has a viewport" meaning "is showing the buffer" — which is also
    // what lets a transient buffer detect it just went hidden and close itself.
    let left_buffers: Vec<BufferId> = {
        let stale: Vec<aether_protocol::ViewportId> = s
            .viewports
            .iter()
            .filter(|(id, v)| v.client_id == client_id && **id != viewport_id)
            .map(|(id, _)| *id)
            .collect();
        let mut buffers: Vec<BufferId> = Vec::new();
        for id in stale {
            if let Some(v) = s.viewports.remove(&id) {
                // Everything that viewport was showing, not just the element under its cursor. A
                // composed view keeps its own document *and* a buffer per element alive; naming
                // only the focused one meant navigating away from working changes left the patch
                // and every other file it had opened behind, unreferenced and uncollected.
                let shown_buffers = match s.try_view(v.view_id) {
                    Some(view) => v.shown_buffers(view),
                    None => vec![v.view_id.presenting_buffer()],
                };
                for shown in shown_buffers {
                    if shown != buffer_id && !buffers.contains(&shown) {
                        buffers.push(shown);
                    }
                }
            }
        }
        buffers
    };
    let (closed, stopped_servers) = s.close_orphaned_transients(left_buffers);
    let mut pushes = Vec::new();
    if !closed.is_empty() {
        for &id in &closed {
            tracing::debug!(buffer_id = id, "transient buffer closed (hidden)");
        }
        pushes.extend(refresh_buffer_pickers(&mut s));
    }
    if !stopped_servers.is_empty() {
        pushes.extend(refresh_lsp_server_pickers(&mut s));
    }

    // Which element holds the cursor and the buffer it windows, with the cursor seated inside it.
    // Answered for every view, not only a composed one: the client mirrors `focused` and it has to
    // start from the server's value, whichever element that is. Answering only when the focused
    // element windowed a *different* buffer left a subscribe that landed in a patch's own text (a
    // deleted file's block, via `focus_path` or a restore) with the server on element N and the
    // client still on element 0 — and no cursor seated in either.
    //
    // It also carries the buffer-level status the client can't derive from the window —
    // external-change flags, diagnostic counts, language-server health, the breadcrumb — snapshotted
    // *after* the cursor is seated, since the breadcrumb is taken from wherever the cursor ends up.
    // These otherwise only reach a client via change-notifications, so a viewport subscribing after
    // the relevant change already happened would show stale state until the next one. Every field
    // is a fact about the buffer under the cursor, which for a composed view is a file and never the
    // generated patch, which has no outline, no diagnostics and no language server.
    let focus = focus_answer(&mut s, client_id, viewport_id)?;
    let buffer_status = focus.buffer_status.clone();
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    Ok(ViewportSubscribeResult {
        viewport_id,
        window,
        buffer_status,
        focus,
    })
}

pub async fn viewport_resize(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportResizeParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    // A resize changes the *view's* width, so every element takes it. They share one width today;
    // when one doesn't — a side-by-side diff — this is where that stops being true.
    vp.cols = params.cols;
    vp.rows = params.rows;
    let window = render_viewport(&s, params.viewport_id, SneakLabels::Shown);
    Ok(ViewportWindowResult { window })
}

/// Seat this client's cursor inside an element's window onto its buffer, keeping it where it is when
/// it is already there.
///
/// The rule **both** ways of establishing focus follow — the answer a subscribe gives and
/// `view/focus_element` — because an element's window is what the view shows of that file, and a
/// cursor outside it is a cursor nothing draws.
///
/// Keeping one that is already inside is what makes focusing idempotent, which is what lets a client
/// establish focus on every subscribe without the cursor hopping on each resize. It matters twice
/// over because cursors are per `(client, buffer)`: a needless move here also yanks any ordinary
/// view of the same file.
fn seat_cursor_in_element(
    s: &mut ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
    start_line: u32,
) -> Result<CursorState, RpcError> {
    let key = (client_id, buffer_id);
    let scope = s.motion_scope(client_id, buffer_id)?;
    // Already inside: nothing moves, and nothing is *written* either — `set_cursor` wakes the
    // cursor-following decorations, which have nothing to follow here. An absent cursor is the
    // origin, as every read of the map takes it to be, so a fresh buffer's first subscribe stores
    // nothing and the breadcrumb follow first fires when the client itself moves. For an ordinary
    // view, whose one element is the whole buffer, this is every subscribe.
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    if scope.contains(current.position) {
        return Ok(current);
    }
    // Clamped, because an element's extent is a claim about its buffer that a concurrent edit
    // could have outrun.
    let position = motion::clamp_position(
        s.doc_of(buffer_id),
        aether_protocol::LogicalPosition {
            line: start_line,
            col: 0,
        },
    );
    let cursor = CursorState {
        position,
        anchor: position,
        match_bracket: None,
        jumplist_position: None,
    };
    set_cursor(s, key, cursor);
    Ok(cursor)
}

/// Whether any element **other than** `focused` windows a buffer with unsaved changes — the
/// view-wide half of the status bar's dirty dot. See [`Window::other_elements_dirty`] for why the
/// focused element is excluded rather than counted.
///
/// Distinct buffers only in spirit: two elements over the same file answer the same way, which is
/// right — one dirty document makes the view dirty however many hunks of it are on screen.
fn other_elements_dirty(
    s: &ServerState,
    elements: &[crate::state::ElementBinding],
    focused: aether_protocol::viewport::FieldId,
) -> bool {
    let focused_buffer = elements.get(focused as usize).map(|e| e.buffer_id);
    elements
        .iter()
        .enumerate()
        .filter(|(i, e)| *i != focused as usize && Some(e.buffer_id) != focused_buffer)
        .any(|(_, e)| s.try_doc_of(e.buffer_id).is_some_and(|d| d.dirty))
}

/// The buffer-level status for `buffer_id`: everything the status bar shows that the window itself
/// cannot say — external-change flags, diagnostic counts, language-server health, and the
/// breadcrumb.
///
/// One definition, because every caller wants it for the same reason and about the same thing: the
/// buffer *under the cursor*. Subscribing seeds it; focusing another element re-seeds it, since
/// crossing an element crosses into another buffer and all four facts change at once. Building it
/// in two places is how the focus path came to answer only half of it.
///
/// Records the breadcrumb as last-sent, so the follow loop's next push is a real change rather than
/// a duplicate of this seed — which is why it needs `&mut`.
fn buffer_status_for(
    s: &mut ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> BufferStatusSnapshot {
    let symbol_path = symbol_path_for(s, client_id, buffer_id);
    s.symbol_path_sent
        .insert((client_id, buffer_id), symbol_path.clone());
    let buf = s.doc_of(buffer_id);
    BufferStatusSnapshot {
        externally_modified: buf.externally_modified,
        externally_deleted: buf.externally_deleted,
        diagnostics: diagnostic_counts(buffer_diagnostics(s, buffer_id)),
        lsp_status: s.lsp.status_for_buffer(buffer_id),
        symbol_path,
    }
}

/// The focus a subscribe answers with: which element holds the cursor and the buffer it windows,
/// with the cursor seated inside that element.
///
/// Seating the cursor is the same rule [`viewport_focus_element`] follows, and for the same reason:
/// an element's window onto a file is what the view shows of it, and a cursor outside that is a
/// cursor nothing draws. It keeps a cursor already inside, so re-subscribing (a wrap toggle, a
/// reconnect) doesn't move it — and cursors are per `(client, buffer)`, so a needless move would
/// also yank an ordinary view of the same file. For an ordinary view the element is the whole
/// buffer and the seating is always a no-op.
fn focus_answer(
    s: &mut ServerState,
    client_id: ClientId,
    viewport_id: aether_protocol::ViewportId,
) -> Result<aether_protocol::viewport::ViewportFocusElementResult, RpcError> {
    let vp = s.viewports.get(&viewport_id).ok_or_else(|| {
        RpcError::internal(format!(
            "viewport {viewport_id} vanished before its subscribe was answered"
        ))
    })?;
    let (element, binding) = (vp.focused, vp.focus(s.view_of(vp)).clone());
    let cursor = seat_cursor_in_element(s, client_id, binding.buffer_id, binding.start_line())?;
    Ok(aether_protocol::viewport::ViewportFocusElementResult {
        element,
        buffer: describe_buffer(s, binding.buffer_id, cursor)?,
        // After the seating above, never before it: seating is what decides where the cursor is,
        // and the breadcrumb is taken from the cursor.
        buffer_status: buffer_status_for(s, client_id, binding.buffer_id),
    })
}

/// `viewport/focus_element`: step focus to the next or previous editor element of this view.
///
/// Stops at the ends rather than wrapping — the same rule `]`/`[` follow for hunks, so repeated
/// presses make progress and then stop instead of silently cycling you back to the top.
pub async fn viewport_focus_element(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::viewport::ViewportFocusElementParams,
) -> Result<aether_protocol::viewport::ViewportFocusElementResult, RpcError> {
    use aether_protocol::viewport::{FocusStep, FocusTarget};

    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    let vp = &s.viewports[&params.viewport_id];
    let view = s.view_of(vp);

    let last = view.elements.len().saturating_sub(1) as u32;
    let focused = match params.target {
        FocusTarget::Step {
            direction: FocusStep::Next,
        } => vp.focused.saturating_add(1).min(last),
        FocusTarget::Step {
            direction: FocusStep::Previous,
        } => vp.focused.saturating_sub(1),
        // Clamped rather than refused: an id names an element the client just saw, and a view that
        // rebuilt underneath it is a stale id, not a protocol error.
        FocusTarget::Element { element } => element.min(last),
    };
    let binding = view
        .elements
        .get(focused as usize)
        .unwrap_or(&view.elements[0]);
    let (buffer_id, start_line) = (binding.buffer_id, binding.start_line());
    s.viewports
        .get_mut(&params.viewport_id)
        .expect("checked above")
        .focused = focused;

    // An element just moved to has no remembered position inside it, so the cursor takes its first
    // line — unless it is already inside this one. See [`seat_cursor_in_element`].
    let cursor = seat_cursor_in_element(&mut s, client_id, buffer_id, start_line)?;
    // An active search is the focused element's, so moving focus re-runs it where the cursor now is.
    let pushes = rescope_search(&mut s, client_id, buffer_id);

    let result = aether_protocol::viewport::ViewportFocusElementResult {
        element: focused,
        buffer: describe_buffer(&s, buffer_id, cursor)?,
        buffer_status: buffer_status_for(&mut s, client_id, buffer_id),
    };
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(result)
}

/// Every change in a view, as `(element, first line of the run)`, in reading order.
///
/// A run is a maximal block of consecutive marked lines: one edit, however many lines it spans, is
/// one stop — the same rule `c` follows in a file, so a patch does not suddenly stutter line by
/// line through a rewritten paragraph.
///
/// One walk over both kinds of element, because a view holds both at once: a **bound** element
/// carries the diff's account of its file as decorations, and its runs come from those; an
/// **unbound** one windows the generated document — a deleted file, a binary swap — and its changes
/// are the ones the patch's own index recorded for that region. This used to treat the index as a
/// fallback for a view with no bound elements at all, so in a review of an edited file beside a
/// deleted one `c` stepped the edit and never arrived at the deletion.
fn change_anchors(
    s: &ServerState,
    vp: &Viewport,
) -> Vec<(aether_protocol::viewport::FieldId, u32)> {
    let mut out = Vec::new();
    let view = s.view_of(vp);
    let view_buffer = vp.view_id.presenting_buffer();
    let generated = s.try_doc_of(view_buffer).and_then(|d| d.generated.as_ref());
    let layout = s.layout_of(&view.elements);
    for (idx, binding) in view.elements.iter().enumerate() {
        let Some(decorations) = binding.decorations.as_deref() else {
            // No diff of its own to read: an ordinary view's one element, or a hunk bound to a file
            // by nothing that marked its lines. Its changes are the buffer's own — the hunks the
            // gutter draws — clipped to what the element windows. The generated document's own
            // unbound regions are answered below from the patch's index instead.
            if generated.is_some() && binding.buffer_id == view_buffer {
                continue;
            }
            let range = layout.element_range(idx as aether_protocol::viewport::FieldId);
            out.extend(
                crate::handlers::buffer_change_anchors(s, binding.buffer_id)
                    .into_iter()
                    .filter(|&line| line >= range.start() && line < range.end_exclusive())
                    .map(|line| (idx as aether_protocol::viewport::FieldId, line)),
            );
            continue;
        };
        let mut lines: Vec<u32> = decorations.markers.keys().copied().collect();
        lines.sort_unstable();
        let mut previous: Option<u32> = None;
        for line in lines {
            if previous != Some(line.wrapping_sub(1)) {
                out.push((idx as aether_protocol::viewport::FieldId, line));
            }
            previous = Some(line);
        }
    }
    // The view's own document — never the focused element's, which is a file whenever focus sits
    // in a bound element.
    if let Some(generated) = generated {
        // Which region of the generated text holds a patch line: the patch's own span table, in
        // patch lines. Not an element's `start_line`, which is a *file* line once the element is
        // bound — comparing the two is the mistake this module exists to make hard.
        let region_of = |patch_line: u32| {
            generated
                .decorations
                .elements
                .iter()
                .rposition(|r| r.start_line <= patch_line)
                .unwrap_or(0)
        };
        let unbound = |element: usize| {
            view.elements
                .get(element)
                .is_some_and(|e| e.decorations.is_none() && e.buffer_id == view_buffer)
        };
        for file in &generated.index.files {
            for change in &file.changes {
                let element = region_of(change.start_line);
                if unbound(element) {
                    out.push((
                        element as aether_protocol::viewport::FieldId,
                        change.start_line,
                    ));
                }
            }
        }
    }
    out.sort_unstable();
    out
}

/// One entry of a composed view's **outline**: a change, where it is, and what to call it.
#[derive(Clone)]
pub struct OutlineEntry {
    /// The element the change lives in.
    pub element: aether_protocol::viewport::FieldId,
    /// Its first line **in that element's buffer** — a file line for a bound element, a line of the
    /// generated document for one with no file behind it. This is the coordinate every consumer
    /// needs, and the one the patch's own index does *not* store: the index speaks patch lines.
    pub line: u32,
    /// Repo-relative path of the file the change is in — the outline's group.
    pub file: String,
    /// What to call the change: git's enclosing signature for its hunk, which is the same thing the
    /// hunk header renders. Empty when git offered none.
    pub label: String,
    /// The change's line in the **patch document**, for the callers that address the patch itself.
    pub patch_line: u32,
    /// The hunk's rows in the patch document, header included: where its lines are found when
    /// the element windows the generated text.
    pub patch_lines: std::ops::Range<u32>,
    /// How the file this change is in is named **durably** — the name [`buffer_identity`] gives
    /// the buffer a bound element windows: a canonical path for a working-tree file, a virtual key
    /// for a file at a revision. Known whether or not the element is bound, so an entry captured
    /// from a bound view can be found again in one that is not. `None` for a change with no file
    /// behind it at all.
    pub identity: Option<String>,
    /// The hunk's lines **in the file** (0-based, new side), whatever the element's binding —
    /// empty for a pure removal, which sits above `start`, and for a delta with no hunks.
    pub file_lines: std::ops::Range<u32>,
}

/// Every buffer a **view** shows, in view order, each named once.
///
/// The set a listing kind fans out over. For an ordinary view it is `[focused]` and the fan-out is
/// a loop of one — which is the whole point of defining these at view level: a one-element view
/// keeps behaving exactly as it did, so a regression can only be a regression in composed views.
///
/// Falls back to `[focused]` when the client sent no view id, which covers an older client and the
/// re-view calls that carry no ids at all. Duplicates are dropped: two hunks of one file are two
/// elements over one buffer, and its diagnostics should be listed once.
pub fn view_element_buffers(
    s: &ServerState,
    view_id: Option<aether_protocol::ViewId>,
    focused: BufferId,
) -> Vec<BufferId> {
    let Some(view) = view_id else {
        return vec![focused];
    };
    let mut out: Vec<BufferId> = Vec::new();
    for element in s.view_elements(view).iter() {
        if !out.contains(&element.buffer_id) {
            out.push(element.buffer_id);
        }
    }
    if out.is_empty() {
        out.push(focused);
    }
    out
}

/// Which element of the view named by `view_key` holds `(abs_path, line)`, and the buffer that
/// element currently windows — where a jumplist entry captured from that view should be seated.
///
/// Keyed by [`crate::state::VirtualSource::key`] rather than a `ViewId`, because a view id is a
/// buffer id and dies with the view; the key still names it after it has been reopened.
///
/// Resolved by asking [`view_outline`] rather than by walking the elements independently, so a jump
/// lands exactly where the picker row it was captured from lands. The two used to be separate
/// derivations and drifted: the picker seated the cursor in the element while the jump addressed
/// the bare file, so `]` yanked the file up instead of moving within the view.
///
/// `None` when the view has no viewport for this client — a subscribe supersedes the client's
/// previous one, so that means it is not on screen — or when it holds no element for that line any
/// more (the patch was rebuilt over it). Both mean the caller should fall back to the entry's own
/// durable file target, which is the whole reason the entry keeps one.
pub fn element_holding(
    s: &ServerState,
    client_id: ClientId,
    view_key: &str,
    identity: &str,
    line: u32,
) -> Option<(aether_protocol::viewport::ViewSeat, LogicalPosition)> {
    let vp = s.viewports.values().find(|vp| {
        vp.client_id == client_id && view_key_of(s, vp.view_id) == Some(view_key.into())
    })?;
    seat_in(
        s,
        vp.view_id.presenting_buffer(),
        &s.view_of(vp).elements,
        identity,
        line,
    )
}

/// The [`crate::state::VirtualSource::key`] of a view, when it is a materialised one.
pub fn view_key_of(s: &ServerState, view: ViewId) -> Option<String> {
    s.try_doc_of(view.presenting_buffer())?
        .virtual_source
        .as_ref()
        .map(|src| src.target.key())
}

/// Which element of an already-built element list holds `(identity, file line)`, and where in
/// that element's buffer the line is.
///
/// Addressed by **file**, not by the buffer id an entry was captured against: a view's element
/// buffers are transient, so by the time anything jumps back the same file may be a different
/// buffer — or the id may name nothing at all. And resolved against the outline's own account of
/// its files rather than against whatever each element happens to window: an element rebuilt
/// without its file describes the same hunk in patch coordinates, and a lookup that compared the
/// captured file line with *that* found nothing and quietly opened the file in an editor instead.
///
/// The line names the hunk it is the top of, or failing that the hunk it falls inside — an entry
/// captured from a changed line rather than a hunk's top, or a hunk whose top has drifted under an
/// edit above it. The position returned is in the element's buffer: the file line itself when the
/// element windows the file, the patch row of that line when it windows the generated text.
pub fn seat_in(
    s: &ServerState,
    view_buffer: BufferId,
    elements: &[crate::state::ElementBinding],
    identity: &str,
    line: u32,
) -> Option<(aether_protocol::viewport::ViewSeat, LogicalPosition)> {
    let entries = view_outline_of(s, view_buffer, elements);
    let named = |e: &OutlineEntry| -> bool {
        let Some(own) = e.identity.as_deref() else {
            return false;
        };
        // A derived path is the repo's canonical workdir plus the file's path, which a symlink
        // inside the repo could still separate from the canonical path a buffer carries.
        own == identity || std::fs::canonicalize(own).is_ok_and(|p| p.to_string_lossy() == identity)
    };
    let hit = entries
        .iter()
        .find(|e| named(e) && e.file_lines.start == line)
        .or_else(|| {
            entries
                .iter()
                .find(|e| named(e) && e.file_lines.contains(&line))
        })?;
    let buffer_id = elements.get(hit.element as usize)?.buffer_id;
    let position = if buffer_id != view_buffer {
        LogicalPosition { line, col: 0 }
    } else {
        // The element windows the generated text: the same file line, as the patch numbers it.
        let index = &s.doc_of(view_buffer).generated.as_ref()?.index;
        let row = hit
            .patch_lines
            .clone()
            .find(|&i| {
                index
                    .lines
                    .get(i as usize)
                    .copied()
                    .flatten()
                    .and_then(|info| info.new_lineno)
                    == Some(line + 1)
            })
            .unwrap_or(hit.patch_line);
        LogicalPosition { line: row, col: 0 }
    };
    Some((
        aether_protocol::viewport::ViewSeat {
            element: hit.element,
            buffer_id,
        },
        position,
    ))
}

/// How a buffer is named **durably**: its canonical path, or — for a materialised one — its
/// [`crate::state::VirtualSource::key`]. `None` only for a scratch, which has neither.
///
/// One function because a view's elements window both kinds and a jumplist entry must be able to
/// name either. The working changes bind their elements to working-tree files (paths); a commit's
/// patch binds them to that file *at that revision*, which is a virtual buffer with no path at all
/// — so keying on the path alone left every commit-patch row unable to find its element.
pub fn buffer_identity(s: &ServerState, buffer_id: BufferId) -> Option<String> {
    let doc = s.try_doc_of(buffer_id)?;
    if let Some(path) = doc.canonical_path.as_ref() {
        return Some(path.to_string_lossy().into_owned());
    }
    doc.virtual_source.as_ref().map(|src| src.target.key())
}

/// Where `(abs_path, line)` sits in a **freshly materialised** view — one no viewport exists for
/// yet, because the jump had to reopen it.
///
/// The elements are the view's own if it has been presented, else what presenting it will build,
/// so the element named here is the one the client's own subscribe will produce.
pub fn seat_in_fresh_view(
    s: &ServerState,
    view_buffer: BufferId,
    identity: &str,
    line: u32,
) -> Option<(aether_protocol::viewport::ViewSeat, LogicalPosition)> {
    let elements = s.view_elements(ViewId(view_buffer));
    seat_in(s, view_buffer, &elements, identity, line)
}

/// A composed view's outline: its changes, in reading order, each with its file and label.
///
/// **The one source.** `Space o` lists it, `o`/`Alt-o` steps it, and the status bar's breadcrumb is
/// the path through it to the cursor — so the three cannot disagree about what the stops are or what
/// they are called. They previously each asked something different: the picker asked the focused
/// file's language server, the motion asked the same, and the breadcrumb composed a file label with
/// an LSP symbol path.
///
/// Returns empty for an ordinary view, which has no outline of this kind — its outline is the
/// document symbols of the one buffer it shows, and that is answered elsewhere.
pub fn view_outline(s: &ServerState, vp: &Viewport) -> Vec<OutlineEntry> {
    view_outline_of(s, vp.view_id.presenting_buffer(), &s.view_of(vp).elements)
}

/// [`view_outline`] for a view **nobody is subscribed to yet** — the state a jumplist entry finds
/// its view in when it has to reopen it before it can land.
///
/// Takes the element bindings rather than a viewport because a `FieldId` indexes the element list,
/// which comes from the document's own layout: `cols` changes how those elements *wrap*, never how
/// many there are or what they window. So an element resolved here is the same element the client's
/// own subscribe will produce.
pub fn view_outline_of(
    s: &ServerState,
    view_buffer: BufferId,
    elements: &[crate::state::ElementBinding],
) -> Vec<OutlineEntry> {
    let doc = s.doc_of(view_buffer);
    let Some(generated) = doc.generated.as_ref() else {
        return Vec::new();
    };
    // How a file the view shows is named when nothing windows it: the same name the buffer a bound
    // element windows would have, derived from the view's own target rather than read off a buffer
    // that may not exist. A file at a revision is a virtual buffer keyed by repo, revision and path;
    // a working-tree file is its path under the repo's canonical workdir.
    let source = doc.virtual_source.as_ref();
    let derived_identity = |path: &str| -> Option<String> {
        use aether_protocol::git::ShowTarget;
        let target = &source?.target;
        Some(match &target.what {
            ShowTarget::WorkingChanges => std::path::Path::new(&target.repo_id)
                .join(path)
                .to_string_lossy()
                .into_owned(),
            ShowTarget::Commit { rev } => crate::state::VirtualTarget::new(
                target.repo_id.clone(),
                ShowTarget::File {
                    rev: rev.clone(),
                    path: path.to_string(),
                },
            )
            .key(),
            ShowTarget::File { .. } => return None,
        })
    };
    // Which element a *patch* line falls in. Read from the regions the patch was rendered into —
    // `decorations.elements` is parallel to the elements a driver built from them — because an
    // element's own `start_line` is a **file** line once it is bound, and comparing that with a
    // patch line is the mistake this module exists to make hard.
    let region_of = |patch_line: u32| -> aether_protocol::viewport::FieldId {
        generated
            .decorations
            .elements
            .iter()
            .rposition(|r| r.start_line <= patch_line)
            .unwrap_or(0) as aether_protocol::viewport::FieldId
    };
    // One entry per **hunk**, not per change block. A hunk is the display region git names in its
    // header; a change block is a maximal run of `+`/`-` lines, and one hunk routinely holds several
    // (see `PatchChangeBlock`). Outlining the blocks gave several rows per hunk all carrying the
    // *same* signature — the hunk's — which is a list that cannot be read.
    //
    // It is also what separates `o` from `c`: `o` steps the structure (hunks), `c` steps the things
    // you act on (changes). Different keys because they are different questions.
    let mut out = Vec::new();
    for file in &generated.index.files {
        // A delta with no hunks — a binary swap, a bare mode change, a deletion with nothing to
        // show — still gets a row, over its placeholder line. Dropping it would leave the outline
        // disagreeing with the view about what is in the review.
        // Each span with the hunk's lines in the **file** (libgit2 counts from 1; the `- 1` is the
        // one conversion), which is the same range a bound element's extent is built from — so the
        // two cannot disagree about where a hunk's file lines start.
        let spans: Vec<(u32, u32, &str, std::ops::Range<u32>)> = if file.hunks.is_empty() {
            vec![(file.start_line, file.end_line, "", 0..0)]
        } else {
            file.hunks
                .iter()
                .map(|h| {
                    let first = h.new_start.saturating_sub(1);
                    (
                        h.start_line,
                        h.end_line,
                        h.signature.as_str(),
                        first..first + h.new_lines,
                    )
                })
                .collect()
        };
        for (start, end, signature, file_lines) in spans {
            // The hunk's **top**, context included — not its first change.
            //
            // `o` and the picker land in the same place because they read this one number, and the
            // top is the right one: it is where the hunk's heading is, so you arrive at the change
            // *with the context that makes it readable* rather than partway into it. It also makes
            // "which entry is the cursor in" answerable for the context lines themselves, which
            // otherwise resolved to the hunk above. `c`/`Alt-c` still land on the changes.
            let anchor = start;
            let element = region_of(anchor);
            let binding = elements
                .get(element as usize)
                .filter(|e| e.buffer_id != view_buffer);
            // A bound element windows the real file, so the entry's line must be a *file* line: the
            // hunk's first new-side line, or for a pure removal the line it sits above — which is
            // also where the element's own extent starts. An unbound one windows the generated text
            // itself, where the patch line already is the buffer line.
            let line = if binding.is_some() {
                file_lines.start
            } else {
                anchor
            };
            let identity = if file.hunks.is_empty() {
                None
            } else {
                binding
                    .and_then(|b| buffer_identity(s, b.buffer_id))
                    .or_else(|| derived_identity(file.path()))
            };
            out.push(OutlineEntry {
                element,
                line,
                file: file.path().to_string(),
                label: signature.to_string(),
                patch_line: anchor,
                patch_lines: anchor..end.max(anchor + 1),
                identity,
                file_lines,
            });
        }
    }
    out
}

/// The breadcrumb for a **composed** view: the path through its outline to the cursor.
///
/// `file.rs › fn outer` — the file the cursor is in, then the label of the change it is inside, both
/// read from [`view_outline`]. `None` for an ordinary view, which has no outline of this kind and
/// whose breadcrumb is its document symbols.
///
/// The third consumer of the one source, and the reason it exists: `Space o` lists these entries,
/// `o`/`Alt-o` steps them, and this names the one you are in. Composed from a *file* crumb plus the
/// entry's own label rather than the language server's chain, so the three cannot describe the same
/// position in different words.
/// **Where the cursor is in the outline**: the entry it sits in, and that entry's index.
///
/// The one question both the breadcrumb and the picker's opening selection ask, so they ask it once.
/// Answered against the *focused element* first: entries of other elements are other files, and
/// being "past" one of those says nothing about where the cursor is.
pub fn outline_entry_at(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Option<(usize, OutlineEntry)> {
    let vp = s
        .viewports
        .values()
        .find(|v| v.client_id == client_id && s.view_of(v).binds(buffer_id))?;
    let entries = view_outline(s, vp);
    if entries.is_empty() {
        return None;
    }
    let focused = vp.focused;
    let line = s
        .cursors
        .get(&(client_id, buffer_id))
        .map_or(0, |c| c.position.line);
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.element == focused && e.line <= line)
        .next_back()
        // Before the element's first change — still in its file, which is the answer that matters.
        .or_else(|| {
            entries
                .iter()
                .enumerate()
                .find(|(_, e)| e.element == focused)
        })
        .map(|(i, e)| (i, e.clone()))
}

pub fn outline_breadcrumb(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Option<Vec<aether_protocol::lsp::SymbolCrumb>> {
    use aether_protocol::lsp::SymbolCrumb;
    use aether_protocol::picker::SymbolKind;

    let (_, here) = outline_entry_at(s, client_id, buffer_id)?;
    let mut path = vec![SymbolCrumb {
        name: here.file.clone(),
        kind: SymbolKind::File,
    }];
    if !here.label.is_empty() {
        path.push(SymbolCrumb {
            name: here.label.clone(),
            kind: SymbolKind::Function,
        });
    }
    Some(path)
}

/// `viewport/navigate_change`: step between a composed view's changes, crossing elements — and so
/// buffers — as needed.
pub async fn viewport_navigate_change(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::viewport::ViewportNavigateChangeParams,
) -> Result<aether_protocol::viewport::ViewportFocusElementResult, RpcError> {
    use aether_protocol::viewport::{FocusStep, NavigateGrain};

    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    // Check ownership up front, then read: the anchors need the state alongside the viewport.
    require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    let vp = s.viewports[&params.viewport_id].clone();
    let (focused, here) = (vp.focused, s.focused_buffer(&vp));
    let forward = params.direction == FocusStep::Next;
    let anchors = match params.grain {
        NavigateGrain::Change => change_anchors(&s, &vp),
        // One stop per **outline entry** — which is one per change, since that is what the outline's
        // rows are. Same source as the picker, so `o` and `Space o` cannot disagree about the stops
        // or their order.
        NavigateGrain::Outline => {
            let outline = view_outline(&s, &vp);
            if outline.is_empty() {
                // A view with no outline of its own — an ordinary buffer — has the structure of the
                // buffer it shows: its document symbols, stepped exactly as `o` always stepped them.
                // The target's identifier lands selected, Shift grows the selection to it, and a
                // count the outline cannot honour refuses.
                let (cursor, update) = step_navigation_unit(
                    &mut s,
                    client_id,
                    here,
                    forward,
                    params.count.unwrap_or(1).max(1),
                    params.extend,
                )?;
                let result = aether_protocol::viewport::ViewportFocusElementResult {
                    element: focused,
                    buffer: crate::handlers::describe_buffer(&s, here, cursor)?,
                    buffer_status: buffer_status_for(&mut s, client_id, here),
                };
                drop(s);
                if let Some((sender, notif)) = update {
                    let _ = sender.send(notif).await;
                }
                return Ok(result);
            }
            outline.into_iter().map(|e| (e.element, e.line)).collect()
        }
    };
    let from = s
        .cursors
        .get(&(client_id, here))
        .map(|c| c.position.line)
        .unwrap_or(0);

    // Strictly past the cursor in either direction, so landing on a change and pressing again
    // moves off it rather than finding itself. A count steps that many changes, not that many
    // lines.
    //
    // A count that the view cannot honour **refuses**, exactly as `c`/`Alt-c` over an ordinary
    // buffer already did (`git_navigate_hunk` takes the `n`th or answers `moved: false`). These are
    // the same two keys, and clamping here gave them two different meanings depending on which kind
    // of view they were pressed in: `5c` with three changes left jumped to the last one in a patch
    // and did nothing in a file. The count names *which* change; there isn't a fifth.
    let count = params.count.unwrap_or(1).max(1) as usize;
    let last = anchors.len().saturating_sub(1);
    let target = if forward {
        anchors
            .iter()
            .position(|&a| a > (focused, from))
            .and_then(|i| (i + count - 1 <= last).then_some(i + count - 1))
    } else {
        anchors
            .iter()
            .rposition(|&a| a < (focused, from))
            .and_then(|i| i.checked_sub(count - 1))
    };
    let Some(&(element, line)) = target.and_then(|t| anchors.get(t)) else {
        // Nothing to step to: report where we are rather than erroring, so a held key is quiet.
        let cursor = s
            .cursors
            .get(&(client_id, here))
            .copied()
            .unwrap_or_default();
        return Ok(aether_protocol::viewport::ViewportFocusElementResult {
            element: focused,
            buffer: crate::handlers::describe_buffer(&s, here, cursor)?,
            buffer_status: buffer_status_for(&mut s, client_id, here),
        });
    };

    require_viewport_mut(&mut s, params.viewport_id, client_id)?.focused = element;
    let buffer_id = s.focused_buffer(&s.viewports[&params.viewport_id]);
    let key = (client_id, buffer_id);
    let current = s.cursors.get(&key).copied().unwrap_or_default();
    let cursor = {
        let doc = s.doc_of(buffer_id);
        let position =
            motion::clamp_position(doc, aether_protocol::LogicalPosition { line, col: 0 });
        CursorState {
            position,
            // Shift grows the selection to the change — within the element, since a selection
            // cannot span buffers; a step into another element lands as a point there.
            anchor: if params.extend && element == focused {
                current.anchor
            } else {
                position
            },
            match_bracket: None,
            jumplist_position: None,
        }
    };
    // Landed like any motion: recorded for motion undo, the virtual column and tree-selection
    // history dropped — what `c` over one file always did, so the two paths cannot drift.
    let (cursor, _) = commit_move(&mut s, client_id, buffer_id, current, cursor, None);
    let pushes = rescope_search(&mut s, client_id, buffer_id);
    let result = aether_protocol::viewport::ViewportFocusElementResult {
        element,
        buffer: crate::handlers::describe_buffer(&s, buffer_id, cursor)?,
        buffer_status: buffer_status_for(&mut s, client_id, buffer_id),
    };
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }
    Ok(result)
}

/// `view/window`: load the slices the client's viewport reaches, replacing whatever was loaded.
///
/// Each slice is named by element and row within it; the server maps the row to a line through
/// its own wrapping (the one fact the client lacks) and renders from there. An element the client
/// names that the view no longer has — it rebuilt underneath — is a stale id, not an error, and is
/// simply not loaded.
pub async fn viewport_window(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::viewport::ViewportWindowParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    let loaded = {
        let vp = &s.viewports[&params.viewport_id];
        let view = s.view_of(vp);
        let layout = s.layout_of(&view.elements);
        let geom = vp.wrap_geometry();
        let mut loaded: Vec<Option<std::ops::Range<u32>>> = vec![None; view.elements.len()];
        for req in &params.slices {
            let Some(binding) = view.elements.get(req.element as usize) else {
                continue;
            };
            let range = layout.element_range(req.element);
            let (geom, phantoms) = element_geometry(&s, binding, geom, vp.diff_view);
            let slice = whole_if_client_laid_out(binding, range, || {
                slice_from(
                    s.doc_of(binding.buffer_id),
                    geom,
                    &phantoms,
                    range,
                    req.from_row,
                    req.rows,
                )
            });
            if !slice.is_empty() {
                loaded[req.element as usize] = Some(slice);
            }
        }
        loaded
    };
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.loaded = loaded;
    vp.anchor = params.anchor;
    let view_id = vp.view_id;
    // Where the client is, as content, so a reopen of this view restores it.
    s.last_scroll.insert((client_id, view_id), params.anchor);
    let window = render_viewport(&s, params.viewport_id, SneakLabels::Shown);
    Ok(ViewportWindowResult { window })
}

pub async fn viewport_set_wrap(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportSetWrapParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.wrap = params.wrap;
    let window = render_viewport(&s, params.viewport_id, SneakLabels::Shown);
    Ok(ViewportWindowResult { window })
}

/// `view/window_at_cursor`: a window containing this client's cursor in the focused element.
///
/// The answer to a question the client cannot phrase itself — see
/// [`aether_protocol::viewport::ViewportWindowAtCursor`]. Both halves live here: which line the
/// cursor is on, and which row of its element that line starts at.
///
/// The cursor lands a third of a screen down rather than at the top, so a reveal that follows has
/// context on both sides and does not have to scroll again. The client still positions itself
/// against the window it gets back; this only decides which slice to send. Only the focused
/// element's slice is loaded by it: the client asks for the neighbours its viewport reaches once
/// it has placed itself, exactly as it does after any scroll.
pub async fn viewport_window_at_cursor(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::viewport::ViewportWindowAtCursorParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    let (loaded, anchor) = {
        let vp = &s.viewports[&params.viewport_id];
        let view = s.view_of(vp);
        let layout = s.layout_of(&view.elements);
        let geom = vp.wrap_geometry();
        let element = vp
            .focused
            .min(view.elements.len().saturating_sub(1) as aether_protocol::viewport::FieldId);
        let binding = &view.elements[element as usize];
        let range = layout.element_range(element);
        let cursor = s
            .cursors
            .get(&(client_id, binding.buffer_id))
            .copied()
            .unwrap_or_default();
        let (geom, phantoms) = element_geometry(&s, binding, geom, vp.diff_view);
        let doc = s.doc_of(binding.buffer_id);
        // Clamped into the element: its extent is a claim about the buffer that a rebuild between
        // the ask and the answer could have outrun.
        let line = cursor.position.line.clamp(
            range.start(),
            range.end_exclusive().saturating_sub(1).max(range.start()),
        );
        let from_row = element_rows_before(doc, geom, &phantoms, range, line)
            .saturating_sub(vp.rows / 3 + vp.overscan_rows);
        let slice = whole_if_client_laid_out(binding, range, || {
            slice_from(
                doc,
                geom,
                &phantoms,
                range,
                from_row,
                vp.rows + 2 * vp.overscan_rows,
            )
        });
        let mut loaded = vec![None; view.elements.len()];
        let anchor = ScrollPosition {
            element,
            line: slice.start,
            sub_row: 0.0,
        };
        if !slice.is_empty() {
            loaded[element as usize] = Some(slice);
        }
        (loaded, anchor)
    };
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.loaded = loaded;
    vp.anchor = anchor;
    let view_id = vp.view_id;
    s.last_scroll.insert((client_id, view_id), anchor);
    let window = render_viewport(&s, params.viewport_id, SneakLabels::Shown);
    Ok(ViewportWindowResult { window })
}

// ---- helpers -----------------------------------------------------------------------------------

pub fn require_viewport_mut(
    state: &mut ServerState,
    viewport_id: aether_protocol::ViewportId,
    client_id: ClientId,
) -> Result<&mut Viewport, RpcError> {
    let vp = state.viewports.get_mut(&viewport_id).ok_or_else(|| {
        RpcError::new(
            ErrorCode::VIEWPORT_NOT_FOUND,
            format!("unknown viewport_id: {viewport_id}"),
        )
    })?;
    if vp.client_id != client_id {
        return Err(RpcError::new(
            ErrorCode::VIEWPORT_NOT_FOUND,
            "viewport is not owned by this client",
        ));
    }
    Ok(vp)
}

/// Render `viewport_id`'s window: the whole tree, with the slices the viewport has loaded.
///
/// The one tail every geometry handler shares — resize, wrap, the diff toggle, a window request.
/// It used to be nine values unpacked from the viewport by hand at each of them and fed to the
/// renderer positionally, any one of which could name another element's width or the wrong buffer;
/// now a handler changes what it changes on the viewport and asks for the frame.
pub fn render_viewport(
    s: &ServerState,
    viewport_id: aether_protocol::ViewportId,
    sneak_labels: SneakLabels,
) -> Window {
    render_window(s, &s.viewports[&viewport_id], sneak_labels)
}

/// Consume the edit's line shift into every view's extents and every viewport's loaded slices,
/// then re-diff. Call **before** building `viewport/lines_changed` notifications after any mutation
/// that may grow or shrink the buffer — otherwise a growth (e.g. undoing a join) leaves a loaded
/// slice one line short and the freshly restored line never reaches the client.
pub fn refresh_viewport_ranges_for_buffer(s: &mut ServerState, buffer_id: BufferId) {
    // A view's elements window *slices* of their buffers, and so do the viewports' loaded slices;
    // an edit that changed the line count moved both. Consumed here, once, rather than threaded
    // through the dozen paths that reach this function. Undo, redo and reload replace the rope
    // wholesale and record no shift; the layout's clamp against the live buffer covers those.
    if let Some(shift) = s
        .try_doc_of_mut(buffer_id)
        .and_then(|d| d.last_shift.take())
    {
        s.shift_element_extents(buffer_id, shift);
    }
    reseat_orphaned_slices(s, buffer_id);
    // Every buffer of the document: a mutation through one workspace's buffer moves the shared
    // content under every sibling's viewports too.
    for id in s.doc_siblings(buffer_id) {
        recompute_diff_hunks_if_viewed(s, id);
    }
}

/// A loaded slice that no longer overlaps its element at all is moved onto the element's tail.
///
/// A shift keeps a slice on the text it held through an edit; a wholesale replacement — reload,
/// undo of a large paste, a formatter — records none, and a slice loaded deep in a file that just
/// lost most of its lines then names lines the element no longer has. The clip in `render_window`
/// keeps that from indexing past the rope, but a push carrying no lines blanks the client until
/// its next fetch. Reseating the slice where the client's viewport will land — the element's
/// last screen, since its scroll clamps to the new height — makes the push itself the answer.
fn reseat_orphaned_slices(s: &mut ServerState, buffer_id: BufferId) {
    let siblings = s.doc_siblings(buffer_id);
    let ids: Vec<_> = s.viewports.keys().copied().collect();
    for id in ids {
        let vp = &s.viewports[&id];
        let elements = &s.view_of(vp).elements;
        let layout = s.layout_of(elements);
        let want = vp.rows + 2 * vp.overscan_rows;
        let reseat: Vec<(usize, std::ops::Range<u32>)> = elements
            .iter()
            .zip(vp.loaded.iter())
            .enumerate()
            .filter_map(|(idx, (binding, slice))| {
                let slice = slice.as_ref()?;
                if !siblings.contains(&binding.buffer_id) {
                    return None;
                }
                let range = layout.element_range(idx as aether_protocol::viewport::FieldId);
                if range.intersect(slice.clone()).is_some() || range.is_empty() {
                    return None;
                }
                let end = range.end_exclusive();
                let last_screen = end.saturating_sub(want).max(range.start())..end;
                Some((
                    idx,
                    whole_if_client_laid_out(binding, range, || last_screen),
                ))
            })
            .collect();
        let vp = s.viewports.get_mut(&id).expect("listed viewport");
        for (idx, slice) in reseat {
            vp.loaded[idx] = Some(slice);
        }
    }
}

/// Recompute the buffer's diff hunks after a mutation, when any client is viewing it. The gutter
/// change-bar is always on, so any open viewport needs fresh hunks — not just ones with the
/// inline diff view enabled. Called from the post-mutation refresh so every edit path is covered
/// in one place.
///
/// Cheap: it diffs the **cached** baseline against the buffer — no repository discovery or blob
/// read on the keystroke path (those happen in `load_baseline`, on open and on Git changes). It
/// is still a whole-file in-memory diff per edit; debouncing is the next optimisation if that
/// ever bites on very large files.
pub fn recompute_diff_hunks_if_viewed(s: &mut ServerState, buffer_id: BufferId) {
    let viewed = s
        .viewports
        .values()
        .any(|vp| s.view_of(vp).binds(buffer_id));
    if !viewed {
        return;
    }
    // Re-diff against the cached index blob — a per-edit in-memory diff with no repo I/O. The
    // combined view recomposes from the cached staged hunks (they only change on git events, not
    // buffer edits) + the fresh unstaged. The conflict rescan rides along: the edit may have been
    // the user resolving a block, and the decoration has to follow it away.
    recompute_git_hunks(s, buffer_id);
}

/// The phantom "deleted" rows each anchor line shows above it, derived from the buffer's diff
/// hunks. Only hunks with removed text (Modified / Deleted) contribute; pure additions have none.
/// A deletion past the last line is clamped onto the final line index, so a newline-terminated
/// file shows it above its trailing empty line. Each row carries its hunk's `DiffStage`; where a
/// staged and an unstaged layer would stack at one anchor (a region modified, staged, then
/// modified again), only the unstaged rows are kept — what's shown deleted is exactly what a
/// revert would restore, and HEAD's text resurfaces once the top layer is staged or reverted.
fn deleted_rows_by_anchor(
    hunks: &[crate::git::DiffHunk],
    line_count: u32,
    intraline: Option<&IntralineEmphasis>,
) -> HashMap<u32, Vec<BaselineRow>> {
    let mut map: HashMap<u32, Vec<BaselineRow>> = HashMap::new();
    let last_line = line_count.saturating_sub(1);
    for (hunk_idx, h) in hunks.iter().enumerate() {
        if h.deleted.is_empty() {
            continue;
        }
        let anchor = h.anchor_line.min(last_line);
        let rows = map.entry(anchor).or_default();
        rows.extend(h.deleted.iter().enumerate().map(|(row_idx, text)| {
            // A deleted row's colour is entirely the diff palette's: it has no grammar of its own
            // here (the baseline text isn't parsed), so there is nothing to span.
            BaselineRow {
                text: text.clone(),
                stage: h.stage,
                emphasis: intraline
                    .and_then(|m| m.rows.get(&(hunk_idx, row_idx)))
                    .cloned()
                    .unwrap_or_default(),
            }
        }));
    }
    for rows in map.values_mut() {
        let unstaged = |r: &BaselineRow| r.stage == DiffStage::Unstaged;
        if rows.iter().any(unstaged) {
            rows.retain(unstaged);
        }
    }
    map
}

/// Intra-line diff emphasis for the window being rendered: for each old/new line pair of a
/// Modified hunk, the sub-line byte ranges that actually changed (see
/// [`crate::git::intraline_emphasis`]). Computed only while the diff view is on, and only for
/// pairs that can appear in the window — the phantom rows all sit at the hunk's anchor, the new
/// side on `anchor + i` — so cost scales with what's on screen, not with the diff.
#[derive(Default)]
struct IntralineEmphasis {
    /// Old-side ranges, keyed by (index into the hunk slice, index into that hunk's `deleted`).
    rows: HashMap<(usize, usize), Vec<EmphasisRange>>,
    /// New-side ranges, keyed by buffer line.
    lines: HashMap<u32, Vec<EmphasisRange>>,
}

fn intraline_for_window(
    hunks: &[crate::git::DiffHunk],
    buf: &Document,
    first: u32,
    last_excl: u32,
) -> IntralineEmphasis {
    use crate::git::ChangeKind;
    let mut out = IntralineEmphasis::default();
    // Where a staged and an unstaged hunk pair up the same buffer line, the unstaged layer wins
    // (matching `diff_markers_by_line`): the line's tint is unstaged, so its emphasis is too.
    let mut line_stage: HashMap<u32, DiffStage> = HashMap::new();
    for (hunk_idx, h) in hunks.iter().enumerate() {
        if !matches!(h.kind, ChangeKind::Modified) {
            continue;
        }
        let anchor_in_window = h.anchor_line >= first && h.anchor_line < last_excl;
        let pairs = (h.deleted.len() as u32).min(h.new_lines);
        for i in 0..pairs {
            let line = h.anchor_line + i;
            let line_in_window = line >= first && line < last_excl;
            // The pair matters if its new side is in the window, or its phantom row is (all of a
            // hunk's phantom rows render at the anchor).
            if (!line_in_window && !anchor_in_window) || line >= buf.line_count() {
                continue;
            }
            let old = &h.deleted[i as usize];
            let mut new: String = buf.text.line(line as usize).chunks().collect();
            if new.ends_with('\n') {
                new.pop();
            }
            // A bail (`None`: rewritten / overlong pair) renders the same as an identical pair —
            // no emphasis — so both fold to empty spans here.
            let (old_spans, new_spans) =
                crate::git::intraline_emphasis(old, &new).unwrap_or_default();
            let to_ranges = |spans: crate::git::EmphasisSpans| -> Vec<EmphasisRange> {
                spans
                    .into_iter()
                    .map(|(start, end)| EmphasisRange { start, end })
                    .collect()
            };
            if !old_spans.is_empty() {
                out.rows
                    .insert((hunk_idx, i as usize), to_ranges(old_spans));
            }
            if line_in_window {
                let unstaged_present = line_stage.get(&line) == Some(&DiffStage::Unstaged);
                if !(unstaged_present && h.stage == DiffStage::Staged) {
                    if new_spans.is_empty() {
                        out.lines.remove(&line);
                    } else {
                        out.lines.insert(line, to_ranges(new_spans));
                    }
                    line_stage.insert(line, h.stage);
                }
            }
        }
    }
    out
}

/// The Git change marker (and its stage) for each affected buffer line, for the gutter
/// change-bar. Added/modified hunks mark their new-side lines `Added`/`Modified`; a pure deletion
/// marks the single line it sits above as `Deleted` (clamped onto the last line for an
/// end-of-buffer deletion), without overriding an Added/Modified marker that's already there.
/// Where a staged and an unstaged hunk cover the same line (modified, staged, modified again),
/// the unstaged layer wins both kind and stage — the line reads as plain unstaged, since that's
/// the content on screen and the layer `git/apply_hunk` acts on.
fn diff_markers_by_line(
    hunks: &[crate::git::DiffHunk],
    line_count: u32,
) -> HashMap<u32, (DiffMarker, DiffStage)> {
    use crate::git::ChangeKind;
    let last_line = line_count.saturating_sub(1);
    let mut map: HashMap<u32, (DiffMarker, DiffStage)> = HashMap::new();
    let put = |map: &mut HashMap<u32, (DiffMarker, DiffStage)>,
               line: u32,
               marker: DiffMarker,
               stage: DiffStage| {
        map.entry(line)
            .and_modify(|(k, s)| {
                // A staged hunk never overrides an unstaged marker (the top layer wins)...
                if *s == DiffStage::Unstaged && stage == DiffStage::Staged {
                    return;
                }
                //...while Added/Modified outrank a Deleted-above flag (which never downgrades
                // them), and an unstaged write takes the stage with it.
                if marker != DiffMarker::Deleted {
                    *k = marker;
                }
                *s = stage;
            })
            .or_insert((marker, stage));
    };
    for h in hunks {
        match h.kind {
            ChangeKind::Added | ChangeKind::Modified => {
                let marker = if matches!(h.kind, ChangeKind::Added) {
                    DiffMarker::Added
                } else {
                    DiffMarker::Modified
                };
                for line in h.anchor_line..h.anchor_line.saturating_add(h.new_lines) {
                    put(&mut map, line, marker, h.stage);
                }
            }
            ChangeKind::Deleted => {
                put(
                    &mut map,
                    h.anchor_line.min(last_line),
                    DiffMarker::Deleted,
                    h.stage,
                );
            }
        }
    }
    map
}

/// Which part of a conflict each line inside one belongs to, for the window renderer.
///
/// The marker lines are *derived* rather than stored: inside a region, anything that isn't one of
/// the three content ranges is by definition one of the four markers. That keeps
/// [`crate::git::ConflictRegion`] describing the block's structure and nothing else, and there is
/// no fourth line number to keep consistent with the other three.
fn conflict_lines_by_line(
    regions: &[crate::git::ConflictRegion],
    line_count: u32,
) -> HashMap<u32, ConflictLine> {
    let mut map = HashMap::new();
    for r in regions {
        for line in r.start_line..=r.end_line.min(line_count.saturating_sub(1)) {
            let kind = if r.ours.contains(&line) {
                ConflictLine::Ours
            } else if r.base.as_ref().is_some_and(|b| b.contains(&line)) {
                ConflictLine::Base
            } else if r.theirs.contains(&line) {
                ConflictLine::Theirs
            } else {
                ConflictLine::Marker
            };
            map.insert(line, kind);
        }
    }
    map
}

/// The buffer-wide change summary for the status bar: line counts by change class. `added` /
/// `modified` count the new-side lines of Added / Modified hunks (matching the gutter bars);
/// `deleted` counts the lines a pure deletion removed. A Modified hunk's replaced old-side lines
/// are represented by its `modified` count, not counted again as deletions.
fn git_change_counts(hunks: &[crate::git::DiffHunk]) -> GitChangeCounts {
    use crate::git::ChangeKind;
    let mut c = GitChangeCounts::default();
    for h in hunks {
        match h.kind {
            ChangeKind::Added => c.added += h.new_lines,
            ChangeKind::Modified => c.modified += h.new_lines,
            ChangeKind::Deleted => c.deleted += h.deleted.len() as u32,
        }
    }
    c
}

/// The buffer's *unstaged* diff hunks (vs the index), or an empty slice when none are cached.
fn buffer_unstaged_hunks(s: &ServerState, buffer_id: BufferId) -> &[crate::git::DiffHunk] {
    s.git_unstaged_hunks
        .get(&buffer_id)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The combined staged+unstaged hunks that drive the gutter / inline diff (each tagged with its
/// `DiffStage`), or an empty slice when none are cached (no repo / untracked / clean).
pub fn buffer_both_hunks(s: &ServerState, buffer_id: BufferId) -> &[crate::git::DiffHunk] {
    s.git_both_hunks
        .get(&buffer_id)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The buffer's conflict blocks, or an empty slice when the file isn't conflicted — which is every
/// file, nearly always.
pub fn buffer_conflicts(s: &ServerState, buffer_id: BufferId) -> &[crate::git::ConflictRegion] {
    s.git_conflicts
        .get(&buffer_id)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Hide the parts of a freshly-computed diff that sit inside the buffer's conflict blocks.
///
/// Call after [`recompute_conflicts`], since it reads the cache that fills. A no-op — and a cheap
/// one — for every file that isn't conflicted.
pub fn mask_hunks_against_conflicts(
    s: &ServerState,
    buffer_id: BufferId,
    unstaged: Vec<crate::git::DiffHunk>,
    both: Vec<crate::git::DiffHunk>,
) -> (Vec<crate::git::DiffHunk>, Vec<crate::git::DiffHunk>) {
    let regions = buffer_conflicts(s, buffer_id);
    (
        crate::git::mask_conflicts(unstaged, regions),
        crate::git::mask_conflicts(both, regions),
    )
}

/// Recompute a buffer's diff hunks from its cached baseline, with the conflict blocks masked out.
/// The shared body of every "the buffer or its baseline changed" path.
pub fn recompute_git_hunks(s: &mut ServerState, buffer_id: BufferId) {
    recompute_conflicts(s, buffer_id);
    let Some(baseline) = s.git_baseline.get(&buffer_id) else {
        return;
    };
    let Some(doc) = s.try_doc_of(buffer_id) else {
        return;
    };
    // Pending: no gutter yet, rather than a gutter claiming the whole file is new. The deferred
    // load comes back through here the moment it lands.
    let Some(effective) = crate::git::effective_baseline(baseline, doc.disk_blob.as_deref()) else {
        s.git_unstaged_hunks.remove(&buffer_id);
        s.git_both_hunks.remove(&buffer_id);
        return;
    };
    let unstaged = crate::git::diff_hunks(effective.blob, &doc.text);
    let both = crate::git::compose_both(effective.staged, &unstaged);
    let (unstaged, both) = mask_hunks_against_conflicts(s, buffer_id, unstaged, both);
    s.git_unstaged_hunks.insert(buffer_id, unstaged);
    s.git_both_hunks.insert(buffer_id, both);
}

/// Rescan a buffer for conflict markers, on the same triggers as the diff hunks.
///
/// Gated on the baseline's `conflicted` flag — the index's word for it — so an ordinary edit never
/// pays for the scan, and the cache empties the moment a file stops being conflicted (someone
/// staged the resolution, here or in a terminal, and the watcher reloaded the baseline).
pub fn recompute_conflicts(s: &mut ServerState, buffer_id: BufferId) {
    if !s
        .git_baseline
        .get(&buffer_id)
        .and_then(|b| b.content())
        .is_some_and(|c| c.conflicted)
    {
        s.git_conflicts.remove(&buffer_id);
        return;
    }
    let Some(doc) = s.try_doc_of(buffer_id) else {
        return;
    };
    let regions = crate::git::conflict_regions(&doc.text);
    s.git_conflicts.insert(buffer_id, regions);
}

/// Whether a baseline-picker row names the baseline currently in force.
///
/// Revisions compare by **label**, not by resolved commit: the row offers `main`, and what the user
/// wants marked is the row they would have picked. Two labels resolving to the same commit (`main`
/// and `HEAD` on a clean checkout) are still two different standing instructions, and marking both
/// would say the picker had two current rows.
pub fn baseline_row_is_current(
    row: &crate::git::BaselineRow,
    current: Option<&GitBaselineSource>,
) -> bool {
    match (&row.choice, current) {
        (None, None) => true,
        (Some(GitBaselineChoice::Saved), Some(GitBaselineSource::Saved)) => true,
        (Some(GitBaselineChoice::Rev { rev }), Some(GitBaselineSource::Rev { label, .. })) => {
            rev == label
        }
        _ => false,
    }
}

/// Buffer-level Git status for the status bar: branch + staged (HEAD→index) and unstaged
/// (index→buffer) change counts. `Some` for any file inside a repo; `None` otherwise. Staged counts
/// come from the effective baseline's staged layer — empty under any pinned baseline, since
/// nothing but the index has an index relationship to report — and unstaged from the per-edit diff.
pub fn buffer_git_status(s: &ServerState, buffer_id: BufferId) -> Option<GitBufferStatus> {
    let Some(baseline) = s.git_baseline.get(&buffer_id) else {
        // No baseline, but possibly still *of* a repo: `git/show`'s patches and revision views
        // carry the repo-level cluster alone (see `ServerState::virtual_git_status`). Without this
        // the branch indicator — the editor's one sign that a repository is active at all —
        // vanishes the moment you open the very view that is showing you that repo's diff.
        return s.virtual_git_status.get(&buffer_id).cloned();
    };
    baseline.repo.as_ref()?; // only file-backed buffers inside a repo carry status
    let disk_blob = s.try_doc_of(buffer_id).and_then(|d| d.disk_blob.as_deref());
    // A baseline whose content is still loading reports its **branch** — that half was never
    // deferred — and no counts yet, which is what the buffer's own hunks say too. The counts fill
    // in with the gutter, on the same push.
    let effective = crate::git::effective_baseline(baseline, disk_blob);
    Some(GitBufferStatus {
        branch: baseline.branch.clone(),
        staged: git_change_counts(effective.as_ref().map_or(&[][..], |e| e.staged)),
        unstaged: git_change_counts(buffer_unstaged_hunks(s, buffer_id)),
        upstream: baseline.upstream.clone(),
        // Only a *pinned* baseline gets a status-bar token: the default needs no explaining.
        baseline: effective
            .is_some_and(|e| e.pinned)
            .then(|| baseline.choice.clone())
            .flatten(),
        conflicts: buffer_conflicts(s, buffer_id).len() as u32,
        operation: baseline.operation,
        worktree: baseline.worktree,
    })
}

/// Number of real visual rows for one logical line (1 under no-wrap, else the wrapped count).
/// Visual rows occupied by lines `start..end_excl` — one element's height.
///
/// Shipped on every [`Element::Editor`] so a client can lay out and scroll a view from the tree alone,
/// without a round trip per scroll. Phantom rows count: they occupy a row on screen, and a client
/// that summed only lines would place everything below them too high.
/// Takes a [`BufferRange`] rather than two `u32`s, so it cannot be handed lines the buffer does not
/// have. It used to re-clamp its own end, which read as belt-and-braces and was not: one caller
/// passed an element's raw extents from the diff, so a hunk whose file had since shrunk reported a
/// height measured over lines that were no longer there.
fn element_visual_rows(
    buf: &Document,
    range: BufferRange,
    geom: wrap::WrapGeometry,
    extra_rows: &HashMap<u32, u32>,
) -> u32 {
    let phantoms = extra_rows
        .iter()
        .filter(|(line, _)| range.lines().contains(line))
        .fold(0u32, |total, (_, n)| total.saturating_add(*n));
    match geom.wrap {
        aether_protocol::viewport::WrapMode::None => range.len().saturating_add(phantoms),
        aether_protocol::viewport::WrapMode::Soft => {
            // From the document's own table, not by wrapping the lines again: a render asks for
            // every element's height, so this is where a whole view's lines were being re-wrapped
            // on every scroll step.
            let rows = buf.wrapped_rows(geom);
            let lines = range.lines();
            rows[(lines.start as usize).min(rows.len())..(lines.end as usize).min(rows.len())]
                .iter()
                .fold(0u32, |total, n| total.saturating_add(*n))
                .saturating_add(phantoms)
        }
    }
}

/// The **buffer** line whose rows contain element row `target_row`, counting from the element's
/// first line — clamped to its last line for a row past its end.
///
/// Ranged rather than whole-buffer because an element windows a slice of its file: rows are counted
/// from the element's own first line, not the document's. A whole-buffer view passes the element's
/// whole range and gets exactly what it always did. The range is a [`BufferRange`] for the reason
/// [`element_visual_rows`]'s is.
pub fn line_at_element_row(
    buf: &Document,
    geom: wrap::WrapGeometry,
    extra_rows: &HashMap<u32, u32>,
    range: BufferRange,
    target_row: u32,
) -> u32 {
    let (from, to_excl) = (range.start(), range.end_exclusive());
    if range.is_empty() {
        return from;
    }
    let last = to_excl - 1;
    let no_wrap = matches!(geom.wrap, aether_protocol::viewport::WrapMode::None);
    if no_wrap && extra_rows.is_empty() {
        return from.saturating_add(target_row).min(last);
    }
    let rows = (!no_wrap).then(|| buf.wrapped_rows(geom));
    let mut acc = 0u32;
    for i in range.lines() {
        let virtual_n = extra_rows.get(&i).copied().unwrap_or(0);
        let wrapped = rows
            .as_ref()
            .map_or(1, |r| r.get(i as usize).copied().unwrap_or(1));
        let n = wrapped + virtual_n;
        if acc + n > target_row {
            return i;
        }
        acc += n;
    }
    last
}

/// Whether an edit to lines `first..last_excl` of any of `buffers` touches a slice `vp` has loaded.
///
/// Per element: the edit is in buffer lines, and so is each loaded slice, so the overlap is asked
/// of the elements windowing one of those buffers and nothing else. (It used to be asked of the
/// viewport's pushed range of *view* lines, which had to be mapped into first.)
///
/// **A set, not one id**: one document may be open as a buffer per workspace, and an edit through
/// any of them moves the text under all of them. Matching a single id silently stopped the *other*
/// workspace's viewport ever hearing about the change.
pub fn edit_touches_window(
    s: &ServerState,
    vp: &Viewport,
    buffers: &[BufferId],
    first: u32,
    last_excl: u32,
) -> bool {
    s.view_of(vp)
        .elements
        .iter()
        .zip(vp.loaded.iter())
        .any(|(binding, loaded)| {
            buffers.contains(&binding.buffer_id)
                && loaded
                    .as_ref()
                    .is_some_and(|slice| slice.start < last_excl && first < slice.end)
        })
}

/// The row within an element at which `line` starts: the rows of the element's lines above it,
/// wrapped rows and phantoms alike.
fn element_rows_before(
    doc: &Document,
    geom: wrap::WrapGeometry,
    extra_rows: &HashMap<u32, u32>,
    range: BufferRange,
    line: u32,
) -> ElementRow {
    ElementRow(element_visual_rows(
        doc,
        range.before(line),
        geom,
        extra_rows,
    ))
}

/// The lines of `range` that fill `rows` rows from element row `from_row`: the line holding that
/// row, and as many after it as the rows take — at least one, so a request past the element's end
/// still answers with its last line. Empty only for an element with no lines at all.
fn slice_from(
    doc: &Document,
    geom: wrap::WrapGeometry,
    extra_rows: &HashMap<u32, u32>,
    range: BufferRange,
    from_row: ElementRow,
    rows: u32,
) -> std::ops::Range<u32> {
    if range.is_empty() {
        return range.start()..range.start();
    }
    let first = line_at_element_row(doc, geom, extra_rows, range, from_row.get());
    let no_wrap = matches!(geom.wrap, aether_protocol::viewport::WrapMode::None);
    let wrapped = (!no_wrap).then(|| doc.wrapped_rows(geom));
    // The rows of the first line above `from_row` — a wrapped line's earlier rows, or the phantom
    // rows above its text — are not rows the request asked for, so they do not count towards its
    // fill: a request for three rows from a line's second row reaches two lines further down.
    let first_starts = element_rows_before(doc, geom, extra_rows, range, first).get();
    let mut filled = i64::from(first_starts) - i64::from(from_row.get());
    let mut last_excl = first;
    for i in first..range.end_exclusive() {
        if filled >= i64::from(rows) && last_excl > first {
            break;
        }
        let own = wrapped
            .as_ref()
            .map_or(1, |r| r.get(i as usize).copied().unwrap_or(1));
        filled += i64::from(own + extra_rows.get(&i).copied().unwrap_or(0));
        last_excl = i + 1;
    }
    first..last_excl
}

/// Everything that decorates a rendered window beyond the text itself: search highlights, the
/// inline-diff state, diagnostics squiggles, and the buffer's git status. Bundled because every
/// `render_window` caller assembles the same set from `ServerState`.
struct WindowDecorations<'a> {
    search: Option<&'a SearchEntry>,
    sneak: Option<&'a SneakEntry>,
    diff_view: bool,
    hunks: &'a [crate::git::DiffHunk],
    /// Conflict blocks, for a file a stopped merge or rebase left conflicted. Empty otherwise —
    /// and when it isn't, `hunks` is empty, because a conflicted file has no baseline to diff.
    conflicts: &'a [crate::git::ConflictRegion],
    diagnostics: &'a [crate::lsp::diagnostics::BufferDiagnostic],
}

/// Whether a render carries sneak labels.
///
/// Explicit rather than "show them whenever a session exists", because the post-edit broadcast must
/// *not*: a sneak session can't coexist with an edit by the same client, so labels riding that
/// render would be stale by construction. Naming the two cases keeps that reasoning at the call
/// site instead of in a `None` that looks like an oversight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SneakLabels {
    Shown,
    Hidden,
}

/// Resolve everything a render overlays on `buffer_id`'s text.
///
/// Inside the renderer rather than at each call site: it was assembled identically at all eight,
/// and it has to resolve per *element* once a view's elements window different buffers — which is
/// a change to make in one place, not eight.
fn window_decorations(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
    diff_view: bool,
    sneak: SneakLabels,
) -> WindowDecorations<'_> {
    WindowDecorations {
        search: render_matches(s, client_id, buffer_id),
        sneak: match sneak {
            SneakLabels::Shown => s.sneaks.get(&(client_id, buffer_id)),
            SneakLabels::Hidden => None,
        },
        diff_view,
        hunks: buffer_both_hunks(s, buffer_id),
        conflicts: buffer_conflicts(s, buffer_id),
        diagnostics: buffer_diagnostics(s, buffer_id),
    }
}

#[allow(clippy::too_many_arguments)] // the view's geometry, spelled out
/// One element's rendered lines: the slice of its own buffer currently in view.
///
/// Everything resolved here is **buffer**-scoped — text, syntax, hunks, conflicts, diagnostics,
/// search, sneak — which is precisely why it had to come out of the view-level render. Once a
/// view's elements window different buffers, each of these answers differently per element, and a
/// single set computed once for "the" buffer would silently apply one file's diff to another's text.
#[allow(clippy::too_many_arguments)]
fn render_element_lines(
    s: &ServerState,
    client_id: ClientId,
    binding: &ElementBinding,
    first: u32,
    last_excl: u32,
    geom: wrap::WrapGeometry,
    diff_view: bool,
    sneak_labels: SneakLabels,
) -> Vec<LogicalLineRender> {
    let buffer_id = binding.buffer_id;
    let buf = s.doc_of(buffer_id);
    let wrap::WrapGeometry {
        wrap,
        cols,
        marker_width,
        tab_width,
    } = geom;
    let WindowDecorations {
        search,
        sneak,
        diff_view,
        hunks,
        conflicts,
        diagnostics,
    } = window_decorations(s, client_id, buffer_id, diff_view, sneak_labels);

    // What the *view* says about these lines wins over what the buffer says about itself. A patch's
    // hunk windows a real file, but shows the diff's opinion of it — which lines this commit added,
    // and what it removed — not the file's current state against HEAD. Where the view has no
    // opinion (an ordinary editor), the buffer describing itself is exactly right.
    let supplied = binding.decorations.as_deref();

    // Per-line change markers drive the always-on gutter, so they're computed whenever hunks are
    // known — independent of the diff-view toggle. Phantom "deleted" rows, by contrast, only
    // appear while the diff view is on.
    let markers = match supplied {
        Some(d) => d.markers.clone(),
        None => diff_markers_by_line(hunks, buf.line_count()),
    };
    // Not gated on `diff_view`, unlike everything else here: the sides of a conflict are not a
    // review mode you opt into, they're the only way to read the file at all.
    let conflict_lines = conflict_lines_by_line(conflicts, buf.line_count());
    let intraline = if diff_view {
        intraline_for_window(hunks, buf, first, last_excl)
    } else {
        IntralineEmphasis::default()
    };
    // Phantoms are the diff view, wherever they come from. A patch's hunk windows a real file, so
    // with the view off it reads as that file's changed region — the removed content collapses to
    // the gutter's `▔` on the line it sat above, exactly as it does in an ordinary editor. That
    // marker is ungated (see `markers` above), so a deletion stays visible, navigable and
    // stageable with the diff off; only its *text* goes away.
    //
    // A whole-file deletion is the case this cannot reach: it has no surviving line to mark, so its
    // removed lines are ordinary buffer text of the generated document rather than phantoms, and
    // they stay. Removals collapse wherever there is a line left to collapse onto.
    let baseline_rows = match supplied {
        Some(d) if diff_view => d.baseline_above.clone(),
        None if diff_view && buf.generated.is_none() => {
            deleted_rows_by_anchor(hunks, buf.line_count(), Some(&intraline))
        }
        _ => HashMap::new(),
    };

    // For highlighting we need the whole source as bytes. Computed once per element rather than
    // per line. Skipped entirely when no syntax is attached.
    let source: Option<String> = buf
        .syntax
        .as_ref()
        .map(|_| buf.text.chunks().collect::<String>());
    // Generated read-only content (a commit's patch) instead of a parse tree — see
    // [`crate::patch::GeneratedPatch`]. Both are never present at once.
    let generated = buf.generated.as_ref().map(|g| &g.decorations);

    let mut lines: Vec<LogicalLineRender> =
        Vec::with_capacity(last_excl.saturating_sub(first) as usize);
    for i in first..last_excl {
        let line_slice = buf.text.line(i as usize);
        let mut text: String = line_slice.chunks().collect();
        if text.ends_with('\n') {
            text.pop();
        }

        let highlights = match (&buf.syntax, source.as_deref()) {
            (Some(syntax), Some(source)) => {
                let line_char_start = buf.text.line_to_char(i as usize);
                let line_byte_start = buf.text.char_to_byte(line_char_start);
                let line_byte_end = line_byte_start + text.len();
                crate::syntax::highlights_for_range(
                    syntax.config,
                    &syntax.tree,
                    &syntax.injections,
                    source,
                    line_byte_start,
                    line_byte_end,
                )
            }
            // No tree — but generated content classified itself when it was built, and those spans
            // are already in this exact shape.
            _ => generated
                .and_then(|d| d.highlights.get(i as usize))
                .cloned()
                .unwrap_or_default(),
        };

        let mut render =
            wrap::render_line(&text, i, cols, wrap, marker_width, tab_width, highlights);
        if let Some(entry) = search {
            render.search_matches = matches_on_line(entry, i, text.len() as u32);
        }
        if let Some(entry) = sneak {
            render.sneak_targets = sneak_targets_on_line(entry, i, text.len() as u32);
        }
        // Phantom baseline rows belong *to* this line. A patch's chrome does not — it is composed
        // into the tree below, as siblings of the hunks it separates.
        if let Some(rows) = baseline_rows.get(&i) {
            render.baseline_above = rows.clone();
        }
        // The line's change-state. A match rather than the sequence of writes this used to be:
        // the three cases are mutually exclusive by construction (a conflicted file's blocks are
        // masked out of its own diff; a generated patch has no baseline), and expressing that as
        // one value is what stopped two producers writing to the same `stage` and `emphasis`.
        let patch_at = |f: fn(&crate::patch::StaticDecorations) -> &Vec<DiffStage>| {
            generated.and_then(|d| f(d).get(i as usize).copied())
        };
        render.change = if let Some(side) = generated
            .and_then(|d| d.patch.get(i as usize).copied())
            .flatten()
        {
            LineChange::Patch {
                side,
                stage: patch_at(|d| &d.stage).unwrap_or_default(),
                // A patch's emphasis is precomputed and lands on *both* sides — unlike the inline
                // diff view, where the old side is a phantom row carrying its own.
                emphasis: generated
                    .and_then(|d| d.emphasis.get(i as usize))
                    .cloned()
                    .unwrap_or_default(),
            }
        } else if let Some(side) = conflict_lines.get(&i).copied() {
            LineChange::Conflict { side }
        } else if let Some((marker, stage)) = markers.get(&i).copied() {
            LineChange::Changed {
                marker,
                stage,
                // Gated with the phantoms it belongs to: intra-line emphasis says *which part* of a
                // line the removed one differed over, which is not a question the diff-off view is
                // asking. `intraline` is already empty when the view is off; a supplied one has to
                // be gated here.
                emphasis: match supplied {
                    Some(d) if diff_view => d.emphasis.get(&i).cloned().unwrap_or_default(),
                    Some(_) => Vec::new(),
                    None => intraline.lines.get(&i).cloned().unwrap_or_default(),
                },
            }
        } else {
            LineChange::None
        };
        render.diagnostics = diagnostic_spans_on_line(diagnostics, i, text.len() as u32);
        lines.push(render);
    }
    lines
}

/// One element as rendered for this frame: its identity and height, plus the slice in view.
struct RenderedElement {
    element: aether_protocol::viewport::FieldId,
    buffer: BufferId,
    rows: u32,
    /// Where the loaded slice starts within the element.
    first_row: ElementRow,
    laid_out_by: aether_protocol::ui::LayoutOwner,
    chrome_above: std::sync::Arc<Vec<Element>>,
    first_buffer_line: u32,
    lines: Vec<LogicalLineRender>,
}

/// Render the window a viewport shows of its view: the whole tree, every element carrying its
/// height, with the lines of the slices the viewport has loaded placed by their row within their
/// element.
///
/// Everything a render needs is read off the viewport — which view, which slices, at what width,
/// which element holds the cursor, the wrap geometry, the diff toggle — so a caller cannot hand it
/// another element's width or the wrong buffer. The only free input is whether sneak labels ride
/// along.
pub fn render_window(s: &ServerState, vp: &Viewport, sneak_labels: SneakLabels) -> Window {
    let (client_id, elements, focused, diff_view) = (
        vp.client_id,
        &s.view_of(vp).elements[..],
        vp.focused,
        vp.diff_view,
    );
    let geom = vp.wrap_geometry();
    // Each element's lines as the buffer has them now — the clamp that keeps a stale extent, or a
    // stale loaded slice, from indexing past the end of a rope.
    let layout = s.layout_of(elements);
    let wrap::WrapGeometry {
        wrap, tab_width, ..
    } = geom;
    // The view's own closing chrome: the rule closing a patch belongs to the view rather than to
    // any element, so it lives on the **view's** document — the generated patch, even when every
    // element windows a real file. Looking on the first *element's* buffer answered `None` the
    // moment the elements stopped being that document, and the rule silently stopped rendering.
    let trailing_chrome: &[Element] = s
        .try_doc_of(vp.view_id.presenting_buffer())
        .and_then(|d| d.generated.as_ref())
        .map(|g| &g.decorations.trailing_chrome[..])
        .unwrap_or(&[]);
    // Per **element**, because that is how the rows themselves are decided: `render_element_lines`
    // takes an element's phantom rows from the *view's* decorations when it has them, and only an
    // ordinary editor's inline diff falls back to the buffer's own hunks. Counting them from one
    // buffer's diff instead left a bound patch's height six rows short of the view it described —
    // the scroll bound stopped before the end, and the fetch that fills the viewport thought it had
    // already reached it, so the bottom of the screen went blank.
    let geometry: Vec<(wrap::WrapGeometry, HashMap<u32, u32>)> = elements
        .iter()
        .map(|binding| element_geometry(s, binding, geom, diff_view))
        .collect();

    // Render each element's loaded slice, if it has one. An element with nothing loaded contributes
    // no lines — but still reports its height, which is what lets a client place the ones that
    // are. A loaded slice is clipped to the lines the element has *now*: a slice comes from an
    // earlier answer, but the buffer it indexes is live, and a stale range used to index past the
    // end of a rope and panic the server.
    let mut rendered: Vec<RenderedElement> = Vec::with_capacity(elements.len());
    for (idx, binding) in elements.iter().enumerate() {
        let element = idx as aether_protocol::viewport::FieldId;
        let range = layout.element_range(element);
        let doc = s.doc_of(binding.buffer_id);
        let loaded = vp
            .loaded
            .get(idx)
            .cloned()
            .flatten()
            .and_then(|slice| layout.clip(element, slice));
        let (geom, phantom_rows) = &geometry[idx];
        let geom = *geom;
        let (first_row, first_buffer_line, lines) = match loaded {
            Some(r) => (
                element_rows_before(doc, geom, phantom_rows, range, r.start()),
                r.start(),
                render_element_lines(
                    s,
                    client_id,
                    binding,
                    r.start(),
                    r.end_exclusive(),
                    geom,
                    diff_view,
                    sneak_labels,
                ),
            ),
            None => (ElementRow::ZERO, range.start(), Vec::new()),
        };
        rendered.push(RenderedElement {
            element,
            buffer: binding.buffer_id,
            // The element's *whole* height, from the layout's clamped range rather than the raw
            // extents: the diff's account of a hunk can outlive the lines it described.
            rows: element_visual_rows(doc, range, geom, phantom_rows),
            first_row,
            laid_out_by: binding.laid_out_by,
            chrome_above: binding.chrome_above.clone(),
            first_buffer_line,
            lines,
        });
    }

    let max_line_width = if matches!(wrap, aether_protocol::viewport::WrapMode::None) {
        elements
            .iter()
            .map(|e| s.doc_of(e.buffer_id).max_line_width(tab_width))
            .max()
            .unwrap_or(0)
    } else {
        0
    };

    Window {
        max_line_width,
        // The status bar's git cluster is about the *focused* element's buffer: it sits beside a
        // label naming the focused file, so reading it off element 0 put a different file's change
        // counts next to that name in any multi-file view.
        git_status: buffer_git_status(s, s.focused_buffer(vp)),
        other_elements_dirty: other_elements_dirty(s, elements, focused),
        root: compose_tree(rendered, trailing_chrome),
    }
}

/// How an element's rows are counted: the viewport's wrapping and the element's phantom rows for
/// one the server lays out; unwrapped and phantom-free for one the client lays out, where a wire
/// row is a line and nothing the server could add to the count would survive the client's
/// measure. Every path that counts an element's rows — the subscribe, a window request, a cursor
/// window, the render — takes its geometry from here, so no path can wrap what another sent whole.
fn element_geometry(
    s: &ServerState,
    binding: &ElementBinding,
    geom: wrap::WrapGeometry,
    diff_view: bool,
) -> (wrap::WrapGeometry, HashMap<u32, u32>) {
    match binding.laid_out_by {
        aether_protocol::ui::LayoutOwner::Server => {
            (geom, element_phantom_rows(s, binding, diff_view))
        }
        aether_protocol::ui::LayoutOwner::Client => (
            wrap::WrapGeometry {
                wrap: aether_protocol::viewport::WrapMode::None,
                ..geom
            },
            HashMap::new(),
        ),
    }
}

/// The lines to load of an element: the whole of it for one the client lays out, whatever was
/// asked, else what `screen` says. The client renders prose from the source entire — a block's
/// shape depends on the lines around it — so a partial load is never a state it can use, and
/// making the load whole here rather than trusting each request to ask for it is what keeps every
/// push, reseat and re-render of the element whole too.
fn whole_if_client_laid_out(
    binding: &ElementBinding,
    range: BufferRange,
    screen: impl FnOnce() -> std::ops::Range<u32>,
) -> std::ops::Range<u32> {
    match binding.laid_out_by {
        aether_protocol::ui::LayoutOwner::Client => range.lines(),
        aether_protocol::ui::LayoutOwner::Server => screen(),
    }
}

/// The phantom ("deleted") rows an element's lines carry, as counts per buffer line.
///
/// The height half of what [`render_element_lines`] puts on the lines themselves, and it must follow
/// the same rule or the view is a different height than the rows it ships: what the **view** says
/// about an element's lines wins over what the buffer says about itself — a patch's hunk windows a
/// real file but shows the diff's removed lines, which are the element's decorations. Only an
/// ordinary editor, where the view has no opinion, falls back to the buffer's own hunks.
///
/// Both sources are gated on the inline diff being on, and gated *here as well as there*: a
/// mismatch between the rows a view counts and the rows it ships is what blanks the bottom of a
/// screen and puts a scroll limit somewhere the content isn't.
fn element_phantom_rows(
    s: &ServerState,
    binding: &ElementBinding,
    diff_view: bool,
) -> HashMap<u32, u32> {
    if !diff_view {
        return HashMap::new();
    }
    if let Some(d) = binding.decorations.as_deref() {
        return d
            .baseline_above
            .iter()
            .map(|(line, rows)| (*line, rows.len() as u32))
            .collect();
    }
    let buf = s.doc_of(binding.buffer_id);
    if buf.generated.is_some() {
        return HashMap::new();
    }
    // Counts only: the emphasis a phantom row carries does not change how many there are.
    deleted_rows_by_anchor(
        buffer_both_hunks(s, binding.buffer_id),
        buf.line_count(),
        None,
    )
    .into_iter()
    .map(|(line, rows)| (line, rows.len() as u32))
    .collect()
}

/// Compose the rendered elements and a generated patch's chrome into the view's tree.
///
/// Chrome *separates* hunks, so a chrome run closes the editor element above it and the next line
/// opens a new one. An ordinary buffer has no chrome and comes out as a single editor — which is
/// what makes this a strict generalisation: the flat window is the one-element case.
///
/// Element ids index the view's elements, and come from the document's own span table rather than
/// from a walk of the visible window — see [`crate::patch::ElementSpan`].
fn compose_tree(rendered: Vec<RenderedElement>, trailing_chrome: &[Element]) -> Element {
    // Every element gets a node, including ones with nothing loaded — those carry their height and
    // an empty `lines`. The tree is the whole view, not the visible part of it: a client lays the
    // view out and scrolls it from the tree alone, so an element that vanished while off screen
    // would take its height with it and everything below would slide up as you scrolled. Chrome is
    // always in it for the same reason: it occupies rows whether or not the lines under it are
    // loaded, and a client laying the view out has to know where.
    let node_of = |r: RenderedElement| Element::Editor {
        element: r.element,
        buffer: r.buffer,
        rows: r.rows,
        first_row: r.first_row,
        laid_out_by: r.laid_out_by,
        first_buffer_line: r.first_buffer_line,
        lines: r.lines,
    };
    // One element and no chrome at all is an ordinary buffer: the tree is that editor, not a stack
    // of one. Everything else composes. Note this asks about the *elements*, not about whether a
    // document was generated — composition is a fact about the view's shape, and a driver that
    // builds elements without generating a document gets the same answer.
    if rendered.len() == 1 && rendered[0].chrome_above.is_empty() && trailing_chrome.is_empty() {
        let mut rendered = rendered;
        return node_of(rendered.remove(0));
    }
    let mut children: Vec<Element> = Vec::new();
    for r in rendered {
        children.extend(r.chrome_above.iter().cloned());
        children.push(node_of(r));
    }
    // The closing rule has no line to sit above: the patch ends without a trailing newline, so
    // there is no empty last line to anchor it to. As a sibling it simply comes last.
    children.extend(trailing_chrome.iter().cloned());
    Element::Stack { children }
}

/// The match set to paint for `(client, buffer)`: the active search if there is one, else the LSP
/// document-highlight set (the symbol under the cursor). Both render through `matches_on_line` with
/// the identical fill, and a real search always wins — which is exactly what enforces "symbol
/// highlights only when no search is active". Used by every *view-change* render path (subscribe,
/// scroll, resize, wrap, diff-view, and the cursor-settle refresh); the post-mutation broadcast
/// paths keep reading `searches` directly, since a mutation clears the symbol set anyway.
pub fn render_matches(
    s: &ServerState,
    client_id: ClientId,
    buffer_id: BufferId,
) -> Option<&SearchEntry> {
    s.searches
        .get(&(client_id, buffer_id))
        .or_else(|| s.symbol_highlights.get(&(client_id, buffer_id)))
}

/// Per-line byte ranges from `entry.matches` clipped to `[0, line_len)` for `line_idx`. Matches
/// that span multiple lines contribute one range per line they touch.
fn matches_on_line(entry: &SearchEntry, line_idx: u32, line_len: u32) -> Vec<SearchMatchRange> {
    let mut out = Vec::new();
    for (start, end_excl) in &entry.matches {
        if line_idx < start.line || line_idx > end_excl.line {
            continue;
        }
        let s = if line_idx == start.line { start.col } else { 0 };
        let e = if line_idx == end_excl.line {
            end_excl.col
        } else {
            line_len
        };
        let s = s.min(line_len);
        let e = e.min(line_len);
        if s < e {
            out.push(SearchMatchRange { start: s, end: e });
        }
    }
    out
}

/// Sneak word-jump targets on `line_idx` as byte ranges (clamped to the line), each carrying its
/// label. Candidate words never span lines, so each contributes to exactly the line it starts on.
fn sneak_targets_on_line(entry: &SneakEntry, line_idx: u32, line_len: u32) -> Vec<SneakTarget> {
    let mut out = Vec::new();
    for cand in &entry.candidates {
        if cand.start.line != line_idx {
            continue;
        }
        let start = cand.start.col.min(line_len);
        let end = cand.end_excl.col.min(line_len);
        // The chip spans the typed prefix, but only for a labelled (jumpable) target; deferred
        // candidates stay calm with just the word tint, so collapse their chip to empty.
        let prefix_end = if cand.label.is_some() {
            cand.prefix_end.col.min(line_len).max(start)
        } else {
            start
        };
        if start < end {
            out.push(SneakTarget {
                start,
                end,
                prefix_end,
                label: cand.label,
            });
        }
    }
    out
}

#[cfg(test)]
mod diff_anchor_tests {
    use super::*;

    /// The text and stage of a phantom baseline row. Chrome rows never appear in these maps.
    fn baseline(row: &BaselineRow) -> (&str, DiffStage) {
        (row.text.as_str(), row.stage)
    }
    use crate::git::{ChangeKind, DiffHunk};

    fn hunk(kind: ChangeKind, anchor_line: u32, new_lines: u32, deleted: &[&str]) -> DiffHunk {
        DiffHunk {
            kind,
            anchor_line,
            new_lines,
            deleted: deleted.iter().map(|s| s.to_string()).collect(),
            old_start: 0, // irrelevant for render-anchoring tests
            stage: aether_protocol::viewport::DiffStage::Unstaged,
        }
    }

    #[test]
    fn modified_and_deleted_hunks_anchor_their_removed_text() {
        let hunks = vec![
            hunk(ChangeKind::Modified, 1, 1, &["old beta"]),
            hunk(ChangeKind::Deleted, 4, 0, &["gone one", "gone two"]),
            hunk(ChangeKind::Added, 7, 2, &[]), // additions contribute no phantom rows
        ];
        let map = deleted_rows_by_anchor(&hunks, 100, None);
        assert_eq!(map.get(&1).map(Vec::len), Some(1));
        assert_eq!(baseline(&map[&1][0]), ("old beta", DiffStage::Unstaged));
        assert_eq!(map.get(&4).map(Vec::len), Some(2));
        assert_eq!(baseline(&map[&4][1]).0, "gone two");
        assert!(!map.contains_key(&7), "pure additions have no deleted rows");
    }

    #[test]
    fn eof_deletion_clamps_to_last_line() {
        // A deletion anchored past the last line (e.g. removed the file's tail) clamps onto the
        // final line index so it still renders (above the trailing empty line of the buffer).
        let hunks = vec![hunk(ChangeKind::Deleted, 9, 0, &["tail"])];
        let map = deleted_rows_by_anchor(&hunks, 5, None); // line_count = 5 → last index 4
        assert!(!map.contains_key(&9));
        assert_eq!(map.get(&4).map(Vec::len), Some(1));
        assert_eq!(map[&4][0].text, "tail");
    }

    fn doc_with(text: &str) -> Document {
        let mut doc = Document::scratch(DocumentId(1), None);
        doc.text = ropey::Rope::from_str(text);
        doc
    }

    #[test]
    fn intraline_pairs_modified_lines_and_rides_the_rows() {
        // Line 1 modified: "count" → "total". The pair's emphasis lands on the buffer line (new
        // side) and, via `deleted_rows_by_anchor`, on the phantom row (old side).
        let buf = doc_with("aaa\nlet total = 1;\nccc\n");
        let hunks = vec![hunk(ChangeKind::Modified, 1, 1, &["let count = 1;"])];
        let intra = intraline_for_window(&hunks, &buf, 0, 3);
        let count = EmphasisRange { start: 4, end: 9 };
        assert_eq!(intra.lines.get(&1), Some(&vec![count])); // "total" (same cols)
        assert_eq!(intra.rows.get(&(0, 0)), Some(&vec![count])); // "count"

        let map = deleted_rows_by_anchor(&hunks, 3, Some(&intra));
        assert_eq!(map[&1][0].emphasis, vec![count]);
    }

    #[test]
    fn intraline_leaves_unpaired_extra_lines_alone() {
        // Modified hunk with 1 old line and 2 new: only the first new line pairs; the second is
        // effectively an addition and gets no emphasis.
        let buf = doc_with("let total = 1;\nbrand new line\n");
        let hunks = vec![hunk(ChangeKind::Modified, 0, 2, &["let count = 1;"])];
        let intra = intraline_for_window(&hunks, &buf, 0, 2);
        assert!(intra.lines.contains_key(&0));
        assert!(!intra.lines.contains_key(&1));
    }

    #[test]
    fn intraline_skips_pairs_fully_outside_the_window() {
        let buf = doc_with("aaa\nlet total = 1;\nccc\n");
        let hunks = vec![hunk(ChangeKind::Modified, 1, 1, &["let count = 1;"])];
        // Window [2, 3): neither the pair's line (1) nor its anchor (1) is visible.
        let intra = intraline_for_window(&hunks, &buf, 2, 3);
        assert!(intra.lines.is_empty());
        assert!(intra.rows.is_empty());
    }

    #[test]
    fn intraline_unstaged_pair_wins_the_line_over_staged() {
        // The same buffer line paired by a staged and an unstaged Modified hunk (modified,
        // staged, modified again): the line's emphasis follows the unstaged top layer, matching
        // the marker/tint rule.
        let buf = doc_with("let total = 1;\n");
        let mut staged = hunk(ChangeKind::Modified, 0, 1, &["let count = 1;"]);
        staged.stage = DiffStage::Staged;
        let unstaged = hunk(ChangeKind::Modified, 0, 1, &["let sum = 1;"]);
        // Staged listed first (compose order); the unstaged result must still win.
        let intra = intraline_for_window(&[staged, unstaged], &buf, 0, 1);
        // "sum" (3 bytes) → "total": emphasis derives from the unstaged pair on the new side.
        assert_eq!(
            intra.lines.get(&0),
            Some(&vec![EmphasisRange { start: 4, end: 9 }])
        );
        // Both phantom rows keep their own old-side emphasis regardless.
        assert!(intra.rows.contains_key(&(0, 0)));
        assert!(intra.rows.contains_key(&(1, 0)));
    }

    #[test]
    fn markers_cover_new_side_lines_and_deletion_anchors() {
        let hunks = vec![
            hunk(ChangeKind::Modified, 2, 1, &["was"]), // line 2 → Modified
            hunk(ChangeKind::Added, 5, 3, &[]),         // lines 5,6,7 → Added
            hunk(ChangeKind::Deleted, 9, 0, &["x"]),    // line 9 → Deleted (gutter flag)
        ];
        let map = diff_markers_by_line(&hunks, 100);
        let unstaged = aether_protocol::viewport::DiffStage::Unstaged;
        assert_eq!(map.get(&2), Some(&(DiffMarker::Modified, unstaged)));
        assert_eq!(map.get(&5), Some(&(DiffMarker::Added, unstaged)));
        assert_eq!(map.get(&7), Some(&(DiffMarker::Added, unstaged)));
        assert_eq!(map.get(&8), None);
        assert_eq!(map.get(&9), Some(&(DiffMarker::Deleted, unstaged)));
    }

    #[test]
    fn added_modified_marker_wins_over_a_deletion_anchor_on_the_same_line() {
        // A deletion anchored on a line that's also added/modified keeps the stronger marker.
        let hunks = vec![
            hunk(ChangeKind::Deleted, 3, 0, &["gone"]),
            hunk(ChangeKind::Modified, 3, 1, &["was"]),
        ];
        let map = diff_markers_by_line(&hunks, 100);
        let unstaged = aether_protocol::viewport::DiffStage::Unstaged;
        assert_eq!(map.get(&3), Some(&(DiffMarker::Modified, unstaged)));
    }

    #[test]
    fn overlapping_staged_and_unstaged_hunks_read_as_unstaged() {
        use aether_protocol::viewport::DiffStage;
        // Composed-view collision: line 3 staged-modified then modified again. The unstaged top
        // layer wins outright — the line reads as plain unstaged (no third state), regardless of
        // which order the hunks arrive in. Surrounding staged-only lines stay Staged.
        let mut staged = hunk(ChangeKind::Modified, 2, 3, &["a", "b", "c"]); // lines 2,3,4
        staged.stage = DiffStage::Staged;
        let unstaged = hunk(ChangeKind::Modified, 3, 1, &["b'"]); // line 3 only
        let map = diff_markers_by_line(&[staged.clone(), unstaged.clone()], 100);
        assert_eq!(
            map.get(&2),
            Some(&(DiffMarker::Modified, DiffStage::Staged))
        );
        assert_eq!(
            map.get(&3),
            Some(&(DiffMarker::Modified, DiffStage::Unstaged))
        );
        assert_eq!(
            map.get(&4),
            Some(&(DiffMarker::Modified, DiffStage::Staged))
        );
        // Order-independent: a staged hunk processed after the unstaged one changes nothing.
        let reversed = diff_markers_by_line(&[unstaged, staged], 100);
        assert_eq!(
            reversed.get(&3),
            Some(&(DiffMarker::Modified, DiffStage::Unstaged))
        );
    }

    #[test]
    fn deleted_rows_keep_only_the_unstaged_layer_at_a_shared_anchor() {
        use aether_protocol::viewport::DiffStage;
        // A staged and an unstaged deletion anchored at the same line: only the index's (unstaged)
        // text is shown — it's what a revert would restore. A staged deletion elsewhere keeps its
        // rows (with the staged tag).
        let mut staged = hunk(ChangeKind::Deleted, 1, 0, &["head text"]);
        staged.stage = DiffStage::Staged;
        let unstaged = hunk(ChangeKind::Deleted, 1, 0, &["index text"]);
        let mut staged_elsewhere = hunk(ChangeKind::Deleted, 5, 0, &["solo head text"]);
        staged_elsewhere.stage = DiffStage::Staged;
        let map = deleted_rows_by_anchor(&[staged, unstaged, staged_elsewhere], 10, None);
        let rows = &map[&1];
        assert_eq!(
            rows.len(),
            1,
            "staged layer suppressed at the shared anchor"
        );
        assert_eq!(baseline(&rows[0]), ("index text", DiffStage::Unstaged));
        let solo = &map[&5];
        assert_eq!(baseline(&solo[0]), ("solo head text", DiffStage::Staged));
    }

    #[test]
    fn change_counts_tally_lines_by_class() {
        // Added/Modified count new-side lines (`new_lines`); Deleted counts removed lines
        // (`deleted.len`). A Modified hunk's replaced old lines ride its `modified` count and are
        // *not* also tallied as deletions.
        let hunks = vec![
            hunk(ChangeKind::Added, 5, 3, &[]),                // +3
            hunk(ChangeKind::Modified, 2, 1, &["a", "b"]),     // ~1 (2 old lines → 1 new)
            hunk(ChangeKind::Modified, 10, 2, &["c"]),         // ~2
            hunk(ChangeKind::Deleted, 9, 0, &["x", "y", "z"]), // -3
        ];
        let c = git_change_counts(&hunks);
        assert_eq!((c.added, c.modified, c.deleted), (3, 3, 3));
        assert!(git_change_counts(&[]).is_empty());
    }
}

#[cfg(test)]
mod subscribe_snapshot_tests {
    use super::*;
    use crate::lsp::diagnostics::BufferDiagnostic;
    use aether_protocol::viewport::{DiagnosticSeverity, ScrollPosition, WrapMode};
    use aether_protocol::LogicalPosition;
    use tokio::sync::Mutex;

    fn diag(line: u32, severity: DiagnosticSeverity) -> BufferDiagnostic {
        BufferDiagnostic {
            start: LogicalPosition { line, col: 0 },
            end: LogicalPosition { line, col: 1 },
            severity,
            message: "m".into(),
        }
    }

    #[test]
    fn workspace_diagnostics_read_path_store_not_buffer_set() {
        use crate::lsp::diagnostics::RawDiagnostic;
        let mut st = ServerState::new();
        let root = std::path::PathBuf::from("/proj");
        st.workspaces.insert(
            "p".to_string(),
            crate::state::WorkspaceEntry {
                worktrees: Default::default(),
                id: "p".to_string(),
                name: Some("p".to_string()),
                base_paths: None,
                paths: vec![root.clone()],
                workspace_index: std::sync::Arc::new(crate::workspace_index::WorkspaceIndex::new(
                    vec![root.clone()],
                )),
                mru_buffers: std::collections::VecDeque::new(),
                dormant_buffers: Vec::new(),
                jumplist: None,
                projects: Vec::new(),
            },
        );
        let client_id = uuid::Uuid::new_v4();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        std::mem::forget(_rx);
        st.clients.insert(
            client_id,
            crate::state::ClientSession {
                client_id,
                outbound: tx,
                pushes_written: Default::default(),
                active_workspace: Some("p".to_string()),
            },
        );

        // a.rs is open with a diagnostic in the BUFFER-keyed set (line 42). The workspace picker is a
        // separate lens — it must ignore that set entirely and read only `path_diagnostics`.
        let buffer_id = st.allocate_buffer_id();
        st.insert_buffer_with_document(buffer_id, None, false, |d| {
            Document::new_at_path(d, root.join("src/a.rs"), None)
        });
        st.diagnostics
            .insert(buffer_id, vec![diag(42, DiagnosticSeverity::Error)]);

        // The path-keyed store is the workspace picker's sole source: a.rs (open) + b.rs (closed).
        st.path_diagnostics.insert(
            root.join("src/a.rs"),
            vec![
                RawDiagnostic {
                    line: 2,
                    severity: DiagnosticSeverity::Warning,
                    message: "warn".into(),
                },
                RawDiagnostic {
                    line: 9,
                    severity: DiagnosticSeverity::Error,
                    message: "err".into(),
                },
            ],
        );
        st.path_diagnostics.insert(
            root.join("src/b.rs"),
            vec![RawDiagnostic {
                line: 5,
                severity: DiagnosticSeverity::Error,
                message: "boom".into(),
            }],
        );

        let cands = build_workspace_diagnostic_candidates(&st, client_id);

        // Three rows, all from `path_diagnostics`; the buffer-keyed line 42 is NOT merged in.
        assert_eq!(cands.len(), 3);
        assert!(
            !cands.iter().any(|c| c.line == 42),
            "buffer-keyed set is a separate lens, not merged"
        );
        // Grouped by file (a.rs before b.rs), then by line; every row is line-granular (col 0).
        assert_eq!(cands[0].relative_path, "src/a.rs");
        assert_eq!(cands[0].line, 2);
        assert_eq!(cands[1].relative_path, "src/a.rs");
        assert_eq!(cands[1].line, 9);
        assert_eq!(cands[2].relative_path, "src/b.rs");
        assert_eq!(cands[2].line, 5);
        assert_eq!(cands[2].message, "boom");
        assert!(
            cands.iter().all(|c| c.col == 0 && c.end_col == 0),
            "workspace rows are line-granular"
        );
    }

    #[test]
    fn open_workspace_picker_live_refreshes_from_path_diagnostics() {
        use crate::lsp::diagnostics::RawDiagnostic;
        let mut st = ServerState::new();
        let root = std::path::PathBuf::from("/proj");
        st.workspaces.insert(
            "p".to_string(),
            crate::state::WorkspaceEntry {
                worktrees: Default::default(),
                id: "p".to_string(),
                name: Some("p".to_string()),
                base_paths: None,
                paths: vec![root.clone()],
                workspace_index: std::sync::Arc::new(crate::workspace_index::WorkspaceIndex::new(
                    vec![root.clone()],
                )),
                mru_buffers: std::collections::VecDeque::new(),
                dormant_buffers: Vec::new(),
                jumplist: None,
                projects: Vec::new(),
            },
        );
        let client_id = uuid::Uuid::new_v4();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        std::mem::forget(_rx);
        st.clients.insert(
            client_id,
            crate::state::ClientSession {
                client_id,
                outbound: tx,
                pushes_written: Default::default(),
                active_workspace: Some("p".to_string()),
            },
        );
        // An OPEN (subscribed), currently-empty workspace-diagnostics picker.
        let mut picker =
            picker_state::PickerState::new(picker_state::PickerCandidates::Diagnostics(Vec::new()));
        picker.kind = PickerKind::DiagnosticsWorkspace;
        picker.subscribed = Some(picker_state::SubscribedWindow {
            offset: 0,
            limit: 50,
        });
        st.pickers
            .insert((client_id, PickerKind::DiagnosticsWorkspace), picker);

        // A push lands a never-opened file's diagnostic in `path_diagnostics`...
        st.path_diagnostics.insert(
            root.join("src/markdown.rs"),
            vec![RawDiagnostic {
                line: 37,
                severity: DiagnosticSeverity::Error,
                message: "unexpected `}`".into(),
            }],
        );
        let pushes = refresh_workspace_diagnostics_pickers(&mut st);

        //...and the open picker picks it up live, with an update pushed to the viewing client.
        assert_eq!(
            pushes.len(),
            1,
            "one update to the client viewing the picker"
        );
        let picker = &st.pickers[&(client_id, PickerKind::DiagnosticsWorkspace)];
        let picker_state::PickerCandidates::Diagnostics(rows) = &picker.candidates else {
            panic!("expected Diagnostics candidates");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].relative_path, "src/markdown.rs");
        assert_eq!(rows[0].line, 37);
    }

    /// State with one file-text buffer carrying `diags` and the given external-change flags. No
    /// language server is attached (so `lsp_status` snapshots as `None`). `viewport_subscribe` reads
    /// only buffer/diagnostic/lsp state, so no client session registration is needed.
    fn setup(
        diags: Vec<BufferDiagnostic>,
        externally_modified: bool,
        externally_deleted: bool,
    ) -> (SharedState, ClientId, BufferId) {
        let mut st = ServerState::new();
        let buffer_id = st.allocate_buffer_id();
        st.insert_buffer_with_document(buffer_id, Some(1), false, |d| {
            let mut doc = Document::scratch(d, None);
            doc.text = ropey::Rope::from_str("alpha\nbeta\n");
            doc.externally_modified = externally_modified;
            doc.externally_deleted = externally_deleted;
            doc
        });
        if !diags.is_empty() {
            st.diagnostics.insert(buffer_id, diags);
        }
        (Arc::new(Mutex::new(st)), uuid::Uuid::new_v4(), buffer_id)
    }

    fn sub_params(buffer_id: ViewId) -> ViewportSubscribeParams {
        ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 24,
            overscan_rows: 0,
            scroll: ScrollPosition::default(),
            focus: None,
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
            diff_view: false,
            kind: None,
        }
    }

    #[tokio::test]
    async fn subscribe_snapshots_existing_diagnostic_counts() {
        // The regression: diagnostics computed before this viewport subscribed must still reach the
        // status bar. They now ride the subscribe response, not only the change-notification.
        let (state, client_id, buffer_id) = setup(
            vec![
                diag(0, DiagnosticSeverity::Error),
                diag(1, DiagnosticSeverity::Warning),
                diag(1, DiagnosticSeverity::Warning),
            ],
            false,
            false,
        );
        let mut ctx = ConnectionCtx { client_id };
        let res = viewport_subscribe(&state, &mut ctx, sub_params(ViewId(buffer_id)))
            .await
            .unwrap();
        let c = res.buffer_status.diagnostics;
        assert_eq!((c.errors, c.warnings), (1, 2));
    }

    #[tokio::test]
    async fn subscribe_snapshots_external_change_flags() {
        // A client that starts showing a buffer the watcher already flagged externally-modified must
        // see the flag immediately, not only on the next disk event.
        let (state, client_id, buffer_id) = setup(Vec::new(), true, false);
        let mut ctx = ConnectionCtx { client_id };
        let res = viewport_subscribe(&state, &mut ctx, sub_params(ViewId(buffer_id)))
            .await
            .unwrap();
        assert!(res.buffer_status.externally_modified);
        assert!(!res.buffer_status.externally_deleted);
    }

    #[tokio::test]
    async fn subscribe_to_clean_unbacked_buffer_snapshots_empty_status() {
        let (state, client_id, buffer_id) = setup(Vec::new(), false, false);
        let mut ctx = ConnectionCtx { client_id };
        let res = viewport_subscribe(&state, &mut ctx, sub_params(ViewId(buffer_id)))
            .await
            .unwrap();
        let s = &res.buffer_status;
        assert!(s.diagnostics.is_empty());
        assert!(!s.externally_modified && !s.externally_deleted);
        assert!(s.lsp_status.is_none());
    }
}

#[cfg(test)]
mod slice_tests {
    use super::*;
    use crate::state::{DocumentId, ElementLines, ViewLayout};

    fn doc(text: &str) -> Document {
        let mut d = Document::scratch(DocumentId(1), None);
        d.text = ropey::Rope::from_str(text);
        d
    }

    fn geom(cols: u32) -> wrap::WrapGeometry {
        wrap::WrapGeometry {
            wrap: aether_protocol::viewport::WrapMode::Soft,
            cols,
            marker_width: 0,
            tab_width: 4,
        }
    }

    fn whole(doc: &Document) -> BufferRange {
        let binding = ElementBinding {
            buffer_id: 1,
            lines: ElementLines::Whole,
            decorations: None,
            chrome_above: Default::default(),
            laid_out_by: aether_protocol::ui::LayoutOwner::Server,
        };
        ViewLayout::of(std::slice::from_ref(&binding), |_| doc.line_count()).element_range(0)
    }

    /// A slice is addressed by row within the element and answers with the lines that fill it:
    /// the line holding the first row and as many after it as the rows take, so a wrapped line
    /// counts for all of its rows.
    #[test]
    fn a_slice_covers_the_rows_asked_for() {
        // Lines 0 and 2 wrap to two rows at 8 cols; the rest are one row each.
        let d = doc("0123456789
b
0123456789
d
e
");
        let range = whole(&d);
        let none = HashMap::new();
        assert_eq!(
            slice_from(&d, geom(8), &none, range, ElementRow(0), 3),
            0..2
        );
        assert_eq!(
            slice_from(&d, geom(8), &none, range, ElementRow(1), 3),
            0..3,
            "row 1 is still line 0's; three rows from there reach into line 2"
        );
        assert_eq!(
            element_rows_before(&d, geom(8), &none, range, 3),
            ElementRow(5),
            "line 3 starts after two two-row lines and one one-row line"
        );
    }

    /// A request past the element's end answers with its last line rather than nothing: the client
    /// asked for rows the element does not have, and the honest answer is where it does end.
    #[test]
    fn a_slice_past_the_end_lands_on_the_last_line() {
        let d = doc("a
b
c
");
        let range = whole(&d);
        let none = HashMap::new();
        let last = range.end_exclusive() - 1;
        assert_eq!(
            slice_from(&d, geom(80), &none, range, ElementRow(99), 10),
            last..last + 1
        );
    }

    /// Phantom rows count towards a slice's rows and a line's start row, as they do on screen.
    #[test]
    fn phantom_rows_count() {
        let d = doc("a
b
c
");
        let range = whole(&d);
        let phantoms: HashMap<u32, u32> = [(1, 2)].into_iter().collect();
        assert_eq!(
            element_rows_before(&d, geom(80), &phantoms, range, 2),
            ElementRow(4),
            "line 2 starts after line 0, line 1's two phantoms and line 1"
        );
        assert_eq!(
            slice_from(&d, geom(80), &phantoms, range, ElementRow(0), 3),
            0..2,
            "three rows from the top are line 0 and line 1 with its phantoms"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Document, ElementLines, View};
    use std::path::PathBuf;

    fn buffer_with(s: &mut ServerState, path: &str, text: &str) -> BufferId {
        let id = s.allocate_buffer_id();
        s.insert_buffer_with_document(id, None, false, |d| {
            let mut doc = Document::new_at_path(d, PathBuf::from(path), None);
            doc.text = ropey::Rope::from_str(text);
            doc
        });
        id
    }

    /// A viewport over a view of one element per buffer, each windowing line 0 of it. The view is
    /// installed in `s`, since that is where the viewport's elements live.
    fn viewport_over(s: &mut ServerState, buffers: Vec<BufferId>) -> Viewport {
        let view_id = ViewId(buffers.first().copied().unwrap_or_default());
        let elements = buffers.len();
        s.views.insert(
            view_id,
            View {
                elements: buffers
                    .into_iter()
                    .map(|buffer_id| ElementBinding {
                        buffer_id,
                        lines: ElementLines::Range {
                            start: 0,
                            end_exclusive: 1,
                        },
                        decorations: None,
                        chrome_above: Default::default(),
                        laid_out_by: aether_protocol::ui::LayoutOwner::Server,
                    })
                    .collect(),
            },
        );
        Viewport {
            id: 1,
            view_id,
            client_id: uuid::Uuid::new_v4(),
            rows: 24,
            overscan_rows: 0,
            wrap: aether_protocol::viewport::WrapMode::None,
            tab_width: 4,
            diff_view: false,
            cols: 80,
            continuation_marker_width: 0,
            // Everything loaded; the render clips each slice to what the element has.
            loaded: vec![Some(0..u32::MAX); elements],
            anchor: ScrollPosition::default(),
            focused: 0,
        }
    }

    /// The elements of the view `vp` presents, to set a test's shape up.
    fn elements_mut<'a>(s: &'a mut ServerState, vp: &Viewport) -> &'a mut Vec<ElementBinding> {
        &mut s.views.get_mut(&vp.view_id).expect("installed").elements
    }

    /// Chrome belongs to the element it introduces, not to a line of some document.
    ///
    /// This view has **no generated document** — two plain buffers — yet composes with headings
    /// between them, which is exactly what a driver building elements over real files needs. The
    /// headings are in the tree whatever is loaded: chrome is a fact about the view's shape, and
    /// a client laying the view out from the tree needs every row of it accounted for. A partly
    /// loaded element says where its slice sits instead.
    #[test]
    fn chrome_travels_with_its_element_not_with_a_document() {
        use aether_protocol::ui::{Element, RailJoin};
        use aether_protocol::viewport::ChromeKind;

        let heading = |text: &str| {
            std::sync::Arc::new(vec![Element::Chrome {
                kind: ChromeKind::FileHeader,
                rail: RailJoin::Opens,
                children: vec![Element::text(text, Vec::new())],
            }])
        };

        let mut s = ServerState::new();
        let a = buffer_with(&mut s, "/a.txt", "alpha\nbravo\ncharlie\n");
        let b = buffer_with(&mut s, "/b.txt", "one\ntwo\nthree\n");
        let vp = viewport_over(&mut s, vec![a, b]);
        for (e, name) in elements_mut(&mut s, &vp).iter_mut().zip(["a.txt", "b.txt"]) {
            e.lines = ElementLines::Range {
                start: 0,
                end_exclusive: 3,
            };
            e.chrome_above = heading(name);
        }

        let render = |vp: &Viewport| render_window(&s, vp, SneakLabels::Hidden);

        let headings = |root: &Element| -> Vec<String> {
            let mut out = Vec::new();
            fn walk(n: &Element, out: &mut Vec<String>) {
                match n {
                    Element::Chrome { children, .. } => {
                        out.push(children.iter().map(Element::text_content).collect())
                    }
                    Element::Stack { children } => children.iter().for_each(|c| walk(c, out)),
                    _ => {}
                }
            }
            walk(root, &mut out);
            out
        };

        // Both elements loaded: both headings show, in order, with no document behind them.
        let window = render(&vp);
        assert_eq!(headings(&window.root), vec!["a.txt", "b.txt"]);

        // Part of the first loaded and none of the second: the headings are still there — the tree
        // is the whole view — and each editor says where its slice sits and how tall it is, which
        // is what the client places the rows it has against.
        let mut partial = vp.clone();
        partial.loaded = vec![Some(1..3), None];
        let window = render(&partial);
        assert_eq!(headings(&window.root), vec!["a.txt", "b.txt"]);
        let editors = window.root.editors();
        let Element::Editor {
            first_row,
            rows,
            lines,
            ..
        } = editors[0]
        else {
            panic!("an editor");
        };
        assert_eq!((*first_row, *rows, lines.len()), (ElementRow(1), 3, 2));
        let Element::Editor { rows, lines, .. } = editors[1] else {
            panic!("an editor");
        };
        assert_eq!(
            (*rows, lines.len()),
            (3, 0),
            "unloaded, but still three rows tall"
        );
    }

    /// A view's opinion about its element's lines overrides the buffer's opinion of itself.
    ///
    /// This is what lets a patch hunk window a real file: the lines shown are the file's, but the
    /// `+`/`−` on them is the *diff's* — which lines this commit added, and what it removed. The
    /// buffer's own diff against HEAD answers a different question, and for a historical commit a
    /// wrong one. Both elements here window the same buffer, so anything that differs between them
    /// can only have come from the view.
    ///
    /// The view's *marker* reaches the line whichever way the inline diff is set — it drives the
    /// always-on gutter — while its phantoms answer to the toggle, like an ordinary editor's.
    #[test]
    fn a_views_decorations_override_what_the_buffer_says_about_itself() {
        use aether_protocol::viewport::{BaselineRow, DiffMarker, DiffStage};

        let mut s = ServerState::new();
        let a = buffer_with(&mut s, "/a.txt", "alpha\nbravo\ncharlie\n");
        let vp = viewport_over(&mut s, vec![a, a]);
        for e in elements_mut(&mut s, &vp).iter_mut() {
            e.lines = ElementLines::Range {
                start: 0,
                end_exclusive: 3,
            };
        }

        let mut decorations = crate::state::ElementDecorations::default();
        decorations
            .markers
            .insert(1, (DiffMarker::Added, DiffStage::Unstaged));
        decorations.baseline_above.insert(
            1,
            vec![BaselineRow {
                text: "the line it replaced".into(),
                stage: DiffStage::Unstaged,
                emphasis: Vec::new(),
            }],
        );
        elements_mut(&mut s, &vp)[1].decorations = Some(std::sync::Arc::new(decorations));

        let line_1_of = |diff_view: bool, element: usize| -> LogicalLineRender {
            let mut vp = vp.clone();
            vp.diff_view = diff_view;
            let window = render_window(&s, &vp, SneakLabels::Hidden);
            match window.root.editors()[element] {
                Element::Editor { lines, .. } => lines
                    .iter()
                    .find(|l| l.logical_line == 1)
                    .expect("line 1 is in view")
                    .clone(),
                _ => unreachable!(),
            }
        };

        let undecorated = line_1_of(true, 0);
        assert_eq!(
            undecorated.change.marker(),
            None,
            "the buffer has no git state, so it says nothing about its own lines"
        );
        assert!(undecorated.baseline_above.is_empty());

        let decorated = line_1_of(true, 1);
        assert_eq!(
            decorated.change.marker(),
            Some(DiffMarker::Added),
            "the view's marker reaches the rendered line"
        );
        assert_eq!(
            decorated
                .baseline_above
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>(),
            vec!["the line it replaced"],
            "and its phantoms, which is where a patch's removed lines live"
        );

        // Diff off: the removed line collapses onto the marker, which is what the gutter draws as
        // `▔`. A supplied phantom is the diff view just as much as a buffer's own is.
        let collapsed = line_1_of(false, 1);
        assert_eq!(
            collapsed.change.marker(),
            Some(DiffMarker::Added),
            "the marker is ungated — the gutter has to be right either way"
        );
        assert!(
            collapsed.baseline_above.is_empty(),
            "but the removed line itself is only drawn while the diff view is on"
        );
    }

    /// The view acts on the **focused** element's buffer, not the first one's.
    ///
    /// Inert while every element windows one buffer — which is why storing focus earlier would have
    /// been meaningless — and the whole point once they don't: an edit, a search, a motion or an
    /// undo has to land in the buffer you are actually in, not in whichever file the patch happens
    /// to list first.
    #[test]
    fn the_views_buffer_follows_focus() {
        let mut s = ServerState::new();
        let a = buffer_with(&mut s, "/a.txt", "alpha\n");
        let b = buffer_with(&mut s, "/b.txt", "one\n");
        let mut vp = viewport_over(&mut s, vec![a, b]);

        assert_eq!(
            s.focused_buffer(&vp),
            a,
            "focus starts on the first element"
        );
        vp.focused = 1;
        assert_eq!(
            s.focused_buffer(&vp),
            b,
            "and moves the view's buffer with it"
        );

        // A stale id is not worth a panic: a view always has a first element to fall back on.
        vp.focused = 99;
        assert_eq!(s.focused_buffer(&vp), a);
    }

    /// A view whose elements window *different* buffers renders each from its own text.
    ///
    /// This is the property a patch needs in order to show real files rather than a generated copy
    /// of them, and it is the first case that cannot be satisfied by rendering one contiguous range
    /// of one document. Note both elements cover lines 0..3: logical line numbers collide across
    /// elements by design, which is why nothing may resolve a line without knowing whose it is.
    #[test]
    fn elements_render_from_their_own_buffers() {
        let mut s = ServerState::new();
        let a = buffer_with(&mut s, "/a.txt", "alpha\nbravo\ncharlie\n");
        let b = buffer_with(&mut s, "/b.txt", "one\ntwo\nthree-longest\n");
        let binding = |buffer_id| ElementBinding {
            buffer_id,
            lines: ElementLines::Range {
                start: 0,
                end_exclusive: 3,
            },
            decorations: None,
            chrome_above: Default::default(),
            laid_out_by: aether_protocol::ui::LayoutOwner::Server,
        };
        let vp = viewport_over(&mut s, vec![a, b]); // no generated view behind these plain buffers
        *elements_mut(&mut s, &vp) = vec![binding(a), binding(b)];

        let window = render_window(&s, &vp, SneakLabels::Hidden);

        let text_of = |line: &LogicalLineRender| -> String {
            line.visual_rows
                .iter()
                .flat_map(|r| r.segments.iter().map(|seg| seg.text.as_str()))
                .collect()
        };
        let editors: Vec<(BufferId, Vec<String>)> = window
            .root
            .editors()
            .into_iter()
            .filter_map(|n| match n {
                Element::Editor { buffer, lines, .. } => {
                    Some((*buffer, lines.iter().map(text_of).collect()))
                }
                _ => None,
            })
            .collect();

        assert_eq!(
            editors.len(),
            2,
            "each element should render as its own editor, got {editors:?}"
        );
        assert_eq!(editors[0].0, a, "the first element names buffer a");
        assert_eq!(editors[1].0, b, "the second element names buffer b");
        assert_eq!(editors[0].1, vec!["alpha", "bravo", "charlie"]);
        assert_eq!(
            editors[1].1,
            vec!["one", "two", "three-longest"],
            "the second element must render b's text, not a's"
        );

        // View-level geometry is a sum over the elements, not a reading of the first one: each
        // editor carries its own height, and the widest line is taken over both — a width of 7
        // from "charlie" if only element 0 is consulted.
        let heights: Vec<u32> = window
            .root
            .editors()
            .iter()
            .filter_map(|n| match n {
                Element::Editor { rows, .. } => Some(*rows),
                _ => None,
            })
            .collect();
        assert_eq!(heights, vec![3, 3], "every element reports its own rows");
        assert_eq!(
            window.max_line_width, 13,
            "the widest line in the view lives in the second element"
        );
    }
}

/// `view/save`: write every document the view's elements window.
///
/// A loop over [`view_element_buffers`] calling `buffer/save`, so every rule that path enforces —
/// read-only refusal, external-change detection, the change notifications — holds per document
/// without being restated here. Clean and read-only documents are skipped rather than refused: a
/// working-changes view windows the files you edited *and* the ones you only looked at, and a
/// commit's patch windows nothing writable at all.
///
/// The first document needing confirmation aborts with its own error code. Ones already written
/// stay written and are clean, so the client's existing confirm-and-retry lands on the rest.
pub async fn view_save(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::viewport::ViewSaveParams,
) -> Result<aether_protocol::viewport::ViewSaveResult, RpcError> {
    let client_id = ctx.client_id;
    let (buffers, focused_buffer) = {
        let s = state.lock().await;
        let focused = s
            .viewports
            .values()
            .find(|vp| vp.client_id == client_id && vp.view_id == params.view_id)
            .map(|vp| s.focused_buffer(vp))
            .unwrap_or_else(|| params.view_id.presenting_buffer());
        let dirty: Vec<BufferId> = view_element_buffers(&s, Some(params.view_id), focused)
            .into_iter()
            .filter(|id| s.try_doc_of(*id).is_some_and(|d| d.dirty && !d.read_only()))
            .collect();
        (dirty, focused)
    };

    let mut saved = 0;
    let mut focused_result = None;
    for buffer_id in buffers {
        let result = crate::handlers::buffer_save(
            state,
            ctx,
            aether_protocol::buffer::BufferSaveParams {
                buffer_id,
                path_index: None,
                relative_path: None,
                overwrite: params.overwrite,
            },
        )
        .await?;
        saved += 1;
        if buffer_id == focused_buffer {
            focused_result = Some(result);
        }
    }
    Ok(aether_protocol::viewport::ViewSaveResult {
        saved,
        focused: focused_result,
    })
}
