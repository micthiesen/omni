//! Wire DTOs for every REST/SSE contract.
//!
//! `serde` + `serde_json` only; must compile for `wasm32-unknown-unknown`.
//! Serde rules: `rename_all = "camelCase"`; a nullable field is `Option<T>` and is
//! always serialized; an optional field is
//! `#[serde(default, skip_serializing_if = "Option::is_none")]`; string unions
//! are enums with explicit `rename`; never `deny_unknown_fields`.

/// The running build's identity and `/api/health`.
pub mod build;
/// Error body, pagination, path builders.
pub mod common;
/// `/api/costs`.
pub mod costs;
/// Task runs, run logs and the run-log SSE frames.
pub mod runs;
/// `/api/tasks`.
pub mod tasks;

/// Data manager.
pub mod data;
/// Dashboard snapshot (`/api/snapshot`, `/api/events`).
pub mod snapshot;

/// iOS live controls.
pub mod ios;
/// Tracked-streamer configuration.
pub mod streamer_config;
/// Streamers, metrics, sessions, trigger channels.
pub mod streamers;

/// Livestream intelligence details and feedback.
pub mod intelligence;

/// Email activity, logs, rules, feedback.
pub mod email;

/// PressPods episodes, jobs, details.
pub mod presspods;

/// Podcast recommendations and taste profile.
pub mod podcasts;

/// Media recommendations, taste profile, on-deck items.
pub mod media;

/// The primary iCloud calendar.
pub mod calendar;

/// Claude activity, sessions and transcripts.
pub mod claude;
/// MCP Events payloads published through the event port.
pub mod events;
/// MCP gateway management.
pub mod gateway;
/// MCP activity.
pub mod mcp_activity;

/// Reminders public status.
pub mod reminders;

/// Pets.
pub mod pets;

/// Parcel deliveries (cached Parcel API reads).
pub mod parcels;

/// The read-only stylist ledger overview.
pub mod stylist;
