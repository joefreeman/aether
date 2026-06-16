//! Picker overlay rendering (the iced shell's half — state lives in `core::picker`):
//! the web client's panel styling, virtual-scrolled results, chip row + editor line.
//! RPC dispatch stays in `app.rs`; everything here is pure view building in a small
//! `PickerMsg` space the app maps.

pub use crate::core::picker::*;

use crate::chips::{self, Chip, ChipEditorField, ChipId};
use crate::theme;
use aether_protocol::git::GitStatus;
use aether_protocol::picker::{BufferDirtyState, PickerItem, PickerKind};
use iced::advanced::widget::Tree;
use iced::advanced::{layout, mouse, renderer, Layout, Widget};
use iced::widget::{column, container, row, text};
use iced::{Border, Color, Element, Length, Rectangle, Size};

/// A conventional rotating "loading" throbber — a ring of dots with a brightness comet trailing the
/// head, drawn with `fill_quad` (no canvas feature needed). `phase` (radians) is advanced over time
/// by the app's frame ticks while a search is in progress, so the rotation is smooth regardless of
/// how fast results stream in.
struct Spinner {
    phase: f32,
}

impl Spinner {
    const SIZE: f32 = 13.0;
    const DOTS: usize = 8;
    const DOT: f32 = 2.4;
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Spinner
where
    Renderer: renderer::Renderer,
{
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(Self::SIZE), Length::Fixed(Self::SIZE))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        _limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(Size::new(Self::SIZE, Self::SIZE))
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let b = layout.bounds();
        let cx = b.x + b.width / 2.0;
        let cy = b.y + b.height / 2.0;
        let radius = b.width / 2.0 - Self::DOT;
        for i in 0..Self::DOTS {
            let a = i as f32 / Self::DOTS as f32 * std::f32::consts::TAU;
            // Distance behind the rotating head (0 = at the head, brightest), normalised to 0..1.
            let d = (self.phase - a).rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU;
            let color = Color {
                a: 0.15 + 0.85 * (1.0 - d),
                ..theme::NORD8
            };
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: cx + radius * a.cos() - Self::DOT / 2.0,
                        y: cy + radius * a.sin() - Self::DOT / 2.0,
                        width: Self::DOT,
                        height: Self::DOT,
                    },
                    border: Border {
                        radius: (Self::DOT / 2.0).into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                color,
            );
        }
    }
}

impl<'a, Message, Theme, Renderer> From<Spinner> for Element<'a, Message, Theme, Renderer>
where
    Renderer: renderer::Renderer,
{
    fn from(s: Spinner) -> Self {
        Element::new(s)
    }
}

/// Scrollbar width (rail + scroller) — narrower than iced's default chrome.
const SCROLLBAR_W: f32 = 5.0;

/// Trailing slack (px) reserved past a chip-editor ghost layer's content, so the overlaid
/// `text_input` has headroom and won't scroll its value to keep the end-of-line caret visible
/// (iced scrolls once the caret reaches `width - 5.0`). Must exceed that 5px fudge — see
/// `field_with_ghost`.
const CARET_SLACK: f32 = 8.0;

/// Every display row (item or group header) is exactly this tall — what makes the
/// virtual-scroll spacer math exact. Shell geometry: the core speaks display *rows*;
/// this is where rows become pixels.
pub const ROW_H: f32 = 24.0;

/// The list viewport's height in px (shrinks below [`VISIBLE_ROWS`] for short lists, and
/// collapses entirely when there's nothing to list — a reserved blank row would read as a
/// missing entry).
pub fn list_height(state: &PickerState) -> f32 {
    // The synthetic "+ Create …" row is appended client-side (not in `total_display_rows`), so add a
    // row for it — otherwise the viewport is one row short and clips it.
    let rows = state.total_display_rows + state.pending_create().is_some() as u32;
    (rows.min(VISIBLE_ROWS as u32) as f32) * ROW_H
}

/// The display row a scroll offset of `y` px puts at the top of the list view — the px→row
/// conversion done at the shell edge before asking the core's `scrolled_refetch`.
pub fn first_visible_row(y: f32) -> u32 {
    (y / ROW_H).floor().max(0.0) as u32
}

/// Messages from the rendered panel (buttons/rows need `Clone`; the app maps these). Not `Copy`:
/// the query `text_input`'s `on_input` carries the new value as a `String` ([`PickerMsg::Query`]).
#[derive(Debug, Clone)]
pub enum PickerMsg {
    /// A row was clicked — absolute index into the match list.
    Click(u32),
    /// The results list scrolled (absolute y offset in px).
    Scrolled(f32),
    /// The pointer entered / left a row (drives the clickable-underline hover state).
    Hovered(u32),
    Unhovered(u32),
    /// A filter chip was clicked — row index (selection is virtual, like the keyboard path).
    ChipClicked(usize),
    /// The query `text_input` produced new text (session picker only — the controlled-input path;
    /// the boot chooser's picker keeps the fake-caret rendering and never emits this).
    Query(String),
    /// The chip editor's root-filter `text_input` produced new text (multi-root dir editor).
    EditorRoot(String),
    /// The chip editor's path/glob `text_input` produced new text.
    EditorPath(String),
    /// A chip-boundary key a focused `text_input` would otherwise swallow, forwarded to the core's
    /// keymap (`:` confirm-root, Backspace step-to-root). The app maps it to a `Message::Key`.
    CoreKey(crate::keymap::KeyCode),
}

/// Which boundary gesture a chip-editor field forwards to the core (a focused `text_input` captures
/// these, so the wrapper intercepts them — web/TUI parity). Shared with the save-as prompt, whose
/// root/path segments reuse [`field_with_ghost`] and the same `:`/Backspace boundaries.
#[derive(Clone, Copy)]
pub(crate) enum Boundary {
    /// No boundary key (glob field, single-root dir path).
    None,
    /// Root segment: `:` confirms the root and moves into the path.
    ConfirmRoot,
    /// Multi-root dir path segment: `Backspace` at the start steps back into the root.
    PathToRoot,
}

/// Query-input placeholder per picker — kept in sync with the web client's `PLACEHOLDER` map
/// and the TUI's `picker_placeholder`.
fn placeholder(kind: PickerKind) -> &'static str {
    match kind {
        PickerKind::Files => "Find files…",
        PickerKind::Buffers => "Switch buffer…",
        PickerKind::Grep => "Grep workspace…",
        PickerKind::Explorer => "Explore files…",
        PickerKind::Projects => "Select project…",
        PickerKind::Diagnostics => "List diagnostics…",
        PickerKind::LspServers => "List LSPs…",
        PickerKind::References => "List references…",
        PickerKind::DocumentSymbols => "Go to symbol…",
    }
}

const SANS: iced::Font = iced::Font {
    family: iced::font::Family::SansSerif,
    ..iced::Font::DEFAULT
};

/// Build the picker panel. `roots` are the project root paths (rows show a root label only in
/// multi-root projects, like the other clients). `scroll_y` is the shell-tracked scroll
/// offset of the results list (for the sticky-header pin).
/// `controlled` selects the query-input rendering: `true` (the session picker) draws a real
/// `text_input` synced to the core via [`PickerMsg::Query`]; `false` (the boot chooser, whose
/// query lives outside the core session) keeps the fake-caret rendering driven by key events.
pub fn overlay<'a>(
    state: &'a PickerState,
    roots: &'a [String],
    scroll_y: f32,
    spinner_phase: f32,
    controlled: bool,
    // Byte caret into `state.query`, used only by the fake-caret (`!controlled`) boot-chooser path;
    // the controlled `text_input` owns its own caret, so the session picker passes 0.
    query_cursor: usize,
) -> Element<'a, PickerMsg> {
    let mut panel = column![];

    // Input row: query (or a dim per-kind placeholder) with beam cursor, match counts on the
    // right. Sits on NORD0 against the panel's NORD1 (web's .picker-input-row). The explorer
    // leads with a `label: rel/` breadcrumb (project-relative, terminal format), flush
    // against the query; the placeholder only shows when there's no breadcrumb.
    let mut input = row![].align_y(iced::Alignment::Center);
    // Filter chips lead the row, before the explorer breadcrumb (docs/picker-filters.md).
    let chip_row = state.chip_row(roots);
    if !chip_row.is_empty() {
        let mut chips_el = row![].spacing(6).align_y(iced::Alignment::Center);
        for (i, c) in chip_row.iter().enumerate() {
            chips_el = chips_el.push(chip_el(c, i, state.chip_selected == Some(i)));
        }
        input = input.push(chips_el);
        input = input.push(iced::widget::Space::new().width(8));
    }
    let prefix = explorer_prefix(state, roots);
    if let Some(pfx) = &prefix {
        input = input.push(text(pfx.clone()).size(13).font(SANS).color(theme::NORD8));
    }
    // The breadcrumb / a non-empty chip row already says where typing will act, so the per-kind
    // placeholder is suppressed there (both the controlled and fake-caret paths honour this).
    let show_placeholder = prefix.is_none() && chip_row.is_empty();
    if controlled {
        // Session picker: a real `text_input` synced to the core (web parity). NORD6 value, NORD8
        // caret/selection, dim placeholder, no border/background (the input row's NORD0 is the
        // box). Width::Shrink so it sits flush after any breadcrumb rather than filling the row and
        // pushing the count off-screen.
        let ph = if show_placeholder {
            placeholder(state.kind)
        } else {
            ""
        };
        // `alt_passthrough` keeps Alt-nav chords (Alt-j/k/l) out of the query input (winit delivers
        // `Alt+letter` as text on some platforms, which the focused input would otherwise insert).
        let q_input = iced::widget::text_input(ph, &state.query)
            .id(query_input_id())
            .on_input(PickerMsg::Query)
            .font(SANS)
            .size(13)
            .padding(0)
            // `text_input` has no intrinsic content width — `Shrink` collapses it to nothing
            // (the typed text wouldn't show). Fill the row up to the count instead (so the
            // trailing Fill spacer below is skipped for this path).
            .width(Length::Fill)
            .style(|_theme, _status| iced::widget::text_input::Style {
                background: iced::Background::Color(iced::Color::TRANSPARENT),
                border: iced::Border::default(),
                icon: theme::NORD6,
                placeholder: theme::NORD3,
                value: theme::NORD6,
                selection: theme::NORD8,
            });
        // With chips present — and only while no chip is yet selected (the input still owns focus) —
        // Left / Backspace at the *start* of the query select the rightmost chip instead of editing
        // it: the browser tag-input gesture (web parity). Once a chip *is* selected the input is
        // blurred and the chip-row keys (navigate / remove / deselect) flow to the core directly.
        let wrapped = if !chip_row.is_empty() && state.chip_selected.is_none() {
            let last = chip_row.len() - 1;
            crate::alt_filter::alt_passthrough_intercept(
                q_input,
                state.query.clone(),
                move |key, at_start| {
                    use iced::keyboard::key::Named;
                    (at_start
                        && matches!(
                            key,
                            iced::keyboard::Key::Named(Named::ArrowLeft | Named::Backspace)
                        ))
                    .then_some(PickerMsg::ChipClicked(last))
                },
            )
        } else {
            crate::alt_filter::alt_passthrough(q_input)
        };
        input = input.push(wrapped);
    } else if state.query.is_empty() {
        let mut q = row![
            container(iced::widget::Space::new().width(2).height(15)).style(|_| {
                container::Style {
                    background: Some(theme::NORD8.into()),
                    ..container::Style::default()
                }
            }),
        ]
        .spacing(2)
        .align_y(iced::Alignment::Center);
        if show_placeholder {
            q = q.push(
                text(placeholder(state.kind))
                    .size(13)
                    .font(SANS)
                    .color(theme::NORD3),
            );
        }
        input = input.push(q);
    } else {
        let mut query_row = row![].align_y(iced::Alignment::Center);
        let pre = &state.query[..query_cursor];
        let post = &state.query[query_cursor..];
        if !pre.is_empty() {
            query_row = query_row.push(text(pre).size(13).font(SANS).color(theme::NORD6));
        }
        query_row = query_row.push(
            container(iced::widget::Space::new().width(2).height(15)).style(|_| container::Style {
                background: Some(theme::NORD8.into()),
                ..container::Style::default()
            }),
        );
        if !post.is_empty() {
            query_row = query_row.push(text(post).size(13).font(SANS).color(theme::NORD6));
        }
        input = input.push(query_row);
    }
    // The fake-caret query has intrinsic width, so a Fill spacer pushes the count to the right edge.
    // The controlled `text_input` is itself Fill, so it already does that — a second Fill would
    // split the row and shrink the input.
    if !controlled {
        input = input.push(iced::widget::Space::new().width(Length::Fill));
    }
    // A rotating throbber sits to the left of the count while a search is still streaming; the count
    // itself shows progress. The app drives `spinner_phase` from frame ticks for a smooth spin.
    if state.ticking {
        input = input.push(
            container(Spinner {
                phase: spinner_phase,
            })
            .padding(iced::Padding::ZERO.right(6)),
        );
    }
    // A filtered file/buffer list shows `matched/total`; an unfiltered list — and grep, where every
    // candidate is a hit — collapses to a single total (rather than the redundant "M/M").
    let counts = if state.total_matches == 0 {
        String::new()
    } else if state.total_candidates > state.total_matches {
        // `matched/total` only when the candidate set is a genuine larger superset; otherwise just
        // the count (guards against a stale `total_candidates`, e.g. 0, from an async picker's fill
        // push racing the view response — which would otherwise read as `106/0`).
        format!("{}/{}", state.total_matches, state.total_candidates)
    } else {
        format!("{}", state.total_matches)
    };
    input = input.push(text(counts).size(12).font(SANS).color(theme::NORD3_BRIGHT));
    // An empty grep query means "no search has run" — saying "No matches" there would read
    // as a failed search. Every other kind lists candidates without a query, so an empty
    // result set is informative.
    let unqueried_grep = state.kind == PickerKind::Grep && state.query.is_empty();
    // The Explorer's "+ Create …" row is content in its own right — don't also say "No matches"
    // when a brand-new name has zero existing matches.
    let show_empty_note = state.total_matches == 0
        && !state.ticking
        && !unqueried_grep
        && state.pending_create().is_none();
    // Nothing renders below the input (no rows, no message, no editor line): round its bottom
    // corners too, so the NORD0 row doesn't poke out of the panel's rounded border.
    let input_is_last = state.total_display_rows == 0
        && state.pending_create().is_none()
        && !show_empty_note
        && state.chip_editor.is_none();
    panel = panel.push(
        container(input)
            .width(Length::Fill)
            .padding([10, 12])
            .style(move |_| container::Style {
                background: Some(theme::NORD0.into()),
                // Round the top corners to nest inside the panel's radius — iced doesn't clip
                // children to the parent's rounded border (the web uses overflow: hidden).
                border: iced::Border {
                    radius: iced::border::Radius {
                        top_left: 5.0,
                        top_right: 5.0,
                        bottom_right: if input_is_last { 5.0 } else { 0.0 },
                        bottom_left: if input_is_last { 5.0 } else { 0.0 },
                    },
                    ..iced::Border::default()
                },
                ..container::Style::default()
            }),
    );
    // Chip-editor line (glob / dir), revealed below the input row so chips + query stay
    // visible while editing. The slot is ALWAYS present (zero-size when closed) — swapping
    // the tree shape would reset the scrollable's state below (keyed by tree position).
    panel = panel.push(editor_line(state, roots));
    // Separator below the input, only coloured when the list has anything to separate (the
    // web's `.picker-list.filled` border-top). The slot itself is always present.
    let filled = state.total_display_rows > 0;
    panel = panel.push(
        container(iced::widget::Space::new().width(Length::Fill).height(1)).style(move |_| {
            container::Style {
                background: filled.then(|| theme::NORD3.into()),
                ..container::Style::default()
            }
        }),
    );

    // Results: the fetched window rendered as uniform-height rows inside a native scrollable,
    // with spacers sized to the whole virtual result set (the web's virtual-scroll model —
    // grep's spacer math uses the server's display-row counts so group headers are exact).
    let rows = state.display_rows();
    let window_rows = rows.len() as u32;
    let window_base = state.window_base();
    let mut list = column![];
    list = list.push(iced::widget::Space::new().height(window_base as f32 * ROW_H));
    for r in rows {
        match r {
            DisplayRow::Header {
                path_index,
                relative_path,
            } => {
                list = list.push(grep_header(roots, path_index, relative_path));
            }
            DisplayRow::Item { abs, item } => {
                let selected = abs == state.selected;
                let hovered = state.hovered == Some(abs);
                let row_el = container(render_item(item, roots, hovered))
                    .width(Length::Fill)
                    .height(ROW_H)
                    .padding([3, 12])
                    .align_y(iced::alignment::Vertical::Center)
                    .style(move |_| container::Style {
                        background: selected.then(|| theme::NORD2.into()),
                        ..container::Style::default()
                    });
                list = list.push(
                    iced::widget::mouse_area(row_el)
                        .interaction(iced::mouse::Interaction::Pointer)
                        .on_press(PickerMsg::Click(abs))
                        .on_enter(PickerMsg::Hovered(abs))
                        .on_exit(PickerMsg::Unhovered(abs)),
                );
            }
            DisplayRow::Create { abs, name, is_dir } => {
                let selected = abs == state.selected;
                let label = if state.kind == aether_protocol::picker::PickerKind::Projects {
                    format!("+ Create project {name}")
                } else if is_dir {
                    format!("+ Create directory {name}/")
                } else {
                    format!("+ Create file {name}")
                };
                let row_el = container(
                    text(label)
                        .size(13)
                        .font(iced::Font {
                            style: iced::font::Style::Italic,
                            ..iced::Font::DEFAULT
                        })
                        .color(theme::NORD8),
                )
                .width(Length::Fill)
                .height(ROW_H)
                .padding([3, 12])
                .align_y(iced::alignment::Vertical::Center)
                .style(move |_| container::Style {
                    background: selected.then(|| theme::NORD2.into()),
                    ..container::Style::default()
                });
                list = list.push(
                    iced::widget::mouse_area(row_el)
                        .interaction(iced::mouse::Interaction::Pointer)
                        .on_press(PickerMsg::Click(abs))
                        .on_enter(PickerMsg::Hovered(abs))
                        .on_exit(PickerMsg::Unhovered(abs)),
                );
            }
        }
    }
    let below = state
        .total_display_rows
        .saturating_sub(window_base + window_rows);
    list = list.push(iced::widget::Space::new().height(below as f32 * ROW_H));

    let scroll = iced::widget::scrollable(list)
        .id(list_id())
        .width(Length::Fill)
        .height(list_height(state))
        .direction(iced::widget::scrollable::Direction::Vertical(
            iced::widget::scrollable::Scrollbar::new()
                .width(SCROLLBAR_W)
                .margin(0)
                .scroller_width(SCROLLBAR_W),
        ))
        .on_scroll(|vp| PickerMsg::Scrolled(vp.absolute_offset().y));

    // Sticky group header: pin the top visible row's file header over the list (web's
    // `position: sticky`). The stack is ALWAYS present with a pin slot — conditionally
    // changing the tree shape would reset the scrollable's state (iced keys widget state by
    // tree position), which is a scroll-to-top.
    let pinned: Option<(u32, String)> = {
        let first_visible = first_visible_row(scroll_y);
        first_visible.checked_sub(window_base).and_then(|rel| {
            state
                .display_rows()
                .into_iter()
                .nth(rel as usize)
                .and_then(|r| match r {
                    DisplayRow::Item { item, .. } => match item {
                        PickerItem::GrepHit {
                            path_index,
                            relative_path,
                            ..
                        } => Some((*path_index, relative_path.clone())),
                        _ => None,
                    },
                    // A header at the top pins itself (identical overlay, no flicker).
                    DisplayRow::Header {
                        path_index,
                        relative_path,
                    } => Some((path_index, relative_path.to_string())),
                    // The create row is Explorer-only; grep (the only picker with headers)
                    // never shows it, so it's never the pinned row.
                    DisplayRow::Create { .. } => None,
                })
        })
    };
    let pin_layer: Element<'_, PickerMsg> = match pinned {
        Some((path_index, relative_path)) => {
            container(grep_header(roots, path_index, &relative_path))
                .width(Length::Fill)
                .align_y(iced::alignment::Vertical::Top)
                // Stop short of the scrollbar lane rather than covering it.
                .padding(iced::Padding {
                    right: SCROLLBAR_W + 2.0,
                    ..iced::Padding::ZERO
                })
                .into()
        }
        None => iced::widget::Space::new().width(0).height(0).into(),
    };
    panel = panel.push(iced::widget::stack![scroll, pin_layer]);

    if show_empty_note {
        // The web's `.picker-empty` styling (italic, dim, list-row padding) — but a notch
        // brighter than its NORD3: that reads fine on the web panel, too faint here.
        panel = panel.push(
            container(
                text("No matches")
                    .size(13)
                    .font(SANS_ITALIC)
                    .color(theme::NORD3_BRIGHT),
            )
            .padding([6, 12]),
        );
    }

    let boxed = container(panel)
        .width(720)
        // 1px inset keeps the border ring visible around the (otherwise covering) input row.
        .padding(1)
        .style(|_| container::Style {
            background: Some(theme::NORD1.into()),
            border: iced::Border {
                color: theme::NORD3,
                width: 1.0,
                radius: 6.0.into(),
            },
            shadow: iced::Shadow {
                color: iced::Color::from_rgba8(0, 0, 0, 0.4),
                offset: iced::Vector::new(0.0, 12.0),
                blur_radius: 40.0,
            },
            ..container::Style::default()
        });
    // Dimmed full-screen backdrop behind the panel (web's `.overlay`).
    container(boxed)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center)
        .align_y(iced::alignment::Vertical::Top)
        .padding(iced::Padding {
            top: 56.0,
            ..iced::Padding::ZERO
        })
        .style(|_| container::Style {
            background: Some(iced::Color::from_rgba8(20, 24, 30, 0.5).into()),
            ..container::Style::default()
        })
        .into()
}

const SANS_BOLD: iced::Font = iced::Font {
    weight: iced::font::Weight::Bold,
    ..SANS
};

/// The results scrollable's id, for programmatic `scroll_to` (keyboard reveals).
pub fn list_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("picker-results")
}

/// The query `text_input`'s id — must match `OverlayField::PickerQuery::id()` so the shell's
/// focus task lands on it when the picker opens.
pub fn query_input_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("overlay-picker-query")
}

/// The chip editor's root-filter `text_input` id — must match `OverlayField::ChipRoot::id()`.
pub fn editor_root_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("overlay-chip-root")
}

/// The chip editor's path/glob `text_input` id — must match `OverlayField::ChipPath::id()`.
pub fn editor_path_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("overlay-chip-path")
}

/// One filter chip: compact label on a raised background; selected inverts; exclude globs
/// (leading `!`) tint red; only the whole-word chip underlines. Clicking selects (selection is
/// virtual — focus never leaves the query, exactly like the keyboard path).
fn chip_el<'a>(chip: &Chip, idx: usize, selected: bool) -> Element<'a, PickerMsg> {
    let exclude = chip.label.starts_with('!');
    let (bg, fg) = match (selected, exclude) {
        (true, true) => (theme::NORD11, theme::NORD0),
        (true, false) => (theme::NORD8, theme::NORD0),
        (false, true) => (theme::NORD2, theme::NORD11),
        (false, false) => (theme::NORD2, theme::NORD8),
    };
    let underline = matches!(chip.id, ChipId::Word);
    let spans: Vec<iced::widget::text::Span<'a>> =
        vec![iced::widget::span(truncate_chars(&chip.label, 28))
            .size(12)
            .font(SANS)
            .color(fg)
            .underline(underline)];
    let label = iced::widget::rich_text(spans);
    let el = container(label)
        .padding([1, 7])
        .style(move |_| container::Style {
            background: Some(bg.into()),
            border: iced::Border {
                radius: 4.0.into(),
                ..iced::Border::default()
            },
            ..container::Style::default()
        });
    iced::widget::mouse_area(el)
        .interaction(iced::mouse::Interaction::Pointer)
        .on_press(PickerMsg::ChipClicked(idx))
        .into()
}

/// An editable field of the chip editor, rendered as a controlled `text_input` over a ghost
/// layer (web parity — see `editorWrap`/`fillGhost` in `web/src/shell.ts`). The ghost layer is
/// the FULL suggestion (`typed + ghost suffix`) drawn dim gray; the `text_input` sits on top with
/// a transparent background and an opaque value colour, so it covers the gray prefix exactly and
/// only the suffix peeks out past the typed text. The two layers share font/size/padding/alignment,
/// so the typed glyphs overlap the gray prefix flush and the suffix lands right after the caret.
///
/// `invalid` paints the typed value red (the "Enter will refuse this" cue, matching the old
/// fake-caret rendering). `placeholder` shows only when the field is empty (the glob hint).
/// `on_input` carries the typed value back to the core via the shell's `OverlayInput` mapping.
#[allow(clippy::too_many_arguments)]
pub(crate) fn field_with_ghost<'a>(
    input: &chips::Input,
    ghost: Option<String>,
    invalid: bool,
    id: iced::advanced::widget::Id,
    placeholder: &str,
    on_input: fn(String) -> PickerMsg,
    hug: bool,
    boundary: Boundary,
) -> Element<'a, PickerMsg> {
    let color = if invalid { theme::NORD11 } else { theme::NORD6 };
    // The gray layer behind: the typed text (its prefix is covered by the opaque input value)
    // followed by the suggestion suffix (the only part that shows through).
    let suffix = ghost.filter(|g| !g.is_empty()).unwrap_or_default();
    // `text_input` hardcodes `Shaping::Advanced`; the bare `text` widget defaults to `Basic`. The
    // two shapers give different glyph advances, so without matching this the layers drift apart
    // character by character and the gray prefix leaks out from under the opaque value (and the
    // suffix lands in the wrong place). Match the shaper so the layers stay glyph-aligned.
    let ghost_text = text(format!("{}{}", input.text, suffix))
        .size(13)
        .font(SANS)
        .color(theme::NORD3_BRIGHT)
        .shaping(iced::widget::text::Shaping::Advanced)
        .wrapping(iced::widget::text::Wrapping::None);
    // The stack sizes to its base layer (the ghost). `text_input` scrolls its value left to keep
    // the caret visible once the caret's x reaches `width - 5.0` (iced's hardcoded fudge) — and with
    // the caret at the end of a box sized exactly to the content, that fires every time, shifting
    // the opaque value left of the ghost (the gray prefix then leaks out the left, the suffix off
    // the right). A fixed trailing spacer in the base gives the input ~CARET_SLACK px of headroom so
    // the offset stays 0 and the layers stay pinned. A `Space` (not trailing text) is immune to the
    // shaper trimming trailing whitespace out of the measured width.
    let ghost_layer: Element<'a, PickerMsg> =
        row![ghost_text, iced::widget::Space::new().width(CARET_SLACK)].into();
    // The controlled input on top: transparent background so the gray suffix shows through after
    // the typed text; opaque NORD6 (or red) value covers the gray prefix; NORD8 caret/selection.
    let field_inner = iced::widget::text_input(placeholder, &input.text)
        .id(id)
        .on_input(on_input)
        .font(SANS)
        .size(13)
        .padding(0)
        .width(Length::Fill)
        .style(move |_theme, _status| iced::widget::text_input::Style {
            background: iced::Background::Color(iced::Color::TRANSPARENT),
            border: iced::Border::default(),
            icon: color,
            placeholder: theme::NORD3_BRIGHT,
            value: color,
            selection: theme::NORD8,
        });
    // Forward the field's boundary key to the core's keymap instead of letting the focused input
    // swallow it (web/TUI parity).
    use crate::keymap::KeyCode;
    use iced::keyboard::key::Named;
    let field = match boundary {
        Boundary::None => crate::alt_filter::alt_passthrough(field_inner),
        Boundary::ConfirmRoot => {
            crate::alt_filter::alt_passthrough_intercept(field_inner, input.text.clone(), |key, _| {
                matches!(key.as_ref(), iced::keyboard::Key::Character(":"))
                    .then_some(PickerMsg::CoreKey(KeyCode::Char(':')))
            })
        }
        Boundary::PathToRoot => crate::alt_filter::alt_passthrough_intercept(
            field_inner,
            input.text.clone(),
            |key, at_start| {
                (at_start && matches!(key, iced::keyboard::Key::Named(Named::Backspace)))
                    .then_some(PickerMsg::CoreKey(KeyCode::Backspace))
            },
        ),
    };
    let stacked = iced::widget::stack![ghost_layer, field];
    if hug {
        // The root segment sizes to its content so the `:` separator sits flush after it (web's
        // `.hug`). The stack's width follows the ghost layer (the real content metric); the
        // `text_input`'s `Fill` then fills exactly that, rather than the whole row.
        container(stacked).width(Length::Shrink).into()
    } else {
        // The path/glob segment stretches across the rest of the row.
        stacked.into()
    }
}

/// The chip-editor line below the input row, or a zero-size placeholder (the slot must always
/// exist — see the call site). Mirrors the web's `.picker-editor-row`: `glob:`/`dir:` label,
/// then for multi-root dir editors a root typeahead segment, a `:` separator (shown once the
/// path is in play), and the root-relative path with directory ghost suggestions.
fn editor_line<'a>(state: &'a PickerState, roots: &'a [String]) -> Element<'a, PickerMsg> {
    let Some(ed) = &state.chip_editor else {
        return iced::widget::Space::new().width(0).height(0).into();
    };
    let labels = crate::labels::root_labels(roots);
    let multi_root = ed.is_dir() && roots.len() > 1;
    let mut line = row![].spacing(6).align_y(iced::Alignment::Center);
    let tag = if ed.is_dir() { "dir:" } else { "glob:" };
    line = line.push(text(tag).size(13).font(SANS).color(theme::NORD8));
    if multi_root {
        let invalid = ed.root_invalid(&labels);
        // Group the root segment with its `:` separator at zero spacing, so the colon sits flush
        // against the root rather than inheriting the row's 6px gap (the web's `margin-left: -6px`).
        // The row gap then still separates this group from the path that follows.
        let mut root_group = row![].spacing(0).align_y(iced::Alignment::Center);
        if ed.field == ChipEditorField::Root {
            let ghost = ed.root_ghost(&labels).map(|(_, suffix)| suffix);
            root_group = root_group.push(field_with_ghost(
                &ed.root_filter,
                ghost,
                invalid,
                editor_root_id(),
                "",
                PickerMsg::EditorRoot,
                true,
                Boundary::ConfirmRoot,
            ));
        } else {
            // Unfocused root: the chosen label in the breadcrumb blue — or the raw filter
            // text, red, when it matches nothing (never a fallback the commit would refuse).
            let display = if invalid {
                ed.root_filter.text.clone()
            } else {
                labels
                    .get(ed.chosen_root(&labels) as usize)
                    .cloned()
                    .unwrap_or_default()
            };
            let color = if invalid { theme::NORD11 } else { theme::NORD8 };
            root_group = root_group.push(text(display).size(13).font(SANS).color(color));
        }
        // The separator appears once the path is in play (focused, or already holding text) —
        // a fresh root prompt doesn't dangle a `:` off an unentered field.
        if ed.field == ChipEditorField::Path || !ed.input.text.is_empty() {
            root_group =
                root_group.push(text(":").size(13).font(SANS).color(theme::NORD3_BRIGHT));
        }
        line = line.push(root_group);
    }
    if ed.field == ChipEditorField::Path || !multi_root {
        let ghost = if ed.is_dir() { ed.path_ghost() } else { None };
        // Only a multi-root dir path can step back into a root; the glob field and a single-root
        // dir path have no root segment behind them.
        let path_boundary = if multi_root {
            Boundary::PathToRoot
        } else {
            Boundary::None
        };
        if !ed.is_dir() && ed.input.text.is_empty() {
            // Glob: no inline ghost (there's nothing to complete); the syntax-by-example hint
            // rides as the `text_input`'s placeholder instead (web parity).
            line = line.push(field_with_ghost(
                &ed.input,
                None,
                false,
                editor_path_id(),
                "*.rs · !*_test.rs · src/**",
                PickerMsg::EditorPath,
                false,
                Boundary::None,
            ));
        } else {
            line = line.push(field_with_ghost(
                &ed.input,
                ghost,
                ed.path_invalid(),
                editor_path_id(),
                "",
                PickerMsg::EditorPath,
                false,
                path_boundary,
            ));
        }
    } else if !ed.input.text.is_empty() {
        // Unfocused path: plain text (red when invalid) — no suggestion until it's focused.
        let color = if ed.path_invalid() {
            theme::NORD11
        } else {
            theme::NORD6
        };
        line = line.push(text(ed.input.text.clone()).size(13).font(SANS).color(color));
    }
    container(line)
        .width(Length::Fill)
        .padding([6, 12])
        .style(|_| container::Style {
            background: Some(theme::NORD0.into()),
            ..container::Style::default()
        })
        .into()
}

/// A grep group header row: bold file path on the panel background, [`ROW_H`] tall (it doubles
/// as the sticky pinned header, so it must fully cover the row beneath).
fn grep_header<'a>(
    roots: &[String],
    path_index: u32,
    relative_path: &str,
) -> Element<'a, PickerMsg> {
    let mut label = root_label(roots, path_index).unwrap_or_default();
    label.push_str(relative_path);
    container(text(label).size(13).font(SANS_BOLD).color(theme::NORD8))
        .width(Length::Fill)
        .height(ROW_H)
        .padding([3, 12])
        .align_y(iced::alignment::Vertical::Center)
        .style(|_| container::Style {
            background: Some(theme::NORD1.into()),
            ..container::Style::default()
        })
        .into()
}

/// A fixed-width leading bullet cell, so rows with and without a status dot line up.
fn dot_cell<'a>(color: Option<iced::Color>) -> Element<'a, PickerMsg> {
    let inner: Element<'a, PickerMsg> = match color {
        Some(c) => text("●").size(9).color(c).into(),
        None => iced::widget::Space::new().into(),
    };
    container(inner)
        .width(14)
        .align_x(iced::alignment::Horizontal::Center)
        .into()
}

/// A fixed-width leading cell holding a severity glyph (diagnostics picker) — same footprint as
/// [`dot_cell`] so rows stay aligned, and the same glyph the status-bar count shows.
fn glyph_cell<'a>(glyph: &'static str, color: iced::Color) -> Element<'a, PickerMsg> {
    container(text(glyph).size(13).font(SANS).color(color))
        .width(14)
        .align_x(iced::alignment::Horizontal::Center)
        .into()
}

/// Right-aligned dim metadata (line numbers, ranges, paths). Never wraps — rows are exactly
/// [`ROW_H`] tall, so a wrapped second line would spill into the row below.
fn meta<'a>(s: String) -> Element<'a, PickerMsg> {
    text(s)
        .size(12)
        .font(SANS)
        .color(theme::NORD3_BRIGHT)
        .wrapping(iced::widget::text::Wrapping::None)
        .into()
}

/// One row's content per item kind. Layout mirrors the web client's row model: optional
/// fixed-width bullet, primary text with match tinting, right-aligned meta.
fn render_item<'a>(
    item: &'a PickerItem,
    roots: &'a [String],
    hovered: bool,
) -> Element<'a, PickerMsg> {
    match item {
        PickerItem::File {
            path_index,
            relative_path,
            match_indices,
            git_status,
        } => {
            let mut r = row![dot_cell(git_status.map(git_status_color))]
                .spacing(6)
                .align_y(iced::Alignment::Center);
            r = r.push(highlighted(
                relative_path,
                match_indices,
                theme::NORD4,
                SANS,
                hovered,
            ));
            // Multi-root projects: the root's label, dim, after the path (web/terminal style).
            if let Some(label) = root_label(roots, *path_index) {
                r = r.push(
                    text(label.trim_end_matches('/').to_string())
                        .size(13)
                        .font(SANS)
                        .color(theme::NORD3_BRIGHT),
                );
            }
            r.into()
        }
        PickerItem::Buffer {
            display,
            status,
            match_indices,
            transient,
            ..
        } => {
            // Buffer-state dot on the right, matching the web picker and the status bar.
            let mut r = row![
                highlighted(
                    display,
                    match_indices,
                    theme::NORD4,
                    if *transient { SANS_ITALIC } else { SANS },
                    hovered,
                ),
                iced::widget::Space::new().width(Length::Fill),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center);
            if let Some(color) = dirty_color(*status) {
                r = r.push(text("●").size(9).color(color));
            }
            r.into()
        }
        PickerItem::GrepHit {
            line,
            preview,
            match_indices,
            ..
        } => {
            // The file lives in the group header; the row is the trimmed line + its number on
            // the right. Match indices shift down by the stripped leading-whitespace chars.
            // Long lines truncate (no wrapping — row heights must stay one line).
            let trimmed = preview.trim_start();
            let lead = (preview.chars().count() - trimmed.chars().count()) as u32;
            let shifted: Vec<u32> = match_indices
                .iter()
                .filter_map(|i| i.checked_sub(lead))
                .filter(|i| (*i as usize) < PREVIEW_MAX_CHARS)
                .collect();
            row![
                highlighted_owned(
                    truncate_chars(trimmed.trim_end(), PREVIEW_MAX_CHARS),
                    shifted,
                    theme::NORD4,
                    iced::Font::MONOSPACE,
                    hovered,
                ),
                iced::widget::Space::new().width(Length::Fill),
                meta(format!("{}", line + 1)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into()
        }
        PickerItem::Diagnostic {
            line,
            col,
            end_line,
            end_col,
            severity,
            message,
            match_indices,
        } => row![
            glyph_cell(theme::diag_glyph(*severity), theme::diagnostic_color(*severity)),
            highlighted(
                message.split('\n').next().unwrap_or(message),
                match_indices,
                theme::NORD4,
                SANS,
                hovered,
            ),
            iced::widget::Space::new().width(Length::Fill),
            meta(diag_range_label(*line, *col, *end_line, *end_col)),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into(),
        PickerItem::Reference {
            display_path,
            line,
            preview,
            match_indices,
            ..
        } => {
            let trimmed = preview.trim_start();
            let lead = (preview.chars().count() - trimmed.chars().count()) as u32;
            let shifted: Vec<u32> = match_indices
                .iter()
                .filter_map(|i| i.checked_sub(lead))
                .filter(|i| (*i as usize) < REFERENCE_PREVIEW_MAX_CHARS)
                .collect();
            // The `path:line` location shares the row with the preview, so both truncate:
            // the path segment-elides (the filename always survives) and the preview gets a
            // smaller cap than grep's — together they can't exceed one visual line.
            let loc = format!(
                "{}:{}",
                crate::app::truncate_path_label(display_path, REFERENCE_PATH_MAX_CHARS),
                line + 1
            );
            row![
                highlighted_owned(
                    truncate_chars(trimmed.trim_end(), REFERENCE_PREVIEW_MAX_CHARS),
                    shifted,
                    theme::NORD4,
                    iced::Font::MONOSPACE,
                    hovered,
                ),
                iced::widget::Space::new().width(Length::Fill),
                meta(loc),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into()
        }
        PickerItem::DirEntry {
            name,
            is_dir,
            match_indices,
            git_status,
        } => {
            // A status bullet for real changes; ignored entries dim their text instead.
            let changed = git_status.filter(|s| *s != GitStatus::Ignored);
            let base = if *git_status == Some(GitStatus::Ignored) {
                theme::NORD3_BRIGHT
            } else if *is_dir {
                theme::NORD8
            } else {
                theme::NORD4
            };
            // The trailing `/` rides inside the name text (match indices point into `name`
            // chars, so appending is safe) — a separate widget would inherit the row gap.
            let display = if *is_dir {
                format!("{name}/")
            } else {
                name.clone()
            };
            row![
                dot_cell(changed.map(git_status_color)),
                highlighted_owned(display, match_indices.clone(), base, SANS, hovered),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into()
        }
        PickerItem::Root {
            path_index,
            match_indices,
        } => {
            let name = roots
                .get(*path_index as usize)
                .map(|r| {
                    std::path::Path::new(r)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| r.clone())
                })
                .unwrap_or_default();
            row![
                dot_cell(None),
                highlighted_owned(name, match_indices.clone(), theme::NORD8, SANS, hovered),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into()
        }
        PickerItem::Project {
            name,
            match_indices,
        } => row![highlighted(
            name,
            match_indices,
            theme::NORD6,
            SANS,
            hovered
        )]
        .align_y(iced::Alignment::Center)
        .into(),
        PickerItem::LspServer {
            name,
            language,
            root_label,
            status,
            progress,
            match_indices,
            ..
        } => {
            // Health dot (busy colour while progress is in flight), name, then dim metadata:
            // language, monorepo sub-root, and the active operation.
            let busy =
                matches!(status, aether_protocol::lsp::LspStatus::Ready) && !progress.is_empty();
            let color = if busy {
                theme::NORD13
            } else {
                theme::lsp_status_color(status)
            };
            let mut m = language.clone();
            if !root_label.is_empty() {
                m.push_str(&format!(" · {root_label}"));
            }
            if let Some(p) = progress.first() {
                m.push_str(&format!(" · {}", p.title));
            }
            row![
                dot_cell(Some(color)),
                highlighted(name, match_indices, theme::NORD6, SANS, hovered),
                iced::widget::Space::new().width(Length::Fill),
                meta(m),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into()
        }
        PickerItem::Symbol {
            name,
            symbol_kind,
            detail,
            depth,
            context,
            match_indices,
            ..
        } => {
            // Nested members indent under their container; the name leads, then the dim
            // `detail` (signature) beside it, and the kind tag right-aligned. A
            // `context` row (an ancestor shown for tree context while filtering) renders dim,
            // like a non-selectable header above its matched descendants.
            let mut r = iced::widget::Row::new()
                .spacing(6)
                .align_y(iced::Alignment::Center);
            let indent = (*depth as usize) * 2;
            if indent > 0 {
                r = r.push(iced::widget::Space::new().width(Length::Fixed((indent * 6) as f32)));
            }
            if *context {
                r = r.push(meta(name.clone())); // dim, non-matching ancestor
            } else {
                r = r.push(highlighted(name, match_indices, theme::NORD6, SANS, hovered));
            }
            if !detail.is_empty() {
                r = r.push(meta(truncate_chars(detail, PREVIEW_MAX_CHARS)));
            }
            r = r.push(iced::widget::Space::new().width(Length::Fill));
            r = r.push(meta(symbol_kind.label().to_string()));
            r.into()
        }
    }
}

/// Display-width cap for code previews — one visual line, web's text-overflow stand-in.
/// ~80 mono chars is what fits the 720px panel beside the line-number meta.
const PREVIEW_MAX_CHARS: usize = 80;
/// Reference rows share the line with a `path:line` location, so their preview budget is
/// smaller, and the path itself segment-elides — between them a row can't wrap.
const REFERENCE_PREVIEW_MAX_CHARS: usize = 56;
const REFERENCE_PATH_MAX_CHARS: usize = 24;

/// `s` capped at `max` chars, with an ellipsis when something was cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// `12:3-7`, `12:3-15:7`, or `12:3` — the diagnostic's range, 1-based.
fn diag_range_label(line: u32, col: u32, end_line: u32, end_col: u32) -> String {
    if end_line <= line {
        if end_col > col + 1 {
            format!("{}:{}-{}", line + 1, col + 1, end_col)
        } else {
            format!("{}:{}", line + 1, col + 1)
        }
    } else {
        format!("{}:{}-{}:{}", line + 1, col + 1, end_line + 1, end_col)
    }
}

const SANS_ITALIC: iced::Font = iced::Font {
    style: iced::font::Style::Italic,
    ..SANS
};

/// `display` with the fuzzy-matched char offsets tinted — the match-highlight treatment.
/// Sans for names/paths/messages (web's panel font); pass `MONOSPACE` for code previews.
fn highlighted<'a>(
    display: &'a str,
    match_indices: &'a [u32],
    base: iced::Color,
    font: iced::Font,
    underline: bool,
) -> Element<'a, PickerMsg> {
    highlighted_owned(
        display.to_string(),
        match_indices.to_vec(),
        base,
        font,
        underline,
    )
}

/// [`highlighted`] over owned data, for displays derived on the fly (trimmed grep previews).
fn highlighted_owned<'a>(
    display: String,
    match_indices: Vec<u32>,
    base: iced::Color,
    font: iced::Font,
    underline: bool,
) -> Element<'a, PickerMsg> {
    let mut spans: Vec<iced::widget::text::Span<'a>> = Vec::new();
    let mut run = String::new();
    let mut run_matched = false;
    let flush = |run: &mut String, matched: bool, spans: &mut Vec<_>| {
        if run.is_empty() {
            return;
        }
        let color = if matched { theme::NORD13 } else { base };
        spans.push(
            iced::widget::span(std::mem::take(run))
                .size(13)
                .font(font)
                .color(color)
                .underline(underline),
        );
    };
    for (ci, ch) in display.chars().enumerate() {
        let matched = match_indices.contains(&(ci as u32));
        if matched != run_matched {
            flush(&mut run, run_matched, &mut spans);
            run_matched = matched;
        }
        run.push(ch);
    }
    flush(&mut run, run_matched, &mut spans);
    iced::widget::rich_text(spans).into()
}

/// The explorer's input-row breadcrumb: `label: rel/` — root label only in multi-root
/// projects, both parts empty at a root's top, nothing in roots mode (the rows already say
/// "pick a root"). Mirrors the TUI's `explorer_path_parts`.
fn explorer_prefix(state: &PickerState, roots: &[String]) -> Option<String> {
    if state.kind != PickerKind::Explorer {
        return None;
    }
    let dir = state.directory.as_deref()?;
    let (idx, rel) = crate::app::strip_longest_root(dir, roots)?;
    let mut out = String::new();
    if roots.len() > 1 {
        if let Some(root) = roots.get(idx as usize) {
            let name = std::path::Path::new(root)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.clone());
            out.push_str(&format!("{name}: "));
        }
    }
    if !rel.is_empty() {
        out.push_str(&format!("{rel}/"));
    }
    (!out.is_empty()).then_some(out)
}

/// Root label prefix (`rootname/`) for multi-root projects; `None` with a single root.
fn root_label(roots: &[String], path_index: u32) -> Option<String> {
    if roots.len() < 2 {
        return None;
    }
    let root = roots.get(path_index as usize)?;
    let name = std::path::Path::new(root)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.clone());
    Some(format!("{name}/"))
}

fn git_status_color(s: GitStatus) -> iced::Color {
    match s {
        GitStatus::Conflicted | GitStatus::Deleted => theme::NORD11,
        GitStatus::Modified => theme::NORD13,
        GitStatus::Added => theme::NORD14,
        GitStatus::Untracked => theme::NORD14,
        GitStatus::Ignored => theme::NORD3_BRIGHT,
    }
}

/// Buffer-state dot colour — same precedence/colours as the status bar.
fn dirty_color(s: BufferDirtyState) -> Option<iced::Color> {
    match s {
        BufferDirtyState::Clean => None,
        BufferDirtyState::Unsaved => Some(theme::NORD9),
        BufferDirtyState::ExternallyModified => Some(theme::NORD12),
        BufferDirtyState::ExternallyDeleted => Some(theme::NORD11),
    }
}
