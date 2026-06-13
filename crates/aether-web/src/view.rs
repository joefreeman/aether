//! The render `View` (docs/web-core.md): a JSON projection of [`Session`] for the TS shell to
//! paint, built the way `aether-tui/src/shell.rs::sync()`/`editor_view()` build the TUI's
//! `AppState`. Pure core state — no pixels. The shell layers its own geometry (scroll position,
//! cell metrics) on top when it renders.
//!
//! Embedded protocol types (`Window`, `CursorState`, `LspServerStatus`, …) are wire types that
//! already derive `Serialize`, so they serialise straight in; only the core's own enums (`Mode`,
//! `Pending`, `ConnState`) and the `SearchState`/`BufferInfo` projections are mapped by hand.
//!
//! This slice covers the editor, status, and search surfaces. The picker and prompt overlays are
//! exposed as `has_picker`/`has_prompt` flags for now; their full DTOs land in the next slice.

use aether_client::chips::{ChipEditor, ChipEditorField};
use aether_client::picker::PickerState;
use aether_client::session::{ConnState, Mode, Pending, Prompt, Session};
use serde::Serialize;
use serde_json::{json, Value};

/// Serialise any wire type into the view, or `Null` if it somehow can't (it always can).
fn jv<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// Lower-cased debug name for a small `Copy` enum that has no serde derive (e.g. `Direction`).
fn name<T: std::fmt::Debug>(v: &T) -> String {
    format!("{v:?}").to_lowercase()
}

/// Build the render view from the session. The TS shell reads this each frame.
pub fn build_view(s: &Session) -> Value {
    json!({
        "mode": mode(s.mode),
        "conn": conn(&s.conn),
        "project": s.project,
        "project_paths": s.project_paths,
        "buffer": buffer(s),
        "viewport_id": s.viewport_id,
        "window": s.window.as_ref().map(jv),
        "wrap": jv(&s.wrap),
        "diff_view": s.diff_view,
        "diagnostics": jv(&s.diagnostics),
        "lsp": s.lsp.as_ref().map(jv),
        "externally_modified": s.externally_modified,
        "externally_deleted": s.externally_deleted,
        "blame": s.blame.as_ref().map(|(line, text)| json!({ "line": line, "text": text })),
        "count": s.count,
        "pending": pending(&s.pending),
        "search": search(s),
        "prompt": prompt(&s.prompt),
        "picker": picker(&s.picker, &s.project_paths),
    })
}

/// The picker overlay, when open. The items (`PickerItem`) and kind (`PickerKind`) are protocol wire
/// types and serialise verbatim; the shell renders rows from them and drives nav through the global
/// keydown → `on_picker_key`. (Filter chips + the chip editor are a follow-up slice; the filters
/// still apply server-side, they're just not drawn yet.)
fn picker(p: &Option<PickerState>, project_paths: &[String]) -> Value {
    match p {
        None => Value::Null,
        Some(p) => {
            // The derived chip row (active filters), for display. `flag` marks the underlined
            // word-boundary chip; exclusion is carried in the label's leading `!` (the shell reads
            // it, matching the old client). The valued-chip editor is a follow-up slice.
            let chips = p
                .chip_row(project_paths)
                .iter()
                .map(|c| json!({ "label": c.label, "flag": matches!(&c.id, aether_client::chips::ChipId::Word) }))
                .collect::<Vec<_>>();
            json!({
                "kind": jv(&p.kind),
                "query": p.query,
                "cursor": p.cursor,
                "offset": p.offset,
                "selected": p.selected,
                "items": p.items.iter().map(jv).collect::<Vec<_>>(),
                "total_matches": p.total_matches,
                "total_candidates": p.total_candidates,
                "ticking": p.ticking,
                "total_display_rows": p.total_display_rows,
                "directory": p.directory,
                "directory_parent": p.directory_parent,
                "chips": chips,
                "chip_selected": p.chip_selected,
                "chip_editor": chip_editor(&p.chip_editor, project_paths),
            })
        }
    }
}

/// The glob/dir filter-creation editor (the row below the query), when open. The core owns all the
/// editing logic (`on_chip_editor_key`) and the ghost/validity computation; the shell just renders
/// this projection. `root_*` fields apply only to a multi-root dir editor.
fn chip_editor(ce: &Option<ChipEditor>, project_paths: &[String]) -> Value {
    let Some(ed) = ce else { return Value::Null };
    let labels = aether_client::labels::root_labels(project_paths);
    let input = |i: &aether_client::chips::Input| json!({ "text": i.text, "cursor": i.cursor });
    json!({
        "is_dir": ed.is_dir(),
        "field": match ed.field {
            ChipEditorField::Root => "root",
            ChipEditorField::Path => "path",
        },
        "input": input(&ed.input),
        "root_filter": input(&ed.root_filter),
        "multi_root": ed.is_dir() && project_paths.len() > 1,
        "root_ghost": ed.root_ghost(&labels).map(|(_, suffix)| suffix),
        "root_invalid": ed.root_invalid(&labels),
        "root_display": labels.get(ed.chosen_root(&labels) as usize).cloned().unwrap_or_default(),
        "path_ghost": if ed.is_dir() { ed.path_ghost() } else { None },
        "path_invalid": ed.path_invalid(),
    })
}

/// The modal prompt overlay, when one is open (confirm / save-as / LSP info). Keys flow through the
/// core's `on_prompt_key` (the shell only renders this); see docs/web-core.md.
fn prompt(p: &Option<Prompt>) -> Value {
    match p {
        None => Value::Null,
        Some(Prompt::Confirm { message, .. }) => json!({ "kind": "confirm", "message": message }),
        Some(Prompt::SaveAs {
            path_index,
            input,
            cursor,
        }) => json!({
            "kind": "saveas", "path_index": path_index, "input": input, "cursor": cursor,
        }),
        Some(Prompt::LspInfo(status)) => json!({ "kind": "lspinfo", "status": jv(status) }),
    }
}

fn mode(m: Mode) -> &'static str {
    match m {
        Mode::Normal => "normal",
        Mode::Insert => "insert",
        Mode::Search => "search",
    }
}

fn conn(c: &ConnState) -> Value {
    match c {
        ConnState::Connected => json!({ "state": "connected" }),
        ConnState::Reconnecting {
            attempt,
            had_unsaved,
        } => json!({ "state": "reconnecting", "attempt": attempt, "had_unsaved": had_unsaved }),
        ConnState::Failed => json!({ "state": "failed" }),
    }
}

fn buffer(s: &Session) -> Value {
    let b = &s.buffer;
    json!({
        "buffer_id": b.buffer_id,
        "path": b.path,
        "label": b.label,
        "language": b.language,
        "revision": b.revision,
        "saved_revision": b.saved_revision,
        "transient": b.transient,
        "cursor": jv(&b.cursor),
        // The buffer's restored scroll (server-provided; positions a fresh subscribe). The shell
        // reads this each subscribe so a jump always loads the window around its target.
        "scroll": jv(&b.scroll),
        "lsp_server": b.lsp_server.as_ref().map(jv),
    })
}

fn pending(p: &Pending) -> Value {
    match p {
        Pending::None => Value::Null,
        Pending::Leader => json!({ "kind": "leader" }),
        Pending::Find {
            dir,
            till,
            extend,
            count,
        } => json!({
            "kind": "find", "dir": name(dir), "till": till, "extend": extend, "count": count,
        }),
        Pending::Surround(target) => json!({ "kind": "surround", "target": name(target) }),
    }
}

fn search(s: &Session) -> Value {
    let q = &s.search;
    json!({
        "query": q.query,
        "cursor": q.cursor,
        "active": q.active,
        "summary": q.summary.as_ref().map(jv),
        "history": q.history,
        "history_cursor": q.history_cursor,
        "history_draft": q.history_draft,
        "extend_to_cursor": q.extend_to_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WasmSession;

    #[test]
    fn placeholder_view_has_the_core_shape() {
        let s = WasmSession::new();
        let v = build_view(s.session());
        assert_eq!(v["mode"], "normal");
        assert_eq!(v["conn"]["state"], "connected");
        assert_eq!(v["picker"], Value::Null);
        assert_eq!(v["window"], Value::Null);
        assert_eq!(v["pending"], Value::Null);
        // The buffer projection carries the protocol cursor verbatim.
        assert!(v["buffer"]["cursor"].is_object());
    }

    #[test]
    fn mode_tracks_session_state() {
        let mut s = WasmSession::new();
        s.dispatch_key("i", false, false, false, 40);
        assert_eq!(build_view(s.session())["mode"], "insert");
    }
}
