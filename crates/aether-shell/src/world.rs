//! What the language needs to know about the world, and nothing more.
//!
//! Validation asks four questions — what is this variable, what is at this path, what does this
//! directory hold, where is home — and this trait is exactly those. The server answers them over
//! the shell's own directory and environment; tests answer them over a map.

use std::path::{Path, PathBuf};

/// What a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Dir,
    File {
        executable: bool,
    },
    /// Something else: a socket, a device, a broken link. Exists, but is neither of the above.
    Other,
}

pub trait World {
    /// The variable's value, if it is set. `PATH` and `HOME` come through here too.
    fn variable(&self, name: &str) -> Option<String>;

    /// What `path` names, or `None` when nothing does. `path` is absolute.
    fn path_kind(&self, path: &Path) -> Option<PathKind>;

    /// The entries of the directory at `dir` (absolute): each name and what it is. Empty for a
    /// directory that cannot be read.
    fn list_dir(&self, dir: &Path) -> Vec<(String, PathKind)>;

    /// Where `~` points.
    fn home(&self) -> Option<PathBuf> {
        self.variable("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
    }
}

/// `path` with `.` and `..` resolved lexically — the way a shell's `$PWD` is kept, so that `..`
/// from inside a symlinked directory goes where the prompt said you were rather than where the
/// filesystem says the link points.
pub fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_resolves_dots_lexically() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(normalize(Path::new("/a/../..")), PathBuf::from("/"));
        assert_eq!(normalize(Path::new("/")), PathBuf::from("/"));
    }
}
