//! Bridge to the user's podcast client account.
//!
//! Reads return [`FetchResult`]: an unavailable account MUST stay
//! distinguishable from an empty list (three-state rule), so callers abort
//! decisions that would be wrong against missing state. Shows are identified
//! by feed URL and/or iTunes id whenever possible; titles are display data.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

/// The account could not be read; never an empty result.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct Unavailable {
    pub reason: String,
}

impl Unavailable {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// `Ok`, or unavailable with a reason.
pub type FetchResult<T> = Result<T, Unavailable>;

/// A subscribed show.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodcastSubscription {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub itunes_id: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodcastSearchResult {
    pub client_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub feed_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub itunes_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodcastEpisodeSearchResult {
    pub client_id: String,
    pub title: String,
    pub show_title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_url: Option<String>,
}

/// A playback state for one episode; newest wins.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenedEpisode {
    pub show_title: String,
    pub episode_title: String,
    /// RSS item GUID when known, else a stable client-native id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_guid: Option<String>,
    /// Enclosure URL, for clients that rewrite the RSS guid. Not part of the MCP output.
    #[serde(skip)]
    pub media_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub itunes_id: Option<i64>,
    /// Epoch ms of the most recent playback activity.
    pub listened_at: i64,
    /// 0-1 fraction listened, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starred: Option<bool>,
}

/// An episode in the play queue.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedEpisode {
    pub show_title: String,
    pub episode_title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_guid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<i64>,
}

/// An episode visible in the client's Inbox.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxEpisode {
    /// Client-native id used for state mutations.
    pub client_episode_id: String,
    pub show_title: String,
    pub episode_title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_guid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PodcastWriteResult {
    Added,
    Removed,
    AlreadyExists,
    NotFound,
    Unavailable,
    Error,
}

impl PodcastWriteResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::AlreadyExists => "already_exists",
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PodcastQueuePosition {
    #[default]
    Next,
    Last,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EnqueueEpisodeRequest {
    pub feed_url: String,
    pub itunes_id: Option<i64>,
    /// RSS item GUID of the episode to queue.
    pub episode_guid: String,
    /// Enclosure URL, the preferred episode key (hosts rewrite guids).
    pub media_url: Option<String>,
    pub show_title: String,
    pub episode_title: String,
    pub position: Option<PodcastQueuePosition>,
}

/// How a client matched a requested episode to its own episode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeMatchKind {
    Guid,
    Enclosure,
    /// A unique normalized title: not an exact identity.
    Title,
}

/// A read-only episode lookup in the client's catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EpisodeLookup {
    Found {
        client_episode_id: String,
        matched_by: EpisodeMatchKind,
    },
    ShowNotFound,
    EpisodeNotFound,
    /// The client cannot look episodes up without changing state.
    Unsupported,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubscribeToShowRequest {
    pub title: String,
    pub feed_url: String,
    pub itunes_id: Option<i64>,
}

/// The podcast account client. Writes report their idempotent outcome and never fail.
pub trait PodcastAccount: Send + Sync {
    /// Human-readable client name for logs (e.g. "Castro").
    fn name(&self) -> &str;
    fn fetch_subscriptions(&self) -> BoxFuture<'_, FetchResult<Vec<PodcastSubscription>>>;
    /// Playback history, newest first; `since_ms` bounds the look-back.
    fn fetch_listen_history(
        &self,
        since_ms: Option<i64>,
    ) -> BoxFuture<'_, FetchResult<Vec<ListenedEpisode>>>;
    fn fetch_queue(&self) -> BoxFuture<'_, FetchResult<Vec<QueuedEpisode>>>;
    fn fetch_inbox(&self) -> BoxFuture<'_, FetchResult<Vec<InboxEpisode>>>;
    fn search_podcasts<'a>(
        &'a self,
        query: &'a str,
    ) -> BoxFuture<'a, FetchResult<Vec<PodcastSearchResult>>>;
    fn search_episodes<'a>(
        &'a self,
        query: &'a str,
    ) -> BoxFuture<'a, FetchResult<Vec<PodcastEpisodeSearchResult>>>;
    /// Idempotency: `AlreadyExists` when queued.
    fn enqueue_episode(&self, request: EnqueueEpisodeRequest) -> BoxFuture<'_, PodcastWriteResult>;
    /// Idempotency: `NotFound` when absent.
    fn dequeue_episode<'a>(&'a self, episode_guid: &'a str) -> BoxFuture<'a, PodcastWriteResult>;
    fn clear_inbox_episode<'a>(
        &'a self,
        client_episode_id: &'a str,
    ) -> BoxFuture<'a, PodcastWriteResult>;
    fn subscribe_to_show(
        &self,
        request: SubscribeToShowRequest,
    ) -> BoxFuture<'_, PodcastWriteResult>;
    /// Finds the client's episode for an enqueue request without queueing it.
    fn find_episode(
        &self,
        _request: EnqueueEpisodeRequest,
    ) -> BoxFuture<'_, FetchResult<EpisodeLookup>> {
        Box::pin(async { Ok(EpisodeLookup::Unsupported) })
    }
}

/// Resolves the configured account for one run (a fresh client with fresh
/// metadata caches each time, over the shared paced API), or `None`.
pub trait AccountProvider: Send + Sync {
    fn resolve(&self) -> Option<Arc<dyn PodcastAccount>>;
}

/// A provider that always returns the same account (tests and overrides).
pub struct FixedAccount(pub Option<Arc<dyn PodcastAccount>>);

impl AccountProvider for FixedAccount {
    fn resolve(&self) -> Option<Arc<dyn PodcastAccount>> {
        self.0.clone()
    }
}
