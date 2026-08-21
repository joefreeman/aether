//! Git worktrees: the store, the naming rules, the reads, and the seeding.
//!
//! Stage 7 of the git work — `docs/worktrees.md` has the research and the design. The split with
//! [`crate::git_cli`] is the house rule sharpened: **libgit2 lists and validates worktrees, the
//! `git` CLI creates and removes them.** That isn't a preference. Vendored libgit2 1.9.4 can't
//! produce a detached HEAD, has no `--force`, uses the admin name verbatim as both a directory and
//! a branch name, and leaves `.git/worktrees/<n>` behind when creation fails halfway (§5).
//!
//! ## The store
//!
//! Worktrees are created in one app-managed directory, **not** as siblings of the repo the way VS
//! Code and magit do it. A centralised store is invisible to `reachable_repos` until explicitly
//! opened — no root contains it and it contains no root — so creating a worktree perturbs the
//! current workspace not at all. The sibling convention gets swallowed by any broad root like
//! `~/Projects`, showing up as an untracked path in status and duplicating every search hit.
//!
//! It sits outside the profile *state* subtree, which is documented as sweepable: worktrees hold
//! uncommitted work and nothing that clears machine state may go near them.
//!
//! ## Names
//!
//! Three names, deliberately distinct, and the bugs come from letting them merge:
//!
//! | | Example | Chosen by |
//! |---|---|---|
//! | branch | `feature/auth` | the user, typed |
//! | admin name | `feature-auth` | derived here, sanitised + uniquified |
//! | directory | `<store>/aether-3f9c/feature-auth` | derived from the admin name |
//!
//! The derivation runs **one way only**. Zed #47208 let a sanitised directory name flow back and
//! truncated the branch to `my-feature`; orca #13011 is the same bug mirrored. Nothing here ever
//! computes a branch name from a directory name.

use aether_protocol::git::{GitWorktreeAtRisk, GitWorktreeRow};
use std::path::{Path, PathBuf};

/// Where worktrees are created: `override_dir` (the server's
/// [`crate::state::ServerState::worktree_store`], which tests point at a tempdir) if set, else the
/// `worktree_store` app setting, else `$XDG_DATA_HOME/aether/worktrees`.
///
/// Not under `profile_state_dir()`: that subtree is documented as machine state safe to delete, and
/// a worktree may hold hours of uncommitted work. Not under a workspace root either — see the
/// module docs.
///
/// A **setting** rather than an environment variable, which is what this was first: Aether is
/// launched from a desktop entry (`ae --gui %f`) as often as from a shell, and a variable exported
/// in a shell rc never reaches that process. `AppSettings` had to drop its `Copy` derive to hold a
/// path, which was the right trade — the derive was an accident of every field having been a scalar.
pub fn store_root(override_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(dir) = override_dir {
        return Ok(dir.to_path_buf());
    }
    let configured = crate::config::load_app_settings()
        .map(|s| s.worktree_store)
        .unwrap_or_default();
    if !configured.is_empty() {
        return Ok(PathBuf::from(configured));
    }
    Ok(crate::config::data_dir()?.join("worktrees"))
}

/// The store subdirectory for one repo **family**, keyed by common dir.
///
/// Namespaced per repo because a flat store collides the moment two repos both want
/// `feature-auth`. The basename keeps it legible when you `cd` into the store by hand; the hash is
/// what makes it unique, since two checkouts of the same project have the same basename.
///
/// Keyed by *common dir* rather than workdir on purpose: every worktree of a family must land in
/// one bucket, and the common dir is the only identifier they share (`docs/worktrees.md` §1).
pub fn repo_key(common_dir: &Path) -> String {
    // The common dir is `<repo>/.git`, so the repo's own name is one level up.
    let basename = common_dir
        .parent()
        .and_then(|p| p.file_name())
        .or_else(|| common_dir.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let basename = sanitize_path_component(&basename);
    format!("{basename}-{:08x}", path_hash(common_dir))
}

/// FNV-1a over the path's bytes. Hand-rolled rather than `DefaultHasher` because this value ends
/// up in a directory name that must stay the same across builds — `RandomState` is seeded per
/// process and `SipHasher`'s output is explicitly not a stable format.
fn path_hash(path: &Path) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Reduce a string to something safe as a single path component: no separators, no `..`, no
/// leading dot, no control characters. Used for the *directory* half of the naming table only.
fn sanitize_path_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(['.', ' ', '-']);
    if trimmed.is_empty() {
        "repo".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Derive a worktree admin name from a branch name, mirroring git's own
/// `sanitize_refname_component`.
///
/// Slashes become dashes rather than nested directories: `feature/auth` must not create
/// `<store>/<repo>/feature/auth`, which is the nested-directory half of Zed #47208. **The result
/// never flows back into the branch name** — the caller passes the user's branch string to git
/// untouched.
pub fn admin_name_for_branch(branch: &str) -> String {
    let mut out = String::with_capacity(branch.len());
    for c in branch.chars() {
        // git's refname rules, applied to a directory component: these are the characters it
        // refuses outright, plus the separator we're flattening.
        if c.is_control()
            || matches!(
                c,
                '/' | '\\' | ' ' | '~' | '^' | ':' | '?' | '*' | '[' | ']'
            )
        {
            out.push('-');
        } else {
            out.push(c);
        }
    }
    // Collapse runs and trim the characters git refuses at the ends of a component.
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let trimmed = out.trim_matches(['-', '.']);
    if trimmed.is_empty() {
        "worktree".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `base`, or `base1`, `base2`, … until nothing in the family uses it and the directory is free.
///
/// Copies git's own uniquifier (`builtin/worktree.c` appends until `mkdir` succeeds) rather than
/// inventing a scheme, so a worktree made here and one made in a terminal are named the same way.
///
/// Comparison is case-insensitive: on macOS and Windows `Feature` and `feature` are the same
/// directory, and a name that "isn't taken" would collide on `mkdir`.
pub fn unique_admin_name(existing: &[String], store_dir: &Path, base: &str) -> String {
    let taken: Vec<String> = existing.iter().map(|n| n.to_lowercase()).collect();
    let free = |candidate: &str| {
        !taken.contains(&candidate.to_lowercase()) && !store_dir.join(candidate).exists()
    };
    if free(base) {
        return base.to_string();
    }
    for n in 1u32.. {
        let candidate = format!("{base}{n}");
        if free(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 range is non-empty")
}

/// Every worktree of the family reachable from `workdir`, main first.
///
/// libgit2 reads throughout — this is the half of the worktree API that is sound (§5). The main
/// worktree is recovered from the common dir (`<main>/.git`, so its parent), because
/// `worktrees()` lists only the *linked* ones and a caller standing in a linked worktree would
/// otherwise never see the main checkout.
///
/// Known fragility, inherited from `branches_checked_out_elsewhere`: `commondir().parent()` is not
/// the main worktree for a `--separate-git-dir` or bare repo. Both fail safe here — the main row
/// is simply omitted rather than wrong.
pub fn list(workdir: &Path) -> Vec<GitWorktreeRow> {
    let Ok(repo) = git2::Repository::open(workdir) else {
        return Vec::new();
    };
    let here = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let mut rows = Vec::new();

    if let Some(main) = repo.commondir().parent() {
        if let Ok(main) = main.canonicalize() {
            if main.join(".git").is_dir() {
                rows.push(GitWorktreeRow {
                    name: String::new(),
                    is_main: true,
                    is_current: main == here,
                    head: head_of(&main),
                    path: main.to_string_lossy().into_owned(),
                    locked: false,
                    prunable: false,
                });
            }
        }
    }

    let Ok(names) = repo.worktrees() else {
        return rows;
    };
    for name in names.iter() {
        let Ok(Some(name)) = name else { continue };
        let Ok(worktree) = repo.find_worktree(name) else {
            continue;
        };
        let name = name.to_string();
        let path = worktree.path().to_path_buf();
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        // `validate` failing means the admin entry outlived its directory — someone `rm -rf`'d the
        // tree instead of using `git worktree remove`. That, and only that, is what may be pruned.
        let prunable = worktree.validate().is_err();
        rows.push(GitWorktreeRow {
            name,
            is_main: false,
            is_current: canonical == here,
            head: if prunable { None } else { head_of(&canonical) },
            path: canonical.to_string_lossy().into_owned(),
            locked: !matches!(
                worktree.is_locked(),
                Ok(git2::WorktreeLockStatus::Unlocked) | Err(_)
            ),
            prunable,
        });
    }
    rows
}

fn head_of(workdir: &Path) -> Option<aether_protocol::git::GitHead> {
    git2::Repository::open(workdir)
        .ok()
        .and_then(|r| crate::git::head_state(&r))
}

/// Resolve a worktree admin name to its working directory, from any member of the family.
///
/// This is the resolution step the whole variant model rests on: bindings store the **admin
/// name**, never a path, so the store can move, `git worktree repair` can relocate things, and a
/// worktree created in a terminal needs no import (`docs/worktrees.md` §7.2).
pub fn path_for_name(family_member: &Path, name: &str) -> Option<PathBuf> {
    let repo = git2::Repository::open(family_member).ok()?;
    let worktree = repo.find_worktree(name).ok()?;
    worktree.validate().ok()?;
    let path = worktree.path().to_path_buf();
    Some(path.canonicalize().unwrap_or(path))
}

/// What removing this worktree would destroy: uncommitted changes to tracked files, untracked
/// files, and whether an operation is stopped mid-flight.
///
/// Committed work is never in this list — objects live in the shared common dir, so a removed
/// worktree whose branch survives loses nothing durable. This is the entire risk surface, which is
/// why the confirmation can itemise it instead of asking "are you sure?".
pub fn at_risk(workdir: &Path) -> GitWorktreeAtRisk {
    let Ok(repo) = git2::Repository::open(workdir) else {
        return GitWorktreeAtRisk::default();
    };
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .include_ignored(false)
        // A whole ignored directory is not "work at risk" — it's a build output. Untracked files
        // outside one are, and they're the ones with no copy anywhere.
        .recurse_untracked_dirs(true);
    let mut risk = GitWorktreeAtRisk {
        operation_in_progress: repo.state() != git2::RepositoryState::Clean,
        ..GitWorktreeAtRisk::default()
    };
    let Ok(statuses) = repo.statuses(Some(&mut opts)) else {
        return risk;
    };
    for entry in statuses.iter() {
        let s = entry.status();
        if s.contains(git2::Status::WT_NEW) {
            risk.untracked += 1;
        } else if !s.is_empty() {
            risk.modified += 1;
        }
    }
    risk
}

/// Does this repo have submodules? A warning on a successful create, never a refusal.
///
/// git-worktree(1)'s own BUGS section still advises against multiple checkouts of a superproject,
/// and the failure mode is silent commit loss. Refusing outright would block a workflow that does
/// work if you know the hazard, so the result carries the fact and the client says so (§4.9).
pub fn has_submodules(workdir: &Path) -> bool {
    let Ok(repo) = git2::Repository::open(workdir) else {
        return false;
    };
    repo.submodules().is_ok_and(|s| !s.is_empty())
}

/// Copy gitignored files named by `.worktreeinclude` from `source` into a freshly created `dest`.
/// Returns how many were copied.
///
/// **Why this exists**: `git worktree add` checks out tracked files only, so a new tree has no
/// `.env`, no `node_modules/`, no `target/`, no `.venv`. For a person that's a nicety. For an
/// external agent, whose first act is to build, a tree that can't build did nothing.
///
/// The shape is Claude Code's `.worktreeinclude`: gitignore syntax, and a file is copied only when
/// it **matches a pattern AND is gitignored**. The second half is what keeps this safe — tracked
/// files are never duplicated, so this can't shadow a checkout or resurrect a deleted file.
///
/// Deliberate limits, all in the name of not walking `node_modules` twice:
/// - an ignored *directory* that matches is copied whole and not descended into;
/// - an ignored directory that does **not** match is skipped entirely, so a pattern has to name
///   the ignored directory itself (`node_modules/`) rather than something buried inside one;
/// - symlinks are skipped rather than followed — copying through one writes into the target.
pub fn seed_from_include_file(source: &Path, dest: &Path) -> u32 {
    let include_file = source.join(".worktreeinclude");
    let Ok(contents) = std::fs::read_to_string(&include_file) else {
        return 0;
    };
    let mut builder = ignore::gitignore::GitignoreBuilder::new(source);
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let _ = builder.add_line(None, line);
    }
    let Ok(matcher) = builder.build() else {
        return 0;
    };
    let Ok(repo) = git2::Repository::open(source) else {
        return 0;
    };
    let mut copied = 0u32;
    seed_dir(&repo, &matcher, source, dest, Path::new(""), &mut copied);
    copied
}

fn seed_dir(
    repo: &git2::Repository,
    matcher: &ignore::gitignore::Gitignore,
    source: &Path,
    dest: &Path,
    rel: &Path,
    copied: &mut u32,
) {
    let Ok(entries) = std::fs::read_dir(source.join(rel)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let is_dir = file_type.is_dir();
        let rel_child = rel.join(&name);
        let from = source.join(&rel_child);
        let matched = matcher
            .matched_path_or_any_parents(&rel_child, is_dir)
            .is_ignore();
        let ignored = repo.is_path_ignored(&rel_child).unwrap_or(false);

        if matched && ignored {
            let to = dest.join(&rel_child);
            if is_dir {
                copy_tree(&from, &to, copied);
            } else if copy_file(&from, &to) {
                *copied += 1;
            }
            continue;
        }
        // Descend into ordinary directories looking for deeper matches; never into an ignored one
        // we didn't want, which is what stops this walking `node_modules` for nothing.
        if is_dir && !ignored {
            seed_dir(repo, matcher, source, dest, &rel_child, copied);
        }
    }
}

fn copy_tree(from: &Path, to: &Path, copied: &mut u32) {
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let child_from = entry.path();
        let child_to = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&child_from, &child_to, copied);
        } else if copy_file(&child_from, &child_to) {
            *copied += 1;
        }
    }
}

fn copy_file(from: &Path, to: &Path) -> bool {
    if let Some(parent) = to.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    std::fs::copy(from, to).is_ok()
}

/// Resolve a variant's roots: the base workspace's roots, with every root belonging to a bound repo
/// remapped into that repo's worktree.
///
/// Returns the roots and the bindings that **failed to resolve** — a worktree removed in a terminal
/// since the binding was written. Those roots fall back to the base path rather than failing the
/// activation: a variant that can't fully materialise degrades to the base, because refusing to
/// open would leave the user with no way back in (`docs/worktrees.md` §10.6).
///
/// ```text
/// root' = worktree.join(root.strip_prefix(repo_workdir))
/// ```
///
/// `strip_prefix` yields `""` when the root *is* the repo workdir, so `join("")` gives the worktree
/// itself. Those two lines therefore cover **repo-is-the-root**, **root-inside-the-repo** (the
/// monorepo `services/api` case) and **several roots in one repo**, uniformly. A root that merely
/// *contains* a repo is not remapped — it holds other things too, and remapping it would drag them
/// along.
///
/// Root count and order are preserved, which is what lets `ProjectRef::root_index` — positional —
/// carry over to a variant untouched.
pub fn materialise_roots(
    base_roots: &[PathBuf],
    bindings: &std::collections::BTreeMap<PathBuf, String>,
) -> (Vec<PathBuf>, Vec<String>) {
    let mut roots = Vec::with_capacity(base_roots.len());
    let mut unresolved = Vec::new();
    for root in base_roots {
        let Some(identity) = crate::git::discover_repo(root) else {
            roots.push(root.clone());
            continue;
        };
        let Some(name) = bindings.get(&identity.workdir) else {
            roots.push(root.clone());
            continue;
        };
        // Only a root *inside* the repo can follow it. `discover_repo` walks upward, so this is
        // almost always true — the exception is a root that contains the repo, which resolves to it
        // but must not be remapped.
        let Ok(relative) = root.strip_prefix(&identity.workdir) else {
            roots.push(root.clone());
            continue;
        };
        match path_for_name(&identity.workdir, name) {
            // `join("")` would leave a trailing separator, which is not the same *string* as the
            // path everything else compares against — and root identity is by string in the picker
            // rows, the watcher registry and `starts_with` containment. The repo-is-the-root case
            // is the common one, so this is not a corner.
            Some(worktree) if relative.as_os_str().is_empty() => roots.push(worktree),
            Some(worktree) => roots.push(worktree.join(relative)),
            None => {
                if !unresolved.contains(name) {
                    unresolved.push(name.clone());
                }
                roots.push(root.clone());
            }
        }
    }
    (roots, unresolved)
}

/// Drop bindings whose repo is no longer reachable from any of the base workspace's roots.
///
/// Structural cleanup, run at load: removing a root removes the binding for the repo it reached,
/// and a variant left with no bindings is no longer a variant at all. This is the machine-state
/// counterpart of the config file nesting `projects` inside their root so a dangling reference is
/// unrepresentable — here it *is* representable, so it is pruned instead.
pub fn prune_unreachable_bindings(
    base_roots: &[PathBuf],
    bindings: &mut std::collections::BTreeMap<PathBuf, String>,
) {
    let reachable: std::collections::HashSet<PathBuf> = base_roots
        .iter()
        .filter_map(|root| crate::git::discover_repo(root).map(|i| i.workdir))
        .collect();
    bindings.retain(|workdir, _| reachable.contains(workdir));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_name_flattens_and_never_nests() {
        // The Zed #47208 shape: a branch with a slash must not become a nested directory.
        assert_eq!(admin_name_for_branch("feature/auth"), "feature-auth");
        assert_eq!(admin_name_for_branch("fix/a/b"), "fix-a-b");
        assert_eq!(admin_name_for_branch("plain"), "plain");
    }

    #[test]
    fn admin_name_strips_what_git_refuses() {
        assert_eq!(admin_name_for_branch("a b"), "a-b");
        assert_eq!(admin_name_for_branch("wip~1"), "wip-1");
        assert_eq!(admin_name_for_branch("a^b:c?d*e[f]"), "a-b-c-d-e-f");
        // Runs collapse and the ends are trimmed, so nothing ends up as a bare separator.
        assert_eq!(admin_name_for_branch("//weird//"), "weird");
        // A name with nothing usable still has to produce a directory.
        assert_eq!(admin_name_for_branch("///"), "worktree");
        assert_eq!(admin_name_for_branch(""), "worktree");
    }

    #[test]
    fn uniquifier_appends_like_git_does() {
        let store = tempfile::tempdir().unwrap();
        let dir = store.path();
        assert_eq!(unique_admin_name(&[], dir, "feature"), "feature");
        let taken = vec!["feature".to_string()];
        assert_eq!(unique_admin_name(&taken, dir, "feature"), "feature1");
        let taken = vec!["feature".to_string(), "feature1".to_string()];
        assert_eq!(unique_admin_name(&taken, dir, "feature"), "feature2");
    }

    #[test]
    fn uniquifier_is_case_insensitive_and_sees_stray_directories() {
        // macOS and Windows would collide on `mkdir` where a case-sensitive check says "free".
        let store = tempfile::tempdir().unwrap();
        let taken = vec!["Feature".to_string()];
        assert_eq!(
            unique_admin_name(&taken, store.path(), "feature"),
            "feature1"
        );
        // A directory left behind by a failed create is taken even with no admin entry for it.
        std::fs::create_dir(store.path().join("orphan")).unwrap();
        assert_eq!(unique_admin_name(&[], store.path(), "orphan"), "orphan1");
    }

    #[test]
    fn repo_key_is_stable_legible_and_unique_per_family() {
        let a = repo_key(Path::new("/src/aether/.git"));
        let b = repo_key(Path::new("/other/aether/.git"));
        assert!(a.starts_with("aether-"), "{a} should stay legible");
        assert!(b.starts_with("aether-"));
        // Same basename, different repos: the hash is what keeps them apart.
        assert_ne!(a, b);
        // Stable across calls — it names a directory that has to be found again.
        assert_eq!(a, repo_key(Path::new("/src/aether/.git")));
    }
}
