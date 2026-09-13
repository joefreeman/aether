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
use aether_protocol::settings::ThemeMode;
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
        collapsed: false,
        element,
        buffer,
        rows: lines.len() as u32,
        first_row: ElementRow::ZERO,
        laid_out_by: LayoutOwner::Server,
        role: aether_protocol::ui::ElementRole::Field,
        first_buffer_line: first,
        lines,
    }
}

/// A box holding one **folded** element: no bottom border, so the whole box is the row its name
/// rides. What `render_window` sends for a collapsed tool call.
fn folded(element: u32, title: &str) -> ViewElement {
    use aether_protocol::ui::{Band, Edges, Sides};
    ViewElement::titled(
        Edges {
            border: Sides {
                top: 1,
                left: 1,
                right: 1,
                bottom: 0,
            },
            padding: Sides::ZERO,
            collapse: false,
        },
        Band::Chrome,
        vec![ViewElement::text(title, Vec::new())],
        vec![ViewElement::Editor {
            element,
            buffer: 10 + element as u64,
            rows: 0,
            first_row: ElementRow::ZERO,
            laid_out_by: LayoutOwner::Server,
            role: aether_protocol::ui::ElementRole::Field,
            collapsed: true,
            first_buffer_line: 0,
            lines: Vec::new(),
        }],
    )
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

/// A shell's input element: an editor like any other, marked so the client can find it.
fn input(element: u32, buffer: u64, lines: Vec<LogicalLineRender>) -> ViewElement {
    ViewElement::Editor {
        collapsed: false,
        element,
        buffer,
        rows: lines.len() as u32,
        first_row: ElementRow::ZERO,
        laid_out_by: LayoutOwner::Server,
        role: aether_protocol::ui::ElementRole::Input,
        first_buffer_line: 0,
        lines,
    }
}

/// A shell as the server composes one: each run in a box of its own, named on its top border for
/// where it ran and how it went, holding the command over its output; and the input last, in a box
/// of its own named only for the directory. Every box closes itself.
fn shell_view() -> Window {
    use aether_protocol::ui::{Band, Edges, Sides};
    let boxed = |title: &str, children: Vec<ViewElement>| {
        ViewElement::titled(
            Edges {
                border: Sides::all(1),
                padding: Sides::ZERO,
                collapse: false,
            },
            Band::Chrome,
            vec![ViewElement::text(title, Vec::new())],
            children,
        )
    };
    window_of(vec![
        boxed(
            "~/proj  ok",
            vec![chrome("echo one"), editor(0, 7, 0, vec![line(0, "one")])],
        ),
        chrome(""),
        boxed(
            "~/proj  ok",
            vec![chrome("echo two"), editor(1, 7, 1, vec![line(1, "two")])],
        ),
        chrome(""),
        boxed("~/proj", vec![input(2, 8, vec![line(0, "cargo build")])]),
    ])
}

/// A shell with `runs` completed runs, tall enough to need scrolling — the case the sticky tail
/// exists for. Same shape as [`shell_view`], repeated; `tail_loaded` says whether the last run's
/// output came with the push or only its height.
fn shell_of(runs: u32, tail_loaded: bool) -> Window {
    use aether_protocol::ui::{Band, Edges, Sides};
    let boxed = |title: &str, children: Vec<ViewElement>| {
        ViewElement::titled(
            Edges {
                border: Sides::all(1),
                padding: Sides::ZERO,
                collapse: false,
            },
            Band::Chrome,
            vec![ViewElement::text(title, Vec::new())],
            children,
        )
    };
    let mut children = Vec::new();
    for n in 0..runs {
        let out = if tail_loaded || n + 1 < runs {
            editor(n, 7, n, vec![line(n, "out")])
        } else {
            ViewElement::Editor {
                collapsed: false,
                element: n,
                buffer: 7,
                rows: 1,
                first_row: ElementRow::ZERO,
                laid_out_by: LayoutOwner::Server,
                role: aether_protocol::ui::ElementRole::Field,
                first_buffer_line: n,
                lines: Vec::new(),
            }
        };
        children.push(boxed("~/proj  ok", vec![chrome(&format!("echo {n}")), out]));
        children.push(chrome(""));
    }
    children.push(boxed("~/proj", vec![input(runs, 8, vec![line(0, "")])]));
    window_of(children)
}

fn tall_shell(runs: u32) -> Window {
    shell_of(runs, true)
}

/// [`tall_shell`] as a push leaves it when the run that just finished landed outside the slices
/// the viewport had loaded: the last run carries its output's height and none of its lines.
fn tall_shell_tail_unloaded(runs: u32) -> Window {
    shell_of(runs, false)
}

/// A `view/lines_changed` push carrying `window` — how a run's output reaches a client.
fn pushed(window: &Window) -> Message {
    use aether_protocol::envelope::{JsonRpc, Notification, NotificationMethod};
    use aether_protocol::viewport::ViewportLinesChanged;
    Message::Inbound(Some(Inbound::Notification(Notification {
        jsonrpc: JsonRpc,
        method: ViewportLinesChanged::NAME.into(),
        params: serde_json::json!({
            "viewport_id": 1,
            "buffer": 7,
            "revision": 2,
            "window": window,
        }),
    })))
}

/// A shell parked at the end follows its own output: the run you just submitted lands at the
/// bottom and the input stays on screen under it. Without it the view holds its scroll while the
/// content grows past it, and everything you type happens off the bottom of the screen.
#[test]
fn a_shell_at_the_end_follows_its_output() {
    let mut app = laid_out(app_showing(tall_shell(12)));
    app.scroll_px = app.max_scroll_px();
    let before = app.scroll_px;
    assert!(before > 0.0, "the view has to exceed the screen to scroll");
    // Every adopted window records the height the next one is compared against; this stands in
    // for the adoption that put the view on screen.
    let _ = app.sticky_tail_px();

    let _ = app.update(pushed(&tall_shell(18)));

    // Within a row of the new bottom: the policy puts the view's last row back on screen, which
    // leaves the content's own bottom padding off it.
    let bottom = app.max_scroll_px();
    let row = app.cell.expect("a laid-out shell has a cell").height;
    assert!(bottom > before, "the push has to have made the view taller");
    assert!(
        bottom - app.scroll_px <= row,
        "a shell at the end must follow its output down: {} of {bottom}",
        app.scroll_px
    );
}

/// Following the output has to ask for the screen it lands on. The push carries the new run's
/// height but not its lines — the viewport had not loaded rows that did not exist yet — so the
/// scroll to the tail arrives on rows nothing has fetched, and the run box paints empty until
/// some later scroll happens to ask. The terminal cannot reach this (it checks coverage once per
/// loop, whatever moved the view) and the browser's own scroll event asks for it; this shell sets
/// its offset directly, so the adoption is the only thing that can.
#[test]
fn following_the_output_asks_for_the_rows_it_lands_on() {
    let mut app = laid_out(app_showing(tall_shell(12)));
    app.scroll_px = app.max_scroll_px();
    let _ = app.sticky_tail_px();
    assert!(!app.fetch_in_flight, "nothing asked for yet");

    let _ = app.update(pushed(&tall_shell_tail_unloaded(18)));

    assert!(
        app.fetch_in_flight,
        "the view followed its output onto rows it never asked for"
    );
}

/// A plain file: one editor element and no chrome at all, windowing lines partway down the file
/// so nothing lands on the cursor's line 0 and every row paints the plain editor background.
fn plain_file() -> Window {
    window_of(vec![editor(
        0,
        7,
        10,
        (10..14)
            .map(|n| line(n, &format!("fn f{n}() {{}}")))
            .collect(),
    )])
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

/// [`two_files`] as a subscribe leaves it when the server's estimate of the first screen fell
/// short: the second file carries its height and no lines at all.
fn two_files_second_unloaded() -> Window {
    window_of(vec![
        chrome("alpha.rs"),
        editor(0, 7, 10, vec![line(10, "from alpha")]),
        chrome("beta.rs"),
        ViewElement::Editor {
            collapsed: false,
            element: 1,
            buffer: 8,
            rows: 40,
            first_row: ElementRow::ZERO,
            laid_out_by: LayoutOwner::Server,
            role: aether_protocol::ui::ElementRole::Field,
            first_buffer_line: 10,
            lines: Vec::new(),
        },
    ])
}

/// A subscribe answering with `window`. The buffer half is built from JSON so the fields a test
/// has no opinion about take their serde defaults rather than being spelled out here.
fn subscribed(window: Window) -> Message {
    let focus = serde_json::from_value(serde_json::json!({
        "element": 0,
        "buffer": {
            "buffer_id": 7,
            "language": null,
            "line_count": 4,
            "byte_count": 40,
            "revision": 1,
            "saved_revision": 1,
            "path": null,
        },
    }))
    .expect("a focus answer");
    Message::Subscribed(Box::new(ViewportSubscribeResult {
        viewport_id: 1,
        window,
        buffer_status: Default::default(),
        focus,
    }))
}

/// A subscribe's window is the server's estimate of the first screen, and it cannot estimate an
/// element this shell lays out — so the shell checks coverage against its own layout and asks for
/// what is missing, exactly as it does after every other window it adopts.
///
/// Without that check a composed view stood blank below the element the scroll named: the tree
/// carries every hunk's height, so the headings and rules painted and the files' text never
/// arrived, until a scroll or a cursor move happened to ask.
#[test]
fn a_subscribe_short_of_the_screen_fetches_the_rest() {
    let mut app = laid_out(app_showing(plain_file()));
    assert!(!app.fetch_in_flight, "nothing asked for yet");
    let _ = app.update(subscribed(two_files_second_unloaded()));
    assert!(
        app.fetch_in_flight,
        "the shell adopted a window that leaves the screen short and asked for nothing"
    );
}

/// The other half, so the check above cannot pass by fetching unconditionally: a window that does
/// cover the screen — every ordinary file view — asks for nothing.
#[test]
fn a_subscribe_that_covers_the_screen_fetches_nothing() {
    let mut app = laid_out(app_showing(plain_file()));
    let _ = app.update(subscribed(plain_file()));
    assert!(
        !app.fetch_in_flight,
        "a covered screen still cost a round trip"
    );
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

/// The editor's rows are the well and the app's ground is around them — in both modes.
///
/// Two probes and no more: a pixel inside a row of buffer text is the editor shade, and a pixel
/// in the empty pane below the last row is the ground. Everything else here is row layout, which
/// `Operation::text` can see; a background is a `fill`, so only the frame's own pixels can say
/// which shade landed where. Light runs the same probes because the two shades swap ends there,
/// and it is the only headless test that paints the light table at all.
#[test]
fn editor_rows_are_the_well_and_the_pane_around_them_is_the_ground() {
    for (mode, name) in [
        (ThemeMode::Dark, "plain-file"),
        (ThemeMode::Light, "plain-file-light"),
    ] {
        let mut app = app_showing(plain_file());
        app.session.theme = mode;
        let p = crate::theme::palette(mode);
        let rgb = |c: iced::Color| {
            let b = c.into_rgba8();
            [b[0], b[1], b[2]]
        };
        assert_ne!(
            rgb(p.bg),
            rgb(p.bg_app),
            "{mode:?}: the two shades are the point"
        );

        let mut sim = simulate(&app);
        let texts = seen(&mut sim);
        let row = texts
            .iter()
            .find(|s| s.visible && s.text == "fn f11() {}")
            .expect("the row is on the frame")
            .clone();
        // The status bar's cursor readout — the floor of the editor pane, so the ground probe
        // lands between the last row and it rather than on either.
        let status = texts
            .iter()
            .find(|s| s.visible && s.text.contains("1:1"))
            .expect("the status bar is on the frame")
            .clone();
        let frame = pixels(&mut sim, &app);
        let scale = frame.0 as f32 / WIDTH;
        let y_of = |y: f32| (y * scale) as usize;

        let in_row = y_of(row.bounds.y + row.bounds.height / 2.0);
        assert!(
            !columns_painted(&frame, in_row, rgb(p.bg)).is_empty(),
            "{mode:?}: a row of buffer text sits in the editor well"
        );
        assert!(
            columns_painted(&frame, in_row, rgb(p.bg_app)).is_empty(),
            "{mode:?}: and nothing on that row is ground"
        );

        let below = y_of((row.bounds.y + row.bounds.height + status.bounds.y) / 2.0);
        assert!(
            !columns_painted(&frame, below, rgb(p.bg_app)).is_empty(),
            "{mode:?}: below the last row the app's ground shows"
        );
        assert!(
            columns_painted(&frame, below, rgb(p.bg)).is_empty(),
            "{mode:?}: and no editor shade reaches past the rows"
        );

        snapshot(&mut sim, &app, name);
    }
}

/// The reading view is the app's **ground**, edge to edge.
///
/// The well is for text you can put a cursor in; a rendered document is read, so the page takes
/// the shade everything that is not a row of buffer text takes. (It painted the well until the
/// backgrounds were split by what a surface *is* rather than by which element carried it — and
/// before that it painted nothing at all, taking iced's own theme by accident.)
#[test]
fn the_reading_view_is_one_ground_from_edge_to_edge() {
    // A quote with two children and a list in it: the shape that showed the panel banding, one
    // page-coloured bar per child, when a child's focus wrapper assumed it sat on the page.
    let text = "# Reading\n\nProse sits on the app's ground.\n\n> A quote, holding a paragraph\n> and a list:\n>\n> - first item\n> - second item\n\nAfter the quote, with `a chip` in it.\n\n| Name | Role |\n| --- | --- |\n| Ada | Engineer |\n| Bo | Designer |\n";
    let mut read = aether_client::session::ReadView::loading(7);
    read.adopt(
        1,
        aether_client::markdown::parse(text),
        aether_protocol::ui::SourceLines::of(text),
    );
    let mut session = session_showing(plain_file());
    session.view.read = Some(read);
    let app = app_with(session);

    let p = crate::theme::palette(app.session.theme);
    let rgb = |c: iced::Color| {
        let b = c.into_rgba8();
        [b[0], b[1], b[2]]
    };
    let mut sim = simulate(&app);
    let status = seen(&mut sim)
        .into_iter()
        .find(|s| s.visible && s.text.contains("1:1"))
        .expect("the status bar is on the frame");
    let frame = pixels(&mut sim, &app);
    let scale = frame.0 as f32 / WIDTH;
    // Halfway down the empty part of the pane, below the document and above the status bar.
    let y = ((status.bounds.y / 2.0) * scale) as usize;
    assert!(
        !columns_painted(&frame, y, rgb(p.bg_app)).is_empty(),
        "the reading pane is the app's ground"
    );
    assert!(
        columns_painted(&frame, y, rgb(p.bg)).is_empty(),
        "…and no editor well shows inside it"
    );
    snapshot(&mut sim, &app, "reading-view");
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

/// A shell's rows: each run in a box named on its top border, and the input at the bottom in one
/// of its own. The name is on the border row rather than in a row of its own, so it lands one row
/// above the command it introduces and the box grows no taller for having a name.
#[test]
fn a_shell_paints_its_runs_then_its_input() {
    let app = app_showing(shell_view());
    let mut sim = simulate(&app);
    let rows = rows(&mut sim);
    // Matched whole: "one" is a substring of "echo one", and `row_of`'s `contains` would find
    // the command row and call it the output.
    let row_of = |needle: &str| {
        rows.iter()
            .position(|r| r.trim() == needle)
            .unwrap_or_else(|| panic!("no row reading {needle:?}:\n{}", rows.join("\n")))
    };
    let first_title = row_of("~/proj  ok");
    let first_command = row_of("echo one");
    let first_out = row_of("one");
    let second_command = row_of("echo two");
    let second_out = row_of("two");
    let input = row_of("cargo build");
    assert!(
        first_title + 1 == first_command
            && first_command < first_out
            && first_out < second_command
            && second_command < second_out
            && second_out < input,
        "each run named on the border above its command, then the input:\n{}",
        rows.join("\n")
    );
    // The input's box is named too, on the row directly above the line you type — nothing else
    // stands between them.
    assert_eq!(
        rows[input - 1].trim(),
        "~/proj",
        "the input's box says where you are:\n{}",
        rows.join("\n")
    );
    snapshot(&mut sim, &app, "shell-runs");
}

/// The running-shell indicator reaches the status bar, in the git operation's slot.
#[test]
fn a_running_shell_shows_in_the_status_bar() {
    let mut session = session_showing(shell_view());
    session.shell_runs.insert(
        session.view.view_id,
        aether_protocol::shell::RunState {
            run: 1,
            command: "cargo build".into(),
            status: aether_protocol::shell::RunStatus::Running,
        },
    );
    let app = app_with(session);
    let mut sim = simulate(&app);
    let rows = rows(&mut sim);
    assert!(
        rows.iter().any(|r| r.contains("\u{27f3} cargo build")),
        "the indicator names the command you are waiting on:\n{}",
        rows.join("\n")
    );
    snapshot(&mut sim, &app, "shell-indicator");
}

/// A file shown at a revision names itself by its path in the status bar, with the bracketed commit
/// it is shown at beside it — the pairing the buffers picker paints, here in the filename's slot.
#[test]
fn a_file_at_a_revision_shows_its_commit_in_the_status_bar() {
    let mut session = session_showing(shell_view());
    session.view.view_label = aether_client::labels::Label::at("a.rs", Some("abc1234".into()));
    let app = app_with(session);
    let mut sim = simulate(&app);
    let rows = rows(&mut sim);
    assert!(
        rows.iter()
            .any(|r| r.contains("a.rs") && r.contains("(abc1234)")),
        "the status bar names the file and the revision it is shown at:\n{}",
        rows.join("\n")
    );
}

/// An agent conversation as the server composes one: the prose bare, the machinery boxed, and the
/// reply laid out by *this shell* rather than wrapped by the server.
fn agent_view() -> Window {
    use aether_protocol::ui::{Band, Edges, Sides};
    let boxed = |title: &str, children: Vec<ViewElement>| {
        ViewElement::titled(
            Edges {
                border: Sides::all(1),
                padding: Sides::ZERO,
                collapse: false,
            },
            Band::Chrome,
            vec![ViewElement::text(title, Vec::new())],
            children,
        )
    };
    let prose = |element: u32, source: &str| ViewElement::Prose {
        element,
        blocks: aether_client::markdown::parse(source),
        source: aether_protocol::ui::SourceLines::of(source),
    };
    window_of(vec![
        chrome("You"),
        editor(0, 10, 0, vec![line(0, "why are semicolons dropped?")]),
        chrome(""),
        prose(1, AGENT_REPLY),
        chrome(""),
        boxed(
            "● Running cargo test",
            vec![editor(2, 12, 0, vec![line(0, "42 passed")])],
        ),
        chrome(""),
        input(3, 13, vec![line(0, "and now fix it")]),
    ])
}

const AGENT_REPLY: &str =
    "# Findings\n\nThe parser drops `;` in **two** places:\n\n- `stmt()`\n- `block()`";

/// A cell size and pane size, as the editor widget reports on its first frame. Without them the
/// prose layer has no idea how wide a gutter column is and declines to build.
fn laid_out(mut app: App) -> App {
    app.cell = Some(Size::new(8.0, 18.0));
    app.view_size = Size::new(WIDTH, HEIGHT);
    app
}

/// The bounds of a prose element's container, by id — the same thing `ProseMeasureProbe` reads, as
/// a selector so a test can ask the simulator for it.
#[derive(Clone)]
struct ProseBounds(iced::advanced::widget::Id);

impl Selector for ProseBounds {
    type Output = Rectangle;

    fn select(&mut self, candidate: Candidate<'_>) -> Option<Rectangle> {
        match candidate {
            Candidate::Container { id, bounds, .. } if id == Some(&self.0) => Some(bounds),
            _ => None,
        }
    }

    fn description(&self) -> String {
        format!("prose container {:?}", self.0)
    }
}

/// A **folded** tool call costs one row, and it is the row its title rides.
///
/// Row layout, which is what this harness is for — the cursorline fill that marks the focused one
/// is pixels, and deliberately not compared. What would break here is the fold spending a second
/// row on a bottom rule it should not have, which is the difference between a conversation that
/// skims and one that still scrolls.
#[test]
fn a_folded_call_is_one_titled_row() {
    let window = window_of(vec![
        chrome("You"),
        editor(0, 10, 0, vec![line(0, "fix the parser")]),
        chrome(""),
        folded(1, "Running cargo test"),
        chrome(""),
        folded(2, "Reading main.rs"),
        chrome(""),
        input(3, 13, vec![line(0, "and now this")]),
    ]);
    let mut session = session_showing(window);
    session.view.focused_element = 2;
    let app = laid_out(app_with(session));
    let mut sim = simulate(&app);
    let rows = rows(&mut sim);
    // In **pixels**, not in the index of the text-bearing rows: `rows` skips the blank chrome
    // between the boxes, so an index comparison would read the same whether or not each fold
    // spent a closing rule. That rule is exactly what this is here to catch.
    let y_of = |needle: &str| {
        seen(&mut simulate(&app))
            .into_iter()
            .find(|s| s.visible && s.text.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not on the frame:\n{}", rows.join("\n")))
            .bounds
            .y
    };
    // One row, measured off the frame itself rather than off `app.cell`: the speaker chrome and
    // the prompt under it are adjacent by construction, and the rasteriser's leading is its own.
    let row_px = y_of("fix the parser") - y_of("You");
    let gap = y_of("Reading main.rs") - y_of("Running cargo test");
    // The title row, then the blank chrome row between the boxes — and no rule below either name.
    assert!(
        (gap - row_px * 2.0).abs() < 1.0,
        "a folded box spent {gap} px where two rows are {}: it drew a rule below its name\n{}",
        row_px * 2.0,
        rows.join("\n")
    );
    // And the call's output is nowhere on screen, which is the point of the fold.
    assert!(
        !rows.iter().any(|r| r.contains("passed")),
        "a folded call painted its output:\n{}",
        rows.join("\n")
    );
    snapshot(&mut sim, &app, "agent-folded");
}

/// The reply renders as **real type**, in the layer over the editor: a container of its own,
/// starting where the grid puts the element and taller than the row the grid gives an unmeasured
/// one.
///
/// This is what the layer is answerable for. That the *blocks* look right is the reading view's
/// job, tested there and shared verbatim; what only this can go wrong at is placement — an earlier
/// version of the cell-based renderer drew a whole reply within a pixel of its first row. Run with
/// `AETHER_SNAPSHOT_DIR` to look at it.
#[test]
fn an_agent_reply_renders_as_prose() {
    let window = agent_view();
    let origin = aether_client::grid::element_origins(
        &window.root,
        &grid::Measured::at_resolution(crate::app::UNITS_PER_ROW),
    )[&1]
        .row
        .get();

    let mut session = session_showing(window);
    session.view.focused_element = 3;
    let mut app = laid_out(app_with(session));
    let (row_px, cell_w) = {
        let cell = app.cell.expect("laid out");
        (cell.height, cell.width)
    };
    let bounds = {
        let mut sim = simulate(&app);
        sim.find(ProseBounds(crate::app::prose_id(app.window, 1)))
            .expect("the reply has a container of its own in the layer")
    };

    // Where the grid put the element — the layer's whole job.
    let want_y = crate::editor::PAD + origin as f32 / crate::app::UNITS_PER_ROW as f32 * row_px;
    assert!(
        (bounds.y - want_y).abs() < 1.0,
        "the reply is at {} rather than at its origin {want_y}",
        bounds.y
    );
    // Proportional type, not the one row an unmeasured element stands at: a heading, a paragraph
    // and two bullets cannot fit in one.
    assert!(
        bounds.height > row_px * 4.0,
        "the reply came out {} px tall — one row is {row_px}",
        bounds.height
    );
    // Inset past the gutter, so it starts where the lines around it do.
    assert!(bounds.x >= cell_w);

    // Feed the measurement back the way the probe does, and the settled frame is what to look at.
    let _ = app.update(crate::app::Message::ProseMeasured(vec![(1, bounds.height)]));
    let mut sim = simulate(&app);
    snapshot(&mut sim, &app, "agent-reply");
}

/// A measured reply is **taller than the row an unmeasured one stands at**, and everything below it
/// moves down by the difference.
///
/// The heights arrive a frame late in this shell — proportional type has no height until it has
/// been drawn — so the thing to pin is that folding one in actually re-places the view. The bug
/// this stands against: the height going into the table but nothing re-clamping, so a conversation
/// could not be scrolled to its own bottom.
#[test]
fn a_measured_reply_extends_the_scrollable_height() {
    let window = agent_view();
    let unmeasured = aether_client::grid::total_rows(
        &window.root,
        &grid::Measured::at_resolution(crate::app::UNITS_PER_ROW),
    );

    let mut app = laid_out(app_showing(window));
    // Ten rows' worth of reply, as the probe would report it.
    let px = app.cell.unwrap().height * 10.0;
    let _ = app.update(crate::app::Message::ProseMeasured(vec![(1, px)]));

    let rendered = aether_client::grid::total_rows(
        &app.session.view.window.as_ref().unwrap().root,
        &app.measured,
    );
    assert!(
        rendered > unmeasured,
        "a measured reply ({rendered} rows) is no taller than the row an unmeasured one stands at \
         ({unmeasured}); this test can no longer tell whether the height was folded in"
    );
}

/// `Space u` from the editor lands the reading view with the focused block **on screen**.
///
/// The switch captures a content anchor and this shell hands the whole placement to
/// [`App::read_place_subscribed`] — `Message::Subscribed` stands the focus-change reveal down for
/// the reader precisely because that function is supposed to rest the focus itself. It did not:
/// the anchor branch returned first, so an anchor that named content far from the cursor (an
/// editor scrolled away from it, which is the normal way to end up switching) opened the reader on
/// that content with the focused block somewhere below the fold and nothing left to reveal it.
///
/// Driven through the real messages rather than the widgets, because keys never reach a simulated
/// widget tree and the reader's geometry arrives as a probe result either way.
#[test]
fn a_switch_into_the_reader_rests_the_focus_the_anchor_left_off_screen() {
    use aether_protocol::ui::SourceLines;
    use aether_protocol::viewport::ViewportWindowResult;

    // Forty paragraphs, two lines each: long enough that the cursor's block is far below an
    // anchor pinned to the top of the document.
    let text: String = (0..40).map(|n| format!("Paragraph {n}.\n\n")).collect();
    let blocks = aether_client::markdown::parse(&text);
    let stops = aether_client::markdown::stops(&blocks);

    // The editor of that file, scrolled to the top with the cursor way below the loaded slice —
    // so the capture pins the top line, as it does whenever the cursor is off screen.
    let mut app = laid_out(app_showing(window_of(vec![editor(
        0,
        7,
        0,
        (0..10).map(|n| line(n, "Paragraph 0.")).collect(),
    )])));
    app.session.view.buffer.cursor.position = aether_protocol::LogicalPosition { line: 60, col: 0 };
    app.session
        .capture_scroll_anchor(aether_protocol::coords::VisualRow(0), 20_000, &app.measured);

    // The reader's window arrives: one prose element carrying the parse and its line table.
    let _ = app.session.adopt_window(ViewportWindowResult {
        window: window_of(vec![ViewElement::Prose {
            element: 0,
            blocks: blocks.clone(),
            source: SourceLines::of(&text),
        }]),
    });
    assert!(
        app.session.view.read.is_some(),
        "the prose window did not become the reading view"
    );

    // What the probe would report: each block twice a row tall, in document order.
    let row_px = app.cell.unwrap().height;
    let view_h = 400.0;
    let spans: Vec<(u32, u32, f32)> = stops
        .iter()
        .enumerate()
        .map(|(i, s)| (s.span().start, s.span().end, i as f32 * 2.0 * row_px))
        .collect();
    let content_h = stops.len() as f32 * 2.0 * row_px;
    let _ = app.update(crate::app::Message::ReadMeasured(
        Some(crate::app::ReadGeometry {
            spans,
            content_h,
            view_h,
            offset: 0.0,
        }),
        crate::app::ReadThen::Placement,
    ));

    // The focused block is the cursor's, and it is on screen — not left below the fold by an
    // anchor that pinned the document's first line.
    let (_, start, end) = app.read_focus_key().expect("a focused block");
    let (top, bottom) = app
        .read_block_px((start, end))
        .expect("the block is measured");
    let scroll = app.read_scroll_px;
    assert!(
        top >= scroll && bottom <= scroll + view_h,
        "the focused block ({top}..{bottom}) is outside the viewport ({scroll}..{}) — the switch \
         placed the document and never revealed the focus",
        scroll + view_h
    );
}

/// Leaving the reader captures the anchor from **the reader's** scroller.
///
/// This shell has two — `read_scroll_px` for the reading view, `scroll_px` for the editor — and
/// picked between them on `session.view.read`. The core clears that at the keystroke, before the
/// shell runs the `SaveContentAnchor` the same keystroke produced, so the capture read the
/// editor's mirror: untouched since before the reader opened, so zero. The anchor pinned the top
/// of the document, and `Space u` back to the editor threw away however far down you had read.
///
/// The window is the honest witness — it still holds the prose — and is what both scroller
/// questions ask now.
#[test]
fn leaving_the_reader_anchors_where_the_reader_was() {
    use aether_client::effect::{Effect, Effects};
    use aether_protocol::ui::SourceLines;
    use aether_protocol::viewport::ViewportWindowResult;

    let text: String = (0..40).map(|n| format!("Paragraph {n}.\n\n")).collect();
    let blocks = aether_client::markdown::parse(&text);
    let stops = aether_client::markdown::stops(&blocks);

    let mut app = laid_out(app_showing(window_of(vec![ViewElement::Prose {
        element: 0,
        blocks: blocks.clone(),
        source: SourceLines::of(&text),
    }])));
    let _ = app.session.adopt_window(ViewportWindowResult {
        window: window_of(vec![ViewElement::Prose {
            element: 0,
            blocks,
            source: SourceLines::of(&text),
        }]),
    });

    // Measured as drawn, and scrolled well down — the cursor's block in the middle of the screen.
    let row_px = app.cell.unwrap().height;
    let spans: Vec<(u32, u32, f32)> = stops
        .iter()
        .enumerate()
        .map(|(i, s)| (s.span().start, s.span().end, i as f32 * 2.0 * row_px))
        .collect();
    let _ = app.update(crate::app::Message::ReadMeasured(
        Some(crate::app::ReadGeometry {
            spans,
            content_h: stops.len() as f32 * 2.0 * row_px,
            view_h: 400.0,
            offset: 0.0,
        }),
        crate::app::ReadThen::Refresh,
    ));
    app.session.view.buffer.cursor.position = aether_protocol::LogicalPosition { line: 60, col: 0 };
    // Block 30 sits at 60 rows down; rest the viewport just above it, as reading there would.
    app.read_scroll_px = 58.0 * row_px;
    app.read_view_h = 400.0;
    app.scroll_px = 0.0; // the editor's mirror: stale, and what this used to read

    // `Space u` out of the reader: the core drops the reading view at the keystroke, *then* the
    // shell runs the effect it produced.
    app.session.view.read = None;
    let _ = app.run_core(Effects::one(Effect::SaveContentAnchor));

    let anchored = app
        .session
        .relayout_anchor_position()
        .expect("the switch captured an anchor");
    assert_eq!(
        anchored.line, 60,
        "the anchor pinned line {} — the capture read the editor's scroller, not the reader's",
        anchored.line
    );
}

/// A quote is **one panel**, not a bar per child.
///
/// Its children are reading stops in their own right, so each carries a focus wrapper — and a
/// wrapper repaints an opaque background behind itself to keep the bar strip from bleeding
/// through. While that background was hard-coded to the page, every child of a quote painted a
/// page-coloured band across the quote's own panel: the quote came out striped, one bar per
/// paragraph and list item, which is exactly what a container is not.
#[test]
fn a_quote_paints_one_panel() {
    let text = "Before.\n\n> A quote, holding a paragraph\n> and a list:\n>\n> - first item\n> - second item\n\nAfter.\n";
    let mut read = aether_client::session::ReadView::loading(7);
    read.adopt(
        1,
        aether_client::markdown::parse(text),
        aether_protocol::ui::SourceLines::of(text),
    );
    let mut session = session_showing(plain_file());
    session.view.read = Some(read);
    let app = app_with(session);
    let p = crate::theme::palette(app.session.theme);
    let rgb = |c: iced::Color| {
        let b = c.into_rgba8();
        [b[0], b[1], b[2]]
    };
    let mut sim = simulate(&app);
    // The status bar shares this shade (it is the panel shade too), so the scan stops above it.
    let status = seen(&mut sim)
        .into_iter()
        .find(|s| s.visible && s.text.contains("1:1"))
        .expect("the status bar is on the frame");
    let frame = pixels(&mut sim, &app);
    let scale = frame.0 as f32 / WIDTH;
    let floor = ((status.bounds.y - 4.0) * scale) as usize;

    // Every row the quote's shade appears on, and the rows between the first and last of them.
    let rows: Vec<usize> = (0..floor)
        .filter(|y| !columns_painted(&frame, *y, rgb(p.md_panel_bg)).is_empty())
        .collect();
    assert!(
        rows.len() > 10,
        "the quote painted no panel at all (rows: {})",
        rows.len()
    );
    let (top, bottom) = (rows[0], rows[rows.len() - 1]);
    // The panel's own extent, from the rows that are unmistakably it.
    let cols = columns_painted(&frame, top + 2, rgb(p.md_panel_bg));
    let (left, right) = (cols[0] + 8, cols[cols.len() - 1] - 8);
    // Inside those bounds nothing may paint the *page*: a child that repainted the page behind
    // itself is precisely the banding, and it covers most of the panel's width where it happens.
    let banded: Vec<usize> = (top..=bottom)
        .filter(|y| {
            columns_painted(&frame, *y, rgb(p.bg_app))
                .iter()
                .filter(|x| (left..=right).contains(x))
                .count()
                > (right - left) / 4
        })
        .collect();
    assert!(
        banded.is_empty(),
        "{} rows inside the quote paint the page across it — a band per child, not one panel",
        banded.len()
    );
    // And the page still shows outside it, so the panel has edges.
    assert!(
        !columns_painted(&frame, top.saturating_sub(6), rgb(p.bg_app)).is_empty(),
        "no page above the quote"
    );
}
