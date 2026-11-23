//! The four live slots and their delivery hash.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_api::streamers::{DggPresence, StreamerTier};
use omni_ios_controls::slots::{build_live_control_slots, live_control_slot_hash};
use omni_live::status::{LiveStatus, StreamerStatus, upsert_status};
use omni_live::streamers::{DiscoverySource, Streamer};
use omni_live::{Platform, PlatformBinding};
use omni_store::cbor::Extra;

const STARTED: i64 = 1_787_054_400_000; // 2026-08-18T12:00:00Z

fn streamers() -> Vec<Streamer> {
    vec![
        Streamer::new(
            "alpha",
            "Alpha",
            vec![PlatformBinding::new(Platform::Twitch, "alpha")],
            StreamerTier::Primary,
        ),
        Streamer::new(
            "beta",
            "Beta",
            vec![PlatformBinding::new(Platform::YouTube, "beta")],
            StreamerTier::Background,
        ),
        Streamer::new(
            "gamma",
            "Gamma",
            vec![PlatformBinding::new(Platform::Kick, "gamma")],
            StreamerTier::Primary,
        ),
    ]
}

async fn live(store: &omni_store::Store, id: &str, platform: Platform, viewers: i64) {
    upsert_status(
        store,
        StreamerStatus::Live(LiveStatus {
            streamer_id: id.into(),
            primary: PlatformBinding::new(platform, id),
            primary_title: format!("{id} title"),
            started_at: STARTED,
            max_viewer_count: viewers + 10,
            viewer_count: Some(viewers),
            sources: None,
            category: None,
            extra: Extra::new(),
        }),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn ranks_primary_channels_before_hotter_background_channels() {
    let (store, _) = common::store(0).await;
    live(&store.store, "alpha", Platform::Twitch, 10).await;
    live(&store.store, "beta", Platform::YouTube, 10_000).await;
    live(&store.store, "gamma", Platform::Kick, 20).await;
    let slots = build_live_control_slots(&store.store, &streamers(), "http://omni.boris", 123)
        .await
        .unwrap();
    let ids: Vec<Option<&str>> = slots.iter().map(|s| s.streamer_id.as_deref()).collect();
    assert_eq!(ids, [Some("gamma"), Some("alpha"), Some("beta"), None]);
    assert_eq!(slots[0].slot, 1);
    assert_eq!(slots[0].display_name, "Gamma");
    assert_eq!(slots[0].url, "https://kick.com/gamma");
    assert_eq!(slots[0].updated_at, 123);
    assert!(!slots[3].is_live);
    assert_eq!(slots[3].display_name, "Nobody Live");
    assert_eq!(slots[3].url, "http://omni.boris");
}

#[tokio::test]
async fn ranks_dgg_only_channels_by_dgg_viewers_but_returns_platform_viewers() {
    let (store, _) = common::store(0).await;
    let mut large = Streamer::new(
        "dgg:kick:large",
        "Large Platform Stream",
        vec![PlatformBinding::new(Platform::Kick, "large")],
        StreamerTier::Background,
    );
    large.discovery_source = Some(DiscoverySource::Dgg);
    large.dgg = Some(DggPresence {
        hosted: false,
        viewers: Some(10),
    });
    let mut small = Streamer::new(
        "configured-small",
        "Configured Small Stream",
        vec![PlatformBinding::new(Platform::Kick, "configured-small")],
        StreamerTier::Background,
    );
    small.dgg = Some(DggPresence {
        hosted: true,
        viewers: Some(1),
    });
    live(&store.store, "dgg:kick:large", Platform::Kick, 50_000).await;
    live(&store.store, "configured-small", Platform::Kick, 50).await;
    let slots = build_live_control_slots(&store.store, &[large, small], "http://omni.boris", 123)
        .await
        .unwrap();
    let ids: Vec<Option<&str>> = slots.iter().map(|s| s.streamer_id.as_deref()).collect();
    assert_eq!(
        ids,
        [Some("configured-small"), Some("dgg:kick:large"), None, None]
    );
    assert_eq!(slots[1].viewer_count, Some(50_000));
    assert!(
        serde_json::to_value(&slots[1])
            .unwrap()
            .get("orderingViewerCount")
            .is_none()
    );
}

#[tokio::test]
async fn does_not_change_the_state_hash_when_only_updated_at_changes() {
    let (store, _) = common::store(0).await;
    live(&store.store, "alpha", Platform::Twitch, 10).await;
    let first = build_live_control_slots(&store.store, &streamers(), "http://omni.boris", 1)
        .await
        .unwrap();
    let second = build_live_control_slots(&store.store, &streamers(), "http://omni.boris", 2)
        .await
        .unwrap();
    assert_eq!(
        live_control_slot_hash(&first[0]),
        live_control_slot_hash(&second[0])
    );
}

#[tokio::test]
async fn does_not_push_for_viewer_and_uptime_changes_that_leave_the_slot_unchanged() {
    let (store, _) = common::store(0).await;
    live(&store.store, "alpha", Platform::Twitch, 10).await;
    let first = build_live_control_slots(&store.store, &streamers(), "http://omni.boris", 1)
        .await
        .unwrap();
    upsert_status(
        &store.store,
        StreamerStatus::Live(LiveStatus {
            streamer_id: "alpha".into(),
            primary: PlatformBinding::new(Platform::Twitch, "alpha"),
            primary_title: "alpha title".into(),
            started_at: STARTED + 3_600_000,
            max_viewer_count: 999,
            viewer_count: Some(500),
            sources: None,
            category: None,
            extra: Extra::new(),
        }),
    )
    .await
    .unwrap();
    let second = build_live_control_slots(&store.store, &streamers(), "http://omni.boris", 2)
        .await
        .unwrap();
    assert_eq!(
        live_control_slot_hash(&first[0]),
        live_control_slot_hash(&second[0])
    );
}

#[test]
fn hashes_the_js_json_of_the_visible_state() {
    // sha256(JSON.stringify({slot:1,isLive:false,streamerId:null,displayName:"Nobody Live",title:null,url:"http://omni.boris"}))
    let slot = omni_api::ios::LiveSlotState {
        slot: 1,
        is_live: false,
        streamer_id: None,
        display_name: "Nobody Live".into(),
        title: None,
        platform: None,
        url: "http://omni.boris".into(),
        viewer_count: None,
        started_at: None,
        updated_at: 5,
    };
    // Pinned: the persisted lastDeliveredHash must stay stable across releases.
    assert_eq!(
        live_control_slot_hash(&slot),
        "cfb5bd175d198f96a15bd62ef98b762c26a4b1d97a4a8ac67ba879518356c021"
    );
}
