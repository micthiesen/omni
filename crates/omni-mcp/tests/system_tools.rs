//! System tools over fake ports.
//! Every output passes the output-schema validation of `typed_tool`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use common::*;
use futures::future::BoxFuture;
use omni_mcp::tools::system::{ConfiguredFeatures, SystemDeps, system_tools};
use omni_runtime::ports::{
    LiveDirectory, LiveIntelligence, LiveObservation, LiveTransition, PortError, Ports,
};
use omni_tasks::persistence::TaskRunData;
use serde_json::{Value, json};

const NOW: i64 = EPOCH_MS;
const DAY: i64 = 86_400_000;

fn summary(id: &str, name: &str, live: bool, viewers: Option<i64>) -> Value {
    json!({
        "id": id,
        "displayName": name,
        "tier": "primary",
        "bindings": [{"platform": "kick", "username": id, "url": format!("https://kick.com/{id}")}],
        "dgg": null,
        "live": live,
        "title": if live { json!("Live now") } else { Value::Null },
        "category": null,
        "viewerCount": viewers,
        "maxViewerCount": viewers,
        "startedAt": if live { json!(NOW - 60_000) } else { Value::Null },
        "lastStartedAt": null,
        "lastEndedAt": if live { Value::Null } else { json!(NOW - DAY) },
        "primary": if live { json!({"platform": "kick", "username": id, "url": format!("https://kick.com/{id}")}) } else { Value::Null },
        "sources": if live { json!([{"platform": "kick", "username": id, "title": "Live now", "viewerCount": viewers, "category": null}]) } else { json!([]) },
        "discoverySource": "dgg",
    })
}

struct Directory;

impl LiveDirectory for Directory {
    fn streamers(&self) -> BoxFuture<'_, Result<Vec<Value>, PortError>> {
        Box::pin(async {
            Ok(vec![
                summary("zed", "Zed", false, None),
                summary("ann", "Ann", true, Some(10)),
                summary("bob", "Bob", true, Some(500)),
                summary("amy", "amy", false, None),
            ])
        })
    }
    fn statuses(&self) -> BoxFuture<'_, Result<Vec<Value>, PortError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn display(&self) -> BoxFuture<'_, Result<Vec<Value>, PortError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn details<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Value>, PortError>> {
        Box::pin(async move {
            Ok((id == "bob").then(|| {
                let bucket = |days: i64| json!({"date": "2026-01-01", "maxViewers": 5, "timestamp": NOW - days * DAY});
                json!({
                    "livestream": summary("bob", "Bob", true, Some(500)),
                    "metrics": {
                        "dailyBuckets": [bucket(40), bucket(2)],
                        "allTimeMax": 900,
                        "allTimeMaxTimestamp": NOW - 50 * DAY,
                        "platforms": [{"platform": "kick", "username": "bob", "dailyBuckets": [bucket(40), bucket(1)], "allTimeMax": 900, "allTimeMaxTimestamp": NOW - 50 * DAY}],
                    },
                    "sessions": [
                        {"startedAt": NOW - 3 * DAY, "endedAt": NOW - 3 * DAY + 1000, "durationMs": 1000, "peakViewers": 3, "title": "old", "platform": "kick", "username": "bob"},
                        {"startedAt": NOW - DAY, "endedAt": NOW - DAY + 1000, "durationMs": 1000, "peakViewers": 4, "title": "new", "platform": "kick", "username": "bob"},
                    ],
                })
            }))
        })
    }
}

struct Intelligence;

impl LiveIntelligence for Intelligence {
    fn observe_live<'a>(&'a self, _: &'a LiveObservation) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn after_tick(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn on_transition<'a>(&'a self, _: &'a LiveTransition) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn details<'a>(
        &'a self,
        _: &'a str,
        _: usize,
    ) -> BoxFuture<'a, Result<Option<Value>, PortError>> {
        Box::pin(async {
            Ok(Some(json!({
                "intelligence": null,
                "diagnostics": null,
                "events": [],
                "runtime": null,
                "generatedAt": NOW,
            })))
        })
    }
    fn diagnostics(&self) -> BoxFuture<'_, Result<Value, PortError>> {
        Box::pin(async { Ok(Value::Null) })
    }
    fn record_feedback(&self, _: Value) -> BoxFuture<'_, Result<Value, PortError>> {
        Box::pin(async { Ok(Value::Null) })
    }
}

fn run(id: &str, task: &str) -> TaskRunData {
    serde_json::from_value(json!({
        "runId": id, "taskName": task, "trigger": "schedule", "startedAt": 1, "finishedAt": 2,
        "status": "success",
    }))
    .unwrap()
}

async fn router() -> (axum::Router, omni_testkit::TestStore) {
    let clock = clock();
    let db = test_store(&clock).await;
    let ports = Ports::default();
    ports.set_live_directory(Arc::new(Directory)).unwrap();
    ports.set_live_intelligence(Arc::new(Intelligence)).unwrap();
    let tasks = FakeTasks {
        tasks: vec![serde_json::from_value(json!({
            "name": "PressPods", "schedule": "0 0 9 * * 0", "running": false,
            "nextRuns": ["2026-10-11T16:00:00.000Z"],
            "lastRun": {"runId": "PressPods:1", "taskName": "PressPods", "trigger": "manual",
                        "scheduledFor": null, "startedAt": 1, "finishedAt": 2, "status": "error", "error": "boom", "summary": null}
        }))
        .unwrap()],
        runs: vec![run("A:1", "A"), run("B:1", "B")],
        ..FakeTasks::default()
    };
    let own = system_tools(&SystemDeps {
        tasks: Arc::new(tasks),
        ports,
        features: ConfiguredFeatures {
            icloud: true,
            web_search: false,
            ios_controls: true,
            printing: false,
        },
        clock: clock.clone(),
    })
    .unwrap();
    (mcp_router(&db.store, &clock, own, None), db)
}

fn structured(message: &Value) -> &Value {
    assert!(!is_error(message), "{message}");
    &message["result"]["structuredContent"]
}

#[tokio::test]
async fn reports_capabilities_without_configuration_values() {
    let (router, _db) = router().await;
    let message = call_tool(&router, "system_status", json!({})).await;
    assert_eq!(
        structured(&message)["capabilities"],
        json!({
            "taskControls": true, "livestreams": true, "livestreamIntelligence": true,
            "iCloudEmail": false, "iCloudCalendar": true, "webSearch": false,
            "iosControls": true, "printing": false,
        })
    );
}

#[tokio::test]
async fn lists_tasks_with_runs_in_the_task_run_shape() {
    let (router, _db) = router().await;
    let message = call_tool(&router, "tasks_list", json!({})).await;
    let last = &structured(&message)["tasks"][0]["lastRun"];
    assert_eq!(
        *last,
        json!({"runId": "PressPods:1", "taskName": "PressPods", "trigger": "manual",
               "startedAt": 1, "finishedAt": 2, "status": "error", "error": "boom"})
    );
    let runs = call_tool(&router, "task_runs_list", json!({"taskName": " B "})).await;
    let runs = structured(&runs);
    assert_eq!(runs["total"], 1);
    assert_eq!(runs["resultWindowTruncated"], false);
    let got = call_tool(&router, "task_run_get", json!({"runId": "A:1"})).await;
    assert_eq!(structured(&got)["run"]["runId"], "A:1");
    assert_eq!(structured(&got)["logs"], json!([]));
    let missing = call_tool(&router, "task_run_get", json!({"runId": "Z:1"})).await;
    assert_eq!(error_text(&missing), "[failed] Unknown task run \"Z:1\"");
}

#[tokio::test]
async fn lists_livestreams_live_first_by_viewers_then_name() {
    let (router, _db) = router().await;
    let message = call_tool(&router, "livestreams_list", json!({})).await;
    let names: Vec<&str> = structured(&message)["livestreams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["displayName"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Bob", "Ann", "amy", "Zed"]);
    assert!(
        structured(&message)["livestreams"][0]
            .get("discoverySource")
            .is_none()
    );
    let live = call_tool(
        &router,
        "livestreams_list",
        json!({"liveOnly": true, "limit": 1}),
    )
    .await;
    assert_eq!(structured(&live)["total"], 2);
    assert_eq!(structured(&live)["nextCursor"], 1);
}

#[tokio::test]
async fn gets_one_livestream_with_bounded_metrics_and_sessions() {
    let (router, _db) = router().await;
    let message = call_tool(
        &router,
        "livestream_get",
        json!({"streamerId": "bob", "include": ["metrics", "sessions", "intelligence"], "metricsDays": 30, "sessionLimit": 1}),
    )
    .await;
    let body = structured(&message);
    assert_eq!(body["livestream"]["id"], "bob");
    assert_eq!(body["metrics"]["dailyBuckets"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["metrics"]["platforms"][0]["dailyBuckets"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(body["sessions"][0]["title"], "new");
    assert_eq!(
        body["intelligence"],
        json!({"current": null, "diagnostics": null, "events": [], "runtime": null})
    );
    let plain = call_tool(&router, "livestream_get", json!({"streamerId": "bob"})).await;
    assert_eq!(structured(&plain)["metrics"], Value::Null);
    let unknown = call_tool(&router, "livestream_get", json!({"streamerId": "nope"})).await;
    assert_eq!(error_text(&unknown), "[failed] Unknown livestream \"nope\"");
}
