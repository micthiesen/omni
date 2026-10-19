//! The gateway's management API: integration lifecycle, write-only
//! credentials, tool visibility, client tokens shown once, validation
//! messages, and the OAuth callback's failure pages.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod gateway_support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gateway_support::{FakeUpstream, Fixture, tool};
use serde_json::{Value, json};

async fn api(
    fixture: &Fixture,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "omni.boris")
        .header("origin", "http://omni.boris")
        .header("content-type", "application/json");
    let request = builder
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let exchange = common::send(&fixture.admin, request).await;
    (exchange.status, exchange.text)
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn integrations_are_managed_and_credentials_never_come_back() {
    let fixture = Fixture::new().await;
    let upstream = FakeUpstream::start().await;
    upstream
        .state
        .tokens
        .lock()
        .unwrap()
        .insert("api-key-123".into());
    *upstream.state.tools.lock().unwrap() = vec![
        tool("search", "Search things", true),
        tool("write", "", false),
    ];

    let (status, body) = api(
        &fixture,
        "POST",
        "/api/gateway/integrations",
        Some(json!({"id": "search_svc", "name": "Search", "transport": {"kind": "http", "url": upstream.url}, "auth": "headers"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let detail = json_of(&body);
    assert_eq!(detail["summary"]["status"], "pending");
    assert_eq!(detail["config"]["credentialsSet"], false);

    let (status, body) = api(
        &fixture,
        "PUT",
        "/api/gateway/integrations/search_svc/credentials",
        Some(json!({"headers": [{"name": "Authorization", "value": "Bearer api-key-123"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("api-key-123"), "{body}");
    assert_eq!(
        json_of(&body)["config"]["headerNames"],
        json!(["authorization"])
    );
    assert_eq!(json_of(&body)["config"]["credentialsSet"], true);
    // Saving refreshes at once, so the answer already shows the catalog.
    let detail = json_of(&body);
    assert_eq!(detail["summary"]["status"], "ready");
    assert_eq!(detail["summary"]["toolCount"], 2);
    assert_eq!(detail["server"]["name"], "fake-upstream");
    assert_eq!(detail["tools"][0]["exposedName"], "search_svc__search");
    assert_eq!(detail["tools"][0]["readOnly"], true);
    assert_eq!(detail["tools"][0]["change"], "added");
    assert_eq!(detail["lastRefresh"]["added"], json!(["search", "write"]));
    let (status, body) = api(
        &fixture,
        "POST",
        "/api/gateway/integrations/search_svc/refresh",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(json_of(&body)["lastRefresh"]["added"], json!([]));

    let (status, body) = api(
        &fixture,
        "PUT",
        "/api/gateway/integrations/search_svc/tools",
        Some(json!({"hidden": ["write"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let detail = json_of(&body);
    assert_eq!(detail["summary"]["visibleToolCount"], 1);
    assert_eq!(detail["tools"][1]["visible"], false);

    let (_, body) = api(&fixture, "GET", "/api/gateway", None).await;
    assert!(!body.contains("api-key-123"));
    let overview = json_of(&body);
    assert_eq!(overview["path"], "/gateway/mcp");
    assert_eq!(overview["oauthAvailable"], false);
    assert_eq!(overview["integrations"][0]["id"], "search_svc");
    assert!(overview["nativeTools"].as_u64().unwrap() > 50);
    assert_eq!(
        overview["exposedTools"].as_u64().unwrap(),
        overview["nativeTools"].as_u64().unwrap() + 1
    );

    let (status, _) = api(
        &fixture,
        "DELETE",
        "/api/gateway/integrations/search_svc/credentials",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = api(
        &fixture,
        "DELETE",
        "/api/gateway/integrations/search_svc",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = api(
        &fixture,
        "GET",
        "/api/gateway/integrations/search_svc",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    fixture.stop().await;
}

#[tokio::test]
async fn invalid_requests_get_actionable_messages() {
    let fixture = Fixture::new().await;
    let cases = [
        (
            json!({"id": "A", "name": "x", "transport": {"kind": "http", "url": "https://example.com/mcp"}}),
            "lowercase",
        ),
        (
            json!({"id": "ok", "name": "", "transport": {"kind": "http", "url": "https://example.com/mcp"}}),
            "name",
        ),
        (
            json!({"id": "ok", "name": "x", "transport": {"kind": "http", "url": "http://example.com/mcp"}}),
            "https",
        ),
        (
            json!({"id": "ok", "name": "x", "transport": {"kind": "stdio", "server": "nope"}}),
            "bundled",
        ),
        (
            json!({"id": "ok", "name": "x", "transport": {"kind": "http", "url": "https://example.com/mcp"}, "auth": "env"}),
            "stdio",
        ),
        (
            json!({"id": "ok", "name": "x", "transport": {"kind": "http", "url": "https://example.com/mcp"}, "timeoutSeconds": 1}),
            "timeout",
        ),
    ];
    for (body, needle) in cases {
        let (status, text) = api(&fixture, "POST", "/api/gateway/integrations", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
        assert!(text.to_lowercase().contains(needle), "{needle}: {text}");
    }
    let upstream = FakeUpstream::start().await;
    let create =
        json!({"id": "dup", "name": "x", "transport": {"kind": "http", "url": upstream.url}});
    assert_eq!(
        api(
            &fixture,
            "POST",
            "/api/gateway/integrations",
            Some(create.clone())
        )
        .await
        .0,
        StatusCode::CREATED
    );
    assert_eq!(
        api(&fixture, "POST", "/api/gateway/integrations", Some(create))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let (status, text) = api(
        &fixture,
        "PUT",
        "/api/gateway/integrations/dup/credentials",
        Some(json!({"headers": [{"name": "Authorization", "value": "x"}]})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an auth-less integration takes no credentials: {text}"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn a_client_token_is_shown_once_and_can_be_revoked() {
    let fixture = Fixture::new().await;
    let (status, body) = api(
        &fixture,
        "POST",
        "/api/gateway/clients",
        Some(json!({"label": "Claude Code"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created = json_of(&body);
    let token = created["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("omg_") && token.len() > 40);
    let id = created["client"]["id"].as_str().unwrap().to_owned();
    let (_, overview) = api(&fixture, "GET", "/api/gateway", None).await;
    assert!(!overview.contains(&token), "the token is never listed");
    assert_eq!(json_of(&overview)["clients"][0]["label"], "Claude Code");
    assert!(token.starts_with(json_of(&overview)["clients"][0]["prefix"].as_str().unwrap()));
    assert!(
        fixture
            .tool_names(&token)
            .await
            .contains(&"email_search".to_owned())
    );
    let (status, _) = api(
        &fixture,
        "DELETE",
        &format!("/api/gateway/clients/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let request = Request::builder()
        .method("POST")
        .uri("/gateway/mcp")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string(),
        ))
        .unwrap();
    assert_eq!(
        common::send(&fixture.router, request).await.status,
        StatusCode::UNAUTHORIZED
    );
    let (status, _) = api(
        &fixture,
        "POST",
        "/api/gateway/clients",
        Some(json!({"label": " "})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    fixture.stop().await;
}

#[tokio::test]
async fn the_oauth_callback_refuses_unknown_or_replayed_state() {
    let fixture = Fixture::new().await;
    let (status, body) = api(
        &fixture,
        "GET",
        "/gateway/oauth/callback?state=forged&code=x",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("unknown or was already used"), "{body}");
    let (status, _) = api(&fixture, "GET", "/gateway/oauth/callback?code=x", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = api(
        &fixture,
        "POST",
        "/api/gateway/authorizations/x/approve",
        Some(json!({"userCode": "ABCD-EFGH"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    fixture.stop().await;
}

#[tokio::test]
async fn unknown_gateway_paths_are_not_found() {
    let fixture = Fixture::new().await;
    // The app serves the UI as its fallback, like `app_router`.
    let app = fixture.router.clone().fallback(|| async { "ui" });
    for path in [
        "/gateway/anything",
        "/gateway/assets/app.js",
        "/gateway/mcp/extra",
    ] {
        let request = Request::builder().uri(path).body(Body::empty()).unwrap();
        let exchange = common::send(&app, request).await;
        assert_eq!(exchange.status, StatusCode::NOT_FOUND, "{path}");
    }
    let page = Request::builder()
        .uri("/gateway")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        common::send(&app, page).await.text,
        "ui",
        "the UI page itself stays"
    );
    fixture.stop().await;
}
