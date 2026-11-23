//! Run log capture, including nested spans, other targets and spawned work
//! instrumented with the run span.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use common::{FakeTask, harness, local_time};
use omni_core::LogLevel;
use omni_core::clock::TestClock;
use omni_store::entity::{EntityWrite as _, UpsertOpts};
use omni_store::{DocMeta, DocWrite as _, JsValue, LogLine};
use omni_tasks::log_capture::{MAX_LINE_CHARS, MAX_LINES, run_span};
use omni_tasks::persistence::{self, TaskRunLog};
use omni_tasks::{EventBus, RunLogEvent, RunLogLayer, RunLogs};
use tracing::Instrument as _;
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt as _;

fn capture() -> (RunLogs, tracing::subscriber::DefaultGuard) {
    let logs = RunLogs::new(EventBus::new(64), TestClock::new(1_000));
    // A console layer at INFO, like LOG_LEVEL=info: the run layer still sees DEBUG.
    let console = tracing_subscriber::fmt::layer()
        .with_test_writer()
        .with_filter(tracing_subscriber::filter::LevelFilter::INFO);
    let subscriber = tracing_subscriber::registry()
        .with(console)
        .with(RunLogLayer::new(logs.clone()));
    let guard = tracing::subscriber::set_default(subscriber);
    (logs, guard)
}

#[tokio::test(flavor = "current_thread")]
async fn attributes_lines_logged_inside_the_run_context_including_sub_loggers() {
    let (logs, _guard) = capture();
    logs.start("run-1", "TaskA");
    async {
        tracing::info!(target: "Test", "hello");
        let sub = tracing::info_span!("sub");
        async {
            tracing::warn!(target: "Test:Sub", code = 7, "careful");
        }
        .instrument(sub)
        .await;
    }
    .instrument(run_span("run-1", "TaskA"))
    .await;
    let (lines, _) = logs.active("run-1").unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].level, LogLevel::Info);
    assert_eq!(lines[0].logger, "Test");
    assert_eq!(lines[0].msg, "hello");
    assert_eq!(lines[1].level, LogLevel::Warn);
    assert_eq!(lines[1].logger, "Test:Sub");
    assert_eq!(lines[1].msg, "careful code=7");
}

#[tokio::test(flavor = "current_thread")]
async fn captures_debug_lines_even_when_the_sink_threshold_is_info() {
    let (logs, _guard) = capture();
    logs.start("run-debug", "TaskA");
    run_span("run-debug", "TaskA")
        .in_scope(|| tracing::debug!(target: "Test", "below console threshold"));
    assert_eq!(logs.active("run-debug").unwrap().0.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn ignores_lines_logged_outside_any_run_context() {
    let (logs, _guard) = capture();
    logs.start("run-2", "TaskA");
    tracing::info!(target: "Test", "ambient log");
    assert!(logs.active("run-2").unwrap().0.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn keeps_concurrent_runs_separate() {
    let (logs, _guard) = capture();
    logs.start("run-a", "TaskA");
    logs.start("run-b", "TaskB");
    let (release_a, wait_a) = tokio::sync::oneshot::channel::<()>();
    let a = async move {
        tracing::info!(target: "Test", "from A");
        wait_a.await.unwrap();
        tracing::info!(target: "Test", "from A again");
    }
    .instrument(run_span("run-a", "TaskA"));
    let b = async move {
        tracing::info!(target: "Test", "from B");
        release_a.send(()).unwrap();
    }
    .instrument(run_span("run-b", "TaskB"));
    tokio::join!(a, b);
    let msgs = |id: &str| -> Vec<String> {
        logs.active(id)
            .unwrap()
            .0
            .into_iter()
            .map(|l| l.msg)
            .collect()
    };
    assert_eq!(msgs("run-a"), vec!["from A", "from A again"]);
    assert_eq!(msgs("run-b"), vec!["from B"]);
}

#[tokio::test(flavor = "current_thread")]
async fn attribution_follows_spawned_work() {
    let (logs, _guard) = capture();
    logs.start("run-spawn", "TaskA");
    let tracker = tokio_util::task::TaskTracker::new();
    async {
        omni_core::spawn::spawn_tracked(&tracker, "child", async {
            tokio::task::yield_now().await;
            tracing::info!(target: "Test", "from child");
        })
        .await
        .unwrap();
    }
    .instrument(run_span("run-spawn", "TaskA"))
    .await;
    assert_eq!(logs.active("run-spawn").unwrap().0[0].msg, "from child");
}

#[tokio::test(flavor = "current_thread")]
async fn drops_the_oldest_lines_beyond_the_per_run_cap_and_counts_them() {
    let (logs, _guard) = capture();
    logs.start("run-cap", "TaskA");
    run_span("run-cap", "TaskA").in_scope(|| {
        for index in 0..MAX_LINES + 100 {
            tracing::debug!(target: "Test", "line {index}");
        }
    });
    let (lines, dropped) = logs.active("run-cap").unwrap();
    assert_eq!(lines.len(), MAX_LINES);
    assert_eq!(dropped, 100);
    assert_eq!(lines[0].msg, "line 100");
    assert_eq!(lines[MAX_LINES - 1].msg, format!("line {}", MAX_LINES + 99));
}

#[tokio::test(flavor = "current_thread")]
async fn truncates_oversized_lines() {
    let (logs, _guard) = capture();
    logs.start("run-long", "TaskA");
    let long = "x".repeat(MAX_LINE_CHARS + 5_000);
    run_span("run-long", "TaskA").in_scope(|| tracing::debug!(target: "Test", "{long}"));
    let line = &logs.active("run-long").unwrap().0[0];
    assert_eq!(omni_core::js::utf16_len(&line.msg), MAX_LINE_CHARS + 1);
    assert!(line.msg.ends_with('…'));
}

#[tokio::test(flavor = "current_thread")]
async fn persists_the_buffer_on_finish_emits_end_and_clears_the_live_buffer() {
    let (_, _guard) = capture();
    let h = harness(TestClock::new(local_time(15, 10))).await;
    // The harness has its own RunLogs; route this test's subscriber to it.
    let subscriber = tracing_subscriber::registry().with(RunLogLayer::new(h.logs.clone()));
    let _inner = tracing::subscriber::set_default(subscriber);
    let mut events = h.bus.run_logs();
    let task = Arc::new(FakeTask::new("TaskC", "0 0 5 * * *").behaving(|_| {
        Box::pin(async {
            tracing::info!(target: "Test", "persisted line");
            Ok(())
        })
    }));
    h.registry.track(task).unwrap();
    let outcome = h.registry.run_now_and_wait("TaskC", None).await.unwrap();
    let run_id = outcome.run.run_id;
    assert!(h.logs.active(&run_id).is_none());
    let stored = persistence::get_run_logs(&h.store, &run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.task_name, "TaskC");
    assert_eq!(stored.lines.len(), 1);
    let mut last = None;
    while let Ok(event) = events.try_recv() {
        last = Some(event);
    }
    assert_eq!(
        last,
        Some(RunLogEvent::End {
            run_id: run_id.clone()
        })
    );
    let (run, lines, dropped) = h.registry.run_logs(&run_id).await.unwrap().unwrap();
    assert_eq!(run.run_id, run_id);
    assert_eq!(lines.len(), 1);
    assert_eq!(dropped, 0);
}

#[tokio::test]
async fn reads_legacy_uncompressed_log_rows() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let row = TaskRunLog {
        run_id: "run-legacy".to_owned(),
        task_name: "TaskC".to_owned(),
        lines: Some(vec![LogLine {
            t: 1,
            level: LogLevel::Info,
            logger: "Test".to_owned(),
            msg: "old row".to_owned(),
        }]),
        lines_gz: None,
        dropped: 0,
        extra: Default::default(),
    };
    h.store
        .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
        .await
        .unwrap();
    let stored = persistence::get_run_logs(&h.store, "run-legacy")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.lines[0].msg, "old row");
}

#[tokio::test]
async fn does_not_persist_a_row_for_runs_that_logged_nothing() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    h.registry
        .track(Arc::new(FakeTask::new("Silent", "0 0 5 * * *")))
        .unwrap();
    let outcome = h.registry.run_now_and_wait("Silent", None).await.unwrap();
    assert!(
        persistence::get_run_logs(&h.store, &outcome.run.run_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn drops_an_unreadable_log_row_instead_of_failing() {
    let h = harness(TestClock::new(local_time(15, 10))).await;
    let pk = omni_store::entity::pk::<TaskRunLog>(&"run-bad".to_owned()).unwrap();
    let mut row = indexmap::IndexMap::new();
    row.insert("runId".to_owned(), JsValue::String("run-bad".to_owned()));
    row.insert("taskName".to_owned(), JsValue::String("TaskC".to_owned()));
    row.insert("linesGz".to_owned(), JsValue::String("not gzip".to_owned()));
    row.insert("dropped".to_owned(), JsValue::Int(0));
    let key = pk.clone();
    h.store
        .write(move |tx| {
            tx.upsert_doc(
                &key,
                &JsValue::Object(row),
                DocMeta {
                    entity: Some("task-run-log".to_owned()),
                    ..DocMeta::default()
                },
            )
        })
        .await
        .unwrap();
    assert!(
        persistence::get_run_logs(&h.store, "run-bad")
            .await
            .unwrap()
            .is_none()
    );
    let key = pk.clone();
    let present = h
        .store
        .read(move |docs| omni_store::DocOps::has_doc(docs, &key))
        .await
        .unwrap();
    assert!(!present);
}
