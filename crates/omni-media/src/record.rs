//! `media_recommendation_record`: an assistant's exact-identity preview,
//! private reservation, and delivery bookkeeping for one film or series.
//!
//! Nothing here acquires media, writes to Radarr or Sonarr, or sends a
//! notification. A preview performs only reads (no identity cache writes).
//! A reservation is one store transaction that re-checks every recorded
//! recommendation, so no two callers or idempotency keys can reserve the
//! same title. Delivery follows `omni_runtime::chat_record`.

use std::collections::HashMap;

use omni_api::media::{RecommendationFeedback, RecommendationStatus};
use omni_core::clock::utc_date_stamp;
use omni_runtime::chat_record::{
    self, ChatRecord, ChatRecordError, DeliveryReport, RecommendationOrigin, Transition,
};
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};

use crate::identity::{RESOLUTION_CONFIDENCE_THRESHOLD, parse_guid_external_ids};
use crate::persistence::{
    COOLDOWN_MS, FAILED_RETRY_MS, IdentityAliasData, NotificationState, RecommendationData,
};
use crate::services::MediaServices;
use crate::tmdb::tmdb_url;
use crate::tmdb::types::TmdbTitleDetails;
use crate::types::{FetchResult, MediaItem, MediaType, make_canonical_id};

pub const WHY_MAX: usize = 2000;
pub const CAVEATS_MAX: usize = 5;
pub const CAVEAT_MAX: usize = 300;

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error(transparent)]
    Chat(#[from] ChatRecordError),
    #[error("Media recommendation not found")]
    NotFound,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The exact title being recommended.
#[derive(Clone, Debug, PartialEq)]
pub struct Identity {
    pub canonical_id: String,
    pub tmdb_id: i64,
    pub media_type: MediaType,
    pub title: String,
    pub year: Option<i64>,
    pub poster_path: Option<String>,
    pub tmdb_url: String,
}

/// Why a title cannot be recommended now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exclusion {
    pub code: &'static str,
    pub detail: String,
}

impl Exclusion {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// A source whose state could not be read; eligibility is then unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnavailableSource {
    pub source: &'static str,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preview {
    pub identity: Option<Identity>,
    pub details: Option<TmdbTitleDetails>,
    pub in_library: Option<bool>,
    pub exclusions: Vec<Exclusion>,
    pub unavailable: Vec<UnavailableSource>,
}

impl Preview {
    pub fn eligible(&self) -> bool {
        self.identity.is_some() && self.exclusions.is_empty() && self.unavailable.is_empty()
    }
}

fn normalize_title(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// The canonical id of a server item without network access: a confident
/// cached alias, else a TMDB id embedded in its GUID or external ids.
fn local_canonical_id(item: &MediaItem, aliases: &HashMap<String, String>) -> Option<String> {
    if let Some(id) = aliases.get(&item.guid) {
        return Some(id.clone());
    }
    parse_guid_external_ids(&item.guid)
        .overlay(item.external_ids.as_ref())
        .tmdb
        .map(|tmdb| make_canonical_id(item.media_type, tmdb))
}

/// How one server view relates to the target title.
enum Presence {
    Exact,
    /// Unresolved items that share the title, type and year.
    TitleOnly(usize),
    Absent,
}

fn presence<'a>(
    items: impl Iterator<Item = &'a MediaItem>,
    identity: &Identity,
    aliases: &HashMap<String, String>,
) -> Presence {
    let title_key = normalize_title(&identity.title);
    let mut title_only = 0;
    for item in items {
        match local_canonical_id(item, aliases) {
            Some(id) if id == identity.canonical_id => return Presence::Exact,
            Some(_) => {}
            None => {
                let same_year = match (item.year, identity.year) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                };
                if item.media_type == identity.media_type
                    && same_year
                    && !title_key.is_empty()
                    && normalize_title(&item.title) == title_key
                {
                    title_only += 1;
                }
            }
        }
    }
    if title_only > 0 {
        Presence::TitleOnly(title_only)
    } else {
        Presence::Absent
    }
}

/// Exclusions from recorded recommendations of `canonical_id`, mirroring
/// the selection pipeline's cooldowns plus any still-pending reservation.
pub fn record_exclusions(
    records: &[RecommendationData],
    canonical_id: &str,
    now: i64,
) -> Vec<Exclusion> {
    let mut exclusions = Vec::new();
    let mut latest_feedback: Option<&RecommendationData> = None;
    for rec in records.iter().filter(|r| r.canonical_id == canonical_id) {
        let status = rec.status();
        if status == RecommendationStatus::Pending {
            exclusions.push(Exclusion::new(
                "pending_recommendation",
                format!("reservation {} is still pending", rec.recommendation_id),
            ));
        } else if matches!(
            status,
            RecommendationStatus::Watched | RecommendationStatus::Abandoned
        ) {
            exclusions.push(Exclusion::new(
                "terminal_outcome",
                format!(
                    "recommendation {} ended {}",
                    rec.recommendation_id,
                    status.as_str()
                ),
            ));
        } else {
            let (window, label) = if status == RecommendationStatus::Failed {
                (FAILED_RETRY_MS, "failed within the last 24 hours")
            } else {
                (COOLDOWN_MS, "recommended within the last 180 days")
            };
            if now - rec.recommended_at < window {
                exclusions.push(Exclusion::new(
                    "recently_recommended",
                    format!("recommendation {} {label}", rec.recommendation_id),
                ));
            }
        }
        if rec.feedback.is_some()
            && latest_feedback.is_none_or(|prior| {
                rec.feedback_at.unwrap_or(rec.recommended_at)
                    > prior.feedback_at.unwrap_or(prior.recommended_at)
            })
        {
            latest_feedback = Some(rec);
        }
    }
    if let Some(rec) = latest_feedback
        && let Some(
            feedback @ (RecommendationFeedback::NotForMe | RecommendationFeedback::AlreadyWatched),
        ) = rec.feedback
    {
        exclusions.push(Exclusion::new(
            "feedback_excluded",
            format!("latest feedback is {}", feedback.as_str()),
        ));
    }
    exclusions
}

fn unavailable<T>(
    result: FetchResult<T>,
    source: &'static str,
    out: &mut Vec<UnavailableSource>,
) -> Option<T> {
    match result {
        FetchResult::Ok(value) => Some(value),
        FetchResult::Unavailable { reason } => {
            out.push(UnavailableSource { source, reason });
            None
        }
    }
}

/// Exact identity, current library, history and watchlist state, and
/// recorded exclusions. Reads only.
pub async fn preview(
    services: &MediaServices,
    media_type: MediaType,
    tmdb_id: i64,
) -> Result<Preview, RecordError> {
    let mut preview = Preview::default();
    let (title, details, history, in_progress, library, watchlist) = futures::join!(
        services.catalog.title(media_type, tmdb_id),
        services.catalog.title_details(media_type, tmdb_id),
        services.library.watch_history(),
        services.library.in_progress(),
        services.library.library_index(),
        services.watchlist.fetch()
    );
    match title {
        Ok(title) => {
            preview.identity = Some(Identity {
                canonical_id: make_canonical_id(media_type, tmdb_id),
                tmdb_id,
                media_type,
                title: title.title,
                year: title.year,
                poster_path: title.poster_path,
                tmdb_url: tmdb_url(media_type, tmdb_id),
            });
            preview.details = details.ok();
        }
        Err(error) => preview.unavailable.push(UnavailableSource {
            source: "tmdb",
            reason: error.to_string(),
        }),
    }
    let history = unavailable(history, "watch_history", &mut preview.unavailable);
    let in_progress = unavailable(in_progress, "in_progress", &mut preview.unavailable);
    let library = unavailable(library, "library", &mut preview.unavailable);
    let watchlist = unavailable(watchlist, "watchlist", &mut preview.unavailable);
    let Some(identity) = preview.identity.clone() else {
        return Ok(preview);
    };

    let aliases: HashMap<String, String> = services
        .store
        .read(|docs| docs.get_all::<IdentityAliasData>())
        .await?
        .into_iter()
        .filter(|a| a.confidence >= RESOLUTION_CONFIDENCE_THRESHOLD)
        .filter_map(|a| a.canonical_id.map(|id| (a.guid, id)))
        .collect();
    let mut check = |items: Option<Vec<&MediaItem>>,
                     exact: (&'static str, &'static str),
                     source: &'static str| {
        let Some(items) = items else { return };
        match presence(items.into_iter(), &identity, &aliases) {
            Presence::Exact => preview.exclusions.push(Exclusion::new(exact.0, exact.1)),
            Presence::TitleOnly(count) => preview.exclusions.push(Exclusion::new(
                "possible_title_match",
                format!(
                    "{count} unresolved {source} item(s) share this title; seen status is unknown"
                ),
            )),
            Presence::Absent => {}
        }
    };
    check(
        history
            .as_ref()
            .map(|h| h.iter().map(|w| &w.item).collect()),
        ("already_watched", "in the media server's watch history"),
        "watch history",
    );
    check(
        in_progress
            .as_ref()
            .map(|p| p.iter().map(|w| &w.item).collect()),
        ("in_progress", "currently in progress"),
        "in-progress",
    );
    check(
        watchlist.as_ref().map(|w| w.iter().collect()),
        ("on_watchlist", "already tracked by Radarr or Sonarr"),
        "watchlist",
    );
    preview.in_library = library.as_ref().map(|items| {
        items.iter().any(|item| {
            local_canonical_id(item, &aliases).as_deref() == Some(&identity.canonical_id)
        })
    });
    let records = services
        .store
        .read(|docs| docs.get_all::<RecommendationData>())
        .await?;
    preview.exclusions.extend(record_exclusions(
        &records,
        &identity.canonical_id,
        services.now(),
    ));
    Ok(preview)
}

/// The caller's prose for a reservation.
#[derive(Clone, Debug, PartialEq)]
pub struct PrepareInput {
    pub idempotency_key: String,
    pub media_type: MediaType,
    pub tmdb_id: i64,
    pub why_for_user: String,
    pub caveats: Vec<String>,
    pub dry_run: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PrepareResult {
    /// The key already reserved this title; the existing record.
    Replayed(Box<RecommendationData>),
    /// Eligibility could not be established.
    Unavailable(Box<Preview>),
    Ineligible(Box<Preview>),
    /// Dry run: a reservation would be made.
    Eligible(Box<Preview>),
    Reserved(Box<RecommendationData>, Box<Preview>),
}

fn validate_prepare(input: &PrepareInput) -> Result<PrepareInput, ChatRecordError> {
    if input.tmdb_id <= 0 {
        return Err(ChatRecordError::Input("tmdbId must be positive".to_owned()));
    }
    if input.caveats.len() > CAVEATS_MAX {
        return Err(ChatRecordError::Input(format!(
            "caveats must have at most {CAVEATS_MAX} items"
        )));
    }
    Ok(PrepareInput {
        idempotency_key: chat_record::caller_key(&input.idempotency_key)?,
        why_for_user: chat_record::prose(&input.why_for_user, "whyForUser", WHY_MAX)?,
        caveats: input
            .caveats
            .iter()
            .map(|c| chat_record::prose(c, "caveats[]", CAVEAT_MAX))
            .collect::<Result<_, _>>()?,
        ..input.clone()
    })
}

/// The record a key already reserved, or a conflict when the key names a
/// different title.
fn replay(
    records: &[RecommendationData],
    key: &str,
    canonical_id: &str,
) -> Result<Option<RecommendationData>, ChatRecordError> {
    let Some(existing) = records
        .iter()
        .find(|r| r.chat_record.as_ref().is_some_and(|c| c.caller_key == key))
    else {
        return Ok(None);
    };
    if existing.canonical_id == canonical_id {
        Ok(Some(existing.clone()))
    } else {
        Err(ChatRecordError::Conflict(format!(
            "idempotencyKey already reserved {} ({})",
            existing.canonical_id, existing.recommendation_id
        )))
    }
}

fn reservation(
    input: &PrepareInput,
    identity: &Identity,
    details: Option<&TmdbTitleDetails>,
    now: i64,
) -> RecommendationData {
    let non_empty = |v: &Vec<String>| (!v.is_empty()).then(|| v.clone());
    RecommendationData {
        recommendation_id: omni_core::ids::uuid_v4(),
        canonical_id: identity.canonical_id.clone(),
        tmdb_id: identity.tmdb_id,
        media_type: identity.media_type,
        title: identity.title.clone(),
        year: identity.year,
        poster_path: identity.poster_path.clone(),
        status: RecommendationStatus::Pending,
        why_for_user: Some(input.why_for_user.clone()),
        caveats: Some(input.caveats.clone()),
        genres: details.map(|d| d.genres.clone()),
        runtime_minutes: details.and_then(|d| d.runtime_minutes),
        season_count: details.and_then(|d| d.season_count),
        episode_count: details.and_then(|d| d.episode_count),
        series_status: details.and_then(|d| d.series_status.clone()),
        original_language: details.and_then(|d| d.original_language.clone()),
        origin_countries: details.and_then(|d| non_empty(&d.origin_countries)),
        creators: details.and_then(|d| non_empty(&d.creators)),
        cast: details.and_then(|d| non_empty(&d.cast)),
        keywords: details.and_then(|d| non_empty(&d.keywords)),
        certification: details.and_then(|d| d.certification.clone()),
        run_date: utc_date_stamp(now),
        recommended_at: now,
        origin: Some(RecommendationOrigin::AssistantChat),
        chat_record: Some(ChatRecord::reserved(input.idempotency_key.clone(), now)),
        ..RecommendationData::default()
    }
}

/// Previews and, unless `dry_run`, reserves the title atomically.
pub async fn prepare(
    services: &MediaServices,
    input: &PrepareInput,
) -> Result<PrepareResult, RecordError> {
    let input = validate_prepare(input)?;
    let canonical_id = make_canonical_id(input.media_type, input.tmdb_id);
    let records = services
        .store
        .read(|docs| docs.get_all::<RecommendationData>())
        .await?;
    if let Some(existing) = replay(&records, &input.idempotency_key, &canonical_id)? {
        return Ok(PrepareResult::Replayed(Box::new(existing)));
    }
    let preview = preview(services, input.media_type, input.tmdb_id).await?;
    let Some(identity) = preview.identity.clone() else {
        return Ok(PrepareResult::Unavailable(Box::new(preview)));
    };
    if !preview.unavailable.is_empty() {
        return Ok(PrepareResult::Unavailable(Box::new(preview)));
    }
    if !preview.exclusions.is_empty() {
        return Ok(PrepareResult::Ineligible(Box::new(preview)));
    }
    if input.dry_run {
        return Ok(PrepareResult::Eligible(Box::new(preview)));
    }
    let now = services.now();
    let row = reservation(&input, &identity, preview.details.as_ref(), now);
    let key = input.idempotency_key.clone();
    let reserved = services
        .store
        .write(move |tx| -> Result<Reservation, RecordError> {
            let records = tx.get_all::<RecommendationData>()?;
            if let Some(existing) = replay(&records, &key, &row.canonical_id)? {
                return Ok(Reservation::Replayed(existing));
            }
            let exclusions = record_exclusions(&records, &row.canonical_id, now);
            if !exclusions.is_empty() {
                return Ok(Reservation::Excluded(exclusions));
            }
            tx.upsert(&row, UpsertOpts::default())?;
            Ok(Reservation::Reserved(row))
        })
        .await?;
    Ok(match reserved {
        Reservation::Reserved(rec) => PrepareResult::Reserved(Box::new(rec), Box::new(preview)),
        Reservation::Replayed(rec) => PrepareResult::Replayed(Box::new(rec)),
        Reservation::Excluded(exclusions) => PrepareResult::Ineligible(Box::new(Preview {
            exclusions,
            ..preview
        })),
    })
}

/// The outcome of the reservation transaction.
enum Reservation {
    Reserved(RecommendationData),
    Replayed(RecommendationData),
    Excluded(Vec<Exclusion>),
}

/// The record before and after a finish or withdraw.
#[derive(Clone, Debug, PartialEq)]
pub struct DeliveryOutcome {
    /// False when the record already said this (a retry).
    pub changed: bool,
    pub record: RecommendationData,
}

fn chat_record_of(rec: &RecommendationData) -> Result<&ChatRecord, ChatRecordError> {
    match (rec.origin, rec.chat_record.as_ref()) {
        (Some(RecommendationOrigin::AssistantChat), Some(record)) => Ok(record),
        _ => Err(ChatRecordError::NotChatRecord(
            rec.recommendation_id.clone(),
        )),
    }
}

/// Applies a chat transition to the row's status fields.
fn apply(rec: &RecommendationData, next: ChatRecord, now: i64) -> RecommendationData {
    let mut row = rec.clone();
    match next.delivery {
        chat_record::ChatDelivery::Sent => {
            row.status = RecommendationStatus::Notified;
            row.notified_at = next.receipt.as_ref().map(|r| r.sent_at);
            row.notification_state = Some(NotificationState::Sent);
        }
        chat_record::ChatDelivery::Unknown => {
            row.notification_state = Some(NotificationState::Unknown);
        }
        chat_record::ChatDelivery::Withdrawn => {
            row.status = RecommendationStatus::Failed;
            row.resolved_at = Some(now);
        }
        chat_record::ChatDelivery::Reserved => {}
    }
    row.chat_record = Some(next);
    row
}

async fn transition(
    store: &Store,
    recommendation_id: &str,
    now: i64,
    dry_run: bool,
    decide: impl FnOnce(&RecommendationData, &ChatRecord) -> Result<Transition, ChatRecordError>
    + Send
    + 'static,
) -> Result<DeliveryOutcome, RecordError> {
    let id = recommendation_id.to_owned();
    store
        .write(move |tx| -> Result<DeliveryOutcome, RecordError> {
            let rec = tx
                .get::<RecommendationData>(&id)?
                .ok_or(RecordError::NotFound)?;
            let record = chat_record_of(&rec)?;
            match decide(&rec, record)? {
                Transition::Unchanged => Ok(DeliveryOutcome {
                    changed: false,
                    record: rec,
                }),
                Transition::Changed(next) => {
                    let row = apply(&rec, *next, now);
                    if !dry_run {
                        tx.upsert(&row, UpsertOpts::default())?;
                    }
                    Ok(DeliveryOutcome {
                        changed: true,
                        record: row,
                    })
                }
            }
        })
        .await
}

/// Records the caller's delivery report. Only a receipt marks a row
/// notified; an unknown report leaves it pending.
pub async fn finish(
    services: &MediaServices,
    recommendation_id: &str,
    expected_revision: i64,
    report: &DeliveryReport,
    dry_run: bool,
) -> Result<DeliveryOutcome, RecordError> {
    let now = services.now();
    let report = report.clone();
    transition(
        &services.store,
        recommendation_id,
        now,
        dry_run,
        move |rec, record| {
            let report = chat_record::validate_report(&report, rec.recommended_at, now)?;
            chat_record::finish(record, expected_revision, &report, now)
        },
    )
    .await
}

/// Withdraws a reservation the caller knows was never presented: the row
/// fails, keeping the 24-hour retry exclusion.
pub async fn withdraw(
    services: &MediaServices,
    recommendation_id: &str,
    expected_revision: i64,
    reason: &str,
    dry_run: bool,
) -> Result<DeliveryOutcome, RecordError> {
    let now = services.now();
    let reason = reason.to_owned();
    transition(
        &services.store,
        recommendation_id,
        now,
        dry_run,
        move |_, record| chat_record::withdraw(record, expected_revision, &reason, now),
    )
    .await
}
