//! The On Air widget timeline, and widget pushes after live
//! ticks when the On Air content changes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;

use omni_api::ios::ApnsEnvironment;
use omni_api::streamers::StreamerTier;
use omni_ios_controls::apns::{ApnsPushKind, ApnsPushResult};
use omni_ios_controls::devices::list_widget_pushes;
use omni_ios_controls::widgets::{build_on_air, on_air_hash};
use omni_live::status::{LiveStatus, OfflineStatus, StreamerStatus, upsert_status};
use omni_live::streamers::{DiscoverySource, Streamer};
use omni_live::{Platform, PlatformBinding};
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityWrite, UpsertOpts};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const STARTED: i64 = 1_787_054_400_000;
const NOW: i64 = STARTED + 3_600_000;

fn streamer(id: &str, platform: Platform, tier: StreamerTier) -> Streamer {
    Streamer::new(
        id,
        id.to_uppercase(),
        vec![PlatformBinding::new(platform, id)],
        tier,
    )
}

fn roster() -> Vec<Streamer> {
    let mut discovered = streamer("dgg:kick:found", Platform::Kick, StreamerTier::Background);
    discovered.discovery_source = Some(DiscoverySource::Dgg);
    vec![
        streamer("alpha", Platform::Twitch, StreamerTier::Primary),
        streamer("beta", Platform::YouTube, StreamerTier::Background),
        streamer("gamma", Platform::Kick, StreamerTier::Primary),
        streamer("delta", Platform::Kick, StreamerTier::Primary),
        streamer("epsilon", Platform::Twitch, StreamerTier::Primary),
        streamer("zeta", Platform::Kick, StreamerTier::Background),
        discovered,
    ]
}

async fn live(
    store: &omni_store::Store,
    id: &str,
    platform: Platform,
    viewers: Option<i64>,
    title: &str,
) {
    upsert_status(
        store,
        StreamerStatus::Live(LiveStatus {
            streamer_id: id.into(),
            primary: PlatformBinding::new(platform, id),
            primary_title: title.into(),
            started_at: STARTED,
            max_viewer_count: viewers.unwrap_or(0) + 10,
            viewer_count: viewers,
            sources: None,
            category: Some("Just Chatting".into()),
            extra: Extra::new(),
        }),
    )
    .await
    .unwrap();
}

async fn offline(store: &omni_store::Store, id: &str, ended_at: Option<i64>) {
    upsert_status(
        store,
        StreamerStatus::Offline(OfflineStatus {
            streamer_id: id.into(),
            last_ended_at: ended_at,
            last_started_at: ended_at.map(|t| t - 1_000),
            last_max_viewer_count: Some(5),
            extra: Extra::new(),
        }),
    )
    .await
    .unwrap();
}

/// The raw `livestream-intelligence` document (owned by omni-live-intel).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IntelligenceDoc {
    streamer_id: String,
    semantic: Value,
}

impl Entity for IntelligenceDoc {
    const NAME: &'static str = "livestream-intelligence";
    type Key = String;
    fn key(&self) -> String {
        self.streamer_id.clone()
    }
}

#[tokio::test]
async fn ranks_live_streamers_like_the_slots_and_lists_recent_primaries() {
    let (store, _) = common::store(NOW).await;
    let s = &store.store;
    live(s, "alpha", Platform::Twitch, Some(10), "Alpha title").await;
    live(s, "beta", Platform::YouTube, Some(10_000), "Beta title").await;
    live(s, "gamma", Platform::Kick, Some(20), "Gamma title").await;
    live(s, "dgg:kick:found", Platform::Kick, None, "").await;
    offline(s, "delta", Some(NOW - 60_000)).await;
    offline(s, "epsilon", Some(NOW - 10_000)).await;
    offline(s, "zeta", Some(NOW - 1_000)).await;
    s.write(|tx| {
        tx.upsert(
            &IntelligenceDoc {
                streamer_id: "gamma".into(),
                semantic: json!({
                    "headline": "Debating the news",
                    "topics": [], "contentKind": "debate", "importance": 0.5,
                    "reason": "", "updatedAt": STARTED + 1_000
                }),
            },
            UpsertOpts::default(),
        )
    })
    .await
    .unwrap();
    let avatars = HashMap::from([("gamma".to_owned(), "https://img/gamma.png".to_owned())]);
    let on_air = build_on_air(s, &roster(), &avatars, NOW).await.unwrap();
    let ids: Vec<&str> = on_air.live.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["gamma", "alpha", "beta", "dgg:kick:found"]);
    assert_eq!(on_air.live_count, 4);
    assert_eq!(on_air.total_viewers, 10_030);
    assert_eq!(
        serde_json::to_value(&on_air.live[0]).unwrap(),
        json!({
            "id": "gamma", "name": "GAMMA", "tier": "primary", "platform": "kick",
            "title": "Gamma title", "category": "Just Chatting", "viewers": 20,
            "peakViewers": 30, "startedAt": STARTED,
            "avatarUrl": "https://img/gamma.png", "watchUrl": "https://kick.com/gamma",
            "appUrl": null, "headline": "Debating the news"
        })
    );
    assert_eq!(
        on_air.live[1].app_url.as_deref(),
        Some("twitch://stream/alpha")
    );
    assert_eq!(on_air.live[1].headline, None);
    assert_eq!(on_air.live[3].title, None);
    assert_eq!(on_air.live[3].viewers, None);
    // Background zeta never appears in the empty state.
    let recent: Vec<(&str, i64)> = on_air
        .recent
        .iter()
        .map(|r| (r.id.as_str(), r.ended_at))
        .collect();
    assert_eq!(recent, [("epsilon", NOW - 10_000), ("delta", NOW - 60_000)]);
    assert_eq!(on_air.recent[0].watch_url, "https://www.twitch.tv/epsilon");
    assert_eq!(on_air.generated_at, NOW);
}

#[tokio::test]
async fn caps_the_live_list_but_counts_every_live_streamer() {
    let (store, _) = common::store(NOW).await;
    let streamers: Vec<Streamer> = (0..10)
        .map(|i| streamer(&format!("s{i}"), Platform::Kick, StreamerTier::Primary))
        .collect();
    for (i, s) in streamers.iter().enumerate() {
        live(&store.store, &s.id, Platform::Kick, Some(i as i64), "t").await;
    }
    let on_air = build_on_air(&store.store, &streamers, &HashMap::new(), NOW)
        .await
        .unwrap();
    assert_eq!(on_air.live.len(), 8);
    assert_eq!(on_air.live_count, 10);
    assert_eq!(on_air.live[0].id, "s9");
    assert_eq!(on_air.total_viewers, 45);
}

#[tokio::test]
async fn the_on_air_hash_follows_the_live_set_and_titles_but_not_rank_or_viewers() {
    let (store, _) = common::store(NOW).await;
    let s = &store.store;
    let streamers = roster();
    live(s, "alpha", Platform::Twitch, Some(10), "A").await;
    live(s, "gamma", Platform::Kick, Some(20), "G").await;
    let hash = |on_air| on_air_hash(&on_air);
    let first = hash(
        build_on_air(s, &streamers, &HashMap::new(), NOW)
            .await
            .unwrap(),
    );
    live(s, "gamma", Platform::Kick, Some(25), "G").await;
    let viewers = hash(
        build_on_air(s, &streamers, &HashMap::new(), NOW + 1)
            .await
            .unwrap(),
    );
    assert_eq!(first, viewers);
    live(s, "alpha", Platform::Twitch, Some(30), "A").await;
    let reordered = hash(
        build_on_air(s, &streamers, &HashMap::new(), NOW)
            .await
            .unwrap(),
    );
    // Alpha now outranks gamma: same set, same titles, no push.
    assert_eq!(first, reordered);
    live(s, "alpha", Platform::Twitch, Some(30), "A2").await;
    let retitled = hash(
        build_on_air(s, &streamers, &HashMap::new(), NOW)
            .await
            .unwrap(),
    );
    assert_ne!(reordered, retitled);
    live(s, "alpha", Platform::Kick, Some(30), "A2").await;
    let replatformed = hash(
        build_on_air(s, &streamers, &HashMap::new(), NOW)
            .await
            .unwrap(),
    );
    assert_ne!(retitled, replatformed);
    live(s, "beta", Platform::YouTube, Some(1), "B").await;
    let joined = hash(
        build_on_air(s, &streamers, &HashMap::new(), NOW)
            .await
            .unwrap(),
    );
    assert_ne!(replatformed, joined);
}

const WIDGET_TOKEN: &str = "abababababababababababababababababababababababababababababababab";

#[tokio::test(start_paused = true)]
async fn pushes_widgets_only_when_the_on_air_content_changes() {
    let (store, clock) = common::store(NOW).await;
    let apns = common::FakeApns::new(Ok(ApnsPushResult::Sent));
    let service = common::service(&store.store, &clock, roster(), Some(apns.clone()));
    service
        .register_widget_push("device-12345", WIDGET_TOKEN, ApnsEnvironment::Sandbox)
        .await
        .unwrap();
    live(&store.store, "alpha", Platform::Twitch, Some(10), "A").await;
    service.reconcile_widgets().await.unwrap();
    let pushes = apns.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0].kind, ApnsPushKind::Widgets);
    assert_eq!(pushes[0].device_token, WIDGET_TOKEN);
    let expires_in = pushes[0].expiration - NOW / 1_000;
    assert!((3_600..3_610).contains(&expires_in), "{expires_in}");
    assert!(
        list_widget_pushes(&store.store).await.unwrap()[0]
            .last_delivered_hash
            .is_some()
    );
    // Viewer counts alone do not push.
    live(&store.store, "alpha", Platform::Twitch, Some(99), "A").await;
    service.reconcile_widgets().await.unwrap();
    assert_eq!(apns.pushes().len(), 1);
    // A title change does.
    live(&store.store, "alpha", Platform::Twitch, Some(99), "B").await;
    service.reconcile_widgets().await.unwrap();
    assert_eq!(apns.pushes().len(), 2);
    // Re-registering the same token keeps the delivered hash.
    service
        .register_widget_push("device-12345", WIDGET_TOKEN, ApnsEnvironment::Sandbox)
        .await
        .unwrap();
    service.reconcile_widgets().await.unwrap();
    assert_eq!(apns.pushes().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn carries_transient_widget_failures_and_drops_invalid_tokens() {
    let (store, clock) = common::store(NOW).await;
    let apns = common::FakeApns::new(common::failed(503, "ServiceUnavailable"));
    let service = common::service(&store.store, &clock, roster(), Some(apns.clone()));
    service
        .register_widget_push("device-12345", WIDGET_TOKEN, ApnsEnvironment::Sandbox)
        .await
        .unwrap();
    service.reconcile_widgets().await.unwrap();
    assert_eq!(apns.pushes().len(), 2, "one immediate retry");
    assert!(
        list_widget_pushes(&store.store).await.unwrap()[0]
            .last_delivered_hash
            .is_none()
    );
    // A permanent rejection is not repeated for the same content.
    apns.clear();
    apns.set_default(common::failed(400, "BadTopic"));
    service.reconcile_widgets().await.unwrap();
    service.reconcile_widgets().await.unwrap();
    assert_eq!(apns.pushes().len(), 1);
    // An invalid token deletes the registration.
    apns.set_default(Ok(ApnsPushResult::InvalidToken {
        reason: "Unregistered".into(),
    }));
    live(&store.store, "alpha", Platform::Twitch, Some(10), "new").await;
    service.reconcile_widgets().await.unwrap();
    assert!(list_widget_pushes(&store.store).await.unwrap().is_empty());
}

#[tokio::test]
async fn does_nothing_without_apns() {
    let (store, clock) = common::store(NOW).await;
    let service = common::service(&store.store, &clock, roster(), None);
    service
        .register_widget_push("device-12345", WIDGET_TOKEN, ApnsEnvironment::Sandbox)
        .await
        .unwrap();
    service.reconcile_widgets().await.unwrap();
    assert!(
        list_widget_pushes(&store.store).await.unwrap()[0]
            .last_delivered_hash
            .is_none()
    );
}
