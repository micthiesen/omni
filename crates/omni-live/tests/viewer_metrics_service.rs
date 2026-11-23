//! The viewer metrics service over a real temporary docstore.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use omni_live::avatars::StreamerAvatar;
use omni_live::error::LiveError;
use omni_live::metrics::{
    ViewerMetrics, ViewerMetricsService, ViewerObservation, get_viewer_metrics,
};
use omni_live::notification_policy::ViewerRecordScope;
use omni_live::notify::LiveSender;
use omni_live::platform::NotificationUrlFields;
use omni_live::{Platform, PlatformBinding};
use omni_store::cbor::Extra;
use omni_store::entity::{EntityWrite, UpsertOpts};

fn url_fields() -> NotificationUrlFields {
    NotificationUrlFields {
        url: "https://example.com".into(),
        url_title: "Watch".into(),
    }
}

fn observation(
    id: &str,
    name: &str,
    count: i64,
    token: Option<&str>,
    scope: ViewerRecordScope,
) -> ViewerObservation {
    ViewerObservation {
        streamer_id: id.into(),
        display_name: name.into(),
        viewer_count: count,
        url_fields: url_fields(),
        token: token.map(str::to_owned),
        scope,
        avatar_bindings: Vec::new(),
    }
}

async fn service(
    now: i64,
) -> (
    omni_testkit::TestStore,
    ViewerMetricsService,
    common::FakeNotifier,
) {
    let (store, _clock) = common::test_store(now).await;
    let notifier = common::FakeNotifier::default();
    let service = ViewerMetricsService::new(
        store.store.clone(),
        common::utc(),
        Arc::new(notifier.clone()),
    );
    (store, service, notifier)
}

async fn seed(store: &omni_store::Store, id: &str, all_time_max: i64) {
    let metrics = ViewerMetrics {
        all_time_max,
        ..ViewerMetrics::empty(id)
    };
    store
        .write(move |tx| tx.upsert(&metrics, UpsertOpts::default()))
        .await
        .unwrap();
}

const T: i64 = 1_790_000_000_000;

#[tokio::test]
async fn notifies_on_a_confirmed_record_with_no_mute_path_records_fire_for_muted_streamers_too() {
    let (_store, service, notifier) = service(T).await;
    service
        .record_viewer_count(
            &observation(
                "muted",
                "Muted",
                100,
                Some("tok-live"),
                ViewerRecordScope::All,
            ),
            T,
        )
        .await
        .unwrap();
    assert_eq!(notifier.count(), 0);
    service
        .record_viewer_count(
            &observation(
                "muted",
                "Muted",
                90,
                Some("tok-live"),
                ViewerRecordScope::All,
            ),
            T,
        )
        .await
        .unwrap();
    let sent = notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0.as_deref(), Some("tok-live"));
    assert!(sent[0].1.title.contains("Muted"));
    assert_eq!(sent[0].1.url, Some(url_fields()));
    assert_eq!(sent[0].1.title, "New all-time record for Muted!");
    assert_eq!(sent[0].1.message, "Peaked at 100 viewers.");
}

#[tokio::test]
async fn notifies_on_flush_pending_peaks_offline_flush_with_no_mute_path() {
    let (_store, service, notifier) = service(T).await;
    service
        .record_viewer_count(
            &observation("muted", "Muted", 100, None, ViewerRecordScope::All),
            T,
        )
        .await
        .unwrap();
    assert_eq!(notifier.count(), 0);
    service
        .flush_pending_peaks(
            &observation("muted", "Muted", 0, None, ViewerRecordScope::All),
            T,
        )
        .await
        .unwrap();
    assert_eq!(notifier.count(), 1);
    assert!(notifier.titles()[0].contains("Muted"));
}

#[tokio::test]
async fn does_not_notify_while_a_peak_is_still_climbing() {
    let (_store, service, notifier) = service(T).await;
    for count in [100, 150, 149] {
        service
            .record_viewer_count(
                &observation("s", "S", count, None, ViewerRecordScope::All),
                T,
            )
            .await
            .unwrap();
    }
    assert_eq!(notifier.count(), 0);
}

#[tokio::test]
async fn uses_the_test_clock_for_bucket_and_confirmed_peak_timestamps() {
    let (store, service, _notifier) = service(42_000).await;
    service
        .record_viewer_count(
            &observation("clocked", "Clocked", 100, None, ViewerRecordScope::All),
            42_000,
        )
        .await
        .unwrap();
    service
        .record_viewer_count(
            &observation("clocked", "Clocked", 90, None, ViewerRecordScope::All),
            42_000,
        )
        .await
        .unwrap();
    let metrics = get_viewer_metrics(&store.store, "clocked").await.unwrap();
    assert_eq!(metrics.all_time_max_timestamp, 42_000);
    assert_eq!(metrics.daily_buckets[0].timestamp, 42_000);
}

#[tokio::test]
async fn returns_persistence_writes_as_typed_failures() {
    let (store, service, _notifier) = service(T).await;
    common::break_store(&store.store).await;
    let error = service
        .record_viewer_count(
            &observation("broken", "Broken", 100, None, ViewerRecordScope::All),
            T,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, LiveError::Persistence { .. }), "{error}");
}

#[tokio::test]
async fn suppresses_window_only_confirmations_7d_30d_90d_when_scope_is_all_time_only() {
    let (store, service, notifier) = service(T).await;
    seed(&store.store, "bg", 500).await;
    let scope = ViewerRecordScope::AllTimeOnly;
    service
        .record_viewer_count(&observation("bg", "Background", 50, Some("tok"), scope), T)
        .await
        .unwrap();
    assert_eq!(notifier.count(), 0);
    service
        .record_viewer_count(&observation("bg", "Background", 40, Some("tok"), scope), T)
        .await
        .unwrap();
    assert_eq!(notifier.count(), 0);
    let persisted = get_viewer_metrics(&store.store, "bg").await.unwrap();
    assert_eq!(persisted.daily_buckets.last().unwrap().max_viewers, 50);
    assert_eq!(persisted.all_time_max, 500);
}

#[tokio::test]
async fn still_notifies_a_genuine_all_time_record_when_scope_is_all_time_only() {
    let (_store, service, notifier) = service(T).await;
    let scope = ViewerRecordScope::AllTimeOnly;
    service
        .record_viewer_count(
            &observation("bg2", "Background2", 100, Some("tok"), scope),
            T,
        )
        .await
        .unwrap();
    assert_eq!(notifier.count(), 0);
    service
        .record_viewer_count(
            &observation("bg2", "Background2", 90, Some("tok"), scope),
            T,
        )
        .await
        .unwrap();
    assert_eq!(notifier.count(), 1);
    assert!(notifier.titles()[0].contains("all-time record"));
}

#[tokio::test]
async fn suppresses_a_window_only_flush_on_went_offline_when_scope_is_all_time_only() {
    let (store, service, notifier) = service(T).await;
    seed(&store.store, "bg3", 500).await;
    let scope = ViewerRecordScope::AllTimeOnly;
    service
        .record_viewer_count(
            &observation("bg3", "Background3", 50, Some("tok"), scope),
            T,
        )
        .await
        .unwrap();
    service
        .flush_pending_peaks(&observation("bg3", "Background3", 0, Some("tok"), scope), T)
        .await
        .unwrap();
    assert_eq!(notifier.count(), 0);
}

#[tokio::test]
async fn formats_the_previous_record_with_grouping() {
    let (store, service, notifier) = service(T).await;
    seed(&store.store, "big", 12_000).await;
    service
        .record_viewer_count(
            &observation("big", "Big", 15_000, None, ViewerRecordScope::All),
            T,
        )
        .await
        .unwrap();
    service
        .record_viewer_count(
            &observation("big", "Big", 1_000, None, ViewerRecordScope::All),
            T,
        )
        .await
        .unwrap();
    let sent = notifier.sent();
    assert_eq!(
        sent[0].1.message,
        "Peaked at 15,000 viewers (previous: 12,000)."
    );
}

const DAY_MS: i64 = 86_400_000;

/// Seeds prior-day buckets as `(days_ago, max_viewers)` pairs.
async fn seed_history(store: &omni_store::Store, id: &str, all_time_max: i64, days: &[(i64, i64)]) {
    let daily_buckets = days
        .iter()
        .map(|&(ago, max_viewers)| {
            let at = T - ago * DAY_MS;
            omni_live::metrics::DailyBucket {
                date: omni_live::metrics::to_date_stamp(at),
                max_viewers,
                timestamp: at,
            }
        })
        .collect();
    let metrics = ViewerMetrics {
        all_time_max,
        daily_buckets,
        ..ViewerMetrics::empty(id)
    };
    store
        .write(move |tx| tx.upsert(&metrics, UpsertOpts::default()))
        .await
        .unwrap();
}

async fn observe(service: &ViewerMetricsService, id: &str, count: i64, at: i64) {
    service
        .record_viewer_count(
            &observation(id, "S", count, None, ViewerRecordScope::All),
            at,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn confirms_a_7_day_high_over_the_prior_week_after_falling_5_percent() {
    let (store, service, notifier) = service(T).await;
    seed_history(&store.store, "w7", 1_000, &[(3, 200), (20, 400)]).await;
    observe(&service, "w7", 150, T).await;
    observe(&service, "w7", 300, T + 60_000).await;
    observe(&service, "w7", 290, T + 120_000).await;
    assert_eq!(notifier.count(), 0, "290 is within 5 percent of 300");
    observe(&service, "w7", 280, T + 180_000).await;
    let sent = notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1.title, "New 7-day high for S!");
    assert_eq!(sent[0].1.message, "Peaked at 300 viewers (previous: 200).");
}

#[tokio::test]
async fn reports_the_highest_priority_window_that_was_beaten() {
    let (store, service, notifier) = service(T).await;
    seed_history(
        &store.store,
        "w30",
        1_000,
        &[(3, 200), (20, 250), (60, 500)],
    )
    .await;
    observe(&service, "w30", 300, T).await;
    observe(&service, "w30", 200, T + 60_000).await;
    let sent = notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1.title, "New 30-day high for S!");
    assert_eq!(sent[0].1.message, "Peaked at 300 viewers (previous: 250).");
}

#[tokio::test]
async fn does_not_repeat_a_window_record_already_confirmed_today() {
    let (store, service, notifier) = service(T).await;
    seed_history(&store.store, "again", 1_000, &[(3, 200)]).await;
    observe(&service, "again", 300, T).await;
    observe(&service, "again", 250, T + 60_000).await;
    assert_eq!(notifier.count(), 1);
    // Back above the prior week, but not above today's confirmed peak.
    observe(&service, "again", 290, T + 120_000).await;
    observe(&service, "again", 200, T + 180_000).await;
    service
        .flush_pending_peaks(
            &observation("again", "S", 0, None, ViewerRecordScope::All),
            T + 240_000,
        )
        .await
        .unwrap();
    assert_eq!(notifier.count(), 1);
}

#[tokio::test]
async fn flushes_a_pending_window_high_when_the_stream_goes_offline() {
    let (store, service, notifier) = service(T).await;
    seed_history(&store.store, "flush", 1_000, &[(10, 400), (40, 600)]).await;
    observe(&service, "flush", 450, T).await;
    assert_eq!(notifier.count(), 0);
    service
        .flush_pending_peaks(
            &observation("flush", "S", 0, None, ViewerRecordScope::All),
            T + 60_000,
        )
        .await
        .unwrap();
    let sent = notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1.title, "New 30-day high for S!");
    assert_eq!(sent[0].1.message, "Peaked at 450 viewers (previous: 400).");
    let persisted = get_viewer_metrics(&store.store, "flush").await.unwrap();
    assert_eq!(persisted.all_time_max, 1_000);
}

#[tokio::test]
async fn background_scope_never_notifies_a_beaten_window_with_history() {
    let (store, service, notifier) = service(T).await;
    seed_history(&store.store, "bgw", 1_000, &[(3, 200), (20, 250)]).await;
    let scope = ViewerRecordScope::AllTimeOnly;
    for (count, at) in [(300, T), (200, T + 60_000)] {
        service
            .record_viewer_count(&observation("bgw", "S", count, None, scope), at)
            .await
            .unwrap();
    }
    assert_eq!(notifier.count(), 0);
}

#[tokio::test]
async fn silent_scope_records_an_all_time_high_without_notifying() {
    let (store, service, notifier) = service(T).await;
    let scope = ViewerRecordScope::Silent;
    for count in [1_000, 900] {
        service
            .record_viewer_count(&observation("dgg:kick:x", "X", count, None, scope), T)
            .await
            .unwrap();
    }
    service
        .record_viewer_count(&observation("dgg:kick:x", "X", 2_000, None, scope), T)
        .await
        .unwrap();
    service
        .flush_pending_peaks(&observation("dgg:kick:x", "X", 0, None, scope), T)
        .await
        .unwrap();
    assert_eq!(notifier.count(), 0);
    let persisted = get_viewer_metrics(&store.store, "dgg:kick:x")
        .await
        .unwrap();
    assert_eq!(persisted.all_time_max, 2_000);
}

#[tokio::test]
async fn record_notifications_name_the_streamer_with_its_cached_avatar() {
    let (store, service, notifier) = service(T).await;
    let primary = PlatformBinding::new(Platform::Kick, "destiny");
    let avatar = StreamerAvatar {
        binding: "kick:destiny".into(),
        platform: Platform::Kick,
        username: "destiny".into(),
        url: Some("https://files.kick.com/destiny.png".into()),
        fetched_at: Some(T),
        failed_at: None,
        extra: Extra::new(),
    };
    store
        .store
        .write(move |tx| tx.upsert(&avatar, UpsertOpts::default()))
        .await
        .unwrap();
    let scope = ViewerRecordScope::All;
    let mut first = observation("destiny", "Destiny", 100, None, scope);
    first.avatar_bindings = vec![primary.clone()];
    let mut second = first.clone();
    second.viewer_count = 90;
    service.record_viewer_count(&first, T).await.unwrap();
    service.record_viewer_count(&second, T).await.unwrap();
    let (_, message) = notifier.sent().pop().unwrap();
    assert_eq!(message.thread_id.as_deref(), Some("live:destiny"));
    assert_eq!(
        message.sender,
        Some(LiveSender {
            streamer_id: "destiny".into(),
            display_name: "Destiny".into(),
            avatar_url: Some("https://files.kick.com/destiny.png".into()),
        })
    );
}
