//! The wiring contract of `McpPackage` over a test `AppContext`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TOKEN, other_tools, send};
use futures::future::BoxFuture;
use omni_config::Config;
use omni_core::email::EmailOrigin;
use omni_mcp::McpPackage;
use omni_runtime::BootPhase;
use omni_runtime::ports::{ArchiveEcho, PortError};
use omni_testkit::TestApp;
use serde_json::json;

fn with_token(app: &TestApp, token: Option<&str>) -> omni_runtime::AppContext {
    let mut env = omni_testkit::test_app_env();
    env.remove("OMNI_MCP_TOKEN");
    env.remove("OMNI_DEVICE_LINK_TOKEN");
    if let Some(token) = token {
        env.insert("OMNI_MCP_TOKEN".to_owned(), token.to_owned());
    }
    let mut ctx = app.ctx.clone();
    ctx.config = Arc::new(Config::from_env(&env).unwrap());
    ctx
}

struct NoMoves;

impl ArchiveEcho for NoMoves {
    fn is_archive_action_message<'a>(
        &'a self,
        _message_id: &'a str,
        _origin: Option<&'a EmailOrigin>,
    ) -> BoxFuture<'a, Result<bool, PortError>> {
        Box::pin(async { Ok(false) })
    }
}

#[tokio::test]
async fn serves_every_tool_and_registers_tasks_entities_and_the_email_handler() {
    let app = TestApp::new().await;
    let package = McpPackage::new(&with_token(&app, Some(TOKEN))).unwrap();
    let own = package.tools().unwrap();
    assert_eq!(own.len(), 15);
    assert!(package.event_publisher().is_some());
    let handler = package.email_handler().unwrap();
    assert_eq!(handler.name(), "McpEvents");
    let mut all = other_tools(&own);
    all.extend(own);
    let mut subsystem = package.subsystem(all).unwrap();
    let names: Vec<&str> = subsystem.tasks.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        [
            "McpEventDelivery",
            "TaskRunEvents",
            "McpGatewayRefresh",
            "McpGatewayEventFeeds"
        ]
    );
    assert_eq!(subsystem.entities.len(), 16);
    assert_eq!(subsystem.services.len(), 2);
    assert!(subsystem.mcp_tools.is_empty());
    let steps: Vec<_> = subsystem
        .boot_steps
        .iter()
        .map(|step| (step.name, step.phase))
        .collect();
    assert_eq!(
        steps,
        [
            ("markInterruptedCalls", BootPhase::Reconcile),
            ("requireArchiveEcho", BootPhase::Migrate),
            ("loadGateway", BootPhase::Services),
        ]
    );
    // Events are enabled, so boot fails until wiring sets the ArchiveEcho port.
    let require_echo = subsystem.boot_steps.remove(1);
    let error = (require_echo.run)(app.ctx.clone()).await.unwrap_err();
    assert_eq!(error.step, "requireArchiveEcho");
    app.ctx.ports.set_archive_echo(Arc::new(NoMoves)).unwrap();
    let rebuilt = McpPackage::new(&with_token(&app, Some(TOKEN))).unwrap();
    let own = rebuilt.tools().unwrap();
    let mut all = other_tools(&own);
    all.extend(own);
    let mut rebuilt = rebuilt.subsystem(all).unwrap();
    (rebuilt.boot_steps.remove(1).run)(app.ctx.clone())
        .await
        .unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "events_status", "arguments": {}}})
                .to_string(),
        ))
        .unwrap();
    let exchange = send(&subsystem.router, request).await;
    let message = exchange.message();
    assert_eq!(message["result"]["structuredContent"]["enabled"], true);
    let initialize = send(
        &subsystem.router,
        Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}})
                    .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(
        initialize.message()["result"]["capabilities"],
        json!({"events": {}, "tools": {"listChanged": true}})
    );

    let mut boot = subsystem.boot_steps.into_iter();
    let step = boot.next().unwrap();
    (step.run)(app.ctx.clone()).await.unwrap();
}

#[tokio::test]
async fn is_unavailable_without_a_token_and_rejects_an_incomplete_tool_set() {
    let app = TestApp::new().await;
    let package = McpPackage::new(&with_token(&app, None)).unwrap();
    assert!(package.events().is_none());
    assert!(package.email_handler().is_none());
    let own = package.tools().unwrap();
    let mut all = other_tools(&own);
    all.extend(own);
    let subsystem = package.subsystem(all).unwrap();
    // Without the MCP token there are no events; the gateway still refreshes.
    let names: Vec<&str> = subsystem.tasks.iter().map(|t| t.name()).collect();
    assert_eq!(names, ["McpGatewayRefresh"]);
    let exchange = send(
        &subsystem.router,
        Request::builder()
            .method("POST")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(exchange.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        exchange.message(),
        json!({"error": "MCP is not configured"})
    );

    let partial = McpPackage::new(&with_token(&app, Some(TOKEN))).unwrap();
    let own = partial.tools().unwrap();
    let error = partial.subsystem(own).err().unwrap();
    assert!(error.to_string().contains("missing"));
}

#[tokio::test]
async fn takes_the_claude_host_from_the_device_link_port() {
    // TestApp configures distinct MCP and device-link tokens.
    let app = TestApp::new().await;
    assert!(matches!(
        McpPackage::new(&app.ctx),
        Err(omni_mcp::McpSetupError::MissingClaudeHost)
    ));
    let link = omni_device_link::DeviceLink::from_context(&app.ctx).expect("device link");
    assert!(app.ctx.ports.claude_host().is_some());
    let package = McpPackage::new(&app.ctx).unwrap();
    assert!(package.watcher().is_some());
    drop(link);
}
