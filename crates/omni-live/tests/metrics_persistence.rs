//! Viewer metrics persistence. A write failure is produced by dropping the
//! `blobs` table.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_live::Platform;
use omni_live::error::LiveError;
use omni_live::metrics::{get_platform_viewer_metrics, record_platform_viewer_count};

const NOW: i64 = 1_787_688_000_000; // 2026-08-25T20:00:00Z

#[tokio::test]
async fn keeps_each_platform_account_in_a_separate_daily_series() {
    let (store, _clock) = common::test_store(NOW).await;
    let tz = common::utc();
    record_platform_viewer_count(
        &store.store,
        "iri",
        Platform::YouTube,
        "@imreallyimportant",
        427,
        NOW,
        &tz,
    )
    .await
    .unwrap();
    record_platform_viewer_count(
        &store.store,
        "iri",
        Platform::Kick,
        "imreallyimportant",
        475,
        NOW,
        &tz,
    )
    .await
    .unwrap();
    record_platform_viewer_count(
        &store.store,
        "iri",
        Platform::Kick,
        "IMREALLYIMPORTANT",
        500,
        NOW,
        &tz,
    )
    .await
    .unwrap();
    let metrics = get_platform_viewer_metrics(&store.store, "iri")
        .await
        .unwrap();
    assert_eq!(metrics.len(), 2);
    let youtube = metrics.iter().find(|m| m.platform == "youtube").unwrap();
    assert_eq!(youtube.all_time_max, 427);
    assert_eq!(youtube.daily_buckets[0].max_viewers, 427);
    let kick = metrics.iter().find(|m| m.platform == "kick").unwrap();
    assert_eq!(kick.username, "imreallyimportant");
    assert_eq!(kick.all_time_max, 500);
    assert_eq!(kick.daily_buckets.len(), 1);
    assert_eq!(kick.daily_buckets[0].max_viewers, 500);
}

#[tokio::test]
async fn maps_an_entity_write_failure_to_persistence_error() {
    let (store, _clock) = common::test_store(0).await;
    common::break_store(&store.store).await;
    let error = record_platform_viewer_count(
        &store.store,
        "iri",
        Platform::Kick,
        "iri",
        10,
        0,
        &common::utc(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, LiveError::Persistence { .. }));
    assert!(error.to_string().contains("PersistenceError"), "{error}");
}
