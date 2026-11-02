//! Recommendation attempts and identity aliases.

use std::collections::{HashMap, HashSet};

use omni_api::media::{RecommendationFeedback, RecommendationStatus, WatchlistResult};
use omni_runtime::chat_record::{ChatRecord, RecommendationOrigin};
use omni_store::cbor::{Extra, JsValue};
use omni_store::entity::{Entity, EntityOps as _, EntityWrite as _, JsObjectPatch, ModifyOpts};
use omni_store::{Store, StoreError, Tx};
use serde::{Deserialize, Serialize};

use crate::types::{CandidateSource, MediaType};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
pub const COOLDOWN_MS: i64 = 180 * DAY_MS;
pub const FAILED_RETRY_MS: i64 = DAY_MS;
pub const ON_DECK_LIMIT: usize = 4;

/// Durable notification attempt state. `reserved` means delivery was
/// attempted but its local acknowledgement is unknown, so reconciliation must
/// never send it again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationState {
    Reserved,
    Sent,
    Failed,
    Unknown,
}

impl NotificationState {
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationState::Reserved => "reserved",
            NotificationState::Sent => "sent",
            NotificationState::Failed => "failed",
            NotificationState::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortlistScoresData {
    pub taste_match: f64,
    pub novelty: f64,
    pub effort_fit: f64,
    pub composite: f64,
    pub risks: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `recs-recommendation-attempt`, keyed by `recommendationId`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecommendationData {
    pub recommendation_id: String,
    pub canonical_id: String,
    pub tmdb_id: i64,
    pub media_type: MediaType,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster_path: Option<String>,
    pub status: RecommendationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why_for_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caveats: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Selection-time evidence retained for audit and reflection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<CandidateSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genres: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_minutes: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season_count: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_count: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_countries: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creators: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cast: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certification: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortlist_scores: Option<ShortlistScoresData>,
    /// UTC date (`YYYY-MM-DD`) of the run that produced this recommendation.
    pub run_date: String,
    pub recommended_at: i64,
    /// True when this was the backup promoted after `already_exists`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub was_backup: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_state: Option<NotificationState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_reserved_at: Option<i64>,
    /// First time Plex showed playback or progress after delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// When a terminal outcome was assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchlist_result: Option<WatchlistResult>,
    /// Sonarr's series slug, captured at add time (UI deep links).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<RecommendationFeedback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_at: Option<i64>,
    /// Free-form note alongside (or instead of) the rating.
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

impl RecommendationData {
    /// Written by the legacy selection pipeline, whose reconciler may repair it.
    pub fn is_legacy(&self) -> bool {
        self.origin.is_none()
    }

    pub fn status(&self) -> RecommendationStatus {
        self.status
    }

    /// `notifiedAt ?? recommendedAt`.
    pub fn delivered_at(&self) -> i64 {
        self.notified_at.unwrap_or(self.recommended_at)
    }
}

impl Entity for RecommendationData {
    const NAME: &'static str = "recs-recommendation-attempt";
    type Key = String;
    fn key(&self) -> String {
        self.recommendation_id.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionPath {
    #[serde(rename = "external-id")]
    ExternalId,
    #[serde(rename = "tmdb-find")]
    TmdbFind,
    #[serde(rename = "tmdb-search")]
    TmdbSearch,
    #[serde(rename = "unresolved")]
    Unresolved,
}

/// `recs-identity-alias`, keyed by server-native `guid`; failed resolutions
/// are cached too (`canonicalId: null`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityAliasData {
    pub guid: String,
    pub canonical_id: Option<String>,
    pub confidence: f64,
    pub resolution_path: ResolutionPath,
    pub title: String,
    pub resolved_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for IdentityAliasData {
    const NAME: &'static str = "recs-identity-alias";
    type Key = String;
    fn key(&self) -> String {
        self.guid.clone()
    }
}

/// A shallow patch builder for [`omni_store::entity::EntityWrite::patch`].
#[derive(Default)]
pub struct Patch(JsObjectPatch);

impl Patch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn str(mut self, key: &str, value: &str) -> Self {
        self.0
            .insert(key.to_owned(), JsValue::String(value.to_owned()));
        self
    }

    pub fn int(mut self, key: &str, value: i64) -> Self {
        self.0
            .insert(key.to_owned(), JsValue::Int(i128::from(value)));
        self
    }

    pub fn into_inner(self) -> JsObjectPatch {
        self.0
    }
}

/// Patches one recommendation inside `tx`; `None` when it does not exist.
pub fn patch_recommendation(
    tx: &mut Tx<'_>,
    recommendation_id: &str,
    patch: Patch,
) -> Result<Option<RecommendationData>, StoreError> {
    tx.patch::<RecommendationData>(
        &recommendation_id.to_owned(),
        patch.into_inner(),
        ModifyOpts::default(),
    )
}

/// One-call patch through the store.
pub async fn patch_recommendation_in(
    store: &Store,
    recommendation_id: &str,
    patch: Patch,
) -> Result<Option<RecommendationData>, StoreError> {
    let id = recommendation_id.to_owned();
    store
        .write(move |tx| patch_recommendation(tx, &id, patch))
        .await
}

/// Newest first (stable for equal timestamps).
pub fn sort_newest_first(records: &mut [RecommendationData]) {
    records.sort_by_key(|r| std::cmp::Reverse(r.recommended_at));
}

pub async fn get_all_recommendations(store: &Store) -> Result<Vec<RecommendationData>, StoreError> {
    let mut all = store
        .read(|docs| docs.get_all::<RecommendationData>())
        .await?;
    sort_newest_first(&mut all);
    Ok(all)
}

pub async fn get_recommendation(
    store: &Store,
    recommendation_id: &str,
) -> Result<Option<RecommendationData>, StoreError> {
    let id = recommendation_id.to_owned();
    store
        .read(move |docs| docs.get::<RecommendationData>(&id))
        .await
}

/// Canonical ids that must not be recommended right now: anything
/// recommended within the cooldown (24 h for failed attempts, 180 days
/// otherwise), watched/abandoned titles permanently, and titles whose latest
/// explicit feedback is `not_for_me` or `already_watched`.
pub fn compute_excluded_canonical_ids(records: &[RecommendationData], now: i64) -> HashSet<String> {
    let mut excluded = HashSet::new();
    let mut latest_feedback: HashMap<&str, &RecommendationData> = HashMap::new();
    for rec in records {
        let permanent = matches!(
            rec.status(),
            RecommendationStatus::Watched | RecommendationStatus::Abandoned
        );
        let cooldown = if rec.status() == RecommendationStatus::Failed {
            FAILED_RETRY_MS
        } else {
            COOLDOWN_MS
        };
        if permanent || now - rec.recommended_at < cooldown {
            excluded.insert(rec.canonical_id.clone());
        }
        if rec.feedback.is_some() {
            let newer = latest_feedback
                .get(rec.canonical_id.as_str())
                .is_none_or(|prior| {
                    rec.feedback_at.unwrap_or(rec.recommended_at)
                        > prior.feedback_at.unwrap_or(prior.recommended_at)
                });
            if newer {
                latest_feedback.insert(&rec.canonical_id, rec);
            }
        }
    }
    for rec in latest_feedback.values() {
        if matches!(
            rec.feedback,
            Some(RecommendationFeedback::NotForMe | RecommendationFeedback::AlreadyWatched)
        ) {
            excluded.insert(rec.canonical_id.clone());
        }
    }
    excluded
}

pub async fn get_excluded_canonical_ids(
    store: &Store,
    now: i64,
) -> Result<HashSet<String>, StoreError> {
    let all = store
        .read(|docs| docs.get_all::<RecommendationData>())
        .await?;
    Ok(compute_excluded_canonical_ids(&all, now))
}

/// The dashboard "On Deck" strip: newest delivered recommendations still
/// awaiting an outcome (pending rows are not delivered yet).
pub fn select_on_deck(open: &[RecommendationData]) -> Vec<RecommendationData> {
    let mut notified: Vec<RecommendationData> = open
        .iter()
        .filter(|rec| rec.status() == RecommendationStatus::Notified)
        .cloned()
        .collect();
    sort_newest_first(&mut notified);
    notified.truncate(ON_DECK_LIMIT);
    notified
}

/// Whether a row still awaits an outcome label.
pub fn is_open(rec: &RecommendationData) -> bool {
    matches!(
        rec.status(),
        RecommendationStatus::Notified | RecommendationStatus::Pending
    ) && !matches!(
        rec.feedback,
        Some(RecommendationFeedback::NotForMe | RecommendationFeedback::AlreadyWatched)
    )
}

/// Recommendations still awaiting an outcome label (store order).
pub async fn get_open_recommendations(
    store: &Store,
) -> Result<Vec<RecommendationData>, StoreError> {
    let all = store
        .read(|docs| docs.get_all::<RecommendationData>())
        .await?;
    Ok(all.into_iter().filter(is_open).collect())
}

/// A rating, a note, or both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedbackInput {
    pub feedback: Option<RecommendationFeedback>,
    pub note: Option<String>,
}

/// Records feedback (`feedbackAt` = now) and returns the updated row, or
/// `None` when the recommendation does not exist.
pub async fn set_recommendation_feedback(
    store: &Store,
    now: i64,
    recommendation_id: &str,
    input: FeedbackInput,
) -> Result<Option<RecommendationData>, StoreError> {
    let id = recommendation_id.to_owned();
    store
        .write(move |tx| {
            if tx.get::<RecommendationData>(&id)?.is_none() {
                return Ok(None);
            }
            let mut patch = Patch::new().int("feedbackAt", now);
            if let Some(feedback) = input.feedback {
                patch = patch.str("feedback", feedback.as_str());
            }
            if let Some(note) = &input.note {
                patch = patch.str("feedbackNote", note);
            }
            patch_recommendation(tx, &id, patch)
        })
        .await
}

/// The explicit-feedback digest injected into model prompts.
pub fn format_feedback_digest_from(input: &[RecommendationData]) -> String {
    let mut records: Vec<&RecommendationData> = input.iter().collect();
    records.sort_by(|a, b| {
        b.feedback_at
            .unwrap_or(b.recommended_at)
            .cmp(&a.feedback_at.unwrap_or(a.recommended_at))
    });
    let mut seen = HashSet::new();
    let records: Vec<&RecommendationData> = records
        .into_iter()
        .filter(|rec| {
            let has_note = rec.feedback_note.as_deref().is_some_and(|n| !n.is_empty());
            if (rec.feedback.is_none() && !has_note) || seen.contains(&rec.canonical_id) {
                return false;
            }
            seen.insert(rec.canonical_id.clone());
            true
        })
        .take(30)
        .collect();
    if records.is_empty() {
        return "No explicit recommendation feedback yet.".to_owned();
    }
    let titles = |filter: &dyn Fn(&RecommendationData) -> bool| {
        records
            .iter()
            .filter(|rec| filter(rec))
            .map(|rec| format_feedback_title(rec))
            .collect::<Vec<_>>()
    };
    let good = titles(&|rec| rec.feedback == Some(RecommendationFeedback::GoodPick));
    let bad = titles(&|rec| rec.feedback == Some(RecommendationFeedback::NotForMe));
    let note_only = titles(&|rec| {
        rec.feedback.is_none() && rec.feedback_note.as_deref().is_some_and(|n| !n.is_empty())
    });
    let mut lines = vec!["Explicit recommendation feedback:".to_owned()];
    if !good.is_empty() {
        lines.push(format!("- Good picks: {}", good.join(", ")));
    }
    if !bad.is_empty() {
        lines.push(format!("- Not for me: {}", bad.join(", ")));
    }
    if !note_only.is_empty() {
        lines.push(format!("- Notes (no rating): {}", note_only.join(", ")));
    }
    lines.join("\n")
}

fn format_feedback_title(rec: &RecommendationData) -> String {
    let year = rec
        .year
        .filter(|y| *y != 0)
        .map(|y| format!(" ({y})"))
        .unwrap_or_default();
    let base = format!("{}{year} [{}]", rec.title, rec.media_type.as_str());
    match rec.feedback_note.as_deref().filter(|n| !n.is_empty()) {
        Some(note) => format!("{base} — note: \"{note}\""),
        None => base,
    }
}

pub async fn format_feedback_digest(store: &Store) -> Result<String, StoreError> {
    Ok(format_feedback_digest_from(
        &get_all_recommendations(store).await?,
    ))
}
