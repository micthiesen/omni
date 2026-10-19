//! Production-copy checks for every entity this package persists:
//! `OMNI_PROD_COPY=/path/to/docstore.db cargo test -p omni-mcp --test prod_copy -- --ignored --nocapture`.
//!
//! The file is copied into a temporary directory first; the original is
//! never opened. Each row must decode into its typed entity, recompute its
//! primary key, and re-encode to the same JS value (keys holding `undefined`
//! dropped, which node reads identically). The read paths the service uses
//! (subscriptions with legacy `folder`, requests, deliveries, status) must
//! also succeed over the real rows.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::sync::Arc;

use futures::future::BoxFuture;
use omni_core::clock::SystemClock;
use omni_mcp::activity::{ActivityQuery, McpCallData, get_mcp_activity};
use omni_mcp::events::claude_sessions::ClaudeSessionWatch;
use omni_mcp::events::persistence::{
    EventDelivery, EventReceipt, EventRequest, EventStore, EventSubscription,
};
use omni_mcp::events::service::McpEventService;
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_runtime::ports::Ports;
use omni_store::cbor::{self, JsValue};
use omni_store::entity::{Entity, pk};
use omni_store::{DocOps, Store, StoreOptions};

fn strip_undefined(value: &JsValue) -> JsValue {
    match value {
        JsValue::Object(map) => JsValue::Object(
            map.iter()
                .filter(|(_, v)| !matches!(v, JsValue::Undefined))
                .map(|(k, v)| (k.clone(), strip_undefined(v)))
                .collect(),
        ),
        JsValue::Array(items) => JsValue::Array(items.iter().map(strip_undefined).collect()),
        other => other.clone(),
    }
}

/// Top-level key order, marking keys stored as `undefined` (no values).
fn keys(value: &JsValue) -> String {
    value.as_object().map_or_else(String::new, |map| {
        map.iter()
            .map(|(k, v)| match v {
                JsValue::Undefined => format!("{k}=undefined"),
                _ => k.clone(),
            })
            .collect::<Vec<_>>()
            .join(",")
    })
}

async fn check<E: Entity>(store: &Store) -> usize {
    let prefix = format!("${}#", E::NAME);
    let rows = store
        .read(move |docs| docs.get_raw_rows_by_prefix(&prefix))
        .await
        .unwrap();
    let mut identical = 0;
    for row in &rows {
        assert_eq!(row.entity.as_deref(), Some(E::NAME), "{}", row.pk);
        let original = row.decode().unwrap();
        let typed: E = cbor::from_value(original.clone())
            .unwrap_or_else(|e| panic!("{} does not decode: {e}", row.pk));
        assert_eq!(pk::<E>(&typed.key()).unwrap(), row.pk, "primary key");
        let reencoded = cbor::to_value(&typed).unwrap();
        // What node reads back from the bytes Rust would write.
        let read_back = cbor::decode(&cbor::encode(&reencoded)).unwrap();
        assert_eq!(
            strip_undefined(&read_back),
            strip_undefined(&original),
            "{} re-encodes to a different value",
            row.pk
        );
        if row.data.as_deref() == Some(cbor::encode(&reencoded).as_slice()) {
            identical += 1;
        } else if identical == 0 && row.pk == rows[0].pk {
            println!("  stored keys:  {}", keys(&original));
            println!("  written keys: {}", keys(&reencoded));
        }
    }
    println!(
        "{}: {} rows decode and round-trip ({} byte-identical)",
        E::NAME,
        rows.len(),
        identical
    );
    rows.len()
}

struct Inert;

impl WebhookPort for Inert {
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

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a production docstore copy"]
async fn every_wp12_entity_row_decodes_and_round_trips() {
    let source = std::env::var("OMNI_PROD_COPY").expect("set OMNI_PROD_COPY");
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("docstore.db");
    std::fs::copy(&source, &copy).unwrap();
    let mut permissions = std::fs::metadata(&copy).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&copy, permissions).unwrap();
    let clock = Arc::new(SystemClock);
    let store = Store::open(&copy, StoreOptions::new(clock.clone()))
        .await
        .unwrap();

    let mut total = 0;
    total += check::<McpCallData>(&store).await;
    total += check::<EventSubscription>(&store).await;
    total += check::<EventReceipt>(&store).await;
    total += check::<EventDelivery>(&store).await;
    total += check::<EventRequest>(&store).await;
    total += check::<ClaudeSessionWatch>(&store).await;
    println!("total MCP rows: {total}");

    let events = EventStore::new(store.clone());
    events.subscriptions().await.unwrap();
    events.requests().await.unwrap();
    events.deliveries().await.unwrap();
    events.receipts().await.unwrap();
    // A different token: production rows show as stale_key, nothing decrypts.
    let service = McpEventService::new(
        "prod-copy-check-token-0123456789-ABCDEFGHIJ",
        store.clone(),
        clock.clone(),
        Arc::new(Inert),
        Ports::default(),
    );
    let status = service.status().await.unwrap();
    println!(
        "events_status over the copy: {} subscriptions, {} requests",
        status["subscriptionTotal"],
        status["requests"].as_array().map_or(0, Vec::len)
    );
    let activity = get_mcp_activity(
        &store,
        &ActivityQuery {
            limit: 100,
            ..ActivityQuery::default()
        },
        omni_core::clock::Clock::now_ms(&SystemClock),
    )
    .await
    .unwrap();
    println!(
        "mcp activity over the copy: {} stored",
        activity.summary.stored
    );
}
