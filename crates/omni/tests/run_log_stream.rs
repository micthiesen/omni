//! Run logs and their SSE tail: `init` replays the buffer, `line` frames
//! follow, `done` carries the settled run and ends the stream; a finished run
//! gets `init` and `done` back to back.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{SseReader, get, ops};
use futures::future::BoxFuture;
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};
use omni_testkit::TestApp;
use serde_json::Value;
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(5);

struct Gated {
    schedule: CronSchedule,
    release: Arc<Notify>,
    started: Arc<Notify>,
}

impl Task for Gated {
    fn name(&self) -> &str {
        "Gated"
    }
    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }
    fn options(&self) -> TaskOptions {
        TaskOptions::default()
    }
    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            tracing::info!(target: "Gated", "first line");
            self.started.notify_one();
            self.release.notified().await;
            tracing::debug!(target: "Gated", "second line");
            Ok(())
        })
    }
}

fn logging(app: &TestApp) -> impl tracing::Subscriber + Send + Sync {
    omni::logging::subscriber(
        omni::logging::LoggingSetup {
            level: omni_core::LogLevel::Error,
            logs_path: None,
            tz: jiff::tz::TimeZone::UTC,
            clock: app.ctx.clock.clone(),
        },
        Some(app.ctx.run_logs()),
        None,
    )
}

#[tokio::test]
async fn tails_a_running_run_until_done() {
    let app = TestApp::new().await;
    let _guard = tracing::subscriber::set_default(logging(&app));
    let release = Arc::new(Notify::new());
    let started = Arc::new(Notify::new());
    app.ctx
        .tasks
        .track(Arc::new(Gated {
            schedule: CronSchedule::parse("0 0 * * * *", &jiff::tz::TimeZone::UTC).unwrap(),
            release: release.clone(),
            started: started.clone(),
        }))
        .unwrap();
    let (router, _state) = ops(&app.ctx);
    let run_id = app.ctx.tasks.run_now("Gated", None).unwrap();
    started.notified().await;

    let (status, body) = get(&router, &format!("/api/task-runs/{run_id}/logs/stream")).await;
    assert_eq!(status, StatusCode::OK);
    let mut reader = SseReader::new(body);
    let init = reader.next(WAIT).await.unwrap();
    assert_eq!(
        (init.event.as_str(), init.id.as_deref()),
        ("init", Some("0"))
    );
    let init: Value = serde_json::from_str(&init.data).unwrap();
    assert_eq!(init["run"]["status"], "running");
    assert_eq!(init["run"]["finishedAt"], Value::Null);
    assert_eq!(init["lines"][0]["msg"], "first line");
    assert_eq!(init["dropped"], 0);

    release.notify_one();
    let line = reader.next(WAIT).await.unwrap();
    assert_eq!(
        (line.event.as_str(), line.id.as_deref()),
        ("line", Some("1"))
    );
    let line: Value = serde_json::from_str(&line.data).unwrap();
    assert_eq!(line["msg"], "second line");
    assert_eq!(line["level"], "debug");
    assert_eq!(line["logger"], "Gated");
    let done = reader.next(WAIT).await.unwrap();
    assert_eq!(
        (done.event.as_str(), done.id.as_deref()),
        ("done", Some("2"))
    );
    let done: Value = serde_json::from_str(&done.data).unwrap();
    assert_eq!(done["status"], "success");
    assert!(reader.ended(WAIT).await, "the stream ends after done");

    // The finished run: init and done back to back, then the end.
    let (_, body) = get(&router, &format!("/api/task-runs/{run_id}/logs/stream")).await;
    let mut reader = SseReader::new(body);
    let init = reader.next(WAIT).await.unwrap();
    assert_eq!(init.event, "init");
    let init: Value = serde_json::from_str(&init.data).unwrap();
    assert_eq!(init["lines"].as_array().unwrap().len(), 2);
    assert_eq!(reader.next(WAIT).await.unwrap().event, "done");
    assert!(reader.ended(WAIT).await);

    // The JSON route serves the persisted lines.
    let (status, body) = app
        .get_json(&router, &format!("/api/task-runs/{run_id}/logs"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["lines"][1]["msg"], "second line");
    assert_eq!(body["run"]["summary"], Value::Null);
}

#[tokio::test]
async fn unknown_runs_are_404() {
    let app = TestApp::new().await;
    let (router, _state) = ops(&app.ctx);
    let (status, body) = app
        .get_json(&router, "/api/task-runs/nope/logs/stream")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, serde_json::json!({"error": "Unknown run"}));
    let (status, _) = app.get_json(&router, "/api/task-runs/nope/logs").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn task_routes_map_registry_errors() {
    let app = TestApp::new().await;
    let (router, _state) = ops(&app.ctx);
    let (status, body) = app
        .post_json(&router, "/api/tasks/Missing/run", &Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        serde_json::json!({"error": "Unknown task \"Missing\""})
    );
    let (status, body) = app.get_json(&router, "/api/tasks").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({"tasks": []}));
    let (status, body) = app.get_json(&router, "/api/task-runs?limit=0").await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, serde_json::json!({"runs": []}))
    );
    let (status, body) = app.get_json(&router, "/api/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    let (_, snapshot) = app.get_json(&router, "/api/snapshot").await;
    assert_eq!(body["build"], snapshot["build"], "one identity per process");
    assert!(
        body["build"]["server"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert!(body["build"]["frontend"].is_string());
    let (status, body) = app.get_json(&router, "/api/costs?days=14").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        serde_json::json!({"error": "days must be 7, 30, 90, or all"})
    );
    let (status, body) = app.get_json(&router, "/api/costs?days=all").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["days"], Value::Null);
}
