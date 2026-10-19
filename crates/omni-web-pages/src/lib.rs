//! Omni domain pages.
//!
//! `omni-web`'s router mounts exactly these exports; their names and props
//! are this crate's contract with the router:
//!
//! | Export | Route | Props |
//! |---|---|---|
//! | [`MediaPage`] | `/media` (and `/recommendations`) | none |
//! | [`MediaDetailPage`] | `/media/:id` | `id` |
//! | [`PodcastsPage`] | `/podcasts` | none |
//! | [`PodcastDetailPage`] | `/podcasts/:id` | `id` |
//! | [`FeedbackPage`] | `/feedback/(recommendations\|podcasts)/:id` | `kind`, `id` |
//! | [`PodsPage`] | `/pods` | none |
//! | [`PodsDetailPage`] | `/pods/:id` | `id` |
//! | [`DeliveriesPage`], [`CalendarPage`] | `/deliveries` (`?tracking=`), `/calendar` (`?day=`) | none |
//! | [`StreamerConfigPage`] | `/live/streamers` | none |
//! | [`StylistPage`] | `/stylist` (`?ns=` picks a namespace) | none |
//! | [`PetsPage`], [`RemindersPage`], [`McpPage`], [`ClaudePage`] | `/pets`, `/reminders`, `/mcp-activity`, `/claude` | none |
//! | [`GatewayPage`] | `/gateway` (`?integration=` opens one) | none |
//!
//! Path parameters arrive already percent-decoded. Each detail page is
//! re-created when its id changes.

pub mod calendar;
mod claude;
mod common;
pub mod deliveries;
mod feedback;
mod gateway;
mod mcp;
mod media;
pub mod pets;
mod podcasts;
mod pods;
mod pods_detail;
mod rec_ui;
mod recommendation_runs;
pub mod reminders;
pub mod streamer_config;
pub mod stylist;
mod taste_brain;

pub use calendar::{AgendaTile, CalendarPage, CalendarSyncFacts, use_calendar_status};
pub use claude::ClaudePage;
pub use deliveries::{DeliveriesPage, DeliveriesTile, ParcelCacheFacts, use_parcels};
pub use feedback::{FeedbackKind, FeedbackPage};
pub use gateway::GatewayPage;
pub use mcp::McpPage;
pub use media::{MediaDetailPage, MediaPage};
pub use pets::PetsPage;
pub use podcasts::{PodcastDetailPage, PodcastsPage};
pub use pods::{PodsPage, format_audio_duration};
pub use pods_detail::PodsDetailPage;
pub use recommendation_runs::RecommendationRuns;
pub use reminders::RemindersPage;
pub use streamer_config::StreamerConfigPage;
pub use stylist::StylistPage;
pub use taste_brain::{TasteBrain, TasteBrainProfile, TasteClaimView};
