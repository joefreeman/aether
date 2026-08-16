//! Presentation labels for workspace roots and buffer locations. This is the client-side home for
//! *how a path is printed*: the disambiguated root labels ([`root_labels`]), the canonical
//! `"[root]: [path]"` form ([`root_relative_display`]) shown in the status bar and window title,
//! plus window-title assembly and path truncation.
//!
//! Deliberately client-side: the server sends *root indices* (`path_index`), and the client decides
//! how to print them — the buffers picker, Files, and Grep all ship `path_index` + `relative_path`
//! and format their rows here. Anything that needs to show a buffer's location routes through this
//! module (or the shells' thin per-widget wrappers over [`root_labels`]); don't reinvent the
//! `"{label}: {path}"` join, or the status bar / title / picker will drift apart again.
//!
//! The label defaults to a root's basename; when two roots share a basename it grows a
//! parenthesized parent component, then grandparent, etc., until every label is unique. Single-root
//! workspaces have nothing to disambiguate, so the label is empty and the prefix is omitted.

use aether_protocol::lsp::SymbolCrumb;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Cap on disambiguation passes. Real workspaces never need more than a couple, but the loop has
/// to terminate when two paths are literally identical (which `add_root` refuses, but defensive
/// belt-and-braces is cheap).
const MAX_DEPTH: usize = 16;

/// One label per input path, aligned by index. Identical inputs produce identical labels (we
/// can't disambiguate them); otherwise every label is unique within the result.
///
/// Single-root workspaces get an empty string — there's nothing to disambiguate against, so
/// renderers omit the label prefix entirely (`"src/main.rs"` instead of `"repo: src/main.rs"`).
pub fn root_labels(paths: &[String]) -> Vec<String> {
    let n = paths.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![String::new()];
    }
    let bufs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    let mut depths = vec![0usize; n];
    let mut labels = vec![String::new(); n];
    for _ in 0..=MAX_DEPTH {
        for i in 0..n {
            labels[i] = label_at_depth(&bufs[i], depths[i]);
        }
        let mut by_label: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, l) in labels.iter().enumerate() {
            by_label.entry(l.as_str()).or_default().push(i);
        }
        let collisions: Vec<Vec<usize>> = by_label
            .into_values()
            .filter(|idxs| idxs.len() > 1)
            .collect();
        if collisions.is_empty() {
            return labels;
        }
        let mut bumped = false;
        for idxs in collisions {
            for i in idxs {
                if depths[i] < MAX_DEPTH {
                    depths[i] += 1;
                    bumped = true;
                }
            }
        }
        if !bumped {
            // Every colliding entry has maxed out — can't disambiguate further. Return as-is;
            // duplicates are acceptable for this edge case (shouldn't occur in practice).
            return labels;
        }
    }
    labels
}

/// Build `{basename} ({parent1/parent2/...})` with `depth` parent components walked. `depth=0`
/// is the bare basename; nameless ancestors (the filesystem root) are skipped rather than
/// emitted as blanks.
fn label_at_depth(path: &Path, depth: usize) -> String {
    let basename = path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    if depth == 0 {
        return basename;
    }
    let mut parents: Vec<String> = Vec::new();
    let mut cur = path.parent();
    while parents.len() < depth {
        let Some(p) = cur else { break };
        if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
            if !name.is_empty() {
                parents.push(name.to_string());
            }
        }
        cur = p.parent();
    }
    if parents.is_empty() {
        basename
    } else {
        parents.reverse();
        format!("{basename} ({})", parents.join("/"))
    }
}

/// The canonical way a workspace-relative path is shown as a *single string*: `"[root]: [path]"`
/// for a multi-root workspace (with `[root]` the disambiguated [`root_labels`] entry), or the bare
/// `relative_path` for a single-root workspace. An empty `relative_path` — the root directory
/// itself — renders as just the label. Used by the status bar and window title (via
/// [`crate::session::label_for_path`]) and save-as relabelling. Picker *rows* don't call this —
/// they render the label prefix as a separate, non-highlighted span over [`root_labels`], so the
/// fuzzy-match highlight lands only on the path.
pub fn root_relative_display(roots: &[String], path_index: u32, relative_path: &str) -> String {
    let label = root_labels(roots)
        .get(path_index as usize)
        .filter(|l| !l.is_empty())
        .cloned();
    match label {
        Some(label) if relative_path.is_empty() => label,
        Some(label) => format!("{label}: {relative_path}"),
        None => relative_path.to_string(),
    }
}

/// How a workspace id is shown to the user in a *workspace list* (the switcher). A persisted workspace
/// shows its name verbatim; an *ephemeral* one (the synthesized "no workspace" context that hosts
/// files opened outside any configured workspace) shows `(workspace <n>)` — mirroring scratch buffers'
/// `(scratch <n>)`, so multiple ephemeral contexts are distinguishable — rather than its internal
/// `ephemeral/<n>` id. The status bar / title use [`shows_workspace_chrome`] instead: they omit the
/// `[…]` entirely for an ephemeral context.
pub fn workspace_display(workspace: &str) -> String {
    match workspace.strip_prefix(aether_protocol::EPHEMERAL_WORKSPACE_PREFIX) {
        Some(n) => format!("(workspace {n})"),
        None => workspace.to_string(),
    }
}

/// Whether a workspace id should be wrapped in the `[workspace]` chrome shown in the status bar and
/// window title. False for the empty (no workspace active) state *and* for an ephemeral context —
/// neither is a real, named workspace, so we show just the buffer label with no bracket rather than
/// a `[(no workspace)]` that reads like a workspace literally named that.
pub fn shows_workspace_chrome(workspace: &str) -> bool {
    !workspace.is_empty() && !aether_protocol::is_ephemeral_workspace_id(workspace)
}

/// Max characters the window-title path label is shown at before [`truncate_path`] elides it.
/// Title bars have no width budget of their own (unlike the status bar, which truncates to the row
/// width), so this is a fixed cap — long enough for a typical workspace-relative path, short enough
/// that an absolute external path (goto-definition into a dependency) doesn't blow out the title.
const TITLE_LABEL_MAX: usize = 60;

/// Shrink a `/`-separated path `label` into `max_chars` via the standardised segment-elision ladder
/// — the shared shape of the TUI's `truncate_path_with_indices` and the web's `truncatePath`, minus
/// the match-index bookkeeping those need for picker highlighting:
///
///  1. Fits → unchanged.
///  2. Elide whole *middle* segments to a single `…` (`crates/…/src/handlers.rs`): the last segment
///     (the filename) always survives, and among the candidates that fit we keep as many segments
///     as possible, ties broken toward the tail — a file's parents identify it better than leading
///     dirs do.
///  3. Floor: char-level left-cut with a leading `…`, keeping the end of the string.
///
/// Char-based (a window title has no column budget), matching the web/native status bars. Strings
/// without `/` skip straight to the floor, so any label passes through safely.
pub fn truncate_path(label: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if label.chars().count() <= max_chars {
        return label.to_string();
    }
    // Rung 2: segment elision. Candidates keep the first `l` and last `t` segments around one `…`;
    // pick the fitting candidate with the most segments, preferring tail on ties.
    let segs: Vec<&str> = label.split('/').collect();
    let n = segs.len();
    if n >= 2 {
        let seg_w: Vec<usize> = segs.iter().map(|s| s.chars().count()).collect();
        let mut best: Option<(usize, usize)> = None; // (lead, tail), tail ≥ 1
        for t in 1..n {
            for l in 0..=(n - 1 - t) {
                let w: usize = seg_w[..l].iter().sum::<usize>()
                    + seg_w[n - t..].iter().sum::<usize>()
                    + (l + t) // one `/` per kept segment (around the `…` part)
                    + 1; // the `…` itself
                if w <= max_chars && best.is_none_or(|(bl, bt)| (l + t, t) > (bl + bt, bt)) {
                    best = Some((l, t));
                }
            }
        }
        if let Some((l, t)) = best {
            let lead = segs[..l].join("/");
            let tail = segs[n - t..].join("/");
            return if l == 0 {
                format!("…/{tail}")
            } else {
                format!("{lead}/…/{tail}")
            };
        }
    }
    // Rung 3 (floor): keep chars from the end until `max_chars - 1` is filled (one cell for the `…`).
    let chars: Vec<char> = label.chars().collect();
    let keep = max_chars.saturating_sub(1).min(chars.len());
    let tail: String = chars[chars.len() - keep..].iter().collect();
    format!("…{tail}")
}

/// Separator between breadcrumb steps. A single narrow glyph with space either side, distinct from
/// the `/` of a path so a breadcrumb never reads as one.
pub const CRUMB_SEPARATOR: &str = " › ";

/// Separator between the status bar's left-hand sections — file label, Git cluster, breadcrumb. Bare
/// glyph: each shell supplies its own padding (a cell either side in the terminal, the flex gap in
/// the browser, a spacer widget in the GUI). Painted in the muted shade so it reads as structure
/// rather than content, and deliberately a *middle dot* — the fatter `●`/`•` already mean
/// buffer-state and language-server health in this same row.
pub const SECTION_SEPARATOR: &str = "·";

/// Minimum width worth spending on a breadcrumb. Below this the only thing that fits is the elision
/// marker plus a stub, which says less than the empty space it replaces.
const CRUMB_MIN_WIDTH: usize = 8;

/// Render the cursor's enclosing-symbol chain (outermost first) into `max_chars`, as the status bar
/// shows it: `impl LspManager › fn symbol_servers`.
///
/// The ladder mirrors [`truncate_path`]'s, and inverts it twice — because a scope chain is read from
/// the *inside out*, where a path is read from the outside in:
///
///  1. Whole chain, if it fits.
///  2. Drop *leading* crumbs into a single `…`, keeping as many innermost ones as fit
///     (`…› fn symbol_servers`). Unlike [`truncate_path`], the first element is never preserved: the
///     outermost scope is the least informative crumb, and the file label sitting to the left of
///     this in the status bar has already established the top-level context.
///  3. Floor: the innermost crumb alone still overflows, so char-cut it keeping its *start*
///     (`resolve_navigation_m…`). Again the opposite of [`truncate_path`], which keeps the tail —
///     filenames disambiguate at the end, identifiers at the beginning.
///
/// Empty when the chain is empty or the budget is too small to say anything ([`CRUMB_MIN_WIDTH`]).
/// Char-based, matching the rest of this module and the shells' char-budget estimates.
pub fn truncate_symbol_path(crumbs: &[SymbolCrumb], max_chars: usize) -> String {
    truncate_symbol_path_parts(crumbs, max_chars).join(CRUMB_SEPARATOR)
}

/// The same ladder as [`truncate_symbol_path`], as the display segments rather than one joined
/// string — the leading `…` is its own segment when crumbs were dropped, and the last segment is
/// always the innermost (possibly head-cut) crumb. Empty vec for "show nothing".
///
/// Two renderings of one result: the terminal shell joins these with [`CRUMB_SEPARATOR`] because a
/// cell is a cell, while the proportional shells (GUI and browser) lay the segments out themselves
/// so they can put real space either side of the `›` — a space glyph in a proportional face is too
/// tight, and a joined string has no way to say "more room here".
pub fn truncate_symbol_path_parts(crumbs: &[SymbolCrumb], max_chars: usize) -> Vec<String> {
    if crumbs.is_empty() || max_chars < CRUMB_MIN_WIDTH {
        return Vec::new();
    }
    let width = |s: &str| s.chars().count();
    let sep_w = width(CRUMB_SEPARATOR);
    let names: Vec<&str> = crumbs.iter().map(|c| c.name.as_str()).collect();
    let owned = |parts: &[&str]| parts.iter().map(|s| (*s).to_string()).collect();

    // Rung 1: everything.
    if width(&names.join(CRUMB_SEPARATOR)) <= max_chars {
        return owned(&names);
    }

    // Rung 2: keep the longest innermost run that fits behind a `…` marker. The marker stands in for
    // the dropped crumbs *and* their separator, so it costs `…` plus one separator.
    let marker_w = 1 + sep_w;
    for take in (1..names.len()).rev() {
        let tail = &names[names.len() - take..];
        let w: usize = tail.iter().map(|c| width(c)).sum::<usize>() + sep_w * (take - 1);
        if marker_w + w <= max_chars {
            let mut parts = vec!["…".to_string()];
            parts.extend(tail.iter().map(|s| (*s).to_string()));
            return parts;
        }
    }

    // Rung 3 (floor): the innermost crumb alone. Whole if it fits — a name in full says more than a
    // cut one plus the `…›` hint that there was more — otherwise head-cut.
    let last = names.last().expect("non-empty");
    if width(last) <= max_chars {
        return vec![(*last).to_string()];
    }
    let head: String = last.chars().take(max_chars.saturating_sub(1)).collect();
    vec![format!("{head}…")]
}

/// Split a rendered breadcrumb ([`truncate_symbol_path`]) into `(ancestors, innermost)`, where
/// `ancestors` keeps its trailing separator so the two concatenate back to the input. Every shell
/// paints `ancestors` in the muted shade and `innermost` in the body colour: the chain reads as
/// chrome, the symbol you're actually in reads as content. `ancestors` is empty when only one crumb
/// survived truncation.
pub fn split_symbol_path(rendered: &str) -> (&str, &str) {
    match rendered.rfind(CRUMB_SEPARATOR) {
        Some(i) => rendered.split_at(i + CRUMB_SEPARATOR.len()),
        None => ("", rendered),
    }
}

/// The window/terminal title's *body* for `(workspace, buffer label)`: `None` before a workspace is
/// active (the title is then just the app name), otherwise `[workspace]` or `[workspace] label`. The
/// `[…]` is omitted entirely when there's no real workspace — a connecting/chooser state (so no
/// stray `[]`) *and* an ephemeral "(no workspace)" context, which shows just the buffer label.
/// Long paths are segment-elided (see [`truncate_path`]) so an external file's absolute path doesn't
/// overflow the title bar. The TUI prepends a dirty dot; no shell appends the app name.
pub fn title_body(workspace: &str, label: &str) -> Option<String> {
    if workspace.is_empty() {
        // Boot / connecting / chooser: no workspace *and* no buffer — the title is just the app name.
        return None;
    }
    let label = truncate_path(label, TITLE_LABEL_MAX);
    if aether_protocol::is_ephemeral_workspace_id(workspace) {
        // Ephemeral "(no workspace)" context: show the buffer label alone (the filename you're
        // editing), with no `[workspace]` bracket — or nothing when there's no label.
        return (!label.is_empty()).then_some(label);
    }
    Some(if label.is_empty() {
        format!("[{workspace}]")
    } else {
        format!("[{workspace}] {label}")
    })
}

/// The full window title: just the [`title_body`]. The app name is deliberately *not* appended —
/// the title bar is narrow and `[workspace] path` is the part that distinguishes one window from
/// another; the desktop/browser already labels the app (the GUI via its `application_id`, the tab
/// via its favicon).
///
/// The one exception is having nothing to say at all — boot, connecting, the chooser — where the
/// name stands in. An empty title isn't neutral: a browser falls back to showing the raw URL and a
/// window list shows a blank entry, both of which read as a bug rather than as "nothing open yet".
pub fn window_title(workspace: &str, label: &str) -> String {
    title_body(workspace, label).unwrap_or_else(|| APP_NAME.to_string())
}

/// Stand-in title for a window with no workspace and no buffer. See [`window_title`].
pub const APP_NAME: &str = "Aether";

#[cfg(test)]
mod tests {
    use super::*;

    fn s(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    fn crumbs(names: &[&str]) -> Vec<SymbolCrumb> {
        names
            .iter()
            .map(|n| SymbolCrumb {
                name: (*n).to_string(),
                kind: aether_protocol::picker::SymbolKind::Function,
            })
            .collect()
    }

    #[test]
    fn symbol_path_fits_whole() {
        let c = crumbs(&["impl Foo", "fn bar"]);
        assert_eq!(truncate_symbol_path(&c, 40), "impl Foo › fn bar");
    }

    /// Rung 2: the outermost crumbs collapse into `…›` and the innermost survives whole. The
    /// *first* crumb is never the one kept — that's the deliberate inversion of `truncate_path`.
    #[test]
    fn symbol_path_drops_outermost_first() {
        let c = crumbs(&["mod outer", "impl Foo", "fn bar"]);
        assert_eq!(truncate_symbol_path(&c, 29), "mod outer › impl Foo › fn bar");
        assert_eq!(truncate_symbol_path(&c, 25), "… › impl Foo › fn bar");
        assert_eq!(truncate_symbol_path(&c, 20), "… › fn bar");
    }

    /// Rung 3: one crumb too wide for the budget keeps its *head* — identifiers disambiguate at the
    /// start, so `resolve_naviga…` beats `…gation_motion`.
    #[test]
    fn symbol_path_head_cuts_the_innermost() {
        let c = crumbs(&["resolve_navigation_motion"]);
        assert_eq!(truncate_symbol_path(&c, 12), "resolve_nav…");
    }

    /// A budget that can't fit the marker still prefers the bare innermost name over cutting it:
    /// the full identifier says more than a stub plus a "there was more" hint.
    #[test]
    fn symbol_path_prefers_whole_innermost_over_marker() {
        let c = crumbs(&["SomeVeryLongTypeName", "fn bar"]);
        assert_eq!(truncate_symbol_path(&c, 9), "fn bar");
    }

    /// The parts form is the same ladder, unjoined — the `…` is its own segment so a proportional
    /// shell can space it like any other crumb, and joining reproduces `truncate_symbol_path`.
    #[test]
    fn symbol_path_parts_are_the_joined_form_unjoined() {
        let c = crumbs(&["mod outer", "impl Foo", "fn bar"]);
        for budget in [8, 12, 20, 25, 29, 40] {
            let parts = truncate_symbol_path_parts(&c, budget);
            assert_eq!(
                parts.join(CRUMB_SEPARATOR),
                truncate_symbol_path(&c, budget),
                "budget {budget}"
            );
        }
        assert_eq!(
            truncate_symbol_path_parts(&c, 25),
            vec!["…", "impl Foo", "fn bar"]
        );
        assert!(truncate_symbol_path_parts(&c, 4).is_empty());
    }

    #[test]
    fn symbol_path_empty_below_min_width() {
        let c = crumbs(&["fn bar"]);
        assert_eq!(truncate_symbol_path(&c, 7), "");
        assert_eq!(truncate_symbol_path(&[], 80), "");
    }

    /// Char-based, not byte-based: a multi-byte name must not overshoot its column budget.
    #[test]
    fn symbol_path_counts_chars_not_bytes() {
        let c = crumbs(&["日本語識別子テスト"]);
        let out = truncate_symbol_path(&c, 8);
        assert_eq!(out.chars().count(), 8);
        assert_eq!(out, "日本語識別子テ…");
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(root_labels(&[]).is_empty());
    }

    #[test]
    fn single_root_returns_empty_label() {
        // Single-root workspaces don't need a disambiguator — renderers fall through to a label-
        // less display when the label is empty.
        assert_eq!(root_labels(&s(&["/home/joe/work/repo"])), vec![""]);
    }

    #[test]
    fn no_collision_keeps_bare_basename() {
        let labels = root_labels(&s(&["/home/joe/work/api", "/home/joe/work/cli"]));
        assert_eq!(labels, vec!["api", "cli"]);
    }

    #[test]
    fn collision_extends_with_parent() {
        let labels = root_labels(&s(&["/home/joe/work/api", "/home/joe/personal/api"]));
        assert_eq!(labels, vec!["api (work)", "api (personal)"]);
    }

    #[test]
    fn deeper_collision_extends_further() {
        // Both share parent "work" too — algorithm bumps both to depth 2.
        let labels = root_labels(&s(&["/a/x/work/api", "/b/y/work/api"]));
        assert_eq!(labels, vec!["api (x/work)", "api (y/work)"]);
    }

    #[test]
    fn three_way_collision() {
        let labels = root_labels(&s(&[
            "/home/joe/work/api",
            "/home/joe/personal/api",
            "/srv/api",
        ]));
        // All three collide on "api"; each grows by one parent. All parents unique.
        assert_eq!(labels, vec!["api (work)", "api (personal)", "api (srv)"]);
    }

    #[test]
    fn partial_collision_only_extends_the_clash() {
        let labels = root_labels(&s(&[
            "/home/joe/work/api",
            "/home/joe/personal/api",
            "/home/joe/work/cli",
        ]));
        assert_eq!(labels, vec!["api (work)", "api (personal)", "cli"]);
    }

    #[test]
    fn identical_paths_dont_loop_forever() {
        let labels = root_labels(&s(&["/foo/bar", "/foo/bar"]));
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0], labels[1]);
    }

    #[test]
    fn display_single_root_is_bare_path() {
        let roots = s(&["/home/joe/work/repo"]);
        assert_eq!(
            root_relative_display(&roots, 0, "src/main.rs"),
            "src/main.rs"
        );
        assert_eq!(root_relative_display(&roots, 0, ""), "");
    }

    #[test]
    fn display_multi_root_prefixes_disambiguated_label() {
        let roots = s(&["/home/joe/work/api", "/home/joe/personal/api"]);
        assert_eq!(
            root_relative_display(&roots, 0, "src/main.rs"),
            "api (work): src/main.rs"
        );
        assert_eq!(root_relative_display(&roots, 0, ""), "api (work)");
    }

    #[test]
    fn window_title_is_the_body_alone_with_no_app_name_suffix() {
        // With a workspace the title is just what distinguishes this window — no app-name suffix.
        assert_eq!(window_title("demo", ""), "[demo]");
        assert_eq!(window_title("demo", "src/main.rs"), "[demo] src/main.rs");
        assert!(!window_title("demo", "src/main.rs").contains(APP_NAME));
        // No workspace (boot/connecting/chooser): the app name stands in for an otherwise empty
        // title — never a stray `[]`, and never blank (which shows the URL in a browser tab).
        assert_eq!(window_title("", ""), APP_NAME);
        assert_eq!(window_title("", "ignored"), APP_NAME);
        assert_eq!(title_body("", ""), None);
    }

    #[test]
    fn truncate_path_ladder() {
        // Rung 1: fits unchanged.
        assert_eq!(truncate_path("src/main.rs", 60), "src/main.rs");
        // Rung 2: middle segments elide to a single `…`; the filename always survives, and as many
        // segments as fit are kept (here lead `crates` + the last three, at 28 ≤ 30).
        assert_eq!(
            truncate_path("crates/aether-server/src/handlers/lsp.rs", 30),
            "crates/…/src/handlers/lsp.rs"
        );
        // Tighter budget, ties broken toward the tail: keeping `e/filename.rs` (no lead) beats
        // keeping `a` + `filename.rs`, since both keep two segments but the tail candidate wins.
        assert_eq!(
            truncate_path("a/b/c/d/e/filename.rs", 16),
            "…/e/filename.rs"
        );
        // Rung 3 (floor): no `/` to elide, so a long single segment left-cuts to the end (the `…`
        // plus the last `budget - 1` chars).
        assert_eq!(truncate_path("supercalifragilistic.rs", 10), "…listic.rs");
    }

    #[test]
    fn truncate_path_floor_keeps_the_end() {
        // A `/`-less label longer than the budget keeps its last `budget-1` chars after the `…`.
        let out = truncate_path("abcdefghijklmnop", 6);
        assert_eq!(out.chars().count(), 6);
        assert_eq!(out, "…lmnop");
    }

    #[test]
    fn window_title_elides_a_long_external_path() {
        // An external goto-def target (absolute path, outside roots) would otherwise overflow the
        // title bar; the body segment-elides it while keeping the filename.
        let long = "/home/u/.cargo/registry/src/index.crates.io-abc/serde-1.0.200/src/de/mod.rs";
        let body = title_body("demo", long).expect("a workspace with a label has a body");
        assert!(body.starts_with("[demo] "), "workspace chrome is preserved");
        assert!(body.contains('…'), "the long path is elided: {body}");
        assert!(body.ends_with("mod.rs"), "the filename survives: {body}");
        // The label portion is capped (workspace chrome + a bounded label).
        assert!(body.chars().count() <= "[demo] ".chars().count() + 60);
    }

    #[test]
    fn ephemeral_workspace_drops_the_bracket_chrome() {
        // An ephemeral "(no workspace)" context shows no `[workspace]` chrome — just the buffer label,
        // like there's no workspace. With no label it's the bare app name.
        assert!(!shows_workspace_chrome("ephemeral/3"));
        assert_eq!(
            title_body("ephemeral/3", "outside.rs"),
            Some("outside.rs".into())
        );
        assert_eq!(title_body("ephemeral/3", ""), None);
        assert_eq!(window_title("ephemeral/3", "outside.rs"), "outside.rs");
        assert_eq!(window_title("ephemeral/3", ""), APP_NAME);
        // A persisted workspace still gets the bracket.
        assert!(shows_workspace_chrome("demo"));
    }
}
