//! The observed Castro Tentacles sync protocol. Decoding
//! is serde plus refinements (UUIDs, URLs, ranges): a response that fails
//! them is a request error.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CastroActionType {
    ClearEpisodeNew,
    EpisodeDequeued,
    EpisodeLastPlayed,
    EpisodeNew,
    EpisodeProgress,
    EpisodeQueued,
}

impl CastroActionType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClearEpisodeNew => "clear_episode_new",
            Self::EpisodeDequeued => "episode_dequeued",
            Self::EpisodeLastPlayed => "episode_last_played",
            Self::EpisodeNew => "episode_new",
            Self::EpisodeProgress => "episode_progress",
            Self::EpisodeQueued => "episode_queued",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CastroActionSource {
    Policy,
    User,
}

/// One sync action. `event_data` is JSON encoded as a string inside the body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroAction {
    pub id: u64,
    pub episode_id: String,
    pub origin_event_id: String,
    pub origin_timestamp: u64,
    pub source: CastroActionSource,
    pub action_type: CastroActionType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_data: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroActionBatch {
    pub actions: Vec<CastroAction>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroSyncStatus {
    pub device_status: i64,
    pub account_status: i64,
    pub latest_event_id: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroEventsResponse {
    pub events: Vec<Value>,
    pub latest_event_id: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroUserEventsResponse {
    pub user_events: Vec<Value>,
    pub latest_event_id: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroSubscribedFeed {
    pub feed_id: String,
    pub feed_url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroSubscriptionResponse {
    pub subscribed: Vec<CastroSubscribedFeed>,
    pub latest_event_id: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroEpisodeState {
    pub episode_id: String,
    pub is_new: bool,
    pub is_starred: bool,
    pub is_played: bool,
    pub last_played: Option<String>,
    pub progress_seconds: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroPodcastState {
    pub public_id: String,
    pub episode_states: Vec<CastroEpisodeState>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroProfileSubscription {
    pub podcast_id: String,
    pub private: bool,
    pub will_notify_device: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroQueueItem {
    pub fractional_position: String,
    pub episode_id: String,
    pub podcast_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroQueue {
    pub queue_items: Vec<CastroQueueItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroArtworkUrls {
    pub large: String,
    pub medium: String,
    pub small: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroPodcastSearchResult {
    pub artwork_url: CastroArtworkUrls,
    pub author: Option<String>,
    pub explicit: String,
    pub feed_url: String,
    pub itunes_id: i64,
    pub last_episode_date: Option<String>,
    pub result_position: u64,
    pub summary: Option<String>,
    pub tentacles_id: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroEpisodeSearchResult {
    pub artwork_url: Option<String>,
    pub author: Option<String>,
    pub podcast_artwork_url: Option<String>,
    pub podcast_name: String,
    pub published_at: String,
    pub tentacles_id: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroDuration {
    /// Negative while Castro has not discovered the duration yet.
    pub seconds: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CastroMediaSize {
    Bytes(f64),
    Object { bytes: f64 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroEpisode {
    pub guid: String,
    pub public_id: String,
    pub short_id: String,
    pub title: String,
    pub media_size: Option<CastroMediaSize>,
    pub media_url: String,
    pub artwork_url: Option<String>,
    pub author_name: Option<String>,
    pub link_url: Option<String>,
    pub duration: CastroDuration,
    pub description: String,
    pub published_at: String,
    pub predecessor_public_id: Option<String>,
    pub season_number: Option<i64>,
    pub episode_number: Option<i64>,
    pub episode_type: String,
    pub people: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroPodcast {
    pub public_id: String,
    pub short_id: String,
    pub title: String,
    pub sort_title: String,
    pub site_url: Option<String>,
    pub description: String,
    pub author_name: Option<String>,
    pub artwork_url: Option<String>,
    pub last_event_number: u64,
    pub podcast_type: String,
    pub itunes_category: Option<String>,
    pub itunes_subcategory: Option<String>,
    pub private: bool,
    pub funding_text: String,
    pub funding_url: Option<String>,
    pub episodes: Vec<CastroEpisode>,
    pub people: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroQueueEventData {
    pub fractional_position: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroLastPlayedEventData {
    pub last_played: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CastroProgressEventData {
    pub seconds: f64,
}

/// A protocol payload that failed decoding or a refinement.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProtocolError(pub String);

/// An RFC 9562 layout with version 1-8 and the RFC
/// variant, or the nil/max UUID.
pub fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    if lower == "00000000-0000-0000-0000-000000000000"
        || lower == "ffffffff-ffff-ffff-ffff-ffffffffffff"
    {
        return true;
    }
    let hyphens = [8, 13, 18, 23];
    for (i, b) in bytes.iter().enumerate() {
        if hyphens.contains(&i) {
            if *b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    matches!(bytes[14], b'1'..=b'8')
        && matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
}

/// Anything the WHATWG URL parser accepts.
pub fn is_url(value: &str) -> bool {
    url::Url::parse(value).is_ok()
}

/// Shape and refinement checks applied after serde decoding.
pub trait Validate {
    fn validate(&self) -> Result<(), String>;
}

fn check(ok: bool, what: &str) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(format!("invalid {what}"))
    }
}

fn check_opt_url(value: Option<&str>, what: &str) -> Result<(), String> {
    value.map_or(Ok(()), |v| check(is_url(v), what))
}

impl<T: Validate> Validate for Vec<T> {
    fn validate(&self) -> Result<(), String> {
        self.iter().try_for_each(Validate::validate)
    }
}

impl Validate for CastroAction {
    fn validate(&self) -> Result<(), String> {
        check(is_uuid(&self.episode_id), "episode_id")?;
        check(is_uuid(&self.origin_event_id), "origin_event_id")?;
        check(self.origin_timestamp > 0, "origin_timestamp")
    }
}

impl Validate for CastroActionBatch {
    fn validate(&self) -> Result<(), String> {
        check(!self.actions.is_empty(), "actions (expected at least one)")?;
        self.actions.validate()
    }
}

impl Validate for CastroSyncStatus {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

impl Validate for CastroEventsResponse {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

impl Validate for CastroUserEventsResponse {
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

impl Validate for CastroSubscriptionResponse {
    fn validate(&self) -> Result<(), String> {
        self.subscribed.iter().try_for_each(|feed| {
            check(is_uuid(&feed.feed_id), "feed_id")?;
            check(is_url(&feed.feed_url), "feed_url")
        })
    }
}

impl Validate for CastroPodcastState {
    fn validate(&self) -> Result<(), String> {
        check(is_uuid(&self.public_id), "public_id")?;
        self.episode_states.iter().try_for_each(|state| {
            check(is_uuid(&state.episode_id), "episode_id")?;
            check(state.progress_seconds >= 0.0, "progress_seconds")
        })
    }
}

impl Validate for CastroProfileSubscription {
    fn validate(&self) -> Result<(), String> {
        check(is_uuid(&self.podcast_id), "podcast_id")
    }
}

impl Validate for CastroQueue {
    fn validate(&self) -> Result<(), String> {
        self.queue_items.iter().try_for_each(|item| {
            check(!item.fractional_position.is_empty(), "fractional_position")?;
            check(is_uuid(&item.episode_id), "episode_id")?;
            check(is_uuid(&item.podcast_id), "podcast_id")
        })
    }
}

impl Validate for CastroPodcastSearchResult {
    fn validate(&self) -> Result<(), String> {
        check(is_url(&self.artwork_url.large), "artwork_url.large")?;
        check(is_url(&self.artwork_url.medium), "artwork_url.medium")?;
        check(is_url(&self.artwork_url.small), "artwork_url.small")?;
        check(is_url(&self.feed_url), "feed_url")?;
        check(is_uuid(&self.tentacles_id), "tentacles_id")
    }
}

impl Validate for CastroEpisodeSearchResult {
    fn validate(&self) -> Result<(), String> {
        check_opt_url(self.artwork_url.as_deref(), "artwork_url")?;
        check_opt_url(self.podcast_artwork_url.as_deref(), "podcast_artwork_url")?;
        check(is_uuid(&self.tentacles_id), "tentacles_id")
    }
}

impl Validate for CastroEpisode {
    fn validate(&self) -> Result<(), String> {
        check(is_uuid(&self.public_id), "public_id")?;
        if let Some(CastroMediaSize::Object { bytes }) = &self.media_size {
            check(*bytes >= 0.0, "media_size.bytes")?;
        }
        check(is_url(&self.media_url), "media_url")?;
        check_opt_url(self.artwork_url.as_deref(), "artwork_url")?;
        // `link_url` is the feed's free-form item link and Omni never follows
        // it; one malformed value used to fail the whole podcast, so its
        // episodes could not be matched or queued.
        if let Some(predecessor) = &self.predecessor_public_id {
            check(is_uuid(predecessor), "predecessor_public_id")?;
        }
        Ok(())
    }
}

impl Validate for CastroPodcast {
    fn validate(&self) -> Result<(), String> {
        check(is_uuid(&self.public_id), "public_id")?;
        check_opt_url(self.site_url.as_deref(), "site_url")?;
        check_opt_url(self.artwork_url.as_deref(), "artwork_url")?;
        check_opt_url(self.funding_url.as_deref(), "funding_url")?;
        self.episodes.validate()
    }
}

impl Validate for CastroQueueEventData {
    fn validate(&self) -> Result<(), String> {
        check(!self.fractional_position.is_empty(), "fractional_position")
    }
}

impl Validate for CastroLastPlayedEventData {
    fn validate(&self) -> Result<(), String> {
        check(self.last_played > 0, "last_played")
    }
}

impl Validate for CastroProgressEventData {
    fn validate(&self) -> Result<(), String> {
        check(self.seconds >= 0.0, "seconds")
    }
}

/// Decodes and validates a protocol value.
pub fn decode<T: DeserializeOwned + Validate>(value: Value) -> Result<T, ProtocolError> {
    let decoded: T = serde_json::from_value(value).map_err(|e| ProtocolError(e.to_string()))?;
    decoded.validate().map_err(ProtocolError)?;
    Ok(decoded)
}

/// Decodes and validates a JSON text body.
pub fn decode_str<T: DeserializeOwned + Validate>(body: &str) -> Result<T, ProtocolError> {
    let value: Value = serde_json::from_str(body).map_err(|e| ProtocolError(e.to_string()))?;
    decode(value)
}

/// The action's string-encoded `event_data`.
pub fn parse_castro_event_data<T: DeserializeOwned + Validate>(
    action: &CastroAction,
) -> Result<T, ProtocolError> {
    let Some(data) = &action.event_data else {
        return Err(ProtocolError(format!(
            "{} has no event_data",
            action.action_type.as_str()
        )));
    };
    decode_str(data)
}
