//! The task registry. Started runs are uninterruptible by design (a run, once
//! started, runs inside `must_complete`);
//! `dropping_the_waiter_does_not_cancel_the_run` asserts that rule.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeTask, harness, local_time};
use omni_core::clock::TestClock;
use omni_tasks::persistence::{self, INTERRUPTED_ERROR};
use omni_tasks::{RunNowError, TaskError, TaskRunStatus, Trigger};
use serde_json::json;

#[tokio::test(start_paused = true)]
async fn repairs_interrupted_runs_through_explicit_initialization() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let running = persistence::record_run_start(
        &h.store,
        "Interrupted",
        Trigger::Schedule,
        None,
        None,
        local_time(15, 9),
    )
    .await
    .unwrap();
    h.registry.initialize().await.unwrap();
    let run = persistence::get_run(&h.store, &running.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, TaskRunStatus::Error);
    assert_eq!(run.error.as_deref(), Some(INTERRUPTED_ERROR));
    assert_eq!(run.finished_at, Some(local_time(15, 10)));
}

#[tokio::test(start_paused = true)]
async fn establishes_a_baseline_without_running_a_never_seen_task() {
    let now = local_time(15, 10);
    let h = harness(TestClock::new(now)).await;
    let task = Arc::new(FakeTask::new("Daily", "0 0 5 * * *"));
    h.registry.track(task.clone()).unwrap();
    h.registry.recover_missed().await.unwrap();
    assert_eq!(task.runs(), 0);
    let state = persistence::get_task_schedule_state(&h.store, "Daily")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.task_name, "Daily");
    assert_eq!(state.schedule, "0 0 5 * * *");
    assert_eq!(state.evaluated_through, now);
}

#[tokio::test(start_paused = true)]
async fn records_an_eligible_recovery_with_its_original_scheduled_time() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let task = Arc::new(FakeTask::new("Daily", "0 0 5 * * *"));
    h.registry.track(task.clone()).unwrap();
    persistence::mark_schedule_evaluated(&h.store, "Daily", "0 0 5 * * *", local_time(14, 6))
        .await
        .unwrap();
    h.registry.recover_missed().await.unwrap();
    assert_eq!(task.runs(), 1);
    let last = persistence::get_last_run(&h.store, "Daily")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last.task_name, "Daily");
    assert_eq!(last.trigger, Trigger::Catchup);
    assert_eq!(last.scheduled_for, Some(local_time(15, 5)));
    assert_eq!(last.status, TaskRunStatus::Success);
    let state = persistence::get_task_schedule_state(&h.store, "Daily")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.evaluated_through, local_time(15, 5));
}

#[tokio::test(start_paused = true)]
async fn does_not_add_recovery_on_top_of_a_task_that_runs_on_startup() {
    let now = local_time(15, 10);
    let h = harness(TestClock::new(now)).await;
    let task = Arc::new(FakeTask::new("StartupDaily", "0 0 5 * * *").on_startup());
    h.registry.track(task.clone()).unwrap();
    persistence::mark_schedule_evaluated(
        &h.store,
        "StartupDaily",
        "0 0 5 * * *",
        local_time(14, 6),
    )
    .await
    .unwrap();
    h.registry.recover_missed().await.unwrap();
    assert_eq!(task.runs(), 0);
    let state = persistence::get_task_schedule_state(&h.store, "StartupDaily")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.evaluated_through, now);
}

#[tokio::test(start_paused = true)]
async fn resets_the_baseline_when_a_tasks_schedule_changes() {
    let now = local_time(15, 10);
    let h = harness(TestClock::new(now)).await;
    let task = Arc::new(FakeTask::new("Changed", "0 0 6 * * *"));
    h.registry.track(task.clone()).unwrap();
    persistence::mark_schedule_evaluated(&h.store, "Changed", "0 0 5 * * *", local_time(14, 6))
        .await
        .unwrap();
    h.registry.recover_missed().await.unwrap();
    assert_eq!(task.runs(), 0);
    let state = persistence::get_task_schedule_state(&h.store, "Changed")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.schedule, "0 0 6 * * *");
    assert_eq!(state.evaluated_through, now);
}

#[tokio::test]
async fn passes_optional_input_only_to_tasks_that_handle_manual_runs() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let task = Arc::new(FakeTask::new("Parameterized", "0 0 5 * * *").accepting_input());
    h.registry.track(task.clone()).unwrap();
    h.registry
        .run_now_and_wait("Parameterized", Some(json!({ "count": 5 })))
        .await
        .unwrap();
    assert_eq!(task.inputs(), vec![json!({ "count": 5 })]);
    assert_eq!(task.runs(), 0);
}

#[tokio::test]
async fn rejects_manual_input_for_ordinary_scheduled_tasks() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    h.registry
        .track(Arc::new(FakeTask::new("Ordinary", "0 0 5 * * *")))
        .unwrap();
    assert!(matches!(
        h.registry.run_now("Ordinary", Some(json!({ "count": 5 }))),
        Err(RunNowError::ManualInputUnsupported { ref name }) if name == "Ordinary"
    ));
    assert!(matches!(
        h.registry
            .run_now_and_wait("Ordinary", Some(json!({ "count": 5 })))
            .await,
        Err(RunNowError::ManualInputUnsupported { .. })
    ));
    let missing = h.registry.run_now("Missing", None);
    assert!(matches!(missing, Err(RunNowError::NotFound { ref name }) if name == "Missing"));
    assert_eq!(
        missing.map_err(|e| e.to_string()),
        Err("Unknown task \"Missing\"".to_owned())
    );
}

#[tokio::test]
async fn atomically_rejects_one_of_two_simultaneous_manual_runs() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let task = Arc::new(
        FakeTask::new("ConcurrentManual", "0 0 5 * * *").behaving(move |_| {
            let gate = gate.clone();
            Box::pin(async move {
                gate.notified().await;
                Ok(())
            })
        }),
    );
    h.registry.track(task.clone()).unwrap();
    let first = h.registry.run_now("ConcurrentManual", None);
    let second = h.registry.run_now("ConcurrentManual", None);
    assert!(first.is_ok());
    assert_eq!(
        second.map_err(|e| e.to_string()),
        Err("Task \"ConcurrentManual\" is already running".to_owned())
    );
    release.notify_one();
    h.tracker.close();
    h.tracker.wait().await;
    let run = persistence::get_run(&h.store, &first.unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.trigger, Trigger::Manual);
    assert_eq!(run.status, TaskRunStatus::Success);
}

#[tokio::test]
async fn waits_for_the_exact_manual_run_to_finish_successfully() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let task = Arc::new(FakeTask::new("Awaited", "0 0 5 * * *").accepting_input());
    h.registry.track(task.clone()).unwrap();
    let outcome = h
        .registry
        .run_now_and_wait("Awaited", Some(json!({ "source": "email" })))
        .await
        .unwrap();
    assert_eq!(task.inputs(), vec![json!({ "source": "email" })]);
    let last = persistence::get_last_run(&h.store, "Awaited")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last.run_id, outcome.run.run_id);
    assert_eq!(last.status, TaskRunStatus::Success);
    assert_eq!(outcome.run.status, TaskRunStatus::Success);
}

#[tokio::test]
async fn fails_only_after_the_awaited_run_is_durably_recorded_as_failed() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let task = Arc::new(
        FakeTask::new("AwaitedFailure", "0 0 5 * * *")
            .accepting_input()
            .behaving(|_| Box::pin(async { Err(TaskError::new("workspace run failed")) })),
    );
    h.registry.track(task.clone()).unwrap();
    let result = h
        .registry
        .run_now_and_wait("AwaitedFailure", Some(json!({ "source": "email" })))
        .await;
    let Err(RunNowError::RunFailed { run_id, message }) = result else {
        panic!("expected RunFailed, got {result:?}");
    };
    assert_eq!(message, "workspace run failed");
    let last = persistence::get_last_run(&h.store, "AwaitedFailure")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last.run_id, run_id);
    assert_eq!(last.status, TaskRunStatus::Error);
    assert_eq!(last.error.as_deref(), Some("workspace run failed"));
}

#[tokio::test]
async fn dropping_the_waiter_does_not_cancel_the_run() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let task = Arc::new(FakeTask::new("Native", "0 0 5 * * *").behaving(|_| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(())
        })
    }));
    h.registry.track(task.clone()).unwrap();
    let waiter = h.registry.run_now_and_wait("Native", None);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), waiter)
            .await
            .is_err()
    );
    h.tracker.close();
    h.tracker.wait().await;
    let last = persistence::get_last_run(&h.store, "Native")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last.status, TaskRunStatus::Success);
    assert!(last.finished_at.is_some());
}

#[tokio::test]
async fn a_panicking_task_is_recorded_as_an_error() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let task = Arc::new(
        FakeTask::new("Panics", "0 0 5 * * *").behaving(|_| Box::pin(async { panic!("boom") })),
    );
    h.registry.track(task).unwrap();
    let result = h.registry.run_now_and_wait("Panics", None).await;
    assert!(
        matches!(result, Err(RunNowError::RunFailed { ref message, .. }) if message == "panic: boom")
    );
    assert!(!h.registry.is_running("Panics"));
}

#[tokio::test]
async fn list_reports_last_run_running_flag_and_next_runs() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let task = Arc::new(FakeTask::new("Listed", "0 0 5 * * *"));
    h.registry.track(task).unwrap();
    assert!(
        h.registry
            .track(Arc::new(FakeTask::new("Listed", "0 0 5 * * *")))
            .is_err()
    );
    h.registry.run_now_and_wait("Listed", None).await.unwrap();
    let tasks = h.registry.list().await.unwrap();
    assert_eq!(tasks.len(), 1);
    let info = &tasks[0];
    assert_eq!(info.name, "Listed");
    assert_eq!(info.schedule, "0 0 5 * * *");
    assert!(!info.running);
    assert_eq!(
        info.next_runs,
        vec![
            "2026-07-16T12:00:00.000Z",
            "2026-07-17T12:00:00.000Z",
            "2026-07-18T12:00:00.000Z"
        ]
    );
    let last = info.last_run.as_ref().unwrap();
    assert_eq!(last.status, omni_api::runs::RunStatus::Success);
}

#[tokio::test]
async fn shutdown_abandons_queued_runs_that_have_not_started() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let task = Arc::new(FakeTask::new("Queued", "0 0 5 * * *").behaving(move |_| {
        let gate = gate.clone();
        Box::pin(async move {
            gate.notified().await;
            Ok(())
        })
    }));
    h.registry.track(task.clone()).unwrap();
    h.registry.run_now("Queued", None).unwrap();
    // Let the first run take the permit, then queue a waiter behind it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let registry = h.registry.clone();
    let waiter = tokio::spawn(async move { registry.run_now_and_wait("Queued", None).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    h.registry.shutdown();
    release.notify_one();
    assert!(matches!(waiter.await.unwrap(), Err(RunNowError::Shutdown)));
    h.tracker.close();
    h.tracker.wait().await;
    assert_eq!(task.runs(), 1);
}

#[tokio::test]
async fn a_run_that_reports_degraded_is_recorded_as_degraded_with_its_reasons() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let task = Arc::new(FakeTask::new("Skips", "0 0 5 * * *").behaving(|_| {
        Box::pin(async {
            omni_tasks::report_degraded("Plex offline");
            futures::future::ready(()).await;
            omni_tasks::report_degraded("selection returned an unknown candidate id");
            Ok(())
        })
    }));
    h.registry.track(task).unwrap();
    let outcome = h.registry.run_now_and_wait("Skips", None).await.unwrap();
    assert_eq!(outcome.run.status, TaskRunStatus::Degraded);
    let last = persistence::get_last_run(&h.store, "Skips")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last.status, TaskRunStatus::Degraded);
    assert_eq!(
        last.error.as_deref(),
        Some("Plex offline; selection returned an unknown candidate id")
    );
    assert_eq!(
        omni_api::runs::Run::from(&last).status,
        omni_api::runs::RunStatus::Degraded
    );
}

#[tokio::test]
async fn a_failure_outranks_a_degraded_report_and_reports_never_leak_into_later_runs() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let task = Arc::new(FakeTask::new("Mixed", "0 0 5 * * *").behaving(move |_| {
        let call = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            if call == 0 {
                omni_tasks::report_degraded("upstream down");
                return Err(TaskError::new("then it failed"));
            }
            Ok(())
        })
    }));
    h.registry.track(task).unwrap();
    let first = h.registry.run_now_and_wait("Mixed", None).await;
    assert!(
        matches!(first, Err(RunNowError::RunFailed { ref message, .. }) if message == "then it failed")
    );
    let second = h.registry.run_now_and_wait("Mixed", None).await.unwrap();
    assert_eq!(second.run.status, TaskRunStatus::Success);
    assert_eq!(second.run.error, None);
}
