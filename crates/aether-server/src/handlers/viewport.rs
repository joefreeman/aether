//! `viewport/*` — subscribe, scroll, resize, wrap, and the window rendering behind them.
//!
//! Also owns the per-line decoration that feeds a window: diff markers and phantom deleted rows,
//! intra-line emphasis, conflict bands, and the git-hunk recomputation they read from. Those live
//! here rather than with the git handlers because they exist to build a `LogicalLineRender`, not to
//! operate on a repository.

use super::*;

pub async fn viewport_subscribe(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: ViewportSubscribeParams,
) -> Result<ViewportSubscribeResult, RpcError> {
    let client_id = ctx.client_id;

    let mut s = state.lock().await;
    s.try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    // The buffer may have been mutated while nothing was viewing it — an edit through a sibling
    // buffer in another workspace, or a reload — and the per-mutation refresh skips buffers with
    // no viewport. This is the moment that stops being true, so re-diff before rendering, or the
    // first frame shows a clean gutter for a modified file and stays wrong until the next edit.
    rediff_git_for_buffer(&mut s, params.buffer_id);

    let buf = s
        .try_doc_of(params.buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(params.buffer_id))?;
    let line_count = buf.line_count();
    let buffer_id = params.buffer_id;

    let (first, last_excl) = pushed_range(
        params.scroll.logical_line,
        params.rows,
        params.overscan_rows,
        line_count,
    );
    let search = render_matches(&s, client_id, params.buffer_id);
    let sneak = s.sneaks.get(&(client_id, params.buffer_id));
    let hunks = buffer_both_hunks(&s, params.buffer_id);
    let conflicts = buffer_conflicts(&s, params.buffer_id);
    let diagnostics = buffer_diagnostics(&s, params.buffer_id);
    let buf = s.doc_of(params.buffer_id);
    // Gutter markers ride `hunks` regardless of the diff toggle; the inline view honours the
    // client's sticky setting. Hunks are seeded on open (`load_baseline`) and kept fresh per edit,
    // so they're accurate here without the recompute `git_set_diff_view` does.
    let window = render_window(
        buf,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap: params.wrap,
            cols: params.cols,
            marker_width: params.continuation_marker_width,
            tab_width: params.tab_width,
        },
        params.rows,
        WindowDecorations {
            search,
            sneak,
            diff_view: params.diff_view,
            hunks,
            conflicts,
            diagnostics,
            git_status: buffer_git_status(&s, buffer_id),
        },
    );

    let viewport_id = s.allocate_viewport_id();
    let viewport = Viewport {
        id: viewport_id,
        buffer_id,
        client_id,
        cols: params.cols,
        rows: params.rows,
        overscan_rows: params.overscan_rows,
        scroll_logical_line: params.scroll.logical_line,
        scroll_sub_row: params.scroll.sub_row,
        wrap: params.wrap,
        continuation_marker_width: params.continuation_marker_width,
        tab_width: params.tab_width,
        first_logical_line: first,
        last_logical_line_exclusive: last_excl,
        diff_view: params.diff_view,
    };
    s.viewports.insert(viewport_id, viewport);
    s.last_scroll.insert((client_id, buffer_id), params.scroll);
    tracing::debug!(%client_id, viewport_id, buffer_id, first, last_excl, "viewport subscribed");

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
                if v.buffer_id != buffer_id && !buffers.contains(&v.buffer_id) {
                    buffers.push(v.buffer_id);
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

    // Snapshot the buffer-level status the client can't derive from the window: external-change
    // flags, diagnostic counts, and language-server health. These otherwise only reach a client via
    // change-notifications (`buffer/state`, `lsp/diagnostics_changed`, `lsp/status_changed`), so a
    // viewport that subscribes *after* the relevant change already happened would show stale state
    // until the next change. Returning it in the response (vs a follow-up push) keeps it atomic with
    // the window and free of any ordering race against the client's editor switch.
    // Record what we're about to answer with as the last-sent breadcrumb, so the follow loop's
    // first push after this is a real change rather than a duplicate of the seed.
    let symbol_path = symbol_path_for(&s, client_id, buffer_id);
    s.symbol_path_sent
        .insert((client_id, buffer_id), symbol_path.clone());
    let buf = s.doc_of(buffer_id);
    let buffer_status = BufferStatusSnapshot {
        externally_modified: buf.externally_modified,
        externally_deleted: buf.externally_deleted,
        diagnostics: diagnostic_counts(buffer_diagnostics(&s, buffer_id)),
        lsp_status: s.lsp.status_for_buffer(buffer_id),
        symbol_path,
    };
    drop(s);
    for (sender, notif) in pushes {
        let _ = sender.send(notif).await;
    }

    Ok(ViewportSubscribeResult {
        viewport_id,
        window,
        buffer_status,
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
    vp.cols = params.cols;
    vp.rows = params.rows;
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line, diff_view) = (
        vp.cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.continuation_marker_width,
        vp.tab_width,
        vp.buffer_id,
        vp.scroll_logical_line,
        vp.diff_view,
    );

    let buf = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let search = render_matches(&s, client_id, buffer_id);
    let sneak = s.sneaks.get(&(client_id, buffer_id));
    let hunks = buffer_both_hunks(&s, buffer_id);
    let conflicts = buffer_conflicts(&s, buffer_id);
    let diagnostics = buffer_diagnostics(&s, buffer_id);
    let buf = s.doc_of(buffer_id);
    let window = render_window(
        buf,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        WindowDecorations {
            search,
            sneak,
            diff_view,
            hunks,
            conflicts,
            diagnostics,
            git_status: buffer_git_status(&s, buffer_id),
        },
    );

    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
    Ok(ViewportWindowResult { window })
}

pub async fn viewport_scroll_to_row(
    state: &SharedState,
    ctx: &mut ConnectionCtx,
    params: aether_protocol::viewport::ViewportScrollToRowParams,
) -> Result<ViewportWindowResult, RpcError> {
    let client_id = ctx.client_id;
    let mut s = state.lock().await;
    let vp = require_viewport_mut(&mut s, params.viewport_id, client_id)?;
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, diff_view) = (
        vp.cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.continuation_marker_width,
        vp.tab_width,
        vp.buffer_id,
        vp.diff_view,
    );
    let hunks = buffer_both_hunks(&s, buffer_id);
    let buf = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    // Row-count use only — no emphasis needed to resolve a visual row to a line.
    let deleted_rows = virtual_rows_by_line(buf, diff_view, hunks, None);
    let top_line = logical_line_at_visual_row(
        buf,
        cols,
        wrap,
        marker_width,
        tab_width,
        &deleted_rows,
        params.top_visual_row,
    );
    let (first, last_excl) = pushed_range(top_line, rows, overscan, line_count);
    let search = render_matches(&s, client_id, buffer_id);
    let sneak = s.sneaks.get(&(client_id, buffer_id));
    let hunks = buffer_both_hunks(&s, buffer_id);
    let conflicts = buffer_conflicts(&s, buffer_id);
    let diagnostics = buffer_diagnostics(&s, buffer_id);
    let buf = s.doc_of(buffer_id);
    let window = render_window(
        buf,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        WindowDecorations {
            search,
            sneak,
            diff_view,
            hunks,
            conflicts,
            diagnostics,
            git_status: buffer_git_status(&s, buffer_id),
        },
    );
    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.scroll_logical_line = top_line;
    vp.scroll_sub_row = 0.0;
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
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
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line, diff_view) = (
        vp.cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.continuation_marker_width,
        vp.tab_width,
        vp.buffer_id,
        vp.scroll_logical_line,
        vp.diff_view,
    );

    let buf = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let search = render_matches(&s, client_id, buffer_id);
    let sneak = s.sneaks.get(&(client_id, buffer_id));
    let hunks = buffer_both_hunks(&s, buffer_id);
    let conflicts = buffer_conflicts(&s, buffer_id);
    let diagnostics = buffer_diagnostics(&s, buffer_id);
    let buf = s.doc_of(buffer_id);
    let window = render_window(
        buf,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        WindowDecorations {
            search,
            sneak,
            diff_view,
            hunks,
            conflicts,
            diagnostics,
            git_status: buffer_git_status(&s, buffer_id),
        },
    );

    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
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
    vp.scroll_logical_line = params.scroll.logical_line;
    vp.scroll_sub_row = params.scroll.sub_row;
    let (cols, rows, overscan, wrap, marker_width, tab_width, buffer_id, scroll_line, diff_view) = (
        vp.cols,
        vp.rows,
        vp.overscan_rows,
        vp.wrap,
        vp.continuation_marker_width,
        vp.tab_width,
        vp.buffer_id,
        vp.scroll_logical_line,
        vp.diff_view,
    );

    let buf = s
        .try_doc_of(buffer_id)
        .ok_or_else(|| RpcError::buffer_not_found(buffer_id))?;
    let line_count = buf.line_count();
    let (first, last_excl) = pushed_range(scroll_line, rows, overscan, line_count);
    let search = render_matches(&s, client_id, buffer_id);
    let sneak = s.sneaks.get(&(client_id, buffer_id));
    let hunks = buffer_both_hunks(&s, buffer_id);
    let conflicts = buffer_conflicts(&s, buffer_id);
    let diagnostics = buffer_diagnostics(&s, buffer_id);
    let buf = s.doc_of(buffer_id);
    let window = render_window(
        buf,
        first,
        last_excl,
        wrap::WrapGeometry {
            wrap,
            cols,
            marker_width,
            tab_width,
        },
        rows,
        WindowDecorations {
            search,
            sneak,
            diff_view,
            hunks,
            conflicts,
            diagnostics,
            git_status: buffer_git_status(&s, buffer_id),
        },
    );

    let vp = s
        .viewports
        .get_mut(&params.viewport_id)
        .expect("just checked");
    vp.first_logical_line = first;
    vp.last_logical_line_exclusive = last_excl;
    s.last_scroll.insert((client_id, buffer_id), params.scroll);
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

/// Compute the logical-line range to push for a viewport. Each logical line wraps to >= 1 visual
/// row, so sending `rows + 2*overscan_rows` logical lines is a safe over-approximation of the
/// visible + overscan area. A scroll line past the end of the buffer (a stale restore, or a
/// shrink under the viewport) anchors to the last line rather than collapsing to an empty range
/// past EOF.
pub fn pushed_range(scroll_line: u32, rows: u32, overscan: u32, line_count: u32) -> (u32, u32) {
    let scroll_line = scroll_line.min(line_count.saturating_sub(1));
    let first = scroll_line.saturating_sub(overscan);
    let last_excl = scroll_line
        .saturating_add(rows)
        .saturating_add(overscan)
        .min(line_count);
    (first, last_excl.max(first))
}

/// Recompute every viewport's pushed range for this buffer from `pushed_range` against the new
/// line count. Call **before** building `viewport/lines_changed` notifications after any
/// mutation that may grow or shrink the buffer — otherwise a growth (e.g. undoing a join)
/// leaves the viewport's range clamped to the smaller post-mutation size and the freshly
/// restored lines never reach the client.
pub fn refresh_viewport_ranges_for_buffer(
    s: &mut ServerState,
    buffer_id: BufferId,
    line_count: u32,
) {
    let max_line = line_count.saturating_sub(1);
    // Every viewport on any buffer of the document: a mutation through one workspace's buffer
    // moves the shared content under every sibling's viewports too.
    let attached = s.doc_siblings(buffer_id);
    for vp in s.viewports.values_mut() {
        if !attached.contains(&vp.buffer_id) {
            continue;
        }
        // A shrink can leave the viewport scrolled past EOF (a watcher reload of a rewritten
        // file, an undo, another client's delete). Clamp the stored scroll like reload clamps
        // cursors — otherwise the pushed range is empty and the client shows a blank buffer.
        // The restore map gets the same clamp so a buffer switch doesn't resurrect the stale
        // position.
        if vp.scroll_logical_line > max_line {
            vp.scroll_logical_line = max_line;
            vp.scroll_sub_row = 0.0;
            s.last_scroll.insert(
                (vp.client_id, vp.buffer_id),
                ScrollPosition {
                    logical_line: max_line,
                    sub_row: 0.0,
                },
            );
        }
        let (first, last_excl) = pushed_range(
            vp.scroll_logical_line,
            vp.rows,
            vp.overscan_rows,
            line_count,
        );
        vp.first_logical_line = first;
        vp.last_logical_line_exclusive = last_excl;
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
    let viewed = s.viewports.values().any(|vp| vp.buffer_id == buffer_id);
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
/// Every virtual row above each line, from whichever producer this buffer has.
///
/// The two are mutually exclusive by construction: the inline diff view's phantom deleted rows
/// need a Git baseline to diff against, and a generated patch has none of its own. They merge here
/// rather than at the render site because **the scroll arithmetic needs the same answer** — a
/// chrome row occupies a screen row exactly as a phantom one does, so leaving it out of the extent
/// makes the scrollbar short, puts the last lines out of reach, and lets the cursor sit below the
/// scrollable area.
fn virtual_rows_by_line(
    buf: &Document,
    diff_view: bool,
    hunks: &[crate::git::DiffHunk],
    intraline: Option<&IntralineEmphasis>,
) -> HashMap<u32, Vec<VirtualRow>> {
    if let Some(generated) = buf.generated.as_ref() {
        let mut map: HashMap<u32, Vec<VirtualRow>> = generated
            .decorations
            .virtual_rows
            .iter()
            .enumerate()
            .filter(|(_, rows)| !rows.is_empty())
            .map(|(i, rows)| (i as u32, rows.clone()))
            .collect();
        // The closing rule renders *below* the last line, but for the scroll arithmetic a row is a
        // row — it occupies one either way, and leaving it out would put the bottom of the patch
        // out of reach exactly as the chrome rows once did.
        if !generated.decorations.trailing_rows.is_empty() {
            map.entry(buf.line_count().saturating_sub(1))
                .or_default()
                .extend(generated.decorations.trailing_rows.iter().cloned());
        }
        return map;
    }
    if diff_view {
        deleted_rows_by_anchor(hunks, buf.line_count(), intraline)
    } else {
        HashMap::new()
    }
}

fn deleted_rows_by_anchor(
    hunks: &[crate::git::DiffHunk],
    line_count: u32,
    intraline: Option<&IntralineEmphasis>,
) -> HashMap<u32, Vec<VirtualRow>> {
    let mut map: HashMap<u32, Vec<VirtualRow>> = HashMap::new();
    let last_line = line_count.saturating_sub(1);
    for (hunk_idx, h) in hunks.iter().enumerate() {
        if h.deleted.is_empty() {
            continue;
        }
        let anchor = h.anchor_line.min(last_line);
        let rows = map.entry(anchor).or_default();
        rows.extend(h.deleted.iter().enumerate().map(|(row_idx, text)| {
            VirtualRow {
                text: text.clone(),
                kind: VirtualRowKind::Deleted,
                stage: h.stage,
                emphasis: intraline
                    .and_then(|m| m.rows.get(&(hunk_idx, row_idx)))
                    .cloned()
                    .unwrap_or_default(),
                // A deleted row's colour is entirely the diff palette's: it has no grammar of its
                // own here (the baseline text isn't parsed), so there is nothing to span.
                highlights: Vec::new(),
            }
        }));
    }
    for rows in map.values_mut() {
        if rows.iter().any(|r| r.stage == DiffStage::Unstaged) {
            rows.retain(|r| r.stage == DiffStage::Unstaged);
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

/// Find the largest `scroll_logical_line` such that the buffer's last visual row sits at the
/// bottom of the viewport. Walks logical lines from the end backward, accumulating their visual
/// row counts under the current wrap settings until we have `viewport_rows` rows. Diff-view
/// phantom rows (`deleted_rows`) count as occupied rows so the bottom of a diff still scrolls into
fn compute_max_scroll(
    buf: &Document,
    viewport_rows: u32,
    cols: u32,
    wrap: aether_protocol::viewport::WrapMode,
    marker_width: u32,
    tab_width: u32,
    deleted_rows: &HashMap<u32, Vec<VirtualRow>>,
) -> u32 {
    let line_count = buf.line_count();
    if viewport_rows == 0 || line_count == 0 {
        return 0;
    }
    let no_wrap = matches!(wrap, aether_protocol::viewport::WrapMode::None);
    if no_wrap && deleted_rows.is_empty() {
        return line_count.saturating_sub(viewport_rows);
    }
    let mut rows_remaining = viewport_rows;
    for line_idx in (0..line_count).rev() {
        let virtual_n = deleted_rows.get(&line_idx).map_or(0, |v| v.len() as u32);
        let real_n = if no_wrap {
            1
        } else {
            let mut text: String = buf.text.line(line_idx as usize).chunks().collect();
            if text.ends_with('\n') {
                text.pop();
            }
            wrap::compute_rows(&text, cols, marker_width, tab_width).len() as u32
        };
        let n = real_n + virtual_n;
        if n >= rows_remaining {
            return line_idx;
        }
        rows_remaining -= n;
    }
    0
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

/// `(first_visual_row, total_visual_rows)` for the buffer at this config — the visual row where
/// `first` begins and the buffer's total visual-row height (real + diff phantom rows). O(lines);
/// the no-wrap/no-diff case is O(1).
fn compute_visual_extent(
    buf: &Document,
    cols: u32,
    wrap: aether_protocol::viewport::WrapMode,
    marker_width: u32,
    tab_width: u32,
    deleted_rows: &HashMap<u32, Vec<VirtualRow>>,
    first: u32,
) -> (u32, u32) {
    let line_count = buf.line_count();
    let no_wrap = matches!(wrap, aether_protocol::viewport::WrapMode::None);
    if no_wrap && deleted_rows.is_empty() {
        return (first.min(line_count), line_count);
    }
    let mut total = 0u32;
    let mut first_vr = 0u32;
    for i in 0..line_count {
        if i == first {
            first_vr = total;
        }
        let virtual_n = deleted_rows.get(&i).map_or(0, |v| v.len() as u32);
        total = total.saturating_add(
            line_visual_rows(buf, i, no_wrap, cols, marker_width, tab_width) + virtual_n,
        );
    }
    if first >= line_count {
        first_vr = total;
    }
    (first_vr, total)
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

/// The logical line whose visual-row span contains `target_row` (clamped to the last line).
pub fn logical_line_at_visual_row(
    buf: &Document,
    cols: u32,
    wrap: aether_protocol::viewport::WrapMode,
    marker_width: u32,
    tab_width: u32,
    deleted_rows: &HashMap<u32, Vec<VirtualRow>>,
    target_row: u32,
) -> u32 {
    let line_count = buf.line_count();
    if line_count == 0 {
        return 0;
    }
    let no_wrap = matches!(wrap, aether_protocol::viewport::WrapMode::None);
    if no_wrap && deleted_rows.is_empty() {
        return target_row.min(line_count - 1);
    }
    let mut acc = 0u32;
    for i in 0..line_count {
        let virtual_n = deleted_rows.get(&i).map_or(0, |v| v.len() as u32);
        let n = line_visual_rows(buf, i, no_wrap, cols, marker_width, tab_width) + virtual_n;
        if acc + n > target_row {
            return i;
        }
        acc += n;
    }
    line_count - 1
}

/// Everything that decorates a rendered window beyond the text itself: search highlights, the
/// inline-diff state, diagnostics squiggles, and the buffer's git status. Bundled because every
/// `render_window` caller assembles the same set from `ServerState`.
pub struct WindowDecorations<'a> {
    pub search: Option<&'a SearchEntry>,
    pub sneak: Option<&'a SneakEntry>,
    pub diff_view: bool,
    pub hunks: &'a [crate::git::DiffHunk],
    /// Conflict blocks, for a file a stopped merge or rebase left conflicted. Empty otherwise —
    /// and when it isn't, `hunks` is empty, because a conflicted file has no baseline to diff.
    pub conflicts: &'a [crate::git::ConflictRegion],
    pub diagnostics: &'a [crate::lsp::diagnostics::BufferDiagnostic],
    pub git_status: Option<GitBufferStatus>,
}

pub fn render_window(
    buf: &Document,
    first: u32,
    last_excl: u32,
    geom: wrap::WrapGeometry,
    viewport_rows: u32,
    deco: WindowDecorations<'_>,
) -> Window {
    let WindowDecorations {
        search,
        sneak,
        diff_view,
        hunks,
        conflicts,
        diagnostics,
        git_status,
    } = deco;
    let wrap::WrapGeometry {
        wrap,
        cols,
        marker_width,
        tab_width,
    } = geom;
    let mut lines: Vec<LogicalLineRender> = Vec::with_capacity((last_excl - first) as usize);

    // Per-line change markers drive the always-on gutter, so they're computed whenever hunks are
    // known — independent of the diff-view toggle. Phantom "deleted" rows, by contrast, only
    // appear while the diff view is on.
    let markers = diff_markers_by_line(hunks, buf.line_count());
    // Not gated on `diff_view`, unlike everything else here: the sides of a conflict are not a
    // review mode you opt into, they're the only way to read the file at all.
    let conflict_lines = conflict_lines_by_line(conflicts, buf.line_count());
    let intraline = if diff_view {
        intraline_for_window(hunks, buf, first, last_excl)
    } else {
        IntralineEmphasis::default()
    };
    let deleted_rows = virtual_rows_by_line(buf, diff_view, hunks, Some(&intraline));

    // For highlighting we need the whole source as bytes. Computed once per render rather than
    // per line. Skipped entirely when no syntax is attached.
    let source: Option<String> = buf
        .syntax
        .as_ref()
        .map(|_| buf.text.chunks().collect::<String>());
    // Generated read-only content (a commit's patch) instead of a parse tree — see
    // [`crate::patch::GeneratedPatch`]. Both are never present at once.
    let generated = buf.generated.as_ref().map(|g| &g.decorations);

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
        // A generated patch's above-rows come straight from its decorations, *not* from the map
        // the scroll math uses: that map folds the closing chrome in (a row is a row, wherever it
        // draws), and folding it in here would render it above the last line instead of below it.
        match generated {
            Some(g) => {
                if let Some(rows) = g.virtual_rows.get(i as usize) {
                    render.virtual_rows_above = rows.clone();
                }
            }
            None => {
                if let Some(rows) = deleted_rows.get(&i) {
                    render.virtual_rows_above = rows.clone();
                }
            }
        }
        // The patch's closing rule, on its final line — see `GeneratedPatch::trailing_rows`.
        if i + 1 == buf.line_count() {
            if let Some(g) = generated {
                render.virtual_rows_below = g.trailing_rows.clone();
            }
        }
        if let Some((marker, stage)) = markers.get(&i).copied() {
            render.diff_marker = Some(marker);
            render.diff_stage = stage;
        }
        render.conflict = conflict_lines.get(&i).copied();
        // Ungated, like `conflict` and for the same reason: a patch buffer *is* a diff, so there's
        // no view to toggle it behind.
        render.patch = generated
            .and_then(|d| d.patch.get(i as usize).copied())
            .flatten();
        // Which layer this change sits in. In the working-tree diff it is the only visible effect
        // of staging — the text is identical either way, because staging doesn't move HEAD.
        if let Some(stage) = generated.and_then(|d| d.stage.get(i as usize).copied()) {
            render.diff_stage = stage;
        }
        if let Some(spans) = intraline.lines.get(&i) {
            render.diff_emphasis = spans.clone();
        } else if let Some(spans) = generated.and_then(|d| d.emphasis.get(i as usize)) {
            // A patch's emphasis is precomputed, and lands on *both* sides — unlike the inline
            // diff view, where the old side is a phantom row carrying its own `emphasis` instead.
            render.diff_emphasis = spans.clone();
        }
        render.diagnostics = diagnostic_spans_on_line(diagnostics, i, text.len() as u32);
        lines.push(render);
    }
    let (first_visual_row, total_visual_rows) = compute_visual_extent(
        buf,
        cols,
        wrap,
        marker_width,
        tab_width,
        &deleted_rows,
        first,
    );
    let max_line_width = if matches!(wrap, aether_protocol::viewport::WrapMode::None) {
        compute_max_line_width(buf, tab_width)
    } else {
        0
    };
    Window {
        first_logical_line: first,
        last_logical_line_exclusive: last_excl,
        line_count: buf.line_count(),
        max_scroll_logical_line: compute_max_scroll(
            buf,
            viewport_rows,
            cols,
            wrap,
            marker_width,
            tab_width,
            &deleted_rows,
        ),
        total_visual_rows,
        first_visual_row,
        max_line_width,
        git_status,
        lines,
    }
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
        assert_eq!(map[&1][0].text, "old beta");
        assert_eq!(map[&1][0].kind, VirtualRowKind::Deleted);
        assert_eq!(map.get(&4).map(Vec::len), Some(2));
        assert_eq!(map[&4][1].text, "gone two");
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
        assert_eq!(
            (rows[0].text.as_str(), rows[0].stage),
            ("index text", DiffStage::Unstaged)
        );
        let solo = &map[&5];
        assert_eq!(
            (solo[0].text.as_str(), solo[0].stage),
            ("solo head text", DiffStage::Staged)
        );
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

    fn sub_params(buffer_id: BufferId) -> ViewportSubscribeParams {
        ViewportSubscribeParams {
            buffer_id,
            cols: 80,
            rows: 24,
            overscan_rows: 0,
            scroll: ScrollPosition {
                logical_line: 0,
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
        let res = viewport_subscribe(&state, &mut ctx, sub_params(buffer_id))
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
        let res = viewport_subscribe(&state, &mut ctx, sub_params(buffer_id))
            .await
            .unwrap();
        assert!(res.buffer_status.externally_modified);
        assert!(!res.buffer_status.externally_deleted);
    }

    #[tokio::test]
    async fn subscribe_to_clean_unbacked_buffer_snapshots_empty_status() {
        let (state, client_id, buffer_id) = setup(Vec::new(), false, false);
        let mut ctx = ConnectionCtx { client_id };
        let res = viewport_subscribe(&state, &mut ctx, sub_params(buffer_id))
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

    #[test]
    fn in_range_scroll_spans_visible_plus_overscan() {
        assert_eq!(pushed_range(100, 10, 5, 500), (95, 115));
    }

    #[test]
    fn clamps_to_buffer_ends() {
        // Near the top: overscan saturates at line 0.
        assert_eq!(pushed_range(2, 10, 5, 500), (0, 17));
        // Near the bottom: the range stops at line_count.
        assert_eq!(pushed_range(495, 10, 5, 500), (490, 500));
    }

    #[test]
    fn scroll_past_eof_anchors_to_last_line_not_empty() {
        // The buffer shrank under the viewport (watcher reload, undo). The old scroll is far
        // past EOF; the range must anchor to the end, never collapse to an empty window.
        let (first, last_excl) = pushed_range(300, 10, 5, 50);
        assert!(first < last_excl, "range must be non-empty");
        assert!(last_excl <= 50);
        assert_eq!((first, last_excl), (44, 50));
    }

    #[test]
    fn scroll_past_eof_on_tiny_buffer_covers_it_entirely() {
        assert_eq!(pushed_range(1000, 10, 5, 3), (0, 3));
    }

    #[test]
    fn single_line_buffer_is_never_empty() {
        assert_eq!(pushed_range(0, 10, 5, 1), (0, 1));
        assert_eq!(pushed_range(42, 10, 5, 1), (0, 1));
    }
}
