//! The recommendation pipeline.
//!
//! Order of operations: pull local state (any unavailable view aborts the
//! run), resolve identities, sync passive outcomes, reconcile stale pending
//! rows, build taste inputs, pool and filter candidates, shortlist, research
//! once, then select and commit one title at a time. A commit records the
//! pending row before the external write and reserves the notification
//! before Pushover, so a crash is reconciled instead of re-sent.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use omni_ai::ModelRole;
use omni_api::media::{RecommendationStatus, WatchlistResult};
use omni_core::clock::utc_date_stamp;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};

use crate::candidates::{
    TARGET_POOL_SIZE, WatchSeed, assemble_pool, enrich_candidates, fetch_candidate_buckets,
};
use crate::error::{IntegrationError, RecommendationError, max_recommendations_message};
use crate::filters::{FilterContext, filter_eligible};
use crate::history::{completed_watches, format_history_digest};
use crate::js::slice_utf16;
use crate::persistence::{
    NotificationState, Patch, RecommendationData, ShortlistScoresData, format_feedback_digest,
    get_excluded_canonical_ids, patch_recommendation_in,
};
use crate::record::record_exclusions;
use crate::run_log::{self, RunLogFile};
use crate::selection::{Decision, SelectionPick, research_finalists, select_recommendation};
use crate::services::{MediaServices, RecommendationPush};
use crate::shortlist::{FINALIST_COUNT, ScoredCandidate, shortlist_candidates};
use crate::sync::{FULL_RESOLUTION_HISTORY_LIMIT, outcome_inputs, resolve_many, sync_outcomes};
use crate::taste::{format_taste_profile_digest, get_latest_taste_profile};
use crate::types::{
    AddToWatchlistResult, Candidate, ExternalIds, FetchResult, MediaItem, WatchedItem,
    canonical_tmdb_id,
};
use crate::watchlist::WatchlistAddRequest;

const LOG: &str = "Main:RecsTask";
/// A pending row older than this from a previous run needs reconciliation.
const STALE_PENDING_MS: i64 = 60 * 60 * 1000;
pub const MAX_RECOMMENDATIONS_PER_RUN: u32 = 10;
const FINALISTS_PER_REQUESTED_PICK: usize = 2;
const SEED_LIMIT: usize = 20;
const RATE_THIS_PICK: &str = "Rate this pick";

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PipelineOptions {
    pub dry_run: bool,
    /// Integer 1..=10; default 1.
    pub max_recommendations: Option<f64>,
}

/// Validates a requested batch size (`maxRecommendations`).
pub fn validate_max_recommendations(value: f64) -> Result<usize, RecommendationError> {
    let max = f64::from(MAX_RECOMMENDATIONS_PER_RUN);
    if value.is_finite() && value.fract() == 0.0 && (1.0..=max).contains(&value) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(value as usize)
    } else {
        Err(RecommendationError::Input(max_recommendations_message(
            MAX_RECOMMENDATIONS_PER_RUN,
        )))
    }
}

fn persistence(
    operation: &'static str,
) -> impl FnOnce(omni_store::StoreError) -> RecommendationError {
    RecommendationError::persistence(operation)
}

/// Runs the full pipeline and returns a one-line summary.
pub async fn run_recommendation_pipeline(
    services: &MediaServices,
    log_file: Option<&RunLogFile>,
    options: PipelineOptions,
) -> Result<String, RecommendationError> {
    let max_recommendations =
        validate_max_recommendations(options.max_recommendations.unwrap_or(1.0))?;

    // 1. Local state: never recommend (or label outcomes) against missing state.
    let (history, in_progress, library, watchlist) = futures::join!(
        services.library.watch_history(),
        services.library.in_progress(),
        services.library.library_index(),
        services.watchlist.fetch()
    );
    let history = match history {
        FetchResult::Ok(history) => history,
        FetchResult::Unavailable { reason } => return Ok(skipped(&reason)),
    };
    let watchlist = match watchlist {
        FetchResult::Ok(watchlist) => watchlist,
        FetchResult::Unavailable { reason } => return Ok(skipped(&reason)),
    };
    let in_progress = match in_progress {
        FetchResult::Ok(in_progress) => in_progress,
        FetchResult::Unavailable { reason } => return Ok(skipped(&reason)),
    };
    let library = match library {
        FetchResult::Ok(library) => library,
        FetchResult::Unavailable { reason } => return Ok(skipped(&reason)),
    };

    // 2. Canonical identities.
    let recent_watched: Vec<WatchedItem> = completed_watches(&history)
        .into_iter()
        .take(FULL_RESOLUTION_HISTORY_LIMIT)
        .collect();
    let full_resolution: HashSet<&str> = recent_watched
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
    run_log::section(
        log_file,
        "Identity Resolution",
        &format!(
            "{}/{} items resolved",
            canonical_by_guid.len(),
            all_items.len()
        ),
    )
    .await?;

    // 3. Outcome bookkeeping for open recommendations.
    let outcome_sync_now = services.now();
    let inputs = outcome_inputs(&history, &in_progress, &canonical_by_guid, outcome_sync_now);
    let mut watchlist_ids: HashSet<String> = HashSet::new();
    let mut watchlist_unresolved = 0usize;
    for item in &watchlist {
        match canonical_by_guid.get(&item.guid) {
            Some(id) => {
                watchlist_ids.insert(id.clone());
            }
            None => watchlist_unresolved += 1,
        }
    }
    // Incomplete Arr identity resolution cannot safely reconcile pending writes.
    let watchlist_complete = watchlist_unresolved == 0;
    if !watchlist_complete {
        tracing::warn!(
            target: LOG,
            "{watchlist_unresolved} watchlist item(s) unresolved; skipping absence-based outcome labels"
        );
    }
    sync_outcomes(&services.store, &inputs, log_file).await?;
    reconcile_stale_pending(services, &watchlist_ids, watchlist_complete).await?;

    // 4. Taste inputs: ground-truth history plus explicit feedback.
    let feedback_digest = format_feedback_digest(&services.store)
        .await
        .map_err(persistence("read recommendation feedback"))?;
    let taste_profile = get_latest_taste_profile(&services.store)
        .await
        .map_err(persistence("read media taste profile"))?;
    let taste_digest = format_taste_profile_digest(taste_profile.as_ref().map(|p| &p.profile));
    let history_digest = format!(
        "{}\n\n{feedback_digest}\n\n{taste_digest}",
        format_history_digest(&history, &in_progress)
    );
    let seeds = build_seeds(services, &recent_watched, &canonical_by_guid).await;

    // 5. Candidate pool.
    let buckets = fetch_candidate_buckets(services.catalog.as_ref(), &seeds).await;
    let pool = assemble_pool(&buckets, TARGET_POOL_SIZE);

    // 6. Hard filters before any model call. Watched exclusion is best-effort
    //    for old history whose GUIDs carry no TMDB id (cheap resolution only).
    let watched_ids: HashSet<String> = history
        .iter()
        .filter_map(|item| canonical_by_guid.get(&item.item.guid).cloned())
        .collect();
    let library_ids: HashSet<String> = library
        .iter()
        .filter_map(|item| canonical_by_guid.get(&item.guid).cloned())
        .collect();
    let excluded = get_excluded_canonical_ids(&services.store, outcome_sync_now)
        .await
        .map_err(persistence("read recommendation exclusions"))?;
    let outcome = filter_eligible(
        &pool,
        &FilterContext {
            watched_ids,
            in_progress_ids: inputs.in_progress.keys().cloned().collect(),
            watchlist_ids,
            excluded_recommendation_ids: excluded,
        },
    );
    tracing::info!(
        target: LOG,
        "Candidates: {} pooled, {} eligible",
        pool.len(),
        outcome.kept.len()
    );
    if !outcome.dropped.is_empty() {
        let lines = outcome
            .dropped
            .iter()
            .map(|d| format!("- {}: {}", d.title, d.reason))
            .collect::<Vec<_>>()
            .join("\n");
        run_log::section(log_file, "Filtered Out", &lines).await?;
    }
    if outcome.kept.is_empty() {
        return Ok("no eligible candidates after filtering".to_owned());
    }

    // 7. Cheap-model shortlist.
    let candidates =
        enrich_candidates(services.catalog.as_ref(), &outcome.kept, &library_ids).await?;
    let shortlist_model = services
        .ai
        .model_for(&services.config, ModelRole::RecsShortlist)
        .map_err(|e| IntegrationError::from_error("resolve recommendation shortlist model", &e))?;
    let finalists = shortlist_candidates(
        &services.ai,
        shortlist_model.as_ref(),
        &candidates,
        &history_digest,
        log_file,
        FINALIST_COUNT.max(max_recommendations * FINALISTS_PER_REQUESTED_PICK),
    )
    .await?;
    if finalists.is_empty() {
        return Ok("shortlist returned no scorable candidates".to_owned());
    }

    // 8. Research once, then select repeatedly from the shrinking set.
    let research = research_finalists(services.research.as_ref(), &finalists, log_file).await?;
    let selection_model = services
        .ai
        .model_for(&services.config, ModelRole::RecsSelection)
        .map_err(|e| IntegrationError::from_error("resolve recommendation selection model", &e))?;
    let mut remaining: IndexMap<String, ScoredCandidate> = finalists
        .into_iter()
        .map(|f| (f.candidate.canonical_id.clone(), f))
        .collect();
    let mut recommended: Vec<Candidate> = Vec::new();
    let mut stop_reason: Option<String> = None;

    while recommended.len() < max_recommendations && !remaining.is_empty() {
        let current: Vec<ScoredCandidate> = remaining.values().cloned().collect();
        let decision = select_recommendation(
            &services.ai,
            selection_model.as_ref(),
            &current,
            &history_digest,
            &research,
            log_file,
        )
        .await?;
        let selected_pick = match (&decision.decision, &decision.selected) {
            (Decision::Select, Some(pick)) => pick.clone(),
            _ => {
                let reason = decision
                    .no_add_reason
                    .clone()
                    .unwrap_or_else(|| "no reason given".to_owned());
                tracing::info!(target: LOG, "No further recommendation today: {reason}");
                stop_reason = Some(format!("no_add: {}", slice_utf16(&reason, 120)));
                break;
            }
        };
        let Some(selected) = remaining.shift_remove(&selected_pick.candidate_id) else {
            tracing::warn!(
                target: LOG,
                "Selection returned unknown candidate id: {}",
                selected_pick.candidate_id
            );
            stop_reason = Some("selection returned an unknown candidate id".to_owned());
            omni_tasks::report_degraded("selection returned an unknown candidate id");
            break;
        };

        if options.dry_run {
            recommended.push(selected.candidate);
            continue;
        }

        // 9. Commit: pending row before the external write.
        match commit_recommendation(services, &selected, &selected_pick, false).await? {
            CommitResult::Committed => {
                recommended.push(selected.candidate);
                continue;
            }
            CommitResult::Failed => return Err(commit_failed()),
            CommitResult::AlreadyExists => {}
        }

        let backup = decision.backup.as_ref().and_then(|pick| {
            (pick.candidate_id != selected.candidate.canonical_id)
                .then(|| {
                    remaining
                        .shift_remove(&pick.candidate_id)
                        .map(|b| (pick, b))
                })
                .flatten()
        });
        if let Some((backup_pick, backup)) = backup {
            tracing::info!(
                target: LOG,
                "Primary already on watchlist; promoting backup {}",
                backup.candidate.title
            );
            match commit_recommendation(services, &backup, backup_pick, true).await? {
                CommitResult::Committed => {
                    recommended.push(backup.candidate);
                    continue;
                }
                CommitResult::Failed => return Err(commit_failed()),
                CommitResult::AlreadyExists => {}
            }
        }
        stop_reason = Some("no_add: selected and backup are already tracked".to_owned());
        break;
    }

    Ok(format_batch_summary(
        &recommended,
        max_recommendations,
        options.dry_run,
        stop_reason.as_deref(),
    ))
}

/// Local state was unreadable: the run skips its work and is degraded.
fn skipped(reason: &str) -> String {
    tracing::warn!(target: LOG, "Recommendation run skipped: {reason}");
    omni_tasks::report_degraded(format!("media state unavailable: {reason}"));
    format!("skipped: {reason}")
}

fn commit_failed() -> RecommendationError {
    RecommendationError::Commit("Recommendation acquisition or notification failed".to_owned())
}

fn push_title(rec: &RecommendationData) -> String {
    let year = rec
        .year
        .filter(|y| *y != 0)
        .map(|y| format!(" ({y})"))
        .unwrap_or_default();
    format!("🎬 {}{year}", rec.title)
}

/// Repairs rows a crash left pending: when the acquisition demonstrably
/// landed, deliver the missed notification once (never when a reservation
/// shows an earlier attempt); otherwise fail the row when the watchlist view
/// is complete, or leave it for the next run.
async fn reconcile_stale_pending(
    services: &MediaServices,
    watchlist_ids: &HashSet<String>,
    watchlist_complete: bool,
) -> Result<(), RecommendationError> {
    let now = services.now();
    let stale: Vec<RecommendationData> = services
        .store
        .read(|docs| docs.get_all::<RecommendationData>())
        .await
        .map_err(persistence("read stale recommendations"))?
        .into_iter()
        // Assistant-recorded rows settle only through a finish with a receipt, or a
        // withdraw while still reserved: this repair would fail or re-notify an unknown send.
        .filter(|r| {
            r.is_legacy()
                && r.status() == RecommendationStatus::Pending
                && now - r.recommended_at > STALE_PENDING_MS
        })
        .collect();
    for rec in stale {
        let landed = rec.watchlist_result == Some(WatchlistResult::Available)
            || watchlist_ids.contains(&rec.canonical_id);
        let why = rec.why_for_user.clone().filter(|w| !w.is_empty());
        match why {
            Some(message) if landed => {
                if matches!(
                    rec.notification_state,
                    Some(NotificationState::Reserved | NotificationState::Sent)
                ) {
                    tracing::warn!(
                        target: LOG,
                        "Reconciling pending recommendation {}: notification attempt already reserved",
                        rec.canonical_id
                    );
                    let state = if rec.notification_state == Some(NotificationState::Reserved) {
                        NotificationState::Unknown
                    } else {
                        NotificationState::Sent
                    };
                    patch_recommendation_in(
                        &services.store,
                        &rec.recommendation_id,
                        Patch::new()
                            .str("status", RecommendationStatus::Notified.as_str())
                            .str("notificationState", state.as_str())
                            .int("notifiedAt", rec.notification_reserved_at.unwrap_or(now)),
                    )
                    .await
                    .map_err(persistence(
                        "acknowledge reserved recommendation notification",
                    ))?;
                    continue;
                }
                tracing::warn!(
                    target: LOG,
                    "Reconciling pending recommendation {}: re-notifying",
                    rec.canonical_id
                );
                let reserved_at = now;
                patch_recommendation_in(
                    &services.store,
                    &rec.recommendation_id,
                    Patch::new()
                        .str("notificationState", NotificationState::Reserved.as_str())
                        .int("notificationReservedAt", reserved_at),
                )
                .await
                .map_err(persistence(
                    "reserve reconciled recommendation notification",
                ))?;
                let sent = services
                    .notifier
                    .notify(RecommendationPush {
                        title: push_title(&rec),
                        message,
                        url: services.feedback_url(&rec.recommendation_id),
                        url_title: RATE_THIS_PICK.to_owned(),
                    })
                    .await;
                if let Err(error) = sent {
                    patch_recommendation_in(
                        &services.store,
                        &rec.recommendation_id,
                        Patch::new().str("notificationState", NotificationState::Failed.as_str()),
                    )
                    .await
                    .map_err(persistence(
                        "record reconciled recommendation notification failure",
                    ))?;
                    tracing::warn!(
                        target: LOG,
                        error = %error,
                        "Reconciled notification failed for {}",
                        rec.canonical_id
                    );
                    continue;
                }
                patch_recommendation_in(
                    &services.store,
                    &rec.recommendation_id,
                    Patch::new()
                        .str("status", RecommendationStatus::Notified.as_str())
                        .str("notificationState", NotificationState::Sent.as_str())
                        .int("notifiedAt", reserved_at),
                )
                .await
                .map_err(persistence("mark reconciled recommendation notified"))?;
            }
            _ if watchlist_complete => {
                tracing::warn!(
                    target: LOG,
                    "Marking stale pending recommendation {} as failed",
                    rec.canonical_id
                );
                patch_recommendation_in(
                    &services.store,
                    &rec.recommendation_id,
                    Patch::new()
                        .str("status", RecommendationStatus::Failed.as_str())
                        .int("resolvedAt", now),
                )
                .await
                .map_err(persistence("mark stale recommendation failed"))?;
            }
            _ => {
                // The watchlist view is incomplete, so absence proves nothing.
                tracing::warn!(
                    target: LOG,
                    "Leaving stale pending {} unreconciled (watchlist incomplete)",
                    rec.canonical_id
                );
            }
        }
    }
    Ok(())
}

/// Up to 20 seeds from recent completed watches (sequential genre lookups).
async fn build_seeds(
    services: &MediaServices,
    recent_watched: &[WatchedItem],
    canonical_by_guid: &HashMap<String, String>,
) -> Vec<WatchSeed> {
    let mut seeds = Vec::new();
    for item in recent_watched {
        let Some(canonical_id) = canonical_by_guid.get(&item.item.guid) else {
            continue;
        };
        let Some(tmdb_id) = canonical_tmdb_id(canonical_id) else {
            continue;
        };
        let genre_ids = match services
            .catalog
            .title_genre_ids(item.item.media_type, tmdb_id)
            .await
        {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(
                    target: LOG,
                    error = error.cause_message(),
                    "Genre lookup failed for {canonical_id}"
                );
                Vec::new()
            }
        };
        seeds.push(WatchSeed {
            canonical_id: canonical_id.clone(),
            tmdb_id,
            media_type: item.item.media_type,
            genre_ids,
        });
        if seeds.len() >= SEED_LIMIT {
            break;
        }
    }
    seeds
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitResult {
    Committed,
    AlreadyExists,
    Failed,
}

/// Pending row → acquisition → reservation → Pushover → notified.
async fn commit_recommendation(
    services: &MediaServices,
    scored: &ScoredCandidate,
    pick: &SelectionPick,
    was_backup: bool,
) -> Result<CommitResult, RecommendationError> {
    let now = services.now();
    let candidate = &scored.candidate;
    let recommendation_id = omni_core::ids::uuid_v4();
    let pending = RecommendationData {
        recommendation_id: recommendation_id.clone(),
        canonical_id: candidate.canonical_id.clone(),
        tmdb_id: candidate.tmdb_id,
        media_type: candidate.media_type,
        title: candidate.title.clone(),
        year: candidate.year,
        poster_path: candidate.poster_path.clone(),
        status: RecommendationStatus::Pending,
        why_for_user: Some(pick.why_for_user.clone()),
        caveats: Some(pick.caveats.clone()),
        confidence: Some(pick.confidence),
        source: Some(candidate.source),
        genres: Some(candidate.genres.clone()),
        runtime_minutes: candidate.runtime_minutes,
        season_count: candidate.season_count,
        episode_count: candidate.episode_count,
        series_status: candidate.series_status.clone(),
        original_language: candidate.original_language.clone(),
        origin_countries: candidate.origin_countries.clone(),
        creators: candidate.creators.clone(),
        cast: candidate.cast.clone(),
        keywords: candidate.keywords.clone(),
        certification: candidate.certification.clone(),
        shortlist_scores: Some(ShortlistScoresData {
            taste_match: scored.taste_match,
            novelty: scored.novelty,
            effort_fit: scored.effort_fit,
            composite: scored.composite,
            risks: scored.risks.clone(),
            extra: Default::default(),
        }),
        run_date: utc_date_stamp(now),
        recommended_at: now,
        was_backup: Some(was_backup),
        ..RecommendationData::default()
    };
    // An assistant may have reserved this title since the run read its
    // exclusions; the check and the insert share one transaction.
    let inserted = services
        .store
        .write(move |tx| {
            let assistant: Vec<RecommendationData> = tx
                .get_all::<RecommendationData>()?
                .into_iter()
                .filter(|r| !r.is_legacy())
                .collect();
            if !record_exclusions(&assistant, &pending.canonical_id, now).is_empty() {
                return Ok(false);
            }
            tx.upsert(&pending, UpsertOpts::default())?;
            Ok(true)
        })
        .await
        .map_err(persistence("insert pending recommendation"))?;
    if !inserted {
        tracing::warn!(
            target: LOG,
            "{} was reserved by an assistant during this run",
            candidate.title
        );
        return Ok(CommitResult::AlreadyExists);
    }

    let (add_result, manager_slug) = if candidate.in_library {
        (None, None)
    } else {
        let outcome = services
            .watchlist
            .add(&WatchlistAddRequest {
                tmdb_id: candidate.tmdb_id,
                media_type: candidate.media_type,
                title: candidate.title.clone(),
                year: candidate.year,
                external_ids: Some(ExternalIds {
                    tmdb: Some(candidate.tmdb_id),
                    ..ExternalIds::default()
                }),
            })
            .await;
        (
            Some(outcome.result),
            outcome.title_slug.filter(|slug| !slug.is_empty()),
        )
    };
    let with_slug = |patch: Patch| match &manager_slug {
        Some(slug) => patch.str("managerSlug", slug),
        None => patch,
    };

    let watchlist_result = match add_result {
        None => WatchlistResult::Available,
        Some(AddToWatchlistResult::Added) => WatchlistResult::Added,
        Some(AddToWatchlistResult::AlreadyExists) => {
            tracing::warn!(target: LOG, "{} is already tracked", candidate.title);
            patch_recommendation_in(
                &services.store,
                &recommendation_id,
                with_slug(
                    Patch::new()
                        .str("status", RecommendationStatus::Failed.as_str())
                        .str("watchlistResult", WatchlistResult::AlreadyExists.as_str())
                        .int("resolvedAt", now),
                ),
            )
            .await
            .map_err(persistence("close already tracked recommendation"))?;
            return Ok(CommitResult::AlreadyExists);
        }
        Some(other) => {
            tracing::warn!(
                target: LOG,
                "Acquisition failed for {} ({})",
                candidate.title,
                other.as_str()
            );
            patch_recommendation_in(
                &services.store,
                &recommendation_id,
                Patch::new()
                    .str("status", RecommendationStatus::Failed.as_str())
                    .str("watchlistResult", WatchlistResult::Error.as_str())
                    .int("resolvedAt", now),
            )
            .await
            .map_err(persistence("mark recommendation acquisition failed"))?;
            return Ok(CommitResult::Failed);
        }
    };

    patch_recommendation_in(
        &services.store,
        &recommendation_id,
        with_slug(Patch::new().str("watchlistResult", watchlist_result.as_str())),
    )
    .await
    .map_err(persistence("record recommendation acquisition"))?;

    let reserved_at = now;
    patch_recommendation_in(
        &services.store,
        &recommendation_id,
        Patch::new()
            .str("notificationState", NotificationState::Reserved.as_str())
            .int("notificationReservedAt", reserved_at),
    )
    .await
    .map_err(persistence("reserve recommendation notification"))?;
    let sent = services
        .notifier
        .notify(RecommendationPush {
            title: pick.notification.title.clone(),
            message: pick.notification.message.clone(),
            url: services.feedback_url(&recommendation_id),
            url_title: RATE_THIS_PICK.to_owned(),
        })
        .await;
    if let Err(error) = sent {
        tracing::error!(
            target: LOG,
            error = %error,
            "Notification failed for {}",
            candidate.title
        );
        patch_recommendation_in(
            &services.store,
            &recommendation_id,
            Patch::new().str("notificationState", NotificationState::Failed.as_str()),
        )
        .await
        .map_err(persistence("record recommendation notification failure"))?;
        return Ok(CommitResult::Failed);
    }

    patch_recommendation_in(
        &services.store,
        &recommendation_id,
        Patch::new()
            .str("status", RecommendationStatus::Notified.as_str())
            .int("notifiedAt", reserved_at)
            .str("notificationState", NotificationState::Sent.as_str())
            .str("watchlistResult", watchlist_result.as_str()),
    )
    .await
    .map_err(persistence("mark recommendation notified"))?;
    tracing::info!(
        target: LOG,
        "Recommended {} (acquisition: {})",
        candidate.title,
        watchlist_result.as_str()
    );
    Ok(CommitResult::Committed)
}

fn format_title(candidate: &Candidate) -> String {
    match candidate.year.filter(|y| *y != 0) {
        Some(year) => format!("{} ({year})", candidate.title),
        None => candidate.title.clone(),
    }
}

fn format_batch_summary(
    recommended: &[Candidate],
    requested: usize,
    dry_run: bool,
    stop_reason: Option<&str>,
) -> String {
    if recommended.is_empty() {
        return stop_reason
            .unwrap_or("no_add: no remaining finalists")
            .to_owned();
    }
    let titles = recommended
        .iter()
        .map(format_title)
        .collect::<Vec<_>>()
        .join(", ");
    if dry_run {
        return if requested == 1 {
            format!("dry_run: would recommend {titles}")
        } else {
            format!(
                "dry_run: would recommend {}/{requested}: {titles}",
                recommended.len()
            )
        };
    }
    if requested == 1 {
        return format!("recommended: {titles}");
    }
    let stopped = stop_reason
        .map(|reason| format!("; stopped: {reason}"))
        .unwrap_or_default();
    format!(
        "recommended {}/{requested}: {titles}{stopped}",
        recommended.len()
    )
}
