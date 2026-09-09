//! `viewport/*` — subscribe, scroll, resize, wrap, and the window rendering behind them.
//!
//! Also owns the per-line decoration that feeds a window: diff markers and phantom deleted rows,
//! intra-line emphasis, conflict bands, and the git-hunk recomputation they read from. Those live
//! here rather than with the git handlers because they exist to build a `LogicalLineRender`, not to
//! operate on a repository.

use super::*;
use aether_protocol::coords::{ViewLine, VisualRow};

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

    s.try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let elements = element_bindings(&s, buffer_id, params.cols, params.continuation_marker_width);
    let line_count = ViewLayout::of(&elements, |id| s.doc_of(id).line_count()).line_count();

    let (first, last_excl) = pushed_range(
        params.scroll.logical_line,
        params.rows,
        params.overscan_rows,
        line_count,
    );
    let elements = element_bindings(&s, buffer_id, params.cols, params.continuation_marker_width);
    // Which element the cursor is in. Element 0 for an ordinary view, whose one element *is* the
    // buffer — but a composed view is scrolled to somewhere on purpose (a patch opened on the file
    // you picked, a session restored where you left it), and the element holding that line is the
    // one you are looking at. Deriving it from the scroll is what lets `focus_path` keep working
    // without a second coordinate for it: the caller already said where to open.
    //
    // Computed *before* the render and the status snapshot because both describe the focused
    // element, not the view's own document — see `focus_buffer`.
    let focused = ViewLayout::of(&elements, |id| s.doc_of(id).line_count())
        .element_at(params.scroll.logical_line)
        .unwrap_or(0);
    // The buffer every *buffer-level* answer below is about. For an ordinary view this is
    // `buffer_id` and nothing changes; for a composed one the view's own document is a generated
    // patch, which has no outline, no diagnostics, no language server and no meaningful
    // on-disk state — so seeding any of them from it answered empty for the file you are looking at.
    let focus_buffer = elements
        .get(focused as usize)
        .map_or(buffer_id, |e| e.buffer_id);
    // Gutter markers ride `hunks` regardless of the diff toggle; the inline view honours the
    // client's sticky setting. Hunks are seeded on open (`load_baseline`) and kept fresh per edit,
    // so they're accurate here without the recompute `git_set_diff_view` does.
    let window = render_window(
        &s,
        client_id,
        // Subscribing: the view is the buffer being subscribed to.
        params.buffer_id,
        &elements,
        focused,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap: params.wrap,
            cols: params.cols,
            marker_width: params.continuation_marker_width,
            tab_width: params.tab_width,
        },
        params.rows,
        params.diff_view,
        SneakLabels::Shown,
    );

    let viewport_id = s.allocate_viewport_id();
    let viewport = Viewport {
        id: viewport_id,
        view_id: params.buffer_id,
        focused,
        client_id,
        rows: params.rows,
        overscan_rows: params.overscan_rows,
        scroll_view_line: params.scroll.logical_line,
        scroll_sub_row: params.scroll.sub_row,
        wrap: params.wrap,
        tab_width: params.tab_width,
        diff_view: params.diff_view,
        first_view_line: first,
        last_view_line_exclusive: last_excl,
        elements: element_bindings(&s, buffer_id, params.cols, params.continuation_marker_width),
    };
    s.viewports.insert(viewport_id, viewport);
    s.last_scroll.insert((client_id, buffer_id), params.scroll);
    tracing::debug!(%client_id, viewport_id, buffer_id, first = first.get(), last_excl = last_excl.get(), "viewport subscribed");

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
                for shown in v.shown_buffers() {
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

    // What the subscriber can't work out for itself: which element holds the cursor, and the buffer
    // it windows. Only said when it differs from what was subscribed to — a composed view — because
    // that is the case where a client holding the subscribed buffer holds the wrong one.
    //
    // Ahead of the status snapshot because it *seats the cursor* inside the element, and the
    // breadcrumb is taken from wherever the cursor ends up. Built the other way round, a composed
    // view's first breadcrumb described the position the cursor was seated away from.
    let focus = focus_answer(&mut s, client_id, viewport_id)?;
    // Snapshot the buffer-level status the client can't derive from the window: external-change
    // flags, diagnostic counts, and language-server health. These otherwise only reach a client via
    // change-notifications (`buffer/state`, `lsp/diagnostics_changed`, `lsp/status_changed`), so a
    // viewport that subscribes *after* the relevant change already happened would show stale state
    // until the next change. Returning it in the response (vs a follow-up push) keeps it atomic with
    // the window and free of any ordering race against the client's editor switch.
    //
    // Every field is a fact about the buffer under the cursor, so all of them ask `focus_buffer`
    // rather than the view's own document. For an ordinary view the two are the same id. For a
    // composed one the view's document is a generated patch: it has no outline, no diagnostics, no
    // language server and no on-disk state, so asking it answered empty for all four — a blank
    // breadcrumb and zeroed diagnostic counts on every `Space g w`, until the cursor moved and the
    // follow loop pushed the real ones. A composed view has already computed exactly this snapshot
    // inside `focus`, so reuse it rather than paying for the outline walk twice.
    let buffer_status = match &focus {
        Some(f) => f.buffer_status.clone(),
        None => buffer_status_for(&mut s, client_id, focus_buffer),
    };
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
    for element in vp.elements.iter_mut() {
        element.cols = params.cols;
    }
    vp.rows = params.rows;
    let elements = vp.elements.clone();
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line, diff_view) = (
        vp.focus().cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.focus().continuation_marker_width,
        vp.tab_width,
        vp.buffer_id(),
        vp.scroll_view_line,
        vp.diff_view,
    );

    s.try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = ViewLayout::of(&elements, |id| s.doc_of(id).line_count()).line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let window = render_window(
        &s,
        client_id,
        view_id_of(&s, params.viewport_id, buffer_id),
        &elements,
        focused_of(&s, params.viewport_id),
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        diff_view,
        SneakLabels::Shown,
    );

    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.first_view_line = first;
    vp.last_view_line_exclusive = last_excl;
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
    let cursor = match s.cursors.get(&key).copied() {
        Some(c) if scope.contains(c.position) => c,
        // Clamped, because an element's extent is a claim about its buffer that a concurrent edit
        // could have outrun.
        _ => {
            let position = motion::clamp_position(
                s.doc_of(buffer_id),
                aether_protocol::LogicalPosition {
                    line: start_line,
                    col: 0,
                },
            );
            CursorState {
                position,
                anchor: position,
                match_bracket: None,
                jumplist_position: None,
            }
        }
    };
    set_cursor(s, key, cursor);
    Ok(cursor)
}

/// The focus a subscribe answers with: which element holds the cursor and the buffer it windows,
/// with the cursor seated inside that element.
///
/// `None` for an ordinary view — one element, windowing the buffer that was subscribed to — where
/// the subscriber already knows everything this would say.
///
/// Seating the cursor is the same rule [`viewport_focus_element`] follows, and for the same reason:
/// an element's window onto a file is what the view shows of it, and a cursor outside that is a
/// cursor nothing draws. It keeps a cursor already inside, so re-subscribing (a resize, a wrap
/// toggle) doesn't move it — and cursors are per `(client, buffer)`, so a needless move would also
/// yank an ordinary view of the same file.
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

fn focus_answer(
    s: &mut ServerState,
    client_id: ClientId,
    viewport_id: aether_protocol::ViewportId,
) -> Result<Option<aether_protocol::viewport::ViewportFocusElementResult>, RpcError> {
    let Some(vp) = s.viewports.get(&viewport_id) else {
        return Ok(None);
    };
    let (view_id, element, binding) = (vp.view_id, vp.focused, vp.focus().clone());
    if binding.buffer_id == view_id.presenting_buffer() {
        return Ok(None);
    }
    let cursor = seat_cursor_in_element(s, client_id, binding.buffer_id, binding.start_line)?;
    Ok(Some(
        aether_protocol::viewport::ViewportFocusElementResult {
            element,
            buffer: describe_buffer(s, binding.buffer_id, cursor)?,
            // After the seating above, never before it: seating is what decides where the cursor
            // is, and the breadcrumb is taken from the cursor.
            buffer_status: buffer_status_for(s, client_id, binding.buffer_id),
        },
    ))
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
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;

    let last = vp.elements.len().saturating_sub(1) as u32;
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
    vp.focused = focused;
    let binding = vp.focus();
    let (buffer_id, start_line) = (binding.buffer_id, binding.start_line);

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
/// line through a rewritten paragraph. Elements the view has no opinion about contribute nothing,
/// which is what keeps a placeholder for a binary file out of the walk.
fn change_anchors(
    s: &ServerState,
    vp: &Viewport,
) -> Vec<(aether_protocol::viewport::FieldId, u32)> {
    let mut out = Vec::new();
    for (idx, binding) in vp.elements.iter().enumerate() {
        let Some(decorations) = binding.decorations.as_deref() else {
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
    if !out.is_empty() {
        return out;
    }
    // No element carries the view's opinion — a patch the driver has not built, whose elements are
    // all slices of one generated document. Its change blocks are recorded in the index instead,
    // and each belongs to whichever element contains its line.
    let buffer_id = vp.buffer_id();
    let Some(generated) = s.doc_of(buffer_id).generated.as_ref() else {
        return out;
    };
    let element_of = |line: u32| {
        vp.elements
            .iter()
            .rposition(|e| e.start_line <= line)
            .unwrap_or(0) as aether_protocol::viewport::FieldId
    };
    for file in &generated.index.files {
        for change in &file.changes {
            out.push((element_of(change.start_line), change.start_line));
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
    let view_buffer = view.presenting_buffer();
    let mut out: Vec<BufferId> = Vec::new();
    for layout in s.element_layout_of(view_buffer) {
        let id = layout.extent.buffer(view_buffer);
        if !out.contains(&id) {
            out.push(id);
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
) -> Option<aether_protocol::viewport::ViewSeat> {
    let vp = s
        .viewports
        .values()
        .find(|vp| vp.client_id == client_id && view_key_of(s, vp.view_id) == Some(view_key.into()))?;
    seat_in(s, vp.view_id.presenting_buffer(), &vp.elements, identity, line)
}

/// The [`crate::state::VirtualSource::key`] of a view, when it is a materialised one.
pub fn view_key_of(s: &ServerState, view: ViewId) -> Option<String> {
    s.try_doc_of(view.presenting_buffer())?
        .virtual_source
        .as_ref()
        .map(|src| src.target.key())
}

/// Which element of an already-built element list holds `(abs_path, line)`.
///
/// Addressed by **path**, not by the buffer id an entry was captured against: a view's element
/// buffers are transient, so by the time anything jumps back the same file may be a different
/// buffer — or the id may name nothing at all.
pub fn seat_in(
    s: &ServerState,
    view_buffer: BufferId,
    elements: &[crate::state::ElementBinding],
    identity: &str,
    line: u32,
) -> Option<aether_protocol::viewport::ViewSeat> {
    view_outline_of(s, view_buffer, elements)
        .into_iter()
        .find_map(|e| {
            if e.line != line {
                return None;
            }
            let buffer_id = elements.get(e.element as usize).map(|b| b.buffer_id)?;
            (buffer_identity(s, buffer_id).as_deref() == Some(identity)).then_some(
                aether_protocol::viewport::ViewSeat {
                    element: e.element,
                    buffer_id,
                },
            )
        })
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

/// The **view line** an element's buffer line sits at, in a freshly materialised view.
///
/// What a reopen has to be scrolled to. A subscribe derives its focused element from the line the
/// view is scrolled to (see [`viewport_subscribe`]), so a reopen framed anywhere else focuses
/// whatever element covers *that* line — undoing the focus the jump asked for, and leaving the
/// caller looking at the right view with the wrong editor active.
pub fn view_line_in_fresh_view(
    s: &ServerState,
    view_buffer: BufferId,
    element: aether_protocol::viewport::FieldId,
    line: u32,
) -> Option<ViewLine> {
    let elements = element_bindings(s, view_buffer, 120, 0);
    crate::state::ViewLayout::of(&elements, |id| s.doc_of(id).line_count()).to_view(element, line)
}

/// Where `(abs_path, line)` sits in a **freshly materialised** view — one no viewport exists for
/// yet, because the jump had to reopen it.
///
/// `cols` is nominal: it decides how the elements *wrap*, never how many there are, and a `FieldId`
/// indexes that list. So the element named here is the one the client's own subscribe will produce.
pub fn seat_in_fresh_view(
    s: &ServerState,
    view_buffer: BufferId,
    identity: &str,
    line: u32,
) -> Option<aether_protocol::viewport::ViewSeat> {
    let elements = element_bindings(s, view_buffer, 120, 0);
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
    view_outline_of(s, vp.view_id.presenting_buffer(), &vp.elements)
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
    let Some(generated) = s.doc_of(view_buffer).generated.as_ref() else {
        return Vec::new();
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
        let spans: Vec<(u32, u32, &str)> = if file.hunks.is_empty() {
            vec![(file.start_line, file.end_line, "")]
        } else {
            file.hunks
                .iter()
                .map(|h| (h.start_line, h.end_line, h.signature.as_str()))
                .collect()
        };
        for (start, end, signature) in spans {
            // The hunk's **top**, context included — not its first change.
            //
            // `o` and the picker land in the same place because they read this one number, and the
            // top is the right one: it is where the hunk's heading is, so you arrive at the change
            // *with the context that makes it readable* rather than partway into it. It also makes
            // "which entry is the cursor in" answerable for the context lines themselves, which
            // otherwise resolved to the hunk above. `c`/`Alt-c` still land on the changes.
            let anchor = start;
            let _ = end;
            let element = region_of(anchor);
            let bound = elements
                .get(element as usize)
                .is_some_and(|e| e.buffer_id != view_buffer);
            // A bound element windows the real file, so the entry's line must be a *file* line —
            // the index is what maps between the two spaces. An unbound one windows the generated
            // text itself, where the patch line already is the buffer line.
            let line = if bound {
                let mut found = None;
                for i in anchor..end.max(anchor + 1) {
                    if let Some(Some(info)) = generated.index.lines.get(i as usize) {
                        if let Some(n) = info.new_lineno {
                            found = Some(n.saturating_sub(1));
                            break;
                        }
                    }
                }
                // A pure removal has no new-side line of its own; it sits above whatever survived,
                // which is where the element's own start is.
                found.unwrap_or_else(|| elements.get(element as usize).map_or(0, |e| e.start_line))
            } else {
                anchor
            };
            out.push(OutlineEntry {
                element,
                line,
                file: file.path().to_string(),
                label: signature.to_string(),
                patch_line: anchor,
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
        .find(|v| v.client_id == client_id && v.binds(buffer_id))?;
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
        .or_else(|| entries.iter().enumerate().find(|(_, e)| e.element == focused))
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
    use aether_protocol::viewport::FocusStep;

    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    // Check ownership up front, then read: the anchors need the state alongside the viewport.
    require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    let vp = &s.viewports[&params.viewport_id];
    let anchors = match params.grain {
        aether_protocol::viewport::NavigateGrain::Change => change_anchors(&s, vp),
        // One stop per **outline entry** — which is one per change, since that is what the outline's
        // rows are. Same source as the picker, so `o` and `Space o` cannot disagree about the stops
        // or their order.
        aether_protocol::viewport::NavigateGrain::Outline => view_outline(&s, vp)
            .into_iter()
            .map(|e| (e.element, e.line))
            .collect(),
    };
    let (focused, here) = (vp.focused, vp.buffer_id());
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
    let target = match params.direction {
        FocusStep::Next => anchors
            .iter()
            .position(|&a| a > (focused, from))
            .and_then(|i| (i + count - 1 <= last).then_some(i + count - 1)),
        FocusStep::Previous => anchors
            .iter()
            .rposition(|&a| a < (focused, from))
            .and_then(|i| i.checked_sub(count - 1)),
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

    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.focused = element;
    let buffer_id = vp.focus().buffer_id;
    let cursor = {
        let doc = s.doc_of(buffer_id);
        let position =
            motion::clamp_position(doc, aether_protocol::LogicalPosition { line, col: 0 });
        CursorState {
            position,
            anchor: position,
            match_bracket: None,
            jumplist_position: None,
        }
    };
    set_cursor(&mut s, (client_id, buffer_id), cursor);
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

pub async fn viewport_scroll_to_row(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::viewport::ViewportScrollToRowParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    let elements = vp.elements.clone();
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, diff_view) = (
        vp.focus().cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.focus().continuation_marker_width,
        vp.tab_width,
        vp.buffer_id(),
        vp.diff_view,
    );
    s.try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let layout = ViewLayout::of(&elements, |id| s.doc_of(id).line_count());
    let line_count = layout.line_count();
    // Row-count use only — no emphasis needed to resolve a visual row to a line.
    let phantom_rows: Vec<HashMap<u32, u32>> = elements
        .iter()
        .map(|binding| element_phantom_rows(&s, binding, diff_view, None))
        .collect();
    let geom = wrap::WrapGeometry {
        wrap,
        cols,
        marker_width,
        tab_width,
    };
    let top_line = view_line_at_visual_row(
        &s,
        &elements,
        &layout,
        params.top_visual_row,
        geom,
        &phantom_rows,
    );
    let (first, last_excl) = pushed_range(top_line, rows, overscan, line_count);
    let window = render_window(
        &s,
        client_id,
        view_id_of(&s, params.viewport_id, buffer_id),
        &elements,
        focused_of(&s, params.viewport_id),
        first,
        last_excl,
        geom,
        rows,
        diff_view,
        SneakLabels::Shown,
    );
    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.scroll_view_line = top_line;
    vp.scroll_sub_row = 0.0;
    vp.first_view_line = first;
    vp.last_view_line_exclusive = last_excl;
    // Persist the new top so a buffer switch restores it — mirrors `viewport_scroll`. Without this
    // the restore map only ever held the initial subscribe position, so switching back to a buffer
    // jumped to where it was first opened rather than where it was left.
    s.last_scroll.insert(
        (client_id, buffer_id),
        ScrollPosition {
            logical_line: top_line,
            sub_row: 0.0,
        },
    );
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
    let elements = vp.elements.clone();
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line, diff_view) = (
        vp.focus().cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.focus().continuation_marker_width,
        vp.tab_width,
        vp.buffer_id(),
        vp.scroll_view_line,
        vp.diff_view,
    );

    s.try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = ViewLayout::of(&elements, |id| s.doc_of(id).line_count()).line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let window = render_window(
        &s,
        client_id,
        view_id_of(&s, params.viewport_id, buffer_id),
        &elements,
        focused_of(&s, params.viewport_id),
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        diff_view,
        SneakLabels::Shown,
    );

    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.first_view_line = first;
    vp.last_view_line_exclusive = last_excl;
    Ok(ViewportWindowResult { window })
}

pub async fn viewport_scroll(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportScrollParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.scroll_view_line = params.scroll.logical_line;
    vp.scroll_sub_row = params.scroll.sub_row;
    let elements = vp.elements.clone();
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line, diff_view) = (
        vp.focus().cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.focus().continuation_marker_width,
        vp.tab_width,
        vp.buffer_id(),
        vp.scroll_view_line,
        vp.diff_view,
    );

    s.try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = ViewLayout::of(&elements, |id| s.doc_of(id).line_count()).line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let window = render_window(
        &s,
        client_id,
        view_id_of(&s, params.viewport_id, buffer_id),
        &elements,
        focused_of(&s, params.viewport_id),
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        diff_view,
        SneakLabels::Shown,
    );

    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.first_view_line = first;
    vp.last_view_line_exclusive = last_excl;
    s.last_scroll.insert((client_id, buffer_id), params.scroll);
    Ok(ViewportWindowResult { window })
}

/// `view/window_at_cursor`: a window containing this client's cursor in the focused element.
///
/// The answer to a question the client cannot phrase itself — see
/// [`aether_protocol::viewport::ViewportWindowAtCursor`]. Both halves live here: which line the
/// cursor is on, and where that line sits in the view's own coordinates.
///
/// The cursor lands a third of a screen down rather than at the top, so a reveal that follows has
/// context on both sides and does not have to scroll again. The client still positions itself
/// against the window it gets back; this only decides which slice to send.
pub async fn viewport_window_at_cursor(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::viewport::ViewportWindowAtCursorParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    let (elements, element, buffer_id) = (vp.elements.clone(), vp.focused, vp.buffer_id());
    let (cols, rows, overscan, wrap, marker_width, tab_width, diff_view) = (
        vp.focus().cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.focus().continuation_marker_width,
        vp.tab_width,
        vp.diff_view,
    );
    s.try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;

    let layout = ViewLayout::of(&elements, |id| s.doc_of(id).line_count());
    let line_count = layout.line_count();
    // The cursor's line in the *view's* coordinates. `None` when the element's extent has moved out
    // from under it (a rebuild between the ask and the answer), where the element's own start is the
    // honest fallback — the same one every stale-extent path takes.
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    let at = layout
        .to_view(element, cursor.position.line)
        .or_else(|| layout.span_of(element).map(|(start, _)| start))
        .unwrap_or(ViewLine::ZERO);
    // Sit the cursor a third of a screen down, so a window fetched for it carries context above as
    // well as below: a placement or a minimal reveal against it then has somewhere to go.
    let top = ViewLine(at.get().saturating_sub(rows / 3));

    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.scroll_view_line = top;
    vp.scroll_sub_row = 0.0;
    let (first, last_excl) = pushed_range(top, rows, overscan, line_count);
    let window = render_window(
        &s,
        client_id,
        view_id_of(&s, params.viewport_id, buffer_id),
        &elements,
        focused_of(&s, params.viewport_id),
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        diff_view,
        SneakLabels::Shown,
    );
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    vp.first_view_line = first;
    vp.last_view_line_exclusive = last_excl;
    s.last_scroll.insert(
        (client_id, buffer_id),
        ScrollPosition {
            logical_line: top,
            sub_row: 0.0,
        },
    );
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

/// Compute the **view**-line range to push for a viewport. Each line wraps to >= 1 visual row, so
/// sending `rows + 2*overscan_rows` lines is a safe over-approximation of the visible + overscan
/// area. A scroll line past the end of the view (a stale restore, or a shrink under the viewport)
/// anchors to the last line rather than collapsing to an empty range past the end.
///
/// `view_lines` is the **view's** line count — its elements' extents summed, from
/// [`ViewLayout::line_count`] — not any document's. Ranging against a document is what let the
/// scroll run past a bound patch's real end and blank the screen.
pub fn pushed_range(
    scroll_line: ViewLine,
    rows: u32,
    overscan: u32,
    view_lines: u32,
) -> (ViewLine, ViewLine) {
    let scroll_line = scroll_line.min(ViewLine::last_of(view_lines));
    let first = scroll_line.saturating_sub(overscan);
    let last_excl = scroll_line
        .saturating_add(rows)
        .saturating_add(overscan)
        .min(ViewLine(view_lines));
    (first, last_excl.max(first))
}

/// Recompute every affected viewport's pushed range from `pushed_range` against its view's new
/// length. Call **before** building `viewport/lines_changed` notifications after any mutation that
/// may grow or shrink the buffer — otherwise a growth (e.g. undoing a join) leaves the viewport's
/// range clamped to the smaller post-mutation size and the freshly restored lines never reach the
/// client.
///
/// Each viewport is measured by **its own view**, not by the edited document: an edit to one file
/// changes the length of every view windowing it, and for a patch that length is the sum of its
/// elements' extents rather than any one file's line count. Taking the document's count was right
/// only while a view was one whole buffer.
pub fn refresh_viewport_ranges_for_buffer(s: &mut ServerState, buffer_id: BufferId) {
    // First: a view's elements window *slices* of their buffers, and an edit that changed the line
    // count moved those slices. Consumed here, once, rather than threaded through the dozen paths
    // that reach this function.
    if let Some(shift) = s
        .try_doc_of_mut(buffer_id)
        .and_then(|d| d.last_shift.take())
    {
        s.shift_element_extents(buffer_id, shift);
    }
    // Every viewport on any buffer of the document: a mutation through one workspace's buffer
    // moves the shared content under every sibling's viewports too.
    let attached = s.doc_siblings(buffer_id);
    // Measured up front: laying a view out reads the documents, and the loop below needs the
    // viewports mutably.
    let view_lines: Vec<(aether_protocol::ViewportId, u32)> = s
        .viewports
        .values()
        .filter(|vp| attached.iter().any(|id| vp.binds(*id)))
        .map(|vp| {
            let layout = ViewLayout::of(&vp.elements, |id| s.doc_of(id).line_count());
            (vp.id, layout.line_count())
        })
        .collect();
    for (viewport_id, view_lines) in view_lines {
        let Some(vp) = s.viewports.get_mut(&viewport_id) else {
            continue;
        };
        let max_line = ViewLine::last_of(view_lines);
        // A shrink can leave the viewport scrolled past the end (a watcher reload of a rewritten
        // file, an undo, another client's delete). Clamp the stored scroll like reload clamps
        // cursors — otherwise the pushed range is empty and the client shows a blank buffer.
        // The restore map gets the same clamp so a buffer switch doesn't resurrect the stale
        // position.
        let clamped = (vp.scroll_view_line > max_line).then(|| {
            vp.scroll_view_line = max_line;
            vp.scroll_sub_row = 0.0;
            (vp.client_id, vp.buffer_id())
        });
        let (first, last_excl) =
            pushed_range(vp.scroll_view_line, vp.rows, vp.overscan_rows, view_lines);
        vp.first_view_line = first;
        vp.last_view_line_exclusive = last_excl;
        if let Some(key) = clamped {
            s.last_scroll.insert(
                key,
                ScrollPosition {
                    logical_line: max_line,
                    sub_row: 0.0,
                },
            );
        }
    }
    for id in attached {
        recompute_diff_hunks_if_viewed(s, id);
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
    let viewed = s.viewports.values().any(|vp| vp.binds(buffer_id));
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
fn line_visual_rows(
    buf: &Document,
    line_idx: u32,
    no_wrap: bool,
    cols: u32,
    marker_width: u32,
    tab_width: u32,
) -> u32 {
    if no_wrap {
        return 1;
    }
    let mut text: String = buf.text.line(line_idx as usize).chunks().collect();
    if text.ends_with('\n') {
        text.pop();
    }
    wrap::compute_rows(&text, cols, marker_width, tab_width).len() as u32
}

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
    cols: u32,
    wrap: aether_protocol::viewport::WrapMode,
    marker_width: u32,
    tab_width: u32,
    extra_rows: &HashMap<u32, u32>,
) -> u32 {
    let no_wrap = matches!(wrap, aether_protocol::viewport::WrapMode::None);
    if no_wrap && extra_rows.is_empty() {
        return range.len();
    }
    range.lines().fold(0u32, |total, i| {
        total.saturating_add(
            line_visual_rows(buf, i, no_wrap, cols, marker_width, tab_width)
                + extra_rows.get(&i).copied().unwrap_or(0),
        )
    })
}

/// Display width (cols) of the widest line in the buffer — sizes a client's native horizontal
/// scroller under no-wrap. O(buffer chars); only called when wrap is off.
fn compute_max_line_width(buf: &Document, tab_width: u32) -> u32 {
    let mut max = 0u32;
    for i in 0..buf.line_count() {
        let mut text: String = buf.text.line(i as usize).chunks().collect();
        if text.ends_with('\n') {
            text.pop();
        }
        let mut col = 0u32;
        for c in text.chars() {
            col += wrap::char_display_width(c, col, tab_width);
        }
        max = max.max(col);
    }
    max
}

/// The **buffer** line whose visual-row span contains `target_row`, counting from line `from` and
/// stopping at `to_excl` (clamped to the last line in range).
///
/// Ranged rather than whole-buffer because an element windows a slice of its file: rows are counted
/// from the element's own first line, not the document's. A whole-buffer view passes the element's
/// whole range and gets exactly what it always did. The range is a [`BufferRange`] for the reason
/// [`element_visual_rows`]'s is.
#[allow(clippy::too_many_arguments)]
pub fn logical_line_at_visual_row(
    buf: &Document,
    cols: u32,
    wrap: aether_protocol::viewport::WrapMode,
    marker_width: u32,
    tab_width: u32,
    extra_rows: &HashMap<u32, u32>,
    range: BufferRange,
    target_row: u32,
) -> u32 {
    let (from, to_excl) = (range.start(), range.end_exclusive());
    if range.is_empty() {
        return from;
    }
    let last = to_excl - 1;
    let no_wrap = matches!(wrap, aether_protocol::viewport::WrapMode::None);
    if no_wrap && extra_rows.is_empty() {
        return from.saturating_add(target_row).min(last);
    }
    let mut acc = 0u32;
    for i in range.lines() {
        let virtual_n = extra_rows.get(&i).copied().unwrap_or(0);
        let n = line_visual_rows(buf, i, no_wrap, cols, marker_width, tab_width) + virtual_n;
        if acc + n > target_row {
            return i;
        }
        acc += n;
    }
    last
}

/// Whether an edit to lines `first..last_excl` of any of `buffers` touches the slice of `vp`
/// currently on screen.
///
/// The two ranges are in different spaces — the edit's is buffer lines, the viewport's pushed range
/// is view lines — so they cannot simply be overlapped. They were, for as long as a view was one
/// whole buffer and the two happened to coincide; in a patch they are unrelated, and the test would
/// answer about lines the view has never heard of. Each element windowing one of those buffers maps
/// the edit into view space, and any overlap there is a real one.
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
    let layout = ViewLayout::of(&vp.elements, |id| s.doc_of(id).line_count());
    vp.elements
        .iter()
        .enumerate()
        .filter(|(_, binding)| buffers.contains(&binding.buffer_id))
        .any(|(idx, _)| {
            let element = idx as aether_protocol::viewport::FieldId;
            // The edit's own last line may sit past this element; `to_view` answers `None` there, so
            // walk in from whichever end lands inside it.
            let start = (first..last_excl).find_map(|line| layout.to_view(element, line));
            let end = (first..last_excl)
                .rev()
                .find_map(|line| layout.to_view(element, line));
            match (start, end) {
                (Some(start), Some(end)) => {
                    start < vp.last_view_line_exclusive && vp.first_view_line <= end
                }
                _ => false,
            }
        })
}

/// The **view** line whose block contains absolute visual `row` — the inverse of the row arithmetic
/// a client does when it maps `scrollTop / line_height` to a scroll request.
///
/// Walks the view element by element, chrome included, because a row belongs to the view rather
/// than to any one buffer. Asking the primary element's document instead was right only while a view
/// was one whole buffer, and it is the shape of mistake this module now makes hard to write.
fn view_line_at_visual_row(
    s: &ServerState,
    elements: &[ElementBinding],
    layout: &ViewLayout,
    row: VisualRow,
    geom: wrap::WrapGeometry,
    // Per element, like every other row count: an element's phantom rows come from what the *view*
    // says about its lines. One map for the whole view is one buffer's opinion applied to every
    // element — for a patch, line numbers from a document none of them windows.
    phantom_rows: &[HashMap<u32, u32>],
) -> ViewLine {
    let mut remaining = row.get();
    for (idx, binding) in elements.iter().enumerate() {
        let extra_rows = &phantom_rows[idx];
        let element = idx as aether_protocol::viewport::FieldId;
        let Some((view_start, view_end)) = layout.span_of(element) else {
            break;
        };
        // Chrome sits above the element and holds no line: a row inside it belongs to the element
        // it introduces.
        let chrome = binding.chrome_above.len() as u32;
        if remaining < chrome {
            return view_start;
        }
        remaining -= chrome;
        let doc = s.doc_of(binding.buffer_id);
        let range = layout
            .intersect(element, view_start, view_end)
            .unwrap_or_else(|| layout.element_range(element));
        let height = element_visual_rows(
            doc,
            range,
            binding.cols,
            geom.wrap,
            binding.continuation_marker_width,
            geom.tab_width,
            extra_rows,
        );
        if remaining < height {
            let line = logical_line_at_visual_row(
                doc,
                binding.cols,
                geom.wrap,
                binding.continuation_marker_width,
                geom.tab_width,
                extra_rows,
                range,
                remaining,
            );
            return layout.to_view(element, line).unwrap_or(view_start);
        }
        remaining -= height;
    }
    ViewLine::last_of(layout.line_count())
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
    wrap: aether_protocol::viewport::WrapMode,
    tab_width: u32,
    diff_view: bool,
    sneak_labels: SneakLabels,
) -> Vec<LogicalLineRender> {
    let buffer_id = binding.buffer_id;
    let buf = s.doc_of(buffer_id);
    let cols = binding.cols;
    let marker_width = binding.continuation_marker_width;
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
    /// Chrome introducing this element, and whether the element's own lines are in this window —
    /// which is what decides whether the chrome is drawn with them. See `compose_tree`.
    chrome_above: std::sync::Arc<Vec<Element>>,
    opens_in_view: bool,
    first_buffer_line: u32,
    lines: Vec<LogicalLineRender>,
}

#[allow(clippy::too_many_arguments)]
pub fn render_window(
    s: &ServerState,
    client_id: ClientId,
    // The **view's** own buffer — the thing being presented, which for a patch is the generated
    // document even when every element windows a real file. Its trailing chrome (the rule closing
    // the patch) belongs to the view rather than to any element, so it can only be found here:
    // looking on the primary *element's* buffer answered `None` the moment the elements stopped
    // being that document, and the closing rule silently stopped rendering.
    view_id: ViewId,
    elements: &[ElementBinding],
    // Which element holds the cursor. The status bar's git cluster is about *that* element's
    // buffer: it sits beside a label naming the focused file, so reading it off element 0 put a
    // different file's change counts next to that name in any multi-file view.
    focused: aether_protocol::ui::FieldId,
    first: ViewLine,
    last_excl: ViewLine,
    geom: wrap::WrapGeometry,
    viewport_rows: u32,
    diff_view: bool,
    sneak_labels: SneakLabels,
) -> Window {
    // The view's shape, and the only thing here that crosses between view lines and buffer lines.
    let layout = ViewLayout::of(elements, |id| s.doc_of(id).line_count());
    // Whole-view geometry that is still a one-buffer question — the primary element's own diff, and
    // the scroll coordinate's visual extent. Both are correct exactly while the view *is* that
    // buffer, which `bound` below is the test for.
    let primary = elements
        .first()
        .expect("a viewport always has at least one element");
    let buf = s.doc_of(primary.buffer_id);
    // `cols` and `marker_width` are per-*element* now (each binding carries its own), so only the
    // two genuinely view-wide settings are unpacked here.
    let wrap::WrapGeometry {
        wrap, tab_width, ..
    } = geom;
    // The view's own closing chrome — see `view_id`. For an unbound patch the view *is* the primary
    // buffer and this is the same document; for a bound one it is the only place it exists.
    let trailing_chrome: &[Element] = s
        .try_doc_of(view_id.presenting_buffer())
        .and_then(|d| d.generated.as_ref())
        .map(|g| &g.decorations.trailing_chrome[..])
        .unwrap_or(&[]);
    let hunks = buffer_both_hunks(s, primary.buffer_id);
    // The primary buffer's own diff, meaningful only while the view is that buffer. A bound element
    // carries the diff's account of its lines in its decorations instead, which
    // `render_element_lines` uses.
    let bound = primary.decorations.is_some();
    // Asked in *buffer* lines, via the layout — which is what makes the old hand-written
    // `first.min(buf.line_count())` unnecessary rather than merely correct. Passing view lines
    // straight in is what panicked the server on `j` into a commit patch's first hunk (view line
    // 756 against a 367-line file).
    let intraline = match (diff_view && !bound)
        .then(|| layout.intersect(0, first, last_excl))
        .flatten()
    {
        Some(r) => intraline_for_window(hunks, buf, r.start(), r.end_exclusive()),
        None => IntralineEmphasis::default(),
    };
    // Per **element**, because that is how the rows themselves are decided: `render_element_lines`
    // takes an element's phantom rows from the *view's* decorations when it has them, and only an
    // ordinary editor's inline diff falls back to the buffer's own hunks. Counting them here from
    // the primary buffer's diff instead left a bound patch's height six rows short of the view it
    // described — the scroll bound stopped before the end, and the fetch that fills the viewport
    // thought it had already reached it, so the bottom of the screen went blank.
    let phantom_rows: Vec<HashMap<u32, u32>> = elements
        .iter()
        .map(|binding| element_phantom_rows(s, binding, diff_view, Some(&intraline)))
        .collect();

    // Render each element over the part of its extent that is in view. An element scrolled entirely
    // out of range contributes no lines — but still reports its height, which is what lets a client
    // place the ones that are visible.
    //
    // `first`/`last_excl` are **view** lines; what `render_element_lines` wants are **buffer**
    // lines. `ViewLayout` is the only thing that crosses between them, and it also carries the
    // clamp against each buffer's live length — an extent comes from the diff, but the buffer it
    // indexes is live, and a stale range used to index past the end of a rope and panic the server.
    let mut rendered: Vec<RenderedElement> = Vec::with_capacity(elements.len());
    for (idx, binding) in elements.iter().enumerate() {
        let element = idx as aether_protocol::viewport::FieldId;
        let in_view = layout.intersect(element, first, last_excl);
        let lines = match in_view {
            Some(r) => render_element_lines(
                s,
                client_id,
                binding,
                r.start(),
                r.end_exclusive(),
                wrap,
                tab_width,
                diff_view,
                sneak_labels,
            ),
            None => Vec::new(),
        };
        // Its *first line* in the window, not merely any of them: scroll into the middle of an
        // element and its heading is above you, not at the top of the screen.
        let opens_in_view = layout
            .span_of(element)
            .is_some_and(|(start, _)| start >= first && start < last_excl);
        rendered.push(RenderedElement {
            element,
            buffer: binding.buffer_id,
            opens_in_view,
            // The element's *whole* height, from the layout's clamped range rather than the raw
            // extents: the diff's account of a hunk can outlive the lines it described.
            rows: element_visual_rows(
                s.doc_of(binding.buffer_id),
                layout.element_range(element),
                binding.cols,
                wrap,
                binding.continuation_marker_width,
                tab_width,
                &phantom_rows[idx],
            ),
            chrome_above: binding.chrome_above.clone(),
            first_buffer_line: in_view
                .map(|r| r.start())
                .unwrap_or_else(|| layout.buffer_start(element)),
            lines,
        });
    }

    // Chrome rows come from the **bindings**, which is what `compose_tree` actually draws from — a
    // bound layout's chrome is the element's, not the generated document's, and reading the latter
    // was a way for the two to disagree.
    let chrome_rows: u32 = elements
        .iter()
        .map(|e| e.chrome_above.len() as u32)
        .fold(0u32, u32::saturating_add)
        .saturating_add(trailing_chrome.len() as u32);
    let total_visual_rows = rendered
        .iter()
        .map(|r| r.rows)
        .fold(0u32, u32::saturating_add)
        .saturating_add(chrome_rows);
    let view_line_count = layout.line_count();
    let max_line_width = if matches!(wrap, aether_protocol::viewport::WrapMode::None) {
        elements
            .iter()
            .map(|e| compute_max_line_width(s.doc_of(e.buffer_id), tab_width))
            .max()
            .unwrap_or(0)
    } else {
        0
    };

    Window {
        first_view_line: first,
        last_view_line_exclusive: last_excl,
        view_line_count,
        // Counted in the *view's* rows, not the document's behind it: telling a client it may
        // scroll to a line the view does not have is what let the scroll run off the end and blank
        // the screen.
        max_scroll_view_line: max_scroll_view_line(
            s,
            elements,
            &layout,
            total_visual_rows,
            viewport_rows,
            geom,
            &phantom_rows,
        ),
        total_visual_rows,
        first_visual_row: rows_above(s, elements, &layout, &rendered, first, geom, &phantom_rows),
        // (`rows_above` indexes the same per-element maps.)
        max_line_width,
        git_status: buffer_git_status(
            s,
            elements
                .get(focused as usize)
                .map_or(primary.buffer_id, |e| e.buffer_id),
        ),
        other_elements_dirty: other_elements_dirty(s, elements, focused),
        // The closing rule is in the tree only when the window reaches the view's end — the same
        // rule an element's own chrome follows: a row belongs to the window when its *place* does.
        // It still counts toward the view's height above, because it still occupies a row.
        root: compose_tree(
            rendered,
            if last_excl >= ViewLine(view_line_count) {
                trailing_chrome
            } else {
                &[]
            },
        ),
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
    intraline: Option<&IntralineEmphasis>,
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
    deleted_rows_by_anchor(
        buffer_both_hunks(s, binding.buffer_id),
        buf.line_count(),
        intraline,
    )
    .into_iter()
    .map(|(line, rows)| (line, rows.len() as u32))
    .collect()
}

/// The visual row the window's first line sits on: everything above it, summed.
///
/// Per element, so it is answerable for a view of any shape. Elements wholly above the window
/// contribute their full height; the one the window opens inside contributes only the rows of its
/// lines above `first`; each element's chrome contributes its own rows. Reading it off the primary
/// element's document instead was correct only while a view was one whole buffer.
fn rows_above(
    s: &ServerState,
    elements: &[ElementBinding],
    layout: &ViewLayout,
    rendered: &[RenderedElement],
    first: ViewLine,
    geom: wrap::WrapGeometry,
    phantom_rows: &[HashMap<u32, u32>],
) -> VisualRow {
    let mut rows = 0u32;
    for (idx, binding) in elements.iter().enumerate() {
        let element = idx as aether_protocol::viewport::FieldId;
        let Some((start, end)) = layout.span_of(element) else {
            break;
        };
        if start >= first {
            break;
        }
        rows = rows.saturating_add(binding.chrome_above.len() as u32);
        if end <= first {
            // Wholly above: its whole height counts, which `rendered` already has.
            rows = rows.saturating_add(rendered.get(idx).map_or(0, |r| r.rows));
            continue;
        }
        // The window opens inside this one: only its lines above `first`.
        if let Some(range) = layout.intersect(element, ViewLine::ZERO, first) {
            rows = rows.saturating_add(element_visual_rows(
                s.doc_of(binding.buffer_id),
                range,
                binding.cols,
                geom.wrap,
                binding.continuation_marker_width,
                geom.tab_width,
                &phantom_rows[idx],
            ));
        }
        break;
    }
    VisualRow(rows)
}

/// The highest view line a scroll may legally land on: the one that puts the view's last visual row
/// at the bottom of the viewport.
///
/// Answered in the **view's** own rows: the line holding the visual row `viewport_rows` up from the
/// end is exactly the line that puts the last row at the bottom.
///
/// This used to ask the *primary element's document* for a whole-buffer answer and then `.min()` the
/// result into range. For a bound patch that number is a line count from the generated document — a
/// space none of the elements windows — and the clamp was the only thing standing between it and the
/// screen. It was load-bearing, and what it was hiding was the blank view about three-quarters of
/// the way down a patch: the clamp made the answer *in range* without making it *right*, so the
/// scroll still stopped short of content that existed.
///
/// Walking the view instead makes it right by construction, and for a one-element view it reproduces
/// the old whole-buffer answer line for line — the view *is* the buffer there.
fn max_scroll_view_line(
    s: &ServerState,
    elements: &[ElementBinding],
    layout: &ViewLayout,
    total_visual_rows: u32,
    viewport_rows: u32,
    geom: wrap::WrapGeometry,
    phantom_rows: &[HashMap<u32, u32>],
) -> ViewLine {
    // No viewport to fill: nothing may be scrolled past, so the top is the only legal position.
    if viewport_rows == 0 {
        return ViewLine::ZERO;
    }
    // Rows that must end up above the window for the last one to be on screen.
    let target = total_visual_rows.saturating_sub(viewport_rows);
    let no_wrap = matches!(geom.wrap, aether_protocol::viewport::WrapMode::None);

    // The **first** line with at least `target` rows above it, not the line *at* row `target`.
    //
    // The two differ, and only by chrome. Scroll positions are view lines, but a line's rows above
    // it jump by the whole of its element's chrome — so for a patch there is simply no scroll
    // position that puts row `target` at the top. Asking "which line holds that row" floors to the
    // element's first line, one whole chrome block too high, and the last rows of the view stay
    // unreachable: the limit is legal, the content below it is not on screen, and the symptom is
    // the same short scroll the old whole-buffer answer gave. Rounding *up* costs at most a screen
    // of blank below the final line and reaches everything.
    let mut rows = 0u32;
    for (idx, binding) in elements.iter().enumerate() {
        let element = idx as aether_protocol::viewport::FieldId;
        let Some((start, _)) = layout.span_of(element) else {
            break;
        };
        // An element's own chrome is drawn *inside* a window that opens on its first line, so it
        // is not "above" that line — matching `rows_above`, whose answer this must invert.
        if rows >= target {
            return start;
        }
        rows = rows.saturating_add(binding.chrome_above.len() as u32);
        let doc = s.doc_of(binding.buffer_id);
        let mut line = start;
        for i in layout.element_range(element).lines() {
            // The first line was tested *above*, against the pre-chrome total — its own chrome is
            // drawn inside the window rather than above it. Testing it again here would compare it
            // against a figure `rows_above` never reports for it, and answer a line too early.
            if line != start && rows >= target {
                return line;
            }
            rows = rows.saturating_add(
                line_visual_rows(
                    doc,
                    i,
                    no_wrap,
                    binding.cols,
                    binding.continuation_marker_width,
                    geom.tab_width,
                ) + phantom_rows[idx].get(&i).copied().unwrap_or(0),
            );
            line = line.saturating_add(1);
        }
    }
    ViewLine::last_of(layout.line_count())
}

/// The buffer a viewport is a *view of* — the patch's generated document, not the focused element's
/// file. Falls back to `fallback` for a viewport that has gone (or, on subscribe, not yet arrived),
/// where the view is the buffer being subscribed to.
pub fn view_id_of(
    s: &ServerState,
    viewport_id: aether_protocol::ViewportId,
    fallback: BufferId,
) -> ViewId {
    s.viewports
        .get(&viewport_id)
        .map(|vp| vp.view_id)
        .unwrap_or(ViewId(fallback))
}

/// Which element of a viewport holds the cursor. The sibling of [`view_id_of`], and needed at the
/// same call sites for the same reason: a render describes both the view *and* the element being
/// looked at, and those stopped being the same question once a view could window several buffers.
/// Falls back to element 0 for a viewport that has gone, matching `Viewport::focus`.
pub fn focused_of(
    s: &ServerState,
    viewport_id: aether_protocol::ViewportId,
) -> aether_protocol::ui::FieldId {
    s.viewports
        .get(&viewport_id)
        .map(|vp| vp.focused)
        .unwrap_or(0)
}

/// A copy of a viewport's element bindings, for the handlers that took a `&mut` viewport first and
/// so cannot hold a borrow of it across the render. Five scalars per element.
pub fn elements_of(
    s: &ServerState,
    viewport_id: aether_protocol::ViewportId,
) -> Vec<ElementBinding> {
    s.viewports
        .get(&viewport_id)
        .map(|vp| vp.elements.clone())
        .unwrap_or_default()
}

/// The view's element bindings: one per region the driver split the document into.
///
/// A patch is several editors separated by chrome; every other view is one editor over the whole
/// buffer. Both shapes come from the same place, so nothing downstream has to know which kind it is
/// looking at — a view is always "N elements", and N is usually 1.
fn element_bindings(
    s: &ServerState,
    buffer_id: BufferId,
    cols: u32,
    continuation_marker_width: u32,
) -> Vec<ElementBinding> {
    s.element_layout_of(buffer_id)
        .iter()
        .map(|l| l.bind(buffer_id, cols, continuation_marker_width))
        .collect()
}

/// Compose the rendered lines and a generated patch's chrome into the view's tree.
///
/// Chrome *separates* hunks, so a chrome run closes the editor element above it and the next line
/// opens a new one. An ordinary buffer has no chrome and comes out as a single editor — which is
/// what makes this a strict generalisation: the flat window is the one-element case.
///
/// Element ids index the viewport's `elements`, and come from the document's own span table rather
/// than from a walk of the visible window — see [`crate::patch::ElementSpan`].
fn compose_tree(rendered: Vec<RenderedElement>, trailing_chrome: &[Element]) -> Element {
    // Every element gets a node, including ones scrolled out of view — those carry their height and
    // an empty `lines`. The tree is the whole view, not the visible part of it: a client lays the
    // view out and scrolls it from the tree alone, so an element that vanished while off screen
    // would take its height with it and everything below would slide up as you scrolled.
    let node_of = |r: RenderedElement| Element::Editor {
        element: r.element,
        buffer: r.buffer,
        rows: r.rows,
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
        // An element's chrome is in the tree only when the element's own lines are — because the
        // tree's *rows* are the pushed window's rows, positioned by `first_visual_row`, while an
        // element's `rows` carries its height for the parts that aren't loaded. Chrome for an
        // element with no lines in the window would be a row with nowhere to be: the painter walks
        // regions, so it would draw immediately above the loaded lines, which is somewhere it isn't.
        if r.opens_in_view {
            children.extend(r.chrome_above.iter().cloned());
        }
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
            scroll: ScrollPosition {
                logical_line: ViewLine(0),
                sub_row: 0.0,
            },
            wrap: WrapMode::None,
            continuation_marker_width: 0,
            tab_width: 4,
            diff_view: false,
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
mod pushed_range_tests {
    use super::pushed_range;
    use aether_protocol::coords::ViewLine;

    /// Both ends of the range are **view** lines: `pushed_range` never sees a buffer.
    fn range(scroll: u32, rows: u32, overscan: u32, view_lines: u32) -> (u32, u32) {
        let (first, last_excl) = pushed_range(ViewLine(scroll), rows, overscan, view_lines);
        (first.get(), last_excl.get())
    }

    #[test]
    fn in_range_scroll_spans_visible_plus_overscan() {
        assert_eq!(range(100, 10, 5, 500), (95, 115));
    }

    #[test]
    fn clamps_to_buffer_ends() {
        // Near the top: overscan saturates at line 0.
        assert_eq!(range(2, 10, 5, 500), (0, 17));
        // Near the bottom: the range stops at the view's length.
        assert_eq!(range(495, 10, 5, 500), (490, 500));
    }

    #[test]
    fn scroll_past_eof_anchors_to_last_line_not_empty() {
        // The buffer shrank under the viewport (watcher reload, undo). The old scroll is far
        // past EOF; the range must anchor to the end, never collapse to an empty window.
        let (first, last_excl) = range(300, 10, 5, 50);
        assert!(first < last_excl, "range must be non-empty");
        assert!(last_excl <= 50);
        assert_eq!((first, last_excl), (44, 50));
    }

    #[test]
    fn scroll_past_eof_on_tiny_buffer_covers_it_entirely() {
        assert_eq!(range(1000, 10, 5, 3), (0, 3));
    }

    #[test]
    fn single_line_buffer_is_never_empty() {
        assert_eq!(range(0, 10, 5, 1), (0, 1));
        assert_eq!(range(42, 10, 5, 1), (0, 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Document;
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

    fn viewport_over(buffers: Vec<BufferId>) -> Viewport {
        Viewport {
            id: 1,
            view_id: ViewId(buffers.first().copied().unwrap_or_default()),
            client_id: uuid::Uuid::new_v4(),
            rows: 24,
            overscan_rows: 0,
            scroll_view_line: aether_protocol::coords::ViewLine(0),
            scroll_sub_row: 0.0,
            wrap: aether_protocol::viewport::WrapMode::None,
            tab_width: 4,
            diff_view: false,
            first_view_line: aether_protocol::coords::ViewLine(0),
            last_view_line_exclusive: aether_protocol::coords::ViewLine(1),
            focused: 0,
            elements: buffers
                .into_iter()
                .map(|buffer_id| ElementBinding {
                    buffer_id,
                    cols: 80,
                    continuation_marker_width: 0,
                    start_line: 0,
                    end_line_exclusive: 1,
                    decorations: None,
                    chrome_above: Default::default(),
                })
                .collect(),
        }
    }

    /// Chrome belongs to the element it introduces, not to a line of some document.
    ///
    /// This view has **no generated document** — two plain buffers — yet composes with headings
    /// between them, which is exactly what a driver building elements over real files needs. It
    /// also pins that chrome hides when its element's first line scrolls out of view, since the
    /// heading belongs to the element rather than to the screen — and because the tree's rows are
    /// the *pushed window's* rows: a heading whose lines aren't in the window has nowhere to be.
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
        let mut vp = viewport_over(vec![a, b]);
        for (e, name) in vp.elements.iter_mut().zip(["a.txt", "b.txt"]) {
            e.end_line_exclusive = 3;
            e.chrome_above = heading(name);
        }

        let render = |vp: &Viewport, first: u32, last: u32| {
            render_window(
                &s,
                uuid::Uuid::new_v4(),
                vp.view_id,
                &vp.elements,
                vp.focused,
                aether_protocol::coords::ViewLine(first),
                aether_protocol::coords::ViewLine(last),
                wrap::WrapGeometry {
                    wrap: aether_protocol::viewport::WrapMode::None,
                    cols: 80,
                    marker_width: 0,
                    tab_width: 4,
                },
                24,
                false,
                SneakLabels::Hidden,
            )
        };

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

        // Both elements open in view: both headings show, in order, with no document behind them.
        let window = render(&vp, 0, 6);
        assert_eq!(headings(&window.root), vec!["a.txt", "b.txt"]);

        // Scrolled past the first lines: neither element opens in view, so neither heading draws —
        // it would land at the top of the screen, above lines it doesn't introduce.
        let window = render(&vp, 1, 3);
        assert!(
            headings(&window.root).is_empty(),
            "a heading belongs to its element's first line, not to the top of the screen"
        );
        // The rows it isn't carrying are still accounted for: the window says where it sits in the
        // view, and how tall the whole view is. Those two are what the client scrolls against.
        assert_eq!(
            window.first_visual_row,
            aether_protocol::coords::VisualRow(2),
            "a.txt's heading and its first line are above the window"
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
        let mut vp = viewport_over(vec![a, a]);
        for e in vp.elements.iter_mut() {
            e.end_line_exclusive = 3;
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
        vp.elements[1].decorations = Some(std::sync::Arc::new(decorations));

        let line_1_of = |diff_view: bool, element: usize| -> LogicalLineRender {
            let window = render_window(
                &s,
                uuid::Uuid::new_v4(),
                vp.view_id,
                &vp.elements,
                vp.focused,
                aether_protocol::coords::ViewLine(0),
                aether_protocol::coords::ViewLine(6),
                wrap::WrapGeometry {
                    wrap: aether_protocol::viewport::WrapMode::None,
                    cols: 80,
                    marker_width: 0,
                    tab_width: 4,
                },
                24,
                diff_view,
                SneakLabels::Hidden,
            );
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
        let mut vp = viewport_over(vec![a, b]);

        assert_eq!(vp.buffer_id(), a, "focus starts on the first element");
        vp.focused = 1;
        assert_eq!(vp.buffer_id(), b, "and moves the view's buffer with it");

        // A stale id is not worth a panic: a view always has a first element to fall back on.
        vp.focused = 99;
        assert_eq!(vp.buffer_id(), a);
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
            cols: 80,
            continuation_marker_width: 0,
            start_line: 0,
            end_line_exclusive: 3,
            decorations: None,
            chrome_above: Default::default(),
        };
        let elements = vec![binding(a), binding(b)];

        let window = render_window(
            &s,
            uuid::Uuid::new_v4(),
            ViewId(a), // no generated view behind these plain buffers
            &elements,
            0,
            aether_protocol::coords::ViewLine(0),
            aether_protocol::coords::ViewLine(6),
            wrap::WrapGeometry {
                wrap: aether_protocol::viewport::WrapMode::None,
                cols: 80,
                marker_width: 0,
                tab_width: 4,
            },
            24,
            false,
            SneakLabels::Hidden,
        );

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

        // View-level geometry is a sum over the elements, not a reading of the first one. Each of
        // these has a different, smaller value if only element 0 is consulted: 4 lines, 4 rows, and
        // a width of 7 from "charlie".
        assert_eq!(
            window.view_line_count, 6,
            "the view is as long as its elements together"
        );
        assert_eq!(
            window.total_visual_rows, 6,
            "every element's rows count toward the view's height"
        );
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
            .map(|vp| vp.focus().buffer_id)
            .unwrap_or_else(|| params.view_id.presenting_buffer());
        let dirty: Vec<BufferId> = view_element_buffers(&s, Some(params.view_id), focused)
            .into_iter()
            .filter(|id| {
                s.try_doc_of(*id)
                    .is_some_and(|d| d.dirty && !d.read_only())
            })
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
