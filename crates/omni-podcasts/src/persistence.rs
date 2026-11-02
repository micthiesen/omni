//! Recommendation attempts, exclusions, feedback, and the voice rotation
//! cursor.

use std::collections::{HashMap, HashSet};

use omni_core::clock::utc_date_stamp;
use omni_runtime::chat_record::{ChatRecord, RecommendationOrigin};
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps as _, EntityWrite as _, ModifyOpts, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};

use crate::types::{CanonicalEpisodeId, CanonicalShowId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PodcastRecommendationStatus {
    /// Row written, notification not yet confirmed.
    Pending,
    /// Notification sent; awaiting an outcome.
    Notified,
    /// Listened past the completion threshold.
    Listened,
    /// Started but bailed below the completion threshold.
    Abandoned,
    /// No engagement within the ignore window.
    Ignored,
    /// Run died between the pending write and notification.
    Failed,
}

impl PodcastRecommendationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Notified => "notified",
            Self::Listened => "listened",
            Self::Abandoned => "abandoned",
            Self::Ignored => "ignored",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PodcastFeedback {
    GoodPick,
    NotForMe,
}

/// Castro auto-enqueue outcome at commit time; `NotQueued` collapses every
/// non-success (the notification deep link is the fallback).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PodcastQueueResult {
    Queued,
    AlreadyQueued,
    NotQueued,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortlistScores {
    pub taste_match: f64,
    pub novelty: f64,
    pub composite: f64,
    pub risks: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `podcast-recommendation-attempt`, keyed by `recommendationId`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodcastRecommendationData {
    pub recommendation_id: String,
    pub episode_id: CanonicalEpisodeId,
    pub show_id: CanonicalShowId,
    pub show_title: String,
    pub episode_title: String,
    pub feed_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub itunes_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_url: Option<String>,
    pub episode_guid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_url: Option<String>,
    pub published_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_minutes: Option<i64>,
    pub status: PodcastRecommendationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why_for_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caveats: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Selection-time evidence retained for later audit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_genres: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovered_via: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// Followed voices featured as guests (Tier-1 picks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_voices: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortlist_scores: Option<ShortlistScores>,
    /// UTC date (YYYY-MM-DD) of the producing run.
    pub run_date: String,
    pub recommended_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_result: Option<PodcastQueueResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<PodcastFeedback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_note: Option<String>,
    /// Absent on rows the legacy selection pipeline wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<RecommendationOrigin>,
    /// Delivery bookkeeping of an assistant-recorded recommendation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_record: Option<ChatRecord>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl PodcastRecommendationData {
    /// Written by the legacy selection pipeline, whose reconciler may repair it.
    pub fn is_legacy(&self) -> bool {
        self.origin.is_none()
    }
}

impl Entity for PodcastRecommendationData {
    const NAME: &'static str = "podcast-recommendation-attempt";
    type Key = String;
    fn key(&self) -> String {
        self.recommendation_id.clone()
    }
}

/// `podcast-run-state` singleton: the voice rotation cursor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodcastRunState {
    pub id: String,
    pub voice_cursor: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for PodcastRunState {
    const NAME: &'static str = "podcast-run-state";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

const RUN_STATE_ID: &str = "singleton";
/// Same-show cooldown; episodes themselves are excluded permanently.
pub const SHOW_COOLDOWN_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const FAILED_RETRY_MS: i64 = 24 * 60 * 60 * 1000;

/// Newest first by `recommendedAt` (stable).
pub fn sort_newest_first(records: &mut [PodcastRecommendationData]) {
    records.sort_by_key(|r| std::cmp::Reverse(r.recommended_at));
}

pub async fn get_all_podcast_recommendations(
    store: &Store,
) -> Result<Vec<PodcastRecommendationData>, StoreError> {
    let mut records = store
        .read(|docs| docs.get_all::<PodcastRecommendationData>())
        .await?;
    sort_newest_first(&mut records);
    Ok(records)
}

pub async fn get_podcast_recommendation(
    store: &Store,
    recommendation_id: &str,
) -> Result<Option<PodcastRecommendationData>, StoreError> {
    let id = recommendation_id.to_owned();
    store
        .read(move |docs| docs.get::<PodcastRecommendationData>(&id))
        .await
}

/// Episodes and shows excluded from candidacy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PodcastExclusions {
    /// Every delivered episode (and failed rows inside their retry window).
    pub episode_ids: HashSet<CanonicalEpisodeId>,
    /// Shows on cooldown or excluded permanently by not-for-me feedback.
    pub show_ids: HashSet<CanonicalShowId>,
}

pub fn compute_podcast_exclusions(
    records: &[PodcastRecommendationData],
    now: i64,
) -> PodcastExclusions {
    let mut exclusions = PodcastExclusions::default();
    let mut latest_feedback: HashMap<&str, &PodcastRecommendationData> = HashMap::new();
    for rec in records {
        if rec.status == PodcastRecommendationStatus::Failed {
            if now - rec.recommended_at < FAILED_RETRY_MS {
                exclusions.episode_ids.insert(rec.episode_id.clone());
            }
        } else {
            exclusions.episode_ids.insert(rec.episode_id.clone());
            if now - rec.recommended_at < SHOW_COOLDOWN_MS {
                exclusions.show_ids.insert(rec.show_id.clone());
            }
        }
        if rec.feedback.is_some() {
            let at = rec.feedback_at.unwrap_or(rec.recommended_at);
            let newer = latest_feedback
                .get(rec.show_id.as_str())
                .is_none_or(|prior| at > prior.feedback_at.unwrap_or(prior.recommended_at));
            if newer {
                latest_feedback.insert(&rec.show_id, rec);
            }
        }
    }
    // Latest feedback wins: not-for-me excludes the show permanently.
    for rec in latest_feedback.values() {
        if rec.feedback == Some(PodcastFeedback::NotForMe) {
            exclusions.show_ids.insert(rec.show_id.clone());
        }
    }
    exclusions
}

pub async fn get_podcast_exclusions(
    store: &Store,
    now: i64,
) -> Result<PodcastExclusions, StoreError> {
    let records = store
        .read(|docs| docs.get_all::<PodcastRecommendationData>())
        .await?;
    Ok(compute_podcast_exclusions(&records, now))
}

/// Pending/notified rows without not-for-me feedback (awaiting an outcome).
pub async fn get_open_podcast_recommendations(
    store: &Store,
) -> Result<Vec<PodcastRecommendationData>, StoreError> {
    let records = store
        .read(|docs| docs.get_all::<PodcastRecommendationData>())
        .await?;
    Ok(records
        .into_iter()
        .filter(|r| {
            matches!(
                r.status,
                PodcastRecommendationStatus::Notified | PodcastRecommendationStatus::Pending
            ) && r.feedback != Some(PodcastFeedback::NotForMe)
        })
        .collect())
}

/// Sets `feedbackAt` plus whichever of feedback/note was given. `None` when
/// the row does not exist.
pub async fn set_podcast_recommendation_feedback(
    store: &Store,
    recommendation_id: &str,
    feedback: Option<PodcastFeedback>,
    note: Option<String>,
    now: i64,
) -> Result<Option<PodcastRecommendationData>, StoreError> {
    let id = recommendation_id.to_owned();
    store
        .write(move |tx| {
            tx.update::<PodcastRecommendationData>(
                &id,
                |mut rec| {
                    rec.feedback_at = Some(now);
                    if feedback.is_some() {
                        rec.feedback = feedback;
                    }
                    if note.is_some() {
                        rec.feedback_note = note;
                    }
                    rec
                },
                ModifyOpts::default(),
            )
        })
        .await
}

/// Applies `f` to one row in its own transaction.
pub async fn update_podcast_recommendation(
    store: &Store,
    recommendation_id: &str,
    f: impl FnOnce(PodcastRecommendationData) -> PodcastRecommendationData + Send + 'static,
) -> Result<Option<PodcastRecommendationData>, StoreError> {
    let id = recommendation_id.to_owned();
    store
        .write(move |tx| tx.update::<PodcastRecommendationData>(&id, f, ModifyOpts::default()))
        .await
}

pub async fn insert_podcast_recommendation(
    store: &Store,
    rec: PodcastRecommendationData,
) -> Result<(), StoreError> {
    store
        .write(move |tx| tx.upsert(&rec, UpsertOpts::default()))
        .await
}

/// Latest feedback (or note) per show, grouped by polarity, at most 30 shows.
pub fn format_podcast_feedback_digest_from(input: &[PodcastRecommendationData]) -> String {
    let mut sorted: Vec<&PodcastRecommendationData> = input.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.feedback_at.unwrap_or(r.recommended_at)));
    let mut seen = HashSet::new();
    let records: Vec<&PodcastRecommendationData> = sorted
        .into_iter()
        .filter(|rec| {
            if (rec.feedback.is_none() && rec.feedback_note.as_deref().is_none_or(str::is_empty))
                || seen.contains(&rec.show_id)
            {
                return false;
            }
            seen.insert(rec.show_id.clone());
            true
        })
        .take(30)
        .collect();
    if records.is_empty() {
        return "No explicit podcast feedback yet.".to_owned();
    }
    let good: Vec<String> = records
        .iter()
        .filter(|r| r.feedback == Some(PodcastFeedback::GoodPick))
        .map(|r| feedback_entry(r, true))
        .collect();
    let bad: Vec<String> = records
        .iter()
        .filter(|r| r.feedback == Some(PodcastFeedback::NotForMe))
        .map(|r| feedback_entry(r, false))
        .collect();
    let notes: Vec<String> = records
        .iter()
        .filter(|r| {
            r.feedback.is_none() && r.feedback_note.as_deref().is_some_and(|n| !n.is_empty())
        })
        .map(|r| feedback_entry(r, true))
        .collect();
    let mut lines = vec!["Explicit feedback on past podcast recommendations:".to_owned()];
    if !good.is_empty() {
        lines.push(format!("- Good picks: {}", good.join("; ")));
    }
    if !bad.is_empty() {
        lines.push(format!("- Not for me: {}", bad.join("; ")));
    }
    if !notes.is_empty() {
        lines.push(format!("- Notes (no rating): {}", notes.join("; ")));
    }
    lines.join("\n")
}

fn feedback_entry(rec: &PodcastRecommendationData, include_episode: bool) -> String {
    let base = if include_episode {
        format!("{} — {}", rec.show_title, rec.episode_title)
    } else {
        rec.show_title.clone()
    };
    match rec.feedback_note.as_deref().filter(|n| !n.is_empty()) {
        Some(note) => format!("{base} — note: \"{note}\""),
        None => base,
    }
}

/// Recently recommended episodes for the discovery/selection dedup context;
/// `records` must be newest first.
pub fn format_recent_recommendations_digest_from(
    records: &[PodcastRecommendationData],
    limit: usize,
) -> String {
    let recent: Vec<&PodcastRecommendationData> = records.iter().take(limit).collect();
    if recent.is_empty() {
        return "No podcast episodes recommended yet.".to_owned();
    }
    let mut lines = vec!["Recently recommended episodes (never repeat these):".to_owned()];
    lines.extend(recent.iter().map(|r| {
        format!(
            "- {} — {} ({})",
            r.show_title,
            r.episode_title,
            utc_date_stamp(r.recommended_at)
        )
    }));
    lines.join("\n")
}

/// The batch to person-search now and the cursor to store next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceBatch {
    pub batch: Vec<String>,
    pub next_cursor: i64,
}

/// Pure rotation core.
pub fn compute_voice_batch(voices: &[String], max: usize, cursor: i64) -> VoiceBatch {
    if voices.is_empty() || max == 0 {
        return VoiceBatch {
            batch: Vec::new(),
            next_cursor: cursor,
        };
    }
    if voices.len() <= max {
        return VoiceBatch {
            batch: voices.to_vec(),
            next_cursor: 0,
        };
    }
    let len = i64::try_from(voices.len()).unwrap_or(i64::MAX);
    let start = cursor.rem_euclid(len);
    let batch = (0..max)
        .map(|i| {
            let index = (start + i64::try_from(i).unwrap_or(0)).rem_euclid(len);
            voices[usize::try_from(index).unwrap_or(0)].clone()
        })
        .collect();
    VoiceBatch {
        batch,
        next_cursor: (start + i64::try_from(max).unwrap_or(0)).rem_euclid(len),
    }
}

async fn voice_cursor(store: &Store) -> Result<i64, StoreError> {
    Ok(store
        .read(|docs| docs.get::<PodcastRunState>(&RUN_STATE_ID.to_owned()))
        .await?
        .map_or(0, |state| state.voice_cursor))
}

/// The next batch of up to `max` voices; the cursor is committed separately
/// by [`advance_voice_cursor`] once discovery succeeded.
pub async fn next_voice_batch(
    store: &Store,
    voices: &[String],
    max: usize,
) -> Result<Vec<String>, StoreError> {
    Ok(compute_voice_batch(voices, max, voice_cursor(store).await?).batch)
}

/// Advances the rotation after the selected batch was searched.
pub async fn advance_voice_cursor(
    store: &Store,
    voices: &[String],
    max: usize,
) -> Result<(), StoreError> {
    let cursor = voice_cursor(store).await?;
    let next = compute_voice_batch(voices, max, cursor).next_cursor;
    if voices.len() > max {
        let state = PodcastRunState {
            id: RUN_STATE_ID.to_owned(),
            voice_cursor: next,
            extra: Extra::default(),
        };
        store
            .write(move |tx| tx.upsert(&state, UpsertOpts::default()))
            .await?;
    }
    Ok(())
}
