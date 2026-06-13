//! Read the runtime info file the server writes on startup — same discovery as the TUI
//! (`$XDG_RUNTIME_DIR/aether/server.json`); one server, multi-project, the client activates a
//! project after connecting.

use anyhow::{anyhow, Context};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct RuntimeInfo {
    pub pid: u32,
    pub port: u16,
    pub started_at_unix_ms: u64,
}

pub fn read() -> anyhow::Result<RuntimeInfo> {
    let path = runtime_path()?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading runtime info at {}", path.display()))?;
    Ok(serde_json::from_str(&content)?)
}

fn runtime_path() -> anyhow::Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("could not determine XDG base directories"))?;
    let runtime = base
        .runtime_dir()
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set"))?;
    Ok(runtime.join("aether").join("server.json"))
}
