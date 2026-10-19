//! The modern-era client flow
//! (server/discover, events/list, events/subscribe, events/unsubscribe) for a
//! gateway principal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use common::*;
use futures::future::BoxFuture;
use omni_mcp::events::persistence::EventRequestMethod;
use omni_mcp::events::service::McpEventService;
use omni_mcp::events::webhook::{
    WebhookDestination, WebhookError, WebhookEvent, WebhookFailure, WebhookPort,
};
use omni_runtime::ports::Ports;
use serde_json::{Value, json};

struct Webhook {
    fail_verification: Arc<AtomicBool>,
}

impl WebhookPort for Webhook {
    fn verify<'a>(&'a self, _: &'a WebhookDestination) -> BoxFuture<'a, Result<(), WebhookError>> {
        let fail = self.fail_verification.load(Ordering::SeqCst);
        Box::pin(async move {
            if fail {
                Err(WebhookError::new(WebhookFailure::ChallengeFailed))
            } else {
                Ok(())
            }
        })
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
async fn advertises_events_and_handles_list_subscribe_unsubscribe_for_a_gateway_principal() {
    let clock = clock();
    let db = test_store(&clock).await;
    let owner = owner();
    let fail_verification = Arc::new(AtomicBool::new(false));
    let events = with_active_owners(McpEventService::new(
        TOKEN,
        db.store.clone(),
        clock.clone(),
        Arc::new(Webhook {
            fail_verification: fail_verification.clone(),
        }),
        Ports::default(),
    ));
    let router = mcp_router(&db.store, &clock, Vec::new(), Some(events.clone()));
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "mcp-events-spec", "version": "1.0.0"},
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let modern = |method: &str, id: i64, mut params: Value| {
        params["_meta"] = meta.clone();
        request(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            &[
                ("mcp-protocol-version", "2026-07-28"),
                ("mcp-method", method),
            ],
        )
    };

    let discovery = send(&router, modern("server/discover", 0, json!({}))).await;
    assert!(discovery.text.contains("\"events\""));
    assert_eq!(
        discovery.message()["result"]["supportedVersions"],
        json!(["2026-07-28"])
    );

    let catalog = send(&router, modern("events/list", 1, json!({})))
        .await
        .message();
    let names: Vec<&str> = catalog["result"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "email.received",
            "claude.session.turn_finished",
            "livestream.status_changed",
            "presspods.job_finished",
            "task.run_finished",
            "calendar.event_changed",
            "calendar.event_starting",
        ]
    );
    let requests = events.store().requests().await.unwrap();
    assert!(requests.iter().any(|r| r.method == EventRequestMethod::List
        && r.owner == owner
        && r.outcome == "listed"));

    let secret = format!(
        "whsec_{}",
        base64::engine::general_purpose::STANDARD.encode([9u8; 32])
    );
    let input = |url: &str| {
        json!({
            "name": "email.received",
            "arguments": {"folder": "inbox"},
            "delivery": {"mode": "webhook", "url": url, "secret": secret},
        })
    };
    let subscribed = send(
        &router,
        modern(
            "events/subscribe",
            2,
            input("https://chatgpt.example.com/events/callback"),
        ),
    )
    .await
    .message();
    let id = subscribed["result"]["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("sub_"));
    assert_eq!(subscribed["result"]["cursor"], Value::Null);
    assert!(subscribed["result"]["refreshBefore"].is_string());
    let persisted = events.store().subscription(&id).await.unwrap().unwrap();
    assert_eq!(persisted.owner, owner);
    assert_eq!(persisted.encrypted_authorization, None);

    let http = send(
        &router,
        modern(
            "events/subscribe",
            3,
            input("http://chatgpt.example.com/events/callback"),
        ),
    )
    .await
    .message();
    assert_eq!(http["error"]["code"], -32015);
    assert_eq!(http["error"]["data"], json!({"reason": "invalid_callback"}));

    fail_verification.store(true, Ordering::SeqCst);
    let challenge = send(
        &router,
        modern(
            "events/subscribe",
            4,
            input("https://chatgpt.example.com/events/other"),
        ),
    )
    .await
    .message();
    assert_eq!(challenge["error"]["code"], -32015);
    assert_eq!(
        challenge["error"]["data"],
        json!({"reason": "challenge_failed"})
    );
    fail_verification.store(false, Ordering::SeqCst);

    let removed = send(
        &router,
        modern(
            "events/unsubscribe",
            6,
            json!({
                "name": "email.received",
                "arguments": {"folder": "inbox"},
                "delivery": {"mode": "webhook", "url": "https://chatgpt.example.com/events/callback"},
            }),
        ),
    )
    .await
    .message();
    assert_eq!(removed["result"]["resultType"], "complete");
    assert!(events.store().subscription(&id).await.unwrap().is_none());
}
