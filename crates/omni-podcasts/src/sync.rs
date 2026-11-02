//! Deterministic podcast recommendation data sync: passive outcome labels
//! from listen history and taste evidence capture. No model, no selection,
//! no notification and no queueing.
//!
//! The live `PodcastRecs` and `PodcastTasteReflection` tasks call these
//! functions today. [`PodcastRecommendationSyncTask`] runs the same steps on
//! its own and is deliberately not scheduled while those tasks are live, so
//! no state is synced twice; it takes over their data work when the model
//! tasks retire.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::clock::SharedClock;
use omni_store::{Store, StoreError};
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};

use crate::account::{AccountProvider, ListenedEpisode, PodcastAccount};
use crate::log_file::{self, LogFile, LogFileError};
use crate::outcomes::{PodcastOutcomeChange, decide_episode_outcomes};
use crate::persistence::{
    PodcastRecommendationData, get_all_podcast_recommendations, get_open_podcast_recommendations,
    update_podcast_recommendation,
};
use crate::reflection::evidence::{derive_listen_evidence, derive_recommendation_evidence};
use crate::reflection::store::insert_podcast_taste_evidence;

const LOG: &str = "PodcastRecsTask";
pub const TASK_NAME: &str = "PodcastRecommendationSync";
/// Cushion before the oldest open delivery, for timestamp skew.
const LISTEN_HISTORY_BUFFER_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    LogFile(#[from] LogFileError),
}

/// Look-back cutoff: just before the oldest open delivery, never capped
/// shorter (that would hide real playback and mislabel it ignored).
pub fn listen_history_since(open: &[PodcastRecommendationData], now: i64) -> i64 {
    open.iter()
        .map(|rec| rec.notified_at.unwrap_or(rec.recommended_at))
        .min()
        .map_or(now, |oldest| oldest - LISTEN_HISTORY_BUFFER_MS)
}

/// Applies outcome labels to notified rows; pending rows are never labeled.
pub async fn apply_outcomes(
    store: &Store,
    open: &[PodcastRecommendationData],
    history: &[ListenedEpisode],
    now: i64,
    log_file: Option<&LogFile>,
) -> Result<Vec<PodcastOutcomeChange>, SyncError> {
    let changes = decide_episode_outcomes(open, history, now);
    for change in &changes {
        let status = change.status;
        update_podcast_recommendation(store, &change.recommendation_id, move |mut rec| {
            rec.status = status;
            rec.resolved_at = Some(now);
            rec
        })
        .await?;
        tracing::info!(
            target: LOG,
            "Outcome: {} → {} ({})",
            change.episode_id,
            change.status.as_str(),
            change.reason
        );
    }
    if !changes.is_empty() {
        let lines = changes
            .iter()
            .map(|c| format!("- {} → {} ({})", c.episode_id, c.status.as_str(), c.reason))
            .collect::<Vec<_>>()
            .join("\n");
        log_file::section(log_file, "Outcome Sync", &lines).await?;
    }
    Ok(changes)
}

/// Reads listen history since the oldest open delivery and labels outcomes.
/// Unavailable history skips labeling and degrades the run.
pub async fn sync_outcomes(
    store: &Store,
    account: Option<&dyn PodcastAccount>,
    now: i64,
    log_file: Option<&LogFile>,
) -> Result<usize, SyncError> {
    let Some(account) = account else {
        return Ok(0);
    };
    let open = get_open_podcast_recommendations(store).await?;
    // Listen history is only needed to label open recommendations.
    if open.is_empty() {
        return Ok(0);
    }
    let history = match account
        .fetch_listen_history(Some(listen_history_since(&open, now)))
        .await
    {
        Ok(history) => history,
        Err(e) => {
            tracing::warn!(target: LOG, "Listen history unavailable ({}); skipping outcomes", e.reason);
            omni_tasks::report_degraded(format!(
                "listen history unavailable, outcomes not synced: {}",
                e.reason
            ));
            return Ok(0);
        }
    };
    Ok(apply_outcomes(store, &open, &history, now, log_file)
        .await?
        .len())
}

/// Appends listen and recommendation evidence; existing observations are
/// never rewritten. Returns how many rows were new.
pub async fn capture_taste_evidence(
    store: &Store,
    listened: &[ListenedEpisode],
    recommendations: &[PodcastRecommendationData],
) -> Result<usize, StoreError> {
    let mut incoming = derive_listen_evidence(listened);
    incoming.extend(derive_recommendation_evidence(recommendations));
    insert_podcast_taste_evidence(store, incoming).await
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncSummary {
    Skipped {
        reason: String,
    },
    Synced {
        outcomes: usize,
        evidence_inserted: usize,
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

/// One deterministic pass: the full listen history labels open rows and
/// feeds the evidence log.
pub async fn run_podcast_recommendation_sync(
    store: &Store,
    accounts: &dyn AccountProvider,
    now: i64,
) -> Result<SyncSummary, SyncError> {
    let Some(account) = accounts.resolve() else {
        return Ok(SyncSummary::Skipped {
            reason: "no podcast account client".to_owned(),
        });
    };
    let history = match account.fetch_listen_history(None).await {
        Ok(history) => history,
        Err(e) => {
            tracing::warn!(target: LOG, "Podcast recommendation sync skipped: {}", e.reason);
            omni_tasks::report_degraded(format!("listen history unavailable: {}", e.reason));
            return Ok(SyncSummary::Skipped { reason: e.reason });
        }
    };
    let open = get_open_podcast_recommendations(store).await?;
    let outcomes = apply_outcomes(store, &open, &history, now, None)
        .await?
        .len();
    let recommendations = get_all_podcast_recommendations(store).await?;
    let evidence_inserted = capture_taste_evidence(store, &history, &recommendations).await?;
    Ok(SyncSummary::Synced {
        outcomes,
        evidence_inserted,
    })
}

/// The data half of the podcast recommendation and taste tasks, without a model.
pub struct PodcastRecommendationSyncTask {
    schedule: CronSchedule,
    store: Store,
    clock: SharedClock,
    accounts: Arc<dyn AccountProvider>,
    last_summary: Mutex<Option<String>>,
}

impl PodcastRecommendationSyncTask {
    pub fn new(
        schedule: CronSchedule,
        store: Store,
        clock: SharedClock,
        accounts: Arc<dyn AccountProvider>,
    ) -> Self {
        Self {
            schedule,
            store,
            clock,
            accounts,
            last_summary: Mutex::new(None),
        }
    }

    pub async fn run_once(&self) -> Result<SyncSummary, SyncError> {
        let summary = run_podcast_recommendation_sync(
            &self.store,
            self.accounts.as_ref(),
            self.clock.now_ms(),
        )
        .await?;
        tracing::info!(target: LOG, "Podcast recommendation sync finished: {}", summary.line());
        *self.last_summary.lock().unwrap_or_else(|p| p.into_inner()) = Some(summary.line());
        Ok(summary)
    }
}

impl Task for PodcastRecommendationSyncTask {
    fn name(&self) -> &str {
        TASK_NAME
    }

    fn display_name(&self) -> Option<&str> {
        Some("Podcast Recommendation Sync")
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        TaskOptions {
            jitter: Duration::from_secs(5 * 60),
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
        self.last_summary
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}
