//! Project configuration and runtime discovery files.
//!
//! - Durable config: `$XDG_CONFIG_HOME/aether/projects/<name>.toml`
//! - Runtime info:   `$XDG_RUNTIME_DIR/aether/server.json` (one file per running server, not per
//!   project — a single server now hosts many projects, picked per-client via `project/activate`).

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Fixed loopback port. Single-instance: only one server can bind it. Clients hard-code this.
pub const SERVER_PORT: u16 = 2384;

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
    pub token: String,
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
        Err(e) => {
            return Err(e).with_context(|| format!("reading projects dir {}", dir.display()))
        }
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

pub fn runtime_info_path() -> anyhow::Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("could not determine XDG base directories"))?;
    let runtime = base
        .runtime_dir()
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set"))?;
    Ok(runtime.join("aether").join("server.json"))
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

pub fn pid_is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
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
        format!("renaming project config {} -> {}", from.display(), to.display())
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
