//! The render `View`: a JSON projection of [`Session`] for the TS shell to paint, built the way
//! `aether-tui/src/shell.rs::sync`/`editor_view` build the TUI's `AppState`. Pure core state — no
//! pixels. The shell layers its own geometry (scroll position, cell metrics) on top when it
//! renders.
//!
//! Embedded protocol types (`Window`, `CursorState`, `LspServerStatus`, …) are wire types that
//! already derive `Serialize`, so they serialise straight in; only the core's own enums (`Mode`,
//! `Pending`, `ConnState`) and the `SearchState`/`BufferInfo` projections are mapped by hand.
//!
//! This slice covers the editor, status, and search surfaces. The picker and prompt overlays are
//! exposed as `has_picker`/`has_prompt` flags for now; their full DTOs land in the next slice.

use aether_client::chips::{ChipEditor, ChipEditorField};
use aether_client::path_editor::PathEditor;
use aether_client::picker::PickerState;
use aether_client::session::{ConfirmKind, ConnState, Mode, Pending, Prompt, Session};
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
        "mode": mode(s.view.mode),
        "conn": conn(&s.conn),
        "workspace": s.workspace,
        "workspace_paths": s.workspace_paths,
        // Which context this window is in — the label reads it, and the picker's open-in-new-tab
        // links carry it so a link opened from a worktree lands in that worktree.
        "workspace_worktrees": s.workspace_worktrees,
        // What this view *is*, as distinct from the buffer being edited — see `ViewState::view_id`.
        // `viewport/subscribe` addresses this one.
        "view_id": s.view.view_id,
        // Which element holds the cursor: "is this line loaded?" is a per-element question.
        "focused_element": s.view.focused_element,
        "buffer": buffer(s),
        "viewport_id": s.view.viewport_id,
        "window": s.view.window.as_ref().map(jv),
        "wrap": jv(&s.wrap),
        "diff_view": s.diff_view,
        "ligatures": s.ligatures,
        "buffer_font_size": s.buffer_font_size,
        "ui_font_size": s.ui_font_size,
        // The reading view's column width, in ems of the reading size (the core's shared measure
        // table) — the shell stamps it as `--md-measure`. `null` is the full-width setting: no cap.
        "markdown_measure_em": aether_client::read_layout::measure_em(s.markdown_width),
        // "dark" | "light" (ThemeMode's lowercase wire form): the shell stamps this onto
        // `<html data-theme>` and theme.css switches its role variables on it.
        "theme": jv(&s.theme),
        "diagnostics": jv(&s.view.diagnostics),
        "lsp": s.view.lsp.as_ref().map(jv),
        "externally_modified": s.view.externally_modified,
        "externally_deleted": s.view.externally_deleted,
        // The long-running git operation in flight, if any. The repo id it also carries stays in
        // the core — the shell only paints the indicator; `Space g x` is dispatched core-side.
        "git_operation": s.git_operation.as_ref().map(|(_, op)| jv(op)),
        // Raw blame fields (from the server's `git/blame_changed` push): the TS shell formats
        // the label — "3w ago" needs a clock, and the shell already has one for its own chrome.
        "blame": s.view.blame.as_ref().map(|(line, b)| json!({
            "line": line,
            "author": b.author,
            "timestamp": b.timestamp,
            "is_uncommitted": b.is_uncommitted,
        })),
        "count": s.view.count,
        "pending": pending(&s.view.pending),
        "sneak_active": s.view.sneak.is_some(),
        "search": search(s),
        "prompt": prompt(&s.prompt, &s.workspace_paths, &s.conn),
        "picker": picker(&s.picker, &s.workspace_paths),
        "workspace_settings": workspace_settings(s),
        "app_settings": app_settings(s),
        "hint": s.hint_view().map(|h| {
            let (before, keys, after) = h.parts();
            json!({ "before": before, "keys": keys, "after": after })
        }),
        "read": read_view(s),
    })
}

/// The markdown reading view, when active. The shell renders `blocks` (the shared markdown AST,
/// same shape hover uses) and marks the node whose source span equals `focus_span` with the
/// position bar and the `target_span` node with the target pill — both derived core-side from the
/// one server cursor, so the shell carries no focus state of its own. `focus_span` is block-grain
/// (always present for a non-empty document); `target_span` is the interactive span the cursor sits
/// inside, absent otherwise.
fn read_view(s: &Session) -> Value {
    let Some(read) = &s.view.read else {
        return Value::Null;
    };
    let span_json = |sp: aether_client::markdown::Span| json!({ "start": sp.start, "end": sp.end });
    let cursor = s.view.buffer.cursor;
    let block = read
        .display_block_focus(&cursor)
        .map(|i| read.elements[i].span());
    // Suppressed while the selection is extended — the selection tint replaces the pill on screen.
    let target = read
        .display_target(&cursor)
        .map(|i| read.elements[i].span());
    // The extended selection's inclusive byte range: the shell tints every block node whose
    // span falls inside it. Absent while the cursor is a point.
    let selection = read
        .display_selection(&cursor)
        .map(|(min, max)| json!({ "start": min, "end": max }));
    json!({
        "loading": read.loading,
        // The stand-in line for a document with nothing to render (loading, or genuinely empty),
        // named by the core so all three shells say the same thing. Null when there are blocks.
        "placeholder": read.placeholder(),
        "blocks": jv(&read.blocks),
        "focus_span": block.map(span_json),
        "target_span": target.map(span_json),
        "selection_span": selection,
        "buffer_id": read.buffer_id,
        // Rebuild keys for the shell: the DOM is rebuilt only when the parsed content or the
        // fence highlights change (focus changes just re-mark), so images aren't re-fetched on
        // unrelated re-renders.
        "revision": read.revision,
        "hl_gen": read.hl_gen,
        // Fenced-code tree-sitter runs, keyed by the fence's span start (as a string — JSON
        // object keys). The shell styles them with the editor's own `hl-*` classes.
        "code_highlights": read
            .code_highlights
            .iter()
            .map(|(k, v)| (k.to_string(), jv(v)))
            .collect::<serde_json::Map<String, Value>>(),
    })
}

/// The application-settings overlay (`Space ,`), when open. Core-owned state + key handling
/// (`on_app_settings_key`); the shell renders grouped checkboxes and routes keys through the global
/// keydown → `on_key`, plus checkbox clicks via `app_settings_toggle`. `selected` is the flat row
/// index across all groups (group headers aren't part of it).
fn app_settings(s: &Session) -> Value {
    let Some(a) = &s.app_settings else {
        return Value::Null;
    };
    json!({
        "selected": a.selected,
        "groups": s
            .app_setting_groups()
            .iter()
            .map(|g| json!({
                "title": g.title,
                "rows": g
                    .rows
                    .iter()
                    .map(|r| {
                        use aether_client::session::AppSettingControl as C;
                        let control = match r.control {
                            C::Toggle(v) => json!({ "kind": "toggle", "value": v }),
                            C::Value(v) => json!({ "kind": "value", "value": v }),
                            // The core resolved the option to its label; the shell renders it in
                            // the same pill a stepped value gets.
                            C::Choice(v) => json!({ "kind": "choice", "value": v }),
                        };
                        json!({ "label": r.label, "control": control, "hint": r.hint })
                    })
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
    })
}

/// The workspace-settings overlay (`Space .`), when open. Core-owned state + key handling
/// (`on_workspace_settings_key`); the shell renders this projection and routes keys through the
/// global keydown → `on_key`.
///
/// Selection model: 0 = name field, then the roots, the add-root input, the projects, and the
/// add-project input. The two input indices ride along so the shell can tell which row is focused
/// without re-deriving the arithmetic.
fn workspace_settings(s: &Session) -> Value {
    let Some(ps) = &s.workspace_settings else {
        return Value::Null;
    };
    let field = |f: &aether_client::session::TextField| json!({ "text": f.text });
    json!({
        "name": field(&ps.name),
        "roots": ps.roots,
        "projects": ps.projects,
        "selected": ps.selected,
        // Selection index of the add-root input row.
        "input_index": ps.input_index(),
        // ...and of the add-project input row (the last row).
        "add_project_index": ps.add_project_index(),
        // Where the projects list starts, so the shell can place its section heading.
        "first_project_index": ps.input_index() + 1,
        // A full path editor now, like `add_project` — its `multi_root` comes back `false` (the
        // path is absolute), so the shell's whole root-segment branch falls away for it.
        "add": path_editor(&ps.add, &s.workspace_paths),
        "add_project": path_editor(&ps.add_project, &s.workspace_paths),
        // The affordance each add row shows *instead of* its editor, or null to draw the editor.
        // The rule (unfocused and untouched) and the words both come from the core, so the three
        // shells can't drift — and so a seeded field still collapses to its label.
        "add_placeholder": ps.add_placeholder(aether_client::session::SettingsRow::AddRoot),
        "add_project_placeholder": ps
            .add_placeholder(aether_client::session::SettingsRow::AddProject),
        // The add-project row's optional language override — a typeahead over the languages a
        // server exists for, so the field can only ever produce one that starts something.
        "add_project_language": {
            "input": ps.add_project_language.text,
            "ghost": ps.language_ghost(),
            "invalid": ps.language_invalid(),
            "focused": ps.on_add_project_language,
        },
        "error": ps.error,
    })
}

/// The picker overlay, when open. The items (`PickerItem`) and kind (`PickerKind`) are protocol wire
/// types and serialise verbatim; the shell renders rows from them and drives nav through the global
/// keydown → `on_picker_key`. (Filter chips + the chip editor are a follow-up slice; the filters
/// still apply server-side, they're just not drawn yet.)
fn picker(p: &Option<PickerState>, workspace_paths: &[String]) -> Value {
    match p {
        None => Value::Null,
        Some(p) => {
            // The derived chip row (active filters), for display. `flag` marks the underlined
            // word-boundary chip; exclusion is carried in the label's leading `!` (the shell reads
            // it, matching the old client). The valued-chip editor is a follow-up slice.
            let chips = p
                .chip_row(workspace_paths)
                .iter()
                .map(|c| json!({ "label": c.label, "flag": matches!(&c.id, aether_client::chips::ChipId::Word) }))
                .collect::<Vec<_>>();
            json!({
                "kind": jv(&p.kind),
                "query": p.query,
                "offset": p.offset,
                "selected": p.selected,
                "items": p.items.iter().map(jv).collect::<Vec<_>>(),
                // The window's group runs (server-pushed `GroupSpan`s, window-relative starts) —
                // the shell renders one header row per span instead of re-deriving boundaries
                // from item fields.
                "groups": p.groups.iter().map(jv).collect::<Vec<_>>(),
                // Whether this view is collapsible groups. A property of the view, not the
                // kind — a Jumplist captured from the Files or Buffers picker renders flat — so the
                // shell reads this rather than keeping its own list of collapsible kinds.
                "collapsible": p.collapsible,
                "total_matches": p.total_matches,
                "total_candidates": p.total_candidates,
                // The web throbber is CSS-animated off `ticking` (the braille `spinner_glyph` is for
                // the terminal); no glyph needed here.
                "ticking": p.ticking,
                // Settled empty-state line (core-owned wording), or null while searching / when rows
                // exist. The shell renders it verbatim; the "Searching…/Finding…" loading text it
                // still derives from `ticking` + kind.
                "empty_note": p.empty_note(),
                "total_display_rows": p.total_display_rows,
                // Display-row index of the loaded window's first rendered row (a grep header sits one
                // row above the first hit) — where the shell positions the window within the spacer.
                "window_base": p.window_base(),
                // Collapsible kinds: the focused run's absolute rows, for the `Reveal::Run` scroll
                // math. `null` for the other kinds / empty results.
                "focus_run": p.focus_run.map(|r| json!({
                    "header_row": r.header_row,
                    "len": r.len,
                })),
                "directory": p.directory,
                "directory_parent": p.directory_parent,
                // Explorer completion ghost: the rest of the highlighted directory's name, shown
                // dim after the input — the row `Alt-l` descends into. `null` when there's no such
                // row (a file is highlighted, or the name is fully typed).
                "completion": p.explorer_completion(),
                // The Explorer's synthetic "+ Create …" affordance (core-owned decision). `abs` is
                // its selection index, one past the last match; the shell appends the row when the
                // window reaches the list's end and routes a click/Enter through `picker_click`.
                "create": p.pending_create().map(|pc| json!({
                    "name": pc.name,
                    "is_dir": pc.is_dir,
                    // Rendered text, decided in the core beside the decision to offer the row —
                    // the three shells used to word it themselves and disagreed.
                    "label": p.create_row_label(),
                    // Whether to reserve the leading status cell, so the row column-aligns with
                    // the entries above it (only the Explorer's carry one).
                    "bullet": p.create_row_reserves_status_cell(),
                    "abs": p.total_matches,
                })),
                "chips": chips,
                "chip_selected": p.chip_selected,
                "chip_editor": chip_editor(&p.chip_editor, workspace_paths),
            })
        }
    }
}

/// The glob/dir filter-creation editor (the row below the query), when open. The core owns all the
/// editing logic (`on_chip_editor_key`) and the ghost/validity computation; the shell just renders
/// this projection. `root_*` fields apply only to a multi-root dir editor.
fn chip_editor(ce: &Option<ChipEditor>, workspace_paths: &[String]) -> Value {
    let Some(ed) = ce else { return Value::Null };
    let labels = aether_client::labels::root_labels(workspace_paths);
    let input = |i: &aether_client::chips::Input| json!({ "text": i.text });
    json!({
        "is_dir": ed.is_dir(),
        "tag": ed.field_tag(),
        "field": match ed.field {
            ChipEditorField::Root => "root",
            ChipEditorField::Path => "path",
        },
        "input": input(&ed.input),
        "root_filter": input(&ed.root_filter),
        "multi_root": ed.is_dir() && workspace_paths.len() > 1,
        "root_ghost": ed.root_ghost(&labels).map(|(_, suffix)| suffix),
        "root_invalid": ed.root_invalid(&labels),
        "root_display": labels.get(ed.chosen_root(&labels) as usize).cloned().unwrap_or_default(),
        "path_ghost": if ed.is_dir() { ed.path_ghost() } else { None },
        "path_invalid": ed.path_invalid(),
    })
}

/// A path-editing prompt's projection — [`path_editor`] under its own `kind` tag. Both prompts that
/// use one project identically; only the tag (and so which DOM the shell drives) differs.
fn path_prompt(ed: &PathEditor, workspace_paths: &[String], kind: &str) -> Value {
    let mut v = path_editor(ed, workspace_paths);
    v["kind"] = json!(kind);
    v
}

/// A [`PathEditor`]'s projection — same shape as [`chip_editor`]'s dir half, since the UX mirrors
/// it. The core owns the editing/ghost/validity logic; the shell renders this and syncs text back
/// through the matching `*_set_input` / `*_set_root_filter` pair.
///
/// Shared by the save-as prompt and the settings overlay's add-project row, which use the same
/// editor.
fn path_editor(ed: &PathEditor, workspace_paths: &[String]) -> Value {
    let labels = aether_client::labels::root_labels(workspace_paths);
    // The editor's own answer: `false` for an absolute-path field regardless of root count, which
    // is what makes the shell's whole root-segment branch fall away for it.
    let multi_root = ed.multi_root(workspace_paths);
    json!({
        "field": match ed.field {
            ChipEditorField::Root => "root",
            ChipEditorField::Path => "path",
        },
        "input": ed.input.text,
        "root_filter": ed.root_filter.text,
        "multi_root": multi_root,
        "root_ghost": if multi_root { ed.root_ghost(&labels).map(|(_, suffix)| suffix) } else { None },
        "root_invalid": multi_root && ed.root_invalid(&labels),
        "root_display": if multi_root {
            labels.get(ed.chosen_root(&labels) as usize).cloned()
        } else {
            None
        },
        "path_ghost": ed.path_ghost(),
        "path_invalid": ed.path_invalid(),
    })
}

/// The modal prompt overlay, when one is open (confirm / save-as / LSP info). Keys flow through the
/// core's `on_prompt_key` (the shell only renders this);
fn prompt(
    p: &Option<Prompt>,
    workspace_paths: &[String],
    conn: &aether_client::session::ConnState,
) -> Value {
    match p {
        None => Value::Null,
        Some(Prompt::Confirm { kind, .. }) => {
            json!({ "kind": "confirm", "confirm": confirm_kind(kind) })
        }
        Some(Prompt::SaveAs(ed)) => path_prompt(ed, workspace_paths, "saveas"),
        Some(Prompt::LspInfo(status)) => json!({ "kind": "lspinfo", "status": jv(status) }),
        // App info: ship the *composed sections*, not the raw snapshot. Row selection, ordering and
        // wording are the core's (shared with the native shells); the browser only paints them, so
        // it can't drift from what the TUI and GUI show.
        Some(Prompt::AppInfo(info)) => json!({
            "kind": "appinfo",
            "sections": aether_client::app_info::sections(info.as_deref(), conn)
                .into_iter()
                .map(|s| json!({
                    "title": s.title,
                    "rows": s.rows.into_iter().map(|r| json!({
                        "label": r.label,
                        "value": r.value,
                        "warn": r.tone == aether_client::app_info::InfoTone::Warn,
                    })).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        }),
        // Open-from-path: the same editor as save-as, over an absolute path — so `multi_root` comes
        // back false and the shell's root-segment branch never runs. Syncs via
        // `open_path_set_input`.
        Some(Prompt::OpenPath(ed)) => path_prompt(ed, workspace_paths, "openpath"),
    }
}

/// The structured confirmation reason. The shell composes the prompt text from this (see
/// `shell.ts`'s `confirmMessage`) — wording is the web client's presentational choice.
fn confirm_kind(k: &ConfirmKind) -> Value {
    match k {
        ConfirmKind::Overwrite { path } => json!({ "kind": "overwrite", "path": path }),
        ConfirmKind::OverwriteModified => json!({ "kind": "overwrite_modified" }),
        ConfirmKind::RecreateDeleted => json!({ "kind": "recreate_deleted" }),
        ConfirmKind::DiscardOnReload => json!({ "kind": "discard_reload" }),
        ConfirmKind::DiscardOnClose { label } => json!({ "kind": "discard_close", "label": label }),
        ConfirmKind::Delete { noun, name } => {
            json!({ "kind": "delete", "noun": noun, "name": name })
        }
        ConfirmKind::RemoveRoot { path } => json!({ "kind": "remove_root", "path": path }),
        ConfirmKind::RemoveProject { path } => json!({ "kind": "remove_project", "path": path }),
        ConfirmKind::DeleteWorkspace { name } => {
            json!({ "kind": "delete_workspace", "name": name })
        }
        ConfirmKind::DropStash { message } => {
            json!({ "kind": "drop_stash", "message": message })
        }
        ConfirmKind::DeleteBranch { name } => json!({ "kind": "delete_branch", "name": name }),
        ConfirmKind::DeleteUnmergedBranch { name } => {
            json!({ "kind": "delete_unmerged_branch", "name": name })
        }
        // The *label*, not the variant: the wording is the shell's, and the TS has no business
        // knowing git's operation taxonomy to spell one word.
        ConfirmKind::AbandonOperation { operation } => {
            json!({ "kind": "abandon_operation", "operation": operation.label() })
        }
    }
}

fn mode(m: Mode) -> &'static str {
    match m {
        Mode::Normal => "normal",
        Mode::Insert => "insert",
        Mode::Search => "search",
        Mode::Read => "read",
    }
}

fn conn(c: &ConnState) -> Value {
    match c {
        ConnState::Connected => json!({ "state": "connected" }),
        // The browser client is served *by* the daemon, so it never boots before the server —
        // `Connecting` can't occur there, but it's mapped for completeness.
        ConnState::Connecting => json!({ "state": "connecting" }),
        ConnState::Reconnecting {
            attempt,
            had_unsaved,
        } => json!({ "state": "reconnecting", "attempt": attempt, "had_unsaved": had_unsaved }),
        ConnState::Failed => json!({ "state": "failed" }),
    }
}

fn buffer(s: &Session) -> Value {
    let b = &s.view.buffer;
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
        // The git sub-leader is a leader with a prefix already typed; the shell only tests for a
        // non-null `pending` to pick the awaiting-key cursor, so this needs no TS counterpart.
        Pending::LeaderGit => json!({ "kind": "leader", "prefix": "g" }),
        Pending::Find {
            dir,
            till,
            extend,
            count,
        } => json!({
            "kind": "find", "dir": name(dir), "till": till, "extend": extend, "count": count,
        }),
        Pending::Surround(target) => json!({ "kind": "surround", "target": name(target) }),
        Pending::Transform => json!({ "kind": "transform" }),
    }
}

fn search(s: &Session) -> Value {
    let q = &s.view.search;
    // The active match options as chips (case / whole-word / literal), rendered with the same
    // styling as the grep picker's filter chips. `flag` marks the chips that render underlined.
    let chips = q
        .option_chips()
        .iter()
        .map(|c| {
            json!({
                "label": c.label,
                "flag": matches!(&c.id, aether_client::chips::ChipId::Word),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "query": q.query,
        "active": q.active,
        "summary": q.summary.as_ref().map(jv),
        "extend_to_cursor": q.extend_to_cursor,
        "chips": chips,
        "chip_selected": q.chip_selected,
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

    /// The browser's render path over a patch-shaped view, at every kind of scroll position.
    ///
    /// `render()` calls `set_visible_lines` and then `view()` on every frame, and both walk the
    /// window's tree. A patch is the shape that breaks tree walks — several elements, chrome
    /// between them, lines numbered from their own files — and a panic in either is invisible in the
    /// browser beyond `RuntimeError: unreachable executed` (see `install_panic_hook`).
    #[test]
    fn rendering_a_patch_view_walks_its_tree_without_panicking() {
        use aether_protocol::coords::{ViewLine, VisualRow};
        use aether_protocol::viewport::{Element, LogicalLineRender, Segment, Window, WrappedRow};
        let mut s = WasmSession::new();
        let line = |n: u32| LogicalLineRender {
            logical_line: n,
            visual_rows: vec![WrappedRow {
                byte_offset: 0,
                continuation_indent: 0,
                segments: vec![Segment {
                    text: format!("line {n}"),
                    highlights: vec![],
                }],
            }],
            search_matches: vec![],
            baseline_above: vec![],
            change: Default::default(),
            diagnostics: vec![],
            sneak_targets: vec![],
        };
        let chrome = Element::Chrome {
            kind: aether_protocol::viewport::ChromeKind::FileHeader,
            rail: aether_protocol::ui::RailJoin::Opens,
            children: vec![aether_protocol::ui::Element::text("a.rs", vec![])],
        };
        let w = Window {
            first_view_line: ViewLine(0),
            last_view_line_exclusive: ViewLine(4),
            view_line_count: 4,
            max_scroll_view_line: ViewLine(0),
            total_visual_rows: 6,
            first_visual_row: VisualRow(0),
            max_line_width: 0,
            git_status: None,
            root: Element::Stack {
                children: vec![
                    chrome.clone(),
                    Element::Editor {
                        element: 0,
                        buffer: 7,
                        rows: 2,
                        first_buffer_line: 16,
                        lines: vec![line(16), line(17)],
                    },
                    chrome,
                    Element::Editor {
                        element: 1,
                        buffer: 8,
                        rows: 2,
                        first_buffer_line: 40,
                        lines: vec![line(40), line(41)],
                    },
                ],
            },
        };
        s.session_mut().view.window = Some(w);
        s.session_mut().view.focused_element = 1;
        s.session_mut().view.buffer.cursor.position =
            aether_protocol::LogicalPosition { line: 40, col: 0 };
        // What `render()` does, in order.
        for top in [0u32, 3, 5, 6, 99] {
            s.set_visible_lines(top, 40);
            let v = build_view(s.session());
            assert!(v["window"].is_object(), "top={top}");
        }
    }

    #[test]
    fn mode_tracks_session_state() {
        let mut s = WasmSession::new();
        s.dispatch_key("i", "KeyI", false, false, false, 40);
        assert_eq!(build_view(s.session())["mode"], "insert");
    }
}
