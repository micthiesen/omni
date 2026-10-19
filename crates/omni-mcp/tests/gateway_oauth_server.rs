//! The downstream OAuth authorization server ChatGPT connects through:
//! metadata, allowlisted CIMD and capped DCR clients, owner approval by the
//! code shown to the browser that started the request, single-use codes
//! with PKCE, rotating refresh tokens with reuse revocation, the rate
//! limiter, and the access tokens `DownstreamAccess` accepts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use omni_core::clock::TestClock;
use omni_http::Url;
use omni_http::public::PublicHttpClient;
use omni_mcp::gateway::access::{DownstreamAccess, Principal};
use omni_mcp::gateway::model::{AuthorizationRow, GrantRow, OAuthClientRow};
use omni_mcp::gateway::oauth::pkce;
use omni_mcp::gateway::oauth::server::{
    Approval, AuthorizationNotifier, AuthorizationServer, is_verified_client,
};
use omni_mcp::gateway::secrets::GatewaySecrets;
use omni_store::Store;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_testkit::TestStore;
use serde_json::{Value, json};
use tokio_util::task::TaskTracker;
use tower::ServiceExt as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const EPOCH_MS: i64 = 1_790_000_000_000;
const RESOURCE: &str = "https://gateway.example/gateway/mcp";
const ISSUER: &str = "https://gateway.example";
const REDIRECT: &str = "https://chatgpt.com/connector_platform_oauth_redirect";
const CIMD: &str = "https://chatgpt.com/oauth/client.json";
const COOKIE: &str = "__Host-omni-oauth";

struct Fixture {
    clock: Arc<TestClock>,
    store: Store,
    _store: TestStore,
    server: AuthorizationServer,
    router: Router,
    access: DownstreamAccess,
    notified: Arc<Mutex<Vec<AuthorizationRow>>>,
}

async fn fixture_with(http: PublicHttpClient, resource: &str) -> Fixture {
    let clock = TestClock::new(EPOCH_MS);
    let test_store = TestStore::new(clock.clone()).await;
    let store = test_store.store.clone();
    let secrets = GatewaySecrets::from_key([9; 32]);
    let tracker = TaskTracker::new();
    let notified = Arc::new(Mutex::new(Vec::new()));
    let sink = notified.clone();
    let notifier: AuthorizationNotifier =
        Arc::new(move |row: &AuthorizationRow| sink.lock().unwrap().push(row.clone()));
    let server = AuthorizationServer::new(
        store.clone(),
        clock.clone(),
        http,
        secrets.clone(),
        Url::parse(resource).unwrap(),
        Some(notifier),
        tracker.clone(),
    );
    let access = DownstreamAccess::new(store.clone(), clock.clone(), secrets, tracker);
    Fixture {
        router: server.router(),
        clock,
        store,
        _store: test_store,
        server,
        access,
        notified,
    }
}

/// A fixture whose client metadata fetches go nowhere (DCR clients only).
async fn fixture() -> Fixture {
    fixture_with(
        PublicHttpClient::new(&omni_testkit::no_network()).allow_loopback_for_tests(),
        RESOURCE,
    )
    .await
}

/// A fixture whose `https://chatgpt.com` and `https://evil.example`
/// requests reach `server`.
async fn cimd_fixture(server: &MockServer) -> Fixture {
    let http = omni_testkit::mock_http(server, &["https://chatgpt.com", "https://evil.example"]);
    fixture_with(
        PublicHttpClient::new(&http).allow_loopback_for_tests(),
        RESOURCE,
    )
    .await
}

async fn send(f: &Fixture, request: Request<Body>) -> Response<Body> {
    f.router.clone().oneshot(request).await.unwrap()
}

async fn get(f: &Fixture, uri: &str, cookie: Option<&str>) -> Response<Body> {
    let mut request = Request::builder().method("GET").uri(uri);
    if let Some(value) = cookie {
        request = request.header("cookie", format!("{COOKIE}={value}"));
    }
    send(f, request.body(Body::empty()).unwrap()).await
}

async fn text(response: Response<Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn json_body(response: Response<Body>) -> Value {
    serde_json::from_str(&text(response).await).unwrap_or(Value::Null)
}

async fn post_form(f: &Fixture, pairs: &[(&str, &str)]) -> (StatusCode, Value) {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    let response = send(
        f,
        Request::builder()
            .method("POST")
            .uri("/gateway/oauth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(header(&response, "cache-control"), "no-store");
    let status = response.status();
    (status, json_body(response).await)
}

async fn post_json(f: &Fixture, uri: &str, body: Value) -> (StatusCode, Value) {
    let response = send(
        f,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    let status = response.status();
    (status, json_body(response).await)
}

fn header(response: &Response<Body>, name: &str) -> String {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default()
}

/// The redirect target and its query.
fn location(response: &Response<Body>) -> (String, HashMap<String, String>) {
    assert_eq!(response.status(), StatusCode::FOUND);
    let url = Url::parse(&header(response, "location")).unwrap();
    let query = url.query_pairs().into_owned().collect();
    let mut base = url.clone();
    base.set_query(None);
    (base.to_string(), query)
}

fn attr(html: &str, name: &str) -> String {
    let start = html.find(&format!("{name}=\"")).unwrap() + name.len() + 2;
    let end = html[start..].find('"').unwrap();
    html[start..start + end].to_owned()
}

/// The code the waiting page shows (what the owner types on the LAN page).
fn shown_code(html: &str) -> String {
    let element = html.find("class=\"code\"").unwrap();
    let start = element + html[element..].find('>').unwrap() + 1;
    let end = start + html[start..].find('<').unwrap();
    html[start..end].to_owned()
}

async fn register(f: &Fixture) -> String {
    let (status, body) = post_json(
        f,
        "/gateway/oauth/register",
        json!({"client_name": "ChatGPT", "redirect_uris": [REDIRECT]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["client_id"].as_str().unwrap().to_owned()
}

fn authorize_uri(client_id: &str, extra: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", REDIRECT),
        ("state", "state-1"),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
        ("scope", "omni"),
    ];
    for (k, v) in extra {
        pairs.retain(|(name, _)| name != k);
        pairs.push((k, v));
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().filter(|(_, v)| !v.is_empty()))
        .finish();
    format!("/gateway/oauth/authorize?{query}")
}

struct Started {
    id: String,
    cookie: String,
    verifier: String,
    user_code: String,
    html: String,
    response_headers: axum::http::HeaderMap,
}

async fn start(f: &Fixture, client_id: &str) -> Started {
    let verifier = pkce::verifier();
    let challenge = pkce::challenge(&verifier);
    let response = get(
        f,
        &authorize_uri(client_id, &[("code_challenge", &challenge)]),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let set_cookie = header(&response, "set-cookie");
    let cookie = set_cookie
        .split(';')
        .next()
        .and_then(|pair| pair.split_once('='))
        .map(|(_, value)| value.to_owned())
        .unwrap();
    let response_headers = response.headers().clone();
    let html = text(response).await;
    Started {
        id: attr(&html, "data-request"),
        cookie,
        verifier,
        user_code: shown_code(&html),
        html,
        response_headers,
    }
}

async fn approve(f: &Fixture, started: &Started) -> Approval {
    f.server
        .approve(&started.id, &started.user_code)
        .await
        .unwrap()
}

async fn status_of(f: &Fixture, id: &str, cookie: Option<&str>) -> (StatusCode, Value) {
    let response = get(
        f,
        &format!("/gateway/oauth/authorize/status?request={id}"),
        cookie,
    )
    .await;
    let status = response.status();
    (status, json_body(response).await)
}

async fn complete(f: &Fixture, id: &str, cookie: Option<&str>) -> Response<Body> {
    get(
        f,
        &format!("/gateway/oauth/authorize/complete?request={id}"),
        cookie,
    )
    .await
}

/// Approves and completes `started`; the issued code.
async fn approve_and_complete(f: &Fixture, started: &Started) -> String {
    assert_eq!(approve(f, started).await, Approval::Approved);
    let response = complete(f, &started.id, Some(&started.cookie)).await;
    let (target, query) = location(&response);
    assert_eq!(target, REDIRECT);
    query["code"].clone()
}

async fn exchange(f: &Fixture, client_id: &str, code: &str, verifier: &str) -> (StatusCode, Value) {
    post_form(
        f,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT),
            ("client_id", client_id),
            ("code_verifier", verifier),
            ("resource", RESOURCE),
        ],
    )
    .await
}

async fn refresh(f: &Fixture, client_id: &str, token: &str) -> (StatusCode, Value) {
    post_form(
        f,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", token),
            ("client_id", client_id),
            ("resource", RESOURCE),
        ],
    )
    .await
}

async fn authenticates(f: &Fixture, access_token: &str) -> Option<Principal> {
    f.access
        .authenticate(Some(&format!("Bearer {access_token}")))
        .await
}

#[tokio::test]
async fn metadata_documents_describe_the_server() {
    let f = fixture().await;
    assert_eq!(f.server.issuer(), ISSUER);
    assert_eq!(
        f.server.resource_metadata_url(),
        "https://gateway.example/.well-known/oauth-protected-resource/gateway/mcp"
    );
    let expected_prm = json!({
        "resource": RESOURCE,
        "authorization_servers": [ISSUER],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["omni"],
        "resource_name": "Omni",
    });
    for at in [
        "/.well-known/oauth-protected-resource/gateway/mcp",
        "/.well-known/oauth-protected-resource",
    ] {
        let response = get(&f, at, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, "cache-control"), "no-store");
        assert_eq!(json_body(response).await, expected_prm);
    }
    let response = get(&f, "/.well-known/oauth-authorization-server", None).await;
    assert_eq!(header(&response, "cache-control"), "no-store");
    assert_eq!(
        json_body(response).await,
        json!({
            "issuer": ISSUER,
            "authorization_endpoint": "https://gateway.example/gateway/oauth/authorize",
            "token_endpoint": "https://gateway.example/gateway/oauth/token",
            "registration_endpoint": "https://gateway.example/gateway/oauth/register",
            "response_types_supported": ["code"],
            "response_modes_supported": ["query"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "scopes_supported": ["omni"],
            "authorization_response_iss_parameter_supported": true,
            "client_id_metadata_document_supported": true,
        })
    );
}

fn chatgpt_document() -> Value {
    json!({
        "client_id": CIMD,
        "client_name": "ChatGPT",
        "redirect_uris": [REDIRECT],
        "token_endpoint_auth_method": "none",
    })
}

#[tokio::test]
async fn cimd_clients_are_resolved_from_their_metadata_document_and_cached() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/oauth/client.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chatgpt_document()))
        .expect(1)
        .mount(&mock)
        .await;
    let f = cimd_fixture(&mock).await;

    let first = start(&f, CIMD).await;
    // A second request from the same browser reuses its binding.
    let challenge = pkce::challenge(&pkce::verifier());
    let second = get(
        &f,
        &authorize_uri(CIMD, &[("code_challenge", &challenge)]),
        Some(&first.cookie),
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    assert!(header(&second, "set-cookie").starts_with(&format!("{COOKIE}={};", first.cookie)));

    assert!(
        first.html.contains("<strong>ChatGPT</strong>"),
        "{}",
        first.html
    );
    assert!(first.html.contains("<strong>chatgpt.com</strong>"));
    let pending = f.server.pending().await.unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(first.user_code, pending[0].user_code);
    assert_eq!(pending[0].browser_hash, pending[1].browser_hash);
    assert!(is_verified_client(&pending[0]));
    let cached = f
        .store
        .read(|docs| docs.get::<OAuthClientRow>(&CIMD.to_owned()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cached.kind, "cimd");
    assert_eq!(cached.client_name, "ChatGPT");
    assert_eq!(cached.redirect_uris, vec![REDIRECT]);
    mock.verify().await;
}

#[tokio::test]
async fn cimd_problems_render_an_error_page_and_never_redirect() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/oauth/client.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chatgpt_document()))
        .mount(&mock)
        .await;
    let mut other = chatgpt_document();
    other["client_id"] = json!("https://chatgpt.com/oauth/someone-else.json");
    Mock::given(method("GET"))
        .and(path("/oauth/mismatch.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(other))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/oauth/moved.json"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "https://chatgpt.com/oauth/client.json"),
        )
        .mount(&mock)
        .await;
    let f = cimd_fixture(&mock).await;
    let challenge = pkce::challenge(&pkce::verifier());

    let cases = [
        authorize_uri(
            CIMD,
            &[
                ("code_challenge", &challenge),
                ("redirect_uri", "https://evil.example/cb"),
            ],
        ),
        authorize_uri(
            "https://chatgpt.com/oauth/mismatch.json",
            &[("code_challenge", &challenge)],
        ),
        authorize_uri(
            "https://chatgpt.com/oauth/moved.json",
            &[("code_challenge", &challenge)],
        ),
        authorize_uri("unregistered-client", &[("code_challenge", &challenge)]),
        authorize_uri(
            CIMD,
            &[("code_challenge", &challenge), ("redirect_uri", "")],
        ),
        authorize_uri("", &[("code_challenge", &challenge)]),
    ];
    for uri in cases {
        let response = get(&f, &uri, None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        assert!(header(&response, "location").is_empty(), "{uri}");
        assert!(header(&response, "content-type").starts_with("text/html"));
    }
    assert!(f.server.pending().await.unwrap().is_empty());
    assert!(f.notified.lock().unwrap().is_empty());
}

#[tokio::test]
async fn pages_carry_a_strict_policy_and_a_bound_cookie() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;
    let headers = &started.response_headers;
    let csp = headers["content-security-policy"].to_str().unwrap();
    let nonce = attr(&started.html, "nonce");
    assert_eq!(
        csp,
        format!(
            "default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-{nonce}'; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
        )
    );
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert_eq!(headers["cache-control"], "no-store");
    let cookie = headers["set-cookie"].to_str().unwrap();
    assert!(cookie.starts_with(&format!("{COOKIE}={}; ", started.cookie)));
    for attribute in ["Path=/", "Secure", "HttpOnly", "SameSite=Lax"] {
        assert!(cookie.split("; ").any(|a| a == attribute), "{cookie}");
    }
    // Only the hash of the binding is stored.
    let row = &f.server.pending().await.unwrap()[0];
    assert_eq!(
        row.browser_hash,
        omni_core::digest::sha256_hex(&started.cookie)
    );
    assert!(!row.browser_hash.contains(&started.cookie));

    // A plain-http resource (tests, local) drops the __Host- prefix and Secure.
    let plain = fixture_with(
        PublicHttpClient::new(&omni_testkit::no_network()).allow_loopback_for_tests(),
        "http://127.0.0.1:8080/gateway/mcp",
    )
    .await;
    let client_id = register(&plain).await;
    let challenge = pkce::challenge(&pkce::verifier());
    let response = get(
        &plain,
        &authorize_uri(
            &client_id,
            &[
                ("code_challenge", &challenge),
                ("resource", "http://127.0.0.1:8080/gateway/mcp"),
            ],
        ),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = header(&response, "set-cookie");
    assert!(cookie.starts_with("omni-oauth="), "{cookie}");
    assert!(!cookie.contains("Secure"));
}

#[tokio::test(start_paused = true)]
async fn the_full_flow_issues_tokens_that_work_until_the_grant_is_revoked() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;

    let pending = f.server.pending().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, started.id);
    assert_eq!(pending[0].client_name, "ChatGPT");
    let user_code = pending[0].user_code.clone();
    assert_eq!(user_code.len(), 9);
    assert_eq!(started.user_code, user_code);
    // A DCR-registered app is flagged as unverified.
    assert!(!is_verified_client(&pending[0]));
    assert_eq!(f.notified.lock().unwrap().len(), 1);
    assert_eq!(f.notified.lock().unwrap()[0].id, started.id);
    assert_eq!(
        status_of(&f, &started.id, Some(&started.cookie)).await,
        (StatusCode::OK, json!({"status": "pending"}))
    );
    // Not approved yet: completing shows the waiting page again.
    let early = complete(&f, &started.id, Some(&started.cookie)).await;
    assert_eq!(early.status(), StatusCode::OK);

    // The owner types the code the waiting page shows.
    let typed = user_code.to_lowercase().replace('-', " ");
    assert_eq!(
        f.server.approve(&started.id, &typed).await.unwrap(),
        Approval::Approved
    );
    assert!(f.server.pending().await.unwrap().is_empty());
    assert_eq!(
        status_of(&f, &started.id, Some(&started.cookie)).await,
        (StatusCode::OK, json!({"status": "approved"}))
    );
    let response = complete(&f, &started.id, Some(&started.cookie)).await;
    let (target, query) = location(&response);
    assert_eq!(target, REDIRECT);
    assert_eq!(query["state"], "state-1");
    assert_eq!(query["iss"], ISSUER);
    let code = query["code"].clone();
    assert_eq!(code.len(), 43);

    let (status, tokens) = exchange(&f, &client_id, &code, &started.verifier).await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in"], 3600);
    assert_eq!(tokens["scope"], "omni");
    let access = tokens["access_token"].as_str().unwrap().to_owned();
    let refresh_token = tokens["refresh_token"].as_str().unwrap().to_owned();
    assert!(refresh_token.starts_with("ort_"));
    let principal = authenticates(&f, &access).await.unwrap();
    assert!(matches!(&principal, Principal::Grant { client_name, .. } if client_name == "ChatGPT"));

    // Nothing secret is stored in clear.
    let grants: Vec<GrantRow> = f
        .store
        .read(|docs| docs.get_all::<GrantRow>())
        .await
        .unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(
        grants[0].refresh_hash,
        omni_core::digest::sha256_hex(&refresh_token)
    );
    assert_eq!(grants[0].resource, RESOURCE);
    let row: AuthorizationRow = f
        .store
        .read({
            let id = started.id.clone();
            move |docs| docs.get::<AuthorizationRow>(&id)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.code_hash, Some(omni_core::digest::sha256_hex(&code)));

    tokio::time::advance(Duration::from_secs(30 * 60)).await;
    let (status, renewed) = refresh(&f, &client_id, &refresh_token).await;
    assert_eq!(status, StatusCode::OK, "{renewed}");
    let renewed_access = renewed["access_token"].as_str().unwrap().to_owned();
    let renewed_refresh = renewed["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(renewed_refresh, refresh_token);
    assert!(authenticates(&f, &renewed_access).await.is_some());

    let Principal::Grant { id, .. } = principal else {
        panic!("not a grant");
    };
    assert!(f.access.revoke_grant(&id).await.unwrap());
    assert!(authenticates(&f, &renewed_access).await.is_none());
    assert!(authenticates(&f, &access).await.is_none());
    let (status, body) = refresh(&f, &client_id, &renewed_refresh).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_wrong_or_missing_cookie_never_completes_a_request() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;
    assert_eq!(approve(&f, &started).await, Approval::Approved);
    let stranger = omni_core::ids::secure_id_b64url(32);

    assert_eq!(
        status_of(&f, &started.id, Some(&stranger)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status_of(&f, &started.id, None).await.0,
        StatusCode::FORBIDDEN
    );
    for cookie in [Some(stranger.as_str()), None, Some("not-a-valid-cookie")] {
        let response = complete(&f, &started.id, cookie).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(header(&response, "location").is_empty());
    }
    let unknown = complete(&f, "no-such-request", Some(&started.cookie)).await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    // The right browser still completes it, exactly once.
    let response = complete(&f, &started.id, Some(&started.cookie)).await;
    let (_, query) = location(&response);
    assert!(query.contains_key("code"));
    let again = complete(&f, &started.id, Some(&started.cookie)).await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
}

#[tokio::test(start_paused = true)]
async fn a_denied_request_returns_access_denied() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;

    assert!(f.server.deny(&started.id).await.unwrap());
    assert!(!f.server.deny(&started.id).await.unwrap());
    assert_eq!(approve(&f, &started).await, Approval::NotFound);
    assert_eq!(
        status_of(&f, &started.id, Some(&started.cookie)).await.1,
        json!({"status": "denied"})
    );
    let response = complete(&f, &started.id, Some(&started.cookie)).await;
    let (target, query) = location(&response);
    assert_eq!(target, REDIRECT);
    assert_eq!(query["error"], "access_denied");
    assert_eq!(query["state"], "state-1");
    assert_eq!(query["iss"], ISSUER);
    assert!(!query.contains_key("code"));
}

#[tokio::test(start_paused = true)]
async fn an_expired_request_cannot_be_approved_and_is_pruned() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;

    tokio::time::advance(Duration::from_secs(10 * 60 + 1)).await;

    assert!(f.server.pending().await.unwrap().is_empty());
    assert_eq!(
        status_of(&f, &started.id, Some(&started.cookie)).await.1,
        json!({"status": "expired"})
    );
    assert_eq!(approve(&f, &started).await, Approval::NotFound);
    let response = complete(&f, &started.id, Some(&started.cookie)).await;
    let (_, query) = location(&response);
    assert_eq!(query["error"], "access_denied");
    assert!(!query.contains_key("code"));

    f.server.prune().await.unwrap();
    assert_eq!(
        status_of(&f, &started.id, Some(&started.cookie)).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(start_paused = true)]
async fn an_approval_must_be_completed_before_the_request_expires() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;
    assert_eq!(approve(&f, &started).await, Approval::Approved);
    tokio::time::advance(Duration::from_secs(10 * 60)).await;
    let response = complete(&f, &started.id, Some(&started.cookie)).await;
    let (_, query) = location(&response);
    assert_eq!(query["error"], "access_denied");
}

#[tokio::test(start_paused = true)]
async fn a_code_works_once_and_a_replay_revokes_its_grant() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;
    let code = approve_and_complete(&f, &started).await;

    let (status, tokens) = exchange(&f, &client_id, &code, &started.verifier).await;
    assert_eq!(status, StatusCode::OK);
    let access = tokens["access_token"].as_str().unwrap().to_owned();
    assert!(authenticates(&f, &access).await.is_some());

    let (status, body) = exchange(&f, &client_id, &code, &started.verifier).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_grant");
    assert!(authenticates(&f, &access).await.is_none());
    let (status, _) = refresh(&f, &client_id, tokens["refresh_token"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(f.access.grants().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn codes_expire_after_a_minute() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;
    let code = approve_and_complete(&f, &started).await;
    tokio::time::advance(Duration::from_secs(61)).await;
    let (status, body) = exchange(&f, &client_id, &code, &started.verifier).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
}

#[tokio::test(start_paused = true)]
async fn the_exchange_checks_pkce_client_redirect_and_resource() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let other_client = register(&f).await;
    let started = start(&f, &client_id).await;
    let code = approve_and_complete(&f, &started).await;
    let base = |overrides: &[(&'static str, String)]| {
        let mut pairs: Vec<(&'static str, String)> = vec![
            ("grant_type", "authorization_code".into()),
            ("code", code.clone()),
            ("redirect_uri", REDIRECT.into()),
            ("client_id", client_id.clone()),
            ("code_verifier", started.verifier.clone()),
        ];
        for (k, v) in overrides {
            pairs.retain(|(name, _)| name != k);
            pairs.push((k, v.clone()));
        }
        pairs
    };
    let cases: [(&[(&'static str, String)], &str); 6] = [
        (&[("code_verifier", pkce::verifier())], "invalid_grant"),
        (&[("code_verifier", "short".into())], "invalid_request"),
        (&[("client_id", other_client.clone())], "invalid_grant"),
        (
            &[("redirect_uri", "https://chatgpt.com/other".into())],
            "invalid_grant",
        ),
        (
            &[("resource", "https://elsewhere.example/mcp".into())],
            "invalid_target",
        ),
        (&[("code", "unknown-code".into())], "invalid_grant"),
    ];
    for (overrides, expected) in cases {
        let pairs = base(overrides);
        let borrowed: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (status, body) = post_form(&f, &borrowed).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{overrides:?}");
        assert_eq!(body["error"], expected, "{overrides:?}: {body}");
    }
    // Failed attempts do not burn the code.
    let (status, _) = exchange(&f, &client_id, &code, &started.verifier).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn refresh_tokens_rotate_and_the_previous_one_works_for_two_minutes() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;
    let code = approve_and_complete(&f, &started).await;
    let (_, tokens) = exchange(&f, &client_id, &code, &started.verifier).await;
    let r0 = tokens["refresh_token"].as_str().unwrap().to_owned();

    tokio::time::advance(Duration::from_secs(10)).await;
    let (status, first) = refresh(&f, &client_id, &r0).await;
    assert_eq!(status, StatusCode::OK);
    let r1 = first["refresh_token"].as_str().unwrap().to_owned();

    // The response was lost and the client retries with the old token.
    tokio::time::advance(Duration::from_secs(20)).await;
    let (status, retried) = refresh(&f, &client_id, &r0).await;
    assert_eq!(status, StatusCode::OK, "{retried}");
    let r2 = retried["refresh_token"].as_str().unwrap().to_owned();
    assert_ne!(r2, r1);
    assert_ne!(retried["access_token"], first["access_token"]);
    assert!(
        authenticates(&f, retried["access_token"].as_str().unwrap())
            .await
            .is_some()
    );
    // The pair the retry replaced is gone.
    assert_eq!(
        refresh(&f, &client_id, &r1).await.1["error"],
        "invalid_grant"
    );

    tokio::time::advance(Duration::from_secs(101)).await;
    let other_client = register(&f).await;
    assert_eq!(
        refresh(&f, &other_client, &r2).await.1["error"],
        "invalid_grant"
    );
    let (status, next) = refresh(&f, &client_id, &r2).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(next["refresh_token"].as_str().unwrap(), r2);
}

#[tokio::test(start_paused = true)]
async fn a_rotated_refresh_token_presented_after_the_grace_window_revokes_the_grant() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;
    let code = approve_and_complete(&f, &started).await;
    let (_, tokens) = exchange(&f, &client_id, &code, &started.verifier).await;
    let r0 = tokens["refresh_token"].as_str().unwrap().to_owned();
    let (_, rotated) = refresh(&f, &client_id, &r0).await;
    let r1 = rotated["refresh_token"].as_str().unwrap().to_owned();
    let access = rotated["access_token"].as_str().unwrap().to_owned();
    assert!(authenticates(&f, &access).await.is_some());

    tokio::time::advance(Duration::from_secs(121)).await;
    let (status, body) = refresh(&f, &client_id, &r0).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_grant"))
    );
    // The whole grant is gone: its current refresh and access tokens too.
    assert!(f.access.grants().await.unwrap().is_empty());
    assert!(authenticates(&f, &access).await.is_none());
    assert_eq!(
        refresh(&f, &client_id, &r1).await.1["error"],
        "invalid_grant"
    );
}

#[tokio::test(start_paused = true)]
async fn refresh_tokens_expire_after_ninety_days_unused() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;
    let code = approve_and_complete(&f, &started).await;
    let (_, tokens) = exchange(&f, &client_id, &code, &started.verifier).await;
    f.clock.set(EPOCH_MS + 91 * 24 * 60 * 60 * 1000);
    let (_, body) = refresh(&f, &client_id, tokens["refresh_token"].as_str().unwrap()).await;
    assert_eq!(body["error"], "invalid_grant");
    f.server.prune().await.unwrap();
    assert!(f.access.grants().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn authorization_errors_redirect_once_the_redirect_uri_is_trusted() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let challenge = pkce::challenge(&pkce::verifier());
    let cases: [(Vec<(&str, &str)>, &str); 7] = [
        (vec![], "invalid_request"),
        (
            vec![
                ("code_challenge", &challenge),
                ("code_challenge_method", "plain"),
            ],
            "invalid_request",
        ),
        (
            vec![
                ("code_challenge", &challenge),
                ("code_challenge_method", ""),
            ],
            "invalid_request",
        ),
        (vec![("code_challenge", "too-short")], "invalid_request"),
        (
            vec![("code_challenge", &challenge), ("scope", "omni admin")],
            "invalid_scope",
        ),
        (
            vec![
                ("code_challenge", &challenge),
                ("resource", "https://other.example/mcp"),
            ],
            "invalid_target",
        ),
        (
            vec![("code_challenge", &challenge), ("response_type", "token")],
            "unsupported_response_type",
        ),
    ];
    for (extra, expected) in cases {
        let response = get(&f, &authorize_uri(&client_id, &extra), None).await;
        let (target, query) = location(&response);
        assert_eq!(target, REDIRECT);
        assert_eq!(query["error"], expected, "{extra:?}");
        assert!(query.contains_key("error_description"));
        assert_eq!(query["state"], "state-1");
        assert_eq!(query["iss"], ISSUER);
    }
    // Scope and resource may be omitted.
    let response = get(
        &f,
        &authorize_uri(
            &client_id,
            &[
                ("code_challenge", &challenge),
                ("scope", ""),
                ("resource", ""),
            ],
        ),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(f.notified.lock().unwrap().len() == 1);
}

#[tokio::test(start_paused = true)]
async fn waiting_requests_are_capped() {
    let f = fixture().await;
    let client_id = register(&f).await;
    for _ in 0..20 {
        start(&f, &client_id).await;
    }
    let challenge = pkce::challenge(&pkce::verifier());
    let response = get(
        &f,
        &authorize_uri(&client_id, &[("code_challenge", &challenge)]),
        None,
    )
    .await;
    let (_, query) = location(&response);
    assert_eq!(query["error"], "temporarily_unavailable");
    assert_eq!(f.notified.lock().unwrap().len(), 20);
    // Expired requests stop counting.
    tokio::time::advance(Duration::from_secs(10 * 60)).await;
    start(&f, &client_id).await;
}

#[tokio::test(start_paused = true)]
async fn dynamic_registration_is_validated() {
    let f = fixture().await;
    let (status, body) = post_json(
        &f,
        "/gateway/oauth/register",
        json!({
            "client_name": "ChatGPT",
            "redirect_uris": [REDIRECT, "http://127.0.0.1:9000/cb"],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["client_name"], "ChatGPT");
    assert_eq!(
        body["redirect_uris"],
        json!([REDIRECT, "http://127.0.0.1:9000/cb"])
    );
    assert_eq!(body["token_endpoint_auth_method"], "none");
    assert_eq!(body["client_id_issued_at"], EPOCH_MS / 1000);
    let id = body["client_id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 36);

    let refused = [
        (
            json!({"redirect_uris": [REDIRECT], "token_endpoint_auth_method": "client_secret_basic"}),
            "invalid_client_metadata",
        ),
        (json!({"client_name": "x"}), "invalid_redirect_uri"),
        (json!({"redirect_uris": []}), "invalid_redirect_uri"),
        (
            json!({"redirect_uris": ["http://example.com/cb"]}),
            "invalid_redirect_uri",
        ),
        (
            json!({"redirect_uris": ["https://example.com/cb#x"]}),
            "invalid_redirect_uri",
        ),
        (
            json!({"redirect_uris": [REDIRECT], "client_name": "x".repeat(101)}),
            "invalid_client_metadata",
        ),
        (
            json!({"redirect_uris": [REDIRECT], "grant_types": ["client_credentials"]}),
            "invalid_client_metadata",
        ),
        (json!(["not", "an", "object"]), "invalid_client_metadata"),
    ];
    for (request, expected) in refused {
        let (status, body) = post_json(&f, "/gateway/oauth/register", request.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{request}");
        assert_eq!(body["error"], expected, "{request}");
    }

    // A name defaults to the redirect host.
    let (_, unnamed) = post_json(
        &f,
        "/gateway/oauth/register",
        json!({"redirect_uris": [REDIRECT]}),
    )
    .await;
    assert_eq!(unnamed["client_name"], "chatgpt.com");
}

fn dcr_client(index: i64) -> OAuthClientRow {
    OAuthClientRow {
        client_id: omni_core::ids::uuid_v4(),
        kind: "dcr".into(),
        client_name: format!("app {index}"),
        redirect_uris: vec![REDIRECT.to_owned()],
        created_at: EPOCH_MS - 100_000 + index,
        extra: Default::default(),
    }
}

fn grant_for(client_id: &str) -> GrantRow {
    GrantRow {
        id: omni_core::ids::secure_id_b64url(16),
        client_id: client_id.to_owned(),
        client_name: client_id.to_owned(),
        resource: RESOURCE.into(),
        scope: Some("omni".into()),
        refresh_hash: omni_core::ids::secure_id_b64url(32),
        refresh_expires_at: EPOCH_MS + 86_400_000,
        previous_refresh_hash: None,
        rotated_at: None,
        created_at: EPOCH_MS,
        last_used_at: None,
        extra: Default::default(),
    }
}

async fn dcr_ids(f: &Fixture) -> Vec<String> {
    f.store
        .read(|docs| docs.get_all::<OAuthClientRow>())
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.kind == "dcr")
        .map(|c| c.client_id)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_full_registry_evicts_the_oldest_idle_client() {
    let f = fixture().await;
    let clients: Vec<OAuthClientRow> = (0..50).map(dcr_client).collect();
    let granted = clients[0].client_id.clone();
    let waiting = clients[1].client_id.clone();
    let oldest_idle = clients[2].client_id.clone();
    let seeded = clients.clone();
    let grant = grant_for(&granted);
    f.store
        .write(move |tx| {
            for client in &seeded {
                tx.upsert(client, UpsertOpts::default())?;
            }
            tx.upsert(&grant, UpsertOpts::default())
        })
        .await
        .unwrap();
    // The second-oldest client has a request waiting for the owner.
    start(&f, &waiting).await;

    let added = register(&f).await;

    let left = dcr_ids(&f).await;
    assert_eq!(left.len(), 50);
    assert!(left.contains(&granted) && left.contains(&waiting) && left.contains(&added));
    assert!(!left.contains(&oldest_idle));
}

#[tokio::test(start_paused = true)]
async fn a_full_registry_of_clients_in_use_refuses_registration() {
    let f = fixture().await;
    let clients: Vec<OAuthClientRow> = (0..50).map(dcr_client).collect();
    let grants: Vec<GrantRow> = clients.iter().map(|c| grant_for(&c.client_id)).collect();
    f.store
        .write(move |tx| {
            for client in &clients {
                tx.upsert(client, UpsertOpts::default())?;
            }
            for grant in &grants {
                tx.upsert(grant, UpsertOpts::default())?;
            }
            Ok::<_, omni_store::StoreError>(())
        })
        .await
        .unwrap();
    let (status, body) = post_json(
        &f,
        "/gateway/oauth/register",
        json!({"redirect_uris": [REDIRECT]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_client_metadata");
    assert_eq!(dcr_ids(&f).await.len(), 50);
}

#[tokio::test]
async fn the_token_endpoint_refuses_malformed_requests() {
    let f = fixture().await;
    let (status, body) = post_form(&f, &[("grant_type", "password"), ("username", "u")]).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("unsupported_grant_type"))
    );
    let (_, body) = post_form(&f, &[("code", "x")]).await;
    assert_eq!(body["error"], "invalid_request");
    let (_, body) = post_form(
        &f,
        &[
            ("grant_type", "refresh_token"),
            ("grant_type", "refresh_token"),
        ],
    )
    .await;
    assert_eq!(body["error"], "invalid_request");
    let (_, body) = post_form(&f, &[("grant_type", "refresh_token"), ("client_id", "c")]).await;
    assert_eq!(body["error"], "invalid_request");
    let (_, body) = post_form(
        &f,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", "c"),
            ("refresh_token", "ort_unknown"),
        ],
    )
    .await;
    assert_eq!(body["error"], "invalid_grant");

    let response = send(
        &f,
        Request::builder()
            .method("POST")
            .uri("/gateway/oauth/token")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"grant_type":"authorization_code"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(response).await["error"], "invalid_request");
}

#[tokio::test]
async fn prune_drops_stale_clients_but_keeps_those_in_use() {
    let f = fixture().await;
    let day = 24 * 60 * 60 * 1000;
    let client = |id: &str, kind: &str, age_days: i64| OAuthClientRow {
        client_id: id.to_owned(),
        kind: kind.to_owned(),
        client_name: id.to_owned(),
        redirect_uris: vec![REDIRECT.to_owned()],
        created_at: EPOCH_MS - age_days * day,
        extra: Default::default(),
    };
    let rows = vec![
        client("dcr-stale", "dcr", 31),
        client("dcr-fresh", "dcr", 29),
        client("dcr-granted", "dcr", 40),
        client("https://old.example/client.json", "cimd", 2),
        client("https://new.example/client.json", "cimd", 0),
    ];
    let grant = GrantRow {
        id: "grant-1".into(),
        client_id: "dcr-granted".into(),
        client_name: "dcr-granted".into(),
        resource: RESOURCE.into(),
        scope: Some("omni".into()),
        refresh_hash: "h".into(),
        refresh_expires_at: EPOCH_MS + day,
        previous_refresh_hash: None,
        rotated_at: None,
        created_at: EPOCH_MS - 40 * day,
        last_used_at: None,
        extra: Default::default(),
    };
    f.store
        .write(move |tx| {
            for row in &rows {
                tx.upsert(row, UpsertOpts::default())?;
            }
            tx.upsert(&grant, UpsertOpts::default())
        })
        .await
        .unwrap();

    f.server.prune().await.unwrap();

    let mut left: Vec<String> = f
        .store
        .read(|docs| docs.get_all::<OAuthClientRow>())
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.client_id)
        .collect();
    left.sort();
    assert_eq!(
        left,
        vec![
            "dcr-fresh",
            "dcr-granted",
            "https://new.example/client.json"
        ]
    );
}

#[tokio::test]
async fn metadata_documents_off_the_allowlist_are_refused_without_a_fetch() {
    let mock = MockServer::start().await;
    let f = cimd_fixture(&mock).await;
    let challenge = pkce::challenge(&pkce::verifier());
    for client_id in [
        "https://evil.example/client.json",
        "https://chatgpt.com.evil.example/client.json",
        "http://chatgpt.com/oauth/client.json",
        "https://chatgpt.com:8443/oauth/client.json",
        "not-a-registered-id",
    ] {
        let response = get(
            &f,
            &authorize_uri(client_id, &[("code_challenge", &challenge)]),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{client_id}");
        assert!(text(response).await.contains("not registered with Omni"));
    }
    assert!(mock.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn documents_with_bad_redirect_uris_are_refused_and_remembered() {
    let mock = MockServer::start().await;
    let mut insecure = chatgpt_document();
    insecure["client_id"] = json!("https://chatgpt.com/oauth/insecure.json");
    insecure["redirect_uris"] = json!([REDIRECT, "http://example.com/cb"]);
    Mock::given(method("GET"))
        .and(path("/oauth/insecure.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(insecure))
        .expect(1)
        .mount(&mock)
        .await;
    let mut many = chatgpt_document();
    many["client_id"] = json!("https://chatgpt.com/oauth/many.json");
    many["redirect_uris"] = json!(
        (0..11)
            .map(|i| format!("https://chatgpt.com/cb/{i}"))
            .collect::<Vec<_>>()
    );
    Mock::given(method("GET"))
        .and(path("/oauth/many.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(many))
        .expect(1)
        .mount(&mock)
        .await;
    let f = cimd_fixture(&mock).await;
    let challenge = pkce::challenge(&pkce::verifier());

    // Twice each: the failure is remembered, so the second try fetches nothing.
    for client_id in [
        "https://chatgpt.com/oauth/insecure.json",
        "https://chatgpt.com/oauth/many.json",
    ] {
        for _ in 0..2 {
            let response = get(
                &f,
                &authorize_uri(client_id, &[("code_challenge", &challenge)]),
                None,
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{client_id}");
            assert!(header(&response, "location").is_empty());
        }
    }
    let cached = f
        .store
        .read(|docs| docs.get_all::<OAuthClientRow>())
        .await
        .unwrap();
    assert!(cached.is_empty());
    mock.verify().await;
}

#[tokio::test]
async fn cached_metadata_documents_are_capped() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/oauth/client.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chatgpt_document()))
        .mount(&mock)
        .await;
    let f = cimd_fixture(&mock).await;
    let old: Vec<OAuthClientRow> = (0..20)
        .map(|i| OAuthClientRow {
            client_id: format!("https://chatgpt.com/oauth/old-{i}.json"),
            kind: "cimd".into(),
            client_name: "old".into(),
            redirect_uris: vec![REDIRECT.into()],
            created_at: EPOCH_MS - 1_000 + i,
            extra: Default::default(),
        })
        .collect();
    f.store
        .write(move |tx| {
            for row in &old {
                tx.upsert(row, UpsertOpts::default())?;
            }
            Ok::<_, omni_store::StoreError>(())
        })
        .await
        .unwrap();

    start(&f, CIMD).await;

    let ids: Vec<String> = f
        .store
        .read(|docs| docs.get_all::<OAuthClientRow>())
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.client_id)
        .collect();
    assert_eq!(ids.len(), 20);
    assert!(ids.contains(&CIMD.to_owned()));
    assert!(!ids.contains(&"https://chatgpt.com/oauth/old-0.json".to_owned()));
    assert!(ids.contains(&"https://chatgpt.com/oauth/old-1.json".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn the_rate_limiter_answers_429_with_retry_after() {
    let f = fixture().await;
    let metadata = "/.well-known/oauth-authorization-server";
    for _ in 0..60 {
        assert_eq!(get(&f, metadata, None).await.status(), StatusCode::OK);
    }
    let limited = get(&f, metadata, None).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&limited, "retry-after"), "1");
    assert_eq!(header(&limited, "cache-control"), "no-store");
    assert_eq!(json_body(limited).await["error"], "temporarily_unavailable");
    // The token endpoint has its own bucket: refreshes still get answers.
    let (status, _) = post_form(&f, &[("grant_type", "refresh_token")]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for _ in 0..59 {
        post_form(&f, &[("grant_type", "refresh_token")]).await;
    }
    let (status, body) = post_form(&f, &[("grant_type", "refresh_token")]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"], "temporarily_unavailable");
    let page = get(&f, &authorize_uri("anything", &[]), None).await;
    assert_eq!(page.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(header(&page, "content-type").starts_with("text/html"));
    assert_eq!(header(&page, "retry-after"), "1");

    // 20 requests per second refill.
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..20 {
        assert_eq!(get(&f, metadata, None).await.status(), StatusCode::OK);
    }
    assert_eq!(
        get(&f, metadata, None).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test(start_paused = true)]
async fn approval_needs_the_code_the_requesting_browser_shows() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;
    let other = start(&f, &client_id).await;
    assert_ne!(started.user_code, other.user_code);

    // Another request's code (the phishing case) does not approve this one.
    assert_eq!(
        f.server
            .approve(&started.id, &other.user_code)
            .await
            .unwrap(),
        Approval::WrongCode
    );
    assert_eq!(
        status_of(&f, &started.id, Some(&started.cookie)).await.1,
        json!({"status": "pending"})
    );
    assert_eq!(
        f.server
            .approve("unknown-request-id-000", &started.user_code)
            .await
            .unwrap(),
        Approval::NotFound
    );
    assert_eq!(
        f.server
            .approve("bad id", &started.user_code)
            .await
            .unwrap(),
        Approval::NotFound
    );
    let typed = format!(" {} ", started.user_code.to_lowercase());
    assert_eq!(
        f.server.approve(&started.id, &typed).await.unwrap(),
        Approval::Approved
    );
    // Already decided.
    assert_eq!(approve(&f, &started).await, Approval::NotFound);
    assert_eq!(
        status_of(&f, &other.id, Some(&other.cookie)).await.1,
        json!({"status": "pending"})
    );
}

#[tokio::test(start_paused = true)]
async fn five_wrong_codes_deny_the_request() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;
    for attempt in 1..=5 {
        assert_eq!(
            f.server.approve(&started.id, "AAAA-AAAA").await.unwrap(),
            Approval::WrongCode,
            "attempt {attempt}"
        );
        let expected = if attempt < 5 { "pending" } else { "denied" };
        assert_eq!(
            status_of(&f, &started.id, Some(&started.cookie)).await.1,
            json!({"status": expected}),
            "attempt {attempt}"
        );
    }
    assert_eq!(approve(&f, &started).await, Approval::NotFound);
    let response = complete(&f, &started.id, Some(&started.cookie)).await;
    let (_, query) = location(&response);
    assert_eq!(query["error"], "access_denied");
}

#[tokio::test(start_paused = true)]
async fn malformed_values_are_refused_up_front() {
    let f = fixture().await;
    let client_id = register(&f).await;
    let started = start(&f, &client_id).await;

    let (status, _) = status_of(&f, "not-an-id", Some(&started.cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let response = complete(&f, &format!("{}x", started.id), Some(&started.cookie)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let challenge = pkce::challenge(&pkce::verifier());
    let long_state = "s".repeat(2049);
    for extra in [
        vec![
            ("code_challenge", challenge.as_str()),
            ("state", long_state.as_str()),
        ],
        vec![
            ("code_challenge", challenge.as_str()),
            ("redirect_uri", "http://example.com/cb"),
        ],
    ] {
        let response = get(&f, &authorize_uri(&client_id, &extra), None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(header(&response, "location").is_empty());
    }

    let (_, body) = exchange(&f, &client_id, "short-code", &started.verifier).await;
    assert_eq!(body["error"], "invalid_grant");
    for token in [
        "ort_short".to_owned(),
        omni_core::ids::secure_id_b64url(32),
        format!("ort_{}", omni_core::ids::secure_id_b64url(32)),
    ] {
        let (_, body) = refresh(&f, &client_id, &token).await;
        assert_eq!(body["error"], "invalid_grant");
    }
}
