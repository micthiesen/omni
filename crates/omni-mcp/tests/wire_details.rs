//! Wire details: wire-only material lifted out of `events/*` params before the
//! strict checks, safe-integer issues on `ttlMs`, and schema refinements
//! (input-phase tool errors) answered like argument validation without being
//! recorded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use futures::future::BoxFuture;
use omni_mcp::activity::McpCallData;
use omni_mcp::events::service::McpEventService;
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_mcp_kit::{ToolError, ToolOutput, raw_tool};
use omni_runtime::ports::Ports;
use omni_store::Store;
use omni_store::entity::EntityOps as _;
use serde_json::{Value, json};

struct Webhook;

impl WebhookPort for Webhook {
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

fn events(store: &Store, clock: &Arc<omni_core::clock::TestClock>) -> McpEventService {
    McpEventService::new(
        TOKEN,
        store.clone(),
        clock.clone(),
        Arc::new(Webhook),
        Ports::default(),
    )
}

const ENVELOPE: &str = "io.modelcontextprotocol/protocolVersion";

fn modern(method: &str, params: Value) -> axum::http::Request<axum::body::Body> {
    request(
        &json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}),
        &[
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", method),
        ],
    )
}

fn envelope(extra: Value) -> Value {
    let mut meta = json!({
        ENVELOPE: "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    if let (Some(meta), Value::Object(extra)) = (meta.as_object_mut(), extra) {
        meta.extend(extra);
    }
    meta
}

#[tokio::test(start_paused = true)]
async fn strict_event_params_see_only_non_envelope_meta() {
    let clock = clock();
    let db = test_store(&clock).await;
    let router = mcp_router(
        &db.store,
        &clock,
        Vec::new(),
        Some(events(&db.store, &clock)),
    );

    let lifted = send(
        &router,
        modern("events/list", json!({"_meta": envelope(json!({}))})),
    )
    .await
    .message();
    assert!(lifted["result"]["events"].is_array(), "{lifted}");

    let unsubscribe = |meta: Value| {
        json!({
            "name": "email.received",
            "arguments": {"folder": "inbox"},
            "delivery": {"mode": "webhook", "url": "https://hooks.example.com/x"},
            "_meta": meta,
        })
    };
    let lifted = send(
        &router,
        modern("events/unsubscribe", unsubscribe(envelope(json!({})))),
    )
    .await
    .message();
    assert_eq!(lifted["result"]["resultType"], "complete", "{lifted}");

    let progress = send(
        &router,
        modern(
            "events/unsubscribe",
            unsubscribe(envelope(json!({"progressToken": 1}))),
        ),
    )
    .await
    .message();
    assert_eq!(progress["error"]["code"], json!(-32602));
    assert_eq!(
        progress["error"]["message"],
        json!("Invalid params for events/unsubscribe: Unrecognized key: \"_meta\"")
    );

    let legacy_meta = send(
        &router,
        legacy(&json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "events/unsubscribe",
            "params": unsubscribe(json!({"progressToken": "p"})),
        })),
    )
    .await
    .message();
    assert_eq!(legacy_meta["error"]["code"], json!(-32602));

    let mut retried = unsubscribe(json!(null));
    retried.as_object_mut().unwrap().remove("_meta");
    retried["requestState"] = json!("opaque");
    retried["inputResponses"] = json!({});
    let legacy_retry = send(
        &router,
        legacy(&json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "events/unsubscribe",
            "params": retried,
        })),
    )
    .await
    .message();
    assert_eq!(legacy_retry["result"], json!({}), "{legacy_retry}");
}

#[tokio::test(start_paused = true)]
async fn ttl_outside_the_safe_integer_range_reports_zod_issues() {
    let clock = clock();
    let db = test_store(&clock).await;
    let router = mcp_router(
        &db.store,
        &clock,
        Vec::new(),
        Some(events(&db.store, &clock)),
    );
    let subscribe = |ttl: f64| {
        legacy(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "events/subscribe",
            "params": {
                "name": "email.received",
                "arguments": {"folder": "inbox"},
                "delivery": {"mode": "webhook", "url": "https://hooks.example.com/x", "secret": "whsec_x"},
                "ttlMs": ttl,
            },
        }))
    };
    let negative = send(&router, subscribe(-1e20)).await.message();
    assert_eq!(
        negative["error"]["message"],
        json!(
            "Invalid params for events/subscribe: ttlMs: Too small: expected int to be >=-9007199254740991, ttlMs: Too small: expected number to be >0"
        )
    );
    let positive = send(&router, subscribe(1e20)).await.message();
    assert_eq!(
        positive["error"]["message"],
        json!(
            "Invalid params for events/subscribe: ttlMs: Too big: expected int to be <=9007199254740991"
        )
    );
}

async fn recorded(store: &Store) -> Vec<McpCallData> {
    store
        .read(|docs| docs.get_all::<McpCallData>())
        .await
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn schema_refinements_fail_like_argument_validation_and_are_not_recorded() {
    let clock = clock();
    let db = test_store(&clock).await;
    let refined = raw_tool(
        def("email_search"),
        Arc::new(FnTool(|_: Value| -> Result<ToolOutput, ToolError> {
            Err(ToolError::input("before must be later than since"))
        })),
    )
    .unwrap();
    let failing = raw_tool(
        def("tasks_list"),
        Arc::new(FnTool(|_: Value| -> Result<ToolOutput, ToolError> {
            Err(ToolError::execute("registry unavailable"))
        })),
    )
    .unwrap();
    let router = mcp_router(&db.store, &clock, vec![refined, failing], None);

    let message = call_tool(&router, "email_search", json!({})).await;
    assert!(is_error(&message));
    assert_eq!(
        error_text(&message),
        "Input validation error: Invalid arguments for tool email_search: before must be later than since"
    );
    assert!(recorded(&db.store).await.is_empty());

    let message = call_tool(&router, "tasks_list", json!({})).await;
    assert_eq!(error_text(&message), "registry unavailable");
    let calls = recorded(&db.store).await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool, "tasks_list");
    assert_eq!(calls[0].error.as_deref(), Some("registry unavailable"));
}
