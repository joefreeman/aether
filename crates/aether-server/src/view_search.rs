//! Search over a **view**: what a query matches across every element of it, which of those matches
//! the viewport is showing, and which one is current.
//!
//! The handlers (`handlers/search.rs`) own the wire; this owns the model, which is small enough to
//! state in full:
//!
//! - **Every match has a location** — buffer text, a removed line the inline diff draws above a
//!   line ([`MatchAt::Phantom`]), or chrome the view's builder counts as content
//!   ([`MatchAt::Chrome`], a shell run's command) — and an order: element, then the chrome above
//!   it, then line by line with a line's removed rows before its text.
//! - **What is matched is everything; what counts is what is on screen.** A removed line with the
//!   diff off, and anything inside a folded element, is matched and set aside ([`Seen`]), so
//!   toggling either re-derives the count without searching again.
//! - **The current match is the search's own**, not the cursor's. For buffer text the two coincide
//!   — the cursor selects the match — but a removed line or a command holds no cursor, so the
//!   current match is stored ([`ViewSearch::current`]) and the cursor is seated beside it.

use crate::state::{ElementBinding, ServerState, View, Viewport};
use aether_protocol::picker::MatchOptions;
use aether_protocol::search::SearchSummary;
use aether_protocol::ui::FieldId;
use aether_protocol::viewport::Element;
use aether_protocol::{BufferId, LogicalPosition, ViewId};
use std::collections::HashMap;

/// The most matches a search keeps. Past it the count reads `N+`.
pub const SEARCH_MAX_MATCHES: usize = 10_000;

/// Where a match is painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchAt {
    /// In the chrome standing above the element: the `node`th piece of text in it (in tree order),
    /// over `start..end` of that text's bytes.
    Chrome { node: u32, start: u32, end: u32 },
    /// In the `row`th removed line the inline diff draws above buffer line `line`, over
    /// `start..end` of its bytes.
    Phantom {
        line: u32,
        row: u32,
        start: u32,
        end: u32,
    },
    /// In the element's buffer text, end-exclusive.
    Text {
        start: LogicalPosition,
        end: LogicalPosition,
    },
}

/// One match: which element of the view it belongs to, and where in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewMatch {
    pub element: FieldId,
    pub at: MatchAt,
}

/// A match's place in view order, and its identity across a recompute.
///
/// `(element, band, line, layer, row, col)`: chrome is band 0, ahead of every line of its element;
/// a line's removed rows are layer 0, ahead of its text. A cursor sits in text, so where it is
/// orders as the text match that would start there ([`MatchKey::text`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MatchKey(FieldId, u8, u32, u8, u32, u32);

impl MatchKey {
    /// Where a text match starting at `at` in `element` orders — also where a cursor there does.
    pub fn text(element: FieldId, at: LogicalPosition) -> Self {
        MatchKey(element, 1, at.line, 1, 0, at.col)
    }
}

impl ViewMatch {
    pub fn key(&self) -> MatchKey {
        match self.at {
            MatchAt::Chrome { node, start, .. } => MatchKey(self.element, 0, 0, 0, node, start),
            MatchAt::Phantom {
                line, row, start, ..
            } => MatchKey(self.element, 1, line, 0, row, start),
            MatchAt::Text { start, .. } => MatchKey::text(self.element, start),
        }
    }

    pub fn is_text(&self) -> bool {
        matches!(self.at, MatchAt::Text { .. })
    }
}

/// One client's search over one view.
#[derive(Debug, Clone)]
pub struct ViewSearch {
    pub query: String,
    /// How the query matches. Kept so a recompute matches the way the search was set.
    pub options: MatchOptions,
    /// Every match, in view order (sorted by [`ViewMatch::key`]).
    pub matches: Vec<ViewMatch>,
    /// The cap was hit and there are more than `matches` holds.
    pub truncated: bool,
    /// The current match, by key. Set by a step or an anchored set; otherwise re-derived from the
    /// cursor whenever it moves ([`derive_current`]).
    pub current: Option<MatchKey>,
    /// `current_index` as last pushed in `search/state_changed`, so a cursor move that does not
    /// change it sends nothing.
    pub last_pushed_index: u32,
}

/// Whether the viewport is showing a match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// On screen (or would be, scrolled to): counted, painted, stepped to.
    Shown,
    /// Inside an element folded shut: counted apart, not stepped to.
    Folded,
    /// Not drawn at all — a removed line with the diff off.
    Hidden,
}

/// Whether `vp` shows `m`. With no viewport (none subscribed yet) nothing is folded open and the
/// diff is off, which is how a fresh viewport starts.
pub fn seen(m: &ViewMatch, view: &View, vp: Option<&Viewport>) -> Seen {
    let Some(binding) = view.elements.get(m.element as usize) else {
        return Seen::Hidden;
    };
    if matches!(m.at, MatchAt::Phantom { .. }) && !vp.is_some_and(|v| v.diff_view) {
        return Seen::Hidden;
    }
    let folded = match vp {
        Some(vp) => vp.is_collapsed(binding),
        None => binding.collapsible,
    };
    if folded {
        Seen::Folded
    } else {
        Seen::Shown
    }
}

/// Whether the view's search reads an element at all. A shell's input is where the next command
/// is typed, not something the view shows; an agent's reply is prose, whose matches have nowhere
/// to be painted yet.
fn searched(binding: &ElementBinding) -> bool {
    !binding.role.is_input() && !binding.prose
}

/// Every match of `regex` in `view`, in view order, capped at [`SEARCH_MAX_MATCHES`].
pub fn find(s: &ServerState, view: &View, regex: &regex::Regex) -> (Vec<ViewMatch>, bool) {
    let mut out: Vec<ViewMatch> = Vec::new();
    for (idx, binding) in view.elements.iter().enumerate() {
        if !searched(binding) {
            continue;
        }
        // One more than there is room for, so a full list knows whether anything was left out.
        let room = SEARCH_MAX_MATCHES - out.len();
        let element = idx as FieldId;
        let mut found = element_matches(s, binding, element, regex, room + 1);
        // Collected per kind and then ordered, so a cap reached mid-element keeps the matches
        // nearest the top of it rather than whichever kind was scanned first.
        found.sort_by_key(ViewMatch::key);
        if found.len() > room {
            found.truncate(room);
            out.extend(found);
            return (out, true);
        }
        out.extend(found);
    }
    (out, false)
}

/// One element's matches, unordered, at most `limit` of each kind.
fn element_matches(
    s: &ServerState,
    binding: &ElementBinding,
    element: FieldId,
    regex: &regex::Regex,
    limit: usize,
) -> Vec<ViewMatch> {
    let mut out = Vec::new();
    let Some(doc) = s.try_doc_of(binding.buffer_id) else {
        return out;
    };
    let lines = binding.lines_in(doc.line_count());

    if binding.chrome_searched {
        for (node, text) in chrome_texts(&binding.chrome_above).into_iter().enumerate() {
            for (start, end) in find_in(regex, text).take(limit) {
                out.push(ViewMatch {
                    element,
                    at: MatchAt::Chrome {
                        node: node as u32,
                        start,
                        end,
                    },
                });
            }
        }
    }

    // A run that printed nothing windows no lines; a scope over it would hold the line after it,
    // which is the next element's.
    if binding.is_empty() {
        return out;
    }

    let mut phantoms: Vec<(u32, Vec<String>)> = phantom_texts(s, binding, doc.line_count())
        .into_iter()
        .filter(|(line, _)| lines.contains(line))
        .collect();
    phantoms.sort_by_key(|(line, _)| *line);
    let mut taken = 0;
    'phantoms: for (line, rows) in phantoms {
        for (row, text) in rows.iter().enumerate() {
            for (start, end) in find_in(regex, text) {
                if taken == limit {
                    break 'phantoms;
                }
                taken += 1;
                out.push(ViewMatch {
                    element,
                    at: MatchAt::Phantom {
                        line,
                        row: row as u32,
                        start,
                        end,
                    },
                });
            }
        }
    }

    // The element's own text and nothing else, so nothing outside its window can match. Offsets
    // come back relative to the window; `base` returns them to the document's bytes.
    let scope = crate::cursor::Scope::windowed(doc, lines.start, lines.end);
    let source: String = scope.text().chunks().collect();
    let base = scope.first_byte();
    for m in regex
        .find_iter(&source)
        .filter(|m| m.start() != m.end())
        .take(limit)
    {
        out.push(ViewMatch {
            element,
            at: MatchAt::Text {
                start: crate::handlers::byte_to_logical(doc, base + m.start()),
                end: crate::handlers::byte_to_logical(doc, base + m.end()),
            },
        });
    }
    out
}

/// Non-empty matches of `regex` in `text`, as byte ranges. Zero-width matches are skipped so a
/// pattern like `^` doesn't pin the search to every line start.
fn find_in<'t>(regex: &'t regex::Regex, text: &'t str) -> impl Iterator<Item = (u32, u32)> + 't {
    regex
        .find_iter(text)
        .filter(|m| m.start() != m.end())
        .map(|m| (m.start() as u32, m.end() as u32))
}

/// The removed lines the inline diff draws in this element, by the line they sit above — the same
/// two sources the renderer reads: what the view says about the lines when it has an opinion (a
/// patch's hunk), else the buffer's own diff. A generated document has neither.
fn phantom_texts(
    s: &ServerState,
    binding: &ElementBinding,
    line_count: u32,
) -> Vec<(u32, Vec<String>)> {
    fn texts(rows: &[aether_protocol::viewport::BaselineRow]) -> Vec<String> {
        rows.iter().map(|r| r.text.clone()).collect()
    }
    match &binding.decorations {
        Some(d) => d
            .baseline_above
            .iter()
            .map(|(line, rows)| (*line, texts(rows)))
            .collect(),
        None if s
            .try_doc_of(binding.buffer_id)
            .is_some_and(|d| d.patch().is_none()) =>
        {
            crate::handlers::deleted_rows_by_anchor(
                crate::handlers::buffer_both_hunks(s, binding.buffer_id),
                line_count,
                None,
            )
            .into_iter()
            .map(|(line, rows)| (line, texts(&rows)))
            .collect()
        }
        None => Vec::new(),
    }
}

/// The pieces of text in a run of chrome, in tree order — what [`MatchAt::Chrome`]'s `node`
/// indexes. A button's label is the button's, not content, so it is not among them.
pub fn chrome_texts(chrome: &[Element]) -> Vec<&str> {
    fn walk<'a>(node: &'a Element, out: &mut Vec<&'a str>) {
        match node {
            Element::Text { text, .. } => out.push(text),
            Element::Column { children, .. } | Element::Row { children, .. } => {
                for child in children {
                    walk(child, out);
                }
            }
            Element::Editor { .. }
            | Element::Prose { .. }
            | Element::Action { .. }
            | Element::Space { .. }
            | Element::Fill { .. } => {}
        }
    }
    let mut out = Vec::new();
    for node in chrome {
        walk(node, &mut out);
    }
    out
}

/// `chrome` with each match's range set on the text it falls in — `ranges[node]` for the `node`th
/// piece of text, as [`chrome_texts`] numbers them.
pub fn mark_chrome(
    chrome: &[Element],
    ranges: &HashMap<u32, Vec<aether_protocol::search::SearchMatchRange>>,
) -> Vec<Element> {
    fn walk(
        node: &mut Element,
        next: &mut u32,
        ranges: &HashMap<u32, Vec<aether_protocol::search::SearchMatchRange>>,
    ) {
        match node {
            Element::Text { search_matches, .. } => {
                if let Some(r) = ranges.get(next) {
                    *search_matches = r.clone();
                }
                *next += 1;
            }
            Element::Column { children, .. } | Element::Row { children, .. } => {
                for child in children {
                    walk(child, next, ranges);
                }
            }
            Element::Editor { .. }
            | Element::Prose { .. }
            | Element::Action { .. }
            | Element::Space { .. }
            | Element::Fill { .. } => {}
        }
    }
    let mut out = chrome.to_vec();
    let mut next = 0;
    for node in &mut out {
        walk(node, &mut next, ranges);
    }
    out
}

/// The matches `vp` shows, each with its 1-based index in the count — what a render paints and
/// what `n` walks.
pub fn shown<'a>(
    search: &'a ViewSearch,
    view: &'a View,
    vp: Option<&'a Viewport>,
) -> impl Iterator<Item = (u32, &'a ViewMatch)> + 'a {
    search
        .matches
        .iter()
        .filter(move |m| seen(m, view, vp) == Seen::Shown)
        .zip(1u32..)
        .map(|(m, i)| (i, m))
}

/// The summary `vp`'s client sees.
pub fn summary(
    search: &ViewSearch,
    view_id: ViewId,
    view: &View,
    vp: Option<&Viewport>,
) -> SearchSummary {
    let (mut total, mut folded, mut current_index) = (0u32, 0u32, 0u32);
    for m in &search.matches {
        match seen(m, view, vp) {
            Seen::Shown => {
                total += 1;
                if search.current == Some(m.key()) {
                    current_index = total;
                }
            }
            Seen::Folded => folded += 1,
            Seen::Hidden => {}
        }
    }
    SearchSummary {
        view_id,
        total,
        truncated: search.truncated,
        current_index,
        folded,
    }
}

/// The text match the cursor head sits inside, in the focused element — the current match whenever
/// a step hasn't said otherwise.
pub fn derive_current(
    s: &ServerState,
    client_id: aether_protocol::ClientId,
    search: &ViewSearch,
    view: &View,
    vp: Option<&Viewport>,
) -> Option<MatchKey> {
    // With no viewport the client is where one would start: the first element.
    let element = vp.map_or(0, |vp| vp.focused);
    let buffer_id = view.elements.get(element as usize)?.buffer_id;
    let doc = s.try_doc_of(buffer_id)?;
    let cursor = s
        .cursors
        .get(&(client_id, buffer_id))
        .copied()
        .unwrap_or_default();
    let head = crate::cursor::pos_to_char(doc, cursor.position);
    search
        .matches
        .iter()
        .filter(|m| m.element == element)
        .find(|m| match m.at {
            MatchAt::Text { start, end } => {
                let first = crate::cursor::pos_to_char(doc, start);
                let last = crate::cursor::pos_to_char(doc, end).saturating_sub(1);
                head >= first && head <= last
            }
            _ => false,
        })
        .map(ViewMatch::key)
}

/// Run `search`'s query again over the view as it is now, keeping its current match where that
/// match still exists and isn't text (text re-derives from the cursor, which an edit moves with
/// it). `false` when the query no longer builds — never, for a query that built once.
pub fn recompute(
    s: &ServerState,
    client_id: aether_protocol::ClientId,
    search: &mut ViewSearch,
    view: &View,
    vp: Option<&Viewport>,
) -> bool {
    let Ok(regex) = crate::picker::build_match_regex(&search.query, &search.options) else {
        return false;
    };
    let (matches, truncated) = find(s, view, &regex);
    search.matches = matches;
    search.truncated = truncated;
    let kept = search.current.filter(|key| {
        search
            .matches
            .iter()
            .any(|m| !m.is_text() && m.key() == *key)
    });
    search.current = kept.or_else(|| derive_current(s, client_id, search, view, vp));
    true
}

/// Recompute every search on `view_id` — after its composition changed or a document it windows
/// was edited. The summaries ride the re-render that change already pushes.
pub fn refresh_view(s: &mut ServerState, view_id: ViewId) {
    let keys: Vec<_> = s
        .searches
        .keys()
        .filter(|(_, v)| *v == view_id)
        .copied()
        .collect();
    for key in keys {
        let Some(mut search) = s.searches.remove(&key) else {
            continue;
        };
        let Some(view) = s.views.get(&view_id) else {
            continue;
        };
        let vp = s.viewport_on(key.0, view_id);
        if recompute(s, key.0, &mut search, view, vp) {
            search.last_pushed_index = summary(&search, view_id, view, vp).current_index;
            s.searches.insert(key, search);
        }
    }
}

/// Recompute the searches of every view windowing one of `buffers`.
pub fn refresh_views_binding(s: &mut ServerState, buffers: &[BufferId]) {
    let views: Vec<ViewId> = s
        .searches
        .keys()
        .map(|(_, v)| *v)
        .filter(|v| {
            s.views
                .get(v)
                .is_some_and(|view| buffers.iter().any(|b| view.binds(*b)))
        })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    for view in views {
        refresh_view(s, view);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(element: FieldId, line: u32, col: u32) -> ViewMatch {
        ViewMatch {
            element,
            at: MatchAt::Text {
                start: LogicalPosition { line, col },
                end: LogicalPosition { line, col: col + 1 },
            },
        }
    }

    fn phantom(element: FieldId, line: u32, row: u32) -> ViewMatch {
        ViewMatch {
            element,
            at: MatchAt::Phantom {
                line,
                row,
                start: 0,
                end: 1,
            },
        }
    }

    fn chrome(element: FieldId) -> ViewMatch {
        ViewMatch {
            element,
            at: MatchAt::Chrome {
                node: 0,
                start: 0,
                end: 1,
            },
        }
    }

    /// View order is element, then the chrome above it, then line by line with a line's removed
    /// rows ahead of its text — the order the rows are drawn in, top to bottom.
    #[test]
    fn view_order_follows_the_rows_top_to_bottom() {
        let mut all = vec![
            text(1, 0, 0),
            text(0, 5, 2),
            phantom(0, 5, 1),
            chrome(1),
            phantom(0, 5, 0),
            text(0, 3, 9),
            chrome(0),
        ];
        all.sort_by_key(ViewMatch::key);
        assert_eq!(
            all,
            vec![
                chrome(0),
                text(0, 3, 9),
                phantom(0, 5, 0),
                phantom(0, 5, 1),
                text(0, 5, 2),
                chrome(1),
                text(1, 0, 0),
            ]
        );
    }

    #[test]
    fn chrome_text_is_numbered_in_tree_order_and_buttons_are_not_text() {
        let chrome = vec![Element::chrome(vec![Element::row(vec![
            Element::text("✓", Vec::new()),
            Element::Space { cols: 1 },
            Element::Action {
                action: aether_protocol::ui::ViewAction::Expand { expand: None },
                label: vec![Element::text("press me", Vec::new())],
                enabled: true,
            },
            Element::text("cargo test", Vec::new()),
        ])])];
        assert_eq!(chrome_texts(&chrome), vec!["✓", "cargo test"]);

        let range = aether_protocol::search::SearchMatchRange {
            start: 0,
            end: 5,
            index: 3,
        };
        let marked = mark_chrome(&chrome, &HashMap::from([(1, vec![range])]));
        let mut found = Vec::new();
        fn collect(e: &Element, out: &mut Vec<(String, usize)>) {
            match e {
                Element::Text {
                    text,
                    search_matches,
                    ..
                } => out.push((text.clone(), search_matches.len())),
                Element::Column { children, .. } | Element::Row { children, .. } => {
                    children.iter().for_each(|c| collect(c, out))
                }
                _ => {}
            }
        }
        marked.iter().for_each(|e| collect(e, &mut found));
        assert_eq!(
            found,
            vec![("✓".to_string(), 0), ("cargo test".to_string(), 1)]
        );
    }
}
