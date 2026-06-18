//! Project configuration and runtime discovery files.
//!
//! - Durable config: `$XDG_CONFIG_HOME/aether/projects/<name>.toml`
//! - Runtime info:   `$XDG_RUNTIME_DIR/aether/server.json` (one file per running server, not per
//!   project — a single server now hosts many projects, picked per-client via `project/activate`).
//!   `$XDG_RUNTIME_DIR` only exists on Linux/BSD; on macOS (and anywhere it's unset) we fall back
//!   to the user cache dir (`~/Library/Caches/aether/` on macOS), which is the right home for
//!   per-machine-session bookkeeping that needn't survive a reboot.

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Fixed loopback port. Single-instance: only one server can bind it. The canonical definition
/// (shared with the clients, which hard-code it) lives in `aether_protocol`.
pub use aether_protocol::SERVER_PORT;

pub use aether_protocol::settings::AppSettings;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Project name. Derived from the config *filename* (`<name>.toml`), which is authoritative —
    /// it is not stored in the file. `#[serde(skip)]` keeps it off both the parse and write paths;
    /// `load_project` injects it after parsing.
    #[serde(skip)]
    pub name: String,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub pid: u32,
    pub port: u16,
    pub started_at_unix_ms: u64,
}

pub fn load_project(name: &str) -> anyhow::Result<ProjectConfig> {
    let path = project_config_path(name)?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading project config at {}", path.display()))?;
    let mut config: ProjectConfig = toml::from_str(&content)
        .with_context(|| format!("parsing project config at {}", path.display()))?;
    // The filename is the source of truth for the project name; the file body doesn't carry it.
    config.name = name.to_string();
    Ok(config)
}

pub fn project_config_path(name: &str) -> anyhow::Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("could not determine XDG base directories"))?;
    Ok(base
        .config_dir()
        .join("aether")
        .join("projects")
        .join(format!("{name}.toml")))
}

/// Path to the global application-settings file (`$XDG_CONFIG_HOME/aether/settings.toml`). One file
/// per machine, independent of which project is active — see `aether_protocol::settings`.
pub fn app_settings_path() -> anyhow::Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("could not determine XDG base directories"))?;
    Ok(base.config_dir().join("aether").join("settings.toml"))
}

/// Load the application settings. A missing file is not an error — a fresh install has no
/// `settings.toml`, so we return [`AppSettings::default`]. Every field carries a serde default, so
/// a file written by an older build (missing newer keys) still parses.
pub fn load_app_settings() -> anyhow::Result<AppSettings> {
    load_app_settings_at(&app_settings_path()?)
}

/// Write (or overwrite) the application settings. Creates the config directory if needed.
pub fn write_app_settings(settings: &AppSettings) -> anyhow::Result<()> {
    write_app_settings_at(&app_settings_path()?, settings)
}

/// Path-parameterized core of [`load_app_settings`], kept free of XDG resolution so it can be
/// unit-tested against a tempdir without clobbering the developer's real settings.
fn load_app_settings_at(path: &Path) -> anyhow::Result<AppSettings> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AppSettings::default()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading app settings at {}", path.display()))
        }
    };
    toml::from_str(&content).with_context(|| format!("parsing app settings at {}", path.display()))
}

/// Path-parameterized core of [`write_app_settings`]. See [`load_app_settings_at`].
fn write_app_settings_at(path: &Path, settings: &AppSettings) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let body = toml::to_string_pretty(settings).context("serializing app settings")?;
    std::fs::write(path, body)
        .with_context(|| format!("writing app settings at {}", path.display()))?;
    Ok(())
}

/// Directory containing the per-project `.toml` configs. Used by `list_project_names`.
pub fn projects_dir() -> anyhow::Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("could not determine XDG base directories"))?;
    Ok(base.config_dir().join("aether").join("projects"))
}

/// Enumerate the configured project names by scanning `*.toml` files in `projects_dir`. The
/// file *name* (without extension) is the project name; the body carries only `paths`.
/// Returns an empty list (not an error) when the directory doesn't exist yet — a fresh
/// install with no projects configured shouldn't be a server-side fatal.
pub fn list_project_names() -> anyhow::Result<Vec<String>> {
    let dir = projects_dir()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading projects dir {}", dir.display())),
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                return None;
            }
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .collect();
    names.sort();
    Ok(names)
}

/// Outcome of inferring which configured project owns a given path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectMatch {
    /// Exactly one project (most-specific root wins) owns the path.
    One(String),
    /// No configured project's roots contain the path.
    None,
    /// Several projects contain the path with equal specificity — the caller must disambiguate.
    /// Names are sorted.
    Ambiguous(Vec<String>),
}

/// Best-effort absolute, symlink-resolved form of a path that may not exist yet (e.g. a file the
/// user is about to create): canonicalize the longest existing ancestor and re-append the
/// remaining tail. This lets `ae src/new_file.rs` still resolve into a project even though the
/// file isn't on disk. Falls back to a plain absolute path if nothing can be canonicalized.
pub fn resolve_path_for_match(path: &Path) -> PathBuf {
    let expanded = expand_home(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&expanded))
            .unwrap_or(expanded)
    };
    let mut ancestor = absolute.as_path();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canon) = std::fs::canonicalize(ancestor) {
            let mut result = canon;
            for comp in tail.iter().rev() {
                result.push(comp);
            }
            return result;
        }
        match (ancestor.file_name(), ancestor.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                ancestor = parent;
            }
            _ => return absolute,
        }
    }
}

/// Infer which configured project owns `path` by matching it against every project's canonical
/// roots. The path is resolved (and made absolute) first. Most-specific match wins: the project
/// with the deepest containing root is chosen, so a project rooted inside another doesn't collide
/// with its parent. A genuine tie at the deepest root is reported as [`ProjectMatch::Ambiguous`].
pub fn infer_project_for_path(path: &Path) -> anyhow::Result<ProjectMatch> {
    let target = resolve_path_for_match(path);
    let mut projects: Vec<(String, Vec<PathBuf>)> = Vec::new();
    for name in list_project_names()? {
        // Skip projects whose config won't load — one stale config shouldn't break inference for
        // everything else.
        let Ok(config) = load_project(&name) else {
            continue;
        };
        let roots = config
            .paths
            .iter()
            .filter_map(|root| canonicalize_project_path(root).ok())
            .collect();
        projects.push((name, roots));
    }
    Ok(match_project(&target, &projects))
}

/// Pure core of [`infer_project_for_path`]: pick the most-specific project for a resolved target
/// among each project's canonical roots. Kept free of disk access so it can be unit-tested.
fn match_project(target: &Path, projects: &[(String, Vec<PathBuf>)]) -> ProjectMatch {
    let mut matches: Vec<(String, usize)> = Vec::new();
    for (name, roots) in projects {
        let best = roots
            .iter()
            .filter(|root| target == root.as_path() || target.starts_with(root))
            .map(|root| root.components().count())
            .max();
        if let Some(depth) = best {
            matches.push((name.clone(), depth));
        }
    }
    let Some(max_depth) = matches.iter().map(|(_, d)| *d).max() else {
        return ProjectMatch::None;
    };
    let mut winners: Vec<String> = matches
        .into_iter()
        .filter(|(_, d)| *d == max_depth)
        .map(|(name, _)| name)
        .collect();
    winners.sort();
    if winners.len() == 1 {
        ProjectMatch::One(winners.into_iter().next().unwrap())
    } else {
        ProjectMatch::Ambiguous(winners)
    }
}

pub fn runtime_info_path() -> anyhow::Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("could not determine XDG base directories"))?;
    // `runtime_dir()` is `Some` only where `$XDG_RUNTIME_DIR` is set (Linux/BSD). Elsewhere
    // (notably macOS) fall back to the cache dir — the runtime file is disposable session state.
    let dir = base.runtime_dir().unwrap_or_else(|| base.cache_dir());
    Ok(dir.join("aether").join("server.json"))
}

pub fn write_runtime_info(path: &Path, info: &RuntimeInfo) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating runtime dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(info)?;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating runtime file {}", path.display()))?;
    file.write_all(json.as_bytes())?;
    Ok(())
}

pub fn read_runtime_info(path: &Path) -> anyhow::Result<RuntimeInfo> {
    let content = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

/// Whether a process with the given pid is currently alive. Portable across Linux and macOS:
/// `kill(pid, 0)` sends no signal but performs the existence/permission check. It returns 0 when
/// the process exists, or fails with `EPERM` when it exists but we can't signal it — either way
/// it's alive. Only `ESRCH` (no such process) means dead.
pub fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 has no side effects beyond the existence check.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Expand a leading `~/` or bare `~` to the user's home directory. Leaves the path unchanged
/// otherwise.
pub fn expand_home(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(home) = directories::UserDirs::new().map(|u| u.home_dir().to_path_buf()) else {
        return path.to_path_buf();
    };
    if s == "~" {
        home
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else {
        path.to_path_buf()
    }
}

/// Canonicalize a project path. Errors loudly if the path doesn't exist — better to fail at
/// startup than silently mis-resolve later.
pub fn canonicalize_project_path(p: &Path) -> anyhow::Result<PathBuf> {
    let expanded = expand_home(p);
    std::fs::canonicalize(&expanded)
        .with_context(|| format!("canonicalizing project path {}", expanded.display()))
}

/// Write (or overwrite) a project's TOML config. Creates the projects directory if it doesn't
/// yet exist. Caller is responsible for refusing to overwrite when not desired (see
/// `project_config_exists`).
pub fn write_project_config(config: &ProjectConfig) -> anyhow::Result<()> {
    let path = project_config_path(&config.name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating projects dir {}", parent.display()))?;
    }
    let body = toml::to_string_pretty(config)
        .with_context(|| format!("serializing project config {}", config.name))?;
    std::fs::write(&path, body)
        .with_context(|| format!("writing project config at {}", path.display()))?;
    Ok(())
}

/// True if `<projects_dir>/<name>.toml` already exists. Used by `project/create` to refuse
/// overwriting an existing config.
pub fn project_config_exists(name: &str) -> anyhow::Result<bool> {
    Ok(project_config_path(name)?.exists())
}

/// Rename a project's TOML config on disk (`<old>.toml` → `<new>.toml`). Used by
/// `project/rename`. The caller is responsible for refusing when the destination already exists
/// (see `project_config_exists`) — `fs::rename` would otherwise silently clobber it.
pub fn rename_project_config(old: &str, new: &str) -> anyhow::Result<()> {
    let from = project_config_path(old)?;
    let to = project_config_path(new)?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating projects dir {}", parent.display()))?;
    }
    std::fs::rename(&from, &to).with_context(|| {
        format!(
            "renaming project config {} -> {}",
            from.display(),
            to.display()
        )
    })?;
    Ok(())
}

/// Delete a project's TOML config from disk. Used by `project/delete`. Does not remove the source
/// files under the project's roots — only the project definition. A missing file is treated as
/// success (the end state — no config — is what was asked for).
pub fn delete_project_config(name: &str) -> anyhow::Result<()> {
    let path = project_config_path(name)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("deleting project config at {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proj(name: &str, roots: &[&str]) -> (String, Vec<PathBuf>) {
        (name.to_string(), roots.iter().map(PathBuf::from).collect())
    }

    #[test]
    fn no_project_contains_path() {
        let projects = [proj("work", &["/home/joe/work"])];
        assert_eq!(
            match_project(Path::new("/tmp/elsewhere/file.rs"), &projects),
            ProjectMatch::None
        );
    }

    #[test]
    fn single_project_match() {
        let projects = [proj("work", &["/home/joe/work"]), proj("dots", &["/home/joe/.config"])];
        assert_eq!(
            match_project(Path::new("/home/joe/work/src/main.rs"), &projects),
            ProjectMatch::One("work".to_string())
        );
    }

    #[test]
    fn path_equal_to_root_matches() {
        let projects = [proj("work", &["/home/joe/work"])];
        assert_eq!(
            match_project(Path::new("/home/joe/work"), &projects),
            ProjectMatch::One("work".to_string())
        );
    }

    #[test]
    fn most_specific_nested_root_wins() {
        // `sub` is rooted inside `work`; a path under `sub` belongs to the deeper project.
        let projects = [
            proj("work", &["/home/joe/work"]),
            proj("sub", &["/home/joe/work/sub"]),
        ];
        assert_eq!(
            match_project(Path::new("/home/joe/work/sub/file.rs"), &projects),
            ProjectMatch::One("sub".to_string())
        );
        // A sibling under `work` but outside `sub` still resolves to `work`.
        assert_eq!(
            match_project(Path::new("/home/joe/work/other/file.rs"), &projects),
            ProjectMatch::One("work".to_string())
        );
    }

    #[test]
    fn equal_depth_tie_is_ambiguous() {
        // Two projects share the same root — a genuine tie.
        let projects = [
            proj("alpha", &["/home/joe/shared"]),
            proj("beta", &["/home/joe/shared"]),
        ];
        assert_eq!(
            match_project(Path::new("/home/joe/shared/x.rs"), &projects),
            ProjectMatch::Ambiguous(vec!["alpha".to_string(), "beta".to_string()])
        );
    }

    #[test]
    fn deepest_root_across_projects_breaks_what_would_be_a_tie() {
        // Both contain the path, but `beta`'s root is one level deeper, so it wins outright.
        let projects = [
            proj("alpha", &["/home/joe"]),
            proj("beta", &["/home/joe/work"]),
        ];
        assert_eq!(
            match_project(Path::new("/home/joe/work/file.rs"), &projects),
            ProjectMatch::One("beta".to_string())
        );
    }

    #[test]
    fn runtime_info_path_resolves_without_xdg_runtime_dir() {
        // The whole point of the macOS fix: this must not error when `$XDG_RUNTIME_DIR` is unset.
        // We can't reliably unset it here (other tests/threads share the env), so just assert the
        // path resolves and lands under our `aether/server.json` regardless of which base was used.
        let path = runtime_info_path().expect("runtime_info_path should never fail on a fallback");
        assert!(path.ends_with("aether/server.json"), "unexpected path: {}", path.display());
    }

    #[test]
    fn app_settings_missing_file_is_default() {
        use aether_protocol::viewport::WrapMode;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        // No file yet → defaults, not an error.
        let s = load_app_settings_at(&path).unwrap();
        assert_eq!(s.wrap, WrapMode::Soft);
    }

    #[test]
    fn app_settings_round_trip_through_disk() {
        use aether_protocol::viewport::WrapMode;
        let dir = tempfile::tempdir().unwrap();
        // Nested path exercises the create-parent branch.
        let path = dir.path().join("aether").join("settings.toml");
        write_app_settings_at(&path, &AppSettings { wrap: WrapMode::None }).unwrap();
        let s = load_app_settings_at(&path).unwrap();
        assert_eq!(s.wrap, WrapMode::None);
    }

    #[test]
    fn app_settings_partial_file_fills_defaults() {
        // An empty (or older) file with no keys reads back as all-defaults rather than failing.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "").unwrap();
        assert_eq!(load_app_settings_at(&path).unwrap(), AppSettings::default());
    }

    #[test]
    fn current_process_is_alive() {
        assert!(pid_is_alive(std::process::id()));
    }

    #[test]
    fn nonexistent_pid_is_dead() {
        // pid 0 is the process group / scheduler — `kill(0, 0)` would signal our own group, not a
        // real liveness probe, so use a pid that cannot exist instead. `i32::MAX` as a pid is far
        // above any real allocation on Linux or macOS, so `kill` returns ESRCH.
        assert!(!pid_is_alive(i32::MAX as u32));
    }
}
