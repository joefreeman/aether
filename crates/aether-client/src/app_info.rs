//! The application-info dialog (`Space ?`): turning an [`AppInfo`] snapshot into rows to render.
//!
//! The core owns the *composition* — which rows exist, their order, their wording — so all three
//! shells show the same dialog and formatting rules (uptime, the version-drift row, "N open, M
//! unsaved") live in exactly one place. Each shell contributes only its own look: a box, a label
//! column, a value column. Same division as the LSP detail dialog it sits beside.
//!
//! The snapshot describes the **server**. On native shells that's also the client — one binary, and
//! the handshake's exact-match version gate refuses a mismatch outright — but the web client's
//! cached bundle can lag behind the daemon serving it. So rather than assume, [`sections`] compares
//! the payload against the client's own compiled-in build constants and emits a `Client` row only
//! when they actually differ.

use aether_protocol::app::AppInfo;

/// A labelled group of rows. Purely presentational: shells draw the title as a heading and the rows
/// beneath it. A section with no rows is never emitted, so a shell can render unconditionally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoSection {
    pub title: &'static str,
    pub rows: Vec<InfoRow>,
}

/// One `label: value` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoRow {
    pub label: &'static str,
    pub value: String,
    pub tone: InfoTone,
}

/// How prominently to render a row's value. Deliberately two-valued: this is a *diagnostic* screen,
/// so almost everything is a plain fact, and reserving the warning tone for the one row that means
/// "something is actually wrong" keeps it meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoTone {
    Normal,
    /// The client and server are different builds — the one condition here that a user can act on.
    Warn,
}

impl InfoRow {
    fn new(label: &'static str, value: impl Into<String>) -> Self {
        InfoRow {
            label,
            value: value.into(),
            tone: InfoTone::Normal,
        }
    }

    fn warn(label: &'static str, value: impl Into<String>) -> Self {
        InfoRow {
            label,
            value: value.into(),
            tone: InfoTone::Warn,
        }
    }
}

/// Compose the dialog's sections from a snapshot. Pure — no clock, no environment — so the shells
/// and the tests see identical output for identical input. Anything time-derived (uptime) is
/// computed server-side and arrives in the payload; the sans-IO core has no clock of its own.
pub fn sections(info: &AppInfo) -> Vec<InfoSection> {
    let mut out = Vec::new();

    // ---- Build: which binary is this? ----
    let mut build = vec![InfoRow::new("Version", info.version.clone())];
    build.push(InfoRow::new("Build", build_line(info)));
    if let Some(row) = client_drift_row(info) {
        build.push(row);
    }
    if let Some(p) = &info.appimage {
        build.push(InfoRow::new("AppImage", p.clone()));
    }
    out.push(InfoSection {
        title: "Build",
        rows: build,
    });

    // ---- Instance: which daemon am I talking to? ----
    let mut instance = vec![InfoRow::new("Profile", info.profile.clone())];
    if let Some(port) = info.port {
        instance.push(InfoRow::new("Port", port.to_string()));
    }
    instance.push(InfoRow::new("PID", info.pid.to_string()));
    instance.push(InfoRow::new(
        "Uptime",
        format_duration_secs(info.uptime_secs),
    ));
    instance.push(InfoRow::new(
        "Mode",
        match info.idle_timeout_secs {
            // Worth spelling out: an auto-started server reaping is the usual explanation for
            // "my unsaved buffers vanished while I wasn't looking".
            Some(secs) => format!("auto-started, reaps after {} idle", short_secs(secs)),
            None => "persistent (`ae server`)".to_string(),
        },
    ));
    instance.push(InfoRow::new("Clients", info.clients.to_string()));
    instance.push(InfoRow::new(
        "Buffers",
        if info.buffers_unsaved > 0 {
            format!(
                "{} open, {} unsaved",
                info.buffers_open, info.buffers_unsaved
            )
        } else {
            format!("{} open", info.buffers_open)
        },
    ));
    instance.push(InfoRow::new(
        "Workspaces",
        format!("{} active", info.workspaces_active),
    ));
    out.push(InfoSection {
        title: "Instance",
        rows: instance,
    });

    // ---- Paths: where does this profile's state live? ----
    // Profile-scoped, so not guessable from the outside — and the answer to most "reset it" and
    // "why is it remembering that?" questions. A path that failed to resolve is omitted: the same
    // failure disables the feature server-side, so the gap is the finding.
    let paths: Vec<InfoRow> = [
        ("Config", &info.paths.config_dir),
        ("State", &info.paths.state_dir),
        ("Settings", &info.paths.settings),
        ("Sessions", &info.paths.sessions),
        ("Hints", &info.paths.hints),
        ("Backups", &info.paths.backups),
    ]
    .into_iter()
    .filter_map(|(label, p)| p.as_ref().map(|p| InfoRow::new(label, p.clone())))
    .collect();
    if !paths.is_empty() {
        out.push(InfoSection {
            title: "Paths",
            rows: paths,
        });
    }

    out
}

/// The build-identity line: commit, whether the tree was modified, and debug-vs-release. These
/// travel together because individually none of them identifies a binary — `0.2.0` is shared by
/// every build between two releases.
fn build_line(info: &AppInfo) -> String {
    let mut s = match &info.commit {
        Some(c) => c.clone(),
        // Not built from a checkout (tarball, no `git`). Say so rather than showing a blank.
        None => "unknown commit".to_string(),
    };
    if info.commit_dirty {
        s.push_str(" (modified)");
    }
    s.push_str(if info.debug_build {
        " · debug"
    } else {
        " · release"
    });
    s
}

/// A `Client` row naming *our* build, emitted only when it differs from the server's.
///
/// Normally impossible on a native shell: client and server ship in one binary and the handshake
/// rejects a version mismatch outright. The web client is the real case — its bundle is cached by
/// the browser and can outlive the daemon that served it — and it's exactly the situation where
/// nothing else in the dialog can be trusted, so it earns the warning tone.
fn client_drift_row(info: &AppInfo) -> Option<InfoRow> {
    let ours = aether_protocol::PROTOCOL_VERSION;
    let our_commit = aether_protocol::BUILD_COMMIT;
    if info.version == ours && info.commit.as_deref() == our_commit {
        return None;
    }
    let mut v = ours.to_string();
    if let Some(c) = our_commit {
        v.push_str(&format!(" ({c})"));
    }
    v.push_str(" — differs from the server; restart it to pick up this build");
    Some(InfoRow::warn("Client", v))
}

/// Render the whole snapshot as plain text, for `y` (copy). This is the paste-into-a-bug-report
/// payload, so it's built from the same [`sections`] the dialog draws — what you copy is what you
/// saw, and neither can gain a field the other misses.
pub fn to_plain_text(info: &AppInfo) -> String {
    let secs = sections(info);
    // Align values into a column, sized to the widest label across *all* sections so the copied
    // block reads as one table rather than three.
    let width = secs
        .iter()
        .flat_map(|s| s.rows.iter())
        .map(|r| r.label.len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (i, section) in secs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(section.title);
        out.push('\n');
        for row in &section.rows {
            out.push_str(&format!(
                "  {:<width$}  {}\n",
                row.label,
                row.value,
                width = width
            ));
        }
    }
    out
}

/// Render a whole-second duration as at most the two largest non-zero units (`3d 4h`, `5m 12s`).
/// Coarse on purpose: this is an "is it fresh or has it been up for days?" reading, not a timer.
pub fn format_duration_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// A single-unit duration for prose ("reaps after 5m idle"), where the two-unit form reads badly.
fn short_secs(secs: u64) -> String {
    if secs >= 3_600 && secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_protocol::app::AppPaths;

    fn info() -> AppInfo {
        AppInfo {
            version: aether_protocol::PROTOCOL_VERSION.to_string(),
            commit: aether_protocol::BUILD_COMMIT.map(str::to_string),
            commit_dirty: false,
            debug_build: false,
            appimage: None,
            profile: "default".into(),
            port: Some(2384),
            pid: 4242,
            started_at_unix_ms: 1_700_000_000_000,
            uptime_secs: 3 * 3600 + 12 * 60,
            idle_timeout_secs: None,
            clients: 2,
            buffers_open: 5,
            buffers_unsaved: 1,
            workspaces_active: 3,
            paths: AppPaths {
                config_dir: Some("/home/u/.config/aether/profiles/default".into()),
                ..Default::default()
            },
        }
    }

    fn value(secs: &[InfoSection], label: &str) -> Option<String> {
        secs.iter()
            .flat_map(|s| s.rows.iter())
            .find(|r| r.label == label)
            .map(|r| r.value.clone())
    }

    #[test]
    fn sections_cover_build_instance_and_paths() {
        let s = sections(&info());
        let titles: Vec<_> = s.iter().map(|s| s.title).collect();
        assert_eq!(titles, vec!["Build", "Instance", "Paths"]);
        assert_eq!(value(&s, "Profile").as_deref(), Some("default"));
        assert_eq!(value(&s, "Port").as_deref(), Some("2384"));
        assert_eq!(value(&s, "Uptime").as_deref(), Some("3h 12m"));
        assert_eq!(value(&s, "Buffers").as_deref(), Some("5 open, 1 unsaved"));
    }

    /// A path that didn't resolve is omitted rather than rendered blank, and a Paths section with
    /// nothing in it doesn't appear at all — so shells can render sections unconditionally.
    #[test]
    fn unresolved_paths_are_omitted() {
        let s = sections(&info());
        assert!(value(&s, "Config").is_some());
        assert!(value(&s, "Sessions").is_none());

        let mut bare = info();
        bare.paths = AppPaths::default();
        let s = sections(&bare);
        assert!(s.iter().all(|s| s.title != "Paths"));
        assert!(s.iter().all(|s| !s.rows.is_empty()));
    }

    /// The drift row is absent when the server's build matches ours — the normal case on every
    /// native shell, where client and server are the same binary.
    #[test]
    fn no_client_row_when_builds_match() {
        assert!(value(&sections(&info()), "Client").is_none());
    }

    #[test]
    fn client_row_appears_on_version_drift() {
        let mut drifted = info();
        drifted.version = "0.0.1-ancient".into();
        let s = sections(&drifted);
        let row = s
            .iter()
            .flat_map(|s| s.rows.iter())
            .find(|r| r.label == "Client")
            .expect("drift row");
        assert_eq!(row.tone, InfoTone::Warn);
        assert!(row.value.starts_with(aether_protocol::PROTOCOL_VERSION));
    }

    /// Same version, different commit (a rebuilt working tree) still counts as drift: between
    /// releases the version is constant while the code moves, so the commit is the real identity.
    #[test]
    fn client_row_appears_on_commit_drift() {
        let mut drifted = info();
        drifted.commit = Some("deadbee".into());
        assert!(value(&sections(&drifted), "Client").is_some());
    }

    #[test]
    fn build_line_reports_modified_and_debug() {
        let mut i = info();
        i.commit = Some("abc1234".into());
        i.commit_dirty = true;
        i.debug_build = true;
        assert_eq!(build_line(&i), "abc1234 (modified) · debug");

        i.commit = None;
        i.commit_dirty = false;
        i.debug_build = false;
        assert_eq!(build_line(&i), "unknown commit · release");
    }

    #[test]
    fn mode_names_the_reaper() {
        let mut i = info();
        assert_eq!(
            value(&sections(&i), "Mode").as_deref(),
            Some("persistent (`ae server`)")
        );
        i.idle_timeout_secs = Some(300);
        assert_eq!(
            value(&sections(&i), "Mode").as_deref(),
            Some("auto-started, reaps after 5m idle")
        );
    }

    /// The copy payload is derived from the rendered sections, so it can't omit a row the dialog
    /// shows (or vice versa).
    #[test]
    fn plain_text_contains_every_row() {
        let i = info();
        let text = to_plain_text(&i);
        for row in sections(&i).iter().flat_map(|s| s.rows.iter()) {
            assert!(
                text.contains(row.label) && text.contains(&row.value),
                "missing {} in:\n{text}",
                row.label
            );
        }
        assert!(text.starts_with("Build\n"));
    }

    #[test]
    fn durations_show_two_units() {
        assert_eq!(format_duration_secs(0), "0s");
        assert_eq!(format_duration_secs(42), "42s");
        assert_eq!(format_duration_secs(3 * 60 + 4), "3m 4s");
        assert_eq!(format_duration_secs(5 * 3600 + 6 * 60), "5h 6m");
        assert_eq!(format_duration_secs(2 * 86_400 + 3 * 3600), "2d 3h");
    }

    #[test]
    fn short_secs_prefers_one_unit() {
        assert_eq!(short_secs(45), "45s");
        assert_eq!(short_secs(300), "5m");
        assert_eq!(short_secs(7_200), "2h");
        assert_eq!(short_secs(90), "90s");
    }
}
