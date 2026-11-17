//! PressPods: turns submitted article URLs into narrated podcast
//! episodes and serves them as a private RSS feed.
//!
//! Pipeline: durable job queue -> parallel article retrievers rated by the
//! metadata model -> narration cleaning -> chunked, verified TTS with
//! checkpoints -> audio chain (ffmpeg) -> ID3 tags -> episode file then row
//! -> Pushover. See `docs/presspods-audio.md` before touching the audio chain.

pub mod agents;
pub mod audio;
pub mod costs;
pub mod dates;
pub mod doctor;
pub mod error;
pub mod events;
pub mod formatting;
pub mod karakeep;
pub mod mcp;
pub mod model;
pub mod mp3;
pub mod persistence;
pub mod pipeline;
pub mod public_http;
pub mod retrievers;
pub mod routes;
pub mod rss;
pub mod service;
pub mod speech;
pub mod storage;
pub mod submit;
pub mod task;
pub mod types;
pub mod url;

use std::sync::Arc;

use omni_runtime::{AppContext, ManagedEntity, Subsystem};
use omni_store::entity::EntityDescriptor;

pub use error::PressPodsError;
pub use model::{PressPodsEpisode, PressPodsJob};
pub use service::PressPods;

const LOG: &str = "PressPods";

/// Why the subsystem could not be built.
#[derive(Debug, thiserror::Error)]
pub enum SubsystemError {
    #[error(transparent)]
    Service(#[from] PressPodsError),
    #[error("PressPods MCP tools: {0}")]
    Tools(#[from] omni_mcp_kit::ToolMetaError),
    #[error("PressPods schedule: {0}")]
    Schedule(#[from] omni_tasks::InvalidScheduleError),
}

/// Entities owned by PressPods (for `migrate_all` and the compat audit).
pub fn entities() -> Vec<EntityDescriptor> {
    vec![
        EntityDescriptor::of::<PressPodsEpisode>(),
        EntityDescriptor::of::<PressPodsJob>(),
        EntityDescriptor::of::<model::PressPodsJobAttempt>(),
    ]
}

/// Data-manager rows.
pub fn managed_entities() -> Vec<ManagedEntity> {
    vec![
        ManagedEntity {
            slug: "press-pods-episode",
            label: "PressPods episodes",
            description: "Generated podcast episodes: metadata, narration, chunk stats, costs.",
            warning: Some(
                "Deleting a row does not remove its audio file from disk; the episode also drops out of the RSS feed.",
            ),
            entity: EntityDescriptor::of::<PressPodsEpisode>(),
            primary_key: &["episodeId"],
            can_delete: None,
            after_delete: None,
        },
        ManagedEntity {
            slug: "press-pods-job",
            label: "PressPods jobs",
            description: "Durable submission queue with retry backoff and stale-claim reclaim.",
            warning: None,
            entity: EntityDescriptor::of::<PressPodsJob>(),
            primary_key: &["jobId"],
            can_delete: None,
            after_delete: None,
        },
    ]
}

/// Builds the subsystem: routes (only with `PRESSPODS_AUTH_TOKEN`), the
/// `PressPods` task (only when the worker's credentials are complete), the
/// six MCP tools, entities and data-manager rows.
pub fn subsystem(ctx: &AppContext) -> Result<Subsystem, SubsystemError> {
    let service = PressPods::from_context(ctx)?;
    build(ctx, service)
}

/// [`subsystem`] over an explicitly wired service (tests swap the seams).
pub fn build(ctx: &AppContext, service: PressPods) -> Result<Subsystem, SubsystemError> {
    let config = &ctx.config;
    let mut subsystem = Subsystem::named("PressPods");
    subsystem.entities = entities();
    subsystem.managed_entities = managed_entities();
    subsystem.mcp_tools = mcp::tools(&service)?;

    let has_token = config
        .presspods_auth_token
        .as_deref()
        .is_some_and(|t| !t.is_empty());
    let missing: Vec<&str> = service::worker_credentials(config)
        .into_iter()
        .filter(|(_, present)| !present)
        .map(|(key, _)| key)
        .collect();

    if let Some(router) = routes::router(service.clone()) {
        subsystem.router = router;
        // The routes gate only on the token; without TTS/model credentials
        // submissions would queue forever, so say so loudly at boot.
        if let Some(tts) = missing
            .iter()
            .find(|key| matches!(**key, "ELEVENLABS_API_KEY" | "PRESSPODS_TTS_URL"))
        {
            tracing::warn!(
                target: LOG,
                "PressPods routes are active but the worker task is disabled (missing {tts}); submitted jobs will queue without processing"
            );
        }
    }

    if !has_token {
        tracing::info!(target: LOG, "PressPods disabled: missing PRESSPODS_AUTH_TOKEN");
    } else if !missing.is_empty() {
        tracing::info!(target: LOG, "PressPods disabled: missing {}", missing.join(", "));
    } else {
        let tz = jiff::tz::TimeZone::get(&config.tz).unwrap_or(jiff::tz::TimeZone::UTC);
        let schedule = omni_tasks::CronSchedule::parse(task::SCHEDULE, &tz)?;
        subsystem
            .tasks
            .push(Arc::new(task::PressPodsTask::new(service, schedule)));
    }
    Ok(subsystem)
}
