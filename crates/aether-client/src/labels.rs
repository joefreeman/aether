//! Disambiguated labels for project roots. Default is the root's basename; when two roots share
//! a basename, both labels grow a parenthesized parent component, then grandparent, etc., until
//! they're unique. Lives client-side because it's a pure presentation concern — the server sends
//! root indices, the client decides how to print them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
