//! Outbound event delivery (`/api/gateway/outbound*`, LAN only; changes need
//! a trusted browser; see "Event journal" in `docs/mcp-events.md`).
//!
//! - `GET /api/gateway/outbound[?status=failed]`: [`OutboundOverview`];
//! - `PUT|DELETE /api/gateway/outbound/targets/:id` ([`EventTargetUpsert`])
//!   -> [`EventTargetView`];
//! - `POST /api/gateway/outbound/routes` ([`EventRouteCreate`]) ->
//!   [`EventRouteView`]; `PATCH|DELETE /api/gateway/outbound/routes/:id`
//!   ([`EventRoutePatch`]);
//! - `POST /api/gateway/outbound/deliveries/:id/retry` -> [`DeliveryView`].
//!
//! A target's key is write-only: views say whether one is set, never what it is.
//! A PUT replaces the target (a missing `connect` clears it; a missing `key`
//! keeps the stored one). Removing a target a route uses is a 409.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::common::Ms;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundOverview {
    pub targets: Vec<EventTargetView>,
    pub routes: Vec<EventRouteView>,
    /// Newest first, at most 100 (only failed ones with `?status=failed`).
    pub deliveries: Vec<DeliveryView>,
    pub counts: DeliveryCounts,
    /// Event names a route can name, with their argument names.
    pub events: Vec<RoutableEventView>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryCounts {
    /// Waiting for a first attempt or a retry.
    pub pending: u32,
    /// Failed for good in the last 7 days (dead letters; `?status=failed`
    /// lists every kept one, up to 30 days).
    pub failed: u32,
    /// Delivered or accepted in the last 24 hours.
    pub delivered: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutableEventView {
    pub name: String,
    pub description: String,
    /// Argument names a route may set (all optional unless the event says so).
    pub arguments: Vec<String>,
}

/// A bot webhook (or any receiver) the journal posts to with
/// `Authorization: Bearer <key>` and an `Idempotency-Key`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventTargetView {
    pub id: String,
    pub label: String,
    /// The stored URL (never with credentials).
    pub url: String,
    pub key_set: bool,
    /// Where to connect instead of the URL's host (a proxy container name);
    /// TLS still checks the URL's host.
    pub connect: Option<String>,
    pub created_at: Ms,
    pub updated_at: Ms,
}

/// `PUT /api/gateway/outbound/targets/:id`: creates or replaces a target.
/// A missing `key` keeps the stored one (required on create).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventTargetUpsert {
    pub label: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect: Option<String>,
}

/// Every event `name` whose arguments match goes to `target`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRouteView {
    pub id: String,
    pub event: String,
    pub arguments: BTreeMap<String, String>,
    pub target: String,
    pub enabled: bool,
    pub created_at: Ms,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRouteCreate {
    pub event: String,
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
    pub target: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRoutePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryKind {
    /// A stored route to a target.
    #[serde(rename = "route")]
    Route,
    /// A notice addressed to a target (Claude turns, recipe requests).
    #[serde(rename = "notice")]
    Notice,
    /// An `events/subscribe` webhook of an MCP client.
    #[serde(rename = "subscription")]
    Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryState {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "delivered")]
    Delivered,
    /// Accepted, but the bot ran no webhook routine.
    #[serde(rename = "ignored")]
    Ignored,
    /// Failed for good: refused, or still failing a day after it was queued.
    #[serde(rename = "failed")]
    Failed,
    /// Waiting for its target to be created.
    #[serde(rename = "unconfigured")]
    Unconfigured,
    /// Cancelled before it went out.
    #[serde(rename = "skipped")]
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryView {
    pub id: String,
    pub event: String,
    pub kind: DeliveryKind,
    /// The target id, or the callback host of a subscription.
    pub destination: String,
    pub state: DeliveryState,
    pub attempts: u32,
    pub created_at: Ms,
    pub updated_at: Ms,
    /// When the next attempt is due (pending only).
    pub next_attempt_at: Option<Ms>,
    /// Why it failed for good (`rejected`, `attempts_exhausted`, ...).
    pub failure: Option<String>,
    /// Newest first, at most 20.
    pub history: Vec<AttemptView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttemptView {
    pub at: Ms,
    /// The HTTP status, when an answer arrived.
    pub status: Option<u16>,
    /// `timeout`, `network`, `http_403`, ...
    pub error: Option<String>,
    pub duration_ms: u32,
}
