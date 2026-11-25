//! Omni's settings (`/api/settings*`, LAN only; changes need a trusted
//! browser; see "Settings" in `docs/architecture.md`).
//!
//! - `GET /api/settings`: [`SettingsView`];
//! - `PUT /api/settings/:key` ([`SettingUpdate`]) -> [`SettingView`];
//! - `DELETE /api/settings/:key`: back to the environment value or default;
//! - `POST /api/settings/restart`: restarts Omni so saved values apply.
//!
//! Secret values are write-only: views say whether one is set, never what it
//! is. Other values are shown only to a trusted browser.

use serde::{Deserialize, Serialize};

use crate::common::Ms;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    pub settings: Vec<SettingView>,
    /// A saved value is not in effect yet.
    pub restart_needed: bool,
    /// Values were left out: the browser is not trusted.
    pub values_hidden: bool,
    /// When this Omni process started; it changes once a restart is done.
    pub booted_at: Ms,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettingSource {
    /// Saved in Omni.
    #[serde(rename = "stored")]
    Stored,
    /// From the container environment (the fallback until it is stored).
    #[serde(rename = "environment")]
    Environment,
    /// Not set anywhere: the built-in default applies.
    #[serde(rename = "default")]
    Default,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingView {
    /// The environment variable name (`PLEX_URL`).
    pub key: String,
    /// The section it is listed under (`Media`, `Email`, ...).
    pub group: String,
    pub secret: bool,
    /// `false` for what Omni needs before it can read its settings
    /// (`DB_NAME`, `FRONTEND_PORT`, `DOCKERIZED`) and paths the image sets
    /// (`OMNI_RECIPES_WEB_DIST`): environment only.
    pub editable: bool,
    pub source: SettingSource,
    pub set: bool,
    /// The value, for a non-secret setting and a trusted browser.
    pub value: Option<String>,
    /// The container environment sets it (removing a stored value falls back
    /// to that; otherwise to the default).
    pub environment_set: bool,
    /// The environment also sets it, to something else (the stored value wins).
    pub environment_differs: bool,
    /// The value in effect differs from the saved one until Omni restarts.
    pub restart_needed: bool,
    pub updated_at: Option<Ms>,
}

/// `PUT /api/settings/:key`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingUpdate {
    pub value: String,
}
