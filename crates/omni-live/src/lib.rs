//! Livestream monitoring: the tracked-streamer configuration (stored in the
//! docstore, edited through the UI and MCP), aggregate streamer state over
//! platform bindings, edge notifications, viewer metrics and sessions,
//! Destiny.gg discovery with durable profile identity links, the
//! `LiveCheckTask`, streamer avatars (`StreamerAvatars`), the streamer routes
//! and the `LiveDirectory` port.
//!
//! App wiring:
//! ```text
//! let live = omni_live::LiveModule::load(&ctx)?;
//! let ios = omni_ios_controls::IosControls::new(&ctx, live.roster()).await;
//! let live_subsystem = live.into_subsystem(Some(ios.reconciler()))?;   // sets ports.live_directory
//! let ios_subsystem = ios.into_subsystem();
//! ```
//!
//! A `Migrate` boot step loads the configuration; a stored configuration that cannot be read fails
//! boot.

pub mod avatars;
pub mod config;
pub mod config_routes;
pub mod deep_link;
pub mod dgg;
pub mod directory;
pub mod display;
pub mod display_order;
pub mod error;
pub mod events;
pub mod format;
pub mod identity;
pub mod mcp;
pub mod metrics;
pub mod notification_policy;
pub mod notify;
pub mod outage;
pub mod platform;
pub mod platforms;
pub mod profile_links;
pub mod relay;
pub mod routes;
pub mod sessions;
pub mod status;
pub mod streamers;
pub mod task;
pub mod title_debounce;
pub mod transitions;
pub mod trigger_channels;

use std::collections::HashSet;
use std::sync::Arc;

use jiff::tz::TimeZone;
use omni_runtime::{AppContext, BootError, BootPhase, BootStep, ManagedEntity, Subsystem};
use omni_store::entity::EntityDescriptor;

pub use config::{ConfigError, StreamerConfigService, TopEmbeds};
pub use directory::LiveDirectoryService;
pub use error::LiveError;
use omni_api::streamer_config::StreamerConfigCreate;
pub use platform::{Platform, PlatformBinding};
pub use streamers::{Roster, Streamer};
pub use task::{LiveCheck, LiveCheckTask, TickHook};

use crate::avatars::{AvatarRefresh, StreamerAvatarsTask};
use crate::dgg::WebSocketDggFeed;
use crate::identity::ProfileIdentityLink;
use crate::metrics::{PlatformViewerMetrics, ViewerMetrics};
use crate::notify::PushoverNotifier;
use crate::platforms::{KickCredentials, Platforms};
use crate::profile_links::{ProfileFetcher, ProfileIdentityLearner};
use crate::sessions::StreamSessions;
use crate::status::StreamerStatus;
use crate::task::{DggDiscovery, LiveCheckDeps, PortIntelligence};

const LOG: &str = "Main";

/// Why the live subsystem cannot start (fails boot).
#[derive(Debug, thiserror::Error)]
pub enum LiveBootError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("invalid TZ {tz:?}: {message}")]
    TimeZone { tz: String, message: String },
    #[error("invalid live task schedule: {0}")]
    Schedule(String),
    #[error(transparent)]
    Tools(#[from] omni_mcp_kit::ToolMetaError),
}

/// Every entity this crate owns (for `migrate_all` and the compat audit).
pub fn entities() -> Vec<EntityDescriptor> {
    vec![
        EntityDescriptor::of::<config::StreamerConfigRow>(),
        EntityDescriptor::of::<config::LiveSettingsRow>(),
        EntityDescriptor::of::<StreamerStatus>(),
        EntityDescriptor::of::<StreamSessions>(),
        EntityDescriptor::of::<ViewerMetrics>(),
        EntityDescriptor::of::<PlatformViewerMetrics>(),
        EntityDescriptor::of::<ProfileIdentityLink>(),
        EntityDescriptor::of::<avatars::StreamerAvatar>(),
    ]
}

/// Data manager rows, in display order.
pub fn managed_entities() -> Vec<ManagedEntity> {
    let managed = |slug, label, description, warning, entity, primary_key| ManagedEntity {
        slug,
        label,
        description,
        warning,
        entity,
        primary_key,
        can_delete: None,
        after_delete: None,
    };
    vec![
        managed(
            "streamer-status",
            "Streamer status",
            "Current aggregate live session and last offline state per streamer.",
            Some("Deleting live state can cause a fresh went-live transition on the next check."),
            EntityDescriptor::of::<StreamerStatus>(),
            &["streamerId"],
        ),
        managed(
            "streamer-sessions",
            "Stream sessions",
            "Completed live sessions per streamer (start, end, peak, title).",
            Some("Deleted session history cannot be reconstructed."),
            EntityDescriptor::of::<StreamSessions>(),
            &["streamerId"],
        ),
        managed(
            "streamer-viewer-metrics",
            "Viewer metrics",
            "Daily viewer peaks and all-time records per streamer.",
            Some("Deleted viewer records cannot be reconstructed outside the retained window."),
            EntityDescriptor::of::<ViewerMetrics>(),
            &["streamerId"],
        ),
        managed(
            "streamer-platform-viewer-metrics",
            "Streamer platform viewer metrics",
            "Per-platform daily viewer peaks for grouped streamer identities.",
            None,
            EntityDescriptor::of::<PlatformViewerMetrics>(),
            &["streamerId", "platform", "username"],
        ),
        managed(
            "live-profile-identity-link",
            "Livestream profile identity links",
            "Durable direct-profile associations between platform accounts.",
            None,
            EntityDescriptor::of::<ProfileIdentityLink>(),
            &["sourceBinding"],
        ),
    ]
}

/// The live subsystem before it is assembled.
pub struct LiveModule {
    ctx: AppContext,
    roster: Roster,
    configured: Roster,
    top_embeds: TopEmbeds,
    tz: TimeZone,
    /// Shared by the live check and the avatar refresh (one Kick token cache).
    platforms: Arc<Platforms>,
    service: StreamerConfigService,
    /// Streamers were given directly (tests); no boot step loads them.
    preloaded: bool,
}

impl LiveModule {
    /// The production module: streamers load from the docstore in a
    /// `Migrate` boot step, after `migrate_all`.
    pub fn load(ctx: &AppContext) -> Result<Self, LiveBootError> {
        Self::build(ctx, Vec::new(), 0, false)
    }

    /// An in-memory configuration (tests), validated as creates and never
    /// stored. Kick bindings are dropped when Kick credentials are missing.
    pub fn preloaded(
        ctx: &AppContext,
        streamers: Vec<StreamerConfigCreate>,
        dgg_top_embeds: u32,
    ) -> Result<Self, LiveBootError> {
        let configured = config::rows_for(streamers, 0)?
            .iter()
            .filter_map(|row| config::to_streamer(row, true))
            .collect();
        Self::build(ctx, configured, dgg_top_embeds, true)
    }

    fn build(
        ctx: &AppContext,
        configured: Vec<Streamer>,
        dgg_top_embeds: u32,
        preloaded: bool,
    ) -> Result<Self, LiveBootError> {
        let tz = TimeZone::get(&ctx.config.tz).map_err(|e| LiveBootError::TimeZone {
            tz: ctx.config.tz.clone(),
            message: e.to_string(),
        })?;
        let kick = match (&ctx.config.kick_client_id, &ctx.config.kick_client_secret) {
            (Some(id), Some(secret)) if !id.is_empty() && !secret.is_empty() => {
                Some(KickCredentials {
                    client_id: id.clone(),
                    client_secret: secret.clone(),
                })
            }
            _ => None,
        };
        let platforms = Arc::new(
            Platforms::new(ctx.public_http.clone(), kick.clone()).with_clock(ctx.clock.clone()),
        );
        let streamers = if kick.is_some() {
            configured
        } else {
            streamers::drop_platform_bindings(configured, Platform::Kick).0
        };
        let roster = Roster::new(streamers.clone());
        let configured = Roster::new(streamers);
        let top_embeds = TopEmbeds::new(dgg_top_embeds);
        let service = StreamerConfigService::new(
            ctx.store.clone(),
            ctx.clock.clone(),
            Some(ctx.bus.clone()),
            configured.clone(),
            roster.clone(),
            top_embeds.clone(),
            kick.is_some(),
        );
        Ok(Self {
            ctx: ctx.clone(),
            roster,
            configured,
            top_embeds,
            tz,
            platforms,
            service,
            preloaded,
        })
    }

    /// The shared streamer list (iOS controls read it).
    pub fn roster(&self) -> Roster {
        self.roster.clone()
    }

    pub fn time_zone(&self) -> &TimeZone {
        &self.tz
    }

    /// Reads and edits the tracked-streamer configuration.
    pub fn config_service(&self) -> StreamerConfigService {
        self.service.clone()
    }

    pub fn directory(&self) -> Arc<LiveDirectoryService> {
        Arc::new(LiveDirectoryService::new(
            self.ctx.store.clone(),
            self.roster.clone(),
        ))
    }

    /// The production live check over this configuration.
    pub fn live_check(&self, reconcile: Option<Arc<dyn TickHook>>) -> LiveCheck {
        let ctx = &self.ctx;
        let fetcher = self.platforms.clone();
        let deps = LiveCheckDeps {
            store: ctx.store.clone(),
            clock: ctx.clock.clone(),
            tz: self.tz.clone(),
            fetcher,
            notifier: Arc::new(PushoverNotifier::new(ctx.pushover.clone())),
            offline_notifications: ctx.config.offline_notifications,
            live_token: ctx
                .config
                .pushover_token(omni_config::PushoverChannel::Live)
                .map(str::to_owned),
            bus: Some(ctx.bus.clone()),
        };
        let learner =
            ProfileIdentityLearner::new(ctx.store.clone(), ProfileFetcher::new(ctx.http.clone()));
        let mut check =
            LiveCheck::with_configured(self.roster.clone(), self.configured.clone(), deps)
                .with_intelligence(Arc::new(PortIntelligence::new(ctx.ports.clone())))
                .with_events(ctx.ports.clone())
                .with_dgg(DggDiscovery {
                    top_embeds: self.top_embeds.clone(),
                    available_platforms: Platform::ALL.into_iter().collect::<HashSet<_>>(),
                    feed: Arc::new(WebSocketDggFeed::default()),
                    identity: Arc::new(learner),
                });
        if let Some(hook) = reconcile {
            check = check.with_reconcile(hook);
        }
        check
    }

    /// Routes, the `LiveCheckTask`, the configuration boot step and MCP
    /// tools, entities, and the `LiveDirectory` port.
    pub fn into_subsystem(
        self,
        reconcile: Option<Arc<dyn TickHook>>,
    ) -> Result<Subsystem, LiveBootError> {
        if self.ctx.ports.set_live_directory(self.directory()).is_err() {
            tracing::warn!(target: LOG, "LiveDirectory port was already set; keeping the existing one");
        }
        let mut subsystem = Subsystem::named("live");
        let check = Arc::new(self.live_check(reconcile));
        let task = LiveCheckTask::new(check, &self.tz)
            .map_err(|e| LiveBootError::Schedule(e.to_string()))?;
        subsystem.tasks.push(Arc::new(task));
        let refresh = AvatarRefresh::new(
            self.ctx.store.clone(),
            self.ctx.clock.clone(),
            self.roster.clone(),
            self.configured.clone(),
            self.platforms.clone(),
        );
        let avatars = StreamerAvatarsTask::new(Arc::new(refresh), &self.tz)
            .map_err(|e| LiveBootError::Schedule(e.to_string()))?;
        subsystem.tasks.push(Arc::new(avatars));
        if !self.preloaded {
            let service = self.service.clone();
            subsystem.boot_steps.push(BootStep {
                phase: BootPhase::Migrate,
                name: "live-streamer-config",
                run: Box::new(move |_: AppContext| {
                    Box::pin(async move {
                        service
                            .reload()
                            .await
                            .map_err(|e| BootError::new("live-streamer-config", e.to_string()))
                    })
                }),
            });
        }
        let relays = relay::KickRelays::new(
            relay::source::KickPlaybackSource::new(self.ctx.public_http.clone()),
            self.ctx.clock.clone(),
            self.ctx.tracker.clone(),
            self.ctx.shutdown.clone(),
            relay::RelayConfig::default(),
        );
        subsystem.router = routes::router(routes::LiveRoutesState {
            store: self.ctx.store.clone(),
            roster: self.roster.clone(),
            relays,
        })
        .merge(config_routes::router(self.service.clone()));
        subsystem.mcp_tools = mcp::tools(&self.service)?;
        subsystem.entities = entities();
        subsystem.managed_entities = managed_entities();
        Ok(subsystem)
    }
}
