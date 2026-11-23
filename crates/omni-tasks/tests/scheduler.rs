//! Scheduler acceptance tests: no overlap, fires skipped during a
//! run, jitter bounds, startup trigger mapping, uninterruptible runs on
//! shutdown, and the ERROR log for failed runs. These run on real time with
//! an every-second schedule, because the store thread defeats paused-time
//! auto-advance.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{FakeTask, harness};
use omni_core::clock::SystemClock;
use omni_tasks::{Scheduler, TaskError, TaskOptions, TaskRunStatus, Trigger, persistence};
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt as _};

type Spans = Arc<Mutex<Vec<(Instant, Instant)>>>;

fn sleeping_task(name: &str, work: Duration, spans: Spans) -> FakeTask {
    FakeTask::new(name, "* * * * * *").behaving(move |_| {
        let spans = spans.clone();
        Box::pin(async move {
            let start = Instant::now();
            tokio::time::sleep(work).await;
            spans.lock().unwrap().push((start, Instant::now()));
            Ok(())
        })
    })
}

async fn stop(shutdown: CancellationToken, handle: tokio::task::JoinHandle<()>) {
    shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_overlaps_and_skips_fires_that_land_during_a_run() {
    let h = harness(Arc::new(SystemClock)).await;
    let spans: Spans = Arc::default();
    let task = Arc::new(sleeping_task(
        "Slow",
        Duration::from_millis(1_500),
        spans.clone(),
    ));
    h.registry.track(task.clone()).unwrap();
    let shutdown = CancellationToken::new();
    let handle = Scheduler::start(h.registry.clone(), shutdown.clone(), &h.tracker);
    tokio::time::sleep(Duration::from_millis(5_200)).await;
    stop(shutdown, handle).await;

    let spans = spans.lock().unwrap().clone();
    assert!(spans.len() >= 2, "{} runs", spans.len());
    // Five seconds of an every-second schedule, but a 1.5 s run swallows the
    // fires that land inside it: at most three runs fit.
    assert!(spans.len() <= 3, "{} runs", spans.len());
    for pair in spans.windows(2) {
        let (_, previous_end) = pair[0];
        let (next_start, _) = pair[1];
        assert!(next_start >= previous_end, "runs overlapped");
        // The next fire is the first whole second after the run completed.
        assert!(next_start - previous_end <= Duration::from_millis(1_100));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maps_the_first_run_of_a_startup_task_to_the_startup_trigger() {
    let h = harness(Arc::new(SystemClock)).await;
    let spans: Spans = Arc::default();
    let task = Arc::new(sleeping_task("Startup", Duration::from_millis(10), spans).on_startup());
    h.registry.track(task.clone()).unwrap();
    let shutdown = CancellationToken::new();
    let handle = Scheduler::start(h.registry.clone(), shutdown.clone(), &h.tracker);
    tokio::time::sleep(Duration::from_millis(2_300)).await;
    stop(shutdown, handle).await;

    let mut runs = persistence::get_runs(&h.store, Some("Startup"), 10)
        .await
        .unwrap();
    runs.reverse();
    assert!(runs.len() >= 2, "{} runs", runs.len());
    assert_eq!(runs[0].trigger, Trigger::Startup);
    assert!(runs[1..].iter().all(|run| run.trigger == Trigger::Schedule));
    assert!(runs.iter().all(|run| run.status == TaskRunStatus::Success));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jitter_delays_each_run_within_its_bound() {
    let h = harness(Arc::new(SystemClock)).await;
    let offsets = Arc::new(Mutex::new(Vec::new()));
    let seen = offsets.clone();
    let task = Arc::new(
        FakeTask::new("Jittered", "* * * * * *")
            .with_options(TaskOptions {
                jitter: Duration::from_millis(300),
                run_on_startup: false,
            })
            .behaving(move |_| {
                let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
                seen.lock().unwrap().push(since_epoch.subsec_millis());
                Box::pin(async { Ok(()) })
            }),
    );
    h.registry.track(task).unwrap();
    let shutdown = CancellationToken::new();
    let handle = Scheduler::start(h.registry.clone(), shutdown.clone(), &h.tracker);
    tokio::time::sleep(Duration::from_millis(4_200)).await;
    stop(shutdown, handle).await;

    let offsets = offsets.lock().unwrap().clone();
    assert!(offsets.len() >= 3, "{offsets:?}");
    // Each run starts in [fire, fire + jitter) plus store latency.
    assert!(offsets.iter().all(|ms| *ms < 300 + 150), "{offsets:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_in_flight_finishes_on_shutdown() {
    let h = harness(Arc::new(SystemClock)).await;
    let spans: Spans = Arc::default();
    let task =
        Arc::new(sleeping_task("Long", Duration::from_millis(800), spans.clone()).on_startup());
    h.registry.track(task).unwrap();
    let shutdown = CancellationToken::new();
    let handle = Scheduler::start(h.registry.clone(), shutdown.clone(), &h.tracker);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stopping = Instant::now();
    stop(shutdown, handle).await;
    assert!(stopping.elapsed() >= Duration::from_millis(500));
    assert_eq!(spans.lock().unwrap().len(), 1);
    let runs = persistence::get_runs(&h.store, Some("Long"), 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, TaskRunStatus::Success);
    assert!(runs[0].finished_at.is_some());
    h.tracker.close();
    h.tracker.wait().await;
}

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<(tracing::Level, String, String)>>>);

struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Events {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0.lock().unwrap().push((
            *event.metadata().level(),
            event.metadata().target().to_owned(),
            message.0,
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_run_is_recorded_and_logged_at_error() {
    let events = Events::default();
    let _guard = tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(events.clone()),
    );
    let h = harness(Arc::new(SystemClock)).await;
    let task = Arc::new(
        FakeTask::new("Failing", "0 0 5 * * *")
            .on_startup()
            .behaving(|_| Box::pin(async { Err(TaskError::new("nope")) })),
    );
    h.registry.track(task).unwrap();
    let shutdown = CancellationToken::new();
    let handle = Scheduler::start(h.registry.clone(), shutdown.clone(), &h.tracker);
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(shutdown, handle).await;

    let runs = persistence::get_runs(&h.store, Some("Failing"), 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, TaskRunStatus::Error);
    assert_eq!(runs[0].error.as_deref(), Some("nope"));
    let logged = events.0.lock().unwrap().clone();
    assert!(
        logged
            .iter()
            .any(|(level, target, message)| *level == tracing::Level::ERROR
                && target == "Scheduler"
                && message == "Error running task \"Failing\""),
        "{logged:?}"
    );
}
