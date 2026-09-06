//! The GUI painted headlessly.
//!
//! `iced_test`'s [`Simulator`] builds the real widget tree from [`App::view`] under a CPU
//! rasteriser — no window, no GPU — and lets a test find text, click it and take a PNG of the
//! frame. It is the terminal's `TestBackend` for this shell: the same "seed a session, paint,
//! read the frame back" shape, so the row-layout regressions the other shells pin are pinned
//! here too, on the widgets that actually ship.
//!
//! Selectors see what widgets report through `Operation::text`: every plain `text` in the
//! chrome, text inputs, and the editor's painted rows (`EditorView::operate`). Rich text — picker
//! rows with match highlights — reports nothing, so those are checked through the messages a
//! click produces, or by eye in the snapshot. Keys never reach widgets here either: the app
//! takes them off a raw-event subscription, which a simulator does not run.
//!
//! Set `AETHER_SNAPSHOT_DIR` to also write each test's frame there as a PNG to look at.

use super::tests::connecting_bootstrap;
use super::*;
use aether_protocol::coords::ElementRow;
use aether_protocol::picker::{PickerItem, PickerUpdateParams};
use aether_protocol::ui::{Element as ViewElement, LayoutOwner, RailJoin};
use aether_protocol::viewport::{ChromeKind, LogicalLineRender, Segment, Window, WrappedRow};
use iced::Rectangle;
use iced_test::selector::Candidate;
use iced_test::{Selector, Simulator};
use std::sync::{Arc, Mutex, Once};

/// The simulated window. Wide enough for the status bar's segments, short enough that a
/// scrolled-off row is a real case.
const WIDTH: f32 = 800.0;
const HEIGHT: f32 = 600.0;

/// A simulator over the app's current view: the CPU rasteriser pinned (the fallback tries the
/// GPU first, which a test must never touch), the window's own settings, a fixed size.
fn simulate(app: &App) -> Simulator<'_, Message> {
    static PIN: Once = Once::new();
    PIN.call_once(|| std::env::set_var("ICED_TEST_BACKEND", "tiny-skia"));
    Simulator::with_size(settings(), Size::new(WIDTH, HEIGHT), app.view())
}

/// One text the tree reported, where it reported it.
#[derive(Debug, Clone)]
struct Seen {
    text: String,
    bounds: Rectangle,
    visible: bool,
}

/// A selector that never matches but writes down every text it is offered — the frame as words.
#[derive(Clone)]
struct Collect(Arc<Mutex<Vec<Seen>>>);

impl Selector for Collect {
    type Output = ();

    fn select(&mut self, candidate: Candidate<'_>) -> Option<()> {
        let seen = match candidate {
            Candidate::Text {
                bounds,
                visible_bounds,
                content,
                ..
            } => Seen {
                text: content.to_string(),
                bounds,
                visible: visible_bounds.is_some(),
            },
            Candidate::TextInput {
                bounds,
                visible_bounds,
                state,
                ..
            } => Seen {
                text: state.text().to_string(),
                bounds,
                visible: visible_bounds.is_some(),
            },
            _ => return None,
        };
        self.0.lock().unwrap().push(seen);
        None
    }

    fn description(&self) -> String {
        "every text".into()
    }
}

/// Every text on the frame, top to bottom then left to right.
fn seen(sim: &mut Simulator<'_, Message>) -> Vec<Seen> {
    let out = Arc::new(Mutex::new(Vec::new()));
    // Never matches: the collector's job is done by the time `find` reports nothing found.
    let _ = sim.find(Collect(out.clone()));
    let mut all = out.lock().unwrap().clone();
    all.sort_by(|a, b| {
        (a.bounds.y, a.bounds.x)
            .partial_cmp(&(b.bounds.y, b.bounds.x))
            .unwrap()
    });
    all
}

/// The frame as rows of text, top to bottom — everything reported *and visible*, banded by its
/// vertical position and read left to right. The terminal's `render_rows` for this shell: a
/// row scrolled out of its pane is not on the frame.
fn rows(sim: &mut Simulator<'_, Message>) -> Vec<String> {
    let mut bands: Vec<(f32, f32, Vec<Seen>)> = Vec::new();
    for s in seen(sim).into_iter().filter(|s| s.visible) {
        let mid = s.bounds.y + s.bounds.height / 2.0;
        match bands
            .iter_mut()
            .find(|(top, bottom, _)| *top <= mid && mid < *bottom)
        {
            Some((_, _, items)) => items.push(s),
            None => bands.push((s.bounds.y, s.bounds.y + s.bounds.height, vec![s])),
        }
    }
    bands.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    bands
        .into_iter()
        .map(|(_, _, mut items)| {
            items.sort_by(|a, b| a.bounds.x.partial_cmp(&b.bounds.x).unwrap());
            items
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect()
}

/// The first row containing `needle`; panics with the frame when nothing does.
fn row_of(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|r| r.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not on the frame:\n{}", rows.join("\n")))
}

/// Draw the frame. With `AETHER_SNAPSHOT_DIR` set, also write it there as
/// `<name>-<renderer>.png` to look at — overwriting, since this is a viewing aid rather than a
/// comparison.
fn snapshot(sim: &mut Simulator<'_, Message>, app: &App, name: &str) {
    let shot = sim
        .snapshot(&base_theme(app))
        .expect("the frame draws headlessly");
    let Ok(dir) = std::env::var("AETHER_SNAPSHOT_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("snapshot dir");
    for entry in std::fs::read_dir(&dir).expect("snapshot dir").flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if file.starts_with(&format!("{name}-")) && file.ends_with(".png") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    assert!(shot
        .matches_image(dir.join(name))
        .expect("write the snapshot"));
}

// ---- fixtures ------------------------------------------------------------------------------

fn line(n: u32, text: &str) -> LogicalLineRender {
    LogicalLineRender {
        logical_line: n,
        visual_rows: vec![WrappedRow {
            byte_offset: 0,
            continuation_indent: 0,
            segments: vec![Segment {
                text: text.to_string(),
                highlights: Vec::new(),
            }],
        }],
        search_matches: Vec::new(),
        baseline_above: Vec::new(),
        change: Default::default(),
        diagnostics: Vec::new(),
        sneak_targets: Vec::new(),
    }
}

fn chrome(text: &str) -> ViewElement {
    ViewElement::Chrome {
        kind: ChromeKind::FileHeader,
        rail: RailJoin::Opens,
        children: vec![ViewElement::text(text, Vec::new())],
    }
}

fn editor(element: u32, buffer: u64, first: u32, lines: Vec<LogicalLineRender>) -> ViewElement {
    ViewElement::Editor {
        element,
        buffer,
        rows: lines.len() as u32,
        first_row: ElementRow::ZERO,
        laid_out_by: LayoutOwner::Server,
        first_buffer_line: first,
        lines,
    }
}

fn window_of(children: Vec<ViewElement>) -> Window {
    Window {
        other_elements_dirty: false,
        max_line_width: 0,
        git_status: None,
        root: ViewElement::Stack { children },
    }
}

/// A connected session over a real (non-placeholder) buffer, showing `window`.
fn session_showing(window: Window) -> Session {
    let mut s = Session::placeholder();
    s.conn = ConnState::Connected;
    s.view.buffer.buffer_id = 7;
    s.view.viewport_id = Some(1);
    s.view.view_label = "a.rs".into();
    s.view.window = Some(window);
    s
}

/// An app over `session`, wired to a dummy transport like the connecting boot state.
fn app_with(session: Session) -> App {
    let (mut app, _boot) = App::new(window::Id::unique(), connecting_bootstrap());
    app.session = session;
    app
}

fn app_showing(window: Window) -> App {
    app_with(session_showing(window))
}

/// Two files whose hunks start at the same line number — what a patch of two hunks looks like.
fn two_files() -> Window {
    window_of(vec![
        chrome("alpha.rs"),
        editor(0, 7, 10, vec![line(10, "from alpha")]),
        chrome("beta.rs"),
        editor(1, 8, 10, vec![line(10, "from beta")]),
    ])
}

// ---- the editor ----------------------------------------------------------------------------

/// The working-changes view paints its files' lines.
///
/// The shape that blanked the terminal: a hunk whose lines start at file line 16 while the
/// view's own first line is 0, so anything indexing `lines[logical_line - first_view_line]`
/// reads past a seven-item list and draws nothing.
#[test]
fn a_patch_whose_hunk_starts_midfile_actually_paints() {
    let lines = (16..=22)
        .map(|n| line(n, &format!("fn f{}() {{}}", n + 1)))
        .collect();
    let app = app_showing(window_of(vec![chrome("a.rs"), editor(0, 7, 16, lines)]));
    let mut sim = simulate(&app);
    let rows = rows(&mut sim);
    let heading = row_of(&rows, "a.rs");
    let first = row_of(&rows, "fn f17() {}");
    let last = row_of(&rows, "fn f23() {}");
    assert!(
        heading < first && first < last,
        "heading, then the hunk top to bottom:\n{}",
        rows.join("\n")
    );
    snapshot(&mut sim, &app, "hunk-midfile");
}

/// Both files keep their heading when their line numbers collide. Chrome keyed by the line it
/// stood above put both headings on key 10 and lost one; this pins the GUI's half.
#[test]
fn two_files_starting_at_the_same_line_each_keep_their_heading() {
    let app = app_showing(two_files());
    let mut sim = simulate(&app);
    let rows = rows(&mut sim);
    let order: Vec<usize> = ["alpha.rs", "from alpha", "beta.rs", "from beta"]
        .iter()
        .map(|needle| row_of(&rows, needle))
        .collect();
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "each file under its own heading, in order (rows {order:?}):\n{}",
        rows.join("\n")
    );
    snapshot(&mut sim, &app, "two-files");
}

/// A click lands on the row it was painted at — the hit-test and the painter agree, which is
/// the property the terminal pins by pressing where `cell_of` found the text.
#[test]
fn a_click_on_a_painted_row_reports_that_row() {
    let mut app = app_showing(two_files());
    let mut sim = simulate(&app);
    sim.click("from beta").expect("the row is on screen");
    let pressed = sim.into_messages().find_map(|m| match m {
        Message::Editor(EditorEvent::Pressed { row, dcol, .. }) => Some((row, dcol)),
        _ => None,
    });
    // Rows: alpha heading, alpha's line, beta heading, beta's line — reported in layout units,
    // `UNITS_PER_ROW` to a row, so the press at the row's centre reads as row 3 and a half. The
    // column is the text's centre: 4.5 cells into a 9-char row.
    let (row, dcol) = pressed.expect("the editor reported a press");
    assert_eq!(
        (row / UNITS_PER_ROW as i64, dcol),
        (3, 4),
        "the press names beta's row and column (raw row {row})"
    );
    // And the app takes it as a cursor placement without complaint.
    let _ = app.update(Message::Editor(EditorEvent::Pressed {
        row,
        dcol,
        kind: ClickKind::Single,
        shift: false,
    }));
}

// ---- the chrome ----------------------------------------------------------------------------

/// A pinned toast advertises the key that clears it, and its body sits under the title.
#[test]
fn a_pinned_toast_names_its_dismiss_key() {
    let mut app = app_showing(two_files());
    let _ = app.toast(
        "Push failed",
        Some("remote rejected".into()),
        ToastKind::Error,
        None,
    );
    let mut sim = simulate(&app);
    let rows = rows(&mut sim);
    let title = row_of(&rows, "Push failed");
    assert!(
        rows[title].contains("Esc"),
        "a pinned toast names Esc: {:?}",
        rows[title]
    );
    assert!(
        row_of(&rows, "remote rejected") > title,
        "the body follows the title:\n{}",
        rows.join("\n")
    );
    snapshot(&mut sim, &app, "toast");
}

/// A confirm prompt asks its question over the editor.
#[test]
fn a_confirm_prompt_asks_its_question() {
    let mut app = app_showing(two_files());
    app.session.prompt = Some(Prompt::Confirm {
        kind: ConfirmKind::DiscardOnReload,
        action: ConfirmAction::ReloadDiscard,
    });
    let mut sim = simulate(&app);
    let rows = rows(&mut sim);
    row_of(&rows, "Discard local changes and reload?");
    snapshot(&mut sim, &app, "confirm");
}

/// The files picker opens over the editor with its query field and match count. Its rows are
/// rich text (match highlighting), which the tree does not report — they are checked by eye in
/// the snapshot, and by the picker's own geometry tests.
#[test]
fn the_files_picker_opens_over_the_editor() {
    let mut session = session_showing(two_files());
    let _ = session.open_picker(PickerKind::Files, None, None, false, None);
    let file = |path: &str| PickerItem::File {
        path_index: 0,
        relative_path: path.into(),
        match_indices: Vec::new(),
        git_status: None,
    };
    let picker = session.picker.as_mut().expect("open");
    assert!(picker.apply_update(PickerUpdateParams {
        kind: PickerKind::Files,
        generation: 0,
        offset: 0,
        items: Some(vec![file("src/main.rs"), file("src/lib.rs")]),
        total_matches: 2,
        total_candidates: 2,
        ticking: false,
        groups: Vec::new(),
        display_offset: Some(0),
        total_display_rows: Some(2),
        focus_run: None,
        center_on: None,
        explorer_peek_missing: false,
    }));
    let app = app_with(session);
    let mut sim = simulate(&app);
    let rows = rows(&mut sim);
    let query = row_of(&rows, "Find files…");
    assert!(
        rows[query].contains('2'),
        "the match count sits on the query row: {:?}",
        rows[query]
    );
    assert!(
        row_of(&rows, "alpha.rs") < query && query < row_of(&rows, "1:1"),
        "the overlay sits over the editor, above the status bar:\n{}",
        rows.join("\n")
    );
    snapshot(&mut sim, &app, "files-picker");
}
