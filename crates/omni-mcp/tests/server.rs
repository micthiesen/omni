//! The protocol layer `/gateway/mcp` serves, for one gateway principal
//! (authentication is covered by the gateway tests).
//!
//! Tools owned by other packages (email attachments, Hister, podcasts,
//! printer) are served here by closure-backed handlers: the cases check the
//! protocol layer this package owns (listing, input validation and
//! defaults, custom content, failed results). Email handler behavior is
//! tested in `omni-email`. The endpoint is stateless, so an
//! initialize/initialized handshake must leave nothing behind.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use omni_api::tasks::TaskInfo;
use omni_core::clock::SharedClock;
use omni_mcp::tools::system::{ConfiguredFeatures, SystemDeps, system_tools};
use omni_mcp_kit::{McpTool, ToolError, ToolOutput, raw_tool};
use omni_runtime::ports::Ports;
use serde_json::{Map, Value, json};

fn system(tasks: FakeTasks, clock: SharedClock) -> Vec<McpTool> {
    system_tools(&SystemDeps {
        tasks: Arc::new(tasks),
        ports: Ports::default(),
        features: ConfiguredFeatures::default(),
        clock,
    })
    .unwrap()
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

fn fn_tool<F>(name: &str, f: F) -> McpTool
where
    F: Fn(Value) -> Result<ToolOutput, ToolError> + Send + Sync + 'static,
{
    raw_tool(def(name), Arc::new(FnTool(f))).unwrap()
}

async fn list_tools(router: &axum::Router) -> Vec<Value> {
    let exchange = send(
        router,
        legacy(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})),
    )
    .await;
    exchange.message()["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn delivers_private_pdf_resource_bytes_through_the_authenticated_mcp_protocol() {
    let clock = clock();
    let store = test_store(&clock).await;
    let blob = "JVBERi0xLjcKcHJpdmF0ZSBzeW50aGV0aWMgZml4dHVyZQ==";
    let attachment = fn_tool("email_attachment_get", move |_| {
        Ok(ToolOutput::Custom {
            structured: object(json!({
                "messageId": "<fixture@example.test>",
                "attachmentId": "fixture",
                "filename": "fixture.pdf",
                "mimeType": "application/pdf",
                "size": 34,
                "sha256": "x",
                "blob": blob,
            })),
            content: vec![object(json!({
                "type": "resource",
                "resource": {
                    "uri": "omni-email-attachment:fixture",
                    "mimeType": "application/pdf",
                    "blob": blob,
                }
            }))],
        })
    });
    let router = mcp_router(&store.store, &clock, vec![attachment], None);
    let message = call_tool(
        &router,
        "email_attachment_get",
        json!({"messageId": "<fixture@example.test>", "attachmentId": "fixture"}),
    )
    .await;
    assert!(!is_error(&message));
    assert_eq!(
        result(&message)["content"][0],
        json!({
            "type": "resource",
            "resource": {"uri": "omni-email-attachment:fixture", "mimeType": "application/pdf", "blob": blob}
        })
    );
    assert_eq!(
        result(&message)["structuredContent"]["filename"],
        "fixture.pdf"
    );
    assert_eq!(result(&message)["structuredContent"]["size"], 34);
}

#[tokio::test]
async fn serves_bounded_hister_reads_and_rejects_invalid_tool_input_over_mcp() {
    let clock = clock();
    let store = test_store(&clock).await;
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let page = fn_tool("get_browser_page", move |input| {
        counter.fetch_add(1, Ordering::SeqCst);
        let max = input["maxChars"].as_u64().unwrap() as usize;
        let text = "saved page content";
        Ok(ToolOutput::Structured(object(json!({
            "title": "Article",
            "titleTruncated": false,
            "url": input["url"],
            "documentId": null,
            "text": &text[..max],
            "totalChars": text.len(),
            "offset": 0,
            "nextOffset": max,
        }))))
    });
    let router = mcp_router(&store.store, &clock, vec![page], None);
    let ok = call_tool(
        &router,
        "get_browser_page",
        json!({"url": "https://example.test/article", "maxChars": 5}),
    )
    .await;
    assert!(!is_error(&ok));
    let structured = &result(&ok)["structuredContent"];
    assert_eq!(structured["text"], "saved");
    assert_eq!(structured["nextOffset"], 5);
    assert_eq!(structured["totalChars"], 18);
    let invalid = call_tool(
        &router,
        "get_browser_page",
        json!({"url": "https://example.test/article", "maxChars": 50_001}),
    )
    .await;
    assert!(is_error(&invalid));
    assert!(error_text(&invalid).starts_with(
        "Input validation error: Invalid arguments for tool get_browser_page: maxChars:"
    ));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    let tools = list_tools(&router).await;
    let label = tools
        .iter()
        .find(|tool| tool["name"] == "set_browser_page_label")
        .unwrap();
    assert_eq!(label["annotations"]["readOnlyHint"], false);
    assert_eq!(label["annotations"]["idempotentHint"], true);
}

#[tokio::test]
async fn reports_an_unconfigured_hister_integration_as_a_tool_error() {
    let clock = clock();
    let store = test_store(&clock).await;
    let search = fn_tool("search_browser_history", |_| {
        Err(ToolError::execute("Hister is not configured"))
    });
    let router = mcp_router(&store.store, &clock, vec![search], None);
    let message = call_tool(
        &router,
        "search_browser_history",
        json!({"query": "router"}),
    )
    .await;
    assert!(is_error(&message));
    assert!(
        message["result"]["content"]
            .to_string()
            .contains("not configured")
    );
}

#[tokio::test]
async fn serves_over_real_http_with_clean_shutdown() {
    let clock = clock();
    let store = test_store(&clock).await;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let serve = |router: axum::Router| {
        let shutdown = shutdown.clone();
        async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let handle = tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(async move { shutdown.cancelled().await })
                    .await
                    .unwrap();
            });
            (format!("http://{address}/gateway/mcp"), handle)
        }
    };
    let http = omni_testkit::no_network();
    let (url, configured) = serve(mcp_router(&store.store, &clock, Vec::new(), None)).await;
    let listed = http
        .request(
            omni_http::Method::POST,
            omni_http::Url::parse(&url).unwrap(),
        )
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}).to_string())
        .send_bounded(4 * 1024 * 1024)
        .await
        .unwrap();
    let exchange = Exchange {
        status: StatusCode::from_u16(listed.status.as_u16()).unwrap(),
        headers: axum::http::HeaderMap::new(),
        text: String::from_utf8_lossy(&listed.body).into_owned(),
    };
    assert_eq!(
        exchange.message()["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        omni_mcp::tools::TOOL_ORDER.len()
    );
    shutdown.cancel();
    configured.await.unwrap();
}

#[tokio::test]
async fn initializes_and_lists_the_complete_typed_tool_surface_with_the_official_client() {
    let clock = clock();
    let store = test_store(&clock).await;
    let router = mcp_router(&store.store, &clock, Vec::new(), None);
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "omni-mcp-spec", "version": "1.0.0"},
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let discover = send(
        &router,
        request(
            &json!({"jsonrpc": "2.0", "id": "probe", "method": "server/discover", "params": {"_meta": meta}}),
            &[("mcp-protocol-version", "2026-07-28"), ("mcp-method", "server/discover")],
        ),
    )
    .await;
    let discovered = discover.message();
    assert_eq!(
        discovered["result"]["supportedVersions"],
        json!(["2026-07-28"])
    );
    assert_eq!(
        discovered["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "omni"
    );
    let golden = omni_mcp_kit::golden::handshake().unwrap();
    let instructions = discovered["result"]["instructions"].as_str().unwrap();
    assert!(instructions.starts_with("Omni is the owner's MCP gateway"));
    assert!(
        instructions.ends_with(
            golden["legacy"]["initialize"]["instructions"]
                .as_str()
                .unwrap()
        )
    );
    assert!(!instructions.contains("Executor"));
    let listed = send(
        &router,
        request(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": meta}}),
            &[
                ("mcp-protocol-version", "2026-07-28"),
                ("mcp-method", "tools/list"),
            ],
        ),
    )
    .await;
    let tools = listed.message()["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(tools.len(), omni_mcp::tools::TOOL_ORDER.len());
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in ["email_search", "email_draft_create", "email_send"] {
        assert!(names.contains(&expected));
    }
    for tool in &tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["outputSchema"]["type"], "object");
        let annotations = tool["annotations"].as_object().unwrap();
        assert_eq!(annotations.len(), 4);
        for key in [
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ] {
            assert!(annotations[key].is_boolean());
        }
    }
}

#[tokio::test]
async fn executes_representative_read_and_mocked_mutation_calls() {
    let clock = clock();
    let store = test_store(&clock).await;
    let tasks = Arc::new(FakeTasks::default());
    let own = system_tools(&SystemDeps {
        tasks: tasks.clone(),
        ports: Ports::default(),
        features: ConfiguredFeatures::default(),
        clock: clock.clone(),
    })
    .unwrap();
    let router = mcp_router(&store.store, &clock, own, None);
    let read = call_tool(&router, "tasks_list", json!({})).await;
    assert!(!is_error(&read));
    assert_eq!(
        result(&read)["structuredContent"],
        json!({"tasks": [], "nextCursor": null, "total": 0})
    );
    let mutation = call_tool(&router, "task_run", json!({"taskName": "MockTask"})).await;
    assert!(!is_error(&mutation));
    assert_eq!(
        result(&mutation)["structuredContent"],
        json!({"runId": "mock-run-123", "taskName": "MockTask", "queued": true})
    );
    assert_eq!(
        result(&mutation)["content"][0]["text"],
        r#"{"runId":"mock-run-123","taskName":"MockTask","queued":true}"#
    );
    assert_eq!(
        *tasks.run_now_calls.lock().unwrap(),
        vec![("MockTask".to_owned(), None)]
    );
}

#[tokio::test]
async fn paginates_reads_rejects_malformed_bounds_and_preserves_policy_semantics() {
    let clock = clock();
    let store = test_store(&clock).await;
    let tasks = FakeTasks {
        tasks: ["Alpha", "Beta", "Gamma"]
            .iter()
            .map(|name| TaskInfo {
                name: (*name).to_owned(),
                display_name: None,
                schedule: "0 * * * *".to_owned(),
                running: false,
                next_runs: Vec::new(),
                last_run: None,
            })
            .collect(),
        ..FakeTasks::default()
    };
    let router = mcp_router(&store.store, &clock, system(tasks, clock.clone()), None);
    let first = call_tool(&router, "tasks_list", json!({"cursor": 0, "limit": 2})).await;
    let structured = &result(&first)["structuredContent"];
    assert_eq!(structured["total"], 3);
    assert_eq!(structured["nextCursor"], 2);
    assert_eq!(structured["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(structured["tasks"][0]["displayName"], Value::Null);
    let second = call_tool(&router, "tasks_list", json!({"cursor": 2, "limit": 2})).await;
    assert_eq!(
        result(&second)["structuredContent"]["nextCursor"],
        Value::Null
    );
    assert_eq!(
        result(&second)["structuredContent"]["tasks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let malformed = call_tool(
        &router,
        "tasks_list",
        json!({"cursor": -1, "limit": 101, "unexpected": true}),
    )
    .await;
    assert!(is_error(&malformed));
    let unbounded = call_tool(&router, "costs_read", json!({"days": "all"})).await;
    assert!(is_error(&unbounded));
    assert!(error_text(&unbounded).starts_with("Input validation error"));

    let tools = list_tools(&router).await;
    let task_run = tools
        .iter()
        .find(|tool| tool["name"] == "task_run")
        .unwrap();
    assert_eq!(
        task_run["annotations"],
        json!({"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true})
    );
    let policy = &meta("task_run").policy;
    assert_eq!(
        policy.recommended_policy,
        omni_mcp_kit::ApprovalPolicy::RequireApproval
    );
    assert!(
        policy
            .side_effects
            .iter()
            .any(|s| s == "Queues task execution")
    );
    assert!(meta("email_rules_delete").annotations.destructive_hint);
}

#[tokio::test]
async fn runs_a_mocked_consequential_podcast_account_mutation_without_external_effects() {
    let clock = clock();
    let store = test_store(&clock).await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let update = fn_tool("podcast_account_update", move |input| {
        seen.lock().unwrap().push(input.clone());
        Ok(ToolOutput::Structured(object(json!({
            "account": "MockCastro",
            "action": input["action"],
            "result": "removed",
        }))))
    });
    let router = mcp_router(&store.store, &clock, vec![update], None);
    let message = call_tool(
        &router,
        "podcast_account_update",
        json!({"action": "dequeue", "episodeGuid": "episode-guid-1"}),
    )
    .await;
    assert!(!is_error(&message));
    assert_eq!(
        result(&message)["structuredContent"],
        json!({"account": "MockCastro", "action": "dequeue", "result": "removed"})
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec![json!({"action": "dequeue", "episodeGuid": "episode-guid-1"})]
    );
    assert_eq!(
        meta("podcast_account_update").policy.recommended_policy,
        omni_mcp_kit::ApprovalPolicy::RequireApproval
    );
}

#[tokio::test]
async fn exposes_guarded_printer_status_and_physical_printing_through_a_mocked_service() {
    let clock = clock();
    let store = test_store(&clock).await;
    let prints = Arc::new(Mutex::new(Vec::new()));
    let seen = prints.clone();
    let status = fn_tool("get_printer_status", |_| {
        Ok(ToolOutput::Structured(object(json!({
            "configured": true,
            "name": "Brother HL-L2370DW series",
            "uri": "ipp://printer.test/ipp/print",
            "state": "idle",
            "stateReasons": [],
            "ready": true,
            "acceptingJobs": true,
            "queuedJobCount": 0,
            "tonerPercent": 20,
            "monochromeOnly": true,
            "defaultSides": "two-sided-long-edge",
            "supportedFormats": ["application/octet-stream"],
            "supportedMedia": ["na_letter_8.5x11in"],
        }))))
    });
    let print = fn_tool("print_document", move |input| {
        seen.lock().unwrap().push(input.clone());
        Ok(ToolOutput::Structured(object(json!({
            "accepted": true,
            "completed": true,
            "jobId": 42,
            "jobUri": "ipp://printer.test/jobs/42",
            "jobState": "completed",
            "jobName": "Test document",
            "pages": 2,
            "copies": 1,
            "paper": "letter",
            "sides": "two-sided-long-edge",
            "impressionsCompleted": 2,
            "message": "The printer completed the job successfully",
        }))))
    });
    let router = mcp_router(&store.store, &clock, vec![status, print], None);
    let read = call_tool(&router, "get_printer_status", json!({})).await;
    assert!(!is_error(&read));
    assert_eq!(result(&read)["structuredContent"]["tonerPercent"], 20);
    let printed = call_tool(
        &router,
        "print_document",
        json!({"url": "https://example.com/document.pdf", "jobName": "Test document"}),
    )
    .await;
    assert!(!is_error(&printed));
    assert_eq!(result(&printed)["structuredContent"]["jobId"], 42);
    assert_eq!(
        prints.lock().unwrap()[0],
        json!({
            "url": "https://example.com/document.pdf",
            "jobName": "Test document",
            "copies": 1,
            "paper": "letter",
            "sides": "two-sided-long-edge",
            "allowDuplicate": false,
        })
    );
    let private = call_tool(
        &router,
        "print_document",
        json!({"pdfBase64": "JVBERi0xLjQK", "copies": 1}),
    )
    .await;
    assert!(!is_error(&private));
    assert_eq!(prints.lock().unwrap()[1]["pdfBase64"], "JVBERi0xLjQK");
    let annotations = meta("print_document").annotations;
    assert!(!annotations.read_only_hint && !annotations.destructive_hint);
    assert!(!annotations.idempotent_hint && annotations.open_world_hint);
    let malformed = call_tool(
        &router,
        "print_document",
        json!({"url": "file:///tmp/document.pdf", "copies": 99}),
    )
    .await;
    assert!(is_error(&malformed));
    assert_eq!(prints.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn reports_printer_tools_as_unavailable_when_printing_is_not_configured() {
    let clock = clock();
    let store = test_store(&clock).await;
    let status = fn_tool("get_printer_status", |_| {
        Err(ToolError::execute("Printer is not configured"))
    });
    let router = mcp_router(&store.store, &clock, vec![status], None);
    let message = call_tool(&router, "get_printer_status", json!({})).await;
    assert!(is_error(&message));
    assert_eq!(
        result(&message)["content"],
        json!([{"type": "text", "text": "Printer is not configured"}])
    );
}

#[tokio::test]
async fn closes_cleanly_after_initialization() {
    let clock = clock();
    let store = test_store(&clock).await;
    let router = mcp_router(&store.store, &clock, Vec::new(), None);
    let init = send(
        &router,
        request(
            &json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}
            }}),
            &[],
        ),
    )
    .await;
    assert_eq!(init.message()["result"]["protocolVersion"], "2025-06-18");
    let initialized = send(
        &router,
        legacy(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
    )
    .await;
    assert_eq!(initialized.status, StatusCode::ACCEPTED);
    let deleted = send(
        &router,
        Request::builder()
            .method("DELETE")
            .uri("/gateway/mcp")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn a_panicking_tool_becomes_a_failed_result() {
    let clock = clock();
    let store = test_store(&clock).await;
    let status = fn_tool("get_printer_status", |_| {
        panic!("private implementation failure")
    });
    let router = mcp_router(&store.store, &clock, vec![status], None);
    let message = call_tool(&router, "get_printer_status", json!({})).await;
    assert!(is_error(&message));
    assert_eq!(error_text(&message), "private implementation failure");
}

#[tokio::test]
async fn a_failed_tagged_union_names_the_field_in_the_selected_alternative() {
    let clock = clock();
    let store = test_store(&clock).await;
    let router = mcp_router(&store.store, &clock, Vec::new(), None);
    let short = call_tool(
        &router,
        "stylist_record_save",
        json!({"namespace": "fixture-a", "idempotencyKey": "union-error-0000001",
               "record": {"kind": "measurement", "subject": {"type": "body"}, "dimension": "chest",
                          "method": "circumference", "endpoints": "x", "value": 100, "unit": "cm",
                          "observedOn": "2026-10-01",
                          "provenance": {"certainty": "explicit", "source": "user_measurement"}}}),
    )
    .await;
    assert!(is_error(&short));
    let text = error_text(&short);
    assert!(
        text.starts_with(
            "Input validation error: Invalid arguments for tool stylist_record_save: record.endpoints:"
        ),
        "{text}"
    );
    assert!(!text.contains("valid under any"), "{text}");
    let unknown = call_tool(
        &router,
        "stylist_event_append",
        json!({"namespace": "fixture-a", "idempotencyKey": "union-error-0000002",
               "occurredOn": "2026-10-01", "event": {"type": "spin_cycle"}}),
    )
    .await;
    let text = error_text(&unknown);
    assert!(
        text.contains("event.type: \"spin_cycle\" is not one of acquired, try_on"),
        "{text}"
    );
    let missing = call_tool(
        &router,
        "stylist_event_append",
        json!({"namespace": "fixture-a", "idempotencyKey": "union-error-0000003",
               "occurredOn": "2026-10-01", "event": {"type": "wash", "water": "cold"}}),
    )
    .await;
    let text = error_text(&missing);
    assert!(
        text.contains("event: \"pieceId\" is a required property"),
        "{text}"
    );
}
