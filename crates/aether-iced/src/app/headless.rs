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
use aether_protocol::ui::{Element as ViewElement, LayoutOwner};
use aether_protocol::viewport::{
    DiffMarker, DiffStage, LogicalLineRender, Segment, Window, WrappedRow,
};
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

/// The frame as pixels: `(width, height, rgba)`.
///
/// Everything this shell draws that is not a glyph — the box's rails and band, a gutter change
/// bar, the cursor's block — is a `fill`, and `Operation::text` (all a `Selector` ever sees)
/// reports none of it. So a column assertion about any of them has to read the frame itself. Not a
/// golden image: nothing here compares a whole frame, only where a known colour lands on a known
/// row, which is layout in the one alphabet this shell has for it.
fn pixels(sim: &mut Simulator<'_, Message>, app: &App) -> (usize, usize, Vec<u8>) {
    let shot = sim
        .snapshot(&base_theme(app))
        .expect("the frame draws headlessly");
    let dir = tempfile::tempdir().expect("scratch dir");
    let path = dir.path().join("frame");
    // Writes the PNG when the path is free, which is how the frame is got at: `Snapshot` keeps its
    // buffer to itself.
    assert!(shot.matches_image(&path).expect("write the frame"));
    // Written as `<name>-<renderer>.png`, and which renderer is the backend's business.
    let written = std::fs::read_dir(dir.path())
        .expect("scratch dir")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "png"))
        .expect("the frame was written");
    let png = std::fs::File::open(written).expect("the frame is readable");
    let mut reader = png::Decoder::new(std::io::BufReader::new(png))
        .read_info()
        .expect("a readable frame");
    let mut buf = vec![0; reader.output_buffer_size().expect("a frame that fits")];
    let info = reader.next_frame(&mut buf).expect("the frame's pixels");
    buf.truncate(info.buffer_size());
    (info.width as usize, info.height as usize, buf)
}

/// The columns of `row` painted in `colour`, as a fraction of the frame's width.
///
/// Fractions rather than pixels because the snapshot is rasterised at whatever scale the backend
/// picked; what the assertions are about is *which cell*, and a cell is a fixed share of the pane.
fn columns_painted(frame: &(usize, usize, Vec<u8>), y: usize, colour: [u8; 3]) -> Vec<usize> {
    let (w, _, rgba) = frame;
    (0..*w)
        .filter(|x| {
            let i = (y * w + x) * 4;
            rgba[i..i + 3] == colour
        })
        .collect()
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

/// A line with a change against its baseline, so the gutter draws a bar beside it.
fn added(n: u32, text: &str) -> LogicalLineRender {
    LogicalLineRender {
        change: aether_protocol::viewport::LineChange::Changed {
            marker: DiffMarker::Added,
            stage: DiffStage::Unstaged,
            emphasis: Vec::new(),
        },
        ..line(n, text)
    }
}

fn chrome(text: &str) -> ViewElement {
    ViewElement::chrome(vec![ViewElement::text(text, Vec::new())])
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
        root: ViewElement::column(children),
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

// ---- column layout -------------------------------------------------------------------------

/// A box's rails, its gutter change-bar and the cursor's block all land in the box's own columns.
///
/// Every one of these is a `fill`, so `columns()` cannot see any of them — which is how the GUI
/// shipped a box whose rails were buried under the chrome band, a change bar pinned to the pane
/// while its row sat two cells in, and a cursor block drawn at the pane's edge with the character
/// it belongs to highlighted somewhere else entirely. Three of one bug: a painter that knew the
/// inset in one place and not in the others.
#[test]
fn a_boxs_fills_land_in_the_boxs_own_columns() {
    use aether_protocol::ui::{Band, Edges, Sides};

    let mut app = app_showing(window_of(vec![ViewElement::framed(
        Edges {
            border: Sides::all(1),
            padding: Sides {
                left: 1,
                right: 1,
                ..Sides::ZERO
            },
            collapse: true,
        },
        Band::Chrome,
        vec![
            chrome("a.rs"),
            editor(0, 7, 10, vec![added(10, "from alpha")]),
        ],
    )]));
    app.session.view.buffer.cursor.position = LogicalPosition { line: 10, col: 0 };
    app.session.view.buffer.cursor.anchor = app.session.view.buffer.cursor.position;
    // Insert's bar, not normal's block: the block is the foreground colour, which is also every
    // glyph on the row, so nothing could tell the two apart. The bar is the accent and nothing
    // else on a text row is — and both are placed by the same arithmetic.
    app.session.view.mode = Mode::Insert;

    let mut sim = simulate(&app);
    let row = seen(&mut sim)
        .into_iter()
        .find(|s| s.visible && s.text == "from alpha")
        .expect("the row is on the frame");
    let frame = pixels(&mut sim, &app);
    // The snapshot is rasterised at the backend's own scale, so everything is measured in it.
    let scale = frame.0 as f32 / WIDTH;
    let y = ((row.bounds.y + row.bounds.height / 2.0) * scale) as usize;
    let px = |x: f32| (x * scale) as usize;

    let p = crate::theme::palette(app.session.theme);
    let rgb = |c: iced::Color| {
        let b = c.into_rgba8();
        [b[0], b[1], b[2]]
    };

    // A rail down each side, in the box's border cells: the first column of the pane and the last.
    let rails = columns_painted(&frame, y, rgb(p.fg_faint));
    let cell = px(row.bounds.x) / 3; // border, padding, gutter — then the text
    let left = *rails
        .iter()
        .find(|x| **x < cell)
        .unwrap_or_else(|| panic!("a rail in the box's left border cell (0..{cell}): {rails:?}"));
    let right = *rails
        .iter()
        .rev()
        .find(|x| **x >= frame.0 - cell)
        .unwrap_or_else(|| {
            panic!(
                "and one in its right border cell ({}..{}): {rails:?}",
                frame.0 - cell,
                frame.0
            )
        });

    // Down the middle of the cell each is drawn in, and so the same distance in from its own edge.
    // Against the cell's leading edge instead, the two sides read as different weights of line and
    // neither lines up with the terminal's `│`, whose ink is centred in its cell.
    let (in_from_left, in_from_right) = (left, frame.0 - 1 - right);
    assert!(
        in_from_left.abs_diff(in_from_right) <= 1,
        "the rails should be the same distance in from their own edges ({in_from_left} and \
         {in_from_right} of a {cell}px cell)"
    );
    assert!(
        in_from_left.abs_diff(cell / 2) <= 1,
        "…and that distance is half a cell, the middle of the border cell ({in_from_left} of \
         {cell})"
    );

    // The change bar rides with the row, inside the border rather than pinned to the pane.
    let bar = columns_painted(&frame, y, rgb(p.git_added));
    assert!(
        !bar.is_empty() && bar[0] >= cell && bar[0] < px(row.bounds.x),
        "the change bar belongs in the box's gutter cell, got {bar:?} (cell {cell})"
    );

    // And the cursor sits on the character it marks, not at the pane's edge.
    let caret = columns_painted(&frame, y, rgb(p.accent));
    assert!(
        !caret.is_empty(),
        "the cursor draws a bar in insert mode — placed without the row's inset it lands left of \
         the box's content and `fill_content` clips it away entirely"
    );
    // Within half a cell, since a rasterised fill and a reported bound round differently. The
    // failure this guards against is two whole cells wide.
    assert!(
        caret[0].abs_diff(px(row.bounds.x)) * 2 < cell,
        "the cursor starts where its row's text does ({} vs {}): drawn without the row's inset it \
         lands at the pane's edge, leaving two cursors on screen",
        caret[0],
        px(row.bounds.x)
    );
}

/// Where each text begins horizontally, paired with the text — the GUI's half of the column
/// layout.
///
/// The vertical half is pinned by the shared corpus
/// (`aether-client/tests/fixtures/painted_rows.json`, walked by `grid::painted_rows` and its
/// TypeScript mirror). The horizontal half cannot be shared — a cell is not a pixel — so each
/// shell pins its own: here, in `aether-tui/src/ui.rs`, and in `web/src/render.test.ts`.
fn columns(sim: &mut Simulator<'_, Message>) -> Vec<(f32, String)> {
    seen(sim)
        .into_iter()
        .filter(|s| s.visible)
        .map(|s| (s.bounds.x, s.text))
        .collect()
}

/// Buffer text and chrome text start at the same x — one gutter column in from the pane's edge —
/// so a file heading lines up with the code under it.
///
/// Frames move this number. Without it, "the box is a column out in the GUI only" is something a
/// reader has to notice.
#[test]
fn chrome_and_code_share_one_left_edge() {
    let app = app_showing(two_files());
    let mut sim = simulate(&app);
    let cols = columns(&mut sim);

    // Exact, not `contains`: the status bar shows a filename too, and "alpha.rs" ends with
    // "a.rs" — a substring match would silently start measuring the wrong widget.
    let x_of = |needle: &str| -> f32 {
        cols.iter()
            .find(|(_, t)| t == needle)
            .unwrap_or_else(|| panic!("`{needle}` should be on the frame: {cols:?}"))
            .0
    };
    let heading = x_of("alpha.rs");
    for needle in ["from alpha", "beta.rs", "from beta"] {
        assert!(
            (x_of(needle) - heading).abs() < 0.5,
            "`{needle}` starts at {} but the first heading starts at {heading}: {cols:?}",
            x_of(needle)
        );
    }
    // And that shared edge is inset from the pane, not flush with it: the gutter lives there.
    assert!(
        heading > 0.0,
        "the gutter column should sit left of the text (heading x {heading})"
    );
}

/// A box holds its rows in from the pane's edge.
///
/// The GUI's half of the same property the terminal pins in cells and the browser in `ch`: nothing
/// produces this tree yet, so it is built by hand — the painter has to be right before a producer
/// depends on it.
#[test]
fn a_box_insets_the_rows_inside_it() {
    use aether_protocol::ui::{Band, Edges, Sides};

    let plain = app_showing(window_of(vec![
        chrome("a.rs"),
        editor(0, 7, 10, vec![line(10, "from alpha")]),
    ]));
    let mut sim = simulate(&plain);
    let flush = columns(&mut sim)
        .into_iter()
        .find(|(_, t)| t == "from alpha")
        .expect("the row is on the frame")
        .0;

    let boxed_window = window_of(vec![ViewElement::framed(
        Edges {
            border: Sides {
                top: 1,
                left: 1,
                ..Sides::ZERO
            },
            padding: Sides {
                left: 1,
                ..Sides::ZERO
            },
            collapse: true,
        },
        Band::Chrome,
        vec![
            chrome("a.rs"),
            editor(0, 7, 10, vec![line(10, "from alpha")]),
        ],
    )]);
    let boxed = app_showing(boxed_window);
    let mut sim = simulate(&boxed);
    let cols = columns(&mut sim);
    let inset = cols
        .iter()
        .find(|(_, t)| t == "from alpha")
        .unwrap_or_else(|| panic!("the row should still be on the frame: {cols:?}"))
        .0;

    // Two cells in — one border, one padding — from where the same row sat unboxed.
    let cell = (inset - flush) / 2.0;
    assert!(
        inset > flush,
        "a boxed row should start right of an unboxed one ({inset} vs {flush})"
    );
    assert!(
        cell > 1.0,
        "and by two whole cells, not a hairline ({cell}px per cell)"
    );
    // The heading moves with it: a box indents its whole contents, chrome included.
    let heading = cols
        .iter()
        .find(|(_, t)| t == "a.rs")
        .expect("the heading is on the frame")
        .0;
    assert!(
        (heading - inset).abs() < 0.5,
        "chrome and code share the box's left edge ({heading} vs {inset})"
    );
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
