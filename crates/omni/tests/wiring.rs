//! The production wiring over a `TestApp` context: every subsystem builds,
//! every port is set, routes merge without conflicts, the MCP endpoint serves
//! the committed tool set, and the data manager lists every managed entity.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use omni::boot;
use omni::data_manager::MANAGED_ORDER;
use omni_testkit::TestApp;
use serde_json::{Value, json};
use tower::ServiceExt as _;

async fn wired() -> (TestApp, omni::wiring::Wired) {
    let app = TestApp::new().await;
    let wired = omni::wiring::wire(&app.ctx, app.ctx.clock.now_ms())
        .await
        .unwrap();
    (app, wired)
}

#[tokio::test]
async fn every_port_is_set_and_email_tasks_need_a_transport() {
    let (app, wired) = wired().await;
    let ports = &app.ctx.ports;
    assert!(ports.archive_echo().is_some(), "ArchiveEcho");
    assert!(ports.email_retry_handlers().is_some(), "EmailRetryHandlers");
    assert!(ports.calendar_connection().is_some(), "CalendarConnection");
    assert!(ports.live_directory().is_some(), "LiveDirectory");
    assert!(ports.on_deck_source().is_some(), "OnDeckSource");
    assert!(
        ports.claude_host().is_some(),
        "ClaudeHost (device link token set)"
    );
    assert!(
        ports.claude_session_notifier().is_some(),
        "ClaudeSessionNotifier (events + device link)"
    );
    assert!(ports.event_publisher().is_some(), "EventPublisher (events)");
    // No iCloud credentials: no reader, no dispatcher, no email tasks.
    assert!(ports.email_reader().is_none());
    assert!(wired.email_handlers.is_empty());
    let names: BTreeSet<String> = wired
        .subsystems
        .iter()
        .flat_map(|s| s.tasks.iter().map(|t| t.name().to_owned()))
        .collect();
    for email_task in ["EmailArchive", "EmailWatchdog", "EmailRetry"] {
        assert!(
            !names.contains(email_task),
            "{email_task} without a transport"
        );
    }
    assert!(names.contains("McpEventDelivery"));
    assert!(names.contains("ClaudeSessionEvents"));
    assert!(names.contains("TaskRunEvents"));
    assert!(names.contains("RemindersSession"));
    // `/api/tasks` lists them in registration order.
    let mut ordered: Vec<String> = names.into_iter().collect();
    ordered.sort_by_key(|n| omni::wiring::task_rank(n));
    assert_eq!(
        ordered.first().map(String::as_str),
        Some("McpEventDelivery")
    );
}

#[tokio::test]
async fn the_entity_catalog_matches_the_wired_subsystems() {
    let (_app, wired) = wired().await;
    let wired_names: BTreeSet<&str> = boot::all_entities(&wired.subsystems)
        .iter()
        .map(|d| d.name)
        .collect();
    let catalog: BTreeSet<&str> = omni::compat_audit::entity_catalog()
        .iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(wired_names, catalog);
}

#[tokio::test]
async fn the_data_manager_lists_every_entity_in_order() {
    let (app, mut wired) = wired().await;
    let (router, ops) = boot::app_router(
        &app.ctx,
        &mut wired.subsystems,
        std::path::Path::new("/nonexistent"),
    );
    assert_eq!(ops.data.slugs(), MANAGED_ORDER.to_vec());
    let (status, body) = app.get_json(&router, "/api/data/entities").await;
    assert_eq!(status, StatusCode::OK);
    let entities = body["entities"].as_array().unwrap();
    assert_eq!(entities.len(), MANAGED_ORDER.len());
    assert_eq!(entities[2]["slug"], "task-run");
    assert_eq!(entities[2]["primaryKey"], json!(["runId"]));
    assert_eq!(
        entities[2]["warning"],
        "Deleting a run also deletes its stored log."
    );
    assert!(entities[3].get("warning").is_none(), "absent, not null");
    assert!(body["storage"]["databaseSizeBytes"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn the_gateway_serves_the_golden_tool_list() {
    let (app, mut wired) = wired().await;
    let (router, _) = boot::app_router(
        &app.ctx,
        &mut wired.subsystems,
        std::path::Path::new("/nonexistent"),
    );
    let secrets = omni_mcp::gateway::secrets::GatewaySecrets::load_or_create(
        &omni_mcp::gateway::secrets::key_path(&app.ctx.paths.data_dir),
    )
    .unwrap();
    let access = omni_mcp::gateway::access::DownstreamAccess::new(
        app.ctx.store.clone(),
        app.ctx.clock.clone(),
        secrets,
        app.ctx.tracker.clone(),
    );
    let (_, token) = access.create_client("wiring").await.unwrap();
    let request = Request::post("/gateway/mcp")
        .header(header::HOST, "localhost")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .header("mcp-protocol-version", "2025-06-18")
        .body(Body::from(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}).to_string(),
        ))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    let message: Value = text
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .map_or_else(
            || serde_json::from_str(&text).unwrap(),
            |data| serde_json::from_str(data.trim()).unwrap(),
        );
    let served: BTreeSet<String> = message["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tools in {message}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    let golden: Value = serde_json::from_str(omni_mcp_kit::golden::TOOLS_LIST_JSON).unwrap();
    let expected: BTreeSet<String> = golden
        .pointer("/modern/tools")
        .or_else(|| golden.get("tools"))
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(!served.is_empty());
    if !expected.is_empty() {
        assert_eq!(served, expected);
    }
}
