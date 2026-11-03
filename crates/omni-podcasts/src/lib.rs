//! Podcast recommendations, taste reflection and the Castro account bridge.
//!
//! [`subsystem`] wires the three tasks (`PodcastRecs`,
//! `PodcastTasteReflection`, `CastroInboxCleanup`), the
//! `/api/podcast-recommendations` routes, the seven podcast MCP tools, the
//! entity descriptors, the data-manager rows and the Castro alert gate.

use std::sync::Arc;

use jiff::tz::TimeZone;
use omni_ai::ModelRole;
use omni_ai::tools::WebSearch;
use omni_config::Config;
use omni_runtime::{AppContext, BootError, ManagedEntity, Subsystem};
use omni_store::entity::EntityDescriptor;
use omni_tasks::{CronSchedule, Task};

pub mod account;
pub mod candidates;
pub mod castro;
pub mod concurrency;
pub mod discovery;
pub mod exa;
pub mod filters;
pub mod guest_selection;
pub mod guests;
pub mod itunes;
pub mod log_file;
pub mod mcp;
pub mod models;
pub mod outcomes;
pub mod pacing;
pub mod persistence;
pub mod pipeline;
pub mod podcastindex;
pub mod record;
pub mod reflection;
pub mod routes;
pub mod rss;
pub mod selection;
pub mod shortlist;
pub mod sources;
pub mod subscriptions;
pub mod sync;
pub mod task;
pub mod taste;
pub mod titles;
pub mod types;
pub mod voices;

use account::AccountProvider;
use castro::alert_gate::CastroFailureGate;
use castro::cleanup::CastroInboxCleanupTask;
use castro::{CastroAccounts, NoAccount};
use models::Models;
use persistence::{PodcastRecommendationData, PodcastRunState};
use pipeline::{PodcastDeps, PushoverNotifier};
use podcastindex::{PersonSearch, PodcastIndexClient};
use reflection::task::PodcastTasteReflectionTask;
use reflection::types::{PodcastTasteEvidenceData, PodcastTasteProfileData};
use sources::HttpDirectory;
use task::PodcastRecommendationTask;

const LOG: &str = "PodcastRecs";

/// Every entity this package owns (for `migrate_all` and the compat audit).
pub fn entities() -> Vec<EntityDescriptor> {
    vec![
        EntityDescriptor::of::<PodcastRecommendationData>(),
        EntityDescriptor::of::<PodcastRunState>(),
        EntityDescriptor::of::<PodcastTasteEvidenceData>(),
        EntityDescriptor::of::<PodcastTasteProfileData>(),
    ]
}

/// Data manager rows.
pub fn managed_entities() -> Vec<ManagedEntity> {
    vec![
        ManagedEntity {
            slug: "podcast-recommendation-attempt",
            label: "Podcast recommendations",
            description: "Podcast episode picks, queue state, outcomes, and feedback.",
            warning: Some("Deleting rows changes episode exclusions and show cooldowns."),
            entity: EntityDescriptor::of::<PodcastRecommendationData>(),
            primary_key: &["recommendationId"],
            can_delete: None,
            after_delete: None,
        },
        ManagedEntity {
            slug: "podcast-taste-evidence",
            label: "Podcast taste evidence",
            description: "Listen, outcome, and feedback observations for podcast reflection.",
            warning: Some("Deleting evidence changes the inputs available to future reflections."),
            entity: EntityDescriptor::of::<PodcastTasteEvidenceData>(),
            primary_key: &["evidenceId"],
            can_delete: None,
            after_delete: None,
        },
        ManagedEntity {
            slug: "podcast-taste-profile",
            label: "Podcast taste profiles",
            description: "Generated checkpoints of the current podcast taste model.",
            warning: None,
            entity: EntityDescriptor::of::<PodcastTasteProfileData>(),
            primary_key: &["profileId"],
            can_delete: None,
            after_delete: None,
        },
    ]
}

fn time_zone(config: &Config) -> TimeZone {
    TimeZone::get(&config.tz).unwrap_or_else(|_| TimeZone::system())
}

fn schedule(step: &'static str, expr: &str, tz: &TimeZone) -> Result<CronSchedule, BootError> {
    CronSchedule::parse(expr, tz)
        .map_err(|e| BootError::new(step, format!("invalid schedule {expr:?}: {e}")))
}

/// The provider prefix of the model configured for `role`.
fn provider_of(config: &Config, role: ModelRole) -> String {
    config
        .model(role)
        .split(':')
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn provider_key(config: &Config, provider: &str) -> Option<(&'static str, bool)> {
    let present = |key: &Option<String>| key.as_deref().is_some_and(|k| !k.is_empty());
    match provider {
        "openai" => Some(("OPENAI_API_KEY", present(&config.openai_api_key))),
        "anthropic" => Some(("ANTHROPIC_API_KEY", present(&config.anthropic_api_key))),
        "google" => Some((
            "GOOGLE_GENERATIVE_AI_API_KEY",
            present(&config.google_generative_ai_api_key),
        )),
        _ => None,
    }
}

fn non_empty(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|v| !v.is_empty())
}

/// `PodcastRecommendationTask.create` preconditions: the missing names, or
/// the unreadable seed path reason.
pub fn podcast_recs_disabled_reason(config: &Config) -> Option<String> {
    let mut missing: Vec<String> = Vec::new();
    if !non_empty(&config.podcast_taste_path) {
        missing.push("PODCAST_TASTE_PATH".to_owned());
    }
    if !non_empty(&config.tavily_api_key) {
        missing.push("TAVILY_API_KEY".to_owned());
    }
    let mut providers: Vec<String> = Vec::new();
    for role in [ModelRole::RecsShortlist, ModelRole::RecsSelection] {
        let provider = provider_of(config, role);
        if !providers.contains(&provider) {
            providers.push(provider);
        }
    }
    for name in ["openai", "anthropic", "google"] {
        if providers.iter().any(|p| p == name)
            && let Some((key, false)) = provider_key(config, name)
        {
            missing.push(key.to_owned());
        }
    }
    if !missing.is_empty() {
        return Some(format!("missing {}", missing.join(", ")));
    }
    let path = config.podcast_taste_path.as_deref().unwrap_or_default();
    taste::describe_unreadable_file(path)
        .map(|reason| format!("PODCAST_TASTE_PATH ({path}) {reason}"))
}

/// `PodcastTasteReflectionTask.create` preconditions.
pub fn taste_reflection_missing(config: &Config) -> Vec<String> {
    let provider = provider_of(config, ModelRole::PodcastTasteReflection);
    let mut missing = Vec::new();
    if !non_empty(&config.podcast_taste_path) {
        missing.push("PODCAST_TASTE_PATH".to_owned());
    }
    if !non_empty(&config.castro_access_id) {
        missing.push("CASTRO_ACCESS_ID".to_owned());
    }
    if !non_empty(&config.castro_secret_key) {
        missing.push("CASTRO_SECRET_KEY".to_owned());
    }
    if !provider_key(config, &provider).is_some_and(|(_, present)| present) {
        missing.push(format!("{} model credential", provider.to_uppercase()));
    }
    missing
}

/// Builds the podcast subsystem from the app context.
pub fn subsystem(ctx: &AppContext) -> Result<Subsystem, BootError> {
    let config = ctx.config.clone();
    let tz = time_zone(&config);
    let accounts: Arc<dyn AccountProvider> = match CastroAccounts::from_config(
        &config,
        ctx.public_http.clone(),
        ctx.clock.clone(),
        ctx.side_effects,
    ) {
        Some(castro) => Arc::new(castro),
        None => Arc::new(NoAccount),
    };
    let models = Models {
        ai: ctx.ai.clone(),
        config: config.clone(),
    };

    let mut tasks: Vec<Arc<dyn Task>> = Vec::new();
    match podcast_recs_disabled_reason(&config) {
        Some(reason) => tracing::info!(target: LOG, "Podcast recommendations disabled: {reason}"),
        None => {
            let person_search: Option<Arc<dyn PersonSearch>> = PodcastIndexClient::from_config(
                &config,
                ctx.public_http.clone(),
                ctx.clock.clone(),
            )
            .map(|client| Arc::new(client) as Arc<dyn PersonSearch>);
            let deps = PodcastDeps {
                store: ctx.store.clone(),
                clock: ctx.clock.clone(),
                config: config.clone(),
                tz: tz.clone(),
                models: models.clone(),
                notifier: Arc::new(PushoverNotifier(ctx.pushover.clone())),
                accounts: accounts.clone(),
                directory: Arc::new(HttpDirectory {
                    http: ctx.public_http.clone(),
                    tz: tz.clone(),
                }),
                web: Arc::new(WebSearch::new(
                    ctx.public_http.clone(),
                    config.tavily_api_key.clone().unwrap_or_default(),
                    ctx.costs.clone(),
                )),
                person_search,
            };
            tasks.push(Arc::new(PodcastRecommendationTask::new(
                schedule("PodcastRecs", &config.podcast_recs_schedule, &tz)?,
                deps,
            )));
        }
    }

    if non_empty(&config.castro_access_id) && non_empty(&config.castro_secret_key) {
        tasks.push(Arc::new(CastroInboxCleanupTask::new(
            schedule("CastroInboxCleanup", castro::cleanup::SCHEDULE, &tz)?,
            accounts.clone(),
        )));
    } else {
        tracing::info!(
            target: LOG,
            "Castro inbox cleanup disabled: missing CASTRO_ACCESS_ID/CASTRO_SECRET_KEY"
        );
    }

    let missing = taste_reflection_missing(&config);
    if missing.is_empty() {
        tasks.push(Arc::new(PodcastTasteReflectionTask::new(
            schedule(
                "PodcastTasteReflection",
                &config.podcast_taste_reflection_schedule,
                &tz,
            )?,
            ctx.store.clone(),
            ctx.clock.clone(),
            models,
            accounts.clone(),
        )));
    } else {
        tracing::info!(
            target: LOG,
            "Podcast taste reflection disabled: missing {}",
            missing.join(", ")
        );
    }

    let mcp_tools = mcp::tools(mcp::McpState {
        store: ctx.store.clone(),
        clock: ctx.clock.clone(),
        accounts,
        directory: Arc::new(HttpDirectory {
            http: ctx.public_http.clone(),
            tz: tz.clone(),
        }),
        public_url: config.recs_public_url.clone(),
        exa: exa::ExaPodcasts::from_config(&config, ctx.public_http.clone(), ctx.costs.clone())
            .map(Arc::new),
    })
    .map_err(|e| BootError::new("podcast MCP tools", e.to_string()))?;

    Ok(Subsystem {
        name: "podcasts",
        router: routes::router(routes::RoutesState {
            store: ctx.store.clone(),
            clock: ctx.clock.clone(),
            tasks: ctx.tasks.clone(),
        }),
        tasks,
        mcp_tools,
        entities: entities(),
        managed_entities: managed_entities(),
        alert_gates: vec![Arc::new(CastroFailureGate::new(ctx.store.clone()))],
        ..Subsystem::named("podcasts")
    })
}
