//! Deterministic recommendation data sync: identity resolution, passive
//! outcome labels and taste evidence capture. No model, no selection, no
//! notification and no acquisition.
//!
//! The live `Recommendations` and `TasteReflection` tasks call these
//! functions today. [`MediaRecommendationSyncTask`] runs the same steps on
//! its own and is deliberately not scheduled while those tasks are live, so
//! no state is synced twice; it takes over their data work when the model
//! tasks retire.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use futures::StreamExt;
use futures::future::BoxFuture;
use omni_api::media::RecommendationStatus;
use omni_store::Store;
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};

use crate::error::RecommendationError;
use crate::history::completed_watches;
use crate::identity::resolve_identity;
use crate::outcomes::{OutcomeChange, OutcomeInputs, ProgressState, WatchedState, decide_outcomes};
use crate::persistence::{
    Patch, RecommendationData, get_all_recommendations, get_open_recommendations,
    patch_recommendation_in,
};
use crate::run_log::{self, RunLogFile};
use crate::services::{MediaServices, configured};
use crate::taste::{
    CanonicalWatchObservation, derive_recommendation_evidence, derive_watch_evidence,
    insert_taste_evidence,
};
use crate::types::{FetchResult, InProgressItem, MediaItem, WatchedItem, canonical_tmdb_id};

const LOG: &str = "Main:RecsTask";
const RESOLVE_CONCURRENCY: usize = 4;
const METADATA_CONCURRENCY: usize = 6;
/// Full (network-fallback) resolution is reserved for the most recent watches.
pub const FULL_RESOLUTION_HISTORY_LIMIT: usize = 60;
const MAX_WATCH_EVIDENCE: usize = 160;
pub const TASK_NAME: &str = "RecommendationSync";

fn persistence(
    operation: &'static str,
) -> impl FnOnce(omni_store::StoreError) -> RecommendationError {
    RecommendationError::persistence(operation)
}

/// Resolves each distinct GUID once; only confident resolutions map.
pub async fn resolve_many(
    services: &MediaServices,
    items: &[&MediaItem],
    full_resolution: &HashSet<&str>,
) -> Result<HashMap<String, String>, RecommendationError> {
    let mut seen = HashSet::new();
    let unique: Vec<&MediaItem> = items
        .iter()
        .copied()
        .filter(|item| seen.insert(item.guid.as_str()))
        .collect();
    let resolutions: Vec<Result<Option<(String, String)>, RecommendationError>> =
        futures::stream::iter(
            unique
                .into_iter()
                .map(|item| async move {
                    let resolution = resolve_identity(
                        &services.store,
                        services.catalog.as_ref(),
                        item,
                        full_resolution.contains(item.guid.as_str()),
                    )
                    .await
                    .map_err(persistence("resolve media identity"))?;
                    Ok(resolution
                        .confident_id()
                        .map(|id| (item.guid.clone(), id.to_owned())))
                })
                .collect::<Vec<_>>(),
        )
        .buffer_unordered(RESOLVE_CONCURRENCY)
        .collect()
        .await;
    let mut resolved = HashMap::new();
    for resolution in resolutions {
        if let Some((guid, id)) = resolution? {
            resolved.insert(guid, id);
        }
    }
    Ok(resolved)
}

/// Watch and progress state keyed by canonical id.
pub fn outcome_inputs(
    history: &[WatchedItem],
    in_progress: &[InProgressItem],
    canonical_by_guid: &HashMap<String, String>,
    now: i64,
) -> OutcomeInputs {
    let mut watched: HashMap<String, WatchedState> = HashMap::new();
    for item in history {
        let Some(id) = canonical_by_guid.get(&item.item.guid) else {
            continue;
        };
        let prior = watched.get(id).copied();
        if prior.is_none_or(|p| p.last_viewed_at.is_none_or(|at| item.viewed_at > at)) {
            watched.insert(
                id.clone(),
                WatchedState {
                    completion: item.completion,
                    view_count: prior.map_or(0, |p| p.view_count).max(item.view_count),
                    last_viewed_at: Some(item.viewed_at),
                },
            );
        }
    }
    let mut progress: HashMap<String, ProgressState> = HashMap::new();
    for item in in_progress {
        if let Some(id) = canonical_by_guid.get(&item.item.guid) {
            progress.insert(
                id.clone(),
                ProgressState {
                    progress: item.progress,
                    last_viewed_at: Some(item.last_viewed_at),
                },
            );
        }
    }
    OutcomeInputs {
        watched,
        in_progress: progress,
        in_progress_available: true,
        now,
    }
}

/// The first playback after delivery of a notified row (`startedAt`).
/// Pending rows were never presented, so playback cannot be attributed to
/// them.
pub fn started_at(rec: &RecommendationData, inputs: &OutcomeInputs) -> Option<i64> {
    if rec.status() != RecommendationStatus::Notified || rec.started_at.is_some_and(|at| at != 0) {
        return None;
    }
    let delivered_at = rec.delivered_at();
    let history = inputs.watched.get(&rec.canonical_id);
    let watched_after =
        history.is_some_and(|h| h.last_viewed_at.is_none_or(|at| at >= delivered_at));
    let progress = inputs.in_progress.get(&rec.canonical_id);
    let progress_after =
        progress.is_some_and(|p| p.last_viewed_at.is_none_or(|at| at >= delivered_at));
    if !(watched_after || progress_after) {
        return None;
    }
    let observed_at = if watched_after {
        history.and_then(|h| h.last_viewed_at)
    } else {
        progress.and_then(|p| p.last_viewed_at)
    };
    Some(delivered_at.max(observed_at.unwrap_or(inputs.now)))
}

/// Records the first passive playback signal, then applies outcome labels.
pub async fn sync_outcomes(
    store: &Store,
    inputs: &OutcomeInputs,
    log_file: Option<&RunLogFile>,
) -> Result<Vec<OutcomeChange>, RecommendationError> {
    let mut open = get_open_recommendations(store)
        .await
        .map_err(persistence("sync recommendation outcomes"))?;
    for rec in &mut open {
        if let Some(started_at) = started_at(rec, inputs) {
            patch_recommendation_in(
                store,
                &rec.recommendation_id,
                Patch::new().int("startedAt", started_at),
            )
            .await
            .map_err(persistence("sync recommendation outcomes"))?;
            rec.started_at = Some(started_at);
        }
    }
    let changes = decide_outcomes(&open, inputs);
    for change in &changes {
        patch_recommendation_in(
            store,
            &change.recommendation_id,
            Patch::new()
                .str("status", change.status.as_str())
                .int("resolvedAt", inputs.now),
        )
        .await
        .map_err(persistence("sync recommendation outcomes"))?;
        tracing::info!(
            target: LOG,
            "Outcome: {} → {} ({})",
            change.canonical_id,
            change.status.as_str(),
            change.reason
        );
    }
    if !changes.is_empty() {
        let lines = changes
            .iter()
            .map(|c| {
                format!(
                    "- {} → {} ({})",
                    c.canonical_id,
                    c.status.as_str(),
                    c.reason
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        run_log::section(log_file, "Outcome Sync", &lines).await?;
    }
    Ok(changes)
}

/// Up to 160 distinct completed watches, confidently resolved (network
/// allowed), each with TMDB details when they load.
pub async fn build_canonical_watch_evidence(
    services: &MediaServices,
    history: &[WatchedItem],
) -> Result<Vec<CanonicalWatchObservation>, RecommendationError> {
    let mut seen = HashSet::new();
    let unique: Vec<WatchedItem> = completed_watches(history)
        .into_iter()
        .filter(|item| seen.insert(item.item.guid.clone()))
        .take(MAX_WATCH_EVIDENCE)
        .collect();
    let resolved: Vec<Result<Option<(String, WatchedItem)>, RecommendationError>> =
        futures::stream::iter(
            unique
                .into_iter()
                .map(|item| async move {
                    let resolution = resolve_identity(
                        &services.store,
                        services.catalog.as_ref(),
                        &item.item,
                        true,
                    )
                    .await
                    .map_err(persistence("resolve media identity"))?;
                    Ok(resolution
                        .confident_id()
                        .map(|id| (id.to_owned(), item.clone())))
                })
                .collect::<Vec<_>>(),
        )
        .buffered(RESOLVE_CONCURRENCY)
        .collect()
        .await;
    let mut confident = Vec::new();
    for entry in resolved {
        if let Some(pair) = entry? {
            confident.push(pair);
        }
    }
    Ok(futures::stream::iter(
        confident
            .into_iter()
            .map(|(canonical_id, item)| async move {
                let metadata = match canonical_tmdb_id(&canonical_id) {
                    Some(tmdb_id) => match services
                        .catalog
                        .title_details(item.item.media_type, tmdb_id)
                        .await
                    {
                        Ok(details) => Some(details),
                        Err(error) => {
                            tracing::warn!(
                                target: LOG,
                                error = error.cause_message(),
                                "Taste metadata lookup failed for {canonical_id}"
                            );
                            None
                        }
                    },
                    None => None,
                };
                CanonicalWatchObservation {
                    canonical_id,
                    item,
                    metadata,
                }
            })
            .collect::<Vec<_>>(),
    )
    .buffered(METADATA_CONCURRENCY)
    .collect()
    .await)
}

/// Appends watch and recommendation evidence; existing observations are
/// never rewritten. Returns how many rows were new.
pub async fn capture_taste_evidence(
    store: &Store,
    watched: &[CanonicalWatchObservation],
    recommendations: &[RecommendationData],
) -> Result<u64, RecommendationError> {
    let mut incoming = derive_watch_evidence(watched);
    incoming.extend(derive_recommendation_evidence(recommendations));
    insert_taste_evidence(store, incoming)
        .await
        .map_err(persistence("insert taste evidence"))
}

/// What one sync pass did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncSummary {
    /// Local state was unreadable; nothing was labeled.
    Skipped { reason: String },
    Synced {
        outcomes: usize,
        evidence_inserted: u64,
    },
}

impl SyncSummary {
    pub fn line(&self) -> String {
        match self {
            Self::Skipped { reason } => format!("skipped: {reason}"),
            Self::Synced {
                outcomes,
                evidence_inserted,
            } => format!("{outcomes} outcome label(s), {evidence_inserted} new evidence row(s)"),
        }
    }
}

/// One deterministic pass over local state: outcomes, then evidence.
pub async fn run_recommendation_sync(
    services: &MediaServices,
) -> Result<SyncSummary, RecommendationError> {
    let (history, in_progress, library, watchlist) = futures::join!(
        services.library.watch_history(),
        services.library.in_progress(),
        services.library.library_index(),
        services.watchlist.fetch()
    );
    let skipped = |reason: String| {
        tracing::warn!(target: LOG, "Recommendation sync skipped: {reason}");
        omni_tasks::report_degraded(format!("media state unavailable: {reason}"));
        Ok(SyncSummary::Skipped { reason })
    };
    let history = match history {
        FetchResult::Ok(history) => history,
        FetchResult::Unavailable { reason } => return skipped(reason),
    };
    let in_progress = match in_progress {
        FetchResult::Ok(in_progress) => in_progress,
        FetchResult::Unavailable { reason } => return skipped(reason),
    };
    let library = match library {
        FetchResult::Ok(library) => library,
        FetchResult::Unavailable { reason } => return skipped(reason),
    };
    let watchlist = match watchlist {
        FetchResult::Ok(watchlist) => watchlist,
        FetchResult::Unavailable { reason } => return skipped(reason),
    };

    let recent: Vec<WatchedItem> = completed_watches(&history)
        .into_iter()
        .take(FULL_RESOLUTION_HISTORY_LIMIT)
        .collect();
    let full_resolution: HashSet<&str> = recent
        .iter()
        .map(|w| w.item.guid.as_str())
        .chain(watchlist.iter().map(|w| w.guid.as_str()))
        .chain(in_progress.iter().map(|w| w.item.guid.as_str()))
        .collect();
    let all_items: Vec<&MediaItem> = history
        .iter()
        .map(|w| &w.item)
        .chain(in_progress.iter().map(|w| &w.item))
        .chain(watchlist.iter())
        .chain(library.iter())
        .collect();
    let canonical_by_guid = resolve_many(services, &all_items, &full_resolution).await?;
    let inputs = outcome_inputs(&history, &in_progress, &canonical_by_guid, services.now());
    let outcomes = sync_outcomes(&services.store, &inputs, None).await?.len();

    let watched = build_canonical_watch_evidence(services, &history).await?;
    let recommendations = get_all_recommendations(&services.store)
        .await
        .map_err(persistence("read recommendations"))?;
    let evidence_inserted =
        capture_taste_evidence(&services.store, &watched, &recommendations).await?;
    Ok(SyncSummary::Synced {
        outcomes,
        evidence_inserted,
    })
}

/// Settings without which the sync cannot read local state.
pub fn missing_settings(services: &MediaServices) -> Vec<String> {
    let config = &services.config;
    [
        ("TMDB_API_KEY", config.tmdb_api_key.as_deref()),
        ("PLEX_URL", config.plex_url.as_deref()),
        ("PLEX_TOKEN", config.plex_token.as_deref()),
        ("RADARR_URL", config.radarr_url.as_deref()),
        ("RADARR_API_KEY", config.radarr_api_key.as_deref()),
        ("SONARR_URL", config.sonarr_url.as_deref()),
        ("SONARR_API_KEY", config.sonarr_api_key.as_deref()),
    ]
    .into_iter()
    .filter(|(_, value)| !configured(*value))
    .map(|(name, _)| name.to_owned())
    .collect()
}

/// The data half of the recommendation and taste tasks, without a model.
pub struct MediaRecommendationSyncTask {
    services: MediaServices,
    schedule: CronSchedule,
    last_run_summary: Mutex<Option<String>>,
}

impl MediaRecommendationSyncTask {
    /// `None` (logged) when a required setting is missing.
    pub fn create(services: MediaServices, schedule: CronSchedule) -> Option<Self> {
        let missing = missing_settings(&services);
        if !missing.is_empty() {
            tracing::info!(
                target: "Main",
                "Recommendation sync disabled: missing {}",
                missing.join(", ")
            );
            return None;
        }
        Some(Self {
            services,
            schedule,
            last_run_summary: Mutex::new(None),
        })
    }

    pub async fn run_once(&self) -> Result<SyncSummary, RecommendationError> {
        let summary = run_recommendation_sync(&self.services).await?;
        tracing::info!(target: LOG, "Recommendation sync finished: {}", summary.line());
        if let Ok(mut last) = self.last_run_summary.lock() {
            *last = Some(summary.line());
        }
        Ok(summary)
    }
}

impl Task for MediaRecommendationSyncTask {
    fn name(&self) -> &str {
        TASK_NAME
    }

    fn display_name(&self) -> Option<&str> {
        Some("Recommendation Sync")
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        TaskOptions {
            jitter: std::time::Duration::ZERO,
            run_on_startup: false,
        }
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            self.run_once()
                .await
                .map(|_| ())
                .map_err(TaskError::from_error)
        })
    }

    fn last_run_summary(&self) -> Option<String> {
        self.last_run_summary.lock().ok().and_then(|s| s.clone())
    }
}
