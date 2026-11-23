//! Reset task logging: unchanged polls log at debug, changes at INFO.
//! Its own test binary so no concurrent test races the callsite interest cache.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::{Arc, Mutex};

use common::FakeNotifier;
use futures::future::BoxFuture;
use jiff::tz::TimeZone;
use omni_core::clock::SharedClock;
use omni_personal::claude_resets::task::claude_reset_task;
use omni_personal::reset_alerts::delivery::Claude;
use omni_personal::reset_alerts::{
    ResetDeliveryLedger, ResetSnapshot, ResetSourceError, SnapshotSource,
};
use omni_testkit::{TEST_EPOCH_MS, TestStore, test_clock};
use serde_json::{Map, json};

fn tz() -> TimeZone {
    TimeZone::get("America/Vancouver").unwrap()
}

struct MutableSource(Mutex<ResetSnapshot>);

impl SnapshotSource for MutableSource {
    fn read(&self) -> BoxFuture<'_, Result<ResetSnapshot, ResetSourceError>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().clone()) })
    }
}

#[tokio::test]
async fn logs_snapshot_and_summary_at_info_only_when_they_change() {
    let clock: SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock).await;
    let metadata = |generated: &str, items: u32| {
        let mut metadata = Map::new();
        metadata.insert("feedGeneratedAt".into(), json!(generated));
        metadata.insert("feedItems".into(), json!(items));
        metadata
    };
    let source = Arc::new(MutableSource(Mutex::new(ResetSnapshot {
        now: TEST_EPOCH_MS,
        metadata: metadata("2026-10-09T21:00:00Z", 3),
        alerts: vec![],
    })));
    let task = claude_reset_task(
        source.clone(),
        ResetDeliveryLedger::<Claude>::new(store.store.clone(), Arc::new(FakeNotifier::default())),
        &tz(),
    )
    .unwrap();
    let logs = omni_testkit::capture_logs();

    task.run_once().await.unwrap();
    source.0.lock().unwrap().metadata = metadata("2026-10-09T21:01:00Z", 3);
    task.run_once().await.unwrap();
    source.0.lock().unwrap().metadata = metadata("2026-10-09T21:02:00Z", 4);
    task.run_once().await.unwrap();

    let levels = |needle: &str| {
        logs.events()
            .into_iter()
            .filter(|event| event.message.contains(needle))
            .map(|event| event.level)
            .collect::<Vec<_>>()
    };
    use tracing::Level;
    assert_eq!(
        levels("Reset source snapshot"),
        vec![Level::INFO, Level::DEBUG, Level::INFO]
    );
    assert_eq!(
        levels("Reset Radar:"),
        vec![Level::INFO, Level::DEBUG, Level::DEBUG]
    );
}
