//! The MCP gateway against scriptable upstream servers on loopback: naming,
//! pagination, catalog changes, credentials and OAuth refresh, sessions,
//! failures, concurrency, cancellation, result fidelity, resources, access
//! control, URL rules and restarts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod gateway_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt as _;
use futures::future::BoxFuture;
use gateway_support::{EVENTS_TOKEN, FakeUpstream, Fixture, Options, raw, tool};
use omni_api::gateway::{
    AuthKind, CredentialsUpdate, IntegrationCreate, IntegrationPatch, IntegrationStatus,
    SecretPair, TransportInput,
};
use omni_core::clock::Clock as _;
use omni_mcp::activity::McpCallData;
use omni_mcp::gateway::credentials::{RefreshError, RefreshedTokens, TokenRefresher};
use omni_mcp::gateway::model::{CredentialRow, OAuthMeta};
use omni_mcp::gateway::service::AdminError;
use omni_store::entity::EntityOps as _;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn header_auth(value: &str) -> CredentialsUpdate {
    CredentialsUpdate {
        headers: vec![SecretPair {
            name: "Authorization".into(),
            value: value.into(),
        }],
        env: Vec::new(),
    }
}

fn text(message: &Value) -> String {
    message["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn integration_tools_are_prefixed_and_never_collide() {
    let fixture = Fixture::new().await;
    let a = FakeUpstream::start().await;
    let b = FakeUpstream::start().await;
    *a.state.tools.lock().unwrap() =
        vec![tool("echo", "a", true), tool("docs.search", "dotted", true)];
    *b.state.tools.lock().unwrap() = vec![tool("echo", "b", false)];
    fixture.add("alpha", &a.url, AuthKind::None).await;
    fixture.add("beta", &b.url, AuthKind::None).await;
    assert!(fixture.refresh("alpha").await.ok);
    assert!(fixture.refresh("beta").await.ok);
    let token = fixture.client_token().await;
    let names = fixture.tool_names(&token).await;
    assert!(
        names.contains(&"email_search".to_owned()),
        "native tools stay unprefixed"
    );
    let served: Vec<&String> = names.iter().filter(|n| n.contains("__")).collect();
    assert_eq!(served.len(), 3, "{served:?}");
    assert!(names.contains(&"alpha__echo".to_owned()));
    assert!(names.contains(&"beta__echo".to_owned()));
    let dotted = names
        .iter()
        .find(|n| n.starts_with("alpha__docs_search_"))
        .expect("dotted name rewritten")
        .clone();
    assert!(
        dotted
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    );

    let reply = fixture
        .call(&token, "beta__echo", json!({"text": "hi"}))
        .await;
    assert_eq!(text(&reply), r#"echo: {"text":"hi"}"#);
    assert_eq!(b.state.calls("tools/call").len(), 1);
    assert!(a.state.calls("tools/call").is_empty());
    fixture.call(&token, &dotted, json!({})).await;
    assert_eq!(a.state.calls("tools/call")[0]["name"], "docs.search");
    fixture.stop().await;
}

#[tokio::test]
async fn every_page_of_a_catalog_is_served_in_order() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    *upstream.state.tools.lock().unwrap() =
        (0..7).map(|i| tool(&format!("t{i}"), "", true)).collect();
    *upstream.state.page_size.lock().unwrap() = Some(3);
    fixture.add("paged", &upstream.url, AuthKind::None).await;
    assert!(fixture.refresh("paged").await.ok);
    assert_eq!(upstream.state.pages_served.load(Ordering::Relaxed), 3);
    let token = fixture.client_token().await;
    let served: Vec<String> = fixture
        .tool_names(&token)
        .await
        .into_iter()
        .filter(|n| n.starts_with("paged__"))
        .collect();
    let expected: Vec<String> = (0..7).map(|i| format!("paged__t{i}")).collect();
    assert_eq!(served, expected);
    fixture.stop().await;
}

#[tokio::test]
async fn catalog_changes_are_reported_and_announced_to_connected_clients() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    *upstream.state.tools.lock().unwrap() = vec![tool("keep", "v1", true), tool("drop", "", true)];
    fixture.add("svc", &upstream.url, AuthKind::None).await;
    fixture.refresh("svc").await;
    let token = fixture.client_token().await;
    let response = raw(
        &fixture.router,
        Request::builder()
            .method("GET")
            .uri("/gateway/mcp")
            .header("authorization", format!("Bearer {token}"))
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();

    *upstream.state.tools.lock().unwrap() = vec![tool("keep", "v2", true), tool("new", "", false)];
    let report = fixture.refresh("svc").await;
    assert!(report.ok);
    assert_eq!(report.added, ["new"]);
    assert_eq!(report.removed, ["drop"]);
    assert_eq!(report.changed, ["keep"]);
    let frame = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("a notification")
        .unwrap()
        .unwrap();
    let frame = String::from_utf8_lossy(&frame).into_owned();
    assert!(
        frame.contains("notifications/tools/list_changed"),
        "{frame}"
    );

    // An unchanged refresh announces nothing.
    let quiet = fixture.refresh("svc").await;
    assert!(quiet.added.is_empty() && quiet.changed.is_empty() && quiet.removed.is_empty());
    let none = tokio::time::timeout(Duration::from_millis(300), stream.next()).await;
    assert!(none.is_err(), "no notification for an unchanged catalog");
    fixture.stop().await;
}

#[tokio::test]
async fn upstream_list_changed_notifications_trigger_a_refresh() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    upstream
        .state
        .offer_get_stream
        .store(true, Ordering::Relaxed);
    fixture.add("live", &upstream.url, AuthKind::None).await;
    fixture.refresh("live").await;
    let worker = fixture.gateway.clone();
    let worker = tokio::spawn(async move { worker.refresh_worker().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    *upstream.state.tools.lock().unwrap() = vec![tool("echo", "", true), tool("later", "", true)];
    let _ = upstream
        .state
        .notify
        .send("notifications/tools/list_changed".into());
    let token = fixture.client_token().await;
    let mut seen = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if fixture
            .tool_names(&token)
            .await
            .contains(&"live__later".to_owned())
        {
            seen = true;
            break;
        }
    }
    assert!(seen, "the notification refreshed the catalog");
    fixture.shutdown.cancel();
    worker.await.unwrap();
    fixture.stop().await;
}

#[tokio::test]
async fn header_credentials_authenticate_and_rejected_ones_ask_for_new_ones() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    upstream
        .state
        .tokens
        .lock()
        .unwrap()
        .insert("key-one".into());
    fixture.add("keyed", &upstream.url, AuthKind::Headers).await;
    let missing = fixture.refresh("keyed").await;
    assert!(!missing.ok);
    let runtime = fixture.gateway.runtime("keyed").unwrap();
    assert_eq!(runtime.status(), IntegrationStatus::NeedsAuth);
    assert!(
        upstream.state.requests.lock().unwrap().is_empty(),
        "nothing sent without credentials"
    );

    fixture
        .gateway
        .set_credentials("keyed", header_auth("Bearer key-one"))
        .await
        .unwrap();
    assert!(fixture.refresh("keyed").await.ok);
    assert_eq!(runtime.status(), IntegrationStatus::Ready);
    let token = fixture.client_token().await;
    let reply = fixture.call(&token, "keyed__echo", json!({})).await;
    assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
    assert!(
        upstream
            .state
            .authorizations()
            .iter()
            .all(|a| a.as_deref() == Some("Bearer key-one"))
    );

    // The integration rotates its key: calls fail with a next step.
    {
        let mut tokens = upstream.state.tokens.lock().unwrap();
        tokens.clear();
        tokens.insert("key-two".into());
    }
    upstream
        .state
        .expire_sessions
        .store(true, Ordering::Relaxed);
    let reply = fixture.call(&token, "keyed__echo", json!({})).await;
    assert_eq!(reply["result"]["isError"], json!(true));
    assert!(text(&reply).contains("credentials"), "{}", text(&reply));
    assert_eq!(
        reply["result"]["_meta"]["io.omni/gateway"]["error"],
        "auth_required"
    );
    assert_eq!(runtime.status(), IntegrationStatus::NeedsAuth);

    // Stored rows never show the value.
    let row = fixture
        .store
        .read(|docs| docs.get::<CredentialRow>(&"keyed".to_owned()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.header_names, ["authorization"]);
    assert!(!format!("{row:?}").contains("key-one"));
    assert!(!row.sealed.contains("key-one"));
    fixture.stop().await;
}

struct FakeRefresher {
    calls: AtomicUsize,
    reject: bool,
}

impl TokenRefresher for FakeRefresher {
    fn refresh<'a>(
        &'a self,
        _meta: &'a OAuthMeta,
        refresh_token: &'a str,
        _client_secret: Option<&'a str>,
        now: i64,
    ) -> BoxFuture<'a, Result<RefreshedTokens, RefreshError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let reject = self.reject;
        let ok = refresh_token == "refresh-1";
        Box::pin(async move {
            if reject || !ok {
                return Err(RefreshError::Rejected);
            }
            Ok(RefreshedTokens {
                access_token: "fresh".into(),
                refresh_token: Some("refresh-2".into()),
                expires_at: Some(now + 3_600_000),
                scope: None,
            })
        })
    }
}

async fn oauth_fixture(reject: bool) -> (Fixture, FakeUpstream, Arc<FakeRefresher>) {
    let refresher = Arc::new(FakeRefresher {
        calls: AtomicUsize::new(0),
        reject,
    });
    let fixture = Fixture::with(Options {
        refresher: Some(refresher.clone()),
        ..Options::default()
    })
    .await;
    let upstream = FakeUpstream::start().await;
    upstream.state.tokens.lock().unwrap().insert("fresh".into());
    fixture.add("oauthed", &upstream.url, AuthKind::OAuth).await;
    let row = fixture.gateway.runtime("oauthed").unwrap().config();
    let now = fixture.clock.now_ms();
    fixture
        .gateway
        .credentials()
        .save_oauth(
            &row,
            OAuthMeta {
                issuer: "https://upstream.example".into(),
                token_endpoint: "https://upstream.example/token".into(),
                client_id: "client".into(),
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
    (fixture, upstream, refresher)
}

#[tokio::test]
async fn a_rejected_oauth_token_is_refreshed_once_and_the_request_retried() {
    let (fixture, upstream, refresher) = oauth_fixture(false).await;
    assert!(fixture.refresh("oauthed").await.ok);
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    let token = fixture.client_token().await;
    let reply = fixture.call(&token, "oauthed__echo", json!({})).await;
    assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
    assert_eq!(
        refresher.calls.load(Ordering::SeqCst),
        1,
        "the fresh token is stored and reused"
    );
    let row = fixture.gateway.runtime("oauthed").unwrap().config();
    let (_, secrets) = fixture
        .gateway
        .credentials()
        .current(&row)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(secrets.access_token.as_deref(), Some("fresh"));
    assert_eq!(secrets.refresh_token.as_deref(), Some("refresh-2"));
    fixture.stop().await;
    drop(upstream);
}

#[tokio::test]
async fn an_expiring_oauth_token_is_refreshed_before_the_request() {
    let (fixture, upstream, refresher) = oauth_fixture(false).await;
    fixture.clock.set(fixture.clock.now_ms() + 3_600_000);
    assert!(fixture.refresh("oauthed").await.ok);
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    assert!(
        upstream
            .state
            .authorizations()
            .iter()
            .all(|a| a.as_deref() == Some("Bearer fresh")),
        "the expired token was never sent"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn a_revoked_oauth_grant_asks_to_reconnect() {
    let (fixture, _upstream, refresher) = oauth_fixture(true).await;
    let report = fixture.refresh("oauthed").await;
    assert!(!report.ok);
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    let runtime = fixture.gateway.runtime("oauthed").unwrap();
    assert_eq!(runtime.status(), IntegrationStatus::NeedsAuth);
    assert!(
        report.warnings[0].contains("Reconnect"),
        "{:?}",
        report.warnings
    );
    fixture.stop().await;
}

#[tokio::test]
async fn an_expired_session_is_reinitialized_and_the_call_retried() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    fixture.add("sess", &upstream.url, AuthKind::None).await;
    fixture.refresh("sess").await;
    assert_eq!(upstream.state.initializes.load(Ordering::Relaxed), 1);
    upstream
        .state
        .expire_sessions
        .store(true, Ordering::Relaxed);
    let token = fixture.client_token().await;
    let reply = fixture
        .call(&token, "sess__echo", json!({"text": "again"}))
        .await;
    assert_eq!(text(&reply), r#"echo: {"text":"again"}"#);
    assert_eq!(upstream.state.initializes.load(Ordering::Relaxed), 2);
    assert_eq!(
        upstream.state.calls("tools/call").len(),
        1,
        "the expired request never ran"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn outages_are_reported_and_the_catalog_keeps_serving() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    fixture.add("flaky", &upstream.url, AuthKind::None).await;
    fixture.refresh("flaky").await;
    let token = fixture.client_token().await;
    *upstream.state.fail_status.lock().unwrap() = Some(503);
    let reply = fixture.call(&token, "flaky__echo", json!({})).await;
    assert_eq!(reply["result"]["isError"], json!(true));
    assert!(text(&reply).contains("HTTP 503"), "{}", text(&reply));
    assert_eq!(upstream.state.calls("tools/call").len(), 0);
    let report = fixture.refresh("flaky").await;
    assert!(!report.ok);
    let runtime = fixture.gateway.runtime("flaky").unwrap();
    assert_eq!(runtime.status(), IntegrationStatus::Stale);
    assert!(
        fixture
            .tool_names(&token)
            .await
            .contains(&"flaky__echo".to_owned())
    );

    *upstream.state.fail_status.lock().unwrap() = None;
    let reply = fixture.call(&token, "flaky__echo", json!({})).await;
    assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
    assert!(fixture.refresh("flaky").await.ok);
    assert_eq!(runtime.status(), IntegrationStatus::Ready);

    // The server disappears entirely.
    let url = upstream.url.clone();
    drop(upstream);
    tokio::time::sleep(Duration::from_millis(100)).await;
    fixture.gateway.reset_session("flaky").await;
    let reply = fixture.call(&token, "flaky__echo", json!({})).await;
    assert_eq!(reply["result"]["isError"], json!(true));
    assert!(text(&reply).contains("reach"), "{} ({url})", text(&reply));
    fixture.stop().await;
}

#[tokio::test]
async fn concurrent_calls_share_a_session_and_are_bounded() {
    let fixture = Arc::new(Fixture::new().await);
    let upstream = FakeUpstream::start().await;
    upstream.state.call_delay_ms.store(200, Ordering::Relaxed);
    fixture.add("busy", &upstream.url, AuthKind::None).await;
    fixture.refresh("busy").await;
    let mut calls = Vec::new();
    for i in 0..20 {
        let gateway = fixture.gateway.clone();
        calls.push(tokio::spawn(async move {
            gateway
                .call_tool(
                    "busy__echo",
                    json!({"text": i.to_string()}),
                    CancellationToken::new(),
                )
                .await
                .unwrap()
                .unwrap()
        }));
    }
    for call in calls {
        let result = call.await.unwrap();
        assert_ne!(result.get("isError"), Some(&json!(true)), "{result:?}");
    }
    let max = upstream.state.max_active_calls.load(Ordering::SeqCst);
    assert!((2..=8).contains(&max), "max concurrent {max}");
    assert_eq!(upstream.state.initializes.load(Ordering::Relaxed), 1);
    Arc::try_unwrap(fixture).ok().unwrap().stop().await;
}

#[tokio::test]
async fn a_slow_call_times_out_and_is_cancelled_upstream() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    upstream.state.call_delay_ms.store(8_000, Ordering::Relaxed);
    fixture.add("slow", &upstream.url, AuthKind::None).await;
    fixture.refresh("slow").await;
    let started = std::time::Instant::now();
    let result = fixture
        .gateway
        .call_tool("slow__echo", json!({}), CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(7));
    assert_eq!(result["isError"], json!(true));
    assert_eq!(result["_meta"]["io.omni/gateway"]["error"], "timeout");
    for _ in 0..50 {
        if !upstream.state.cancelled.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let cancelled = upstream.state.cancelled.lock().unwrap().clone();
    assert_eq!(cancelled.len(), 1);
    assert!(cancelled[0]["requestId"].is_number());
    fixture.stop().await;
}

#[tokio::test]
async fn a_client_cancellation_reaches_the_integration() {
    let fixture = Arc::new(Fixture::new().await);
    let upstream = FakeUpstream::start().await;
    upstream.state.call_delay_ms.store(4_000, Ordering::Relaxed);
    fixture.add("long", &upstream.url, AuthKind::None).await;
    fixture.refresh("long").await;
    let token = fixture.client_token().await;
    let running = {
        let fixture = fixture.clone();
        let token = token.clone();
        tokio::spawn(async move {
            fixture
                .post(
                    &token,
                    &json!({"jsonrpc": "2.0", "id": 77, "method": "tools/call", "params": {"name": "long__echo", "arguments": {}}}),
                )
                .await
                .message()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let accepted = fixture
        .post(
            &token,
            &json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 77, "reason": "user"}}),
        )
        .await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED);
    let started = std::time::Instant::now();
    let reply = running.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the call ended at once"
    );
    assert_eq!(reply["result"]["isError"], json!(true));
    for _ in 0..50 {
        if !upstream.state.cancelled.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(upstream.state.cancelled.lock().unwrap().len(), 1);
    // Another client cannot cancel someone else's call.
    let other = fixture.access.create_client("other").await.unwrap().1;
    upstream.state.call_delay_ms.store(500, Ordering::Relaxed);
    let running = {
        let fixture = fixture.clone();
        let token = token.clone();
        tokio::spawn(async move {
            fixture
                .post(
                    &token,
                    &json!({"jsonrpc": "2.0", "id": 78, "method": "tools/call", "params": {"name": "long__echo", "arguments": {}}}),
                )
                .await
                .message()
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    fixture
        .post(
            &other,
            &json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 78}}),
        )
        .await;
    let reply = running.await.unwrap();
    assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
    Arc::try_unwrap(fixture).ok().unwrap().stop().await;
}

#[tokio::test]
async fn results_and_errors_are_forwarded_unchanged() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    let rich = json!({
        "content": [
            {"type": "text", "text": "hello", "annotations": {"audience": ["user"], "priority": 0.5}},
            {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"},
            {"type": "audio", "data": "UklGRg==", "mimeType": "audio/wav"},
            {"type": "resource_link", "uri": "repo://octo/readme", "name": "README", "mimeType": "text/markdown"},
            {"type": "resource", "resource": {"uri": "repo://octo/notes", "mimeType": "text/plain", "text": "notes"}},
        ],
        "structuredContent": {"count": 2, "items": [{"id": 1.5}, {"id": null}]},
        "isError": false,
        "_meta": {"upstream/trace": "abc", "progressToken": "x"},
    });
    let failed = json!({"content": [{"type": "text", "text": "no such repo"}], "isError": true});
    *upstream.state.tools.lock().unwrap() = vec![
        tool("rich", "", true),
        tool("failed", "", true),
        tool("broken", "", true),
    ];
    upstream
        .state
        .results
        .lock()
        .unwrap()
        .insert("rich".into(), rich.clone());
    upstream
        .state
        .results
        .lock()
        .unwrap()
        .insert("failed".into(), failed.clone());
    upstream.state.errors.lock().unwrap().insert(
        "broken".into(),
        json!({"code": -32042, "message": "Quota exceeded", "data": {"retryAfter": 30}}),
    );
    fixture.add("rich", &upstream.url, AuthKind::None).await;
    fixture.refresh("rich").await;
    let token = fixture.client_token().await;
    assert_eq!(
        fixture.call(&token, "rich__rich", json!({})).await["result"],
        rich
    );
    assert_eq!(
        fixture.call(&token, "rich__failed", json!({})).await["result"],
        failed
    );
    let broken = fixture.call(&token, "rich__broken", json!({})).await;
    assert_eq!(
        broken["error"],
        json!({"code": -32042, "message": "Quota exceeded", "data": {"retryAfter": 30}})
    );

    // Event-stream responses with interleaved server messages give the same result.
    upstream
        .state
        .stream_responses
        .store(true, Ordering::Relaxed);
    assert_eq!(
        fixture.call(&token, "rich__rich", json!({})).await["result"],
        rich
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let replies = upstream.state.server_replies.lock().unwrap().clone();
    assert!(
        replies
            .iter()
            .any(|r| r["id"] == "srv-1" && r["result"] == json!({})),
        "{replies:?}"
    );

    // The modern protocol era wraps the same result.
    let modern = fixture
        .post(
            &token,
            &json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "rich__failed", "arguments": {}, "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientCapabilities": {}}}}),
        )
        .await;
    let _ = modern;
    fixture.stop().await;
}

#[tokio::test]
async fn resources_are_listed_and_read_through_their_integration() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    *upstream.state.capabilities.lock().unwrap() = json!({"tools": {}, "resources": {}});
    *upstream.state.resources.lock().unwrap() =
        vec![json!({"uri": "repo://octo/readme", "name": "README"})];
    *upstream.state.templates.lock().unwrap() =
        vec![json!({"uriTemplate": "repo://{owner}/{name}", "name": "file"})];
    {
        let mut contents = upstream.state.resource_contents.lock().unwrap();
        contents.insert(
            "repo://octo/readme".into(),
            json!({"uri": "repo://octo/readme", "text": "# Hi"}),
        );
        contents.insert(
            "repo://cat/x".into(),
            json!({"uri": "repo://cat/x", "blob": "AAE="}),
        );
    }
    fixture.add("repo", &upstream.url, AuthKind::None).await;
    fixture.refresh("repo").await;
    let token = fixture.client_token().await;
    let listed = fixture
        .post(
            &token,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"}),
        )
        .await
        .message();
    assert_eq!(
        listed["result"]["resources"][0]["uri"],
        "repo://octo/readme"
    );
    let templates = fixture
        .post(
            &token,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/templates/list"}),
        )
        .await
        .message();
    assert_eq!(
        templates["result"]["resourceTemplates"][0]["uriTemplate"],
        "repo://{owner}/{name}"
    );
    let read = fixture
        .post(&token, &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "repo://octo/readme"}}))
        .await
        .message();
    assert_eq!(read["result"]["contents"][0]["text"], "# Hi");
    let templated = fixture
        .post(&token, &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "repo://cat/x"}}))
        .await
        .message();
    assert_eq!(templated["result"]["contents"][0]["blob"], "AAE=");
    let missing = fixture
        .post(&token, &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "repo://none/y"}}))
        .await
        .message();
    assert_eq!(missing["error"]["code"], -32002);
    fixture.stop().await;
}

#[tokio::test]
async fn resource_links_from_tool_results_stay_readable() {
    let fixture = Fixture::new().await;
    let a = FakeUpstream::start().await;
    let b = FakeUpstream::start().await;
    *a.state.tools.lock().unwrap() = vec![tool("find", "", true)];
    a.state.results.lock().unwrap().insert(
        "find".into(),
        json!({"content": [{"type": "resource_link", "uri": "mem://result/1", "name": "r"}]}),
    );
    a.state.resource_contents.lock().unwrap().insert(
        "mem://result/1".into(),
        json!({"uri": "mem://result/1", "text": "found"}),
    );
    *b.state.capabilities.lock().unwrap() = json!({"tools": {}, "resources": {}});
    fixture.add("aa", &a.url, AuthKind::None).await;
    fixture.add("bb", &b.url, AuthKind::None).await;
    fixture.refresh("aa").await;
    fixture.refresh("bb").await;
    let token = fixture.client_token().await;
    fixture.call(&token, "aa__find", json!({})).await;
    let read = fixture
        .post(&token, &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {"uri": "mem://result/1"}}))
        .await
        .message();
    assert_eq!(read["result"]["contents"][0]["text"], "found");
    assert!(b.state.calls("resources/read").is_empty());
    fixture.stop().await;
}

#[tokio::test]
async fn only_gateway_tokens_are_accepted() {
    let fixture = Fixture::new().await;
    let request = |token: Option<&str>| {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/gateway/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string(),
            ))
            .unwrap()
    };
    let missing = raw(&fixture.router, request(None)).await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    let challenge = missing.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(challenge.contains("resource_metadata=\"https://gateway.example/.well-known/oauth-protected-resource/gateway/mcp\""), "{challenge}");
    for wrong in ["nope", EVENTS_TOKEN, "omg_forged", "oat_forged.sig"] {
        assert_eq!(
            raw(&fixture.router, request(Some(wrong))).await.status(),
            StatusCode::UNAUTHORIZED,
            "{wrong}"
        );
    }
    let (row, token) = fixture.access.create_client("ok").await.unwrap();
    assert_eq!(
        raw(&fixture.router, request(Some(&token))).await.status(),
        StatusCode::OK
    );
    fixture.access.revoke_client(&row.id).await.unwrap();
    assert_eq!(
        raw(&fixture.router, request(Some(&token))).await.status(),
        StatusCode::UNAUTHORIZED
    );
    fixture.stop().await;
}

#[tokio::test]
async fn untrusted_integration_urls_are_refused() {
    let fixture = Fixture::new().await;
    for url in [
        "http://example.com/mcp",
        "https://10.0.0.1/mcp",
        "https://192.168.1.10/mcp",
        "https://[::1]/mcp",
        "https://localhost/mcp",
        "https://user:pw@example.com/mcp",
        "https://example.com/mcp#frag",
        "file:///etc/passwd",
        "ftp://example.com/mcp",
    ] {
        let created = fixture
            .gateway
            .create(IntegrationCreate {
                id: "bad".into(),
                name: "Bad".into(),
                transport: TransportInput::Http { url: url.into() },
                auth: AuthKind::None,
                timeout_seconds: None,
                enabled: None,
            })
            .await;
        assert!(matches!(created, Err(AdminError::Invalid(_))), "{url}");
    }
    for id in ["Bad", "a__b", "x_", "1x", "omni-gw"] {
        let created = fixture
            .gateway
            .create(IntegrationCreate {
                id: id.into(),
                name: "Bad".into(),
                transport: TransportInput::Http {
                    url: "https://example.com/mcp".into(),
                },
                auth: AuthKind::None,
                timeout_seconds: None,
                enabled: None,
            })
            .await;
        assert!(matches!(created, Err(AdminError::Invalid(_))), "{id}");
    }
    fixture.stop().await;
}

#[tokio::test]
async fn changing_the_url_origin_drops_stored_credentials() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    fixture
        .add("moving", &upstream.url, AuthKind::Headers)
        .await;
    fixture
        .gateway
        .set_credentials("moving", header_auth("Bearer secret-value"))
        .await
        .unwrap();
    // Same origin, new path: kept.
    let same_origin = upstream.url.replace("/mcp", "/mcp2");
    fixture
        .gateway
        .patch(
            "moving",
            IntegrationPatch {
                url: Some(same_origin),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        fixture
            .gateway
            .credentials()
            .row("moving")
            .await
            .unwrap()
            .is_some()
    );
    let other = FakeUpstream::start().await;
    fixture
        .gateway
        .patch(
            "moving",
            IntegrationPatch {
                url: Some(other.url.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        fixture
            .gateway
            .credentials()
            .row("moving")
            .await
            .unwrap()
            .is_none()
    );
    fixture.refresh("moving").await;
    assert!(
        other.state.requests.lock().unwrap().is_empty(),
        "nothing sent to the new origin"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn hidden_and_disabled_tools_are_neither_listed_nor_callable() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    *upstream.state.tools.lock().unwrap() =
        vec![tool("echo", "", true), tool("delete_all", "", false)];
    fixture.add("vis", &upstream.url, AuthKind::None).await;
    fixture.refresh("vis").await;
    let token = fixture.client_token().await;
    fixture
        .gateway
        .set_hidden("vis", vec!["delete_all".into()])
        .await
        .unwrap();
    let names = fixture.tool_names(&token).await;
    assert!(names.contains(&"vis__echo".to_owned()));
    assert!(!names.contains(&"vis__delete_all".to_owned()));
    let hidden = fixture.call(&token, "vis__delete_all", json!({})).await;
    assert_eq!(hidden["error"]["message"], "Tool vis__delete_all not found");
    assert!(upstream.state.calls("tools/call").is_empty());
    fixture
        .gateway
        .patch(
            "vis",
            IntegrationPatch {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !fixture
            .tool_names(&token)
            .await
            .iter()
            .any(|n| n.starts_with("vis__"))
    );
    fixture.stop().await;
}

#[tokio::test]
async fn calls_are_recorded_without_their_arguments() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    fixture.add("rec", &upstream.url, AuthKind::None).await;
    fixture.refresh("rec").await;
    let token = fixture.client_token().await;
    fixture
        .call(&token, "rec__echo", json!({"text": "private words"}))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let calls = fixture
        .store
        .read(|docs| docs.get_all::<McpCallData>())
        .await
        .unwrap();
    let call = calls
        .iter()
        .find(|c| c.tool == "rec__echo")
        .expect("recorded");
    assert!(!format!("{call:?}").contains("private words"));
    fixture.stop().await;
}

#[tokio::test]
async fn catalogs_are_served_after_a_restart_before_any_refresh() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    fixture.add("kept", &upstream.url, AuthKind::None).await;
    fixture.refresh("kept").await;
    let token = fixture.client_token().await;
    let store = fixture.store.clone();
    let clock = fixture.clock.clone();
    let holder = fixture;
    drop(upstream);
    let restarted = Fixture::over(store, clock, Options::default()).await;
    assert!(
        restarted
            .tool_names(&token)
            .await
            .contains(&"kept__echo".to_owned())
    );
    let runtime = restarted.gateway.runtime("kept").unwrap();
    assert_eq!(runtime.status(), IntegrationStatus::Ready);
    restarted.stop().await;
    holder.stop().await;
}

#[tokio::test]
async fn a_long_call_keeps_its_response_alive_until_the_result() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    upstream
        .state
        .call_delay_ms
        .store(16_500, Ordering::Relaxed);
    fixture
        .gateway
        .create(IntegrationCreate {
            id: "patient".into(),
            name: "Patient".into(),
            transport: TransportInput::Http {
                url: upstream.url.clone(),
            },
            auth: AuthKind::None,
            timeout_seconds: Some(60),
            enabled: Some(true),
        })
        .await
        .unwrap();
    fixture.refresh("patient").await;
    let token = fixture.client_token().await;
    let response = raw(
        &fixture.router,
        Request::builder()
            .method("POST")
            .uri("/gateway/mcp")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "patient__echo", "arguments": {}}}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(&first[..], b": keepalive\n\n");
    let mut rest = Vec::new();
    while let Some(chunk) = stream.next().await {
        rest.extend_from_slice(&chunk.unwrap());
    }
    let text = String::from_utf8(rest).unwrap();
    assert!(
        text.contains(r#""id":5"#) && text.contains("echo: {}"),
        "{text}"
    );
    fixture.stop().await;
}
