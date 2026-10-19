//! Gateway behavior under concurrency and partial failure: one OAuth refresh
//! for many concurrent 401s, cancellation when a client goes away, partial
//! catalog refreshes, configuration edits during calls, revocation cleanup,
//! the management API guard and record mode.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod gateway_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use gateway_support::{FakeUpstream, Fixture, Options, subscribe_params, tool};
use omni_api::gateway::AuthKind;
use omni_core::clock::Clock as _;
use omni_mcp::events::persistence::EventSubscription;
use omni_mcp::gateway::credentials::{RefreshError, RefreshedTokens, TokenRefresher};
use omni_mcp::gateway::model::OAuthMeta;
use omni_store::entity::EntityOps as _;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct CountingRefresher {
    calls: AtomicUsize,
}

impl TokenRefresher for CountingRefresher {
    fn refresh<'a>(
        &'a self,
        _meta: &'a OAuthMeta,
        _refresh_token: &'a str,
        _client_secret: Option<&'a str>,
        now: i64,
    ) -> BoxFuture<'a, Result<RefreshedTokens, RefreshError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(RefreshedTokens {
                access_token: "fresh".into(),
                refresh_token: Some("refresh-2".into()),
                expires_at: Some(now + 3_600_000),
                scope: None,
            })
        })
    }
}

#[tokio::test]
async fn concurrent_rejections_share_one_refresh_and_one_session() {
    let refresher = Arc::new(CountingRefresher {
        calls: AtomicUsize::new(0),
    });
    let fixture = Arc::new(
        Fixture::with(Options {
            refresher: Some(refresher.clone()),
            ..Options::default()
        })
        .await,
    );
    let upstream = FakeUpstream::start().await;
    upstream.state.tokens.lock().unwrap().insert("fresh".into());
    fixture.add("o", &upstream.url, AuthKind::OAuth).await;
    let row = fixture.gateway.runtime("o").unwrap().config();
    let now = fixture.clock.now_ms();
    fixture
        .gateway
        .credentials()
        .save_oauth(
            &row,
            OAuthMeta {
                issuer: "https://as.example".into(),
                token_endpoint: "https://as.example/token".into(),
                client_id: "c".into(),
                resource: upstream.url.clone(),
                scope: None,
                access_expires_at: Some(now + 3_600_000),
                refreshable: true,
                connected_at: now,
                token_auth: "none".into(),
                extra: Default::default(),
            },
            "stale".into(),
            Some("refresh-1".into()),
            None,
        )
        .await
        .unwrap();
    // The catalog is stored, so calls route before any session exists.
    *upstream.state.tokens.lock().unwrap() = ["stale".to_owned()].into();
    assert!(fixture.refresh("o").await.ok);
    fixture.gateway.reset_session("o").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    *upstream.state.tokens.lock().unwrap() = ["fresh".to_owned()].into();
    let before = upstream.state.initializes.load(Ordering::SeqCst);
    let mut calls = Vec::new();
    for _ in 0..6 {
        let gateway = fixture.gateway.clone();
        calls.push(tokio::spawn(async move {
            gateway
                .call_tool("o__echo", json!({}), CancellationToken::new())
                .await
                .unwrap()
                .unwrap()
        }));
    }
    for call in calls {
        let result = call.await.unwrap();
        assert_ne!(result.get("isError"), Some(&json!(true)), "{result:?}");
    }
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1, "one refresh");
    let connects = upstream.state.initializes.load(Ordering::SeqCst) - before;
    assert!(connects <= 2, "{connects} initialize requests");
    Arc::try_unwrap(fixture).ok().unwrap().stop().await;
}

#[tokio::test]
async fn a_client_that_goes_away_cancels_the_upstream_call() {
    let fixture = Arc::new(Fixture::new().await);
    let upstream = FakeUpstream::start().await;
    upstream.state.call_delay_ms.store(5_000, Ordering::Relaxed);
    fixture.add("gone", &upstream.url, AuthKind::None).await;
    fixture.refresh("gone").await;
    let token = fixture.client_token().await;
    let running = {
        let fixture = fixture.clone();
        tokio::spawn(async move {
            fixture
                .post(
                    &token,
                    &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "gone__echo", "arguments": {}}}),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    running.abort();
    let mut cancelled = 0;
    for _ in 0..40 {
        cancelled = upstream.state.cancelled.lock().unwrap().len();
        if cancelled > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(cancelled, 1, "the abandoned call is cancelled upstream");
    Arc::try_unwrap(fixture).ok().unwrap().stop().await;
}

#[tokio::test]
async fn a_failed_secondary_list_keeps_the_previous_entries() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    *upstream.state.capabilities.lock().unwrap() = json!({
        "tools": {},
        "resources": {},
        "extensions": {"io.modelcontextprotocol/events": {}},
    });
    *upstream.state.resources.lock().unwrap() = vec![json!({"uri": "mem://one", "name": "one"})];
    *upstream.state.events.lock().unwrap() =
        vec![json!({"name": "thing.happened", "delivery": ["poll"]})];
    fixture.add("part", &upstream.url, AuthKind::None).await;
    assert!(fixture.refresh("part").await.ok);
    upstream
        .state
        .fail_methods
        .lock()
        .unwrap()
        .extend(["events/list".to_owned(), "resources/list".to_owned()]);
    let report = fixture.refresh("part").await;
    assert!(report.ok);
    assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
    let catalog = fixture.gateway.runtime("part").unwrap().catalog().unwrap();
    assert_eq!(catalog.events.len(), 1, "the event is still offered");
    assert_eq!(catalog.resources.len(), 1, "the resource is still listed");
    fixture.stop().await;
}

#[tokio::test]
async fn hiding_a_tool_does_not_interrupt_a_running_call() {
    let fixture = Arc::new(Fixture::new().await);
    let upstream = FakeUpstream::start().await;
    *upstream.state.tools.lock().unwrap() = vec![tool("slow", "", true), tool("other", "", true)];
    upstream.state.call_delay_ms.store(800, Ordering::Relaxed);
    fixture.add("busy", &upstream.url, AuthKind::None).await;
    fixture.refresh("busy").await;
    let running = {
        let gateway = fixture.gateway.clone();
        tokio::spawn(async move {
            gateway
                .call_tool("busy__slow", json!({}), CancellationToken::new())
                .await
                .unwrap()
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    fixture
        .gateway
        .set_hidden("busy", vec!["other".into()])
        .await
        .unwrap();
    let result = running.await.unwrap();
    assert_ne!(result.get("isError"), Some(&json!(true)), "{result:?}");
    assert_eq!(upstream.state.initializes.load(Ordering::SeqCst), 1);
    Arc::try_unwrap(fixture).ok().unwrap().stop().await;
}

async fn admin(fixture: &Fixture, method: &str, uri: &str, headers: &[(&str, &str)]) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .header("content-type", "application/json")
        .body(Body::from(json!({"label": "x"}).to_string()))
        .unwrap();
    common::send(&fixture.admin, request).await.status
}

#[tokio::test]
async fn management_changes_need_a_home_network_origin() {
    let fixture = Fixture::new().await;
    let lan = [("host", "omni.boris"), ("origin", "http://omni.boris")];
    assert_eq!(
        admin(&fixture, "POST", "/api/gateway/clients", &lan).await,
        StatusCode::CREATED
    );
    assert_eq!(
        admin(
            &fixture,
            "POST",
            "/api/gateway/clients",
            &[("host", "omni.boris")]
        )
        .await,
        StatusCode::FORBIDDEN,
        "no Origin"
    );
    assert_eq!(
        admin(
            &fixture,
            "POST",
            "/api/gateway/clients",
            &[
                ("host", "rebound.example"),
                ("origin", "http://rebound.example")
            ]
        )
        .await,
        StatusCode::FORBIDDEN,
        "DNS rebinding"
    );
    assert_eq!(
        admin(
            &fixture,
            "GET",
            "/api/gateway",
            &[("host", "rebound.example")]
        )
        .await,
        StatusCode::OK,
        "reads stay open on the LAN"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn revoking_a_client_ends_its_subscriptions() {
    let fixture = Fixture::new().await;
    let (row, token) = fixture.access.create_client("watch").await.unwrap();
    let reply = fixture
        .post(
            &token,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "events/subscribe", "params": subscribe_params("email.received", json!({"folder": "inbox"}))}),
        )
        .await
        .message();
    assert!(reply["result"]["id"].is_string(), "{reply}");
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/api/gateway/clients/{}", row.id))
        .header("host", "omni.boris")
        .header("origin", "http://omni.boris")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        common::send(&fixture.admin, request).await.status,
        StatusCode::NO_CONTENT
    );
    let left: Vec<EventSubscription> = fixture
        .store
        .read(|docs| docs.get_all::<EventSubscription>())
        .await
        .unwrap();
    assert!(left.is_empty(), "{left:?}");
    fixture.stop().await;
}

#[tokio::test]
async fn record_mode_never_calls_integration_tools() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    fixture.add("rec", &upstream.url, AuthKind::None).await;
    fixture.refresh("rec").await;
    let recording = Fixture::over(
        fixture.store.clone(),
        fixture.clock.clone(),
        Options {
            record: true,
            ..Options::default()
        },
    )
    .await;
    let result = recording
        .gateway
        .call_tool("rec__echo", json!({}), CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["isError"], json!(true));
    assert!(upstream.state.calls("tools/call").is_empty());
    let text: &Value = &result["content"][0]["text"];
    assert!(text.as_str().unwrap().contains("recording"), "{text}");
    recording.stop().await;
    fixture.stop().await;
}
