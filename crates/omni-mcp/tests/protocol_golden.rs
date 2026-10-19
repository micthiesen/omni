//! Replays every recorded raw HTTP exchange (`tests/golden/protocol.json`, a
//! committed offline recording) against `/gateway/mcp` and compares status,
//! content type, the security headers and every JSON-RPC message.
//!
//! The exchanges were first captured from the TypeScript server's `/mcp`
//! endpoint; when `/mcp` was removed they were re-recorded against the
//! gateway, dropping the Executor delegated-owner exchanges and the GET that
//! now opens the gateway's notification stream. The services are inert: a
//! webhook that verifies and accepts, every gateway owner active, and a task
//! registry that lists nothing and queues `golden-run`.
//!
//! Normalized before comparing: ISO timestamps (`refreshBefore` depends on
//! the wall clock), subscription ids (they derive from the client's random
//! id) and the detail after "Invalid arguments for tool <name>:" (the
//! recorded issue wording versus the JSON Schema validator's).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use common::*;
use futures::future::BoxFuture;
use omni_mcp::activity::ActivityRecorder;
use omni_mcp::events::service::McpEventService;
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_mcp::gateway::access::DownstreamAccess;
use omni_mcp::gateway::secrets::GatewaySecrets;
use omni_mcp::rpc::McpProtocol;
use omni_mcp::tools::system::{ConfiguredFeatures, SystemDeps, system_tools};
use omni_mcp_kit::McpTool;
use omni_runtime::ports::Ports;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

const GOLDEN_TOKEN: &str = "golden-capture-token-0123456789-ABCDEFGHIJKLMNOPQRSTUVWXYZ";

struct AcceptAll;

impl WebhookPort for AcceptAll {
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

fn is_iso(text: &str) -> bool {
    text.len() == 24
        && text.ends_with('Z')
        && text.as_bytes()[10] == b'T'
        && text.as_bytes()[4] == b'-'
}

fn is_subscription_id(text: &str) -> bool {
    text.strip_prefix("sub_")
        .is_some_and(|hex| hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn normalize(value: &Value) -> Value {
    match value {
        Value::String(text) if is_iso(text) => Value::String("<iso>".to_owned()),
        Value::String(text) if is_subscription_id(text) => Value::String("<sub>".to_owned()),
        Value::String(text)
            if text.starts_with("Input validation error: Invalid arguments for tool ") =>
        {
            let cut = text.find(": ").map_or(text.len(), |i| {
                text[i + 2..].find(": ").map_or(text.len(), |j| i + 2 + j)
            });
            Value::String(format!("{}: <issues>", &text[..cut]))
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), normalize(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn essence(content_type: Option<&str>) -> Option<String> {
    content_type.map(|v| v.split(';').next().unwrap_or_default().trim().to_owned())
}

#[tokio::test(start_paused = true)]
async fn gateway_reproduces_every_recorded_exchange() {
    let golden: Value = serde_json::from_str(include_str!("golden/protocol.json")).unwrap();
    let clock = clock();
    let store = test_store(&clock).await;
    let events = with_active_owners(McpEventService::new(
        GOLDEN_TOKEN,
        store.store.clone(),
        clock.clone(),
        Arc::new(AcceptAll),
        Ports::default(),
    ));
    let tasks = FakeTasks {
        run_id: Some("golden-run"),
        ..FakeTasks::default()
    };
    let own: Vec<McpTool> = system_tools(&SystemDeps {
        tasks: Arc::new(tasks),
        ports: Ports::default(),
        features: ConfiguredFeatures::default(),
        clock: clock.clone(),
    })
    .unwrap();
    let mut tools = other_tools(&own);
    tools.extend(own);
    let tracker = TaskTracker::new();
    let recorder = ActivityRecorder::new(store.store.clone(), clock.clone(), tracker.clone());
    let gateway = test_gateway(&store.store, &clock, recorder.clone());
    let protocol = McpProtocol::gateway(tools, recorder, Some(events), gateway.clone()).unwrap();
    let access = DownstreamAccess::new(
        store.store.clone(),
        clock.clone(),
        GatewaySecrets::from_key([42; 32]),
        tracker,
    );
    let (_, token) = access.create_client("golden").await.unwrap();
    let router = omni_mcp::gateway::endpoint::router(
        protocol,
        access,
        gateway,
        None,
        CancellationToken::new(),
    );

    let mut compared = 0;
    for exchange in golden["exchanges"].as_array().unwrap() {
        let label = exchange["label"].as_str().unwrap();
        let req = &exchange["request"];
        let mut builder = Request::builder()
            .method(req["method"].as_str().unwrap())
            .uri("/gateway/mcp");
        let bearer = if label == "raw unauthorized" {
            "wrong"
        } else {
            token.as_str()
        };
        builder = builder.header("authorization", format!("Bearer {bearer}"));
        for (name, value) in req["headers"].as_object().unwrap() {
            builder = builder.header(name.as_str(), value.as_str().unwrap());
        }
        let body = match &req["body"] {
            Value::Null => Body::empty(),
            Value::String(raw) => Body::from(raw.clone()),
            other => Body::from(other.to_string()),
        };
        let actual = send(&router, builder.body(body).unwrap()).await;
        let expected = &exchange["response"];
        assert_eq!(
            actual.status.as_u16(),
            expected["status"].as_u64().unwrap() as u16,
            "{label}: status ({})",
            actual.text
        );
        assert_eq!(
            essence(actual.header("content-type")),
            essence(expected["contentType"].as_str()),
            "{label}: content type"
        );
        for (name, value) in expected["headers"].as_object().unwrap() {
            assert_eq!(
                actual.header(name),
                value.as_str(),
                "{label}: header {name}"
            );
        }
        let messages = normalize(&Value::Array(actual.messages()));
        let wanted = normalize(&expected["messages"]);
        assert_eq!(
            omni_core::js::json_stringify(&messages),
            omni_core::js::json_stringify(&wanted),
            "{label}: messages"
        );
        compared += 1;
    }
    assert_eq!(compared, golden["exchanges"].as_array().unwrap().len());
    // Nothing in the recording names the retired delegated-owner mechanism.
    let text = golden.to_string();
    assert!(!text.contains("x-omni-events-owner"));
    assert!(!text.contains("Executor"));
}
