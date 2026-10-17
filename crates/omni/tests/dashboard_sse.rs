//! `/api/events` dashboard SSE: fresh initial snapshot, 150 ms debounce,
//! identical-payload skip, 25 s ping, monotonically increasing ids, and
//! updates on streamer changes (not only task runs).
//! A writer failure releasing its parent scope is not tested: axum owns the SSE
//! body, and a dropped client drops the stream future.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::{SseReader, get, ops};
use omni_store::EntityWrite as _;
use omni_store::cbor::Extra;
use omni_store::entity::UpsertOpts;
use omni_tasks::persistence::{TaskRunData, TaskRunStatus};
use omni_tasks::{AppEvent, TaskRunEvent, TaskRunEventKind, Trigger};
use omni_testkit::TestApp;
use serde_json::Value;

const WAIT: Duration = Duration::from_secs(3);
const QUIET: Duration = Duration::from_millis(400);

async fn add_run(app: &TestApp, id: &str) {
    let run = TaskRunData {
        run_id: format!("T:{id}"),
        task_name: "T".to_owned(),
        trigger: Trigger::Schedule,
        scheduled_for: None,
        started_at: 1_000,
        finished_at: Some(2_000),
        status: TaskRunStatus::Success,
        error: None,
        summary: None,
        extra: Extra::new(),
    };
    app.ctx
        .store
        .write(move |tx| tx.upsert(&run, UpsertOpts::default()))
        .await
        .unwrap();
}

fn task_event(app: &TestApp) {
    app.ctx.bus.emit_task_run(TaskRunEvent {
        kind: TaskRunEventKind::RunFinished,
        task_name: "T".to_owned(),
    });
}

fn runs(data: &str) -> usize {
    let snapshot: Value = serde_json::from_str(data).unwrap();
    snapshot["runs"].as_array().unwrap().len()
}

/// Connects and consumes the immediate `ping` that precedes the snapshot.
async fn connect(router: &axum::Router) -> SseReader {
    let (status, body) = get(router, "/api/events").await;
    assert_eq!(status, StatusCode::OK);
    let mut reader = SseReader::new(body);
    let ping = reader.next(WAIT).await.unwrap();
    assert_eq!((ping.event.as_str(), ping.id.as_deref()), ("ping", None));
    reader
}

#[tokio::test]
async fn pings_on_connect_then_sends_the_snapshot_before_any_broadcast() {
    let app = TestApp::new().await;
    let (router, state) = ops(&app.ctx);
    let hub = tokio::spawn(state.dashboard.clone().listen());
    let before = app.ctx.clock.now_ms();
    let (status, body) = get(&router, "/api/events").await;
    assert_eq!(status, StatusCode::OK);
    let mut reader = SseReader::new(body);
    let ping = reader.next(WAIT).await.unwrap();
    assert_eq!(ping.event, "ping");
    let pinged_at: i64 = ping.data.parse().unwrap();
    assert!((before..=app.ctx.clock.now_ms()).contains(&pinged_at));
    let snapshot = reader.next(WAIT).await.unwrap();
    assert_eq!(
        (snapshot.event.as_str(), snapshot.id.as_deref()),
        ("snapshot", Some("0"))
    );
    add_run(&app, "1").await;
    task_event(&app);
    let update = reader.next(WAIT).await.unwrap();
    assert_eq!(
        (update.event.as_str(), update.id.as_deref()),
        ("snapshot", Some("1"))
    );
    app.ctx.shutdown.cancel();
    hub.await.unwrap();
}

#[tokio::test]
async fn sends_a_fresh_initial_snapshot_with_increasing_ids() {
    let app = TestApp::new().await;
    let (router, _state) = ops(&app.ctx);
    add_run(&app, "1").await;
    let mut first = connect(&router).await;
    let frame = first.next(WAIT).await.unwrap();
    assert_eq!(frame.event, "snapshot");
    assert_eq!(frame.id.as_deref(), Some("0"));
    let snapshot: Value = serde_json::from_str(&frame.data).unwrap();
    assert_eq!(
        snapshot.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["tasks", "streamers", "runs", "onDeck", "build"]
    );
    assert_eq!(runs(&frame.data), 1);
    assert_eq!(snapshot["runs"][0]["scheduledFor"], Value::Null);

    // A later client builds its own snapshot (never a replay) with the next id.
    add_run(&app, "2").await;
    let mut second = connect(&router).await;
    let frame = second.next(WAIT).await.unwrap();
    assert_eq!(frame.id.as_deref(), Some("1"));
    assert_eq!(runs(&frame.data), 2);
}

#[tokio::test]
async fn debounces_bursts_and_skips_identical_payloads() {
    let app = TestApp::new().await;
    let (router, state) = ops(&app.ctx);
    let hub = tokio::spawn(state.dashboard.clone().listen());
    let mut a = connect(&router).await;
    let mut b = connect(&router).await;
    assert_eq!(a.next(WAIT).await.unwrap().id.as_deref(), Some("0"));
    assert_eq!(b.next(WAIT).await.unwrap().id.as_deref(), Some("1"));

    // A burst of changes becomes one broadcast, at least 150 ms later.
    add_run(&app, "1").await;
    let started = tokio::time::Instant::now();
    for _ in 0..5 {
        task_event(&app);
    }
    let frame = a.next(WAIT).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(150));
    assert_eq!(
        (frame.event.as_str(), frame.id.as_deref()),
        ("snapshot", Some("2"))
    );
    assert_eq!(runs(&frame.data), 1);
    let same = b.next(WAIT).await.unwrap();
    assert_eq!(same.id.as_deref(), Some("2"));
    assert_eq!(same.data, frame.data);
    assert!(a.next(QUIET).await.is_none(), "one frame per burst");

    // Nothing changed: the identical payload is not sent again.
    tokio::time::sleep(Duration::from_millis(200)).await;
    task_event(&app);
    assert!(a.next(QUIET).await.is_none());

    // Streamer updates rebroadcast too (live viewer counts).
    add_run(&app, "2").await;
    app.ctx.bus.emit_app(AppEvent::StreamersChanged);
    let frame = a.next(WAIT).await.unwrap();
    assert_eq!(frame.id.as_deref(), Some("3"));
    assert_eq!(runs(&frame.data), 2);
    app.ctx.shutdown.cancel();
    hub.await.unwrap();
}

#[tokio::test]
async fn pings_every_25_seconds_and_ends_at_shutdown() {
    let app = TestApp::new().await;
    let (router, _state) = ops(&app.ctx);
    let mut reader = connect(&router).await;
    assert_eq!(reader.next(WAIT).await.unwrap().event, "snapshot");
    tokio::time::pause();
    let before = tokio::time::Instant::now();
    let ping = reader.next(Duration::from_secs(60)).await.unwrap();
    assert_eq!(ping.event, "ping");
    assert_eq!(ping.id, None);
    assert_eq!(ping.data, app.ctx.clock.now_ms().to_string());
    assert!(before.elapsed() >= Duration::from_secs(25) - Duration::from_millis(50));
    tokio::time::resume();
    app.ctx.shutdown.cancel();
    assert!(reader.ended(WAIT).await);
}
