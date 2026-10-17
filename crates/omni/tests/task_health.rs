//! `TaskHealth`: one Pushover note per persistent incident, one recovery note,
//! and a retry when delivery of the incident note fails.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use omni::task_health::{self, HealthNote};
use omni_store::entity::EntityOps as _;
use omni_tasks::health::TaskHealthIncident;
use omni_tasks::persistence::{self, RunEnd};
use omni_tasks::{TaskRunEvent, TaskRunEventKind, TaskRunStatus, Trigger};
use omni_testkit::TestApp;

const HOUR: i64 = 60 * 60_000;
const TASK: &str = "TasteReflection";

async fn settled(app: &TestApp, started_at: i64, status: TaskRunStatus) {
    let run = persistence::record_run_start(
        &app.ctx.store,
        TASK,
        Trigger::Schedule,
        None,
        None,
        started_at,
    )
    .await
    .unwrap();
    let error = match status {
        TaskRunStatus::Degraded => Some("watch history unavailable: Plex offline".to_owned()),
        TaskRunStatus::Error => Some("boom".to_owned()),
        _ => None,
    };
    persistence::record_run_end(
        &app.ctx.store,
        &run.run_id,
        RunEnd {
            status,
            error,
            summary: None,
            finished_at: started_at + 1000,
        },
    )
    .await
    .unwrap();
}

async fn incident(app: &TestApp) -> Option<TaskHealthIncident> {
    app.ctx
        .store
        .read(|docs| docs.get::<TaskHealthIncident>(&TASK.to_owned()))
        .await
        .unwrap()
}

fn titles(app: &TestApp) -> Vec<String> {
    app.pushes
        .all()
        .into_iter()
        .filter_map(|push| push.message.title)
        .collect()
}

#[tokio::test]
async fn notifies_once_per_incident_and_once_on_recovery() {
    let app = TestApp::new().await;
    let tz = jiff::tz::TimeZone::UTC;
    settled(&app, HOUR, TaskRunStatus::Degraded).await;
    settled(&app, 7 * HOUR, TaskRunStatus::Degraded).await;
    task_health::check(&app.ctx, &tz, TASK).await;
    assert!(app.pushes.all().is_empty());

    settled(&app, 13 * HOUR, TaskRunStatus::Error).await;
    task_health::check(&app.ctx, &tz, TASK).await;
    assert_eq!(titles(&app), vec![format!("{TASK} is unhealthy")]);
    let push = app.pushes.all().remove(0).message;
    assert!(
        push.message
            .starts_with("3 consecutive failed or degraded runs since")
    );
    let stored = incident(&app).await.unwrap();
    assert_eq!(stored.first_bad_at, HOUR);

    settled(&app, 19 * HOUR, TaskRunStatus::Degraded).await;
    task_health::check(&app.ctx, &tz, TASK).await;
    assert_eq!(app.pushes.all().len(), 1);

    settled(&app, 25 * HOUR, TaskRunStatus::Success).await;
    task_health::check(&app.ctx, &tz, TASK).await;
    task_health::check(&app.ctx, &tz, TASK).await;
    assert_eq!(
        titles(&app),
        vec![format!("{TASK} is unhealthy"), format!("{TASK} recovered")]
    );
    assert!(incident(&app).await.is_none());
}

#[tokio::test]
async fn a_released_reservation_is_retried_after_the_next_run() {
    let app = TestApp::new().await;
    for hour in [0, 6, 12] {
        settled(&app, hour * HOUR, TaskRunStatus::Degraded).await;
    }
    let note = task_health::evaluate(&app.ctx.store, TASK, 20 * HOUR)
        .await
        .unwrap();
    let Some(HealthNote::Unhealthy(reserved)) = note else {
        panic!("expected an incident, got {note:?}");
    };
    // Reserved: a second evaluation does not notify again.
    assert_eq!(
        task_health::evaluate(&app.ctx.store, TASK, 20 * HOUR)
            .await
            .unwrap(),
        None
    );
    task_health::release(&app.ctx.store, &reserved)
        .await
        .unwrap();
    assert!(matches!(
        task_health::evaluate(&app.ctx.store, TASK, 21 * HOUR)
            .await
            .unwrap(),
        Some(HealthNote::Unhealthy(_))
    ));
}

#[tokio::test]
async fn the_service_reacts_to_finished_runs() {
    let app = TestApp::new().await;
    for hour in [0, 6, 12] {
        settled(&app, hour * HOUR, TaskRunStatus::Degraded).await;
    }
    let service = task_health::service();
    let running = tokio::spawn((service.start)(app.ctx.clone()));
    let mut pushed = false;
    for _ in 0..100 {
        app.ctx.bus.emit_task_run(TaskRunEvent {
            kind: TaskRunEventKind::RunFinished,
            task_name: TASK.to_owned(),
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        if !app.pushes.all().is_empty() {
            pushed = true;
            break;
        }
    }
    app.ctx.shutdown.cancel();
    running.await.unwrap();
    assert!(pushed);
    assert_eq!(app.pushes.all().len(), 1);
}

#[test]
fn only_definite_rejections_and_unsent_notes_release_the_reservation() {
    use omni_alerts::{PushOutcome, PushoverError};
    let error = |status: Option<u16>| -> Result<PushOutcome, PushoverError> {
        Err(PushoverError {
            status,
            body: "x".to_owned(),
        })
    };
    assert!(task_health::delivered_or_uncertain(&Ok(PushOutcome::Sent)));
    assert!(task_health::delivered_or_uncertain(&Ok(
        PushOutcome::Recorded
    )));
    // A timeout or 5xx may have delivered the note: never resend it.
    assert!(task_health::delivered_or_uncertain(&error(None)));
    assert!(task_health::delivered_or_uncertain(&error(Some(503))));
    assert!(!task_health::delivered_or_uncertain(&error(Some(400))));
    assert!(!task_health::delivered_or_uncertain(&Ok(
        PushOutcome::SkippedNoToken
    )));
    assert!(!task_health::delivered_or_uncertain(&Ok(
        PushOutcome::Disabled
    )));
}
