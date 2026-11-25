//! `/api/health/deep`: the deploy smoke test's view of whether this build
//! works. Only critical checks (Omni's own store and scheduler) decide the
//! status; a failing remote service is reported without failing it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::http::StatusCode;
use omni::data_manager::DataManager;
use omni::ops::{OpsState, router};
use omni_runtime::HealthCheck;
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};
use omni_testkit::TestApp;

struct Noop(CronSchedule);

impl Task for Noop {
    fn name(&self) -> &str {
        "Noop"
    }
    fn schedule(&self) -> &CronSchedule {
        &self.0
    }
    fn options(&self) -> TaskOptions {
        TaskOptions {
            jitter: std::time::Duration::ZERO,
            run_on_startup: false,
        }
    }
    fn run<'a>(
        &'a self,
        _cx: &'a RunContext,
    ) -> futures::future::BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async { Ok(()) })
    }
}

fn check<'a>(body: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    body["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap()
}

#[tokio::test]
async fn critical_checks_decide_and_others_only_report() {
    let app = TestApp::new().await;
    let ops = |checks| {
        router(
            OpsState::new(
                app.ctx.clone(),
                DataManager::new(app.ctx.store.clone(), Vec::new()),
            )
            .with_health_checks(checks),
        )
    };

    // No task registered: the scheduler check fails, and it is critical.
    let (status, body) = app.get_json(&ops(Vec::new()), "/api/health/deep").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["ok"], false);
    assert_eq!(check(&body, "store_read")["ok"], true);
    assert_eq!(check(&body, "store_write")["ok"], true);
    assert_eq!(check(&body, "scheduler")["ok"], false);

    app.ctx
        .tasks
        .track(Arc::new(Noop(
            CronSchedule::parse("0 0 * * *", &jiff::tz::TimeZone::UTC).unwrap(),
        )))
        .unwrap();
    let remote = HealthCheck::new("imap", false, || async {
        Err("iCloud did not answer".to_owned())
    });
    let (status, body) = app.get_json(&ops(vec![remote]), "/api/health/deep").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(check(&body, "imap")["ok"], false);
    assert_eq!(check(&body, "imap")["critical"], false);
    assert_eq!(check(&body, "imap")["detail"], "iCloud did not answer");

    let broken = HealthCheck::new("journal", true, || async { Err("stuck".to_owned()) });
    let (status, body) = app.get_json(&ops(vec![broken]), "/api/health/deep").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}
