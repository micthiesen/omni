//! The Castro client over a fake [`CastroTransport`].
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::clock::SharedClock;
use omni_http::SideEffectMode;
use omni_http::public::PublicHttpClient;
use omni_podcasts::account::{
    EnqueueEpisodeRequest, PodcastAccount, PodcastQueuePosition, PodcastWriteResult,
    SubscribeToShowRequest,
};
use omni_podcasts::castro::api::{CastroRequestError, CastroTransport, shared_castro_api};
use omni_podcasts::castro::client::{CastroClient, match_episode, normalize_media_url};
use omni_podcasts::castro::protocol::{
    CastroAction, CastroActionType, CastroArtworkUrls, CastroDuration, CastroEpisode,
    CastroEpisodeSearchResult, CastroEpisodeState, CastroPodcast, CastroPodcastSearchResult,
    CastroPodcastState, CastroProfileSubscription, CastroQueue, CastroQueueEventData,
    CastroQueueItem, CastroSubscribedFeed, CastroSubscriptionResponse, parse_castro_event_data,
};

const PODCAST_ID: &str = "33333333-3333-4333-8333-333333333333";
const EPISODE_ID: &str = "11111111-1111-4111-8111-111111111111";
const FEED_URL: &str = "https://example.com/feed.xml";
const NOW: i64 = 1_784_160_000_000;
const DAY: i64 = 24 * 60 * 60 * 1000;

fn episode(public_id: &str, guid: &str, title: &str) -> CastroEpisode {
    CastroEpisode {
        guid: guid.into(),
        public_id: public_id.into(),
        short_id: "s".into(),
        title: title.into(),
        media_size: None,
        media_url: "https://cdn.example.com/audio/ep.mp3".into(),
        artwork_url: None,
        author_name: None,
        link_url: None,
        duration: CastroDuration { seconds: 100.0 },
        description: String::new(),
        published_at: "2026-07-16T17:30:00.000Z".into(),
        predecessor_public_id: None,
        season_number: None,
        episode_number: None,
        episode_type: "full".into(),
        people: Vec::new(),
    }
}

fn podcast(episodes: Vec<CastroEpisode>) -> CastroPodcast {
    CastroPodcast {
        public_id: PODCAST_ID.into(),
        short_id: "p".into(),
        title: "Example Podcast".into(),
        sort_title: "Example Podcast".into(),
        site_url: None,
        description: String::new(),
        author_name: None,
        artwork_url: None,
        last_event_number: 0,
        podcast_type: "episodic".into(),
        itunes_category: None,
        itunes_subcategory: None,
        private: false,
        funding_text: String::new(),
        funding_url: None,
        episodes,
        people: Vec::new(),
    }
}

fn search_result() -> CastroPodcastSearchResult {
    CastroPodcastSearchResult {
        artwork_url: CastroArtworkUrls {
            large: "https://example.com/large.jpg".into(),
            medium: "https://example.com/medium.jpg".into(),
            small: "https://example.com/small.jpg".into(),
        },
        author: Some("Example Author".into()),
        explicit: "clean".into(),
        feed_url: FEED_URL.into(),
        itunes_id: 1234,
        last_episode_date: Some("2026-07-16T17:30:00.000Z".into()),
        result_position: 0,
        summary: Some("Example summary".into()),
        tentacles_id: PODCAST_ID.into(),
        title: "Example Podcast".into(),
    }
}

fn subscription() -> CastroProfileSubscription {
    CastroProfileSubscription {
        podcast_id: PODCAST_ID.into(),
        private: false,
        will_notify_device: true,
    }
}

fn state(
    episode_id: &str,
    is_new: bool,
    last_played: Option<String>,
    progress: f64,
) -> CastroEpisodeState {
    CastroEpisodeState {
        episode_id: episode_id.into(),
        is_new,
        is_starred: false,
        is_played: false,
        last_played,
        progress_seconds: progress,
    }
}

fn err(path: &str) -> CastroRequestError {
    CastroRequestError {
        method: "GET",
        path_and_query: path.into(),
        cause: omni_podcasts::castro::api::CastroFailure::Http("not scripted".into()),
    }
}

/// Scripted Castro API recording every call.
#[derive(Default)]
struct FakeCastro {
    podcast: Option<CastroPodcast>,
    episodes: HashMap<String, CastroEpisode>,
    /// Episodes whose fetch fails with this HTTP status.
    episode_statuses: HashMap<String, u16>,
    subscriptions: Vec<CastroProfileSubscription>,
    podcast_state: Option<CastroPodcastState>,
    queue_items: Vec<CastroQueueItem>,
    /// After a post, the queue reports the posted episode (the serialization test).
    queue_reflects_posts: bool,
    queued: AtomicBool,
    calls: Mutex<Vec<(&'static str, String)>>,
    posted: Mutex<Vec<Vec<CastroAction>>>,
}

impl FakeCastro {
    fn standard() -> Self {
        Self {
            podcast: Some(podcast(vec![episode(
                EPISODE_ID,
                "rss-guid",
                "Example Episode",
            )])),
            ..Self::default()
        }
    }

    fn record(&self, name: &'static str, arg: &str) {
        self.calls.lock().unwrap().push((name, arg.to_owned()));
    }

    fn calls(&self, name: &str) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| *n == name)
            .map(|(_, a)| a.clone())
            .collect()
    }

    fn posted(&self) -> Vec<Vec<CastroAction>> {
        self.posted.lock().unwrap().clone()
    }
}

impl CastroTransport for FakeCastro {
    fn fetch_podcast<'a>(
        &'a self,
        public_id: &'a str,
    ) -> BoxFuture<'a, Result<CastroPodcast, CastroRequestError>> {
        self.record("fetch_podcast", public_id);
        Box::pin(async move { self.podcast.clone().ok_or_else(|| err("/podcasts")) })
    }

    fn fetch_episode<'a>(
        &'a self,
        public_id: &'a str,
    ) -> BoxFuture<'a, Result<CastroEpisode, CastroRequestError>> {
        self.record("fetch_episode", public_id);
        Box::pin(async move {
            if let Some(&status) = self.episode_statuses.get(public_id) {
                return Err(CastroRequestError {
                    method: "GET",
                    path_and_query: format!("/episodes/{public_id}"),
                    cause: omni_podcasts::castro::api::CastroFailure::Status { status, body: None },
                });
            }
            self.episodes
                .get(public_id)
                .cloned()
                .ok_or_else(|| err("/episodes"))
        })
    }

    fn search_podcasts<'a>(
        &'a self,
        term: &'a str,
    ) -> BoxFuture<'a, Result<Vec<CastroPodcastSearchResult>, CastroRequestError>> {
        self.record("search_podcasts", term);
        Box::pin(async { Ok(vec![search_result()]) })
    }

    fn search_episodes<'a>(
        &'a self,
        term: &'a str,
    ) -> BoxFuture<'a, Result<Vec<CastroEpisodeSearchResult>, CastroRequestError>> {
        self.record("search_episodes", term);
        Box::pin(async {
            Ok(vec![CastroEpisodeSearchResult {
                artwork_url: Some("https://example.com/episode.jpg".into()),
                author: Some("Example Author".into()),
                podcast_artwork_url: Some("https://example.com/podcast.jpg".into()),
                podcast_name: "Example Podcast".into(),
                published_at: "2026-07-16T17:30:00.000Z".into(),
                tentacles_id: EPISODE_ID.into(),
                title: "Example Episode".into(),
            }])
        })
    }

    fn fetch_subscriptions(
        &self,
    ) -> BoxFuture<'_, Result<Vec<CastroProfileSubscription>, CastroRequestError>> {
        self.record("fetch_subscriptions", "");
        Box::pin(async { Ok(self.subscriptions.clone()) })
    }

    fn fetch_queue(&self) -> BoxFuture<'_, Result<CastroQueue, CastroRequestError>> {
        self.record("fetch_queue", "");
        Box::pin(async {
            // Give an overlapping caller the chance to interleave.
            tokio::time::sleep(Duration::from_millis(5)).await;
            let mut items = self.queue_items.clone();
            if self.queue_reflects_posts && self.queued.load(Ordering::SeqCst) {
                items.push(CastroQueueItem {
                    episode_id: EPISODE_ID.into(),
                    podcast_id: PODCAST_ID.into(),
                    fractional_position: "a0".into(),
                });
            }
            Ok(CastroQueue { queue_items: items })
        })
    }

    fn fetch_podcast_state<'a>(
        &'a self,
        public_id: &'a str,
    ) -> BoxFuture<'a, Result<CastroPodcastState, CastroRequestError>> {
        self.record("fetch_podcast_state", public_id);
        Box::pin(async { self.podcast_state.clone().ok_or_else(|| err("/state")) })
    }

    fn post_actions(
        &self,
        actions: Vec<CastroAction>,
    ) -> BoxFuture<'_, Result<(), CastroRequestError>> {
        self.record("post_actions", "");
        self.posted.lock().unwrap().push(actions);
        self.queued.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn subscribe(
        &self,
        feed_ids: Vec<String>,
    ) -> BoxFuture<'_, Result<CastroSubscriptionResponse, CastroRequestError>> {
        self.record("subscribe", &feed_ids.join(","));
        Box::pin(async {
            Ok(CastroSubscriptionResponse {
                subscribed: vec![CastroSubscribedFeed {
                    feed_id: PODCAST_ID.into(),
                    feed_url: FEED_URL.into(),
                }],
                latest_event_id: 1,
            })
        })
    }
}

fn clock() -> SharedClock {
    omni_testkit::test_clock(NOW)
}

fn client(api: Arc<FakeCastro>) -> CastroClient {
    CastroClient::new(api, clock())
}

fn request() -> EnqueueEpisodeRequest {
    EnqueueEpisodeRequest {
        feed_url: FEED_URL.into(),
        episode_guid: "rss-guid".into(),
        show_title: "Example Podcast".into(),
        episode_title: "Example Episode".into(),
        ..EnqueueEpisodeRequest::default()
    }
}

#[test]
fn reuses_pacing_for_unchanged_credentials() {
    let http = PublicHttpClient::new(&omni_testkit::no_network());
    let a = shared_castro_api(
        &http,
        &clock(),
        SideEffectMode::Record,
        "device-a",
        "secret-a",
    );
    let b = shared_castro_api(
        &http,
        &clock(),
        SideEffectMode::Record,
        "device-a",
        "secret-a",
    );
    assert!(Arc::ptr_eq(&a, &b));
}

#[test]
fn does_not_retain_an_api_signed_with_stale_credentials() {
    let http = PublicHttpClient::new(&omni_testkit::no_network());
    let original = shared_castro_api(
        &http,
        &clock(),
        SideEffectMode::Record,
        "device-a",
        "secret-a",
    );
    let next = shared_castro_api(
        &http,
        &clock(),
        SideEffectMode::Record,
        "device-b",
        "secret-b",
    );
    assert!(!Arc::ptr_eq(&original, &next));
}

#[tokio::test]
async fn maps_general_podcast_and_episode_searches() {
    let client = client(Arc::new(FakeCastro::standard()));
    let shows = client.search_podcasts("example").await.unwrap();
    assert_eq!(shows[0].client_id, PODCAST_ID);
    assert_eq!(shows[0].feed_url, FEED_URL);
    assert_eq!(shows[0].itunes_id, Some(1234));
    assert_eq!(shows[0].title, "Example Podcast");
    let episodes = client.search_episodes("example episode").await.unwrap();
    assert_eq!(episodes[0].client_id, EPISODE_ID);
    assert_eq!(episodes[0].show_title, "Example Podcast");
    assert_eq!(episodes[0].published_at, Some(1_784_223_000_000));
}

#[tokio::test]
async fn resolves_an_unsubscribed_rss_feed_and_enqueues_its_episode() {
    let api = Arc::new(FakeCastro::standard());
    let result = client(api.clone())
        .enqueue_episode(EnqueueEpisodeRequest {
            itunes_id: Some(1234),
            position: Some(PodcastQueuePosition::Last),
            ..request()
        })
        .await;
    assert_eq!(result, PodcastWriteResult::Added);
    assert_eq!(api.calls("search_podcasts"), vec![FEED_URL]);
    assert_eq!(api.calls("fetch_podcast"), vec![PODCAST_ID]);
    let posted = api.posted();
    assert_eq!(posted.len(), 1);
    let types: Vec<_> = posted[0].iter().map(|a| a.action_type).collect();
    assert_eq!(
        types,
        vec![
            CastroActionType::EpisodeQueued,
            CastroActionType::ClearEpisodeNew
        ]
    );
}

#[tokio::test]
async fn allocates_unique_action_ids_across_overlapping_client_instances() {
    let first_api = Arc::new(FakeCastro::standard());
    let second_api = Arc::new(FakeCastro::standard());
    let (first, second) = (client(first_api.clone()), client(second_api.clone()));
    let _ = tokio::join!(
        first.enqueue_episode(request()),
        second.enqueue_episode(request())
    );
    let ids: std::collections::HashSet<u64> = first_api
        .posted()
        .into_iter()
        .chain(second_api.posted())
        .flatten()
        .map(|a| a.id)
        .collect();
    assert_eq!(ids.len(), 4);
}

#[tokio::test]
async fn serializes_concurrent_queue_read_decision_write_across_client_instances() {
    let api = Arc::new(FakeCastro {
        queue_reflects_posts: true,
        ..FakeCastro::standard()
    });
    let (first, second) = (client(api.clone()), client(api.clone()));
    let (a, b) = tokio::join!(
        first.enqueue_episode(request()),
        second.enqueue_episode(request())
    );
    let mut results = vec![a, b];
    results.sort();
    assert_eq!(
        results,
        vec![PodcastWriteResult::Added, PodcastWriteResult::AlreadyExists]
    );
    assert_eq!(api.posted().len(), 1);
    assert_eq!(api.calls("fetch_queue").len(), 2);
}

#[tokio::test]
async fn queue_next_inserts_after_the_current_top_item_not_above_it() {
    let api = Arc::new(FakeCastro {
        queue_items: vec![
            CastroQueueItem {
                episode_id: "e0".into(),
                podcast_id: PODCAST_ID.into(),
                fractional_position: "a0".into(),
            },
            CastroQueueItem {
                episode_id: "e1".into(),
                podcast_id: PODCAST_ID.into(),
                fractional_position: "a1".into(),
            },
        ],
        ..FakeCastro::standard()
    });
    client(api.clone())
        .enqueue_episode(EnqueueEpisodeRequest {
            position: Some(PodcastQueuePosition::Next),
            ..request()
        })
        .await;
    let queued = &api.posted()[0][0];
    let data: CastroQueueEventData = parse_castro_event_data(queued).unwrap();
    let position = data.fractional_position;
    assert!(
        position.as_str() > "a0" && position.as_str() < "a1",
        "{position}"
    );
}

#[tokio::test]
async fn resolves_an_rss_feed_and_subscribes_by_castro_podcast_id() {
    let api = Arc::new(FakeCastro::standard());
    let result = client(api.clone())
        .subscribe_to_show(SubscribeToShowRequest {
            title: "Example Podcast".into(),
            feed_url: FEED_URL.into(),
            itunes_id: Some(1234),
        })
        .await;
    assert_eq!(result, PodcastWriteResult::Added);
    assert_eq!(api.calls("subscribe"), vec![PODCAST_ID]);
}

#[tokio::test]
async fn returns_only_is_new_episodes_and_clears_them_without_dequeueing() {
    let mut preview = episode(EPISODE_ID, "rss-guid", "Preview Episode");
    preview.description = "This is a free preview of a paid post.".into();
    let api = Arc::new(FakeCastro {
        subscriptions: vec![subscription()],
        podcast_state: Some(CastroPodcastState {
            public_id: PODCAST_ID.into(),
            episode_states: vec![
                state(EPISODE_ID, true, None, 0.0),
                state("22222222-2222-4222-8222-222222222222", false, None, 0.0),
            ],
        }),
        episodes: HashMap::from([(EPISODE_ID.to_owned(), preview)]),
        ..FakeCastro::standard()
    });
    let client = client(api.clone());
    let inbox = client.fetch_inbox().await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].client_episode_id, EPISODE_ID);
    assert_eq!(inbox[0].episode_guid.as_deref(), Some("rss-guid"));
    assert_eq!(inbox[0].episode_title, "Preview Episode");
    assert_eq!(
        client.clear_inbox_episode(EPISODE_ID).await,
        PodcastWriteResult::Removed
    );
    assert!(api.calls("fetch_queue").is_empty());
    let last = api.posted().pop().unwrap();
    assert_eq!(last.len(), 1);
    assert_eq!(last[0].episode_id, EPISODE_ID);
    assert_eq!(last[0].action_type, CastroActionType::ClearEpisodeNew);
}

#[tokio::test]
async fn an_inbox_episode_castro_no_longer_serves_does_not_fail_the_inbox() {
    let api = Arc::new(FakeCastro {
        subscriptions: vec![subscription()],
        podcast_state: Some(CastroPodcastState {
            public_id: PODCAST_ID.into(),
            episode_states: vec![
                state(EPISODE_ID, true, None, 0.0),
                state("removed-episode", true, None, 0.0),
            ],
        }),
        episodes: HashMap::from([(
            EPISODE_ID.to_owned(),
            episode(EPISODE_ID, "rss-guid", "Kept Episode"),
        )]),
        episode_statuses: HashMap::from([("removed-episode".to_owned(), 404)]),
        ..FakeCastro::standard()
    });
    let inbox = client(api).fetch_inbox().await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].client_episode_id, EPISODE_ID);
}

#[tokio::test]
async fn other_inbox_episode_failures_still_fail_the_inbox() {
    let api = Arc::new(FakeCastro {
        subscriptions: vec![subscription()],
        podcast_state: Some(CastroPodcastState {
            public_id: PODCAST_ID.into(),
            episode_states: vec![state("broken-episode", true, None, 0.0)],
        }),
        episode_statuses: HashMap::from([("broken-episode".to_owned(), 500)]),
        ..FakeCastro::standard()
    });
    assert!(client(api).fetch_inbox().await.is_err());
}

#[tokio::test]
async fn a_removed_queue_episode_is_hidden_and_does_not_block_dequeueing_others() {
    let api = Arc::new(FakeCastro {
        queue_items: vec![
            CastroQueueItem {
                episode_id: "removed-episode".into(),
                podcast_id: PODCAST_ID.into(),
                fractional_position: "a0".into(),
            },
            CastroQueueItem {
                episode_id: EPISODE_ID.into(),
                podcast_id: PODCAST_ID.into(),
                fractional_position: "a1".into(),
            },
        ],
        episodes: HashMap::from([(
            EPISODE_ID.to_owned(),
            episode(EPISODE_ID, "rss-guid", "Kept Episode"),
        )]),
        episode_statuses: HashMap::from([("removed-episode".to_owned(), 404)]),
        ..FakeCastro::standard()
    });
    let client = client(api.clone());
    let queue = client.fetch_queue().await.unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].episode_guid.as_deref(), Some("rss-guid"));
    assert_eq!(
        client.dequeue_episode("rss-guid").await,
        PodcastWriteResult::Removed
    );
    let last = api.posted().pop().unwrap();
    assert_eq!(last[0].episode_id, EPISODE_ID);
    assert_eq!(last[0].action_type, CastroActionType::EpisodeDequeued);
}

const EP_PLAYED: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const EP_PARTIAL: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const EP_OLD: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn iso(ms_ago: i64) -> Option<String> {
    Some(omni_core::js::to_iso_string(NOW - ms_ago))
}

fn history_api(states: Vec<CastroEpisodeState>) -> Arc<FakeCastro> {
    history_api_with(states, HashMap::new())
}

fn history_api_with(
    states: Vec<CastroEpisodeState>,
    episode_statuses: HashMap<String, u16>,
) -> Arc<FakeCastro> {
    let mk = |id: &str, guid: &str, title: &str, media: &str, seconds: f64| {
        let mut e = episode(id, guid, title);
        e.media_url = media.into();
        e.duration = CastroDuration { seconds };
        (id.to_owned(), e)
    };
    Arc::new(FakeCastro {
        subscriptions: vec![subscription()],
        podcast: Some(podcast(Vec::new())),
        podcast_state: Some(CastroPodcastState {
            public_id: PODCAST_ID.into(),
            episode_states: states,
        }),
        episodes: HashMap::from([
            mk(EP_PLAYED, "guid-played", "Played Ep", "u1", 1000.0),
            mk(EP_PARTIAL, "guid-partial", "Partial Ep", "u2", 100.0),
            mk(EP_OLD, "guid-old", "Old Ep", "u3", 100.0),
        ]),
        episode_statuses,
        ..FakeCastro::default()
    })
}

#[tokio::test]
async fn computes_completion_and_honors_the_since_ms_cutoff() {
    let mut played = state(EP_PLAYED, false, iso(2 * DAY), 0.0);
    played.is_starred = true;
    played.is_played = true;
    let api = history_api(vec![
        played,
        state(EP_PARTIAL, false, iso(2 * DAY), 30.0),
        state(EP_OLD, false, iso(100 * DAY), 10.0),
    ]);
    let history = client(api)
        .fetch_listen_history(Some(NOW - 10 * DAY))
        .await
        .unwrap();
    // The 100-day-old play is excluded by the cutoff.
    assert_eq!(history.len(), 2);
    let played = history
        .iter()
        .find(|e| e.episode_guid.as_deref() == Some("guid-played"))
        .unwrap();
    let partial = history
        .iter()
        .find(|e| e.episode_guid.as_deref() == Some("guid-partial"))
        .unwrap();
    assert_eq!(played.completion, Some(1.0));
    assert_eq!(played.starred, Some(true));
    assert_eq!(played.media_url.as_deref(), Some("u1"));
    assert!((partial.completion.unwrap() - 0.3).abs() < 1e-9);
}

#[tokio::test]
async fn skips_episodes_castro_no_longer_serves() {
    let api = history_api_with(
        vec![
            state(EP_PARTIAL, false, iso(DAY), 30.0),
            state("removed-episode", false, iso(DAY), 10.0),
        ],
        HashMap::from([("removed-episode".to_owned(), 404)]),
    );
    let history = client(api).fetch_listen_history(None).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].episode_guid.as_deref(), Some("guid-partial"));
}

#[tokio::test]
async fn other_episode_failures_still_fail_the_history() {
    let api = history_api_with(
        vec![
            state(EP_PARTIAL, false, iso(DAY), 30.0),
            state("broken-episode", false, iso(DAY), 10.0),
        ],
        HashMap::from([("broken-episode".to_owned(), 500)]),
    );
    assert!(client(api).fetch_listen_history(None).await.is_err());
}

#[tokio::test]
async fn clamps_completion_to_1_when_progress_exceeds_duration() {
    let api = history_api(vec![state(EP_PARTIAL, false, iso(DAY), 500.0)]);
    let history = client(api).fetch_listen_history(None).await.unwrap();
    assert_eq!(history[0].completion, Some(1.0));
}

#[tokio::test]
async fn skips_episodes_that_were_never_played_null_last_played() {
    let api = history_api(vec![state(EP_PLAYED, true, None, 0.0)]);
    let history = client(api).fetch_listen_history(None).await.unwrap();
    assert!(history.is_empty());
}

#[test]
fn ignores_protocol_and_query_params() {
    assert_eq!(
        normalize_media_url(Some("HTTPS://cdn.x.com/a/ep.mp3?token=1")).as_deref(),
        Some("cdn.x.com/a/ep.mp3")
    );
    assert_eq!(
        normalize_media_url(Some("http://cdn.x.com/a/ep.mp3")).as_deref(),
        Some("cdn.x.com/a/ep.mp3")
    );
}

#[test]
fn returns_undefined_for_empty_input() {
    assert_eq!(normalize_media_url(None), None);
    assert_eq!(normalize_media_url(Some("  ")), None);
}

fn enqueue_request(guid: &str, media: Option<&str>, title: &str) -> EnqueueEpisodeRequest {
    EnqueueEpisodeRequest {
        feed_url: FEED_URL.into(),
        episode_guid: guid.into(),
        media_url: media.map(str::to_owned),
        show_title: "A Show".into(),
        episode_title: title.into(),
        ..EnqueueEpisodeRequest::default()
    }
}

#[test]
fn matches_by_guid_first() {
    let eps = vec![episode("p1", "rss-guid", "An Episode")];
    let request = enqueue_request("rss-guid", None, "An Episode");
    assert_eq!(
        match_episode(&eps, &request).map(|e| e.public_id.as_str()),
        Some("p1")
    );
}

#[test]
fn falls_back_to_the_enclosure_url_when_guids_differ() {
    let mut e = episode("p2", "castro-only", "An Episode");
    e.media_url = "https://prefix.fm/redirect/cdn.x.com/audio/ep.mp3?aid=rss".into();
    let request = enqueue_request(
        "different-rss-guid",
        Some("https://prefix.fm/redirect/cdn.x.com/audio/ep.mp3?aid=other"),
        "An Episode",
    );
    assert_eq!(
        match_episode(&[e], &request).map(|e| e.public_id.clone()),
        Some("p2".into())
    );
}

#[test]
fn falls_back_to_a_unique_title_match() {
    let eps = vec![episode("p3", "g1", "The Holy Shiver")];
    let request = enqueue_request("nope", None, "The Holy Shiver!");
    assert_eq!(
        match_episode(&eps, &request).map(|e| e.public_id.as_str()),
        Some("p3")
    );
}

#[test]
fn refuses_an_ambiguous_title_match() {
    let mut a = episode("p4", "g1", "Bonus");
    a.media_url = "a".into();
    let mut b = episode("p5", "g2", "Bonus");
    b.media_url = "b".into();
    let request = enqueue_request("nope", None, "Bonus");
    assert!(match_episode(&[a, b], &request).is_none());
}
