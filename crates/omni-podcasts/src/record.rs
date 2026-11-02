//! `podcast_recommendation_record`: an assistant's exact-episode preview,
//! private reservation, and delivery bookkeeping for one podcast episode.
//!
//! Identity is the show's own RSS feed: the episode GUID must appear in it
//! (and the enclosure must match when given). The Castro episode is looked
//! up read-only and reported separately; it is never the identity and never
//! guessed from a title. Nothing here queues, subscribes or notifies. A
//! reservation is one store transaction that re-checks every recorded
//! recommendation, so no two callers or idempotency keys can reserve the
//! same episode. Delivery follows `omni_runtime::chat_record`.

use omni_core::clock::utc_date_stamp;
use omni_runtime::chat_record::{
    self, ChatDelivery, ChatRecord, ChatRecordError, DeliveryReport, RecommendationOrigin,
    Transition,
};
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};

use crate::account::{
    AccountProvider, EnqueueEpisodeRequest, EpisodeLookup, EpisodeMatchKind, PodcastQueuePosition,
};
use crate::castro::client::normalize_media_url;
use crate::persistence::{
    FAILED_RETRY_MS, PodcastFeedback, PodcastRecommendationData, PodcastRecommendationStatus,
    SHOW_COOLDOWN_MS,
};
use crate::rss::FeedEpisode;
use crate::sources::ShowDirectory;
use crate::titles::normalize_title;
use crate::types::{make_episode_id, make_show_id, normalize_feed_url};

pub const WHY_MAX: usize = 2000;
pub const CAVEATS_MAX: usize = 5;
pub const CAVEAT_MAX: usize = 300;
pub const SHOW_TITLE_MAX: usize = 300;
/// Feed items scanned for the GUID; back-catalog episodes stay findable.
const FEED_SCAN_EPISODES: usize = 1000;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error(transparent)]
    Chat(#[from] ChatRecordError),
    #[error("Podcast recommendation not found")]
    NotFound,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What the caller names: the show's feed and the episode's RSS GUID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeRef {
    pub feed_url: String,
    pub episode_guid: String,
    pub enclosure_url: Option<String>,
    pub itunes_id: Option<i64>,
    /// Display only; identity never depends on it.
    pub show_title: String,
}

/// The exact episode, as the feed states it.
#[derive(Clone, Debug, PartialEq)]
pub struct Identity {
    pub episode_id: String,
    pub show_id: String,
    pub show_title: String,
    pub episode_title: String,
    pub feed_url: String,
    pub itunes_id: Option<i64>,
    pub episode_guid: String,
    pub media_url: Option<String>,
    pub episode_url: Option<String>,
    pub published_at: i64,
    pub duration_minutes: Option<i64>,
}

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnavailableSource {
    pub source: &'static str,
    pub reason: String,
}

/// The client's episode for the exact identity, looked up read-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMapping {
    /// Matched by GUID or enclosure: the enqueue request is safe to use.
    Verified {
        client_episode_id: String,
        matched_by: EpisodeMatchKind,
    },
    /// The client has no exact match (a title match is not exact).
    Unverified {
        reason: String,
    },
    Unsupported {
        reason: String,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    pub identity: Option<Identity>,
    pub exclusions: Vec<Exclusion>,
    pub unavailable: Vec<UnavailableSource>,
    pub client: ClientMapping,
}

impl Preview {
    fn empty() -> Self {
        Self {
            identity: None,
            exclusions: Vec::new(),
            unavailable: Vec::new(),
            client: ClientMapping::Unsupported {
                reason: "episode identity was not verified".to_owned(),
            },
        }
    }

    /// The exact `podcast_account_update` enqueue arguments, only when the
    /// client mapping is verified.
    pub fn enqueue_request(&self) -> Option<EnqueueEpisodeRequest> {
        let identity = self.identity.as_ref()?;
        matches!(self.client, ClientMapping::Verified { .. }).then(|| enqueue_request(identity))
    }
}

fn enqueue_request(identity: &Identity) -> EnqueueEpisodeRequest {
    EnqueueEpisodeRequest {
        feed_url: identity.feed_url.clone(),
        itunes_id: identity.itunes_id,
        episode_guid: identity.episode_guid.clone(),
        media_url: identity.media_url.clone(),
        show_title: identity.show_title.clone(),
        episode_title: identity.episode_title.clone(),
        position: Some(PodcastQueuePosition::Next),
    }
}

fn media_key(url: Option<&str>) -> Option<String> {
    normalize_media_url(url)
}

fn same_episode(rec: &PodcastRecommendationData, identity: &Identity) -> bool {
    rec.episode_id == identity.episode_id
        || (normalize_feed_url(&rec.feed_url) == normalize_feed_url(&identity.feed_url)
            && rec.episode_guid == identity.episode_guid)
        || media_key(rec.media_url.as_deref())
            .is_some_and(|key| Some(key) == media_key(identity.media_url.as_deref()))
}

fn same_show(rec: &PodcastRecommendationData, identity: &Identity) -> bool {
    rec.show_id == identity.show_id
        || normalize_feed_url(&rec.feed_url) == normalize_feed_url(&identity.feed_url)
}

/// Exclusions from recorded recommendations, mirroring the selection
/// pipeline: an episode is recommended once (failed rows retry after 24
/// hours), its show cools down for 30 days, and not-for-me feedback on the
/// show excludes it. Shows and episodes match by canonical id, feed URL and
/// GUID, or enclosure, so a different iTunes id cannot bypass them.
pub fn record_exclusions(
    records: &[PodcastRecommendationData],
    identity: &Identity,
    now: i64,
) -> Vec<Exclusion> {
    let mut exclusions = Vec::new();
    let mut latest_feedback: Option<&PodcastRecommendationData> = None;
    let mut cooldown = false;
    for rec in records.iter() {
        if same_episode(rec, identity) {
            match rec.status {
                PodcastRecommendationStatus::Pending => exclusions.push(Exclusion::new(
                    "pending_recommendation",
                    format!("reservation {} is still pending", rec.recommendation_id),
                )),
                PodcastRecommendationStatus::Failed => {
                    if now - rec.recommended_at < FAILED_RETRY_MS {
                        exclusions.push(Exclusion::new(
                            "recently_failed",
                            format!(
                                "recommendation {} failed within the last 24 hours",
                                rec.recommendation_id
                            ),
                        ));
                    }
                }
                status => exclusions.push(Exclusion::new(
                    "already_recommended",
                    format!(
                        "recommendation {} is {}",
                        rec.recommendation_id,
                        status.as_str()
                    ),
                )),
            }
        }
        if !same_show(rec, identity) {
            continue;
        }
        if rec.status != PodcastRecommendationStatus::Failed
            && now - rec.recommended_at < SHOW_COOLDOWN_MS
        {
            cooldown = true;
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
    if cooldown {
        exclusions.push(Exclusion::new(
            "show_cooldown",
            "the show was recommended within the last 30 days",
        ));
    }
    if latest_feedback.is_some_and(|rec| rec.feedback == Some(PodcastFeedback::NotForMe)) {
        exclusions.push(Exclusion::new(
            "show_feedback_excluded",
            "latest feedback on this show is not_for_me",
        ));
    }
    exclusions
}

/// What the preview reads.
pub struct RecordDeps<'a> {
    pub store: &'a Store,
    pub accounts: &'a dyn AccountProvider,
    pub directory: &'a dyn ShowDirectory,
    pub now: i64,
}

fn verify_in_feed(episodes: &[FeedEpisode], target: &EpisodeRef) -> Result<FeedEpisode, Exclusion> {
    let Some(episode) = episodes.iter().find(|e| e.guid == target.episode_guid) else {
        return Err(Exclusion::new(
            "episode_not_in_feed",
            "the feed has no item with this exact GUID",
        ));
    };
    if let Some(expected) = target.enclosure_url.as_deref()
        && media_key(Some(expected)) != media_key(episode.enclosure_url.as_deref())
    {
        return Err(Exclusion::new(
            "enclosure_mismatch",
            "the feed item's enclosure differs from enclosureUrl",
        ));
    }
    Ok(episode.clone())
}

/// Exact identity from the feed, account state (subscriptions, listen
/// history, queue), recorded exclusions and the read-only client mapping.
pub async fn preview(deps: &RecordDeps<'_>, target: &EpisodeRef) -> Result<Preview, RecordError> {
    let mut preview = Preview::empty();
    let feed = deps
        .directory
        .fetch_feed(&target.feed_url, FEED_SCAN_EPISODES)
        .await;
    let episode = match feed {
        Ok(episodes) => match verify_in_feed(&episodes, target) {
            Ok(episode) => episode,
            Err(exclusion) => {
                preview.exclusions.push(exclusion);
                return Ok(preview);
            }
        },
        Err(reason) => {
            preview.unavailable.push(UnavailableSource {
                source: "rss",
                reason,
            });
            return Ok(preview);
        }
    };
    let Some(show_id) = make_show_id(target.itunes_id, Some(&target.feed_url)) else {
        preview.exclusions.push(Exclusion::new(
            "episode_not_in_feed",
            "could not build a show identity from the feed URL",
        ));
        return Ok(preview);
    };
    let identity = Identity {
        episode_id: make_episode_id(&show_id, &episode.guid),
        show_id,
        show_title: target.show_title.clone(),
        episode_title: episode.title.clone(),
        feed_url: target.feed_url.clone(),
        itunes_id: target.itunes_id,
        episode_guid: episode.guid.clone(),
        media_url: episode.enclosure_url.clone(),
        episode_url: episode.link.clone(),
        published_at: episode.published_at,
        duration_minutes: episode.duration_minutes,
    };
    if identity.published_at > deps.now + DAY_MS {
        preview.exclusions.push(Exclusion::new(
            "future_release",
            "the feed dates this episode in the future",
        ));
    }

    match deps.accounts.resolve() {
        None => {
            preview.unavailable.push(UnavailableSource {
                source: "podcast_account",
                reason:
                    "no podcast account is configured; subscriptions and listen history are unknown"
                        .to_owned(),
            });
            preview.client = ClientMapping::Unsupported {
                reason: "no podcast account is configured".to_owned(),
            };
        }
        Some(account) => {
            let (subscriptions, history, queue, lookup) = futures::join!(
                account.fetch_subscriptions(),
                account.fetch_listen_history(None),
                account.fetch_queue(),
                account.find_episode(enqueue_request(&identity))
            );
            let feed_key = normalize_feed_url(&identity.feed_url);
            let title_key = normalize_title(&identity.show_title);
            match subscriptions {
                Ok(subs) => {
                    if subs.iter().any(|s| {
                        make_show_id(s.itunes_id, s.feed_url.as_deref()).as_deref()
                            == Some(identity.show_id.as_str())
                            || s.feed_url
                                .as_deref()
                                .is_some_and(|f| normalize_feed_url(f) == feed_key)
                            || (!title_key.is_empty() && normalize_title(&s.title) == title_key)
                    }) {
                        preview.exclusions.push(Exclusion::new(
                            "already_subscribed",
                            "the show is subscribed",
                        ));
                    }
                }
                Err(e) => preview.unavailable.push(UnavailableSource {
                    source: "subscriptions",
                    reason: e.reason,
                }),
            }
            let media = media_key(identity.media_url.as_deref());
            match history {
                Ok(history) => {
                    if history.iter().any(|l| {
                        l.episode_guid.as_deref() == Some(identity.episode_guid.as_str())
                            || (media.is_some() && media_key(l.media_url.as_deref()) == media)
                    }) {
                        preview.exclusions.push(Exclusion::new(
                            "already_listened",
                            "the episode is in the listen history",
                        ));
                    }
                }
                Err(e) => preview.unavailable.push(UnavailableSource {
                    source: "listen_history",
                    reason: e.reason,
                }),
            }
            match queue {
                Ok(queue) => {
                    if queue
                        .iter()
                        .any(|q| q.episode_guid.as_deref() == Some(identity.episode_guid.as_str()))
                    {
                        preview
                            .exclusions
                            .push(Exclusion::new("in_queue", "the episode is already queued"));
                    }
                }
                Err(e) => preview.unavailable.push(UnavailableSource {
                    source: "queue",
                    reason: e.reason,
                }),
            }
            preview.client = match lookup {
                Ok(EpisodeLookup::Found {
                    client_episode_id,
                    matched_by: matched_by @ (EpisodeMatchKind::Guid | EpisodeMatchKind::Enclosure),
                }) => ClientMapping::Verified {
                    client_episode_id,
                    matched_by,
                },
                Ok(EpisodeLookup::Found {
                    matched_by: EpisodeMatchKind::Title,
                    ..
                }) => ClientMapping::Unverified {
                    reason: format!(
                        "{} matched only by title; queue it only after confirming the episode",
                        account.name()
                    ),
                },
                Ok(EpisodeLookup::ShowNotFound) => ClientMapping::Unverified {
                    reason: format!("{} does not list this show's feed", account.name()),
                },
                Ok(EpisodeLookup::EpisodeNotFound) => ClientMapping::Unverified {
                    reason: format!("{} does not list this episode yet", account.name()),
                },
                Ok(EpisodeLookup::Unsupported) => ClientMapping::Unsupported {
                    reason: format!("{} cannot look episodes up read-only", account.name()),
                },
                Err(e) => ClientMapping::Unavailable { reason: e.reason },
            };
        }
    }

    let records = deps
        .store
        .read(|docs| docs.get_all::<PodcastRecommendationData>())
        .await?;
    preview
        .exclusions
        .extend(record_exclusions(&records, &identity, deps.now));
    preview.identity = Some(identity);
    Ok(preview)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareInput {
    pub idempotency_key: String,
    pub episode: EpisodeRef,
    pub why_for_user: String,
    pub caveats: Vec<String>,
    pub dry_run: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PrepareResult {
    Replayed(Box<PodcastRecommendationData>),
    Unavailable(Box<Preview>),
    Ineligible(Box<Preview>),
    Eligible(Box<Preview>),
    Reserved(Box<PodcastRecommendationData>, Box<Preview>),
}

fn validate_prepare(input: &PrepareInput) -> Result<PrepareInput, ChatRecordError> {
    if input.caveats.len() > CAVEATS_MAX {
        return Err(ChatRecordError::Input(format!(
            "caveats must have at most {CAVEATS_MAX} items"
        )));
    }
    if input.episode.itunes_id.is_some_and(|id| id <= 0) {
        return Err(ChatRecordError::Input(
            "itunesId must be positive".to_owned(),
        ));
    }
    let mut episode = input.episode.clone();
    episode.feed_url = chat_record::prose(&episode.feed_url, "feedUrl", 2000)?;
    episode.episode_guid = chat_record::prose(&episode.episode_guid, "episodeGuid", 1000)?;
    episode.show_title = chat_record::prose(&episode.show_title, "showTitle", SHOW_TITLE_MAX)?;
    Ok(PrepareInput {
        idempotency_key: chat_record::caller_key(&input.idempotency_key)?,
        why_for_user: chat_record::prose(&input.why_for_user, "whyForUser", WHY_MAX)?,
        caveats: input
            .caveats
            .iter()
            .map(|c| chat_record::prose(c, "caveats[]", CAVEAT_MAX))
            .collect::<Result<_, _>>()?,
        episode,
        dry_run: input.dry_run,
    })
}

/// The record a key already reserved, or a conflict when the key names a
/// different episode. Before the feed is read the episode is compared by
/// feed URL and GUID.
fn replay(
    records: &[PodcastRecommendationData],
    key: &str,
    episode: &EpisodeRef,
) -> Result<Option<PodcastRecommendationData>, ChatRecordError> {
    let Some(existing) = records
        .iter()
        .find(|r| r.chat_record.as_ref().is_some_and(|c| c.caller_key == key))
    else {
        return Ok(None);
    };
    if normalize_feed_url(&existing.feed_url) == normalize_feed_url(&episode.feed_url)
        && existing.episode_guid == episode.episode_guid
    {
        Ok(Some(existing.clone()))
    } else {
        Err(ChatRecordError::Conflict(format!(
            "idempotencyKey already reserved {} ({})",
            existing.episode_id, existing.recommendation_id
        )))
    }
}

fn reservation(input: &PrepareInput, identity: &Identity, now: i64) -> PodcastRecommendationData {
    PodcastRecommendationData {
        recommendation_id: omni_core::ids::uuid_v4(),
        episode_id: identity.episode_id.clone(),
        show_id: identity.show_id.clone(),
        show_title: identity.show_title.clone(),
        episode_title: identity.episode_title.clone(),
        feed_url: identity.feed_url.clone(),
        itunes_id: identity.itunes_id,
        artwork_url: None,
        episode_guid: identity.episode_guid.clone(),
        media_url: identity.media_url.clone(),
        episode_url: identity.episode_url.clone(),
        published_at: identity.published_at,
        duration_minutes: identity.duration_minutes,
        status: PodcastRecommendationStatus::Pending,
        why_for_user: Some(input.why_for_user.clone()),
        caveats: Some(input.caveats.clone()),
        confidence: None,
        show_genres: None,
        discovered_via: None,
        source_url: None,
        matched_voices: None,
        shortlist_scores: None,
        run_date: utc_date_stamp(now),
        recommended_at: now,
        notified_at: None,
        queue_result: None,
        resolved_at: None,
        feedback: None,
        feedback_at: None,
        feedback_note: None,
        origin: Some(RecommendationOrigin::AssistantChat),
        chat_record: Some(ChatRecord::reserved(input.idempotency_key.clone(), now)),
        extra: Extra::default(),
    }
}

enum Reservation {
    Reserved(PodcastRecommendationData),
    Replayed(PodcastRecommendationData),
    Excluded(Vec<Exclusion>),
}

/// Previews and, unless `dry_run`, reserves the episode atomically.
pub async fn prepare(
    deps: &RecordDeps<'_>,
    input: &PrepareInput,
) -> Result<PrepareResult, RecordError> {
    let input = validate_prepare(input)?;
    let records = deps
        .store
        .read(|docs| docs.get_all::<PodcastRecommendationData>())
        .await?;
    if let Some(existing) = replay(&records, &input.idempotency_key, &input.episode)? {
        return Ok(PrepareResult::Replayed(Box::new(existing)));
    }
    let preview = preview(deps, &input.episode).await?;
    if !preview.unavailable.is_empty() {
        return Ok(PrepareResult::Unavailable(Box::new(preview)));
    }
    let Some(identity) = preview.identity.clone() else {
        return Ok(PrepareResult::Ineligible(Box::new(preview)));
    };
    if !preview.exclusions.is_empty() {
        return Ok(PrepareResult::Ineligible(Box::new(preview)));
    }
    if input.dry_run {
        return Ok(PrepareResult::Eligible(Box::new(preview)));
    }
    let now = deps.now;
    let row = reservation(&input, &identity, now);
    let key = input.idempotency_key.clone();
    let episode = input.episode.clone();
    let reserved = deps
        .store
        .write(move |tx| -> Result<Reservation, RecordError> {
            let records = tx.get_all::<PodcastRecommendationData>()?;
            if let Some(existing) = replay(&records, &key, &episode)? {
                return Ok(Reservation::Replayed(existing));
            }
            let exclusions = record_exclusions(&records, &identity, now);
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

#[derive(Clone, Debug, PartialEq)]
pub struct DeliveryOutcome {
    pub changed: bool,
    pub record: PodcastRecommendationData,
}

fn chat_record_of(rec: &PodcastRecommendationData) -> Result<&ChatRecord, ChatRecordError> {
    match (rec.origin, rec.chat_record.as_ref()) {
        (Some(RecommendationOrigin::AssistantChat), Some(record)) => Ok(record),
        _ => Err(ChatRecordError::NotChatRecord(
            rec.recommendation_id.clone(),
        )),
    }
}

fn apply(rec: &PodcastRecommendationData, next: ChatRecord, now: i64) -> PodcastRecommendationData {
    let mut row = rec.clone();
    match next.delivery {
        ChatDelivery::Sent => {
            row.status = PodcastRecommendationStatus::Notified;
            row.notified_at = next.receipt.as_ref().map(|r| r.sent_at);
        }
        ChatDelivery::Withdrawn => {
            row.status = PodcastRecommendationStatus::Failed;
            row.resolved_at = Some(now);
        }
        ChatDelivery::Reserved | ChatDelivery::Unknown => {}
    }
    row.chat_record = Some(next);
    row
}

async fn transition(
    store: &Store,
    recommendation_id: &str,
    now: i64,
    dry_run: bool,
    decide: impl FnOnce(&PodcastRecommendationData, &ChatRecord) -> Result<Transition, ChatRecordError>
    + Send
    + 'static,
) -> Result<DeliveryOutcome, RecordError> {
    let id = recommendation_id.to_owned();
    store
        .write(move |tx| -> Result<DeliveryOutcome, RecordError> {
            let rec = tx
                .get::<PodcastRecommendationData>(&id)?
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
    store: &Store,
    now: i64,
    recommendation_id: &str,
    expected_revision: i64,
    report: &DeliveryReport,
    dry_run: bool,
) -> Result<DeliveryOutcome, RecordError> {
    let report = report.clone();
    transition(
        store,
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
    store: &Store,
    now: i64,
    recommendation_id: &str,
    expected_revision: i64,
    reason: &str,
    dry_run: bool,
) -> Result<DeliveryOutcome, RecordError> {
    let reason = reason.to_owned();
    transition(store, recommendation_id, now, dry_run, move |_, record| {
        chat_record::withdraw(record, expected_revision, &reason, now)
    })
    .await
}
