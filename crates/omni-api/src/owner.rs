//! Trusted browsers (`/api/owner`): the owner's browsers that may change
//! Omni's administration (gateway tokens and integrations, settings, data
//! deletion). Everything else in the UI needs no login.

use serde::{Deserialize, Serialize};

use crate::common::Ms;

/// The `code` of the 403 an untrusted browser gets from an admin change.
pub const UNTRUSTED_BROWSER: &str = "untrusted_browser";

/// `{"error": "...", "code": "untrusted_browser"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UntrustedBody {
    pub error: String,
    pub code: String,
}

/// `GET /api/owner`: whether this browser is trusted; the list of trusted
/// browsers only for a trusted one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerStatus {
    pub trusted: bool,
    pub browsers: Vec<TrustedBrowserView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedBrowserView {
    pub id: String,
    pub label: String,
    pub created_at: Ms,
    pub last_seen_at: Option<Ms>,
    /// The browser asking.
    pub current: bool,
}

/// `POST /api/owner/pairings`: a request to trust this browser. The owner's
/// phone gets a push; the approval page there asks for `code`. `claim` stays
/// in this page and fetches the cookie once approved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingStarted {
    pub id: String,
    pub code: String,
    pub claim: String,
    pub expires_at: Ms,
}

/// `POST /api/owner/pairings/{id}/claim`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingClaim {
    pub claim: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairingPhase {
    /// Waiting for the owner.
    #[serde(rename = "pending")]
    Pending,
    /// This response set the cookie.
    #[serde(rename = "trusted")]
    Trusted,
    #[serde(rename = "denied")]
    Denied,
    /// Expired, already claimed, or unknown.
    #[serde(rename = "expired")]
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingStatus {
    pub state: PairingPhase,
}
