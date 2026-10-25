//! Media recommendations: Plex history and library, TMDB catalog,
//! Radarr/Sonarr acquisition, the recommendation pipeline, taste reflection,
//! `/api/recommendations*` routes, the media MCP tools and the dashboard
//! on-deck port.
//!
//! Invariants:
//! - an unavailable Plex history, in-progress view, library or watchlist
//!   aborts a run (never decide against missing state);
//! - a commit writes the pending row before the Arr write and reserves the
//!   notification before Pushover; a `reserved` row reconciles to `unknown`
//!   and is never re-sent;
//! - titles are excluded for 180 days after a recommendation (24 h after a
//!   failed attempt), permanently once watched or abandoned;
//! - Arr additions count only once verified in the service's own list.

use std::sync::Arc;

use omni_ai::tools::WebSearch;
use omni_runtime::{AppContext, ManagedEntity, Subsystem};
use omni_store::entity::EntityDescriptor;
use omni_tasks::{CronSchedule, Task};

pub mod arr;
pub mod candidates;
pub mod error;
pub mod filters;
pub mod history;
pub mod identity;
pub mod js;
pub mod mcp;
pub mod media_library;
pub mod on_deck;
pub mod outcomes;
pub mod persistence;
pub mod pipeline;
pub mod plex;
pub mod routes;
pub mod run_log;
pub mod selection;
pub mod services;
pub mod shortlist;
pub mod task;
pub mod taste;
pub mod tmdb;
pub mod types;
pub mod watchlist;

pub use on_deck::MediaOnDeck;
pub use services::MediaServices;

use arr::inspect::ArrInspect;
use arr::movies::{MovieSearchData, RadarrMovies};
use arr::seasons::{SeasonSearchData, SonarrSeasons};
use arr::{ArrConfig, ArrHttp};
use media_library::PlexLibrary;
use persistence::{IdentityAliasData, RecommendationData};
use plex::HttpPlexGet;
use services::PushoverNotifier;
use taste::{TasteEvidenceData, TasteProfileData};
use tmdb::TmdbClient;
use watchlist::ArrWatchlist;

/// Subsystem construction failures (invalid schedules).
#[derive(Debug, thiserror::Error)]
pub enum MediaSetupError {
    #[error("invalid {key} schedule: {source}")]
    Schedule {
        key: &'static str,
        #[source]
        source: omni_tasks::InvalidScheduleError,
    },
    #[error("media MCP tools: {0}")]
    Tools(#[from] omni_mcp_kit::ToolMetaError),
}

/// The process time zone (`TZ`), UTC when the name is unknown.
pub fn time_zone(config: &omni_config::Config) -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(&config.tz).unwrap_or_else(|error| {
        tracing::warn!(target: "Main", "Unknown TZ {:?} ({error}); using UTC", config.tz);
        jiff::tz::TimeZone::UTC
    })
}

/// The production services over an [`AppContext`].
pub fn services_from_context(ctx: &AppContext) -> MediaServices {
    let config = ctx.config.clone();
    let library = PlexLibrary::from_config(
        config.plex_url.as_deref(),
        config.plex_token.as_deref(),
        config.plex_account_id,
        |url, token| Arc::new(HttpPlexGet::new(ctx.http.clone(), url, token)),
    );
    let arr_http = ArrHttp::new(ctx.http.clone(), ctx.side_effects);
    let sonarr = ArrConfig {
        url: config.sonarr_url.clone(),
        api_key: config.sonarr_api_key.clone(),
        root_folder_path: config.sonarr_root_folder_path.clone(),
        quality_profile_id: config.sonarr_quality_profile_id,
    };
    let seasons = SonarrSeasons::new(
        arr_http.clone(),
        sonarr.clone(),
        ctx.store.clone(),
        ctx.tracker.clone(),
    );
    let radarr = ArrConfig {
        url: config.radarr_url.clone(),
        api_key: config.radarr_api_key.clone(),
        root_folder_path: config.radarr_root_folder_path.clone(),
        quality_profile_id: config.radarr_quality_profile_id,
    };
    let movies = RadarrMovies::new(
        arr_http.clone(),
        radarr.clone(),
        ctx.store.clone(),
        ctx.tracker.clone(),
    );
    let arr = ArrInspect::new(arr_http.clone(), radarr.clone(), sonarr.clone());
    let watchlist = ArrWatchlist::new(arr_http, radarr, sonarr, seasons.clone(), movies.clone());
    let research = WebSearch::new(
        ctx.public_http.clone(),
        config.tavily_api_key.clone().unwrap_or_default(),
        ctx.costs.clone(),
    );
    MediaServices {
        store: ctx.store.clone(),
        clock: ctx.clock.clone(),
        ai: ctx.ai.clone(),
        library: Arc::new(library),
        watchlist: Arc::new(watchlist),
        seasons,
        movies,
        arr,
        catalog: Arc::new(TmdbClient::new(
            ctx.http.clone(),
            config.tmdb_api_key.clone(),
        )),
        research: Arc::new(research),
        notifier: Arc::new(PushoverNotifier(ctx.pushover.clone())),
        config,
    }
}

/// Every entity this package owns (migrate_all and the compat audit).
pub fn entity_descriptors() -> Vec<EntityDescriptor> {
    vec![
        EntityDescriptor::of::<RecommendationData>(),
        EntityDescriptor::of::<IdentityAliasData>(),
        EntityDescriptor::of::<TasteEvidenceData>(),
        EntityDescriptor::of::<TasteProfileData>(),
        EntityDescriptor::of::<SeasonSearchData>(),
        EntityDescriptor::of::<MovieSearchData>(),
    ]
}

/// Data-manager rows, in display order (wiring interleaves the podcast rows
/// between the first and second entries).
pub fn managed_entities() -> Vec<ManagedEntity> {
    let entry = |entity: EntityDescriptor,
                 label: &'static str,
                 description: &'static str,
                 warning: Option<&'static str>,
                 primary_key: &'static [&'static str]| ManagedEntity {
        slug: entity.name,
        label,
        description,
        warning,
        entity,
        primary_key,
        can_delete: None,
        after_delete: None,
    };
    vec![
        entry(
            EntityDescriptor::of::<RecommendationData>(),
            "Media recommendations",
            "Recommendation attempts, delivery state, outcomes, and feedback.",
            Some("Deleting rows changes cooldown, exclusion, and taste evidence behavior."),
            &["recommendationId"],
        ),
        entry(
            EntityDescriptor::of::<TasteEvidenceData>(),
            "Taste evidence",
            "Versioned observations used to build the media taste profile.",
            Some("Deleting evidence changes the inputs available to future reflections."),
            &["evidenceId"],
        ),
        entry(
            EntityDescriptor::of::<TasteProfileData>(),
            "Taste profiles",
            "Generated checkpoints of the current media taste model.",
            None,
            &["profileId"],
        ),
        entry(
            EntityDescriptor::of::<IdentityAliasData>(),
            "Identity aliases",
            "Cached Plex GUID to TMDB identity resolutions.",
            Some("Deleted aliases will be resolved again when encountered."),
            &["guid"],
        ),
    ]
}

/// Builds the tasks that are configured (each logs why when disabled).
pub fn tasks(services: &MediaServices) -> Result<Vec<Arc<dyn Task>>, MediaSetupError> {
    let tz = time_zone(&services.config);
    let recs_schedule =
        CronSchedule::parse(&services.config.recs_schedule, &tz).map_err(|source| {
            MediaSetupError::Schedule {
                key: "RECS_SCHEDULE",
                source,
            }
        })?;
    let taste_schedule = CronSchedule::parse(&services.config.taste_reflection_schedule, &tz)
        .map_err(|source| MediaSetupError::Schedule {
            key: "TASTE_REFLECTION_SCHEDULE",
            source,
        })?;
    let mut tasks: Vec<Arc<dyn Task>> = Vec::new();
    if let Some(task) =
        task::MediaRecommendationTask::create(services.clone(), recs_schedule, tz.clone())
    {
        tasks.push(Arc::new(task));
    }
    if let Some(task) =
        taste::task::MediaTasteReflectionTask::create(services.clone(), taste_schedule)
    {
        tasks.push(Arc::new(task));
    }
    Ok(tasks)
}

/// The media subsystem: routes, tasks, MCP tools and entities; installs the
/// dashboard `OnDeckSource` port ([`MediaOnDeck`]).
pub fn subsystem(ctx: &AppContext) -> Result<Subsystem, MediaSetupError> {
    subsystem_with(ctx, services_from_context(ctx))
}

/// [`subsystem`] over explicit services (tests substitute the I/O seams).
pub fn subsystem_with(
    ctx: &AppContext,
    services: MediaServices,
) -> Result<Subsystem, MediaSetupError> {
    if ctx
        .ports
        .set_on_deck_source(Arc::new(MediaOnDeck::new(services.store.clone())))
        .is_err()
    {
        tracing::warn!(target: "Main", "OnDeckSource port was already set; keeping the existing one");
    }
    let tasks = tasks(&services)?;
    let services = Arc::new(services);
    Ok(Subsystem {
        name: "media",
        router: routes::router(routes::RouteState {
            services: services.clone(),
            tasks: ctx.tasks.clone(),
        }),
        tasks,
        mcp_tools: mcp::media_tools(services)?,
        entities: entity_descriptors(),
        managed_entities: managed_entities(),
        ..Subsystem::named("media")
    })
}
