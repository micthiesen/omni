//! The assembled subsystem: pet routes, the personal MCP tools, and
//! task/tool/entity registration.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolOutput, ToolPhase};
use omni_personal::pets::persistence::{PetRow, PetStore, WeightHistoryRow};
use omni_testkit::TestApp;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

async fn seed(app: &TestApp) {
    let pets = PetStore::open(&app.ctx.store).await.unwrap();
    pets.upsert_pet(&PetRow {
        pet_id: "PET-1".into(),
        name: "Sandy".into(),
        current_weight: 12.617_988_513_839_7,
        updated_at: "2026-10-09T04:40:02.739Z".into(),
    })
    .await
    .unwrap();
    for (timestamp, weight) in [
        ("2026-03-20T15:39:54", 12.05),
        ("2026-03-20T15:37:39", 11.99),
        ("2026-03-21T03:36:56", 12.0),
    ] {
        pets.insert_weight_reading(&WeightHistoryRow {
            pet_id: "PET-1".into(),
            timestamp: timestamp.into(),
            weight,
        })
        .await
        .unwrap();
    }
}

fn tool<'a>(tools: &'a [McpTool], name: &str) -> &'a McpTool {
    tools.iter().find(|t| t.meta.name == name).unwrap()
}

async fn call(tools: &[McpTool], name: &str, input: Value) -> Result<Value, ToolError> {
    let cx = ToolContext {
        call_id: "1".into(),
        cancel: CancellationToken::new(),
    };
    tool(tools, name)
        .handler
        .call(input, cx)
        .await
        .map(|out| match out {
            ToolOutput::Structured(map)
            | ToolOutput::Custom {
                structured: map, ..
            } => Value::Object(map),
        })
}

#[tokio::test]
async fn registers_tasks_tools_and_entities() {
    let app = TestApp::new().await;
    let subsystem = omni_personal::subsystem(&app.ctx).await.unwrap();
    let tasks: Vec<&str> = subsystem.tasks.iter().map(|t| t.name()).collect();
    assert_eq!(tasks, vec!["CodexResets", "ClaudeResets"]);
    let tools: Vec<&str> = subsystem
        .mcp_tools
        .iter()
        .map(|t| t.meta.name.as_str())
        .collect();
    assert_eq!(
        tools,
        vec![
            "pets_read",
            "costs_read",
            "get_printer_status",
            "print_document",
            "search_browser_history",
            "browse_browser_history",
            "get_browser_page",
            "set_browser_page_label",
            "notify_owner_blocked",
            "notify_owner_requested",
        ]
    );
    let entities: Vec<&str> = subsystem.entities.iter().map(|e| e.name).collect();
    assert_eq!(
        entities,
        vec![
            "codex-reset-delivery",
            "claude-reset-delivery",
            "printer-accepted-job",
            "pet-health-alert",
            "pet-health-dismissal",
            "owner-notification"
        ]
    );
    assert_eq!(subsystem.alert_gates.len(), 2);
    assert!(subsystem.alert_gates[0].applies("Error running task \"PetTracker\""));
    assert!(!subsystem.alert_gates[0].applies("Error running task \"CodexResets\""));
    assert!(subsystem.alert_gates[1].applies("Error running task \"CodexResets\""));
    assert!(subsystem.alert_gates[1].applies("Manual run of \"ClaudeResets\" failed"));
    assert!(!subsystem.alert_gates[1].applies("Error running task \"PetTracker\""));
}

/// Three readings a day for 30 days, ending 72 hours before the test epoch
/// (2026-01-01T00:00Z), losing weight steadily.
async fn seed_trend(app: &TestApp) {
    let pets = PetStore::open(&app.ctx.store).await.unwrap();
    pets.upsert_pet(&PetRow {
        pet_id: "PET-1".into(),
        name: "Sam".into(),
        current_weight: 13.4,
        updated_at: "2025-12-29T00:00:00.000Z".into(),
    })
    .await
    .unwrap();
    let end = jiff::civil::date(2025, 12, 29).at(0, 0, 0, 0);
    for i in 0..90 {
        let at = end - jiff::SignedDuration::from_hours(8 * i);
        pets.insert_weight_reading(&WeightHistoryRow {
            pet_id: "PET-1".into(),
            timestamp: at.to_string(),
            weight: 13.4 + 0.004 * f64::from(u8::try_from(i).unwrap()),
        })
        .await
        .unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn serves_health_trends_and_the_household_data_gap() {
    let app = TestApp::new().await;
    seed_trend(&app).await;
    let subsystem = omni_personal::subsystem(&app.ctx).await.unwrap();
    let router = app.router(&subsystem);
    let (status, body) = app.get_json(&router, "/api/pets/health?weeks=4").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["generatedAt"], "2026-01-01T00:00:00.000Z");
    assert_eq!(body["latestReadingAt"], "2025-12-29T00:00:00.000Z");
    assert_eq!(body["hoursSinceLatestReading"], 72);
    assert_eq!(body["dataGap"]["kind"], "data-gap");
    assert_eq!(body["alerts"], json!([]));
    let sam = &body["pets"][0];
    assert_eq!(sam["name"], "Sam");
    assert_eq!(sam["weekly"].as_array().unwrap().len(), 4);
    // The newest block ends now and holds the 12 readings of its first four days.
    assert_eq!(sam["weekly"][3]["readings"], 12);
    assert_eq!(sam["changes"][0]["weeks"], 2);
    assert!(sam["changes"][0]["percent"].as_f64().unwrap() < 0.0);
    // The outage, not the cat, explains the low count: no visit finding.
    assert_eq!(sam["findings"], json!([]));

    let (status, body) = app.get_json(&router, "/api/pets/health?weeks=99").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("weeks"));

    let trend = call(
        &subsystem.mcp_tools,
        "pets_read",
        json!({"resource": "trend", "petId": "PET-1", "weeks": 2}),
    )
    .await
    .unwrap();
    assert_eq!(trend["resource"], "trend");
    assert_eq!(trend["pets"][0]["weekly"].as_array().unwrap().len(), 2);
    assert_eq!(trend["dataGap"]["kind"], "data-gap");
    let missing = call(
        &subsystem.mcp_tools,
        "pets_read",
        json!({"resource": "trend", "petId": "PET-2"}),
    )
    .await;
    assert!(missing.is_err());
}

#[tokio::test]
async fn lists_pets_with_rounded_weights_and_daily_visits() {
    let app = TestApp::new().await;
    seed(&app).await;
    let subsystem = omni_personal::subsystem(&app.ctx).await.unwrap();
    let router = app.router(&subsystem);
    let (status, body) = app.get_json(&router, "/api/pets").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!([{
            "petId": "PET-1",
            "name": "Sandy",
            "currentWeight": 12.62,
            "weightHistory": [
                {"timestamp": "2026-03-20T15:37:39", "weight": 11.99},
                {"timestamp": "2026-03-20T15:39:54", "weight": 12.05},
                {"timestamp": "2026-03-21T03:36:56", "weight": 12}
            ],
            "dailyVisits": [{"date": "2026-03-20", "count": 2}, {"date": "2026-03-21", "count": 1}]
        }])
    );
    let raw = router
        .clone()
        .oneshot(Request::get("/api/pets").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(raw.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("\"weight\":12}"));
}

async fn csv(router: &axum::Router, uri: &str) -> (String, String, String) {
    let response = router
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    let (kind, disposition) = (header("content-type"), header("content-disposition"));
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        kind,
        disposition,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

#[tokio::test]
async fn exports_weight_history_as_csv_with_an_optional_day_window() {
    let app = TestApp::new().await;
    seed(&app).await;
    let subsystem = omni_personal::subsystem(&app.ctx).await.unwrap();
    let router = app.router(&subsystem);
    let (kind, disposition, body) = csv(&router, "/api/pets/PET-1/export.csv").await;
    assert_eq!(kind, "text/csv");
    assert_eq!(disposition, "attachment; filename=\"sandy-weight.csv\"");
    assert_eq!(
        body,
        "timestamp,weight_lbs\n2026-03-20T15:37:39,11.99\n2026-03-20T15:39:54,12.05\n2026-03-21T03:36:56,12"
    );
    // TestApp's clock is 2026-01-01T00:00Z: a 1-day window keeps readings at or
    // after 2025-12-31T00:00Z (local times parse in America/Vancouver).
    let pets = PetStore::open(&app.ctx.store).await.unwrap();
    for timestamp in ["2025-12-30T15:00:00", "2025-12-30T17:00:00"] {
        pets.insert_weight_reading(&WeightHistoryRow {
            pet_id: "PET-1".into(),
            timestamp: timestamp.into(),
            weight: 11.5,
        })
        .await
        .unwrap();
    }
    let (_, _, windowed) = csv(&router, "/api/pets/PET-1/export.csv?days=1").await;
    assert_eq!(
        windowed,
        "timestamp,weight_lbs\n2025-12-30T17:00:00,11.5\n2026-03-20T15:37:39,11.99\n2026-03-20T15:39:54,12.05\n2026-03-21T03:36:56,12"
    );
    let (_, _, ignored) = csv(&router, "/api/pets/PET-1/export.csv?days=abc").await;
    assert_eq!(ignored.lines().count(), 6);
    let (_, disposition, unknown) = csv(&router, "/api/pets/nope/export.csv").await;
    assert_eq!(disposition, "attachment; filename=\"nope-weight.csv\"");
    assert_eq!(unknown, "timestamp,weight_lbs");
}

#[tokio::test]
async fn pets_read_lists_bounded_history_and_pages_one_pet() {
    let app = TestApp::new().await;
    seed(&app).await;
    let tools = omni_personal::subsystem(&app.ctx).await.unwrap().mcp_tools;
    let list = call(
        &tools,
        "pets_read",
        json!({"resource": "list", "historyLimit": 2}),
    )
    .await
    .unwrap();
    assert_eq!(list["resource"], json!("list"));
    let pet = &list["pets"][0];
    assert_eq!(pet["currentWeight"], json!(12.617_988_513_839_7));
    assert_eq!(pet["recentWeights"].as_array().unwrap().len(), 2);
    assert_eq!(pet["recentWeights"][1]["weight"], json!(12));
    assert_eq!(pet["recentVisits"].as_array().unwrap().len(), 2);
    // `slice(-0)` returns everything.
    let all = call(
        &tools,
        "pets_read",
        json!({"resource": "list", "historyLimit": 0}),
    )
    .await
    .unwrap();
    assert_eq!(all["pets"][0]["recentWeights"].as_array().unwrap().len(), 3);

    let page = call(
        &tools,
        "pets_read",
        json!({"resource": "history", "petId": "PET-1", "limit": 2}),
    )
    .await
    .unwrap();
    assert_eq!(page["resource"], json!("history"));
    assert_eq!(page["pet"]["name"], json!("Sandy"));
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["nextCursor"], json!(2));
    assert_eq!(page["total"], json!(3));
    let missing = call(
        &tools,
        "pets_read",
        json!({"resource": "history", "petId": "x"}),
    )
    .await
    .unwrap_err();
    assert_eq!(missing.message, "Pet not found");
    let invalid = call(&tools, "pets_read", json!({"resource": "nope"}))
        .await
        .unwrap_err();
    assert_eq!(invalid.phase, ToolPhase::Input);
}

#[tokio::test]
async fn costs_read_summarizes_persisted_cost_events() {
    let app = TestApp::new().await;
    let tools = omni_personal::subsystem(&app.ctx).await.unwrap().mcp_tools;
    let summary = call(&tools, "costs_read", json!({})).await.unwrap();
    assert_eq!(summary["range"]["days"], json!(30));
    assert_eq!(summary["summary"]["eventCount"], json!(0));
    let error = call(&tools, "costs_read", json!({"days": 14}))
        .await
        .unwrap_err();
    assert_eq!(error.phase, ToolPhase::Input);
}

#[tokio::test]
async fn unconfigured_printer_and_hister_fail_at_call_time() {
    let app = TestApp::new().await;
    let tools = omni_personal::subsystem(&app.ctx).await.unwrap().mcp_tools;
    let printer = call(&tools, "get_printer_status", json!({}))
        .await
        .unwrap_err();
    assert_eq!(printer.message, "Printer is not configured");
    let print = call(
        &tools,
        "print_document",
        json!({"url": "https://example.com/a.pdf"}),
    )
    .await
    .unwrap_err();
    assert_eq!(print.message, "Printer is not configured");
    let blank_job = call(
        &tools,
        "print_document",
        json!({"url": "https://example.com/a.pdf", "jobName": "  "}),
    )
    .await
    .unwrap_err();
    assert_eq!(blank_job.phase, ToolPhase::Input);
    let hister = call(&tools, "search_browser_history", json!({"query": "term"}))
        .await
        .unwrap_err();
    assert_eq!(
        hister.message,
        "Hister: not configured; set HISTER_ACCESS_TOKEN"
    );
    let blank = call(&tools, "search_browser_history", json!({"query": "   "}))
        .await
        .unwrap_err();
    assert_eq!(blank.phase, ToolPhase::Input);
    let reversed = call(
        &tools,
        "search_browser_history",
        json!({"query": "x", "dateFrom": "2026-01-02", "dateTo": "2026-01-01"}),
    )
    .await
    .unwrap_err();
    assert_eq!(reversed.message, "dateFrom must not follow dateTo");
    let browse = call(
        &tools,
        "browse_browser_history",
        json!({"dateFrom": 5, "dateTo": 5}),
    )
    .await
    .unwrap_err();
    assert_eq!(browse.message, "dateFrom must precede dateTo");
}
