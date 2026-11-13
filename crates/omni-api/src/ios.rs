//! The Omni iOS app's signed API under `/api/ios-controls`: live control
//! slots, diagnostics, control, device and widget-push registrations, and the
//! On Air widget timeline.

use serde::{Deserialize, Serialize};

use crate::common::Ms;
use crate::streamers::StreamerTier;

/// Number of control slots the app can register.
pub const IOS_CONTROL_SLOT_COUNT: u8 = 4;

/// `"sandbox" | "production"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApnsEnvironment {
    Sandbox,
    Production,
}

/// The state one control slot shows (`LiveControlSlot`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSlotState {
    pub slot: u8,
    pub is_live: bool,
    pub streamer_id: Option<String>,
    pub display_name: String,
    pub title: Option<String>,
    pub platform: Option<String>,
    pub url: String,
    pub viewer_count: Option<i64>,
    pub started_at: Option<Ms>,
    pub updated_at: Ms,
}

/// `GET /api/ios-controls/diagnostics` (non-secret).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IosDiagnostics {
    pub apns_enabled: bool,
    pub registration_count: u64,
    pub undelivered_count: u64,
    pub last_reconciled_at: Option<Ms>,
    /// Registered app devices.
    #[serde(default)]
    pub device_count: u64,
    /// Devices that receive alerts (alerts enabled and an APNs token).
    #[serde(default)]
    pub alert_device_count: u64,
    /// Registered widget push tokens.
    #[serde(default)]
    pub widget_push_count: u64,
}

/// `"iphone" | "ipad"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IosDeviceKind {
    Iphone,
    Ipad,
}

/// `PUT /api/ios-controls/device` body: this device's alert registration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRegistrationRequest {
    pub device_id: String,
    pub name: String,
    pub kind: IosDeviceKind,
    /// `null` keeps the device registered but un-pushable.
    pub apns_token: Option<String>,
    pub environment: ApnsEnvironment,
    pub alerts_enabled: bool,
    pub app_version: Option<String>,
}

/// `PUT /api/ios-controls/device` success body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRegistrationResponse {
    pub registered: bool,
    pub alerts_enabled: bool,
    /// Whether the server can push (APNs configured).
    pub apns_enabled: bool,
}

/// `PUT /api/ios-controls/widget-push` body: the widget extension's
/// WidgetKit push token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetPushRequest {
    pub device_id: String,
    pub push_token: String,
    pub environment: ApnsEnvironment,
}

/// `PUT /api/ios-controls/widget-push` success body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetPushResponse {
    pub registered: bool,
}

/// One live streamer in the On Air widget.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnAirLive {
    pub id: String,
    pub name: String,
    pub tier: StreamerTier,
    /// `"kick" | "twitch" | "youtube"`: the session's primary platform.
    pub platform: String,
    pub title: Option<String>,
    pub category: Option<String>,
    pub viewers: Option<i64>,
    pub peak_viewers: Option<i64>,
    pub started_at: Option<Ms>,
    pub avatar_url: Option<String>,
    /// The primary binding's watch URL.
    pub watch_url: String,
    /// A native app URL when the https watch URL is not a universal link
    /// (Twitch `twitch://stream/<login>`).
    pub app_url: Option<String>,
    /// The intelligence headline for the current session.
    pub headline: Option<String>,
}

/// A recently ended primary streamer (the On Air empty state).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnAirRecent {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub ended_at: Ms,
    pub avatar_url: Option<String>,
    pub watch_url: String,
}

/// `GET /api/ios-controls/widgets/on-air`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnAirResponse {
    pub generated_at: Ms,
    /// Every live streamer, including background tier and discoveries.
    pub live_count: u64,
    /// Sum of current viewers over live streamers that report one.
    pub total_viewers: i64,
    /// Ranked like the live slots (tier, then viewers), at most 8.
    pub live: Vec<OnAirLive>,
    /// Up to 4 most recently ended primary streamers.
    pub recent: Vec<OnAirRecent>,
}

/// One control in a registration request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlRegistration {
    pub control_id: String,
    pub slot: u8,
    pub push_token: String,
    pub environment: ApnsEnvironment,
}

/// `PUT /api/ios-controls/registrations` body: the device's complete control set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationRequest {
    pub device_id: String,
    pub controls: Vec<ControlRegistration>,
}

/// `PUT /api/ios-controls/registrations` success body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationResponse {
    pub registered: u64,
}

/// Path builders for the iOS routes.
pub mod paths {
    pub const DIAGNOSTICS: &str = "/api/ios-controls/diagnostics";
    pub const REGISTRATIONS: &str = "/api/ios-controls/registrations";
    pub const DEVICE: &str = "/api/ios-controls/device";
    pub const WIDGET_PUSH: &str = "/api/ios-controls/widget-push";
    pub const WIDGET_ON_AIR: &str = "/api/ios-controls/widgets/on-air";

    /// `GET /api/ios-controls/slots/:slot`.
    pub fn slot(slot: u8) -> String {
        format!("/api/ios-controls/slots/{slot}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_slot_serializes_every_nullable_field() {
        let slot = LiveSlotState {
            slot: 4,
            is_live: false,
            streamer_id: None,
            display_name: "Nobody Live".into(),
            title: None,
            platform: None,
            url: "http://omni.boris".into(),
            viewer_count: None,
            started_at: None,
            updated_at: 2,
        };
        assert_eq!(
            serde_json::to_value(slot).unwrap(),
            json!({
                "slot": 4, "isLive": false, "streamerId": null, "displayName": "Nobody Live",
                "title": null, "platform": null, "url": "http://omni.boris",
                "viewerCount": null, "startedAt": null, "updatedAt": 2
            })
        );
    }

    #[test]
    fn registration_request_decodes_the_app_payload() {
        let request: RegistrationRequest = serde_json::from_value(json!({
            "deviceId": "device-12345",
            "controls": [{"controlId": "c", "slot": 1, "pushToken": "ab", "environment": "sandbox"}]
        }))
        .unwrap();
        assert_eq!(request.controls[0].environment, ApnsEnvironment::Sandbox);
    }
}
