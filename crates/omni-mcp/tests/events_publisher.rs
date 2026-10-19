//! The `EventPublisher` port over the outbox and the `task.run_finished`
//! watcher: catalog validation and size bounds before anything is stored,
//! per-subscription matching, replay dedup with stable event IDs, the owner
//! check at delivery (withheld while the owner is revoked), and the run scan's
//! window and filters.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use common::{EPOCH_MS, FakeOwners, clock, test_store, with_owners};
use futures::future::BoxFuture;
use omni_api::events::{
    CALENDAR_EVENT_CHANGED, CALENDAR_EVENT_STARTING, CalendarChangeKind, CalendarChangeOrigin,
    CalendarEventChanged, CalendarEventStarting, CalendarStartTrigger, LIVESTREAM_STATUS_CHANGED,
    LivestreamStatusChanged, LivestreamTransition, PRESSPODS_JOB_FINISHED, PresspodsJobFinished,
    PresspodsOutcome, TASK_RUN_FINISHED, TaskRunFinished, TaskRunOutcome,
};
use omni_core::clock::{Clock as _, TestClock};
use omni_mcp::events::catalog::event_definition;
use omni_mcp::events::persistence::{
    DeliveryStatus, DeliveryWithhold, EventDelivery, EventReceipt,
};
use omni_mcp::events::publisher::{McpEventPublisher, validate_publication};
use omni_mcp::events::service::{McpEventService, SubscribeInput};
use omni_mcp::events::task_runs::{LOOKBACK_MS, TaskRunWatcher};
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_runtime::ports::{EventPublication, PortError, Ports};
use omni_store::Store;
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps, EntityWrite, UpsertOpts};
use omni_tasks::{TaskRunData, TaskRunStatus, Trigger};
use serde_json::{Map, Value, json};

const URL: &str = "https://chatgpt.example.com/events/callback";

struct Webhook(Arc<Mutex<Vec<WebhookEvent>>>);

impl WebhookPort for Webhook {
    fn verify<'a>(&'a self, _: &'a WebhookDestination) -> BoxFuture<'a, Result<(), WebhookError>> {
        Box::pin(async { Ok(()) })
    }

    fn deliver<'a>(
        &'a self,
        _: &'a WebhookDestination,
        event: &'a WebhookEvent,
    ) -> BoxFuture<'a, Result<u16, WebhookError>> {
        self.0.lock().unwrap().push(event.clone());
        Box::pin(async { Ok(204) })
    }
}

/// The gateway principal every subscription here belongs to.
const OWNER: &str = "gateway:client:publisher-test";

struct Harness {
    events: McpEventService,
    publisher: McpEventPublisher,
    sent: Arc<Mutex<Vec<WebhookEvent>>>,
    owners: Arc<FakeOwners>,
}

fn harness(store: &Store, clock: &Arc<TestClock>) -> Harness {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let owners = Arc::new(FakeOwners::default());
    let events = with_owners(
        McpEventService::new(
            "test-omni-bearer",
            store.clone(),
            clock.clone(),
            Arc::new(Webhook(sent.clone())),
            Ports::default(),
        ),
        owners.clone(),
    );
    Harness {
        publisher: McpEventPublisher::new(events.clone()),
        events,
        sent,
        owners,
    }
}

fn secret() -> String {
    format!(
        "whsec_{}",
        base64::engine::general_purpose::STANDARD.encode([7u8; 32])
    )
}

async fn subscribe(h: &Harness, name: &str, arguments: Value, url: &str) {
    h.events
        .subscribe_as(
            OWNER,
            &SubscribeInput {
                name: name.to_owned(),
                arguments,
                url: url.to_owned(),
                secret: secret(),
                ttl_ms: None,
            },
        )
        .await
        .unwrap();
}

fn object(value: impl serde::Serialize) -> Map<String, Value> {
    match serde_json::to_value(value).unwrap() {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

fn live_event(streamer: &str, tier: omni_api::streamers::StreamerTier) -> EventPublication {
    EventPublication {
        name: LIVESTREAM_STATUS_CHANGED,
        dedup_key: format!("{streamer}:1:went_live"),
        occurred_at_ms: EPOCH_MS,
        data: object(LivestreamStatusChanged {
            streamer_id: streamer.to_owned(),
            display_name: "Name".to_owned(),
            transition: LivestreamTransition::WentLive,
            tier,
            platform: "twitch".to_owned(),
            title: Some("Title".to_owned()),
            started_at: "2026-10-09T12:00:00.000Z".to_owned(),
            ended_at: None,
            viewer_count: Some(3),
            max_viewer_count: Some(3),
        }),
    }
}

async fn deliveries(store: &Store) -> Vec<EventDelivery> {
    store
        .read(|docs| docs.get_all::<EventDelivery>())
        .await
        .unwrap()
}

#[test]
fn every_payload_dto_matches_its_catalog_schema() {
    let samples = [
        live_event("a", omni_api::streamers::StreamerTier::Primary),
        EventPublication {
            name: PRESSPODS_JOB_FINISHED,
            dedup_key: "p".to_owned(),
            occurred_at_ms: 0,
            data: object(PresspodsJobFinished {
                outcome: PresspodsOutcome::Published,
                episode_id: Some("e".to_owned()),
                job_id: Some("j".to_owned()),
                title: Some("T".to_owned()),
                article_url_host: Some("example.com".to_owned()),
                duration_seconds: Some(61.5),
                attempts: None,
            }),
        },
        EventPublication {
            name: TASK_RUN_FINISHED,
            dedup_key: "r".to_owned(),
            occurred_at_ms: 0,
            data: object(TaskRunFinished {
                run_id: "PressPods:1".to_owned(),
                task_name: "PressPods".to_owned(),
                trigger: omni_api::runs::RunTrigger::Schedule,
                status: TaskRunOutcome::Degraded,
                started_at: "2026-10-09T12:00:00.000Z".to_owned(),
                finished_at: "2026-10-09T12:00:01.000Z".to_owned(),
            }),
        },
        EventPublication {
            name: CALENDAR_EVENT_CHANGED,
            dedup_key: "c".to_owned(),
            occurred_at_ms: 0,
            data: object(CalendarEventChanged {
                event_id: "ABC-1.ics".to_owned(),
                uid: Some("abc-1".to_owned()),
                change_kind: CalendarChangeKind::Updated,
                summary: Some("Dentist".to_owned()),
                summary_truncated: false,
                start: Some("2026-10-08T21:00:00Z".to_owned()),
                all_day: false,
                recurring: false,
                changed_fields: vec!["start".to_owned(), "other".to_owned()],
                version: Some("\"etag-1\"".to_owned()),
                origin: CalendarChangeOrigin::External,
                detected_at: "2026-10-09T12:00:00.000Z".to_owned(),
            }),
        },
        EventPublication {
            name: CALENDAR_EVENT_STARTING,
            dedup_key: "s".to_owned(),
            occurred_at_ms: 0,
            data: object(CalendarEventStarting {
                event_id: "ABC-1.ics".to_owned(),
                uid: "abc-1".to_owned(),
                recurrence_id: Some("2026-10-19T09:00:00[America/Vancouver]".to_owned()),
                summary: None,
                summary_truncated: false,
                start: "2026-10-19T16:00:00Z".to_owned(),
                end: "2026-10-19T16:30:00Z".to_owned(),
                all_day: false,
                time_zone: Some("America/Vancouver".to_owned()),
                trigger: CalendarStartTrigger::Alarm,
                lead_minutes: None,
                alarm_id: Some("alarm-1".to_owned()),
                fire_at: "2026-10-19T15:30:00Z".to_owned(),
                late: false,
                has_location: false,
                include_all_day: "false".to_owned(),
            }),
        },
    ];
    for sample in &samples {
        assert!(
            validate_publication(sample).is_ok(),
            "{} sample does not validate",
            sample.name
        );
        assert!(event_definition(sample.name).is_some());
    }
}

#[test]
fn rejects_unknown_events_schema_mismatches_and_oversized_payloads() {
    let mut unknown = live_event("a", omni_api::streamers::StreamerTier::Primary);
    unknown.name = "calendar.nope";
    let mut extra = live_event("a", omni_api::streamers::StreamerTier::Primary);
    extra.data.insert("notes".to_owned(), json!("private"));
    let mut huge = live_event("a", omni_api::streamers::StreamerTier::Primary);
    huge.data
        .insert("streamerId".to_owned(), json!("x".repeat(5000)));
    let mut keyless = live_event("a", omni_api::streamers::StreamerTier::Primary);
    keyless.dedup_key.clear();
    for event in [unknown, extra, huge, keyless] {
        assert!(matches!(
            validate_publication(&event),
            Err(PortError::Failed {
                transient: false,
                ..
            })
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn publishes_to_matching_subscriptions_once_per_dedup_key() {
    let clock = clock();
    let db = test_store(&clock).await;
    let h = harness(&db.store, &clock);
    subscribe(&h, LIVESTREAM_STATUS_CHANGED, json!({}), URL).await;
    subscribe(
        &h,
        LIVESTREAM_STATUS_CHANGED,
        json!({"streamer": "other"}),
        "https://chatgpt.example.com/events/other",
    )
    .await;

    let event = live_event("alpha", omni_api::streamers::StreamerTier::Primary);
    assert!(h.publisher.publish(&event).await.unwrap());
    // A replay is dropped by its receipt, through the trait object too.
    let port: Arc<dyn omni_runtime::ports::EventPublisher> = Arc::new(h.publisher.clone());
    assert!(!port.publish(&event).await.unwrap());
    // Background-tier streamers need includeBackground.
    let background = live_event("beta", omni_api::streamers::StreamerTier::Background);
    assert!(!h.publisher.publish(&background).await.unwrap());

    let queued = deliveries(&db.store).await;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].name, LIVESTREAM_STATUS_CHANGED);
    assert_eq!(queued[0].timestamp, omni_core::js::to_iso_string(EPOCH_MS));
    let receipts = db
        .store
        .read(|docs| docs.get_all::<EventReceipt>())
        .await
        .unwrap();
    assert_eq!(receipts.len(), 2);
    assert!(
        receipts
            .iter()
            .all(|r| r.message_key.len() == 64 && !r.message_key.contains("alpha"))
    );

    assert_eq!(h.events.drain().await.unwrap(), 1);
    let sent = h.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].data["streamerId"], "alpha");
    let expected_id = format!(
        "evt_{}",
        &omni_core::digest::sha256_hex(format!("{LIVESTREAM_STATUS_CHANGED}:alpha:1:went_live"))
            [..40]
    );
    assert_eq!(sent[0].event_id, expected_id);

    let arguments = port
        .active_arguments(LIVESTREAM_STATUS_CHANGED)
        .await
        .unwrap();
    assert_eq!(arguments.len(), 2);
    assert!(arguments.iter().all(|a| a["transition"] == "any"));
    assert!(
        port.active_arguments(PRESSPODS_JOB_FINISHED)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn port_events_wait_withheld_while_the_owner_is_revoked() {
    let clock = clock();
    let db = test_store(&clock).await;
    let h = harness(&db.store, &clock);
    let input = SubscribeInput {
        name: PRESSPODS_JOB_FINISHED.to_owned(),
        arguments: json!({"outcome": "failed"}),
        url: URL.to_owned(),
        secret: secret(),
        ttl_ms: None,
    };
    h.events.subscribe_as(OWNER, &input).await.unwrap();
    let failed = EventPublication {
        name: PRESSPODS_JOB_FINISHED,
        dedup_key: "failed:job:3".to_owned(),
        occurred_at_ms: EPOCH_MS,
        data: object(PresspodsJobFinished {
            outcome: PresspodsOutcome::Failed,
            episode_id: None,
            job_id: Some("job".to_owned()),
            title: None,
            article_url_host: Some("example.com".to_owned()),
            duration_seconds: None,
            attempts: Some(3),
        }),
    };
    h.owners.revoked.lock().unwrap().insert(OWNER.to_owned());
    assert!(h.publisher.publish(&failed).await.unwrap());
    h.events.drain().await.unwrap();
    let held = deliveries(&db.store).await;
    assert_eq!(held[0].status, DeliveryStatus::Pending);
    assert_eq!(
        held[0].withheld,
        Some(DeliveryWithhold::AuthorizationInvalid)
    );
    assert!(h.sent.lock().unwrap().is_empty());

    h.owners.revoked.lock().unwrap().clear();
    tokio::time::advance(Duration::from_secs(15 * 60)).await;
    h.events.drain().await.unwrap();
    assert_eq!(h.sent.lock().unwrap().len(), 1);
    assert_eq!(
        deliveries(&db.store).await[0].status,
        DeliveryStatus::Delivered
    );
}

fn run(id: &str, task: &str, status: TaskRunStatus, finished_at: Option<i64>) -> TaskRunData {
    TaskRunData {
        run_id: format!("{task}:{id}"),
        task_name: task.to_owned(),
        trigger: Trigger::Schedule,
        scheduled_for: None,
        started_at: finished_at.unwrap_or(EPOCH_MS) - 1_000,
        finished_at,
        status,
        error: Some("private error text".to_owned()),
        summary: None,
        extra: Extra::default(),
    }
}

async fn put_runs(store: &Store, runs: Vec<TaskRunData>) {
    store
        .write(move |tx| {
            for run in &runs {
                tx.upsert(run, UpsertOpts::default())?;
            }
            Ok::<(), omni_store::StoreError>(())
        })
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn task_run_watcher_publishes_recent_matching_runs_once() {
    let clock = clock();
    let db = test_store(&clock).await;
    let h = harness(&db.store, &clock);
    let watcher = TaskRunWatcher::new(h.events.clone(), db.store.clone(), clock.clone());
    let now = clock.now_ms();
    put_runs(
        &db.store,
        vec![
            run("1", "PressPods", TaskRunStatus::Error, Some(now - 1_000)),
            run("2", "PressPods", TaskRunStatus::Success, Some(now - 1_000)),
            run(
                "3",
                "LiveCheckTask",
                TaskRunStatus::Error,
                Some(now - 2_000),
            ),
            run("4", "PressPods", TaskRunStatus::Running, None),
            run(
                "old",
                "PressPods",
                TaskRunStatus::Error,
                Some(now - LOOKBACK_MS - 1),
            ),
        ],
    )
    .await;

    // Nothing is read or published without a subscription.
    assert_eq!(watcher.poll().await.unwrap(), 0);
    assert!(
        db.store
            .read(|docs| docs.get_all::<EventReceipt>())
            .await
            .unwrap()
            .is_empty()
    );

    subscribe(&h, TASK_RUN_FINISHED, json!({"task": "PressPods"}), URL).await;
    assert_eq!(watcher.poll().await.unwrap(), 1);
    // A rescan inside the window is a no-op.
    assert_eq!(watcher.poll().await.unwrap(), 0);
    let queued = deliveries(&db.store).await;
    assert_eq!(queued.len(), 1);
    let data = Value::Object(queued[0].data_json());
    assert_eq!(data["runId"], "PressPods:1");
    assert_eq!(data["status"], "error");
    assert_eq!(data["trigger"], "schedule");
    assert!(!data.to_string().contains("private error text"));
    // Unmatched runs leave no receipts.
    let receipts = db
        .store
        .read(|docs| docs.get_all::<EventReceipt>())
        .await
        .unwrap();
    assert_eq!(receipts.len(), 1);

    // A run that finishes later is picked up by the next pass.
    tokio::time::advance(Duration::from_secs(30)).await;
    put_runs(
        &db.store,
        vec![run(
            "5",
            "PressPods",
            TaskRunStatus::Error,
            Some(clock.now_ms()),
        )],
    )
    .await;
    assert_eq!(watcher.poll().await.unwrap(), 1);
}

#[tokio::test]
async fn a_backlog_beyond_one_pass_drains_without_waiting_for_the_sweep() {
    let clock = clock();
    let db = test_store(&clock).await;
    let h = harness(&db.store, &clock);
    subscribe(&h, LIVESTREAM_STATUS_CHANGED, json!({}), URL).await;
    for index in 0..12 {
        let event = live_event(
            &format!("streamer{index}"),
            omni_api::streamers::StreamerTier::Primary,
        );
        assert!(h.publisher.publish(&event).await.unwrap());
    }
    let shutdown = tokio_util::sync::CancellationToken::new();
    let worker = {
        let events = h.events.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { events.delivery_worker(shutdown).await })
    };
    // Well short of the 30-second sweep that would otherwise pick up the rest
    // (store I/O runs on blocking threads, so this waits in real time).
    for _ in 0..200 {
        if h.sent.lock().unwrap().len() == 12 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    shutdown.cancel();
    worker.await.unwrap();
    assert_eq!(h.sent.lock().unwrap().len(), 12);
}
