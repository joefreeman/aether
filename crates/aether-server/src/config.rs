//! Project configuration and runtime discovery files.
//!
//! - Durable config: `$XDG_CONFIG_HOME/aether/projects/<name>.toml`
//! - Runtime info:   `$XDG_RUNTIME_DIR/aether/<name>.json` (created on startup, removed on shutdown)

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
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
    let config: ProjectConfig = toml::from_str(&content)
        .with_context(|| format!("parsing project config at {}", path.display()))?;
    if config.name != name {
        bail!(
            "project name mismatch in {}: file says {:?}, expected {:?}",
            path.display(),
            config.name,
            name
        );
    }
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

pub fn runtime_info_path(name: &str) -> anyhow::Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("could not determine XDG base directories"))?;
    let runtime = base
        .runtime_dir()
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set"))?;
    Ok(runtime.join("aether").join(format!("{name}.json")))
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
