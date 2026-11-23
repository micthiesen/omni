//! The durable email retry queue and its task.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::{Arc, Mutex};

use omni_core::email::{EmailHandler, HandlerError};
use omni_email::retry::{self, EmailRetryData};
use omni_email::retry_task::{EmailRetryTask, SCHEDULE};
use omni_runtime::Ports;
use omni_runtime::ports::PortError;
use omni_store::Store;
use omni_store::cbor::Extra;
use omni_store::entity::{EntityWrite as _, UpsertOpts};
use omni_tasks::CronSchedule;

use common::{FakeReader, NOW, email, fn_handler, handlers, store_at};

async fn due_row(store: &Store, pipeline: &str, email_id: &str, attempts: i64) {
    let row = EmailRetryData {
        retry_key: retry::retry_key(pipeline, email_id),
        pipeline: pipeline.to_owned(),
        email_id: email_id.to_owned(),
        reason: "test".to_owned(),
        enqueue_count: None,
        attempts,
        next_attempt_at: NOW - 1000,
        created_at: NOW - 60_000,
        awaiting_build: None,
        signature: None,
        extra: Extra::new(),
    };
    store
        .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
        .await
        .unwrap();
}

fn task(store: &Store, ports: Ports) -> EmailRetryTask {
    let schedule = CronSchedule::parse(SCHEDULE, &jiff::tz::TimeZone::UTC).unwrap();
    EmailRetryTask::new(store.clone(), ports, schedule)
}

fn ports_with(reader: Arc<FakeReader>, handler: Arc<dyn EmailHandler>) -> Ports {
    let ports = Ports::default();
    ports.set_email_reader(reader).ok().unwrap();
    ports
        .set_email_retry_handlers(handlers(vec![("ParcelTracker", handler)]))
        .ok()
        .unwrap();
    ports
}

fn ok_reader() -> Arc<FakeReader> {
    FakeReader::new(|id| Ok(Some(common::email(id))))
}

#[tokio::test]
async fn clears_the_row_when_the_handler_succeeds_without_re_enqueueing() {
    let (store, _clock) = store_at(NOW).await;
    due_row(&store.store, "ParcelTracker", "e1", 1).await;
    let handler = fn_handler("ParcelTracker", |_| async { Ok::<(), HandlerError>(()) });
    task(&store.store, ports_with(ok_reader(), handler))
        .run_pass()
        .await
        .unwrap();
    assert!(
        retry::get(&store.store, "ParcelTracker#e1")
            .await
            .unwrap()
            .is_none()
    );
}

fn re_enqueueing_handler(store: &Store) -> Arc<dyn EmailHandler> {
    let store = store.clone();
    fn_handler("ParcelTracker", move |_| {
        let store = store.clone();
        Box::pin(async move {
            retry::enqueue(&store, "ParcelTracker", "e1", "still down")
                .await
                .map(|_| ())
                .map_err(|e| HandlerError::transient(e.to_string(), None))
        })
    })
}

#[tokio::test]
async fn keeps_the_row_when_the_handler_re_enqueues_without_throwing() {
    let (store, _clock) = store_at(NOW).await;
    due_row(&store.store, "ParcelTracker", "e1", 1).await;
    let handler = re_enqueueing_handler(&store.store);
    task(&store.store, ports_with(ok_reader(), handler))
        .run_pass()
        .await
        .unwrap();
    let row = retry::get(&store.store, "ParcelTracker#e1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.attempts, 2);
}

#[tokio::test]
async fn drops_the_row_once_re_enqueueing_exceeds_the_attempt_cap() {
    let (store, _clock) = store_at(NOW).await;
    due_row(&store.store, "ParcelTracker", "e1", 5).await;
    let handler = re_enqueueing_handler(&store.store);
    task(&store.store, ports_with(ok_reader(), handler))
        .run_pass()
        .await
        .unwrap();
    assert!(
        retry::get(&store.store, "ParcelTracker#e1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn drops_a_permanent_handler_failure_instead_of_retrying_it() {
    let (store, _clock) = store_at(NOW).await;
    due_row(&store.store, "ParcelTracker", "e1", 1).await;
    let handler = fn_handler("ParcelTracker", |_| {
        Box::pin(async { Err::<(), _>(HandlerError::permanent("boom", None)) })
    });
    task(&store.store, ports_with(ok_reader(), handler))
        .run_pass()
        .await
        .unwrap();
    assert!(
        retry::get(&store.store, "ParcelTracker#e1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn continues_the_pass_when_fetching_one_due_email_fails() {
    let (store, _clock) = store_at(NOW).await;
    due_row(&store.store, "ParcelTracker", "e1", 0).await;
    due_row(&store.store, "ParcelTracker", "e2", 0).await;
    let handled = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = handled.clone();
    let reader = FakeReader::new(|id| {
        if id == "e1" {
            Err(PortError::Failed {
                message: "temporary transport failure".to_owned(),
                transient: true,
            })
        } else {
            Ok(Some(email(id)))
        }
    });
    let handler = fn_handler("ParcelTracker", move |emails| {
        seen.lock()
            .unwrap()
            .extend(emails.into_iter().map(|e| e.id));
        Box::pin(async { Ok::<(), HandlerError>(()) })
    });
    task(&store.store, ports_with(reader, handler))
        .run_pass()
        .await
        .unwrap();
    assert_eq!(*handled.lock().unwrap(), ["e2"]);
    assert_eq!(
        retry::get(&store.store, "ParcelTracker#e1")
            .await
            .unwrap()
            .unwrap()
            .attempts,
        1
    );
    assert!(
        retry::get(&store.store, "ParcelTracker#e2")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn defers_while_the_pipelines_are_not_connected() {
    let (store, _clock) = store_at(NOW).await;
    due_row(&store.store, "ParcelTracker", "e1", 1).await;
    let task = task(&store.store, Ports::default());
    task.run_pass().await.unwrap();
    assert_eq!(
        omni_tasks::Task::last_run_summary(&task).as_deref(),
        Some("1 due, pipelines not connected yet")
    );
    assert_eq!(
        retry::get(&store.store, "ParcelTracker#e1")
            .await
            .unwrap()
            .unwrap()
            .attempts,
        1
    );
}

#[tokio::test(start_paused = true)]
async fn uses_the_paused_clock_and_decodes_persisted_rows() {
    let (store, _clock) = store_at(1_800_000_000_000).await;
    retry::enqueue(
        &store.store,
        "ParcelTracker",
        "email-1",
        "service unavailable",
    )
    .await
    .unwrap();
    let rows = retry::get_all(&store.store).await.unwrap();
    let row = rows.first().unwrap();
    assert_eq!(row.retry_key, "ParcelTracker#email-1");
    assert_eq!(row.attempts, 0);
    assert_eq!(row.created_at, 1_800_000_000_000);
    assert_eq!(row.next_attempt_at, 1_800_001_800_000);
}
