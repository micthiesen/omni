//! The one-minute reset alert task, plus the
//! Codex/Claude snapshot readers against stubbed sources.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::FakeNotifier;
use futures::future::BoxFuture;
use jiff::tz::TimeZone;
use omni_core::clock::SharedClock;
use omni_http::public::PublicHttpClient;
use omni_personal::claude_resets::task::{ClaudeSource, claude_reset_task};
use omni_personal::codex_resets::task::CodexSource;
use omni_personal::reset_alerts::delivery::Claude;
use omni_personal::reset_alerts::task::ResetRunError;
use omni_personal::reset_alerts::{
    ResetAlert, ResetDeliveryLedger, ResetSnapshot, ResetSourceError, SnapshotSource,
};
use omni_tasks::Task;
use omni_testkit::{TEST_EPOCH_MS, TestStore, mock_http, mock_server, test_clock};
use serde_json::{Map, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

struct ScriptedSource {
    snapshot: ResetSnapshot,
    reads: AtomicUsize,
    fail_next: Mutex<bool>,
}

impl SnapshotSource for ScriptedSource {
    fn read(&self) -> BoxFuture<'_, Result<ResetSnapshot, ResetSourceError>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let mut fail = self.fail_next.lock().unwrap();
            if *fail {
                *fail = false;
                return Err(ResetSourceError::new("fetch", "offline"));
            }
            Ok(self.snapshot.clone())
        })
    }
}

fn tz() -> TimeZone {
    TimeZone::get("America/Vancouver").unwrap()
}

#[tokio::test]
async fn re_reads_each_poll_delivers_once_and_clears_stale_success_on_source_failure() {
    let clock: SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock).await;
    let now = TEST_EPOCH_MS;
    let mut metadata = Map::new();
    metadata.insert("feedItems".into(), json!(1));
    let source = Arc::new(ScriptedSource {
        snapshot: ResetSnapshot {
            now,
            metadata,
            alerts: vec![ResetAlert {
                key: "task-test".into(),
                aliases: vec![],
                title: "Reset reported".into(),
                message: "Check Usage".into(),
                url: "https://example.com".into(),
                occurred_at: now,
            }],
        },
        reads: AtomicUsize::new(0),
        fail_next: Mutex::new(false),
    });
    let notifier = Arc::new(FakeNotifier::default());
    let task = claude_reset_task(
        source.clone(),
        ResetDeliveryLedger::<Claude>::new(store.store.clone(), notifier.clone()),
        &tz(),
    )
    .unwrap();
    assert_eq!(task.name(), "ClaudeResets");
    assert_eq!(task.display_name(), Some("Claude Code Reset Alerts"));
    assert!(task.options().run_on_startup);
    assert_eq!(task.schedule().as_str(), "0 * * * * *");

    task.run_once().await.unwrap();
    assert!(task.last_run_summary().unwrap().contains("1 sent"));
    task.run_once().await.unwrap();
    assert!(
        task.last_run_summary()
            .unwrap()
            .contains("1 already handled")
    );
    assert_eq!(notifier.calls(), 1);

    *source.fail_next.lock().unwrap() = true;
    assert!(matches!(
        task.run_once().await,
        Err(ResetRunError::Source(_))
    ));
    assert_eq!(task.last_run_summary(), None);
    assert_eq!(source.reads.load(Ordering::SeqCst), 3);
    assert_eq!(notifier.calls(), 1);
}

fn public_client(server: &wiremock::MockServer, origins: &[&str]) -> PublicHttpClient {
    PublicHttpClient::new(&mock_http(server, origins)).allow_loopback_for_tests()
}

#[tokio::test]
async fn claude_source_reads_the_catalog_and_reports_newest_event() {
    let server = mock_server().await;
    let now = "2026-10-07T16:00:00Z"
        .parse::<jiff::Timestamp>()
        .unwrap()
        .as_millisecond();
    Mock::given(method("GET"))
        .and(path("/data/events.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "updated": "2026-10-07",
            "events": [
                {"id": "older", "date": "2026-10-01T15:00:00Z", "type": "counter-reset",
                 "status": "historic", "confidence": "confirmed", "plans": [], "surfaces": ["claude-code"],
                 "title": "Old", "summary": "Old reset.", "sources": []},
                {"id": "new", "date": "2026-10-07T15:00:00Z", "type": "counter-reset",
                 "status": "historic", "confidence": "confirmed", "plans": [], "surfaces": ["claude-code"],
                 "title": "New", "summary": "New reset.", "sources": [{"url": "https://x.com/a/status/9"}]}
            ]
        })))
        .mount(&server)
        .await;
    let source = ClaudeSource {
        http: public_client(&server, &["https://resetradar.com"]),
        clock: test_clock(now),
        tz: tz(),
    };
    let snapshot = source.read().await.unwrap();
    assert_eq!(snapshot.alerts.len(), 1);
    assert_eq!(snapshot.alerts[0].key, "new:reported");
    assert_eq!(snapshot.metadata["newestEventId"], json!("new"));
    assert_eq!(snapshot.metadata["catalogUpdatedAt"], json!("2026-10-07"));
}

#[tokio::test]
async fn codex_source_fails_visibly_on_stale_feeds_and_http_errors() {
    let server = mock_server().await;
    let now = "2026-10-02T16:00:00Z"
        .parse::<jiff::Timestamp>()
        .unwrap()
        .as_millisecond();
    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "generatedAt": "2026-10-02T14:00:00Z", "items": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
        .mount(&server)
        .await;
    let source = CodexSource {
        http: public_client(&server, &["https://resetbeacon.com"]),
        clock: test_clock(now),
        tz: tz(),
    };
    let error = source.read().await.unwrap_err();
    assert_eq!(error.operation, "check alert feed freshness");

    let broken = mock_server().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&broken)
        .await;
    let source = CodexSource {
        http: public_client(&broken, &["https://resetbeacon.com"]),
        clock: test_clock(now),
        tz: tz(),
    };
    let error = source.read().await.unwrap_err();
    assert!(error.to_string().contains("503"), "{error}");
}

#[tokio::test]
async fn codex_source_combines_history_fallbacks_into_a_snapshot() {
    let server = mock_server().await;
    let now = "2026-10-02T12:00:00Z"
        .parse::<jiff::Timestamp>()
        .unwrap()
        .as_millisecond();
    Mock::given(method("GET"))
        .and(path("/api/alerts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "generatedAt": "2026-10-02T12:00:00.000Z",
            "items": [{
                "id": "parent-feed", "eventId": "existing-event", "postId": "post-parent",
                "topic": "schedule", "state": "official_scheduled", "title": "Scheduled",
                "summary": "Earlier", "sourceUrl": "https://resetbeacon.com/parent",
                "evidenceId": null, "targetAt": "2026-10-02T11:00:00Z",
                "publishedAt": "2026-10-02T11:00:00Z", "withdrawn": false
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [
            {"id": "parent", "kind": "special_global", "scope": "all", "eventKind": "scheduled",
             "evidenceClass": "named_public_source", "status": "fulfilled", "fulfilledBy": "done",
             "supersededBy": null, "sources": [{"announcementId": "post-parent", "url": "https://resetbeacon.com/parent"}]},
            {"id": "done", "kind": "special_global", "scope": "all", "eventKind": "completed",
             "evidenceClass": "reported", "status": "completed", "fulfilledBy": null,
             "supersededBy": null, "announcedAt": "2026-10-02T11:30:00Z",
             "sources": [{"announcementId": "post-done", "url": "https://resetbeacon.com/post/2"}]}
        ]})))
        .mount(&server)
        .await;
    let source = CodexSource {
        http: public_client(&server, &["https://resetbeacon.com"]),
        clock: test_clock(now),
        tz: tz(),
    };
    let snapshot = source.read().await.unwrap();
    assert_eq!(snapshot.metadata["completedHistoryFallbacks"], json!(1));
    assert_eq!(snapshot.metadata["newestAlertId"], json!("parent-feed"));
    assert_eq!(snapshot.alerts.len(), 1);
    assert_eq!(snapshot.alerts[0].key, "existing-event:landed:non-banked");
    assert_eq!(
        snapshot.alerts[0].aliases,
        vec!["post:post-done:landed:non-banked".to_owned()]
    );
}
