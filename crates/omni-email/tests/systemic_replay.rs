//! Systemic extraction failures: emails are parked until a different build
//! runs, released once per build with a per-boot cap, replayed through the
//! retry queue, and alerted once per signature.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use omni_core::email::HandlerError;
use omni_email::activity::{
    self, EmailActivityData, EmailActivityOutcome, EmailPipelineName, NewActivity,
};
use omni_email::retry::{self, EmailRetryData, MAX_RETRY_ATTEMPTS};
use omni_email::retry_task::{EmailRetryTask, SCHEDULE};
use omni_email::systemic::{
    self, EmailReplayData, MAX_BUILD_REPLAYS, REPLAY_LOOKBACK_MS, Signature, SystemicReporter,
};
use omni_runtime::Ports;
use omni_store::Store;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_tasks::CronSchedule;
use omni_testkit::TestApp;

use common::{FakeReader, fn_handler, handlers};

/// The 2026-10-09 production failure, as the calendar pipeline recorded it.
const BCS_DETAIL: &str = "extraction failed: Calendar extraction failed: provider error 400: Invalid schema for response_format 'calendar_event_extraction': In context=('properties', 'events', 'items'), $ref cannot have keywords {'description'}.";

fn signature() -> Signature {
    systemic::classify_recorded(BCS_DETAIL).unwrap()
}

fn reporter(app: &TestApp) -> SystemicReporter {
    SystemicReporter::new(app.ctx.store.clone(), app.ctx.pushover.clone())
}

async fn park(store: &Store, pipeline: &str, email_id: &str, build: &str, created_at: i64) {
    let row = EmailRetryData {
        retry_key: retry::retry_key(pipeline, email_id),
        pipeline: pipeline.to_owned(),
        email_id: email_id.to_owned(),
        reason: BCS_DETAIL.to_owned(),
        enqueue_count: Some(1),
        attempts: 0,
        next_attempt_at: created_at,
        created_at,
        awaiting_build: Some(build.to_owned()),
        signature: Some(signature().key),
        extra: Default::default(),
    };
    store
        .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
        .await
        .unwrap();
}

async fn record_error(store: &Store, email_id: &str, detail: &str) {
    activity::record(
        store,
        NewActivity {
            detail: Some(detail.to_owned()),
            ..NewActivity::new(
                EmailPipelineName::CalendarEvents,
                activity::ActivityEmail {
                    id: email_id.to_owned(),
                    subject: "BCS1882-Notice-Community Manager on Vacation".to_owned(),
                    from: "strata@example.com".to_owned(),
                    received_at: "2026-10-09T17:00:00.000Z".to_owned(),
                },
                EmailActivityOutcome::Error,
            )
        },
    )
    .await
    .unwrap();
}

async fn row(store: &Store, pipeline: &str, email_id: &str) -> Option<EmailRetryData> {
    retry::get(store, &retry::retry_key(pipeline, email_id))
        .await
        .unwrap()
}

async fn ledger(store: &Store, key: &str) -> Option<EmailReplayData> {
    let key = key.to_owned();
    store
        .read(move |docs| docs.get::<EmailReplayData>(&key))
        .await
        .unwrap()
}

/// The test clock advances on every read.
fn now(app: &TestApp) -> i64 {
    app.ctx.clock.now_ms()
}

#[tokio::test]
async fn a_systemic_failure_parks_the_email_and_alerts_once_per_signature() {
    let app = TestApp::new().await;
    let reporter = reporter(&app);
    for id in ["e1", "e2", "e3", "e4", "e5"] {
        reporter
            .report("CalendarEvents", id, BCS_DETAIL, &signature())
            .await
            .unwrap();
    }
    let build = systemic::current_build().await;
    let parked = row(&app.ctx.store, "CalendarEvents", "e3").await.unwrap();
    assert_eq!(parked.awaiting_build.as_deref(), Some(build.as_str()));
    assert_eq!(parked.signature, Some(signature().key));
    assert!(retry::select_due(&[parked], now(&app) + 365 * 86_400_000).is_empty());

    let pushes = app.pushes.all();
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        pushes[0].message.title.as_deref(),
        Some("Calendar extraction failing")
    );
    assert!(
        pushes[0]
            .message
            .message
            .contains("$ref cannot have keywords")
    );
    let alerts = systemic::alerts(&app.ctx.store).await.unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].failures, 5);

    // A different signature is a distinct incident.
    let other = Signature::from_text("provider error 401: Incorrect API key provided");
    reporter
        .report("CalendarEvents", "e6", "x", &other)
        .await
        .unwrap();
    assert_eq!(app.pushes.all().len(), 2);
}

#[tokio::test]
async fn the_same_build_keeps_rows_parked_and_a_new_build_releases_them_once() {
    let app = TestApp::new().await;
    let store = &app.ctx.store;
    park(store, "CalendarEvents", "e1", "build-1", now(&app) - 60_000).await;

    let same = systemic::release_for_build(store, "build-1", 20)
        .await
        .unwrap();
    assert_eq!(same.released, 0);
    assert!(
        row(store, "CalendarEvents", "e1")
            .await
            .unwrap()
            .awaiting_build
            .is_some()
    );

    let next = systemic::release_for_build(store, "build-2", 20)
        .await
        .unwrap();
    assert_eq!(next.released, 1);
    let released = row(store, "CalendarEvents", "e1").await.unwrap();
    assert_eq!(released.awaiting_build, None);
    assert_eq!(
        retry::select_due(std::slice::from_ref(&released), now(&app)).len(),
        1
    );
    assert_eq!(
        ledger(store, "CalendarEvents#e1").await.unwrap().builds,
        ["build-2"]
    );

    // A restart of the same build does not release it again.
    let again = systemic::release_for_build(store, "build-2", 20)
        .await
        .unwrap();
    assert_eq!(again.released, 0);
}

#[tokio::test]
async fn releases_are_capped_per_boot_and_the_rest_follow_on_the_next_boot() {
    let app = TestApp::new().await;
    let store = &app.ctx.store;
    let base = now(&app);
    for i in 0..25 {
        park(
            store,
            "ParcelTracker",
            &format!("e{i}"),
            "build-1",
            base - i * 1_000,
        )
        .await;
    }
    let first = systemic::release_for_build(store, "build-2", 20)
        .await
        .unwrap();
    assert_eq!((first.released, first.deferred), (20, 5));
    // Newest failures first: the five oldest wait.
    assert!(
        row(store, "ParcelTracker", "e24")
            .await
            .unwrap()
            .awaiting_build
            .is_some()
    );
    assert!(
        row(store, "ParcelTracker", "e0")
            .await
            .unwrap()
            .awaiting_build
            .is_none()
    );

    let second = systemic::release_for_build(store, "build-2", 20)
        .await
        .unwrap();
    assert_eq!((second.released, second.deferred), (5, 0));
}

#[tokio::test]
async fn an_email_replays_under_at_most_a_few_builds_then_is_dropped() {
    let app = TestApp::new().await;
    let store = &app.ctx.store;
    for build in 1..=MAX_BUILD_REPLAYS {
        park(
            store,
            "CalendarEvents",
            "e1",
            &format!("build-{build}"),
            now(&app),
        )
        .await;
        let report = systemic::release_for_build(store, &format!("build-{}", build + 1), 20)
            .await
            .unwrap();
        assert_eq!(report.released, 1);
    }
    park(store, "CalendarEvents", "e1", "build-9", now(&app)).await;
    let report = systemic::release_for_build(store, "build-10", 20)
        .await
        .unwrap();
    assert_eq!((report.released, report.expired), (0, 1));
    assert!(row(store, "CalendarEvents", "e1").await.is_none());
}

#[tokio::test]
async fn stale_and_unsafe_rows_are_never_released() {
    let app = TestApp::new().await;
    let store = &app.ctx.store;
    park(
        store,
        "CalendarEvents",
        "old",
        "build-1",
        now(&app) - REPLAY_LOOKBACK_MS - 1,
    )
    .await;
    park(store, "McpEvents", "m1", "build-1", now(&app)).await;
    let report = systemic::release_for_build(store, "build-2", 20)
        .await
        .unwrap();
    assert_eq!((report.released, report.expired), (0, 1));
    assert!(row(store, "CalendarEvents", "old").await.is_none());
    assert!(
        row(store, "McpEvents", "m1")
            .await
            .unwrap()
            .awaiting_build
            .is_some()
    );
}

#[tokio::test]
async fn adopts_recent_systemic_error_activity_without_a_retry_row() {
    let app = TestApp::new().await;
    let store = &app.ctx.store;
    record_error(store, "bcs1882", BCS_DETAIL).await;
    record_error(
        store,
        "content",
        "extraction failed: Calendar extraction failed: No object generated: response did not match schema.",
    )
    .await;
    record_error(
        store,
        "busy",
        "extraction failed: Calendar extraction failed: provider error 503: overloaded",
    )
    .await;

    let report = systemic::release_for_build(store, "build-2", 20)
        .await
        .unwrap();
    assert_eq!(report.released, 1);
    let adopted = row(store, "CalendarEvents", "bcs1882").await.unwrap();
    assert_eq!(adopted.awaiting_build, None);
    assert_eq!(adopted.attempts, 0);
    assert_eq!(adopted.signature, Some(signature().key));
    assert!(row(store, "CalendarEvents", "content").await.is_none());
    assert!(row(store, "CalendarEvents", "busy").await.is_none());

    // Once replayed (row cleared, activity still an error), it is not adopted again.
    retry::clear(store, "CalendarEvents", "bcs1882")
        .await
        .unwrap();
    let again = systemic::release_for_build(store, "build-3", 20)
        .await
        .unwrap();
    assert_eq!(again.released, 0);
}

#[tokio::test]
async fn releases_legacy_retry_rows_whose_reason_is_systemic() {
    let app = TestApp::new().await;
    let store = &app.ctx.store;
    retry::enqueue(
        store,
        "CalendarEvents",
        "legacy",
        "Calendar extraction failed: provider error 400: Invalid schema for response_format",
    )
    .await
    .unwrap();
    let report = systemic::release_for_build(store, "build-2", 20)
        .await
        .unwrap();
    assert_eq!(report.released, 1);
    let released = row(store, "CalendarEvents", "legacy").await.unwrap();
    assert_eq!(
        retry::select_due(std::slice::from_ref(&released), now(&app)).len(),
        1
    );
}

fn task(store: &Store, handler: Arc<dyn omni_core::email::EmailHandler>) -> EmailRetryTask {
    let ports = Ports::default();
    ports
        .set_email_reader(FakeReader::new(|id| Ok(Some(common::email(id)))))
        .ok()
        .unwrap();
    ports
        .set_email_retry_handlers(handlers(vec![("CalendarEvents", handler)]))
        .ok()
        .unwrap();
    let schedule = CronSchedule::parse(SCHEDULE, &jiff::tz::TimeZone::UTC).unwrap();
    EmailRetryTask::new(store.clone(), ports, schedule)
}

#[tokio::test]
async fn the_retry_task_replays_a_released_email_and_marks_it_replayed() {
    let app = TestApp::new().await;
    let store = app.ctx.store.clone();
    record_error(&store, "bcs1882", BCS_DETAIL).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let handler = {
        let (store, calls) = (store.clone(), calls.clone());
        fn_handler("CalendarEvents", move |emails| {
            let (store, calls) = (store.clone(), calls.clone());
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                // The fixed build: triage now filters the email.
                activity::record(
                    &store,
                    NewActivity {
                        detail: Some("triage: not an event".to_owned()),
                        ..NewActivity::new(
                            EmailPipelineName::CalendarEvents,
                            &emails[0],
                            EmailActivityOutcome::Filtered,
                        )
                    },
                )
                .await
                .map(|_| ())
                .map_err(|e| HandlerError::transient(e.to_string(), None))
            }
        })
    };
    let task = task(&store, handler);

    // Nothing is due before a new build releases the email.
    park(&store, "CalendarEvents", "parked", "build-2", now(&app)).await;
    systemic::release_for_build(&store, "build-2", 20)
        .await
        .unwrap();
    task.run_pass().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(row(&store, "CalendarEvents", "bcs1882").await.is_none());
    assert!(row(&store, "CalendarEvents", "parked").await.is_some());

    let activity_row: EmailActivityData = activity::get(&store, "CalendarEvents#bcs1882")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(activity_row.outcome, EmailActivityOutcome::Filtered);
    assert_eq!(
        activity_row.detail.as_deref(),
        Some("triage: not an event (replayed after fix)")
    );
}

#[tokio::test]
async fn a_replay_that_fails_systemically_again_waits_for_the_next_build() {
    let app = TestApp::new().await;
    let store = app.ctx.store.clone();
    park(&store, "CalendarEvents", "e1", "build-1", now(&app)).await;
    systemic::release_for_build(&store, "build-2", 20)
        .await
        .unwrap();
    let handler = {
        let reporter = reporter(&app);
        fn_handler("CalendarEvents", move |_| {
            let reporter = reporter.clone();
            async move {
                reporter
                    .report("CalendarEvents", "e1", BCS_DETAIL, &signature())
                    .await
                    .map_err(|e| HandlerError::transient(e.to_string(), None))
            }
        })
    };
    task(&store, handler).run_pass().await.unwrap();
    let parked = row(&store, "CalendarEvents", "e1").await.unwrap();
    assert_eq!(parked.attempts, 1);
    assert!(parked.attempts < MAX_RETRY_ATTEMPTS);
    assert_eq!(parked.awaiting_build, Some(systemic::current_build().await));
}

#[tokio::test]
async fn stale_rows_parked_under_the_running_build_expire_too() {
    let app = TestApp::new().await;
    let store = &app.ctx.store;
    park(
        store,
        "CalendarEvents",
        "old",
        "build-1",
        now(&app) - REPLAY_LOOKBACK_MS - 1,
    )
    .await;
    park(store, "CalendarEvents", "fresh", "build-1", now(&app)).await;
    let report = systemic::release_for_build(store, "build-1", 20)
        .await
        .unwrap();
    assert_eq!((report.released, report.expired), (0, 1));
    assert!(row(store, "CalendarEvents", "old").await.is_none());
    assert!(row(store, "CalendarEvents", "fresh").await.is_some());
}
