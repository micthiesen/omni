//! [`PodcastAccount`] over the Castro sync protocol.
//!
//! Each client keeps its own metadata caches (podcast/episode/search 1 h,
//! subscriptions 15 min), so a long-lived instance never serves a stale
//! episode list beyond that; only the HTTP layer is shared. Action ids are a
//! process-wide Lamport clock and queue placement is serialized process-wide,
//! because recommendations, cleanup and MCP clients can overlap.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use jiff::tz::TimeZone;
use moka::future::Cache;
use omni_core::clock::SharedClock;
use omni_core::js::date_parse;

use super::api::{CastroFailure, CastroRequestError, CastroTransport};
use super::fractional::generate_key_between;
use super::protocol::{
    CastroAction, CastroActionSource, CastroActionType, CastroEpisode, CastroPodcast,
    CastroPodcastSearchResult, CastroPodcastState, CastroProfileSubscription, CastroQueueEventData,
    CastroQueueItem,
};
use crate::account::{
    EnqueueEpisodeRequest, FetchResult, InboxEpisode, ListenedEpisode, PodcastAccount,
    PodcastEpisodeSearchResult, PodcastQueuePosition, PodcastSearchResult, PodcastSubscription,
    PodcastWriteResult, QueuedEpisode, SubscribeToShowRequest, Unavailable,
};
use crate::titles::normalize_title;
use crate::types::normalize_feed_url;

const LOG: &str = "Castro";
const HISTORY_WINDOW_MS: i64 = 180 * 24 * 60 * 60 * 1000;
const READ_CONCURRENCY: usize = 8;
const METADATA_TTL: Duration = Duration::from_secs(60 * 60);
const SUBSCRIPTIONS_TTL: Duration = Duration::from_secs(15 * 60);

/// Process-wide Lamport action ids.
static ACTION_IDS: Mutex<i64> = Mutex::new(0);
/// Queue read/decision/write is serialized across every client instance.
static ENQUEUE_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Errors inside a Castro operation (request failures or protocol misuse).
#[derive(Debug, Clone, thiserror::Error)]
enum ClientError {
    #[error(transparent)]
    Request(#[from] CastroRequestError),
    #[error("{0}")]
    Other(String),
}

impl From<Arc<CastroRequestError>> for ClientError {
    fn from(e: Arc<CastroRequestError>) -> Self {
        ClientError::Request((*e).clone())
    }
}

fn unavailable(e: &ClientError) -> Unavailable {
    Unavailable::new(e.to_string())
}

pub struct CastroClient {
    api: Arc<dyn CastroTransport>,
    clock: SharedClock,
    podcasts: Cache<String, CastroPodcast>,
    episodes: Cache<String, CastroEpisode>,
    searches: Cache<String, Vec<CastroPodcastSearchResult>>,
    subscriptions: Cache<(), Vec<CastroProfileSubscription>>,
}

impl CastroClient {
    pub fn new(api: Arc<dyn CastroTransport>, clock: SharedClock) -> Self {
        Self {
            api,
            clock,
            podcasts: Cache::builder()
                .max_capacity(500)
                .time_to_live(METADATA_TTL)
                .build(),
            episodes: Cache::builder()
                .max_capacity(2_000)
                .time_to_live(METADATA_TTL)
                .build(),
            searches: Cache::builder()
                .max_capacity(200)
                .time_to_live(METADATA_TTL)
                .build(),
            subscriptions: Cache::builder()
                .max_capacity(1)
                .time_to_live(SUBSCRIPTIONS_TTL)
                .build(),
        }
    }

    async fn get_subscriptions(&self) -> Result<Vec<CastroProfileSubscription>, ClientError> {
        let api = self.api.clone();
        Ok(self
            .subscriptions
            .try_get_with((), async move { api.fetch_subscriptions().await })
            .await?)
    }

    async fn fetch_podcast(&self, public_id: &str) -> Result<CastroPodcast, ClientError> {
        let api = self.api.clone();
        let id = public_id.to_owned();
        Ok(self
            .podcasts
            .try_get_with(
                public_id.to_owned(),
                async move { api.fetch_podcast(&id).await },
            )
            .await?)
    }

    async fn fetch_episode(&self, public_id: &str) -> Result<CastroEpisode, ClientError> {
        let api = self.api.clone();
        let id = public_id.to_owned();
        Ok(self
            .episodes
            .try_get_with(
                public_id.to_owned(),
                async move { api.fetch_episode(&id).await },
            )
            .await?)
    }

    /// The episode, or `None` when Castro no longer serves it (HTTP 404). A
    /// removed episode can linger in the inbox or queue state; one such id
    /// must not fail the whole listing.
    async fn fetch_live_episode(
        &self,
        public_id: &str,
    ) -> Result<Option<CastroEpisode>, ClientError> {
        match self.fetch_episode(public_id).await {
            Ok(episode) => Ok(Some(episode)),
            Err(ClientError::Request(CastroRequestError {
                cause: CastroFailure::Status { status: 404, .. },
                ..
            })) => {
                tracing::debug!(target: LOG, "Skipping removed Castro episode {public_id}");
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    async fn search_podcast_metadata(
        &self,
        query: &str,
    ) -> Result<Vec<CastroPodcastSearchResult>, ClientError> {
        let api = self.api.clone();
        let term = query.to_owned();
        Ok(self
            .searches
            .try_get_with(
                query.to_owned(),
                async move { api.search_podcasts(&term).await },
            )
            .await?)
    }

    async fn find_podcast_search_result(
        &self,
        query: &str,
        predicate: impl Fn(&CastroPodcastSearchResult) -> bool,
    ) -> Result<Option<CastroPodcastSearchResult>, ClientError> {
        Ok(self
            .search_podcast_metadata(query)
            .await?
            .into_iter()
            .find(|r| predicate(r)))
    }

    async fn resolve_podcast(
        &self,
        feed_url: &str,
        itunes_id: Option<i64>,
    ) -> Result<Option<CastroPodcastSearchResult>, ClientError> {
        let normalized = normalize_feed_url(feed_url);
        self.find_podcast_search_result(feed_url, |result| {
            normalize_feed_url(&result.feed_url) == normalized
                || itunes_id.is_some_and(|id| result.itunes_id == id)
        })
        .await
    }

    fn action(
        &self,
        episode_id: &str,
        action_type: CastroActionType,
        timestamp: i64,
        event_data: Option<String>,
    ) -> CastroAction {
        let id = {
            let mut previous = ACTION_IDS.lock().unwrap_or_else(|p| p.into_inner());
            let id = (*previous + 1).max(timestamp);
            *previous = id;
            id
        };
        CastroAction {
            id: u64::try_from(id).unwrap_or(0),
            episode_id: episode_id.to_owned(),
            origin_event_id: omni_core::ids::uuid_v4(),
            origin_timestamp: u64::try_from(timestamp).unwrap_or(0),
            source: CastroActionSource::User,
            action_type,
            event_data,
        }
    }

    async fn subscriptions_impl(&self) -> Result<Vec<PodcastSubscription>, ClientError> {
        let subscriptions = self.get_subscriptions().await?;
        let podcasts: Vec<CastroPodcast> = ordered(
            subscriptions
                .iter()
                .map(|s| Box::pin(self.fetch_podcast(&s.podcast_id)) as BoxFuture<'_, _>)
                .collect(),
        )
        .await?;
        let lookups: Vec<BoxFuture<'_, PodcastSubscription>> = podcasts
            .iter()
            .map(|podcast| {
                Box::pin(async move {
                    let matched = self
                        .find_podcast_search_result(&podcast.title, |r| {
                            r.tentacles_id == podcast.public_id
                        })
                        .await
                        .ok()
                        .flatten();
                    PodcastSubscription {
                        title: podcast.title.clone(),
                        feed_url: matched.as_ref().map(|m| m.feed_url.clone()),
                        itunes_id: matched.as_ref().map(|m| m.itunes_id),
                    }
                }) as BoxFuture<'_, PodcastSubscription>
            })
            .collect();
        Ok(crate::concurrency::buffered(lookups, READ_CONCURRENCY).await)
    }

    async fn listen_history_impl(
        &self,
        since_ms: Option<i64>,
    ) -> Result<Vec<ListenedEpisode>, ClientError> {
        let subscriptions = self.get_subscriptions().await?;
        let states: Vec<(CastroPodcast, CastroPodcastState)> = ordered(
            subscriptions
                .iter()
                .map(|subscription| {
                    Box::pin(async move {
                        let (podcast, state) = futures::try_join!(
                            self.fetch_podcast(&subscription.podcast_id),
                            async {
                                Ok::<_, ClientError>(
                                    self.api
                                        .fetch_podcast_state(&subscription.podcast_id)
                                        .await?,
                                )
                            }
                        )?;
                        Ok::<_, ClientError>((podcast, state))
                    }) as BoxFuture<'_, _>
                })
                .collect(),
        )
        .await?;
        let now = self.clock.now_ms();
        let cutoff = since_ms.map_or(now - HISTORY_WINDOW_MS, |since| {
            since.max(now - HISTORY_WINDOW_MS)
        });
        let recent: Vec<_> = states
            .iter()
            .flat_map(|(podcast, state)| {
                state
                    .episode_states
                    .iter()
                    .filter_map(move |episode_state| {
                        let played = episode_state.last_played.as_deref()?;
                        let listened_at = date_parse(played, &TimeZone::UTC)?;
                        (listened_at >= cutoff).then_some((podcast, episode_state, listened_at))
                    })
            })
            .collect();
        let history: Vec<Option<ListenedEpisode>> = ordered(
            recent
                .into_iter()
                .map(|(podcast, state, listened_at)| {
                    Box::pin(async move {
                        // Drop a play of a removed episode instead of losing the
                        // whole history.
                        let Some(episode) = self.fetch_live_episode(&state.episode_id).await?
                        else {
                            return Ok::<_, ClientError>(None);
                        };
                        let completion = if state.is_played {
                            Some(1.0)
                        } else if episode.duration.seconds > 0.0 {
                            Some(
                                (state.progress_seconds / episode.duration.seconds).clamp(0.0, 1.0),
                            )
                        } else {
                            None
                        };
                        Ok(Some(ListenedEpisode {
                            show_title: podcast.title.clone(),
                            episode_title: episode.title.clone(),
                            episode_guid: Some(guid_or_public_id(&episode)),
                            media_url: Some(episode.media_url.clone()),
                            feed_url: None,
                            itunes_id: None,
                            listened_at,
                            completion,
                            starred: Some(state.is_starred),
                        }))
                    }) as BoxFuture<'_, _>
                })
                .collect(),
        )
        .await?;
        let mut history: Vec<ListenedEpisode> = history.into_iter().flatten().collect();
        history.sort_by_key(|h| std::cmp::Reverse(h.listened_at));
        Ok(history)
    }

    async fn queue_impl(&self) -> Result<Vec<QueuedEpisode>, ClientError> {
        let mut items = self.api.fetch_queue().await?.queue_items;
        items.sort_by(|a, b| a.fractional_position.cmp(&b.fractional_position));
        let queued: Vec<Option<QueuedEpisode>> = ordered(
            items
                .iter()
                .map(|item| {
                    Box::pin(async move {
                        let (podcast, episode) = futures::try_join!(
                            self.fetch_podcast(&item.podcast_id),
                            self.fetch_live_episode(&item.episode_id)
                        )?;
                        Ok::<_, ClientError>(episode.map(|episode| QueuedEpisode {
                            show_title: podcast.title.clone(),
                            episode_title: episode.title.clone(),
                            episode_guid: Some(guid_or_public_id(&episode)),
                            feed_url: None,
                            description: Some(episode.description.clone()),
                            added_at: None,
                        }))
                    }) as BoxFuture<'_, _>
                })
                .collect(),
        )
        .await?;
        Ok(queued.into_iter().flatten().collect())
    }

    async fn inbox_impl(&self) -> Result<Vec<InboxEpisode>, ClientError> {
        let subscriptions = self.get_subscriptions().await?;
        let states: Vec<(String, CastroPodcastState)> = ordered(
            subscriptions
                .iter()
                .map(|subscription| {
                    Box::pin(async move {
                        let state = self
                            .api
                            .fetch_podcast_state(&subscription.podcast_id)
                            .await?;
                        Ok::<_, ClientError>((subscription.podcast_id.clone(), state))
                    }) as BoxFuture<'_, _>
                })
                .collect(),
        )
        .await?;
        let new_episodes: Vec<(String, String)> = states
            .iter()
            .flat_map(|(podcast_id, state)| {
                state
                    .episode_states
                    .iter()
                    .filter(|s| s.is_new)
                    .map(move |s| (podcast_id.clone(), s.episode_id.clone()))
            })
            .collect();
        let inbox: Vec<Option<InboxEpisode>> = ordered(
            new_episodes
                .iter()
                .map(|(podcast_id, episode_id)| {
                    Box::pin(async move {
                        let (podcast, episode) = futures::try_join!(
                            self.fetch_podcast(podcast_id),
                            self.fetch_live_episode(episode_id)
                        )?;
                        Ok::<_, ClientError>(episode.map(|episode| InboxEpisode {
                            client_episode_id: episode.public_id.clone(),
                            show_title: podcast.title.clone(),
                            episode_title: episode.title.clone(),
                            episode_guid: Some(guid_or_public_id(&episode)),
                            description: Some(episode.description.clone()),
                        }))
                    }) as BoxFuture<'_, _>
                })
                .collect(),
        )
        .await?;
        Ok(inbox.into_iter().flatten().collect())
    }

    async fn enqueue_impl(
        &self,
        request: EnqueueEpisodeRequest,
    ) -> Result<PodcastWriteResult, ClientError> {
        let _guard = ENQUEUE_LOCK.lock().await;
        let Some(resolved) = self
            .resolve_podcast(&request.feed_url, request.itunes_id)
            .await?
        else {
            return Ok(PodcastWriteResult::NotFound);
        };
        let podcast = self.fetch_podcast(&resolved.tentacles_id).await?;
        let Some(episode) = match_episode(&podcast.episodes, &request) else {
            return Ok(PodcastWriteResult::NotFound);
        };
        let queue = self.api.fetch_queue().await?;
        if queue
            .queue_items
            .iter()
            .any(|item| item.episode_id == episode.public_id)
        {
            return Ok(PodcastWriteResult::AlreadyExists);
        }
        let mut positions: Vec<&str> = queue
            .queue_items
            .iter()
            .map(|item| item.fractional_position.as_str())
            .collect();
        positions.sort_unstable();
        // "Queue Next" matches the app: after the current top item, as the new 2nd item.
        let fractional_position = match request.position.unwrap_or_default() {
            PodcastQueuePosition::Last => generate_key_between(positions.last().copied(), None),
            PodcastQueuePosition::Next => {
                generate_key_between(positions.first().copied(), positions.get(1).copied())
            }
        }
        .map_err(|e| ClientError::Other(e.to_string()))?;
        let event_data = serde_json::to_string(&CastroQueueEventData {
            fractional_position,
        })
        .map_err(|e| ClientError::Other(e.to_string()))?;
        let now = self.clock.now_ms();
        let actions = vec![
            self.action(
                &episode.public_id,
                CastroActionType::EpisodeQueued,
                now,
                Some(event_data),
            ),
            self.action(
                &episode.public_id,
                CastroActionType::ClearEpisodeNew,
                now,
                None,
            ),
        ];
        self.api.post_actions(actions).await?;
        Ok(PodcastWriteResult::Added)
    }

    async fn subscribe_impl(
        &self,
        request: SubscribeToShowRequest,
    ) -> Result<PodcastWriteResult, ClientError> {
        let Some(resolved) = self
            .resolve_podcast(&request.feed_url, request.itunes_id)
            .await?
        else {
            return Ok(PodcastWriteResult::NotFound);
        };
        let subscriptions = self.get_subscriptions().await?;
        if subscriptions
            .iter()
            .any(|s| s.podcast_id == resolved.tentacles_id)
        {
            return Ok(PodcastWriteResult::AlreadyExists);
        }
        let response = self
            .api
            .subscribe(vec![resolved.tentacles_id.clone()])
            .await?;
        Ok(
            if response
                .subscribed
                .iter()
                .any(|s| s.feed_id == resolved.tentacles_id)
            {
                PodcastWriteResult::Added
            } else {
                PodcastWriteResult::Error
            },
        )
    }

    async fn dequeue_impl(&self, episode_guid: &str) -> Result<PodcastWriteResult, ClientError> {
        let queue = self.api.fetch_queue().await?;
        let episodes: Vec<(&CastroQueueItem, Option<CastroEpisode>)> = ordered(
            queue
                .queue_items
                .iter()
                .map(|item| {
                    Box::pin(async move {
                        let episode = self.fetch_live_episode(&item.episode_id).await?;
                        Ok::<_, ClientError>((item, episode))
                    }) as BoxFuture<'_, _>
                })
                .collect(),
        )
        .await?;
        let Some((item, _)) = episodes.iter().find(|(_, episode)| {
            episode
                .as_ref()
                .is_some_and(|episode| guid_or_public_id(episode) == episode_guid)
        }) else {
            return Ok(PodcastWriteResult::NotFound);
        };
        let now = self.clock.now_ms();
        let actions = vec![
            self.action(
                &item.episode_id,
                CastroActionType::EpisodeDequeued,
                now,
                None,
            ),
            self.action(
                &item.episode_id,
                CastroActionType::ClearEpisodeNew,
                now,
                None,
            ),
        ];
        self.api.post_actions(actions).await?;
        Ok(PodcastWriteResult::Removed)
    }

    async fn clear_inbox_impl(
        &self,
        client_episode_id: &str,
    ) -> Result<PodcastWriteResult, ClientError> {
        let now = self.clock.now_ms();
        let action = self.action(
            client_episode_id,
            CastroActionType::ClearEpisodeNew,
            now,
            None,
        );
        self.api.post_actions(vec![action]).await?;
        Ok(PodcastWriteResult::Removed)
    }
}

/// Eight at a time: ordered results, first error wins.
async fn ordered<T>(
    futures: Vec<BoxFuture<'_, Result<T, ClientError>>>,
) -> Result<Vec<T>, ClientError> {
    crate::concurrency::try_buffered(futures, READ_CONCURRENCY).await
}

fn guid_or_public_id(episode: &CastroEpisode) -> String {
    if episode.guid.is_empty() {
        episode.public_id.clone()
    } else {
        episode.guid.clone()
    }
}

fn write_result(
    operation: &str,
    result: Result<PodcastWriteResult, ClientError>,
) -> PodcastWriteResult {
    result.unwrap_or_else(|error| {
        tracing::error!(target: LOG, error = %error, "{operation}");
        PodcastWriteResult::Error
    })
}

impl PodcastAccount for CastroClient {
    fn name(&self) -> &str {
        "Castro"
    }

    fn fetch_subscriptions(&self) -> BoxFuture<'_, FetchResult<Vec<PodcastSubscription>>> {
        Box::pin(async move { self.subscriptions_impl().await.map_err(|e| unavailable(&e)) })
    }

    fn fetch_listen_history(
        &self,
        since_ms: Option<i64>,
    ) -> BoxFuture<'_, FetchResult<Vec<ListenedEpisode>>> {
        Box::pin(async move {
            self.listen_history_impl(since_ms)
                .await
                .map_err(|e| unavailable(&e))
        })
    }

    fn fetch_queue(&self) -> BoxFuture<'_, FetchResult<Vec<QueuedEpisode>>> {
        Box::pin(async move { self.queue_impl().await.map_err(|e| unavailable(&e)) })
    }

    fn fetch_inbox(&self) -> BoxFuture<'_, FetchResult<Vec<InboxEpisode>>> {
        Box::pin(async move { self.inbox_impl().await.map_err(|e| unavailable(&e)) })
    }

    fn search_podcasts<'a>(
        &'a self,
        query: &'a str,
    ) -> BoxFuture<'a, FetchResult<Vec<PodcastSearchResult>>> {
        Box::pin(async move {
            let results = self
                .search_podcast_metadata(query)
                .await
                .map_err(|e| unavailable(&e))?;
            Ok(results
                .into_iter()
                .map(|r| PodcastSearchResult {
                    client_id: r.tentacles_id,
                    title: r.title,
                    author: r.author,
                    feed_url: r.feed_url,
                    itunes_id: Some(r.itunes_id),
                    summary: r.summary,
                    artwork_url: Some(r.artwork_url.large),
                })
                .collect())
        })
    }

    fn search_episodes<'a>(
        &'a self,
        query: &'a str,
    ) -> BoxFuture<'a, FetchResult<Vec<PodcastEpisodeSearchResult>>> {
        Box::pin(async move {
            let results = self
                .api
                .search_episodes(query)
                .await
                .map_err(|e| Unavailable::new(e.to_string()))?;
            Ok(results
                .into_iter()
                .map(|r| PodcastEpisodeSearchResult {
                    client_id: r.tentacles_id,
                    title: r.title,
                    show_title: r.podcast_name,
                    author: r.author,
                    published_at: date_parse(&r.published_at, &TimeZone::UTC),
                    artwork_url: r.artwork_url.or(r.podcast_artwork_url),
                })
                .collect())
        })
    }

    fn enqueue_episode(&self, request: EnqueueEpisodeRequest) -> BoxFuture<'_, PodcastWriteResult> {
        Box::pin(
            async move { write_result("Castro enqueue failed", self.enqueue_impl(request).await) },
        )
    }

    fn dequeue_episode<'a>(&'a self, episode_guid: &'a str) -> BoxFuture<'a, PodcastWriteResult> {
        Box::pin(async move {
            write_result(
                "Castro dequeue failed",
                self.dequeue_impl(episode_guid).await,
            )
        })
    }

    fn clear_inbox_episode<'a>(
        &'a self,
        client_episode_id: &'a str,
    ) -> BoxFuture<'a, PodcastWriteResult> {
        Box::pin(async move {
            write_result(
                super::alert_gate::INBOX_CLEAR_FAILED_TITLE,
                self.clear_inbox_impl(client_episode_id).await,
            )
        })
    }

    fn subscribe_to_show(
        &self,
        request: SubscribeToShowRequest,
    ) -> BoxFuture<'_, PodcastWriteResult> {
        Box::pin(async move {
            write_result(
                "Castro subscribe lookup failed",
                self.subscribe_impl(request).await,
            )
        })
    }
}

/// Matches a requested episode against a Castro podcast's episodes: RSS guid
/// first, then the enclosure URL (hosts rewrite guids), then a unique
/// normalized title.
pub fn match_episode<'a>(
    episodes: &'a [CastroEpisode],
    request: &EnqueueEpisodeRequest,
) -> Option<&'a CastroEpisode> {
    if let Some(by_guid) = episodes.iter().find(|e| e.guid == request.episode_guid) {
        return Some(by_guid);
    }
    if let Some(media_key) = normalize_media_url(request.media_url.as_deref())
        && let Some(by_media) = episodes.iter().find(|e| {
            normalize_media_url(Some(&e.media_url)).as_deref() == Some(media_key.as_str())
        })
    {
        return Some(by_media);
    }
    let title_key = normalize_title(&request.episode_title);
    if !title_key.is_empty() {
        let by_title: Vec<&CastroEpisode> = episodes
            .iter()
            .filter(|e| normalize_title(&e.title) == title_key)
            .collect();
        if let [only] = by_title.as_slice() {
            return Some(only);
        }
    }
    None
}

/// Enclosure URLs compared by host+path, ignoring protocol and query.
pub fn normalize_media_url(url: Option<&str>) -> Option<String> {
    let trimmed = url?.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    let no_protocol = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(&trimmed);
    Some(match no_protocol.find('?') {
        Some(i) => no_protocol[..i].to_owned(),
        None => no_protocol.to_owned(),
    })
}
