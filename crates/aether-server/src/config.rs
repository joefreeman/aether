//! Project configuration and runtime discovery files.
//!
//! - Durable config: `$XDG_CONFIG_HOME/aether/projects/<name>.toml`
//! - Runtime info:   `$XDG_RUNTIME_DIR/aether/server.json` (one file per running server, not per
//!   project — a single server now hosts many projects, picked per-client via `project/activate`).
//!   `$XDG_RUNTIME_DIR` only exists on Linux/BSD; on macOS (and anywhere it's unset) we fall back
//!   to the user cache dir (`~/Library/Caches/aether/` on macOS), which is the right home for
//!   per-machine-session bookkeeping that needn't survive a reboot.

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The default profile's port. Named profiles get an allocated port (see [`PORT_BAND`]); the
/// default profile keeps the well-known port for back-compat. The canonical constant lives in
/// `aether_protocol` (the clients reference it too).
pub use aether_protocol::SERVER_PORT;

/// The name of the implicit profile used when none is selected.
pub const DEFAULT_PROFILE: &str = "default";

/// Band that named profiles allocate their (recorded, reused) port from. Deliberately *below* the
/// OS ephemeral range so a recorded port doesn't clash with transient outbound sockets later — see
/// `docs/profiles.md`.
const PORT_BAND: std::ops::RangeInclusive<u16> = 2385..=2484;

/// How long an auto-started server stays up with no clients before idle-reaping, unless the profile
/// overrides it via `idle_timeout_secs`. Long enough to ride out swapping one client for another;
/// a `dev` profile typically sets something much shorter. `ae server` (explicit) ignores this and
/// runs persistently.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

/// The active profile for this process, set once at startup from `--profile`/`AETHER_PROFILE`. One
/// process serves exactly one profile (the singleton is per-profile), so a process-global is the
/// right shape — every path helper reads it rather than threading a parameter through every caller.
static ACTIVE_PROFILE: OnceLock<String> = OnceLock::new();

/// Select the active profile for this process. Call once, early in `main`, before any path helper.
/// Subsequent calls are ignored (a process never switches profiles).
pub fn set_active_profile(name: String) {
    let _ = ACTIVE_PROFILE.set(name);
}

/// The active profile name (`"default"` until [`set_active_profile`] runs).
pub fn active_profile() -> &'static str {
    ACTIVE_PROFILE.get().map(String::as_str).unwrap_or(DEFAULT_PROFILE)
}

/// `<config>/aether` — the root holding `profiles/` (and, pre-migration, the legacy layout).
fn aether_config_dir() -> anyhow::Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| anyhow!("could not determine XDG base directories"))?;
    Ok(base.config_dir().join("aether"))
}

/// `<config>/aether/profiles` — the parent of every profile's subtree.
pub fn profiles_dir() -> anyhow::Result<PathBuf> {
    Ok(aether_config_dir()?.join("profiles"))
}

/// The active profile's config subtree: `<config>/aether/profiles/<name>/`. Everything durable
/// (settings, sessions, project configs, `profile.toml`) lives under here.
fn profile_config_dir() -> anyhow::Result<PathBuf> {
    Ok(profiles_dir()?.join(active_profile()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileConfig {
    port: u16,
    /// Override for the auto-start idle timeout (seconds). Hand-editable; absent ⇒
    /// [`DEFAULT_IDLE_TIMEOUT_SECS`]. A `dev` profile sets this short so a stopped client's
    /// auto-started server reaps quickly. Does not affect an explicit `ae server` (always persistent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    idle_timeout_secs: Option<u64>,
}

fn profile_config_path() -> anyhow::Result<PathBuf> {
    Ok(profile_config_dir()?.join("profile.toml"))
}

/// Resolve the active profile's port, creating the profile (allocating + recording a port) on first
/// use. The `default` profile is pinned to [`SERVER_PORT`]; named profiles get the lowest free port
/// in [`PORT_BAND`], recorded once and reused forever (a stable URL for the web client). The
/// `create_new` write guards the first-use race between a client and the server it spawns — the
/// loser just reads the winner's recorded port.
pub fn ensure_profile_port() -> anyhow::Result<u16> {
    let path = profile_config_path()?;
    if let Some(cfg) = read_profile_config(&path)? {
        return Ok(cfg.port);
    }
    let port = if active_profile() == DEFAULT_PROFILE {
        SERVER_PORT
    } else {
        allocate_port()?
    };
    std::fs::create_dir_all(profile_config_dir()?)?;
    let body = toml::to_string(&ProfileConfig {
        port,
        idle_timeout_secs: None,
    })?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(body.as_bytes())?;
            Ok(port)
        }
        // Lost the creation race — adopt whatever the winner recorded.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_profile_config(&path)?
            .map(|c| c.port)
            .ok_or_else(|| anyhow!("profile config vanished after creation race")),
        Err(e) => Err(e).context("writing profile.toml"),
    }
}

/// The active profile's auto-start idle timeout, in seconds: its `idle_timeout_secs` if set, else
/// [`DEFAULT_IDLE_TIMEOUT_SECS`]. The client's auto-start passes this to the spawned server.
pub fn profile_idle_timeout_secs() -> anyhow::Result<u64> {
    Ok(read_profile_config(&profile_config_path()?)?
        .and_then(|c| c.idle_timeout_secs)
        .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS))
}

fn read_profile_config(path: &Path) -> anyhow::Result<Option<ProfileConfig>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(toml::from_str(&s).with_context(|| {
            format!("parsing profile config at {}", path.display())
        })?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Lowest free port in [`PORT_BAND`] not already recorded by another profile. "Free" is checked by a
/// throwaway bind — advisory only (the port can be taken later), which the server's bind handles by
/// failing loudly. Skipping already-recorded ports keeps two new profiles from grabbing the same one.
fn allocate_port() -> anyhow::Result<u16> {
    let taken = recorded_ports()?;
    for port in PORT_BAND {
        if taken.contains(&port) {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    bail!(
        "no free port available in {}..={} for a new profile",
        PORT_BAND.start(),
        PORT_BAND.end()
    );
}

/// Ports recorded by all existing profiles (for allocation and `profiles list`).
fn recorded_ports() -> anyhow::Result<std::collections::HashSet<u16>> {
    Ok(list_profiles()?.into_iter().map(|p| p.port).collect())
}

/// A profile and its recorded port, for `ae profiles list`.
#[derive(Debug, Clone)]
pub struct ProfileEntry {
    pub name: String,
    pub port: u16,
}

/// Enumerate existing profiles (a `profiles/<name>/profile.toml` each), sorted by name. Empty when
/// nothing's been created yet.
pub fn list_profiles() -> anyhow::Result<Vec<ProfileEntry>> {
    list_profiles_at(&profiles_dir()?)
}

fn list_profiles_at(dir: &Path) -> anyhow::Result<Vec<ProfileEntry>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut out: Vec<ProfileEntry> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(cfg) = read_profile_config(&entry.path().join("profile.toml"))? {
            out.push(ProfileEntry {
                name,
                port: cfg.port,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

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
    Ok(profile_config_dir()?
        .join("projects")
        .join(format!("{name}.toml")))
}

/// Path to the active profile's application-settings file
/// (`…/profiles/<profile>/settings.toml`). One file per profile, independent of which project is
/// active within it — see `aether_protocol::settings`.
pub fn app_settings_path() -> anyhow::Result<PathBuf> {
    Ok(profile_config_dir()?.join("settings.toml"))
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

/// Current wall-clock time in Unix milliseconds. Used to stamp project-session activation times.
/// Saturates to 0 if the clock is somehow before the epoch.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Machine-managed (not user-authored) state for one project, persisted across server restarts so
/// the project switcher can sort by recency and a re-activated project can restore the buffers that
/// were open. Distinct from [`ProjectConfig`] (the user's `paths`) — this never holds anything the
/// user types, which is why it lives in a JSON file rather than the project's TOML.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSession {
    /// Wall-clock time (Unix ms) this project was last activated. Drives the switcher's
    /// most-recent-first ordering. `0` for a project that's only ever had its buffer list written.
    #[serde(default)]
    pub last_activated_at: u64,
    /// Canonical paths of the file-backed buffers that were open in this project, most-recently-used
    /// first. On re-activation they're restored as *dormant* buffers (listed in the picker, loaded
    /// lazily). Scratch buffers have no path and are omitted — persisting their unsaved contents is
    /// future work.
    #[serde(default)]
    pub buffers: Vec<PathBuf>,
}

/// The whole session file: every named project's [`ProjectSession`], keyed by project name. A
/// `BTreeMap` keeps the on-disk JSON deterministically ordered. Ephemeral projects (no `<name>.toml`)
/// are never recorded here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSessions {
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectSession>,
}

/// Path to the single machine-managed session file
/// (`$XDG_CONFIG_HOME/aether/sessions.json`). One file for all projects so the switcher can read
/// every recency stamp in one go. JSON (not TOML) signals "machine-managed, don't hand-edit".
pub fn project_sessions_path() -> anyhow::Result<PathBuf> {
    Ok(profile_config_dir()?.join("sessions.json"))
}

/// Load the project-session file. A missing file is not an error — a fresh install has none, so we
/// return an empty map. Path-parameterized so it can be unit-tested against a tempdir and pointed
/// at a throwaway path in server tests (see [`crate::state::ServerState::sessions_path`]).
pub fn load_project_sessions_at(path: &Path) -> anyhow::Result<ProjectSessions> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ProjectSessions::default()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading project sessions at {}", path.display()))
        }
    };
    serde_json::from_str(&content)
        .with_context(|| format!("parsing project sessions at {}", path.display()))
}

/// Write (or overwrite) the project-session file, creating the config directory if needed.
pub fn write_project_sessions_at(path: &Path, sessions: &ProjectSessions) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(sessions).context("serializing project sessions")?;
    std::fs::write(path, body)
        .with_context(|| format!("writing project sessions at {}", path.display()))?;
    Ok(())
}

/// Remove a project's recorded session, if present. Best-effort read-modify-write of the session
/// file. Called when a project is deleted so its session doesn't linger as an orphan.
pub fn remove_project_session_at(path: &Path, name: &str) -> anyhow::Result<()> {
    let mut sessions = load_project_sessions_at(path)?;
    if sessions.projects.remove(name).is_some() {
        write_project_sessions_at(path, &sessions)?;
    }
    Ok(())
}

/// Move a project's recorded session from `old` to `new`, if present. Called when a project is
/// renamed so its restored buffers and recency stamp follow the new name.
pub fn rename_project_session_at(path: &Path, old: &str, new: &str) -> anyhow::Result<()> {
    let mut sessions = load_project_sessions_at(path)?;
    if let Some(sess) = sessions.projects.remove(old) {
        sessions.projects.insert(new.to_string(), sess);
        write_project_sessions_at(path, &sessions)?;
    }
    Ok(())
}

/// Reorder an alphabetically-sorted project-name list into most-recently-activated-first, using the
/// session file's `last_activated_at` stamps. Projects with no recorded session (never activated, or
/// only ever had buffers written) sort as `0` and stay at the end — and because the sort is stable
/// over an already-alphabetical input, ties (including all the never-activated ones) keep their
/// alphabetical order. Pure so it can be unit-tested without disk.
pub fn sort_names_by_recency(names: &mut [String], sessions: &ProjectSessions) {
    names.sort_by_key(|name| {
        // Negate to get descending order from an ascending stable sort key.
        std::cmp::Reverse(
            sessions
                .projects
                .get(name)
                .map(|s| s.last_activated_at)
                .unwrap_or(0),
        )
    });
}

/// Directory containing the per-project `.toml` configs. Used by `list_project_names`.
pub fn projects_dir() -> anyhow::Result<PathBuf> {
    Ok(profile_config_dir()?.join("projects"))
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
    // Per-profile: each profile's server is its own singleton, keyed by its own pid file.
    Ok(dir
        .join("aether")
        .join(active_profile())
        .join("server.json"))
}

/// Write the running server's pid to its runtime file (0600). The file is the per-profile singleton
/// marker: its presence with a live pid means a server already owns this profile. It holds only the
/// pid — the port now lives in the profile config, and restart detection (`server_started_at`)
/// flows from `ServerState` over the wire, not from here. If a start time is ever needed, the
/// file's own mtime stands in.
pub fn write_runtime_pid(path: &Path, pid: u32) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating runtime dir {}", parent.display()))?;
    }
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating runtime file {}", path.display()))?;
    file.write_all(pid.to_string().as_bytes())?;
    Ok(())
}

pub fn read_runtime_pid(path: &Path) -> anyhow::Result<u32> {
    let content = std::fs::read_to_string(path)?;
    content
        .trim()
        .parse::<u32>()
        .with_context(|| format!("parsing pid from runtime file {}", path.display()))
}

/// The pid of the running server for the active profile, or `None` when none is running — no pid
/// file, or it names a dead process. Drives `ae server stop`.
pub fn running_server_pid() -> anyhow::Result<Option<u32>> {
    match read_runtime_pid(&runtime_info_path()?) {
        Ok(pid) if pid_is_alive(pid) => Ok(Some(pid)),
        _ => Ok(None),
    }
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
        let projects = [
            proj("work", &["/home/joe/work"]),
            proj("dots", &["/home/joe/.config"]),
        ];
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
        // path resolves and lands under our per-profile `aether/<profile>/server.json` regardless of
        // which base was used. (`active_profile()` is `default` unless a test set it.)
        let path = runtime_info_path().expect("runtime_info_path should never fail on a fallback");
        assert!(
            path.ends_with("server.json") && path.to_string_lossy().contains("/aether/"),
            "unexpected path: {}",
            path.display()
        );
    }

    #[test]
    fn list_profiles_reads_recorded_ports_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = dir.path().join("profiles");
        for (name, port) in [("dev", 2385u16), ("default", SERVER_PORT)] {
            let p = profiles.join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(
                p.join("profile.toml"),
                toml::to_string(&ProfileConfig {
                    port,
                    idle_timeout_secs: None,
                })
                .unwrap(),
            )
            .unwrap();
        }
        // A directory with no profile.toml is ignored.
        std::fs::create_dir_all(profiles.join("garbage")).unwrap();

        let got = list_profiles_at(&profiles).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].name.as_str(), got[0].port), ("default", SERVER_PORT));
        assert_eq!((got[1].name.as_str(), got[1].port), ("dev", 2385));
    }

    #[test]
    fn profile_idle_timeout_parses_overrides_and_omits_when_unset() {
        let dir = tempfile::tempdir().unwrap();
        // No override → None (the caller falls back to DEFAULT_IDLE_TIMEOUT_SECS).
        let bare = dir.path().join("bare.toml");
        std::fs::write(&bare, "port = 2384\n").unwrap();
        assert_eq!(
            read_profile_config(&bare).unwrap().unwrap().idle_timeout_secs,
            None
        );
        // Hand-edited override is honoured.
        let custom = dir.path().join("custom.toml");
        std::fs::write(&custom, "port = 2385\nidle_timeout_secs = 15\n").unwrap();
        assert_eq!(
            read_profile_config(&custom).unwrap().unwrap().idle_timeout_secs,
            Some(15)
        );
        // Writing with None keeps the file minimal (no empty/null key).
        let body = toml::to_string(&ProfileConfig {
            port: 2384,
            idle_timeout_secs: None,
        })
        .unwrap();
        assert!(
            !body.contains("idle_timeout_secs"),
            "None should be omitted, got: {body}"
        );
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
        write_app_settings_at(
            &path,
            &AppSettings {
                wrap: WrapMode::None,
                ligatures: false,
                font_size: 18,
            },
        )
        .unwrap();
        let s = load_app_settings_at(&path).unwrap();
        assert_eq!(s.wrap, WrapMode::None);
        assert!(!s.ligatures);
        assert_eq!(s.font_size, 18);
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
    fn project_sessions_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        // No file yet → empty map, not an error.
        assert_eq!(
            load_project_sessions_at(&path).unwrap(),
            ProjectSessions::default()
        );
    }

    #[test]
    fn project_sessions_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        // Nested path exercises the create-parent branch.
        let path = dir.path().join("aether").join("sessions.json");
        let mut sessions = ProjectSessions::default();
        sessions.projects.insert(
            "work".into(),
            ProjectSession {
                last_activated_at: 1000,
                buffers: vec![PathBuf::from("/work/a.rs"), PathBuf::from("/work/b.rs")],
            },
        );
        sessions.projects.insert(
            "dots".into(),
            ProjectSession {
                last_activated_at: 2000,
                buffers: vec![],
            },
        );
        write_project_sessions_at(&path, &sessions).unwrap();
        assert_eq!(load_project_sessions_at(&path).unwrap(), sessions);
    }

    #[test]
    fn recency_sort_orders_most_recent_first_then_alphabetical() {
        let mut sessions = ProjectSessions::default();
        sessions.projects.insert(
            "beta".into(),
            ProjectSession {
                last_activated_at: 100,
                buffers: vec![],
            },
        );
        sessions.projects.insert(
            "alpha".into(),
            ProjectSession {
                last_activated_at: 200,
                buffers: vec![],
            },
        );
        // `gamma` and `delta` have no recorded session → stamp 0 → they sit at the end, keeping the
        // alphabetical order they came in with (the input is the alphabetical disk listing).
        let mut names = vec![
            "alpha".to_string(),
            "beta".to_string(),
            "delta".to_string(),
            "gamma".to_string(),
        ];
        sort_names_by_recency(&mut names, &sessions);
        assert_eq!(names, vec!["alpha", "beta", "delta", "gamma"]);

        // Flip the stamps: `beta` is now the most recent.
        sessions.projects.get_mut("beta").unwrap().last_activated_at = 999;
        sort_names_by_recency(&mut names, &sessions);
        assert_eq!(names, vec!["beta", "alpha", "delta", "gamma"]);
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
