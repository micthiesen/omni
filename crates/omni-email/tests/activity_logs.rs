//! Per-activity log capture and persistence. Persistence failures are injected
//! with a SQLite trigger.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_core::LogLevel;
use omni_email::activity_logs::{self, EmailActivityLogData, with_capture};
use omni_store::LogLine;
use omni_tasks::{EventBus, RunLogLayer, RunLogs};
use tracing_subscriber::layer::SubscriberExt as _;

use common::{NOW, break_writes, store_at};

fn subscriber(logs: &RunLogs) -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(
        tracing_subscriber::registry().with(RunLogLayer::new(logs.clone())),
    )
}

#[tokio::test]
async fn captures_lines_logged_during_processing_and_persists_them() {
    let (store, clock) = store_at(NOW).await;
    let logs = RunLogs::new(EventBus::new(16), clock);
    let _guard = subscriber(&logs);
    let result = with_capture(
        &store.store,
        &logs,
        "ParcelTracker#e1",
        "ParcelTracker",
        async {
            tracing::info!(target: "Test", "extracting");
            tokio::task::yield_now().await;
            tracing::info!(target: "Test", "submitted");
            42
        },
    )
    .await;
    assert_eq!(result, 42);
    let stored = activity_logs::get(&store.store, "ParcelTracker#e1")
        .await
        .unwrap()
        .unwrap();
    let messages: Vec<&str> = stored.lines.iter().map(|l| l.msg.as_str()).collect();
    assert_eq!(messages, ["extracting", "submitted"]);
    assert_eq!(stored.dropped, 0);
}

#[tokio::test]
async fn persists_no_row_when_nothing_was_logged() {
    let (store, clock) = store_at(NOW).await;
    let logs = RunLogs::new(EventBus::new(16), clock);
    let _guard = subscriber(&logs);
    with_capture(
        &store.store,
        &logs,
        "ParcelTracker#e2",
        "ParcelTracker",
        async {},
    )
    .await;
    assert!(
        activity_logs::get(&store.store, "ParcelTracker#e2")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn deletes_a_stale_row_when_a_reprocess_captures_nothing() {
    let (store, clock) = store_at(NOW).await;
    let logs = RunLogs::new(EventBus::new(16), clock);
    let _guard = subscriber(&logs);
    activity_logs::save(
        &store.store,
        EmailActivityLogData {
            activity_id: "ParcelTracker#e3".to_owned(),
            lines: vec![LogLine {
                t: 1,
                level: LogLevel::Info,
                logger: "Test".to_owned(),
                msg: "old".to_owned(),
            }],
            dropped: 0,
        },
    )
    .await
    .unwrap();
    assert!(
        activity_logs::get(&store.store, "ParcelTracker#e3")
            .await
            .unwrap()
            .is_some()
    );

    with_capture(
        &store.store,
        &logs,
        "ParcelTracker#e3",
        "ParcelTracker",
        async {},
    )
    .await;
    assert!(
        activity_logs::get(&store.store, "ParcelTracker#e3")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn still_persists_the_capture_when_fn_throws() {
    let (store, clock) = store_at(NOW).await;
    let logs = RunLogs::new(EventBus::new(16), clock);
    let _guard = subscriber(&logs);
    let result: Result<(), &str> = with_capture(
        &store.store,
        &logs,
        "ParcelTracker#e4",
        "ParcelTracker",
        async {
            tracing::info!(target: "Test", "before failure");
            Err("boom")
        },
    )
    .await;
    assert_eq!(result, Err("boom"));
    let stored = activity_logs::get(&store.store, "ParcelTracker#e4")
        .await
        .unwrap()
        .unwrap();
    let messages: Vec<&str> = stored.lines.iter().map(|l| l.msg.as_str()).collect();
    assert_eq!(messages, ["before failure"]);
}

#[tokio::test]
async fn preserves_successful_handler_acceptance_when_diagnostic_persistence_fails() {
    let (store, clock) = store_at(NOW).await;
    let logs = RunLogs::new(EventBus::new(16), clock);
    let _guard = subscriber(&logs);
    break_writes(&store.store, "email-activity-log").await;
    let result = with_capture(
        &store.store,
        &logs,
        "ParcelTracker#e5",
        "ParcelTracker",
        async {
            tracing::info!(target: "Test", "external action completed");
            "accepted"
        },
    )
    .await;
    assert_eq!(result, "accepted");
}

#[tokio::test]
async fn preserves_the_original_handler_failure_when_diagnostic_persistence_also_fails() {
    let (store, clock) = store_at(NOW).await;
    let logs = RunLogs::new(EventBus::new(16), clock);
    let _guard = subscriber(&logs);
    break_writes(&store.store, "email-activity-log").await;
    let result: Result<(), &str> = with_capture(
        &store.store,
        &logs,
        "ParcelTracker#e6",
        "ParcelTracker",
        async {
            tracing::info!(target: "Test", "before handler failure");
            Err("submission failed")
        },
    )
    .await;
    assert_eq!(result, Err("submission failed"));
}

#[tokio::test]
async fn a_dropped_capture_releases_its_live_buffer() {
    let (store, clock) = store_at(NOW).await;
    let logs = RunLogs::new(EventBus::new(16), clock);
    let _guard = subscriber(&logs);
    let pending = with_capture(
        &store.store,
        &logs,
        "ParcelTracker#e7",
        "ParcelTracker",
        async {
            tracing::info!(target: "Test", "started");
            std::future::pending::<()>().await;
        },
    );
    let timed_out = tokio::time::timeout(std::time::Duration::from_millis(20), pending).await;
    assert!(timed_out.is_err());
    // Interrupted work persists nothing.
    assert!(
        activity_logs::get(&store.store, "ParcelTracker#e7")
            .await
            .unwrap()
            .is_none()
    );
}
