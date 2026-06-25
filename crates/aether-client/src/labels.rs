//! Disambiguated labels for project roots. Default is the root's basename; when two roots share
//! a basename, both labels grow a parenthesized parent component, then grandparent, etc., until
//! they're unique. Lives client-side because it's a pure presentation concern — the server sends
//! root indices, the client decides how to print them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How a project id is shown to the user in a *project list* (the switcher). A persisted project
/// shows its name verbatim; an *ephemeral* one (the synthesized "no project" context that hosts
/// files opened outside any configured project) shows `(project <n>)` — mirroring scratch buffers'
/// `(scratch <n>)`, so multiple ephemeral contexts are distinguishable — rather than its internal
/// `ephemeral/<n>` id. The status bar / title use [`shows_project_chrome`] instead: they omit the
/// `[…]` entirely for an ephemeral context.
pub fn project_display(project: &str) -> String {
    match project.strip_prefix(aether_protocol::EPHEMERAL_PROJECT_PREFIX) {
        Some(n) => format!("(project {n})"),
        None => project.to_string(),
    }
}

/// Whether a project id should be wrapped in the `[project]` chrome shown in the status bar and
/// window title. False for the empty (no project active) state *and* for an ephemeral context —
/// neither is a real, named project, so we show just the buffer label with no bracket rather than
/// a `[(no project)]` that reads like a project literally named that.
pub fn shows_project_chrome(project: &str) -> bool {
    !project.is_empty() && !aether_protocol::is_ephemeral_project_id(project)
}

/// Max characters the window-title path label is shown at before [`truncate_path`] elides it.
/// Title bars have no width budget of their own (unlike the status bar, which truncates to the row
/// width), so this is a fixed cap — long enough for a typical project-relative path, short enough
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

/// The window/terminal title's *body* for `(project, buffer label)`: `None` before a project is
/// active (the title is then just the app name), otherwise `[project]` or `[project] label`. The
/// `[…]` is omitted entirely when there's no real project — a connecting/chooser state (so no
/// stray `[]`) *and* an ephemeral "(no project)" context, which shows just the buffer label.
/// Long paths are segment-elided (see [`truncate_path`]) so an external file's absolute path doesn't
/// overflow the title bar. Shells append `" - Aether"` (and the TUI prepends a dirty dot).
pub fn title_body(project: &str, label: &str) -> Option<String> {
    if project.is_empty() {
        // Boot / connecting / chooser: no project *and* no buffer — the title is just the app name.
        return None;
    }
    let label = truncate_path(label, TITLE_LABEL_MAX);
    if aether_protocol::is_ephemeral_project_id(project) {
        // Ephemeral "(no project)" context: show the buffer label alone (the filename you're
        // editing), with no `[project]` bracket — or nothing when there's no label.
        return (!label.is_empty()).then_some(label);
    }
    Some(if label.is_empty() {
        format!("[{project}]")
    } else {
        format!("[{project}] {label}")
    })
}

/// The full window title: the [`title_body`] plus `" - Aether"`, or just `"Aether"` when no
/// project is active (so a fresh/connecting window reads `Aether`, not `[] `).
pub fn window_title(project: &str, label: &str) -> String {
    match title_body(project, label) {
        Some(body) => format!("{body} - Aether"),
        None => "Aether".to_string(),
    }
}

/// Cap on disambiguation passes. Real projects never need more than a couple, but the loop has
/// to terminate when two paths are literally identical (which `add_root` refuses, but defensive
/// belt-and-braces is cheap).
const MAX_DEPTH: usize = 16;

/// One label per input path, aligned by index. Identical inputs produce identical labels (we
/// can't disambiguate them); otherwise every label is unique within the result.
///
/// Single-root projects get an empty string — there's nothing to disambiguate against, so
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

#[cfg(test)]
mod tests {
    use super::*;

    fn s(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(root_labels(&[]).is_empty());
    }

    #[test]
    fn window_title_omits_empty_project_and_appends_app_name() {
        // No project (boot/connecting/chooser): just the app name — never a stray `[]`.
        assert_eq!(window_title("", ""), "Aether");
        assert_eq!(window_title("", "ignored"), "Aether");
        assert_eq!(title_body("", ""), None);
        // With a project, the `[project] label` body gains the " - Aether" suffix.
        assert_eq!(window_title("demo", ""), "[demo] - Aether");
        assert_eq!(
            window_title("demo", "src/main.rs"),
            "[demo] src/main.rs - Aether"
        );
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
        assert_eq!(truncate_path("a/b/c/d/e/filename.rs", 16), "…/e/filename.rs");
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
        let body = title_body("demo", long).expect("a project with a label has a body");
        assert!(body.starts_with("[demo] "), "project chrome is preserved");
        assert!(body.contains('…'), "the long path is elided: {body}");
        assert!(body.ends_with("mod.rs"), "the filename survives: {body}");
        // The label portion is capped (project chrome + a bounded label).
        assert!(body.chars().count() <= "[demo] ".chars().count() + 60);
    }

    #[test]
    fn ephemeral_project_drops_the_bracket_chrome() {
        // An ephemeral "(no project)" context shows no `[project]` chrome — just the buffer label,
        // like there's no project. With no label it's the bare app name.
        assert!(!shows_project_chrome("ephemeral/3"));
        assert_eq!(
            title_body("ephemeral/3", "outside.rs"),
            Some("outside.rs".into())
        );
        assert_eq!(title_body("ephemeral/3", ""), None);
        assert_eq!(
            window_title("ephemeral/3", "outside.rs"),
            "outside.rs - Aether"
        );
        assert_eq!(window_title("ephemeral/3", ""), "Aether");
        // A persisted project still gets the bracket.
        assert!(shows_project_chrome("demo"));
    }

    #[test]
    fn single_root_returns_empty_label() {
        // Single-root projects don't need a disambiguator — renderers fall through to a label-
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
}
