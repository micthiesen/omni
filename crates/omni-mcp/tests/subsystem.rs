//! The wiring contract of `McpPackage` over a test `AppContext`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{package_tools, send};
use futures::future::BoxFuture;
use omni_config::Config;
use omni_core::email::EmailOrigin;
use omni_mcp::McpPackage;
use omni_mcp::events::persistence::EventSubscription;
use omni_runtime::BootPhase;
use omni_runtime::ports::{ArchiveEcho, PortError};
use omni_store::cbor::Extra;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_testkit::TestApp;
use serde_json::{Value, json};

/// The test context without a device link (so no Claude Code host is needed).
fn without_device_link(app: &TestApp) -> omni_runtime::AppContext {
    let mut env = omni_testkit::test_app_env();
    env.remove("OMNI_DEVICE_LINK_TOKEN");
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

fn gateway_post(token: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/gateway/mcp")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn subscription(id: &str, owner: &str) -> EventSubscription {
    EventSubscription {
        id: id.to_owned(),
        owner: owner.to_owned(),
        key_id: "old-key".to_owned(),
        generation: "g".to_owned(),
        name: "email.received".to_owned(),
        arguments: None,
        folder: Some("inbox".to_owned()),
        encrypted_url: "sealed".to_owned(),
        encrypted_secret: "sealed".to_owned(),
        encrypted_previous_secret: None,
        previous_secret_until: None,
        encrypted_authorization: Some("sealed".to_owned()),
        refresh_before: None,
        expires_at: 4_102_444_800_000,
        verified_at: 0,
        lapse_alerted_at: None,
        extra: Extra::default(),
    }
}

#[tokio::test]
async fn serves_every_tool_at_the_gateway_and_registers_tasks_entities_and_the_email_handler() {
    let app = TestApp::new().await;
    let ctx = without_device_link(&app);
    let package = McpPackage::new(&ctx).unwrap();
    let own = package.tools().unwrap();
    assert_eq!(own.len(), 15);
    assert_eq!(package.email_handler().name(), "McpEvents");
    let (_, token) = package.access().create_client("test").await.unwrap();
    let mut all = package_tools(&own);
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
    assert_eq!(subsystem.entities.len(), 17);
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
            ("dropLegacyEventSubscriptions", BootPhase::Migrate),
            ("requireArchiveEcho", BootPhase::Migrate),
            ("loadGateway", BootPhase::Services),
        ]
    );
    // Events are always enabled, so boot fails until wiring sets the ArchiveEcho port.
    let require_echo = subsystem.boot_steps.remove(2);
    let error = (require_echo.run)(ctx.clone()).await.unwrap_err();
    assert_eq!(error.step, "requireArchiveEcho");
    app.ctx.ports.set_archive_echo(Arc::new(NoMoves)).unwrap();
    let rebuilt = McpPackage::new(&ctx).unwrap();
    let own = rebuilt.tools().unwrap();
    let mut all = package_tools(&own);
    all.extend(own);
    let mut rebuilt = rebuilt.subsystem(all).unwrap();
    (rebuilt.boot_steps.remove(2).run)(ctx.clone())
        .await
        .unwrap();

    let exchange = send(
        &subsystem.router,
        gateway_post(
            &token,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "events_status", "arguments": {}}}),
        ),
    )
    .await;
    assert_eq!(
        exchange.message()["result"]["structuredContent"]["enabled"],
        true
    );
    let initialize = send(
        &subsystem.router,
        gateway_post(
            &token,
            &json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
        ),
    )
    .await;
    let capabilities = &initialize.message()["result"]["capabilities"];
    assert_eq!(capabilities["events"], json!({}));
    assert_eq!(capabilities["tools"], json!({"listChanged": true}));

    // The old endpoint is gone.
    let old = send(
        &subsystem.router,
        Request::builder()
            .method("POST")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(old.status, StatusCode::NOT_FOUND);

    let mut boot = subsystem.boot_steps.into_iter();
    let step = boot.next().unwrap();
    (step.run)(ctx.clone()).await.unwrap();
}

#[tokio::test]
async fn boot_drops_subscriptions_not_owned_by_a_gateway_principal() {
    let app = TestApp::new().await;
    let ctx = without_device_link(&app);
    let executor = format!("executor:{}", "a".repeat(64));
    let gateway = "gateway:client:kept";
    ctx.store
        .write({
            let rows = [
                subscription("sub_executor", &executor),
                subscription("sub_direct", &"b".repeat(64)),
                subscription("sub_gateway", gateway),
            ];
            move |tx| {
                for row in &rows {
                    tx.upsert(row, UpsertOpts::default())?;
                }
                Ok::<_, omni_store::StoreError>(())
            }
        })
        .await
        .unwrap();
    let stored = |ctx: omni_runtime::AppContext| async move {
        ctx.store
            .read(|docs| docs.get_all::<EventSubscription>())
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<String>>()
    };
    assert_eq!(stored(ctx.clone()).await.len(), 3);
    let package = McpPackage::new(&ctx).unwrap();
    let own = package.tools().unwrap();
    let mut all = package_tools(&own);
    all.extend(own);
    let mut subsystem = package.subsystem(all).unwrap();
    assert_eq!(stored(ctx.clone()).await.len(), 3);
    let step = subsystem.boot_steps.remove(1);
    assert_eq!(step.name, "dropLegacyEventSubscriptions");
    (step.run)(ctx.clone()).await.unwrap();
    let left: Vec<String> = ctx
        .store
        .read(|docs| docs.get_all::<EventSubscription>())
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(left, ["sub_gateway"]);
}

#[tokio::test]
async fn rejects_an_incomplete_tool_set() {
    let app = TestApp::new().await;
    let partial = McpPackage::new(&without_device_link(&app)).unwrap();
    let own = partial.tools().unwrap();
    let error = partial.subsystem(own).err().unwrap();
    assert!(error.to_string().contains("missing"));
}

#[tokio::test]
async fn takes_the_claude_host_from_the_device_link_port() {
    // TestApp configures a device-link token.
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
