//! Other MCP servers' events through the adapter (`EVENT_UPSTREAMS`): listed with Omni's, and
//! subscriptions to them sent to that server with its own token, never to Omni.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use omni_events_adapter::AdapterBuilder;
use omni_events_adapter::config::EventUpstream;
use serde_json::{Value, json};
use support::*;

type Calls = Arc<Mutex<Vec<(Option<String>, Value)>>>;

/// An artifacts.boo workspace's `/mcp`: lists two events and accepts subscriptions, recording each call.
fn fake_workspace(calls: Calls) -> Router {
    Router::new().fallback(handle_workspace).with_state(calls)
}

async fn handle_workspace(State(calls): State<Calls>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes: Bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let auth = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    calls.lock().unwrap().push((auth, body.clone()));
    let result = match body["method"].as_str() {
        Some("events/list") => json!({"events": [
            {"name": "comment.created", "description": "Someone commented.", "inputSchema": {"type": "object", "properties": {"artifact": {"type": "string"}}, "additionalProperties": false}},
            {"name": "artifact.answered", "description": "Someone answered.", "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}}
        ]}),
        Some("events/subscribe") => json!({"id": "sub_1", "cursor": "s5", "refreshBefore": "2026-10-20T00:00:00Z"}),
        Some("events/unsubscribe") => json!({}),
        _ => return ([("content-type", "application/json")], json!({"jsonrpc": "2.0", "id": body["id"], "error": {"code": -32601, "message": "nope"}}).to_string()).into_response(),
    };
    (
        [("content-type", "application/json")],
        json!({"jsonrpc": "2.0", "id": body["id"], "result": result}).to_string(),
    )
        .into_response()
}

#[tokio::test]
async fn upstream_events_are_listed_with_omnis_and_their_subscriptions_go_upstream() {
    let executor = spawn(fake_executor()).await;
    let inbox = Inbox::default();
    let omni = spawn(fake_omni(inbox.clone())).await;
    let calls = Calls::default();
    let workspace = spawn(fake_workspace(calls.clone())).await;
    let mut opts = options(&executor.url, &omni.url, USER_ID);
    opts.event_upstreams = vec![EventUpstream {
        key: "support".into(),
        url: format!("{}/mcp", workspace.url).parse().unwrap(),
        token: "agent-token".into(),
    }];
    let adapter = spawn(AdapterBuilder::new(opts).build().unwrap().router()).await;
    let call = |method: &'static str, params: Value| {
        let url = adapter.url.clone();
        async move {
            modern(&url, method, params, TOKEN, "/mcp")
                .await
                .json::<Value>()
                .await
                .unwrap()
        }
    };

    // Listed: Omni's (none in the fake) plus the workspace's, each with a workspace argument.
    let listed = call("events/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["artifact.answered", "comment.created"]);
    assert_eq!(
        listed["result"]["events"][1]["inputSchema"]["properties"]["workspace"]["enum"],
        json!(["support"])
    );
    assert_eq!(listed["result"]["resultType"], "complete");

    // A subscription goes to the workspace with its token, without the workspace argument or the
    // caller's protocol metadata, and its answer comes back as the result.
    let delivery = json!({"mode": "webhook", "url": "https://hooks.example.com/x", "secret": "whsec_dGVzdHNlY3JldHRlc3RzZWNyZXR0ZXN0"});
    let subscribed = call("events/subscribe", json!({"name": "comment.created", "arguments": {"workspace": "support", "artifact": "a1"}, "delivery": delivery})).await;
    assert_eq!(subscribed["id"], 7);
    assert_eq!(subscribed["result"]["id"], "sub_1");
    let (auth, sent) = calls.lock().unwrap().last().cloned().unwrap();
    assert_eq!(auth.as_deref(), Some("Bearer agent-token"));
    assert_eq!(sent["method"], "events/subscribe");
    assert_eq!(sent["params"]["arguments"], json!({"artifact": "a1"}));
    assert!(sent["params"].get("_meta").is_none());
    assert_eq!(sent["params"]["delivery"], delivery);

    let unsubscribed = call("events/unsubscribe", json!({"name": "comment.created", "arguments": {"artifact": "a1"}, "delivery": {"url": "https://hooks.example.com/x"}})).await;
    assert!(unsubscribed.get("error").is_none());
    assert_eq!(
        calls.lock().unwrap().last().unwrap().1["method"],
        "events/unsubscribe"
    );

    // A wrong workspace is the caller's mistake, answered here; Omni's own events still go to Omni.
    let wrong = call("events/subscribe", json!({"name": "comment.created", "arguments": {"workspace": "michael"}, "delivery": delivery})).await;
    assert_eq!(wrong["error"]["code"], -32602);
    *inbox.lock().unwrap() = None;
    call(
        "events/subscribe",
        json!({"name": "email.received", "arguments": {"folder": "inbox"}, "delivery": delivery}),
    )
    .await;
    assert_eq!(
        inbox.lock().unwrap().as_ref().unwrap().body["params"]["name"],
        "email.received"
    );
}

#[tokio::test]
async fn an_unreachable_upstream_leaves_omnis_events_listed() {
    let executor = spawn(fake_executor()).await;
    let omni = spawn(fake_omni(Inbox::default())).await;
    let mut opts = options(&executor.url, &omni.url, USER_ID);
    opts.event_upstreams = vec![EventUpstream {
        key: "gone".into(),
        url: "http://127.0.0.1:9/mcp".parse().unwrap(),
        token: "t".into(),
    }];
    let adapter = spawn(AdapterBuilder::new(opts).build().unwrap().router()).await;
    let listed: Value = modern(&adapter.url, "events/list", json!({}), TOKEN, "/mcp")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed["result"]["events"], json!([]));
}
