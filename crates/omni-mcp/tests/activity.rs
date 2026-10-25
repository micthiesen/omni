//! MCP call recording, cancellation and pruning, and the activity routes.
//!
//! Recording cases use the real metadata of `claude_session_start`
//! (require_approval) and `email_search` (allow), so the email row's policy is
//! `allow`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{clock, meta, send, test_store};
use futures::future::BoxFuture;
use omni_api::mcp_activity::{McpCallStatus, RecommendedPolicy};
use omni_core::clock::Clock as _;
use omni_mcp::activity::{
    ActivityQuery, ActivityRecorder, CallOutcome, McpCallData, bound_value, mark_interrupted_calls,
    prune_calls, summarize_calls,
};
use omni_mcp::activity_routes;
use omni_mcp::host::{ClaudeHost, HostCommand, HostError, HostLinkStatus};
use omni_store::Store;
use omni_store::cbor::{Extra, JsValue};
use omni_store::entity::{EntityOps, EntityWrite, UpsertOpts};
use serde_json::{Map, Value, json};
use tokio_util::task::TaskTracker;

fn call(id: &str, overrides: impl FnOnce(&mut McpCallData)) -> McpCallData {
    let mut call = McpCallData {
        call_id: id.to_owned(),
        tool: "email_search".to_owned(),
        title: "Search Email".to_owned(),
        recommended_policy: RecommendedPolicy::Allow,
        read_only: true,
        started_at: 0,
        finished_at: None,
        duration_ms: None,
        status: McpCallStatus::Ok,
        error: None,
        input: JsValue::Object(Default::default()),
        output: JsValue::Null,
        result_bytes: None,
        structured_bytes: None,
        extra: Extra::default(),
    };
    overrides(&mut call);
    call
}

async fn all_calls(store: &Store) -> Vec<McpCallData> {
    store
        .read(|docs| docs.get_all::<McpCallData>())
        .await
        .unwrap()
}

#[test]
fn caps_strings_and_arrays_and_hides_secret_looking_keys() {
    let bounded = bound_value(
        &json!({
            "prompt": "x".repeat(10),
            "items": (0..22).collect::<Vec<_>>(),
            "apiKey": "secret",
            "nested": {"authorization": "Bearer abc", "keep": true},
        }),
        4,
    );
    let mut items: Vec<Value> = (0..20).map(|i| json!(i)).collect();
    items.push(json!("… [2 more items]"));
    assert_eq!(
        bounded,
        json!({
            "prompt": "xxxx… [truncated 6 chars]",
            "items": items,
            "apiKey": "[redacted]",
            "nested": {"authorization": "[redacted]", "keep": true},
        })
    );
    let deep = json!({"a": {"b": {"c": {"d": {"e": {"f": {"g": 1}}}}}}});
    assert_eq!(
        bound_value(&deep, 10),
        json!({"a": {"b": {"c": {"d": {"e": {"f": "[nested]"}}}}}})
    );
}

#[tokio::test(start_paused = true)]
async fn records_successful_claude_calls_with_their_output_and_others_without() {
    let clock = clock();
    let db = test_store(&clock).await;
    let recorder = ActivityRecorder::new(db.store.clone(), clock.clone(), TaskTracker::new());
    let claude = recorder
        .start(
            meta("claude_session_start"),
            &json!({"project": "omni", "prompt": "Do it", "idempotencyKey": "k"}),
        )
        .await;
    claude
        .finish(CallOutcome::Ok(Some(
            json!({"session": {"sessionId": "abc"}, "reused": false})
                .as_object()
                .cloned()
                .unwrap(),
        )))
        .await;
    let email = recorder
        .start(meta("email_search"), &json!({"query": "invoice"}))
        .await;
    email
        .finish(CallOutcome::Ok(Some(
            json!({"messages": ["private"]})
                .as_object()
                .cloned()
                .unwrap(),
        )))
        .await;
    let calls = all_calls(&db.store).await;
    let claude = calls
        .iter()
        .find(|c| c.tool == "claude_session_start")
        .unwrap();
    assert_eq!(claude.status, McpCallStatus::Ok);
    assert_eq!(
        claude.recommended_policy,
        RecommendedPolicy::RequireApproval
    );
    assert_eq!(
        omni_mcp::events::persistence::js_to_json(&claude.input),
        json!({"project": "omni", "prompt": "Do it", "idempotencyKey": "k"})
    );
    assert_eq!(
        omni_mcp::events::persistence::js_to_json(&claude.output),
        json!({"session": {"sessionId": "abc"}, "reused": false})
    );
    assert!(claude.duration_ms.unwrap() >= 0);
    let email = calls.iter().find(|c| c.tool == "email_search").unwrap();
    assert_eq!(email.status, McpCallStatus::Ok);
    assert_eq!(
        omni_mcp::events::persistence::js_to_json(&email.input),
        json!({"query": "invoice"})
    );
    assert_eq!(email.output, JsValue::Null);
}

#[tokio::test(start_paused = true)]
async fn stylist_calls_keep_only_identifiers() {
    let clock = clock();
    let db = test_store(&clock).await;
    let recorder = ActivityRecorder::new(db.store.clone(), clock.clone(), TaskTracker::new());
    let call = recorder
        .start(
            meta("stylist_record_save"),
            &json!({
                "namespace": "real",
                "idempotencyKey": "onboarding-chest-0001",
                "record": {"kind": "measurement", "dimension": "chest", "value": 101.5, "unit": "cm"},
                "correctionReason": "re-measured after breakfast",
            }),
        )
        .await;
    call.finish(CallOutcome::Ok(None)).await;
    let evidence = recorder
        .start(
            meta("stylist_evidence_store"),
            &json!({"namespace": "real", "idempotencyKey": "photo-front-0001", "mimeType": "image/jpeg",
                    "dataBase64": "/9j/4AAQ", "info": {"kind": "body_photo", "caption": "front"}}),
        )
        .await;
    evidence.finish(CallOutcome::Ok(None)).await;
    let rejected = recorder
        .start(
            meta("stylist_event_append"),
            &json!({"namespace": "real", "idempotencyKey": "receipt-0000000001"}),
        )
        .await;
    rejected
        .finish(CallOutcome::Error(
            "[duplicate_receipt] order 4471-ABC line 2 at Uniqlo is already recorded".to_owned(),
        ))
        .await;
    let calls = all_calls(&db.store).await;
    let save = calls
        .iter()
        .find(|c| c.tool == "stylist_record_save")
        .unwrap();
    assert_eq!(
        omni_mcp::events::persistence::js_to_json(&save.input),
        json!({
            "namespace": "real",
            "idempotencyKey": "onboarding-chest-0001",
            "record": {"kind": "measurement"},
            "correctionReason": "[private]",
        })
    );
    let photo = calls
        .iter()
        .find(|c| c.tool == "stylist_evidence_store")
        .unwrap();
    assert_eq!(
        omni_mcp::events::persistence::js_to_json(&photo.input),
        json!({"namespace": "real", "idempotencyKey": "photo-front-0001", "mimeType": "image/jpeg",
               "dataBase64": "[private]", "info": "[private]"})
    );
    let event = calls
        .iter()
        .find(|c| c.tool == "stylist_event_append")
        .unwrap();
    assert_eq!(event.error.as_deref(), Some("[duplicate_receipt]"));
}

#[tokio::test(start_paused = true)]
async fn print_calls_record_inline_pdf_bytes_only_by_length() {
    let clock = clock();
    let db = test_store(&clock).await;
    let recorder = ActivityRecorder::new(db.store.clone(), clock.clone(), TaskTracker::new());
    let call = recorder
        .start(
            meta("print_document"),
            &json!({"pdfBase64": "JVBERi0xLjQK", "jobName": "Return sheet", "copies": 1}),
        )
        .await;
    call.finish(CallOutcome::Ok(None)).await;
    let calls = all_calls(&db.store).await;
    assert_eq!(
        omni_mcp::events::persistence::js_to_json(&calls[0].input),
        json!({"pdfBase64": "[pdf: 12 base64 chars]", "jobName": "Return sheet", "copies": 1})
    );
}

#[tokio::test(start_paused = true)]
async fn records_failures_without_changing_them() {
    let clock = clock();
    let db = test_store(&clock).await;
    let recorder = ActivityRecorder::new(db.store.clone(), clock.clone(), TaskTracker::new());
    let recorded = recorder
        .start(meta("claude_session_send"), &json!({"session": "abcd"}))
        .await;
    recorded
        .finish(CallOutcome::Error(
            "The Mac is offline (offline)".to_owned(),
        ))
        .await;
    let calls = all_calls(&db.store).await;
    assert_eq!(calls[0].status, McpCallStatus::Error);
    assert_eq!(
        calls[0].error.as_deref(),
        Some("The Mac is offline (offline)")
    );
}

#[tokio::test(start_paused = true)]
async fn marks_calls_left_running_by_a_restart_as_interrupted() {
    let clock = clock();
    let db = test_store(&clock).await;
    let left = call("left", |c| c.status = McpCallStatus::Running);
    db.store
        .write(move |tx| tx.upsert(&left, UpsertOpts::default()))
        .await
        .unwrap();
    assert_eq!(
        mark_interrupted_calls(&db.store, clock.now_ms())
            .await
            .unwrap(),
        1
    );
    let activity = omni_mcp::activity::get_mcp_activity(
        &db.store,
        &ActivityQuery {
            limit: 10,
            ..ActivityQuery::default()
        },
        clock.now_ms(),
    )
    .await
    .unwrap();
    assert_eq!(activity.calls[0].call_id, "left");
    assert_eq!(activity.calls[0].status, McpCallStatus::Interrupted);
    assert_eq!(
        activity.calls[0].error.as_deref(),
        Some("Omni restarted before the call finished")
    );
}

#[tokio::test(start_paused = true)]
async fn a_dropped_call_is_recorded_as_interrupted() {
    let clock = clock();
    let db = test_store(&clock).await;
    let tracker = TaskTracker::new();
    let recorder = ActivityRecorder::new(db.store.clone(), clock.clone(), tracker.clone());
    let recorded = recorder
        .start(meta("claude_session_get"), &json!({"session": "abcd"}))
        .await;
    drop(recorded);
    tracker.close();
    tracker.wait().await;
    let calls = all_calls(&db.store).await;
    assert_eq!(calls[0].status, McpCallStatus::Interrupted);
    assert_eq!(
        calls[0].error.as_deref(),
        Some("The MCP request was cancelled")
    );
}

#[tokio::test(start_paused = true)]
async fn keeps_the_newest_two_thousand_calls() {
    let clock = clock();
    let db = test_store(&clock).await;
    let rows: Vec<McpCallData> = (0..2_100)
        .map(|i| call(&format!("c{i}"), |c| c.started_at = i))
        .collect();
    db.store
        .write(move |tx| {
            for row in &rows {
                tx.upsert(row, UpsertOpts::default())?;
            }
            Ok::<_, omni_store::StoreError>(())
        })
        .await
        .unwrap();
    assert_eq!(prune_calls(&db.store).await.unwrap(), 0);
    let extra = call("c2100", |c| c.started_at = 2_100);
    db.store
        .write(move |tx| tx.upsert(&extra, UpsertOpts::default()))
        .await
        .unwrap();
    assert_eq!(prune_calls(&db.store).await.unwrap(), 101);
    let calls = all_calls(&db.store).await;
    assert_eq!(calls.len(), 2_000);
    assert!(calls.iter().all(|c| c.started_at >= 101));
}

mod summary {
    use super::*;

    const NOW: i64 = 10 * 24 * 60 * 60 * 1000;

    fn stored() -> Vec<McpCallData> {
        vec![
            call("a", |c| {
                c.started_at = NOW - 1_000;
                c.duration_ms = Some(100);
            }),
            call("b", |c| {
                c.started_at = NOW - 2_000;
                c.status = McpCallStatus::Error;
                c.duration_ms = Some(300);
            }),
            call("c", |c| {
                c.tool = "claude_session_start".to_owned();
                c.title = "Start".to_owned();
                c.recommended_policy = RecommendedPolicy::RequireApproval;
                c.started_at = NOW - 3_000;
                c.status = McpCallStatus::Running;
            }),
            call("d", |c| {
                c.started_at = NOW - 3 * 24 * 60 * 60 * 1000;
                c.duration_ms = Some(200);
            }),
        ]
    }

    fn ids(response: &omni_api::mcp_activity::McpActivityResponse) -> Vec<&str> {
        response.calls.iter().map(|c| c.call_id.as_str()).collect()
    }

    fn query(limit: usize) -> ActivityQuery {
        ActivityQuery {
            limit,
            ..ActivityQuery::default()
        }
    }

    #[test]
    fn pages_newest_first_and_filters_by_tool_status_and_prefix() {
        let stored = stored();
        let page = summarize_calls(&stored, &query(2), NOW);
        assert_eq!(ids(&page), ["a", "b"]);
        assert_eq!(page.next_before, Some(NOW - 2_000));
        let next = summarize_calls(
            &stored,
            &ActivityQuery {
                before: page.next_before,
                ..query(2)
            },
            NOW,
        );
        assert_eq!(ids(&next), ["c", "d"]);
        assert_eq!(next.next_before, None);
        let errors = summarize_calls(
            &stored,
            &ActivityQuery {
                status: Some(McpCallStatus::Error),
                ..query(10)
            },
            NOW,
        );
        assert_eq!(ids(&errors), ["b"]);
        let claude = summarize_calls(
            &stored,
            &ActivityQuery {
                tool_prefix: Some("claude_".to_owned()),
                ..query(10)
            },
            NOW,
        );
        assert_eq!(ids(&claude), ["c"]);
    }

    #[test]
    fn aggregates_the_last_day_and_per_tool_statistics() {
        let response = summarize_calls(&stored(), &query(10), NOW);
        assert_eq!(
            serde_json::to_value(response.summary).unwrap(),
            json!({"stored": 4, "last24h": 3, "errors24h": 1, "running": 1, "approvalCalls24h": 1})
        );
        assert_eq!(response.tools.len(), 2);
        assert_eq!(response.tools[0].tool, "email_search");
        assert_eq!(response.tools[0].calls, 3);
        assert_eq!(response.tools[0].errors, 1);
        assert_eq!(response.tools[0].avg_duration_ms, Some(200));
        assert_eq!(response.tools[1].tool, "claude_session_start");
        assert_eq!(response.tools[1].calls, 1);
        assert_eq!(response.tools[1].avg_duration_ms, None);
    }
}

struct Host;

impl ClaudeHost for Host {
    fn status(&self) -> HostLinkStatus {
        HostLinkStatus {
            online: true,
            disabled: false,
            host: Some("studio".to_owned()),
            last_seen_at: Some("2026-10-09T00:00:00.000Z".to_owned()),
            pending_jobs: 0,
        }
    }

    fn execute(
        &self,
        command: HostCommand,
        args: Map<String, Value>,
        _timeout: Duration,
    ) -> BoxFuture<'_, Result<Map<String, Value>, HostError>> {
        let result = match command {
            HostCommand::List => Ok(
                json!({"sessions": [{"session_id": "abc", "status": "idle", "cwd": "/Users/michael/x"}]}),
            ),
            HostCommand::Read => Ok(json!({
                "session_id": args["session"],
                "items": [{"index": 0, "kind": "user", "text": "hi"}],
                "revision": 2,
                "next_cursor": 1,
                "has_more": true,
            })),
            HostCommand::Projects => Err(HostError::new(
                "offline",
                "The Claude Code host is offline",
                true,
            )),
            _ => Err(HostError::new("bad_output", "malformed", false)),
        };
        Box::pin(async move { result.map(|v| v.as_object().cloned().unwrap()) })
    }
}

fn get(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

#[tokio::test(start_paused = true)]
async fn activity_routes_serve_mcp_and_claude_views() {
    let clock = clock();
    let db = test_store(&clock).await;
    let rows = vec![
        call("e", |c| c.started_at = clock.now_ms() - 10),
        call("cl", |c| {
            c.tool = "claude_sessions_list".to_owned();
            c.started_at = clock.now_ms() - 5;
        }),
    ];
    db.store
        .write(move |tx| {
            for row in &rows {
                tx.upsert(row, UpsertOpts::default())?;
            }
            Ok::<_, omni_store::StoreError>(())
        })
        .await
        .unwrap();
    let router = activity_routes::router(db.store.clone(), clock.clone(), Some(Arc::new(Host)));
    let activity = send(&router, get("/api/mcp/activity?limit=abc&status=bogus")).await;
    assert_eq!(activity.status, StatusCode::OK);
    let body: Value = serde_json::from_str(&activity.text).unwrap();
    assert_eq!(body["calls"].as_array().unwrap().len(), 2);
    assert_eq!(body["retention"], json!({"maxCalls": 2000}));

    let claude: Value =
        serde_json::from_str(&send(&router, get("/api/claude/activity")).await.text).unwrap();
    assert_eq!(claude["link"]["host"], "studio");
    assert_eq!(claude["actions"].as_array().unwrap().len(), 1);

    let sessions: Value = serde_json::from_str(
        &send(&router, get("/api/claude/sessions?includeStopped=true"))
            .await
            .text,
    )
    .unwrap();
    assert_eq!(sessions["sessions"][0]["cwd"], "/Users/michael/x");

    let bad = send(&router, get("/api/claude/sessions/ab/transcript")).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_str::<Value>(&bad.text).unwrap(),
        json!({"error": "Invalid session id", "code": "bad_request"})
    );
    let transcript: Value = serde_json::from_str(
        &send(
            &router,
            get("/api/claude/sessions/abcd-1234/transcript?cursor=0"),
        )
        .await
        .text,
    )
    .unwrap();
    assert_eq!(transcript["sessionId"], "abcd-1234");
    assert_eq!(transcript["nextCursor"], 1);
    assert_eq!(transcript["hasMore"], true);

    let projects = send(&router, get("/api/claude/projects")).await;
    assert_eq!(projects.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        serde_json::from_str::<Value>(&projects.text).unwrap(),
        json!({"error": "The Claude Code host is offline", "code": "offline"})
    );

    let unconfigured = activity_routes::router(db.store.clone(), clock.clone(), None);
    let missing = send(&unconfigured, get("/api/claude/projects")).await;
    assert_eq!(missing.status, StatusCode::SERVICE_UNAVAILABLE);
    let link: Value =
        serde_json::from_str(&send(&unconfigured, get("/api/claude/activity")).await.text).unwrap();
    assert_eq!(
        link["link"],
        json!({"configured": false, "online": false, "disabled": false, "host": null, "lastSeenAt": null, "pendingJobs": 0})
    );
}
