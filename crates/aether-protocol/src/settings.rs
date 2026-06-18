//! Application-level settings — global, not per-project. Persisted server-side at
//! `$XDG_CONFIG_HOME/aether/settings.toml`. Distinct from project settings (a project's name and
//! roots): these are app-wide preferences that apply regardless of the active project.
//!
//! The client fetches them at boot (`settings/get`) and writes them from the app-settings overlay
//! (`Space .`) with `settings/set`. Kept deliberately small — this is a personal editor, so a
//! setting earns its place by being something worth toggling, not configuring.

use crate::envelope::{NotificationMethod, RpcMethod};
use crate::viewport::WrapMode;
use serde::{Deserialize, Serialize};

/// The full set of application settings. Every field has a serde default so an older (or empty)
/// `settings.toml` round-trips forward as new settings are added — a missing key reads as its
/// default rather than failing the parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    /// Soft-wrap mode applied to viewports. The client seeds `Session.wrap` from this at boot and
    /// the app-settings overlay toggles it.
    #[serde(default = "default_wrap")]
    pub wrap: WrapMode,
}

fn default_wrap() -> WrapMode {
    WrapMode::Soft
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            wrap: default_wrap(),
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
/// (echoing back the new state, like the project RPCs return `ProjectInfo`). The server also pushes
/// [`SettingsChanged`] to every *other* connected client so the change applies live everywhere.
pub struct SettingsSet;
impl RpcMethod for SettingsSet {
    const NAME: &'static str = "settings/set";
    type Params = AppSettings;
    type Result = AppSettings;
}

/// Pushed to every connected client *except* the one that just set them, carrying the new
/// application settings. Settings are global (app-wide, not per-project), so this goes to all
/// clients regardless of their active project. The setter learns the new state from its
/// `settings/set` result instead.
pub struct SettingsChanged;
impl NotificationMethod for SettingsChanged {
    const NAME: &'static str = "settings/changed";
    type Params = AppSettings;
}
