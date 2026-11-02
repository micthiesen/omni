//! The two-tier recommendation run:
//!
//! - Tier 1: episodes where a followed voice guests somewhere new
//!   (default-include, capped by `PODCAST_MAX_GUEST_PICKS`).
//! - Tier 2: standout topic episodes (conservative fill).
//!
//! Commit sequence per pick: pending row → Castro enqueue → durable
//! `queueResult` → Pushover → `notified`. A failed notification fails the
//! run and leaves the row pending (reconciled to failed an hour later).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::BoxFuture;
use jiff::tz::TimeZone;
use omni_alerts::{Pushover, PushoverChannel, PushoverMessage};
use omni_config::Config;
use omni_core::clock::{SharedClock, utc_date_stamp};
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};

use crate::account::{
    AccountProvider, EnqueueEpisodeRequest, PodcastAccount, PodcastQueuePosition,
    PodcastWriteResult,
};
use crate::candidates::resolve_candidates;
use crate::discovery::discover_episodes;
use crate::filters::{PodcastFilterContext, PodcastFilterResult, filter_eligible_episodes};
use crate::guest_selection::select_guest_appearances;
use crate::guests::{GuestSources, discover_guest_appearances};
use crate::log_file::{self, LogFile, LogFileError};
use crate::models::Models;
use crate::persistence::{
    PodcastQueueResult, PodcastRecommendationData, PodcastRecommendationStatus, ShortlistScores,
    advance_voice_cursor, format_recent_recommendations_digest_from,
    get_all_podcast_recommendations, get_podcast_exclusions, next_voice_batch,
    update_podcast_recommendation,
};
use crate::podcastindex::PersonSearch;
use crate::record::{Identity as RecordIdentity, record_exclusions};
use crate::selection::{DecisionKind, PodcastSelectionPick, research_finalists, select_episode};
use crate::shortlist::{FINALIST_COUNT, ScoredEpisode, shortlist_episodes};
use crate::sources::{ShowDirectory, WebSearcher};
use crate::subscriptions::resolve_subscriptions;
use crate::sync::{SyncError, sync_outcomes};
use crate::taste::{TasteSeedError, build_taste_digest, load_taste_seed};
use crate::types::EpisodeCandidate;
use crate::voices::parse_voices;

const LOG: &str = "PodcastRecsTask";
/// A pending row older than this from a previous run needs reconciliation.
const STALE_PENDING_MS: i64 = 60 * 60 * 1000;
pub const MAX_PODCAST_RECOMMENDATIONS_PER_RUN: i64 = 5;
const FINALISTS_PER_REQUESTED_PICK: usize = 2;

/// The message is the cause's message.
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    LogFile(#[from] LogFileError),
    #[error(transparent)]
    TasteSeed(#[from] TasteSeedError),
    #[error("{operation} failed: {detail}")]
    Model {
        operation: &'static str,
        detail: String,
    },
    #[error("Podcast recommendation notification failed")]
    NotificationFailed,
}

impl From<SyncError> for PipelineError {
    fn from(error: SyncError) -> Self {
        match error {
            SyncError::Store(e) => PipelineError::Store(e),
            SyncError::LogFile(e) => PipelineError::LogFile(e),
        }
    }
}

impl PipelineError {
    pub fn model(operation: &'static str) -> impl FnOnce(omni_ai::AiError) -> Self {
        move |e| PipelineError::Model {
            operation,
            detail: e.to_string(),
        }
    }
}

pub fn range_error() -> String {
    format!("maxRecommendations must be an integer from 1 to {MAX_PODCAST_RECOMMENDATIONS_PER_RUN}")
}

/// Sends the recommendation notification (Pushover in production).
pub trait Notifier: Send + Sync {
    fn notify(&self, message: PushoverMessage) -> BoxFuture<'_, Result<(), String>>;
}

/// Pushover on the podcast channel; a skipped or recorded send is success.
pub struct PushoverNotifier(pub Pushover);

impl Notifier for PushoverNotifier {
    fn notify(&self, message: PushoverMessage) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.0
                .send(PushoverChannel::Podcast, message)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }
}

/// Everything a run touches.
#[derive(Clone)]
pub struct PodcastDeps {
    pub store: Store,
    pub clock: SharedClock,
    pub config: Arc<Config>,
    pub tz: TimeZone,
    pub models: Models,
    pub notifier: Arc<dyn Notifier>,
    pub accounts: Arc<dyn AccountProvider>,
    pub directory: Arc<dyn ShowDirectory>,
    pub web: Arc<dyn WebSearcher>,
    pub person_search: Option<Arc<dyn PersonSearch>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PodcastPipelineOptions {
    pub dry_run: bool,
    /// Cap on Tier-2 picks (default 2); Tier-1 uses its own cap.
    pub max_recommendations: Option<i64>,
}

/// One-line run summary.
pub async fn run_podcast_pipeline(
    deps: &PodcastDeps,
    log_file: Option<&LogFile>,
    options: PodcastPipelineOptions,
) -> Result<String, PipelineError> {
    let topic_target = options.max_recommendations.unwrap_or(2);
    if !(1..=MAX_PODCAST_RECOMMENDATIONS_PER_RUN).contains(&topic_target) {
        return Err(PipelineError::Invalid(range_error()));
    }
    let topic_target = usize::try_from(topic_target).unwrap_or(1);
    let store = &deps.store;

    // 1. Local state; a configured account whose read fails aborts.
    let account = deps.accounts.resolve();
    let account_ref: Option<&dyn PodcastAccount> = account.as_deref();
    let subscriptions = match resolve_subscriptions(account_ref).await {
        Ok(state) => state,
        Err(e) => {
            tracing::warn!(target: LOG, "Podcast recommendation run skipped: {}", e.reason);
            omni_tasks::report_degraded(e.reason.clone());
            return Ok(format!("skipped: {}", e.reason));
        }
    };

    // 2. Outcome sync + stale-pending reconciliation.
    sync_outcomes(store, account_ref, deps.clock.now_ms(), log_file).await?;
    let now = deps.clock.now_ms();
    reconcile_stale_pending(store, now).await?;

    // 3. Taste inputs.
    let taste_seed = load_taste_seed(deps.config.podcast_taste_path.as_deref()).await?;
    let taste_digest = build_taste_digest(store, &subscriptions, &taste_seed).await?;
    let recent_digest = format_recent_recommendations_digest_from(
        &get_all_podcast_recommendations(store).await?,
        15,
    );
    let exclusions = get_podcast_exclusions(store, now).await?;
    let context = PodcastFilterContext {
        now,
        subscribed_show_ids: &subscriptions.show_ids,
        subscribed_show_titles: &subscriptions.normalized_titles,
        exclusions: &exclusions,
    };

    // 4a. Tier 1: guest appearances of followed voices (rotated batch).
    let all_voices = parse_voices(&taste_seed);
    let rotation = deps.config.podcast_voice_rotation_max as usize;
    let voices = next_voice_batch(store, &all_voices, rotation).await?;
    let sources = GuestSources {
        person_search: deps.person_search.as_deref(),
        web: deps.web.as_ref(),
        directory: deps.directory.as_ref(),
        account: account_ref,
        models: &deps.models,
    };
    let guest_pool = discover_guest_appearances(&sources, &voices, now, log_file).await?;
    advance_voice_cursor(store, &all_voices, rotation).await?;
    let guest_eligible = log_filtered(
        filter_eligible_episodes(guest_pool, &context),
        "Guest filtered out",
        log_file,
    )
    .await?;

    // 4b. Tier 2: topic discovery.
    let topic_discovered = discover_episodes(
        deps.web.as_ref(),
        &deps.models,
        &taste_digest,
        &recent_digest,
        log_file,
    )
    .await?;
    let topic_pool = if topic_discovered.is_empty() {
        Vec::new()
    } else {
        resolve_candidates(
            &topic_discovered,
            account_ref,
            deps.directory.as_ref(),
            log_file,
        )
        .await?
    };
    let guest_ids: HashSet<String> = guest_eligible
        .iter()
        .map(|c| c.episode_id.clone())
        .collect();
    let topic_eligible: Vec<EpisodeCandidate> = log_filtered(
        filter_eligible_episodes(topic_pool, &context),
        "Topic filtered out",
        log_file,
    )
    .await?
    .into_iter()
    .filter(|c| !guest_ids.contains(&c.episode_id))
    .collect();
    tracing::info!(
        target: LOG,
        "Eligible: {} guest, {} topic",
        guest_eligible.len(),
        topic_eligible.len()
    );
    if guest_eligible.is_empty() && topic_eligible.is_empty() {
        return Ok("no eligible candidates after filtering".to_owned());
    }

    let mut recommended: Vec<EpisodeCandidate> = Vec::new();

    // 5. Tier 1: gate and commit.
    if !guest_eligible.is_empty() {
        let max_guest = deps.config.podcast_max_guest_picks as usize;
        let picks = select_guest_appearances(
            &deps.models,
            &guest_eligible,
            &taste_digest,
            log_file,
            max_guest,
        )
        .await?;
        for guest in picks {
            if commit_or_collect(
                deps,
                account_ref,
                &guest.candidate,
                &guest.pick,
                None,
                options.dry_run,
            )
            .await?
            {
                recommended.push(guest.candidate);
            }
        }
    }
    let guest_count = recommended.len();

    // 6. Tier 2: exclude shows Tier 1 just recommended (their cooldown is not
    // in `exclusions` yet).
    let guest_show_ids: HashSet<String> = recommended.iter().map(|c| c.show_id.clone()).collect();
    let topic_remaining: Vec<EpisodeCandidate> = topic_eligible
        .into_iter()
        .filter(|c| !guest_show_ids.contains(&c.show_id))
        .collect();
    let mut stop_reason: Option<String> = None;
    if topic_target > 0 && !topic_remaining.is_empty() {
        let finalists = shortlist_episodes(
            &deps.models,
            &topic_remaining,
            &taste_digest,
            log_file,
            FINALIST_COUNT.max(topic_target * FINALISTS_PER_REQUESTED_PICK),
        )
        .await?;
        let research = if finalists.is_empty() {
            HashMap::new()
        } else {
            research_finalists(deps.web.as_ref(), &finalists, log_file).await?
        };
        let mut remaining = unique_finalists(&finalists);
        while recommended.len() - guest_count < topic_target && !remaining.is_empty() {
            let decision =
                select_episode(&deps.models, &remaining, &taste_digest, &research, log_file)
                    .await?;
            let selected = match (decision.decision, decision.selected) {
                (DecisionKind::Select, Some(selected)) => selected,
                (_, _) => {
                    let reason = decision
                        .no_add_reason
                        .unwrap_or_else(|| "no reason given".to_owned());
                    stop_reason = Some(format!(
                        "no_add: {}",
                        omni_core::js::utf16_slice(&reason, 0, 120)
                    ));
                    break;
                }
            };
            let Some(position) = remaining
                .iter()
                .position(|f| f.candidate.episode_id == selected.candidate_id)
            else {
                stop_reason = Some("selection returned an unknown candidate id".to_owned());
                omni_tasks::report_degraded("selection returned an unknown candidate id");
                break;
            };
            let finalist = remaining.remove(position);
            let scores = ShortlistScores {
                taste_match: finalist.taste_match,
                novelty: finalist.novelty,
                composite: finalist.composite,
                risks: finalist.risks.clone(),
                extra: Extra::default(),
            };
            if commit_or_collect(
                deps,
                account_ref,
                &finalist.candidate,
                &selected,
                Some(scores),
                options.dry_run,
            )
            .await?
            {
                recommended.push(finalist.candidate.clone());
            }
        }
    }

    Ok(format_batch_summary(
        &recommended,
        guest_count,
        options.dry_run,
        stop_reason.as_deref(),
    ))
}

/// `new Map(finalists.map(f => [episodeId, f]))`: one entry per episode, at
/// its first position with its last score, so a model that scores an episode
/// twice can never get it selected (and committed, and enqueued) twice.
pub fn unique_finalists(finalists: &[ScoredEpisode]) -> Vec<&ScoredEpisode> {
    let mut unique: Vec<&ScoredEpisode> = Vec::new();
    for finalist in finalists {
        match unique
            .iter_mut()
            .find(|f| f.candidate.episode_id == finalist.candidate.episode_id)
        {
            Some(slot) => *slot = finalist,
            None => unique.push(finalist),
        }
    }
    unique
}

async fn commit_or_collect(
    deps: &PodcastDeps,
    account: Option<&dyn PodcastAccount>,
    candidate: &EpisodeCandidate,
    pick: &PodcastSelectionPick,
    scores: Option<ShortlistScores>,
    dry_run: bool,
) -> Result<bool, PipelineError> {
    if dry_run {
        return Ok(true);
    }
    match commit(deps, account, candidate, pick, scores).await? {
        Commit::Notified => Ok(true),
        Commit::ReservedElsewhere => Ok(false),
        Commit::NotificationFailed => Err(PipelineError::NotificationFailed),
    }
}

enum Commit {
    Notified,
    /// An assistant reserved the episode or its show during this run.
    ReservedElsewhere,
    /// The row stays pending.
    NotificationFailed,
}

async fn log_filtered(
    result: PodcastFilterResult,
    section: &str,
    log_file: Option<&LogFile>,
) -> Result<Vec<EpisodeCandidate>, PipelineError> {
    if !result.dropped.is_empty() {
        let lines = result
            .dropped
            .iter()
            .map(|d| {
                format!(
                    "- {} — {}: {}",
                    d.candidate.show_title, d.candidate.episode_title, d.reason
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        log_file::section(log_file, section, &lines).await?;
    }
    Ok(result.kept)
}

pub use crate::sync::listen_history_since;

/// Rows left pending by a crash mid-commit become failed (a 24 h retry
/// exclusion); notification delivery cannot be verified after the fact.
async fn reconcile_stale_pending(store: &Store, now: i64) -> Result<(), StoreError> {
    let stale: Vec<PodcastRecommendationData> = get_all_podcast_recommendations(store)
        .await?
        .into_iter()
        // Assistant-recorded rows settle only through their own finish or
        // withdraw: failing one here would erase an unknown chat send.
        .filter(|r| {
            r.is_legacy()
                && r.status == PodcastRecommendationStatus::Pending
                && now - r.recommended_at > STALE_PENDING_MS
        })
        .collect();
    for rec in stale {
        tracing::warn!(
            target: LOG,
            "Marking stale pending podcast recommendation as failed: {} — {}",
            rec.show_title,
            rec.episode_title
        );
        update_podcast_recommendation(store, &rec.recommendation_id, move |mut row| {
            row.status = PodcastRecommendationStatus::Failed;
            row.resolved_at = Some(now);
            row
        })
        .await?;
    }
    Ok(())
}

async fn commit(
    deps: &PodcastDeps,
    account: Option<&dyn PodcastAccount>,
    candidate: &EpisodeCandidate,
    pick: &PodcastSelectionPick,
    shortlist_scores: Option<ShortlistScores>,
) -> Result<Commit, PipelineError> {
    let store = &deps.store;
    let recommendation_id = omni_core::ids::uuid_v4();
    let recommended_at = deps.clock.now_ms();
    let identity = RecordIdentity {
        episode_id: candidate.episode_id.clone(),
        show_id: candidate.show_id.clone(),
        show_title: candidate.show_title.clone(),
        episode_title: candidate.episode_title.clone(),
        feed_url: candidate.feed_url.clone(),
        itunes_id: candidate.itunes_id,
        episode_guid: candidate.episode_guid.clone(),
        media_url: candidate.media_url.clone(),
        episode_url: candidate.episode_url.clone(),
        published_at: candidate.published_at,
        duration_minutes: candidate.duration_minutes,
    };
    let row = PodcastRecommendationData {
        recommendation_id: recommendation_id.clone(),
        episode_id: candidate.episode_id.clone(),
        show_id: candidate.show_id.clone(),
        show_title: candidate.show_title.clone(),
        episode_title: candidate.episode_title.clone(),
        feed_url: candidate.feed_url.clone(),
        itunes_id: candidate.itunes_id,
        artwork_url: candidate.artwork_url.clone(),
        episode_guid: candidate.episode_guid.clone(),
        media_url: candidate.media_url.clone(),
        episode_url: candidate.episode_url.clone(),
        published_at: candidate.published_at,
        duration_minutes: candidate.duration_minutes,
        status: PodcastRecommendationStatus::Pending,
        why_for_user: Some(pick.why_for_user.clone()),
        caveats: Some(pick.caveats.clone()),
        confidence: Some(pick.confidence),
        show_genres: Some(candidate.show_genres.clone()),
        discovered_via: Some(candidate.discovered_via.clone()),
        source_url: candidate.source_url.clone(),
        matched_voices: candidate.matched_voices.clone(),
        shortlist_scores,
        run_date: utc_date_stamp(recommended_at),
        recommended_at,
        notified_at: None,
        queue_result: None,
        resolved_at: None,
        feedback: None,
        feedback_at: None,
        feedback_note: None,
        origin: None,
        chat_record: None,
        extra: Extra::default(),
    };
    // An assistant may have reserved this episode or show since the run read
    // its exclusions; the check and the insert share one transaction.
    let inserted = store
        .write(move |tx| {
            let assistant: Vec<PodcastRecommendationData> = tx
                .get_all::<PodcastRecommendationData>()?
                .into_iter()
                .filter(|r| !r.is_legacy())
                .collect();
            if !record_exclusions(&assistant, &identity, recommended_at).is_empty() {
                return Ok::<_, StoreError>(false);
            }
            tx.upsert(&row, UpsertOpts::default())?;
            Ok(true)
        })
        .await?;
    if !inserted {
        tracing::warn!(
            target: LOG,
            "{} — {} was reserved by an assistant during this run",
            candidate.show_title,
            candidate.episode_title
        );
        return Ok(Commit::ReservedElsewhere);
    }

    let enqueue_result = match account {
        // Top of the queue: a curated pick should be one tap to play.
        Some(account) => Some(
            account
                .enqueue_episode(EnqueueEpisodeRequest {
                    feed_url: candidate.feed_url.clone(),
                    itunes_id: candidate.itunes_id,
                    episode_guid: candidate.episode_guid.clone(),
                    media_url: candidate.media_url.clone(),
                    show_title: candidate.show_title.clone(),
                    episode_title: candidate.episode_title.clone(),
                    position: Some(PodcastQueuePosition::Next),
                })
                .await,
        ),
        None => None,
    };
    let queue_result = enqueue_and_persist(enqueue_result, |result| {
        let id = recommendation_id.clone();
        async move {
            update_podcast_recommendation(store, &id, move |mut rec| {
                rec.queue_result = Some(result);
                rec
            })
            .await
            .map(|_| ())
        }
    })
    .await?;
    if let Some(write) = enqueue_result {
        if queue_result == PodcastQueueResult::NotQueued {
            tracing::info!(target: LOG, "Castro enqueue {}; continuing with recommendation deep link", write.as_str());
        } else {
            tracing::info!(
                target: LOG,
                "Castro queue {}: {} - {}",
                if queue_result == PodcastQueueResult::Queued { "added" } else { "already contained" },
                candidate.show_title,
                candidate.episode_title
            );
        }
    }

    let message = PushoverMessage {
        title: Some(pick.notification.title.clone()),
        message: append_queue_note(&pick.notification.message, queue_result),
        url: Some(feedback_url(
            &deps.config.recs_public_url,
            &recommendation_id,
        )),
        url_title: Some("Rate this pick".to_owned()),
        ..PushoverMessage::default()
    };
    if let Err(error) = deps.notifier.notify(message).await {
        tracing::error!(target: LOG, error = %error, "Notification failed for {}", candidate.episode_title);
        return Ok(Commit::NotificationFailed);
    }

    let notified_at = deps.clock.now_ms();
    update_podcast_recommendation(store, &recommendation_id, move |mut rec| {
        rec.status = PodcastRecommendationStatus::Notified;
        rec.notified_at = Some(notified_at);
        rec.queue_result = Some(queue_result);
        rec
    })
    .await?;
    tracing::info!(target: LOG, "Recommended {} — {}", candidate.show_title, candidate.episode_title);
    Ok(Commit::Notified)
}

/// Records the enqueue outcome durably before notification is attempted.
pub async fn enqueue_and_persist<F, Fut>(
    enqueue_result: Option<PodcastWriteResult>,
    persist: F,
) -> Result<PodcastQueueResult, StoreError>
where
    F: FnOnce(PodcastQueueResult) -> Fut,
    Fut: std::future::Future<Output = Result<(), StoreError>>,
{
    let queue_result = enqueue_result.map_or(PodcastQueueResult::NotQueued, to_queue_result);
    persist(queue_result).await?;
    Ok(queue_result)
}

pub fn to_queue_result(write: PodcastWriteResult) -> PodcastQueueResult {
    match write {
        PodcastWriteResult::Added => PodcastQueueResult::Queued,
        PodcastWriteResult::AlreadyExists => PodcastQueueResult::AlreadyQueued,
        _ => PodcastQueueResult::NotQueued,
    }
}

fn append_queue_note(message: &str, queue_result: PodcastQueueResult) -> String {
    if queue_result == PodcastQueueResult::NotQueued {
        message.to_owned()
    } else {
        format!("{message}\n\n🎧 Added to your Castro queue.")
    }
}

/// One-tap rating page (`feedbackUrl("podcasts", id)`).
pub fn feedback_url(public_url: &str, recommendation_id: &str) -> String {
    let base = public_url.strip_suffix('/').unwrap_or(public_url);
    format!(
        "{base}/feedback/podcasts/{}",
        omni_core::js::encode_uri_component(recommendation_id)
    )
}

fn format_batch_summary(
    recommended: &[EpisodeCandidate],
    guest_count: usize,
    dry_run: bool,
    stop_reason: Option<&str>,
) -> String {
    if recommended.is_empty() {
        return stop_reason.unwrap_or("no eligible picks").to_owned();
    }
    let titles = recommended
        .iter()
        .map(|c| format!("{} — {}", c.show_title, c.episode_title))
        .collect::<Vec<_>>()
        .join(", ");
    let breakdown = format!(
        "{guest_count} guest, {} topic",
        recommended.len() - guest_count
    );
    let prefix = if dry_run {
        "dry_run: would recommend"
    } else {
        "recommended"
    };
    let stopped = stop_reason
        .map(|r| format!("; stopped: {r}"))
        .unwrap_or_default();
    format!(
        "{prefix} {} ({breakdown}): {titles}{stopped}",
        recommended.len()
    )
}
