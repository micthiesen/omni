//! `LiveCheckTask`, with end-to-end checks of the notification invariants
//! (title debounce, offline text, background mute, outage alerts, dashboard
//! updates, reconcile isolation).
//!
//! Persistence failures are produced by dropping the `blobs` table.
//! Interruption is a dropped (aborted) future.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_api::streamers::{DggPresence, StreamerTier};
use omni_core::clock::SharedClock;
use omni_live::dgg::{DggEmbed, DggFeed, DggFeedError, DggFeedSource, DggMediaMetadata};
use omni_live::error::LiveError;
use omni_live::identity::{ProfileIdentityLink, all_links, remember_link};
use omni_live::platform::{FetchedLive, FetchedStatus};
use omni_live::profile_links::{
    IdentityLearner, LearnError, LearnInput, ProfileFetcher, ProfileIdentityLearner,
    ProfileLinkError,
};
use omni_live::sessions::get_sessions;
use omni_live::status::{LiveStatus, StreamerStatus, get_status, upsert_status};
use omni_live::streamers::{Roster, Streamer};
use omni_live::task::{
    DggDiscovery, IntelligenceObserver, LiveCheck, LiveCheckDeps, LiveObservation, TickHook,
};
use omni_live::{Platform, PlatformBinding};
use omni_store::cbor::Extra;
use omni_tasks::{AppEvent, EventBus};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const T0: i64 = 1_790_000_000_000;

fn embed(
    platform: &str,
    id: &str,
    display_name: &str,
    title: &str,
    count: i64,
    viewers: i64,
) -> DggEmbed {
    DggEmbed {
        platform: platform.into(),
        id: id.into(),
        count,
        media_platform: platform.into(),
        media_id: id.into(),
        metadata: DggMediaMetadata {
            preview_url: None,
            display_name: display_name.into(),
            title: Some(title.into()),
            created_date: None,
            live: true,
            viewers: Some(viewers),
        },
    }
}

fn feed(id: Option<&str>) -> DggFeed {
    DggFeed {
        destiny_live: false,
        hosting: None,
        embeds: id
            .map(|id| vec![embed("twitch", id, id, &format!("{id} live"), 12, 0)])
            .unwrap_or_default(),
    }
}

type Fallback = dyn FnMut() -> Option<Result<DggFeed, DggFeedError>> + Send;

/// Scripted snapshots; once exhausted, `fallback` (or a never-ending fetch).
struct FakeFeed {
    script: Mutex<VecDeque<Result<DggFeed, DggFeedError>>>,
    fallback: Mutex<Box<Fallback>>,
    calls: AtomicUsize,
}

impl FakeFeed {
    fn new(
        script: Vec<Result<DggFeed, DggFeedError>>,
        fallback: impl FnMut() -> Option<Result<DggFeed, DggFeedError>> + Send + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            fallback: Mutex::new(Box::new(fallback)),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl DggFeedSource for FakeFeed {
    fn fetch(&self) -> BoxFuture<'_, Result<DggFeed, DggFeedError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .or_else(|| (self.fallback.lock().unwrap())());
        Box::pin(async move {
            match next {
                Some(result) => result,
                None => futures::future::pending().await,
            }
        })
    }
}

type LearnFn = dyn Fn(LearnInput) -> BoxFuture<'static, Result<Option<ProfileIdentityLink>, LearnError>>
    + Send
    + Sync;

struct FnLearner(Box<LearnFn>);

impl IdentityLearner for FnLearner {
    fn learn(
        &self,
        input: LearnInput,
    ) -> BoxFuture<'_, Result<Option<ProfileIdentityLink>, LearnError>> {
        (self.0)(input)
    }
}

fn no_identity() -> Arc<dyn IdentityLearner> {
    Arc::new(FnLearner(Box::new(|_| Box::pin(async { Ok(None) }))))
}

struct Fixture {
    store: omni_testkit::TestStore,
    clock: Arc<omni_core::clock::TestClock>,
    notifier: common::FakeNotifier,
    fetcher: common::FakeFetcher,
    bus: EventBus,
}

async fn fixture(now: i64) -> Fixture {
    let (store, clock) = common::test_store(now).await;
    Fixture {
        store,
        clock,
        notifier: common::FakeNotifier::default(),
        fetcher: common::FakeFetcher::default(),
        bus: EventBus::default(),
    }
}

impl Fixture {
    fn check(&self, roster: &Roster) -> LiveCheck {
        let clock: SharedClock = self.clock.clone();
        LiveCheck::new(
            roster.clone(),
            LiveCheckDeps {
                store: self.store.store.clone(),
                clock,
                tz: common::utc(),
                fetcher: Arc::new(self.fetcher.clone()),
                notifier: Arc::new(self.notifier.clone()),
                offline_notifications: true,
                live_token: Some("live-token".into()),
                bus: Some(self.bus.clone()),
            },
        )
    }

    fn dgg_check(
        &self,
        roster: &Roster,
        feed: Arc<FakeFeed>,
        identity: Arc<dyn IdentityLearner>,
    ) -> LiveCheck {
        self.check(roster).with_dgg(DggDiscovery {
            top_embeds: omni_live::TopEmbeds::new(1),
            available_platforms: Platform::ALL.into_iter().collect::<HashSet<_>>(),
            feed,
            identity,
        })
    }
}

fn ids(roster: &Roster) -> Vec<String> {
    roster.snapshot().into_iter().map(|s| s.id).collect()
}

fn live(title: &str, viewers: Option<i64>) -> FetchedStatus {
    FetchedStatus::Live(FetchedLive {
        title: title.into(),
        viewer_count: viewers,
        category: None,
        started_at: None,
    })
}

fn streamer(
    id: &str,
    name: &str,
    platform: Platform,
    username: &str,
    tier: StreamerTier,
) -> Streamer {
    Streamer::new(
        id,
        name,
        vec![PlatformBinding::new(platform, username)],
        tier,
    )
}

#[tokio::test]
async fn refreshes_and_polls_dgg_streams_only_on_the_background_cadence() {
    let f = fixture(T0).await;
    let roster = Roster::default();
    let feed = FakeFeed::new(vec![Ok(feed(Some("first"))), Ok(feed(None))], || {
        Some(Ok(feed(None)))
    });
    let check = f.dgg_check(&roster, feed.clone(), no_identity());

    check.tick().await.unwrap();
    assert_eq!(feed.calls(), 1);
    assert_eq!(ids(&roster), ["dgg:twitch:first"]);

    check.tick().await.unwrap();
    check.tick().await.unwrap();
    assert_eq!(feed.calls(), 1);
    assert_eq!(roster.len(), 1);

    check.tick().await.unwrap();
    assert_eq!(feed.calls(), 2);
    assert!(roster.is_empty());
    assert_eq!(f.notifier.count(), 0);
}

#[tokio::test]
async fn retains_the_last_selection_but_observes_it_as_unknown_after_a_refresh_failure() {
    let f = fixture(T0).await;
    let roster = Roster::default();
    let feed = FakeFeed::new(vec![Ok(feed(Some("retained")))], || {
        Some(Err(DggFeedError {
            message: "DGG unavailable".into(),
        }))
    });
    let check = f.dgg_check(&roster, feed, no_identity());
    for _ in 0..4 {
        check.tick().await.unwrap();
    }
    assert_eq!(ids(&roster), ["dgg:twitch:retained"]);
    assert_eq!(f.notifier.count(), 0);
}

#[tokio::test]
async fn propagates_interruption_while_refreshing_dgg_streams() {
    let f = fixture(T0).await;
    let feed = FakeFeed::new(vec![], || None);
    let check = Arc::new(f.dgg_check(&Roster::default(), feed.clone(), no_identity()));
    let running = tokio::spawn({
        let check = check.clone();
        async move { check.tick().await }
    });
    while feed.calls() == 0 {
        tokio::task::yield_now().await;
    }
    running.abort();
    let outcome = running.await;
    assert!(outcome.is_err_and(|e| e.is_cancelled()));
}

#[tokio::test]
async fn propagates_dgg_persistence_failures_instead_of_treating_them_as_feed_outages() {
    let f = fixture(T0).await;
    let check = f.dgg_check(
        &Roster::default(),
        FakeFeed::new(vec![], || Some(Ok(feed(None)))),
        no_identity(),
    );
    common::break_store(&f.store.store).await;
    let error = check.tick().await.unwrap_err();
    assert!(
        matches!(
            error,
            LiveError::Persistence {
                operation: "list profile identity links",
                ..
            }
        ),
        "{error}"
    );
    assert!(error.to_string().contains("PersistenceError"));
}

#[derive(Default)]
struct RecordingObserver {
    events: Mutex<Vec<String>>,
}

impl IntelligenceObserver for RecordingObserver {
    fn observe_live<'a>(
        &'a self,
        observation: &'a LiveObservation,
        _now: i64,
    ) -> BoxFuture<'a, ()> {
        self.events.lock().unwrap().push(format!(
            "live:{}:{}",
            observation.streamer.id, observation.went_live
        ));
        Box::pin(async {})
    }
    fn observe_offline<'a>(&'a self, streamer_id: &'a str, _now: i64) -> BoxFuture<'a, ()> {
        self.events
            .lock()
            .unwrap()
            .push(format!("offline:{streamer_id}"));
        Box::pin(async {})
    }
    fn after_tick(&self) -> BoxFuture<'_, ()> {
        self.events.lock().unwrap().push("after".into());
        Box::pin(async {})
    }
}

#[tokio::test]
async fn schedules_intelligence_work_only_after_every_due_streamer_was_observed() {
    let f = fixture(T0).await;
    let observer = Arc::new(RecordingObserver::default());
    let check = f
        .dgg_check(
            &Roster::default(),
            FakeFeed::new(vec![], || Some(Ok(feed(Some("voice-target"))))),
            no_identity(),
        )
        .with_intelligence(observer.clone());
    check.tick().await.unwrap();
    assert_eq!(
        *observer.events.lock().unwrap(),
        ["live:dgg:twitch:voice-target:true", "after"]
    );
}

#[tokio::test]
async fn enriches_an_explicit_primary_streamer_without_duplicating_or_replacing_its_polling() {
    let f = fixture(T0).await;
    let configured = streamer(
        "configured",
        "Configured",
        Platform::Twitch,
        "configured",
        StreamerTier::Primary,
    );
    let roster = Roster::new(vec![configured.clone()]);
    f.fetcher
        .set(Platform::Twitch, live("Configured live", Some(0)));
    let feed = FakeFeed::new(vec![Ok(feed(Some("configured"))), Ok(feed(None))], || {
        Some(Ok(feed(None)))
    });
    let check = f.dgg_check(&roster, feed.clone(), no_identity());

    check.tick().await.unwrap();
    let current = roster.snapshot();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].tier, StreamerTier::Primary);
    assert_eq!(current[0].bindings, configured.bindings);
    assert_eq!(
        current[0].dgg,
        Some(DggPresence {
            hosted: false,
            viewers: Some(12)
        })
    );
    assert_eq!(feed.calls(), 1);
    assert_eq!(f.fetcher.calls_for(Platform::Twitch), 1);
    assert_eq!(f.notifier.count(), 1);

    for _ in 0..3 {
        check.tick().await.unwrap();
    }
    assert_eq!(feed.calls(), 2);
    assert_eq!(f.fetcher.calls_for(Platform::Twitch), 4);
    assert_eq!(f.notifier.count(), 1);
    assert_eq!(roster.snapshot(), vec![configured]);
}

#[tokio::test]
async fn learns_a_dgg_profile_identity_and_keeps_per_platform_observations() {
    let f = fixture(T0).await;
    let youtube = PlatformBinding::new(Platform::YouTube, "@imreallyimportant");
    let roster = Roster::new(vec![Streamer::new(
        "iri",
        "IRI",
        vec![youtube.clone()],
        StreamerTier::Background,
    )]);
    f.fetcher
        .set(Platform::YouTube, live("Election night", Some(427)));
    let snapshot = DggFeed {
        destiny_live: false,
        hosting: None,
        embeds: vec![embed(
            "kick",
            "imreallyimportant",
            "imreallyimportant",
            "Election night",
            61,
            475,
        )],
    };
    let store = f.store.store.clone();
    let target = youtube.clone();
    let learner = Arc::new(FnLearner(Box::new(move |input: LearnInput| {
        let (store, target) = (store.clone(), target.clone());
        Box::pin(async move {
            Ok(Some(
                remember_link(&store, &input.source, &target, input.now).await?,
            ))
        })
    })));
    let check = f.dgg_check(
        &roster,
        FakeFeed::new(vec![], move || Some(Ok(snapshot.clone()))),
        learner,
    );

    check.tick().await.unwrap();
    let current = roster.snapshot();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].bindings.len(), 2);
    assert_eq!(current[0].bindings[0], youtube);
    assert!(
        current[0].bindings[1]
            .same_account(&PlatformBinding::new(Platform::Kick, "imreallyimportant"))
    );
    let StreamerStatus::Live(status) = get_status(&f.store.store, "iri").await.unwrap() else {
        panic!("live expected");
    };
    assert_eq!(status.viewer_count, Some(902));
    let sources: Vec<(Platform, Option<i64>)> = status
        .sources
        .unwrap()
        .iter()
        .map(|s| (s.platform, s.viewer_count))
        .collect();
    assert_eq!(
        sources,
        [(Platform::YouTube, Some(427)), (Platform::Kick, Some(475))]
    );
}

#[tokio::test]
async fn resolves_successive_youtube_video_owners_without_duplicate_rows_or_viewers() {
    let f = fixture(T0).await;
    let server = omni_testkit::mock_server().await;
    Mock::given(method("GET"))
        .and(path("/oembed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "type": "video", "author_name": "An unrelated display name",
            "author_url": "https://www.youtube.com/@lonerboxlive"
        })))
        .mount(&server)
        .await;
    let learner = Arc::new(ProfileIdentityLearner::new(
        f.store.store.clone(),
        ProfileFetcher::new(omni_testkit::mock_http(
            &server,
            &["https://www.youtube.com"],
        )),
    ));
    let youtube = PlatformBinding::new(Platform::YouTube, "@lonerboxlive");
    let roster = Roster::new(vec![Streamer::new(
        "lonerbox",
        "LonerBox",
        vec![youtube.clone()],
        StreamerTier::Background,
    )]);
    f.fetcher.set(Platform::YouTube, live("Live", Some(100)));
    let video = Arc::new(Mutex::new("GqP2KP9_Blo".to_owned()));
    let current_video = video.clone();
    let feed = FakeFeed::new(vec![], move || {
        let id = current_video.lock().unwrap().clone();
        Some(Ok(DggFeed {
            destiny_live: false,
            hosting: None,
            embeds: vec![embed("youtube", &id, "LonerBox Live", "Live", 15, 100)],
        }))
    });
    let check = f.dgg_check(&roster, feed, learner);
    let owner_fetches = || async { server.received_requests().await.unwrap().len() };

    check.tick().await.unwrap();
    let current = roster.snapshot();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].bindings, vec![youtube.clone()]);
    assert_eq!(current[0].dgg.and_then(|d| d.viewers), Some(15));
    let StreamerStatus::Live(status) = get_status(&f.store.store, "lonerbox").await.unwrap() else {
        panic!("live expected");
    };
    assert_eq!(status.viewer_count, Some(100));
    let sources = status.sources.unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(
        (
            sources[0].platform,
            sources[0].username.as_str(),
            sources[0].viewer_count
        ),
        (Platform::YouTube, "@lonerboxlive", Some(100))
    );

    for _ in 0..3 {
        check.tick().await.unwrap();
    }
    assert_eq!(owner_fetches().await, 1);
    *video.lock().unwrap() = "vcTFGmR6Yns".to_owned();
    for _ in 0..3 {
        check.tick().await.unwrap();
    }
    assert_eq!(owner_fetches().await, 2);
    let current = roster.snapshot();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].bindings, vec![youtube]);
    assert_eq!(f.fetcher.calls_for(Platform::YouTube), 3);
    assert_eq!(all_links(&f.store.store).await.unwrap().len(), 2);
}

#[tokio::test]
async fn revalidates_ownership_without_deleting_aliases_on_lookup_failure() {
    for failed in [false, true] {
        let f = fixture(T0).await;
        let youtube = PlatformBinding::new(Platform::YouTube, "@iri");
        let kick = PlatformBinding::new(Platform::Kick, "iri");
        remember_link(&f.store.store, &kick, &youtube, 0)
            .await
            .unwrap();
        let roster = Roster::new(vec![Streamer::new(
            "iri",
            "IRI",
            vec![youtube],
            StreamerTier::Background,
        )]);
        f.fetcher.set(Platform::YouTube, FetchedStatus::Offline);
        let snapshot = DggFeed {
            destiny_live: false,
            hosting: None,
            embeds: vec![embed(
                "kick",
                "iri",
                "Different Display Name",
                "No longer linked",
                10,
                20,
            )],
        };
        let learner = Arc::new(FnLearner(Box::new(move |_| {
            Box::pin(async move {
                if failed {
                    Err(LearnError::Link(ProfileLinkError {
                        operation: "fetch owner".into(),
                        detail: "HTTP 503".into(),
                    }))
                } else {
                    Ok(None)
                }
            })
        })));
        let check = f.dgg_check(
            &roster,
            FakeFeed::new(vec![], move || Some(Ok(snapshot.clone()))),
            learner,
        );
        check.tick().await.unwrap();
        assert_eq!(
            all_links(&f.store.store).await.unwrap().len(),
            usize::from(failed),
            "failed={failed}"
        );
        let expected: Vec<&str> = if failed {
            vec!["iri"]
        } else {
            vec!["iri", "dgg:kick:iri"]
        };
        assert_eq!(ids(&roster), expected, "failed={failed}");
    }
}

#[tokio::test]
async fn persists_a_live_edge_before_notification_failure_so_the_alert_is_not_repeated() {
    let f = fixture(T0).await;
    let roster = Roster::new(vec![streamer(
        "durable-live",
        "Durable Live",
        Platform::Twitch,
        "durable",
        StreamerTier::Primary,
    )]);
    f.fetcher
        .set(Platform::Twitch, live("Already recorded", None));
    f.notifier.fail_next("Pushover unavailable");
    let check = f.check(&roster);

    let error = check.tick().await.unwrap_err();
    assert!(
        error.to_string().contains("Pushover unavailable"),
        "{error}"
    );
    assert!(
        get_status(&f.store.store, "durable-live")
            .await
            .unwrap()
            .is_live()
    );
    check.tick().await.unwrap();
    assert_eq!(f.notifier.count(), 1);
}

#[tokio::test]
async fn retries_an_offline_alert_before_durably_closing_the_session() {
    let f = fixture(T0).await;
    let s = streamer(
        "durable-offline",
        "Durable Offline",
        Platform::Twitch,
        "durable",
        StreamerTier::Primary,
    );
    upsert_status(
        &f.store.store,
        StreamerStatus::Live(LiveStatus {
            streamer_id: s.id.clone(),
            primary: s.bindings[0].clone(),
            primary_title: "Session".into(),
            started_at: T0 - 60_000,
            max_viewer_count: 10,
            viewer_count: None,
            sources: None,
            category: None,
            extra: Extra::new(),
        }),
    )
    .await
    .unwrap();
    let roster = Roster::new(vec![s.clone()]);
    f.fetcher.set(Platform::Twitch, FetchedStatus::Offline);
    f.notifier.fail_next("Pushover unavailable");
    let check = f.check(&roster);

    let error = check.tick().await.unwrap_err();
    assert!(error.to_string().contains("Pushover unavailable"));
    assert!(get_status(&f.store.store, &s.id).await.unwrap().is_live());
    assert!(
        get_sessions(&f.store.store, &s.id)
            .await
            .unwrap()
            .sessions
            .is_empty()
    );
    check.tick().await.unwrap();
    assert!(!get_status(&f.store.store, &s.id).await.unwrap().is_live());
    assert_eq!(
        get_sessions(&f.store.store, &s.id)
            .await
            .unwrap()
            .sessions
            .len(),
        1
    );
    assert_eq!(f.notifier.count(), 2);
    let (token, message) = &f.notifier.sent()[1];
    assert_eq!(token.as_deref(), Some("live-token"));
    assert_eq!(message.title, "Durable Offline is now offline");
    assert_eq!(message.message, "Streamed for 1 minute with 10 viewers.");
}

#[tokio::test(start_paused = true)]
async fn uses_the_test_clock_for_transition_timestamps() {
    let f = fixture(120_000).await;
    let roster = Roster::new(vec![streamer(
        "clocked-live",
        "Clocked Live",
        Platform::Twitch,
        "clocked",
        StreamerTier::Primary,
    )]);
    f.fetcher.set(Platform::Twitch, live("Virtual time", None));
    f.check(&roster).tick().await.unwrap();
    let status = get_status(&f.store.store, "clocked-live").await.unwrap();
    assert_eq!(status.as_live().map(|l| l.started_at), Some(120_000));
}

#[tokio::test]
async fn returns_streamer_persistence_failures_in_the_typed_error_channel() {
    let f = fixture(T0).await;
    let roster = Roster::new(vec![streamer(
        "persistence-failure",
        "Persistence Failure",
        Platform::Twitch,
        "broken",
        StreamerTier::Primary,
    )]);
    f.fetcher
        .set(Platform::Twitch, live("Cannot persist", None));
    common::break_store(&f.store.store).await;
    let mut events = f.bus.app();
    let error = f.check(&roster).tick().await.unwrap_err();
    assert!(matches!(error, LiveError::Persistence { .. }), "{error}");
    assert!(error.to_string().contains("PersistenceError"));
    // A failed tick still refreshes the dashboard.
    assert!(matches!(events.try_recv(), Ok(AppEvent::StreamersChanged)));
}

// --- additional invariants ------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn notifies_went_live_with_the_previous_session_and_debounces_titles() {
    let f = fixture(T0).await;
    let s = Streamer {
        pushover_token: Some("own-token".into()),
        ..streamer(
            "dest",
            "Destiny",
            Platform::Kick,
            "destiny",
            StreamerTier::Primary,
        )
    };
    let roster = Roster::new(vec![s]);
    let check = f.check(&roster);
    let set = |title: &str, viewers| f.fetcher.set(Platform::Kick, live(title, Some(viewers)));

    set("A", 1_000);
    check.tick().await.unwrap();
    let (token, message) = &f.notifier.sent()[0];
    assert_eq!(token.as_deref(), Some("own-token"));
    assert_eq!(message.title, "Destiny is LIVE!");
    assert_eq!(message.message, "A");
    assert_eq!(
        message.url.as_ref().map(|u| u.url.as_str()),
        Some("https://kick.com/destiny")
    );
    assert_eq!(
        message.url.as_ref().map(|u| u.url_title.as_str()),
        Some("Watch on Kick")
    );

    // A fix right after going live is held, later changes collapse to the last.
    set("B", 1_000);
    f.clock.set(T0 + 60_000);
    check.tick().await.unwrap();
    set("C", 1_000);
    f.clock.set(T0 + 120_000);
    check.tick().await.unwrap();
    assert_eq!(f.notifier.count(), 1);
    f.clock.set(T0 + 10 * 60_000);
    check.tick().await.unwrap();
    assert_eq!(
        f.notifier.titles().last().map(String::as_str),
        Some("Destiny changed title")
    );
    assert_eq!(
        f.notifier
            .sent()
            .last()
            .map(|(_, m)| m.message.clone())
            .as_deref(),
        Some("C")
    );

    // Offline, then live again: the go-live message describes the last session.
    f.fetcher.set(Platform::Kick, FetchedStatus::Offline);
    f.clock.set(T0 + 2 * 60 * 60_000);
    check.tick().await.unwrap();
    assert!(
        f.notifier
            .titles()
            .contains(&"Destiny is now offline".to_owned())
    );
    set("Back", 10);
    f.clock.set(T0 + 5 * 60 * 60_000);
    check.tick().await.unwrap();
    let last = f.notifier.sent().last().cloned().unwrap().1;
    assert_eq!(last.title, "Destiny is LIVE!");
    assert_eq!(
        last.message,
        "Back\n\nLast live about 3 hours ago for about 2 hours with 1,000 viewers."
    );
}

#[tokio::test]
async fn mutes_live_activity_for_background_and_muted_streamers_but_tracks_state() {
    let f = fixture(T0).await;
    let muted = Streamer {
        live_notifications: Some(false),
        ..streamer(
            "muted",
            "Muted",
            Platform::Twitch,
            "muted",
            StreamerTier::Primary,
        )
    };
    let background = streamer(
        "bg",
        "Background",
        Platform::Kick,
        "bg",
        StreamerTier::Background,
    );
    let roster = Roster::new(vec![muted, background]);
    f.fetcher.set(Platform::Twitch, live("t", Some(10)));
    f.fetcher.set(Platform::Kick, live("t", Some(10)));
    let check = f.check(&roster);
    check.tick().await.unwrap();
    assert_eq!(f.notifier.count(), 0);
    assert!(get_status(&f.store.store, "muted").await.unwrap().is_live());
    assert!(get_status(&f.store.store, "bg").await.unwrap().is_live());
    // Background streamers are polled every third tick only.
    check.tick().await.unwrap();
    check.tick().await.unwrap();
    assert_eq!(f.fetcher.calls_for(Platform::Kick), 1);
    assert_eq!(f.fetcher.calls_for(Platform::Twitch), 3);
}

#[tokio::test]
async fn sends_one_fleet_outage_alert_after_three_all_unknown_ticks() {
    let f = fixture(T0).await;
    let roster = Roster::new(vec![streamer(
        "a",
        "Alpha",
        Platform::Twitch,
        "a",
        StreamerTier::Primary,
    )]);
    f.fetcher
        .set(Platform::Twitch, FetchedStatus::unknown("timeout"));
    let check = f.check(&roster);
    for _ in 0..4 {
        check.tick().await.unwrap();
    }
    let sent = f.notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, None);
    assert_eq!(
        sent[0].1.title,
        "Live check degraded: 1/1 streamers unreachable"
    );
    assert_eq!(sent[0].1.message, "Alpha\ntimeout");
    assert_eq!(sent[0].1.sender, None, "fleet alerts have no sender");
    assert_eq!(sent[0].1.thread_id, None);
}

struct FailingHook;

impl TickHook for FailingHook {
    fn after_tick(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Err("APNs down".to_owned()) })
    }
}

#[tokio::test]
async fn emits_streamer_updates_and_isolates_reconcile_failures() {
    let f = fixture(T0).await;
    let mut events = f.bus.app();
    let roster = Roster::new(vec![streamer(
        "a",
        "Alpha",
        Platform::Twitch,
        "a",
        StreamerTier::Primary,
    )]);
    let check = f.check(&roster).with_reconcile(Arc::new(FailingHook));
    check.tick().await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event, AppEvent::StreamersChanged));
}

#[tokio::test]
async fn retires_a_streamer_removed_while_live_without_an_offline_notification() {
    let f = fixture(T0).await;
    let s = streamer(
        "dest",
        "Destiny",
        Platform::Kick,
        "destiny",
        StreamerTier::Primary,
    );
    let roster = Roster::new(vec![s.clone()]);
    let check = f.check(&roster);
    f.fetcher.set(Platform::Kick, live("A", Some(1_000)));
    check.tick().await.unwrap();
    assert_eq!(f.notifier.titles(), ["Destiny is LIVE!"]);

    roster.replace(Vec::new());
    f.clock.set(T0 + 60_000);
    check.tick().await.unwrap();
    assert!(matches!(
        get_status(&f.store.store, "dest").await.unwrap(),
        StreamerStatus::Offline(_)
    ));
    assert_eq!(f.notifier.count(), 1);

    // Re-adding while still live starts a fresh session.
    roster.replace(vec![s]);
    f.clock.set(T0 + 120_000);
    check.tick().await.unwrap();
    assert_eq!(
        f.notifier.titles(),
        ["Destiny is LIVE!", "Destiny is LIVE!"]
    );
}

fn dgg_feed_with_viewers(id: &str, viewers: i64) -> DggFeed {
    DggFeed {
        destiny_live: false,
        hosting: None,
        embeds: vec![embed("twitch", id, id, "live", 12, viewers)],
    }
}

#[tokio::test]
async fn never_pushes_viewer_records_for_unconfigured_dgg_discoveries_but_keeps_their_metrics() {
    let f = fixture(T0).await;
    let roster = Roster::default();
    let feed = FakeFeed::new(
        vec![
            Ok(dgg_feed_with_viewers("drifter", 1_000)),
            Ok(dgg_feed_with_viewers("drifter", 100)),
        ],
        || Some(Ok(dgg_feed_with_viewers("drifter", 100))),
    );
    let check = f.dgg_check(&roster, feed, no_identity());
    for _ in 0..4 {
        check.tick().await.unwrap();
    }
    assert_eq!(ids(&roster), ["dgg:twitch:drifter"]);
    assert_eq!(f.notifier.count(), 0, "{:?}", f.notifier.titles());
    let metrics = omni_live::metrics::get_viewer_metrics(&f.store.store, "dgg:twitch:drifter")
        .await
        .unwrap();
    assert_eq!(metrics.all_time_max, 1_000);
}

#[tokio::test]
async fn streamer_notifications_carry_the_sender_avatar_and_a_per_streamer_thread() {
    let f = fixture(T0).await;
    let avatar = omni_live::avatars::StreamerAvatar {
        binding: "twitch:vinesauce".into(),
        platform: Platform::Twitch,
        username: "vinesauce".into(),
        url: Some("https://static-cdn.jtvnw.net/vinny.png".into()),
        fetched_at: Some(T0),
        failed_at: None,
        extra: Extra::new(),
    };
    f.store
        .store
        .write(move |tx| {
            omni_store::EntityWrite::upsert(tx, &avatar, omni_store::entity::UpsertOpts::default())
        })
        .await
        .unwrap();
    let roster = Roster::new(vec![streamer(
        "vinesauce",
        "Vinesauce",
        Platform::Twitch,
        "Vinesauce",
        StreamerTier::Primary,
    )]);
    f.fetcher
        .set(Platform::Twitch, live("Corruptions", Some(10)));
    let check = f.check(&roster);
    check.tick().await.unwrap();
    f.fetcher.set(Platform::Twitch, FetchedStatus::Offline);
    check.tick().await.unwrap();

    let sent = f.notifier.sent();
    assert_eq!(
        f.notifier.titles(),
        [
            "Vinesauce is LIVE!",
            "New all-time record for Vinesauce!",
            "Vinesauce is now offline"
        ]
    );
    for (_, message) in sent {
        assert_eq!(message.thread_id.as_deref(), Some("live:vinesauce"));
        let sender = message.sender.unwrap();
        assert_eq!(sender.streamer_id, "vinesauce");
        assert_eq!(sender.display_name, "Vinesauce");
        assert_eq!(
            sender.avatar_url.as_deref(),
            Some("https://static-cdn.jtvnw.net/vinny.png")
        );
    }
}
