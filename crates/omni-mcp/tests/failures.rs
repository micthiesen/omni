//! `GET /api/failures`: failed tool calls and dead deliveries from the last
//! week in one list, newest first, each with a link to its story.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use futures::future::BoxFuture;
use omni_api::outbound::EventTargetUpsert;
use omni_http::public::PublicHttpClient;
use omni_http::{HttpClient, HttpConfig};
use omni_mcp::activity::{ActivityRecorder, CallSource};
use omni_mcp::events::service::McpEventService;
use omni_mcp::events::targets::{OpenTarget, TargetError, TargetResponse, TargetTransport};
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_mcp::gateway::bundled::BundledRegistry;
use omni_mcp::gateway::secrets::GatewaySecrets;
use omni_mcp::gateway::{Gateway, GatewayParts};
use omni_mcp_kit::McpTool;
use omni_runtime::ports::{Ports, TargetNotice};
use omni_testkit::TestApp;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

struct Refuses;

impl TargetTransport for Refuses {
    fn post<'a>(
        &'a self,
        _: &'a OpenTarget,
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<TargetResponse, TargetError>> {
        Box::pin(async {
            Ok(TargetResponse {
                status: 401,
                ignored: false,
            })
        })
    }
}

struct NoWebhooks;

impl WebhookPort for NoWebhooks {
    fn verify<'a>(&'a self, _: &'a WebhookDestination) -> BoxFuture<'a, Result<(), WebhookError>> {
        Box::pin(async { Ok(()) })
    }
    fn deliver<'a>(
        &'a self,
        _: &'a WebhookDestination,
        _: &'a WebhookEvent,
    ) -> BoxFuture<'a, Result<u16, WebhookError>> {
        Box::pin(async { Ok(204) })
    }
}

#[tokio::test(start_paused = true)]
async fn failed_calls_and_dead_deliveries_are_listed_with_their_links() {
    let app = TestApp::new().await;
    let ctx = app.ctx.clone();
    let events = McpEventService::new(
        "test-omni-bearer",
        ctx.store.clone(),
        ctx.clock.clone(),
        Arc::new(NoWebhooks),
        Ports::default(),
    );
    events.set_target_transport(Arc::new(Refuses));
    events
        .upsert_target(
            "chief",
            &EventTargetUpsert {
                label: "Chief".into(),
                url: "https://rakazo.example/api/v1/bots/chief/webhook".into(),
                key: Some("chief-key-0123456789".into()),
                connect: None,
            },
        )
        .await
        .unwrap();
    events
        .notify_target(TargetNotice {
            target: "chief".into(),
            key: "omni-claude-turn:s:1".into(),
            name: "claude.session.turn_finished".into(),
            body: json!({}),
        })
        .await
        .unwrap();
    events.drain().await.unwrap();

    let recorder = ActivityRecorder::new(ctx.store.clone(), ctx.clock.clone(), TaskTracker::new());
    let tools: Vec<McpTool> = common::other_tools(&[]);
    let meta = tools
        .iter()
        .find(|t| t.meta.name == "email_search")
        .unwrap()
        .meta
        .clone();
    recorder
        .record_invalid(
            &meta,
            &json!({"limit": "lots"}),
            &CallSource {
                caller: Some("Rakazo (Boris)".into()),
                integration: None,
            },
            "Input validation error: limit must be a number",
        )
        .await;

    let http = HttpClient::new(HttpConfig::default()).unwrap();
    let gateway = Gateway::new(GatewayParts {
        store: ctx.store.clone(),
        clock: ctx.clock.clone(),
        http: PublicHttpClient::new(&http),
        secrets: GatewaySecrets::from_key([42; 32]),
        refresher: None,
        bundled: BundledRegistry::default(),
        tracker: TaskTracker::new(),
        shutdown: CancellationToken::new(),
        recorder,
        allow_loopback_http: false,
        side_effects: omni_http::SideEffectMode::Record,
    });
    let router = omni_mcp::failures::router(ctx, events, gateway);
    let request = Request::get("/api/failures")
        .header("host", "omni.boris")
        .body(Body::empty())
        .unwrap();
    let body: Value = serde_json::from_str(&common::send(&router, request).await.text).unwrap();
    assert_eq!(body["counts"]["toolCalls"], 1);
    assert_eq!(body["counts"]["deliveries"], 1);
    let items = body["items"].as_array().unwrap();
    let call = items.iter().find(|i| i["kind"] == "tool_call").unwrap();
    assert_eq!(call["title"], "email_search");
    assert!(call["detail"].as_str().unwrap().contains("Rakazo (Boris)"));
    assert_eq!(call["link"], "/mcp-activity");
    let delivery = items.iter().find(|i| i["kind"] == "delivery").unwrap();
    assert_eq!(delivery["title"], "claude.session.turn_finished → chief");
    assert!(
        delivery["detail"]
            .as_str()
            .unwrap()
            .contains("rejected (http_401)")
    );
    assert!(!body.to_string().contains("chief-key"));
}
