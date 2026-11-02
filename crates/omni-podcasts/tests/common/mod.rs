//! Shared test fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use omni_podcasts::persistence::{PodcastRecommendationData, PodcastRecommendationStatus};
use omni_store::cbor::Extra;

pub const NOW: i64 = 1_784_160_000_000; // Date.UTC(2026, 6, 16)
pub const DAY: i64 = 24 * 60 * 60 * 1000;

/// A notified recommendation, three days old.
pub fn rec() -> PodcastRecommendationData {
    PodcastRecommendationData {
        recommendation_id: "r1".into(),
        episode_id: "itunes:1#guid-1".into(),
        show_id: "itunes:1".into(),
        show_title: "The Gray Area".into(),
        episode_title: "What is consciousness?".into(),
        feed_url: "https://feeds.example.com/grayarea".into(),
        itunes_id: None,
        artwork_url: None,
        episode_guid: "guid-1".into(),
        media_url: None,
        episode_url: None,
        published_at: NOW - 3 * DAY,
        duration_minutes: None,
        status: PodcastRecommendationStatus::Notified,
        why_for_user: None,
        caveats: None,
        confidence: None,
        show_genres: None,
        discovered_via: None,
        source_url: None,
        matched_voices: None,
        shortlist_scores: None,
        run_date: "2026-07-13".into(),
        recommended_at: NOW - 3 * DAY,
        notified_at: None,
        queue_result: None,
        resolved_at: None,
        feedback: None,
        feedback_at: None,
        feedback_note: None,
        origin: None,
        chat_record: None,
        extra: Extra::default(),
    }
}

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use omni_podcasts::account::{
    AccountProvider, EnqueueEpisodeRequest, EpisodeLookup, FetchResult, InboxEpisode,
    ListenedEpisode, PodcastAccount, PodcastEpisodeSearchResult, PodcastSearchResult,
    PodcastSubscription, PodcastWriteResult, QueuedEpisode, SubscribeToShowRequest,
};

/// A scripted `PodcastAccountClient` that records every call.
pub struct FakeAccount {
    pub subscriptions: FetchResult<Vec<PodcastSubscription>>,
    pub history: FetchResult<Vec<ListenedEpisode>>,
    pub queue: FetchResult<Vec<QueuedEpisode>>,
    pub inbox: FetchResult<Vec<InboxEpisode>>,
    pub search: FetchResult<Vec<PodcastSearchResult>>,
    pub enqueue_result: PodcastWriteResult,
    pub clear_result: PodcastWriteResult,
    /// `None` keeps the trait's default (unsupported) lookup.
    pub episode_lookup: Option<FetchResult<EpisodeLookup>>,
    pub calls: Mutex<Vec<String>>,
    pub enqueued: Mutex<Vec<EnqueueEpisodeRequest>>,
}

impl Default for FakeAccount {
    fn default() -> Self {
        Self {
            subscriptions: Ok(Vec::new()),
            history: Ok(Vec::new()),
            queue: Ok(Vec::new()),
            inbox: Ok(Vec::new()),
            search: Ok(Vec::new()),
            enqueue_result: PodcastWriteResult::Added,
            clear_result: PodcastWriteResult::Removed,
            episode_lookup: None,
            calls: Mutex::new(Vec::new()),
            enqueued: Mutex::new(Vec::new()),
        }
    }
}

impl FakeAccount {
    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn count(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
}

impl PodcastAccount for FakeAccount {
    fn name(&self) -> &str {
        "Castro"
    }
    fn fetch_subscriptions(&self) -> BoxFuture<'_, FetchResult<Vec<PodcastSubscription>>> {
        self.record("fetch_subscriptions".into());
        Box::pin(async { self.subscriptions.clone() })
    }
    fn fetch_listen_history(
        &self,
        since_ms: Option<i64>,
    ) -> BoxFuture<'_, FetchResult<Vec<ListenedEpisode>>> {
        self.record(format!("fetch_listen_history:{since_ms:?}"));
        Box::pin(async { self.history.clone() })
    }
    fn fetch_queue(&self) -> BoxFuture<'_, FetchResult<Vec<QueuedEpisode>>> {
        self.record("fetch_queue".into());
        Box::pin(async { self.queue.clone() })
    }
    fn fetch_inbox(&self) -> BoxFuture<'_, FetchResult<Vec<InboxEpisode>>> {
        self.record("fetch_inbox".into());
        Box::pin(async { self.inbox.clone() })
    }
    fn search_podcasts<'a>(
        &'a self,
        query: &'a str,
    ) -> BoxFuture<'a, FetchResult<Vec<PodcastSearchResult>>> {
        self.record(format!("search_podcasts:{query}"));
        Box::pin(async { self.search.clone() })
    }
    fn search_episodes<'a>(
        &'a self,
        query: &'a str,
    ) -> BoxFuture<'a, FetchResult<Vec<PodcastEpisodeSearchResult>>> {
        self.record(format!("search_episodes:{query}"));
        Box::pin(async { Ok(Vec::new()) })
    }
    fn enqueue_episode(&self, request: EnqueueEpisodeRequest) -> BoxFuture<'_, PodcastWriteResult> {
        self.record("enqueue_episode".into());
        self.enqueued.lock().unwrap().push(request);
        Box::pin(async { self.enqueue_result })
    }
    fn dequeue_episode<'a>(&'a self, episode_guid: &'a str) -> BoxFuture<'a, PodcastWriteResult> {
        self.record(format!("dequeue_episode:{episode_guid}"));
        Box::pin(async { PodcastWriteResult::Removed })
    }
    fn clear_inbox_episode<'a>(&'a self, id: &'a str) -> BoxFuture<'a, PodcastWriteResult> {
        self.record(format!("clear_inbox_episode:{id}"));
        Box::pin(async { self.clear_result })
    }
    fn subscribe_to_show(
        &self,
        request: SubscribeToShowRequest,
    ) -> BoxFuture<'_, PodcastWriteResult> {
        self.record(format!("subscribe_to_show:{}", request.feed_url));
        Box::pin(async { PodcastWriteResult::Added })
    }
    fn find_episode(
        &self,
        request: EnqueueEpisodeRequest,
    ) -> BoxFuture<'_, FetchResult<EpisodeLookup>> {
        self.record(format!("find_episode:{}", request.episode_guid));
        let scripted = self.episode_lookup.clone();
        Box::pin(async move { scripted.unwrap_or(Ok(EpisodeLookup::Unsupported)) })
    }
}

/// Always resolves the same fake account.
pub struct FakeAccounts(pub Arc<FakeAccount>);

impl AccountProvider for FakeAccounts {
    fn resolve(&self) -> Option<Arc<dyn PodcastAccount>> {
        Some(self.0.clone())
    }
}

use std::collections::VecDeque;

use omni_podcasts::itunes::ItunesShow;
use omni_podcasts::rss::FeedEpisode;
use omni_podcasts::sources::ShowDirectory;

/// A scripted iTunes + RSS directory.
#[derive(Default)]
pub struct FakeDirectory {
    pub shows: Vec<ItunesShow>,
    /// Popped per feed fetch; the last entry repeats once the script runs out.
    pub feeds: Mutex<VecDeque<Result<Vec<FeedEpisode>, String>>>,
    pub fetched: Mutex<Vec<String>>,
}

impl FakeDirectory {
    pub fn with_feeds(
        shows: Vec<ItunesShow>,
        feeds: Vec<Result<Vec<FeedEpisode>, String>>,
    ) -> Self {
        Self {
            shows,
            feeds: Mutex::new(feeds.into()),
            fetched: Mutex::new(Vec::new()),
        }
    }
}

impl ShowDirectory for FakeDirectory {
    fn search_itunes<'a>(
        &'a self,
        _term: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ItunesShow>, String>> {
        Box::pin(async { Ok(self.shows.clone()) })
    }

    fn fetch_feed<'a>(
        &'a self,
        feed_url: &'a str,
        _max: usize,
    ) -> BoxFuture<'a, Result<Vec<FeedEpisode>, String>> {
        self.fetched.lock().unwrap().push(feed_url.to_owned());
        let next = {
            let mut feeds = self.feeds.lock().unwrap();
            if feeds.len() > 1 {
                feeds.pop_front()
            } else {
                feeds.front().cloned()
            }
        };
        Box::pin(async move { next.unwrap_or_else(|| Err("no feed scripted".into())) })
    }
}

pub fn feed_episode() -> FeedEpisode {
    FeedEpisode {
        guid: "rss-guid".into(),
        title: "Ep".into(),
        published_at: 1_700_000_000_000,
        duration_minutes: None,
        description: "desc".into(),
        link: Some("https://show/ep".into()),
        enclosure_url: Some("https://cdn/x.mp3".into()),
    }
}
