//! Run history persistence (prune selection is unit-tested in
//! `src/persistence.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{harness, local_time};
use omni_core::clock::TestClock;
use omni_store::DocOps as _;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_tasks::persistence::{self, RunStart, TaskRunLog};
use omni_tasks::{TaskRunData, TaskRunStatus, Trigger};

fn make_run(task: &str, started_at: i64) -> TaskRunData {
    TaskRunData {
        run_id: format!("{task}:{started_at}"),
        task_name: task.to_owned(),
        trigger: Trigger::Schedule,
        scheduled_for: None,
        started_at,
        finished_at: None,
        status: TaskRunStatus::Success,
        error: None,
        summary: None,
        extra: Default::default(),
    }
}

#[tokio::test(start_paused = true)]
async fn commits_the_run_cursor_and_pruning_as_one_operation() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    h.store
        .write(|tx| {
            for index in 0..50 {
                let run = make_run("A", index);
                tx.upsert(&run, UpsertOpts::default())?;
                tx.upsert(
                    &TaskRunLog {
                        run_id: run.run_id.clone(),
                        task_name: "A".to_owned(),
                        lines: Some(Vec::new()),
                        lines_gz: None,
                        dropped: 0,
                        extra: Default::default(),
                    },
                    UpsertOpts::default(),
                )?;
            }
            Ok::<_, omni_store::StoreError>(())
        })
        .await
        .unwrap();

    let run = persistence::record_run_start_and_mark_schedule(
        &h.store,
        RunStart {
            task_name: "A".to_owned(),
            trigger: Trigger::Catchup,
            schedule: "0 0 5 * * *".to_owned(),
            evaluated_through: 10_000,
            run_id: "A:new".to_owned(),
            scheduled_for: Some(9_000),
            started_at: 10_000,
        },
    )
    .await
    .unwrap();

    assert_eq!(run.run_id, "A:new");
    assert_eq!(run.status, TaskRunStatus::Running);
    let count = h
        .store
        .read(|docs| docs.count_by_entity("task-run"))
        .await
        .unwrap();
    assert_eq!(count, 50);
    let state = persistence::get_task_schedule_state(&h.store, "A")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.evaluated_through, 10_000);
    let pruned_log = h
        .store
        .read(|docs| docs.get::<TaskRunLog>(&"A:0".to_owned()))
        .await
        .unwrap();
    assert!(pruned_log.is_none());
    let kept_log = h
        .store
        .read(|docs| docs.get::<TaskRunLog>(&"A:1".to_owned()))
        .await
        .unwrap();
    assert!(kept_log.is_some());
}

#[tokio::test(start_paused = true)]
async fn run_end_settles_status_error_summary_and_finish_time() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let run = persistence::record_run_start(&h.store, "B", Trigger::Manual, None, None, 5)
        .await
        .unwrap();
    assert!(run.run_id.starts_with("B:"));
    let settled = persistence::record_run_end(
        &h.store,
        &run.run_id,
        persistence::RunEnd {
            status: TaskRunStatus::Error,
            error: Some("boom".to_owned()),
            summary: Some("did a thing".to_owned()),
            finished_at: 9,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(settled.status, TaskRunStatus::Error);
    assert_eq!(settled.error.as_deref(), Some("boom"));
    assert_eq!(settled.summary.as_deref(), Some("did a thing"));
    assert_eq!(settled.finished_at, Some(9));
    let runs = persistence::get_runs(&h.store, None, 10).await.unwrap();
    assert_eq!(runs, vec![settled]);
}

#[tokio::test(start_paused = true)]
async fn run_end_errors_are_stored_scrubbed() {
    omni_core::secrets::register(["run-error-secret-44".to_owned()]);
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let run = persistence::record_run_start(&h.store, "B", Trigger::Manual, None, None, 5)
        .await
        .unwrap();
    let settled = persistence::record_run_end(
        &h.store,
        &run.run_id,
        persistence::RunEnd {
            status: TaskRunStatus::Error,
            error: Some("GET ?key=run-error-secret-44 failed".to_owned()),
            summary: None,
            finished_at: 9,
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(settled.error.as_deref(), Some("GET ?key=[redacted] failed"));
}
