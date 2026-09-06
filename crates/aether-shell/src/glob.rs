//! Globs: `*`, `?`, `[…]` within a path component, and `**` as a whole component matching any
//! number of directories. Matched against what the [`World`] says a directory holds, so the
//! matcher is tested without a filesystem.
//!
//! Only characters typed bare are live: a `*` from inside quotes, or from a variable's value, is
//! a literal asterisk. That is what the `live` flag on each character carries.

use crate::world::{PathKind, World};
use std::path::{Path, PathBuf};

/// One character of a pattern, and whether it was typed bare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatChar {
    pub c: char,
    pub live: bool,
}

/// Whether any live glob character is present — the test for "does this word glob at all".
pub fn has_glob(pat: &[PatChar]) -> bool {
    pat.iter().any(|p| p.live && matches!(p.c, '*' | '?' | '['))
}

enum Component<'a> {
    Literal(String),
    Glob(&'a [PatChar]),
    DoubleStar,
}

/// Every path the pattern names, relative if the pattern was, sorted. Dotfiles match only a
/// pattern component that starts with a literal `.`.
pub fn expand(pat: &[PatChar], cwd: &Path, world: &impl World) -> Vec<PathBuf> {
    let absolute = pat.first().is_some_and(|p| p.c == '/');
    let components: Vec<Component<'_>> = pat
        .split(|p| p.c == '/')
        .filter(|c| !c.is_empty())
        .map(|c| {
            if c.iter().all(|p| p.live && p.c == '*') && c.len() == 2 {
                Component::DoubleStar
            } else if has_glob(c) {
                Component::Glob(c)
            } else {
                Component::Literal(c.iter().map(|p| p.c).collect())
            }
        })
        .collect();
    let (base, shown) = if absolute {
        (PathBuf::from("/"), PathBuf::from("/"))
    } else {
        (cwd.to_path_buf(), PathBuf::new())
    };
    let mut out = Vec::new();
    walk(world, base, shown, &components, &mut out);
    out.sort();
    out.dedup();
    out
}

fn walk(
    world: &impl World,
    base: PathBuf,
    shown: PathBuf,
    components: &[Component<'_>],
    out: &mut Vec<PathBuf>,
) {
    let Some((first, rest)) = components.split_first() else {
        out.push(shown);
        return;
    };
    match first {
        Component::Literal(name) => {
            let next = base.join(name);
            if world.path_kind(&next).is_none() {
                return;
            }
            walk(world, next, shown.join(name), rest, out);
        }
        Component::Glob(pat) => {
            let dot_ok = pat.first().is_some_and(|p| p.c == '.');
            for (name, kind) in world.list_dir(&base) {
                if name.starts_with('.') && !dot_ok {
                    continue;
                }
                if !matches(pat, &name) {
                    continue;
                }
                if rest.is_empty() {
                    out.push(shown.join(&name));
                } else if kind == PathKind::Dir {
                    walk(world, base.join(&name), shown.join(&name), rest, out);
                }
            }
        }
        Component::DoubleStar => {
            // Zero directories deep, then every directory below, not descending into dotdirs.
            walk(world, base.clone(), shown.clone(), rest, out);
            for (name, kind) in world.list_dir(&base) {
                if kind == PathKind::Dir && !name.starts_with('.') {
                    walk(world, base.join(&name), shown.join(&name), components, out);
                }
            }
        }
    }
}

/// Whether `name` matches one pattern component.
pub fn matches(pat: &[PatChar], name: &str) -> bool {
    let name: Vec<char> = name.chars().collect();
    matches_at(pat, &name)
}

fn matches_at(pat: &[PatChar], name: &[char]) -> bool {
    let Some((p, rest)) = pat.split_first() else {
        return name.is_empty();
    };
    if p.live {
        match p.c {
            '*' => {
                // Greedy with backtracking: try every split point.
                (0..=name.len()).any(|k| matches_at(rest, &name[k..]))
            }
            '?' => !name.is_empty() && matches_at(rest, &name[1..]),
            '[' => match class(pat) {
                Some((set, after)) => {
                    !name.is_empty() && set.contains(name[0]) && matches_at(after, &name[1..])
                }
                // An unclosed `[` is a literal bracket.
                None => !name.is_empty() && name[0] == '[' && matches_at(rest, &name[1..]),
            },
            c => !name.is_empty() && name[0] == c && matches_at(rest, &name[1..]),
        }
    } else {
        !name.is_empty() && name[0] == p.c && matches_at(rest, &name[1..])
    }
}

struct Class {
    negated: bool,
    ranges: Vec<(char, char)>,
}

impl Class {
    fn contains(&self, c: char) -> bool {
        self.ranges.iter().any(|(lo, hi)| *lo <= c && c <= *hi) != self.negated
    }
}

/// Parse `[…]` at the start of `pat`; `None` when it never closes.
fn class(pat: &[PatChar]) -> Option<(Class, &[PatChar])> {
    let mut i = 1;
    let negated = pat.get(i).is_some_and(|p| matches!(p.c, '!' | '^'));
    if negated {
        i += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let p = pat.get(i)?;
        if p.c == ']' && !first {
            return Some((Class { negated, ranges }, &pat[i + 1..]));
        }
        first = false;
        let lo = p.c;
        if pat.get(i + 1).is_some_and(|d| d.c == '-') && pat.get(i + 2).is_some_and(|h| h.c != ']')
        {
            ranges.push((lo, pat[i + 2].c));
            i += 3;
        } else {
            ranges.push((lo, lo));
            i += 1;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;

    pub(crate) fn live(s: &str) -> Vec<PatChar> {
        s.chars().map(|c| PatChar { c, live: true }).collect()
    }

    /// A world that is a map from absolute paths to what they are.
    pub(crate) struct Fake {
        pub vars: HashMap<String, String>,
        pub tree: HashMap<PathBuf, PathKind>,
    }

    impl Fake {
        pub(crate) fn new(entries: &[(&str, PathKind)]) -> Self {
            let mut tree = HashMap::new();
            for (p, k) in entries {
                let path = PathBuf::from(p);
                for parent in path.ancestors().skip(1) {
                    tree.entry(parent.to_path_buf()).or_insert(PathKind::Dir);
                }
                tree.insert(path, *k);
            }
            Fake {
                vars: HashMap::new(),
                tree,
            }
        }
    }

    impl World for Fake {
        fn variable(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }

        fn path_kind(&self, path: &Path) -> Option<PathKind> {
            self.tree.get(&crate::world::normalize(path)).copied()
        }

        fn list_dir(&self, dir: &Path) -> Vec<(String, PathKind)> {
            let dir = crate::world::normalize(dir);
            let mut out: Vec<_> = self
                .tree
                .iter()
                .filter(|(p, _)| p.parent() == Some(dir.as_path()))
                .map(|(p, k)| (p.file_name().unwrap().to_string_lossy().into_owned(), *k))
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }
    }

    const FILE: PathKind = PathKind::File { executable: false };

    #[test]
    fn component_matching() {
        assert!(matches(&live("*.rs"), "main.rs"));
        assert!(!matches(&live("*.rs"), "main.rsx"));
        assert!(matches(&live("?ain.rs"), "main.rs"));
        assert!(matches(&live("[a-m]ain.rs"), "main.rs"));
        assert!(!matches(&live("[!a-m]ain.rs"), "main.rs"));
        assert!(matches(&live("*"), ""));
        // A `*` from quotes or a value is a literal asterisk.
        let quoted: Vec<_> = "*.rs".chars().map(|c| PatChar { c, live: false }).collect();
        assert!(matches(&quoted, "*.rs"));
        assert!(!matches(&quoted, "main.rs"));
    }

    #[test]
    fn expansion_walks_the_tree_and_skips_dotfiles() {
        let w = Fake::new(&[
            ("/p/a.rs", FILE),
            ("/p/b.rs", FILE),
            ("/p/.hidden.rs", FILE),
            ("/p/src/lib.rs", FILE),
            ("/p/src/deep/x.rs", FILE),
            ("/p/notes.md", FILE),
        ]);
        let cwd = Path::new("/p");
        let names = |pat: &str| -> Vec<String> {
            expand(&live(pat), cwd, &w)
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(names("*.rs"), vec!["a.rs", "b.rs"]);
        assert_eq!(names(".*.rs"), vec![".hidden.rs"]);
        assert_eq!(names("src/*.rs"), vec!["src/lib.rs"]);
        assert_eq!(
            names("**/*.rs"),
            vec!["a.rs", "b.rs", "src/deep/x.rs", "src/lib.rs"]
        );
        assert_eq!(names("/p/s*/lib.rs"), vec!["/p/src/lib.rs"]);
        assert!(names("*.txt").is_empty());
        assert!(names("missing/*.rs").is_empty());
    }
}
