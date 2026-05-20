//! Read the runtime info file the server writes on startup.

use anyhow::{anyhow, Context};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct RuntimeInfo {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub started_at_unix_ms: u64,
}

pub fn read(project_name: &str) -> anyhow::Result<RuntimeInfo> {
    let path = runtime_path(project_name)?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading runtime info at {}", path.display()))?;
    Ok(serde_json::from_str(&content)?)
}

fn runtime_path(project_name: &str) -> anyhow::Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("could not determine XDG base directories"))?;
    let runtime = base
        .runtime_dir()
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set"))?;
    Ok(runtime.join("aether").join(format!("{project_name}.json")))
}
