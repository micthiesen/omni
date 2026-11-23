//! `livestream.status_changed`: published on the aggregate edges through the
//! event port, with a dedup key that survives an offline replay, and never
//! fails the tick when the outbox is unavailable.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use omni_api::events::LIVESTREAM_STATUS_CHANGED;
use omni_api::streamers::StreamerTier;
use omni_core::clock::SharedClock;
use omni_live::platform::{FetchedLive, FetchedStatus};
use omni_live::status::{StreamerStatus, get_status, upsert_status};
use omni_live::streamers::{Roster, Streamer};
use omni_live::task::{LiveCheck, LiveCheckDeps};
use omni_live::{Platform, PlatformBinding};
use omni_runtime::ports::Ports;
use omni_testkit::RecordedEvents;
use serde_json::json;

const T0: i64 = 1_790_000_000_000;

struct Fixture {
    store: omni_testkit::TestStore,
    clock: Arc<omni_core::clock::TestClock>,
    notifier: common::FakeNotifier,
    fetcher: common::FakeFetcher,
    ports: Ports,
    events: RecordedEvents,
}

async fn fixture() -> Fixture {
    let (store, clock) = common::test_store(T0).await;
    let ports = Ports::default();
    let events = RecordedEvents::install(&ports);
    Fixture {
        store,
        clock,
        notifier: common::FakeNotifier::default(),
        fetcher: common::FakeFetcher::default(),
        ports,
        events,
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
                live_token: None,
                bus: None,
            },
        )
        .with_events(self.ports.clone())
    }
}

fn live(title: &str, viewers: i64) -> FetchedStatus {
    FetchedStatus::Live(FetchedLive {
        title: title.into(),
        viewer_count: Some(viewers),
        category: None,
        started_at: None,
    })
}

fn streamer(id: &str, tier: StreamerTier) -> Streamer {
    Streamer::new(
        id,
        "Streamer Name",
        vec![PlatformBinding::new(Platform::Twitch, id)],
        tier,
    )
}

#[tokio::test(start_paused = true)]
async fn publishes_went_live_and_went_offline_with_bounded_payloads() {
    let f = fixture().await;
    let roster = Roster::new(vec![streamer("alpha", StreamerTier::Primary)]);
    let check = f.check(&roster);
    let long_title = "x".repeat(500);
    f.fetcher.set(Platform::Twitch, live(&long_title, 42));
    check.tick().await.unwrap();
    // Still live: no edge, no event.
    f.clock.set(T0 + 60_000);
    check.tick().await.unwrap();
    f.clock.set(T0 + 120_000);
    f.fetcher.set(Platform::Twitch, FetchedStatus::Offline);
    check.tick().await.unwrap();

    let published = f.events.published();
    assert_eq!(published.len(), 2);
    let went_live = &published[0];
    assert_eq!(went_live.name, LIVESTREAM_STATUS_CHANGED);
    assert_eq!(went_live.dedup_key, format!("alpha:{T0}:went_live"));
    assert_eq!(went_live.data["transition"], "went_live");
    assert_eq!(went_live.data["tier"], "primary");
    assert_eq!(went_live.data["platform"], "twitch");
    assert_eq!(went_live.data["viewerCount"], 42);
    assert_eq!(went_live.data["endedAt"], json!(null));
    assert_eq!(
        went_live.data["title"].as_str().unwrap().chars().count(),
        200
    );
    let offline = &published[1];
    assert_eq!(offline.dedup_key, format!("alpha:{T0}:went_offline"));
    assert_eq!(offline.data["transition"], "went_offline");
    assert_eq!(offline.data["viewerCount"], json!(null));
    assert_eq!(offline.data["maxViewerCount"], 42);
    assert_eq!(offline.data["startedAt"], omni_core::js::to_iso_string(T0));
    assert_eq!(
        offline.data["endedAt"],
        omni_core::js::to_iso_string(T0 + 120_000)
    );
}

#[tokio::test(start_paused = true)]
async fn an_offline_edge_replayed_after_a_crash_keeps_its_dedup_key() {
    let f = fixture().await;
    let roster = Roster::new(vec![streamer("beta", StreamerTier::Primary)]);
    let check = f.check(&roster);
    f.fetcher.set(Platform::Twitch, live("Session", 5));
    check.tick().await.unwrap();
    let live_row = get_status(&f.store.store, "beta").await.unwrap();

    f.fetcher.set(Platform::Twitch, FetchedStatus::Offline);
    check.tick().await.unwrap();
    // A crash before the offline write leaves the live row in place.
    upsert_status(&f.store.store, live_row).await.unwrap();
    f.clock.set(T0 + 20_000);
    check.tick().await.unwrap();

    let offline: Vec<_> = f
        .events
        .published()
        .into_iter()
        .filter(|e| e.data["transition"] == "went_offline")
        .collect();
    assert_eq!(offline.len(), 2);
    assert_eq!(offline[0].dedup_key, offline[1].dedup_key);
    assert_eq!(f.events.distinct().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn background_and_muted_streamers_publish_with_their_tier() {
    let f = fixture().await;
    let muted = Streamer {
        live_notifications: Some(false),
        ..streamer("muted", StreamerTier::Primary)
    };
    let roster = Roster::new(vec![muted, streamer("bg", StreamerTier::Background)]);
    f.fetcher.set(Platform::Twitch, live("t", 1));
    f.check(&roster).tick().await.unwrap();
    assert_eq!(f.notifier.count(), 0);
    let tiers: Vec<(String, String)> = f
        .events
        .published()
        .iter()
        .map(|e| {
            (
                e.data["streamerId"].as_str().unwrap().to_owned(),
                e.data["tier"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        tiers,
        [
            ("muted".to_owned(), "primary".to_owned()),
            ("bg".to_owned(), "background".to_owned())
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn an_unavailable_outbox_never_fails_the_tick() {
    let f = fixture().await;
    f.events.fail();
    let roster = Roster::new(vec![streamer("gamma", StreamerTier::Primary)]);
    f.fetcher.set(Platform::Twitch, live("Session", 5));
    f.check(&roster).tick().await.unwrap();
    assert!(matches!(
        get_status(&f.store.store, "gamma").await.unwrap(),
        StreamerStatus::Live(_)
    ));
    assert_eq!(f.notifier.count(), 1);
}
