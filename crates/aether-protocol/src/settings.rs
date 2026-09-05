//! Application-level settings — global, not per-workspace. Persisted server-side at
//! `$XDG_CONFIG_HOME/aether/settings.toml`. Distinct from workspace settings (a workspace's name and
//! roots): these are app-wide preferences that apply regardless of the active workspace.
//!
//! The client fetches them at boot (`settings/get`) and writes them from the app-settings overlay
//! (`Space ,`) with `settings/set`. Kept deliberately small — this is a personal editor, so a
//! setting earns its place by being something worth toggling, not configuring.

use crate::envelope::{NotificationMethod, RpcMethod};
use crate::viewport::WrapMode;
use serde::{Deserialize, Serialize};

/// The full set of application settings. Every field has a serde default so an older (or empty)
/// `settings.toml` round-trips forward as new settings are added — a missing key reads as its
/// default rather than failing the parse.
/// Not `Copy`: [`Self::worktree_store`] is a path. The derive was there because every field
/// happened to be a scalar, not because anything needed it — and letting that accident dictate what
/// a setting is allowed to *be* is the tail wagging the dog. Clones are per-RPC, not per-frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    /// Soft-wrap mode applied to viewports. The client seeds `Session.wrap` from this at boot and
    /// the app-settings overlay toggles it.
    #[serde(default = "default_wrap")]
    pub wrap: WrapMode,
    /// Coding ligatures in the editor font (the bundled JetBrains Mono). Purely a client-side render
    /// choice — the native client toggles its shaping, the web client toggles the `calt`/`liga`
    /// font features. The server stores it but doesn't act on it.
    #[serde(default = "default_ligatures")]
    pub ligatures: bool,
    /// Editor text size in px — the file content itself. Another client-side render choice the
    /// server only stores: the GUI/web clients render the text at this size (and reflow soft-wrap
    /// to the new width); the terminal client ignores it (the terminal owns its font). The overlay
    /// steps it through a small set of preset sizes.
    ///
    /// The alias is what a `settings.toml` written before the setting was renamed still keys this
    /// by, so an existing config keeps loading rather than silently reverting to the default.
    #[serde(default = "default_editor_font_size", alias = "buffer_font_size")]
    pub editor_font_size: u32,
    /// UI text size in px — everything *around* the buffer (status bar, pickers, dialogs, hover,
    /// toasts, hints). Sized independently of [`Self::editor_font_size`] so the chrome can stay
    /// compact while the code is large, or vice versa. Same story otherwise: client-side render
    /// only, GUI/web honour it, the terminal ignores it.
    #[serde(default = "default_ui_font_size")]
    pub ui_font_size: u32,
    /// Hints: the passive corner suggestion that walks through the curriculum. The off-switch only —
    /// learning state lives in `hints.json`, not here.
    #[serde(default = "default_hints")]
    pub hints: bool,
    /// Open markdown buffers as the reading view by default: file-shaped opens land in Read mode,
    /// jump-shaped opens (grep hits, diagnostics) land in the editor either way. Client-side
    /// behaviour the server only stores.
    #[serde(default = "default_markdown_read")]
    pub markdown_read: bool,
    /// How wide the reading view's text column runs. Client-side render only, like the rest of the
    /// view settings: each shell resolves the mode to its own unit (terminal columns, ems) at draw
    /// time. Applies to the reading view alone — the editor's width is the viewport's.
    #[serde(default = "default_markdown_width")]
    pub markdown_width: MarkdownWidth,
    /// Colour theme, app-wide across every client. Purely a client-side render choice the server
    /// only stores: shells resolve the mode to a role→shade table (the client core's `theme`
    /// module) at draw time.
    #[serde(default = "default_theme")]
    pub theme: ThemeMode,
    /// Periodically `git fetch` the workspaces' repos, so the status bar's ahead/behind counts
    /// stay current instead of reporting whenever the user last fetched by hand.
    ///
    /// Unlike every other setting here this one the *server* acts on — it's the only behaviour in
    /// the app that reaches the network without being asked, which is exactly why it needs an
    /// off-switch and why it defaults to **off**. A metered connection, a VPN-gated remote or an
    /// SSH key with a passphrase all make an unattended fetch a nuisance rather than a
    /// convenience, and there's no "just don't press the key" escape from a timer.
    ///
    /// A toggle rather than an interval: the cadence
    /// ([`crate::git::AUTO_FETCH_INTERVAL_MINUTES`]-worth, fixed in the server) is a judgement
    /// call with a good answer, and CLAUDE.md would rather that answer live in code than in
    /// config surface.
    #[serde(default = "default_git_auto_fetch")]
    pub git_auto_fetch: bool,
    /// Where app-managed git worktrees are created. Absolute path; empty means the default,
    /// `$XDG_DATA_HOME/aether/worktrees`.
    ///
    /// **The store is centralised and outside every workspace root on purpose** — a worktree nested
    /// under a root gets swallowed by the workspace index, shows up as an untracked path in status,
    /// and duplicates every search hit. It also sits outside the profile *state* subtree, which is
    /// documented as sweepable: worktrees hold uncommitted work.
    ///
    /// Worth a setting rather than an environment variable, despite being set once and rarely: this
    /// app is launched from a desktop entry as often as from a shell (`ae --gui %f`), and a variable
    /// exported in a shell rc simply never reaches that process. A setting reaches every launcher.
    ///
    /// **No row in the settings overlay.** The overlay is a keyboard-stepped list of toggles and
    /// preset sizes; a filesystem path is neither, and inventing a text-entry control for one
    /// rarely-touched key would be a lot of surface for it. Hand-edit `settings.toml`, or set it
    /// with `settings/set`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worktree_store: String,
}

fn default_wrap() -> WrapMode {
    WrapMode::Soft
}

fn default_ligatures() -> bool {
    true
}

pub const fn default_editor_font_size() -> u32 {
    14
}

/// A notch below the editor default: the chrome is dense (status bar, picker rows) and reads as
/// secondary to the text, which is what the hand-tuned sizes it replaced already assumed.
pub const fn default_ui_font_size() -> u32 {
    13
}

fn default_hints() -> bool {
    true
}

fn default_markdown_read() -> bool {
    true
}

/// The reading view's text-column width. Not a number of columns or pixels: the two units the
/// shells draw in (character cells, ems of the reading size) can't share one figure, so the wire
/// carries the *choice* and each shell resolves it — the client core's `read_layout` holds the
/// table both sides read from, so the terminal and the pixel shells stay in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkdownWidth {
    /// The classic reading measure — long-established as the comfortable one for prose.
    Narrow,
    /// Roughly a third wider. Prose still reads well and wide tables/code fences stop scrolling.
    Wide,
    /// No column at all: the document fills the window.
    Full,
}

/// Narrow — the measure the reading view has always used, and the one prose is easiest to read at.
pub const fn default_markdown_width() -> MarkdownWidth {
    MarkdownWidth::Narrow
}

/// Which colour theme the clients render with. One value for the whole app, like every other app
/// setting — there is deliberately no per-client override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Dark,
    Light,
}

pub const fn default_theme() -> ThemeMode {
    ThemeMode::Dark
}

/// Off. Unattended network access is opt-in — see [`AppSettings::git_auto_fetch`].
fn default_git_auto_fetch() -> bool {
    false
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            wrap: default_wrap(),
            ligatures: default_ligatures(),
            editor_font_size: default_editor_font_size(),
            ui_font_size: default_ui_font_size(),
            hints: default_hints(),
            markdown_read: default_markdown_read(),
            markdown_width: default_markdown_width(),
            theme: default_theme(),
            git_auto_fetch: default_git_auto_fetch(),
            worktree_store: String::new(),
        }
    }
}

/// Read the current application settings. Returns defaults when no `settings.toml` exists yet.
pub struct SettingsGet;
impl RpcMethod for SettingsGet {
    const NAME: &'static str = "settings/get";
    type Params = SettingsGetParams;
    type Result = AppSettings;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SettingsGetParams {}

/// Replace the application settings and persist them to disk. Returns the settings as stored
/// (echoing back the new state, like the workspace RPCs return `WorkspaceInfo`). The server also pushes
/// [`SettingsChanged`] to every *other* connected client so the change applies live everywhere.
pub struct SettingsSet;
impl RpcMethod for SettingsSet {
    const NAME: &'static str = "settings/set";
    type Params = AppSettings;
    type Result = AppSettings;
}

/// Pushed to every connected client *except* the one that just set them, carrying the new
/// application settings. Settings are global (app-wide, not per-workspace), so this goes to all
/// clients regardless of their active workspace. The setter learns the new state from its
/// `settings/set` result instead.
pub struct SettingsChanged;
impl NotificationMethod for SettingsChanged {
    const NAME: &'static str = "settings/changed";
    type Params = AppSettings;
}
