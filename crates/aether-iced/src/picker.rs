//! Picker overlay state + rendering (files / buffers / grep / diagnostics / references).
//!
//! The server owns the candidate cache, fuzzy matcher, and ranked snapshot per
//! `(client, kind)`; this module owns what the protocol leaves to the client — the input line,
//! the highlighted row, and the subscribed window — and renders the web client's picker panel
//! (top-centred, query input + rows with match highlighting). RPC dispatch stays in `app.rs`;
//! everything here is state + pure view building in a small `PickerMsg` space the app maps.

use crate::chips::{self, Chip, ChipEditor, ChipEditorField, ChipId, ChipValue};
use crate::theme;
use aether_protocol::git::GitStatus;
use aether_protocol::picker::{
    BufferDirtyState, PickerFilters, PickerItem, PickerKind, PickerUpdateParams,
};
use iced::widget::{column, container, row, text};
use iced::{Element, Length};

/// Rows the panel shows at once.
pub const VISIBLE_ROWS: usize = 18;
/// Window size requested from the server (over-fetched so small moves don't refetch).
pub const FETCH_LIMIT: u32 = 90;
/// Every display row (item or group header) is exactly this tall — what makes the
/// virtual-scroll spacer math exact.
pub const ROW_H: f32 = 24.0;
/// Scrollbar width (rail + scroller) — narrower than iced's default chrome.
const SCROLLBAR_W: f32 = 5.0;

/// Messages from the rendered panel (buttons/rows need `Clone`; the app maps these).
#[derive(Debug, Clone, Copy)]
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
}

/// The active buffer/project a fresh Buffers/Projects open should skip over when defaulting
/// its highlight (see [`PickerState::default_skip`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultSkip {
    Buffer(aether_protocol::BufferId),
    Project(String),
}

impl DefaultSkip {
    fn matches(&self, item: &PickerItem) -> bool {
        match (self, item) {
            (DefaultSkip::Buffer(id), PickerItem::Buffer { buffer_id, .. }) => id == buffer_id,
            (DefaultSkip::Project(active), PickerItem::Project { name, .. }) => active == name,
            _ => false,
        }
    }
}

/// How to scroll the highlight into view when the next update lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reveal {
    /// Scroll the minimum to bring the row inside the viewport (keyboard moves).
    Minimal,
    /// Align the row to the top unless already visible (grep file-jumps — context below).
    Top,
}

pub struct PickerState {
    pub kind: PickerKind,
    pub query: String,
    /// Byte cursor within `query`.
    pub cursor: usize,
    pub generation: u64,
    /// The fetched window starting at `offset` (absolute index into the match list).
    pub items: Vec<PickerItem>,
    pub offset: u32,
    /// Absolute index of the highlighted row.
    pub selected: u32,
    pub total_matches: u32,
    pub total_candidates: u32,
    pub ticking: bool,
    /// The list's scroll position in px (native scrollable; tracked for sticky-header math
    /// and keyboard reveals).
    pub scroll_y: f32,
    /// Display-row index of the fetched window's first row (grep: headers above included,
    /// from `grep_display_offset`; other kinds: equals `offset`). Sizes the top spacer.
    pub display_offset: u32,
    /// Total display rows in the whole result set (grep: hits + group headers). Sizes the
    /// virtual-scroll spacers.
    pub total_display_rows: u32,
    /// Item to re-highlight when the first matching update arrives (`center_on` echo).
    /// Matched by identity ([`item_key`]) — the listed item carries live decoration
    /// (git status, match indices) the anchor doesn't.
    pub pending_center: Option<PickerItem>,
    /// Fresh-open default highlight: land on the first item that *isn't* the client's active
    /// buffer/project, so Enter is a quick flip to the previous one. By identity, not "row 1"
    /// — the buffers MRU is shared across clients, so another client's activity can put any
    /// buffer at the top. One-shot: the first push with items decides and clears it.
    pub default_skip: Option<DefaultSkip>,
    /// Scroll the highlight into view when the next update lands (set by keyboard moves that
    /// forced a refetch and by centred opens — scroll-driven refetches must NOT yank the view).
    pub reveal_on_update: Option<Reveal>,
    /// The row under the pointer (underlined, web's hover affordance).
    pub hovered: Option<u32>,
    /// Explorer: the canonical directory being listed, echoed by `picker/view`.
    pub directory: Option<String>,
    /// Explorer: the listed directory's parent, when still inside the project boundary.
    pub directory_parent: Option<String>,
    /// The filter set in effect, stored as the ordered chip list — the client's single source
    /// of truth (docs/picker-filters.md). The wire `PickerFilters` is derived per send and
    /// converted back on open/resume; insertion order is session-ephemeral.
    pub chips: Vec<ChipValue>,
    /// Index into the chip row. While set, editing keys act on the chip (Enter edits,
    /// Backspace/Delete removes, Left/Right move). Entered via Left/Backspace at query
    /// cursor 0.
    pub chip_selected: Option<usize>,
    /// Below-input editor line for valued chips (glob / dir); owns all keys while open.
    pub chip_editor: Option<ChipEditor>,
}

impl PickerState {
    pub fn new(kind: PickerKind) -> Self {
        PickerState {
            kind,
            query: String::new(),
            cursor: 0,
            generation: 0,
            items: Vec::new(),
            offset: 0,
            selected: 0,
            total_matches: 0,
            total_candidates: 0,
            ticking: true,
            scroll_y: 0.0,
            display_offset: 0,
            total_display_rows: 0,
            pending_center: None,
            default_skip: None,
            reveal_on_update: None,
            hovered: None,
            directory: None,
            directory_parent: None,
            chips: Vec::new(),
            chip_selected: None,
            chip_editor: None,
        }
    }

    /// The rendered chip row, derived from the stored list.
    pub fn chip_row(&self, project_paths: &[String]) -> Vec<Chip> {
        chips::derive_chips(&self.chips, project_paths)
    }

    /// The wire filter set the active chips fold into — built per send.
    pub fn wire_filters(&self) -> PickerFilters {
        chips::wire_filters(&self.chips)
    }

    /// Adopt a wire filter set (open/resume), replacing the chip list.
    pub fn adopt_filters(&mut self, f: &PickerFilters) {
        self.chips = chips::adopt_filters(f);
        self.chip_selected = None;
        self.chip_editor = None;
    }

    /// The dir scope behind chip `i`, when chip `i` is a dir — the editor's pre-fill.
    pub fn dir_value(&self, i: usize) -> Option<&aether_protocol::picker::ScopedPath> {
        match self.chips.get(i) {
            Some(ChipValue::Dir(d)) => Some(d),
            _ => None,
        }
    }

    /// The glob behind chip `i`, when chip `i` is a glob — the editor's pre-fill.
    pub fn glob_value(&self, i: usize) -> Option<&str> {
        match self.chips.get(i) {
            Some(ChipValue::Glob(g)) => Some(g.as_str()),
            _ => None,
        }
    }

    /// The highlighted item, when it's inside the fetched window.
    pub fn selected_item(&self) -> Option<&PickerItem> {
        self.items
            .get(self.selected.saturating_sub(self.offset) as usize)
    }

    /// Apply a `picker/update` push. Stale pushes (older generation, other window) are
    /// discarded per the protocol. Returns false when discarded.
    pub fn apply_update(&mut self, u: PickerUpdateParams) -> bool {
        if u.kind != self.kind || u.generation != self.generation || u.offset != self.offset {
            return false;
        }
        self.items = u.items;
        self.total_matches = u.total_matches;
        self.total_candidates = u.total_candidates;
        self.ticking = u.ticking;
        self.display_offset = u.grep_display_offset.unwrap_or(u.offset);
        self.total_display_rows = u.grep_total_display_rows.unwrap_or(u.total_matches);
        if let Some(center) = self.pending_center.take() {
            let key = item_key(&center);
            if let Some(pos) = self.items.iter().position(|i| item_key(i) == key) {
                self.selected = self.offset + pos as u32;
            } else {
                self.pending_center = Some(center); // not in this window yet
            }
        } else if !self.items.is_empty() {
            if let Some(skip) = self.default_skip.take() {
                let pos = self
                    .items
                    .iter()
                    .position(|i| !skip.matches(i))
                    .unwrap_or(0); // every item is the active one (single open buffer)
                self.selected = self.offset + pos as u32;
            }
        }
        if self.total_matches > 0 {
            self.selected = self.selected.min(self.total_matches - 1);
        } else {
            self.selected = 0;
        }
        true
    }

    /// Move the highlight by `delta`, returning the new window offset to fetch when the
    /// highlight left the fetched window (the caller sends `picker/view`).
    pub fn move_selection(&mut self, delta: i64) -> Option<u32> {
        if self.total_matches == 0 {
            return None;
        }
        let max = self.total_matches as i64 - 1;
        self.selected = (self.selected as i64 + delta).clamp(0, max) as u32;
        let in_window = self.selected >= self.offset
            && self.selected < self.offset + self.items.len() as u32;
        if in_window {
            return None;
        }
        self.reveal_on_update = Some(Reveal::Minimal);
        Some(self.selected.saturating_sub(FETCH_LIMIT / 2))
    }

    /// The fetched window as uniform display rows: group headers interleaved before each
    /// file's first hit (grep), every row exactly [`ROW_H`] tall.
    fn display_rows(&self) -> Vec<DisplayRow<'_>> {
        let mut rows = Vec::with_capacity(self.items.len() + 8);
        let mut last_file: Option<(u32, &str)> = None;
        for (i, item) in self.items.iter().enumerate() {
            if let PickerItem::GrepHit {
                path_index,
                relative_path,
                ..
            } = item
            {
                let f = (*path_index, relative_path.as_str());
                if last_file != Some(f) {
                    last_file = Some(f);
                    rows.push(DisplayRow::Header {
                        path_index: *path_index,
                        relative_path,
                    });
                }
            }
            rows.push(DisplayRow::Item {
                abs: self.offset + i as u32,
                item,
            });
        }
        rows
    }

    /// Display-row index where the rendered window's FIRST row sits in the whole virtual
    /// list. `display_offset` is the first *item*'s row; when the window leads with a group
    /// header, that header occupies the row just above (the server counted it there — or, for
    /// a mid-file window start, it stands in for the hit row the spacer would otherwise
    /// cover), so the window starts one row earlier.
    fn window_base(&self) -> u32 {
        let leads_with_header = self
            .items
            .first()
            .is_some_and(|i| matches!(i, PickerItem::GrepHit { .. }));
        self.display_offset
            .saturating_sub(leads_with_header as u32)
    }

    /// The highlighted item's display-row index in the whole virtual list, when it's inside
    /// the fetched window.
    pub fn selected_display_row(&self) -> Option<u32> {
        let base = self.window_base();
        let rows = self.display_rows();
        rows.iter()
            .position(|r| matches!(r, DisplayRow::Item { abs, .. } if *abs == self.selected))
            .map(|i| base + i as u32)
    }

    /// The list viewport's height in px (shrinks below [`VISIBLE_ROWS`] for short lists, and
    /// collapses entirely when there's nothing to list — a reserved blank row would read as a
    /// missing entry).
    pub fn list_height(&self) -> f32 {
        (self.total_display_rows.min(VISIBLE_ROWS as u32) as f32) * ROW_H
    }

    /// After a native scroll to `y` px: does the view need a re-fetched window? Returns the
    /// estimated item offset to request. Display rows ≈ items (headers are a minority), so
    /// the estimate maps display rows back to items proportionally; the server clamps.
    pub fn scrolled_refetch(&self, y: f32) -> Option<u32> {
        if self.items.is_empty() || self.total_display_rows == 0 {
            return None; // nothing fetched yet / refetch already in flight
        }
        let first_visible = (y / ROW_H).floor().max(0.0) as u32;
        let last_visible = first_visible + VISIBLE_ROWS as u32;
        let base = self.window_base();
        let window_rows = self.display_rows().len() as u32;
        let window_end = base + window_rows;
        let needs = first_visible < base
            || (last_visible > window_end && window_end < self.total_display_rows);
        if !needs {
            return None;
        }
        let ratio = self.total_matches as f32 / self.total_display_rows as f32;
        let est_item = (first_visible as f32 * ratio) as u32;
        Some(est_item.saturating_sub(FETCH_LIMIT / 2))
    }
}

/// Stable identity of a picker item, so centering anchors match the *live* listed item (which
/// carries decoration — git status, match indices — the anchor doesn't). Mirrors the TUI's
/// `item_key` / the web's `itemKey`.
#[derive(PartialEq)]
pub enum ItemKey<'a> {
    File(u32, &'a str),
    Buffer(aether_protocol::BufferId),
    Grep(u32, &'a str, u32, u32),
    Diagnostic(u32, u32),
    DirEntry(&'a str),
    Root(u32),
    Project(&'a str),
    LspServer(&'a str, &'a str),
    Reference(&'a str, u32, u32),
}

pub fn item_key(item: &PickerItem) -> ItemKey<'_> {
    match item {
        PickerItem::File {
            path_index,
            relative_path,
            ..
        } => ItemKey::File(*path_index, relative_path),
        PickerItem::Buffer { buffer_id, .. } => ItemKey::Buffer(*buffer_id),
        PickerItem::GrepHit {
            path_index,
            relative_path,
            line,
            col,
            ..
        } => ItemKey::Grep(*path_index, relative_path, *line, *col),
        PickerItem::Diagnostic { line, col, .. } => ItemKey::Diagnostic(*line, *col),
        PickerItem::DirEntry { name, .. } => ItemKey::DirEntry(name),
        PickerItem::Root { path_index, .. } => ItemKey::Root(*path_index),
        PickerItem::Project { name, .. } => ItemKey::Project(name),
        PickerItem::LspServer {
            language,
            workspace_root,
            ..
        } => ItemKey::LspServer(language, workspace_root),
        PickerItem::Reference {
            path, line, col, ..
        } => ItemKey::Reference(path, *line, *col),
    }
}

/// One uniform-height row of the rendered list.
enum DisplayRow<'a> {
    Header {
        path_index: u32,
        relative_path: &'a str,
    },
    Item {
        abs: u32,
        item: &'a PickerItem,
    },
}

/// Query-input placeholder per picker — kept in sync with the web client's `PLACEHOLDER` map
/// and the TUI's `picker_placeholder`.
fn placeholder(kind: PickerKind) -> &'static str {
    match kind {
        PickerKind::Files => "Find files…",
        PickerKind::Buffers => "Switch buffer…",
        PickerKind::Grep => "Grep workspace…",
        PickerKind::Explorer => "Explore files…",
        PickerKind::Projects => "Switch project…",
        PickerKind::Diagnostics => "List diagnostics…",
        PickerKind::LspServers => "List LSPs…",
        PickerKind::References => "List references…",
    }
}

const SANS: iced::Font = iced::Font {
    family: iced::font::Family::SansSerif,
    ..iced::Font::DEFAULT
};

/// Build the picker panel. `roots` are the project root paths (rows show a root label only in
/// multi-root projects, like the other clients).
pub fn overlay<'a>(state: &'a PickerState, roots: &'a [String]) -> Element<'a, PickerMsg> {
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
    if state.query.is_empty() {
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
        // The breadcrumb / a non-empty chip row already says where typing will act.
        if prefix.is_none() && chip_row.is_empty() {
            q = q.push(text(placeholder(state.kind)).size(13).font(SANS).color(theme::NORD3));
        }
        input = input.push(q);
    } else {
        let mut query_row = row![].align_y(iced::Alignment::Center);
        let pre = &state.query[..state.cursor];
        let post = &state.query[state.cursor..];
        if !pre.is_empty() {
            query_row = query_row.push(text(pre).size(13).font(SANS).color(theme::NORD6));
        }
        query_row = query_row.push(
            container(iced::widget::Space::new().width(2).height(15)).style(|_| {
                container::Style {
                    background: Some(theme::NORD8.into()),
                    ..container::Style::default()
                }
            }),
        );
        if !post.is_empty() {
            query_row = query_row.push(text(post).size(13).font(SANS).color(theme::NORD6));
        }
        input = input.push(query_row);
    }
    input = input.push(iced::widget::Space::new().width(Length::Fill));
    let counts = if state.ticking {
        format!("{}/{}…", state.total_matches, state.total_candidates)
    } else {
        format!("{}/{}", state.total_matches, state.total_candidates)
    };
    input = input.push(text(counts).size(12).font(SANS).color(theme::NORD3_BRIGHT));
    // An empty grep query means "no search has run" — saying "No matches" there would read
    // as a failed search. Every other kind lists candidates without a query, so an empty
    // result set is informative.
    let unqueried_grep = state.kind == PickerKind::Grep && state.query.is_empty();
    let show_empty_note = state.total_matches == 0 && !state.ticking && !unqueried_grep;
    // Nothing renders below the input (no rows, no message, no editor line): round its bottom
    // corners too, so the NORD0 row doesn't poke out of the panel's rounded border.
    let input_is_last =
        state.total_display_rows == 0 && !show_empty_note && state.chip_editor.is_none();
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
        container(iced::widget::Space::new().width(Length::Fill).height(1)).style(
            move |_| container::Style {
                background: filled.then(|| theme::NORD3.into()),
                ..container::Style::default()
            },
        ),
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
        }
    }
    let below = state
        .total_display_rows
        .saturating_sub(window_base + window_rows);
    list = list.push(iced::widget::Space::new().height(below as f32 * ROW_H));

    let scroll = iced::widget::scrollable(list)
        .id(list_id())
        .width(Length::Fill)
        .height(state.list_height())
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
        let first_visible = (state.scroll_y / ROW_H).floor().max(0.0) as u32;
        first_visible
            .checked_sub(window_base)
            .and_then(|rel| {
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

/// The query input's beam cursor, reused by the chip editor's focused field.
fn cursor_bar<'a>() -> Element<'a, PickerMsg> {
    container(iced::widget::Space::new().width(2).height(15))
        .style(|_| container::Style {
            background: Some(theme::NORD8.into()),
            ..container::Style::default()
        })
        .into()
}

/// An editable field of the chip editor: typed text around the beam cursor, then the gray
/// ghost (the current suggestion's remainder). `invalid` paints the typed text red — the
/// visible form of "Enter will refuse this".
fn field_with_ghost<'a>(
    input: &chips::Input,
    ghost: Option<String>,
    invalid: bool,
) -> Element<'a, PickerMsg> {
    let color = if invalid { theme::NORD11 } else { theme::NORD6 };
    let mut f = row![].spacing(2).align_y(iced::Alignment::Center);
    let pre = &input.text[..input.cursor];
    let post = &input.text[input.cursor..];
    if !pre.is_empty() {
        f = f.push(text(pre.to_string()).size(13).font(SANS).color(color));
    }
    f = f.push(cursor_bar());
    if !post.is_empty() {
        f = f.push(text(post.to_string()).size(13).font(SANS).color(color));
    }
    if let Some(g) = ghost.filter(|g| !g.is_empty()) {
        f = f.push(text(g).size(13).font(SANS).color(theme::NORD3_BRIGHT));
    }
    f.into()
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
        if ed.field == ChipEditorField::Root {
            let ghost = ed.root_ghost(&labels).map(|(_, suffix)| suffix);
            line = line.push(field_with_ghost(&ed.root_filter, ghost, invalid));
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
            line = line.push(text(display).size(13).font(SANS).color(color));
        }
        // The separator appears once the path is in play (focused, or already holding text) —
        // a fresh root prompt doesn't dangle a `:` off an unentered field.
        if ed.field == ChipEditorField::Path || !ed.input.text.is_empty() {
            line = line.push(text(":").size(13).font(SANS).color(theme::NORD3_BRIGHT));
        }
    }
    if ed.field == ChipEditorField::Path || !multi_root {
        let ghost = if ed.is_dir() { ed.path_ghost() } else { None };
        if !ed.is_dir() && ed.input.text.is_empty() {
            // Glob placeholder: the syntax by example (web's input placeholder).
            line = line.push(field_with_ghost(
                &ed.input,
                Some("*.rs · !*_test.rs · src/**".into()),
                false,
            ));
        } else {
            line = line.push(field_with_ghost(&ed.input, ghost, ed.path_invalid()));
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
fn grep_header<'a>(roots: &[String], path_index: u32, relative_path: &str) -> Element<'a, PickerMsg> {
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
fn render_item<'a>(item: &'a PickerItem, roots: &'a [String], hovered: bool) -> Element<'a, PickerMsg> {
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
            r = r.push(highlighted(relative_path, match_indices, theme::NORD4, SANS, hovered));
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
            dot_cell(Some(theme::diagnostic_color(*severity))),
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
        } => row![highlighted(name, match_indices, theme::NORD6, SANS, hovered)]
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
            let busy = matches!(status, aether_protocol::lsp::LspStatus::Ready)
                && !progress.is_empty();
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
    highlighted_owned(display.to_string(), match_indices.to_vec(), base, font, underline)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn update(
        kind: PickerKind,
        generation: u64,
        offset: u32,
        n: usize,
        total: u32,
    ) -> PickerUpdateParams {
        PickerUpdateParams {
            kind,
            generation,
            offset,
            items: (0..n)
                .map(|i| PickerItem::Project {
                    name: format!("p{i}"),
                    match_indices: vec![],
                })
                .collect(),
            total_matches: total,
            total_candidates: total,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
        }
    }

    #[test]
    fn updates_filter_stale_generations_and_windows() {
        let mut s = PickerState::new(PickerKind::Files);
        assert!(s.apply_update(update(PickerKind::Files, 0, 0, 5, 5)));
        assert_eq!(s.items.len(), 5);
        // Older generation / different window / different kind are discarded.
        s.generation = 2;
        assert!(!s.apply_update(update(PickerKind::Files, 1, 0, 9, 9)));
        assert!(!s.apply_update(update(PickerKind::Files, 2, 50, 9, 9)));
        assert!(!s.apply_update(update(PickerKind::Buffers, 2, 0, 9, 9)));
        assert_eq!(s.items.len(), 5);
    }

    #[test]
    fn selection_clamps_and_requests_refetch_outside_window() {
        let mut s = PickerState::new(PickerKind::Files);
        assert!(s.apply_update(update(PickerKind::Files, 0, 0, 90, 500)));
        // Moves within the fetched window need no refetch.
        assert_eq!(s.move_selection(1), None);
        assert_eq!(s.selected, 1);
        assert_eq!(s.move_selection(-10), None); // clamps at 0
        assert_eq!(s.selected, 0);
        // Jumping past the window requests a re-centred offset.
        s.selected = 89;
        let refetch = s.move_selection(1);
        assert_eq!(s.selected, 90);
        assert_eq!(refetch, Some(90 - FETCH_LIMIT / 2));
        // And the end clamps to the last match.
        s.selected = 499;
        assert!(s.move_selection(5).is_some());
        assert_eq!(s.selected, 499);
    }

    #[test]
    fn grep_display_rows_align_with_server_offsets() {
        let hit = |path: &str, line: u32| PickerItem::GrepHit {
            path_index: 0,
            relative_path: path.into(),
            line,
            col: 0,
            preview: "x".into(),
            match_indices: vec![],
        };
        let mut s = PickerState::new(PickerKind::Grep);
        s.offset = 10;
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::Grep,
            generation: 0,
            offset: 10,
            items: vec![hit("a.rs", 1), hit("a.rs", 2), hit("b.rs", 1)],
            // This window is the END of the result set (rows 13..18 of 18).
            total_matches: 13,
            total_candidates: 13,
            ticking: false,
            // The first item sits at display row 14; its group header occupies 13.
            grep_display_offset: Some(14),
            grep_total_display_rows: Some(18),
        }));
        // Window rows: [13]=hdr a.rs, [14]=hit, [15]=hit, [16]=hdr b.rs, [17]=hit.
        s.selected = 10;
        assert_eq!(s.selected_display_row(), Some(14));
        s.selected = 12;
        assert_eq!(s.selected_display_row(), Some(17));
        // Viewing the window's range needs no refetch (nothing exists below it); scrolling
        // above the fetched window does.
        assert_eq!(s.scrolled_refetch(13.0 * ROW_H), None);
        assert!(s.scrolled_refetch(5.0 * ROW_H).is_some());
    }

    #[test]
    fn default_skip_lands_on_first_non_active_item() {
        // Projects open: the highlight defaults past the active project — by identity, not
        // "row 1" (the active one needn't be first). One-shot: later pushes leave the
        // user's selection alone, and unwrap_or(0) covers "every item is the active one".
        let mut s = PickerState::new(PickerKind::Projects);
        s.default_skip = Some(DefaultSkip::Project("aether".into()));
        assert!(s.apply_update(update(PickerKind::Projects, 0, 0, 1, 1)));
        // Single item p0 ≠ "aether" → selected 0 and the skip is spent.
        assert_eq!(s.selected, 0);
        assert!(s.default_skip.is_none());

        let mut s = PickerState::new(PickerKind::Projects);
        s.default_skip = Some(DefaultSkip::Project("p0".into()));
        assert!(s.apply_update(update(PickerKind::Projects, 0, 0, 3, 3)));
        assert_eq!(s.selected, 1, "skips the active project at row 0");

        // Only the active project listed → fall back to row 0.
        let mut s = PickerState::new(PickerKind::Projects);
        s.default_skip = Some(DefaultSkip::Project("p0".into()));
        assert!(s.apply_update(update(PickerKind::Projects, 0, 0, 1, 1)));
        assert_eq!(s.selected, 0);
        assert!(s.default_skip.is_none());
    }

    #[test]
    fn pending_center_matches_by_identity_not_equality() {
        // The explorer's parent-ascend anchor is a bare DirEntry (no git status, no match
        // indices); the listed entry carries live decoration. Identity matching (by name)
        // must still land the highlight on it.
        let mut s = PickerState::new(PickerKind::Explorer);
        s.pending_center = Some(PickerItem::DirEntry {
            name: "src".into(),
            is_dir: true,
            match_indices: vec![],
            git_status: None,
        });
        assert!(s.apply_update(PickerUpdateParams {
            kind: PickerKind::Explorer,
            generation: 0,
            offset: 0,
            items: vec![
                PickerItem::DirEntry {
                    name: "docs".into(),
                    is_dir: true,
                    match_indices: vec![],
                    git_status: None,
                },
                PickerItem::DirEntry {
                    name: "src".into(),
                    is_dir: true,
                    match_indices: vec![],
                    git_status: Some(GitStatus::Modified), // decoration the anchor lacks
                },
            ],
            total_matches: 2,
            total_candidates: 2,
            ticking: false,
            grep_display_offset: None,
            grep_total_display_rows: None,
        }));
        assert_eq!(s.selected, 1);
        assert!(s.pending_center.is_none());
    }

    #[test]
    fn pending_center_resolves_when_its_window_arrives() {
        let mut s = PickerState::new(PickerKind::Grep);
        s.pending_center = Some(PickerItem::Project {
            name: "p7".into(),
            match_indices: vec![],
        });
        assert!(s.apply_update(update(PickerKind::Grep, 0, 0, 10, 10)));
        assert_eq!(s.selected, 7);
        assert!(s.pending_center.is_none());
    }
}
