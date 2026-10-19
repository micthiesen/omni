//! The upstream OAuth client: discovery, registration, the authorization
//! URL and callback, and token exchange and refresh, against wiremock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use omni_http::Url;
use omni_http::public::PublicHttpClient;
use omni_mcp::gateway::oauth::client::{
    Discovery, OAuthClient, OAuthError, Registration, TokenSet,
};
use omni_mcp::gateway::oauth::pkce;
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NOW: i64 = 1_790_000_000_000;

fn client() -> OAuthClient {
    OAuthClient::new(PublicHttpClient::new(&omni_testkit::no_network()).allow_loopback_for_tests())
}

fn mcp_url(server: &MockServer) -> Url {
    Url::parse(&format!("{}/mcp", server.uri())).unwrap()
}

fn as_metadata(issuer: &str) -> Value {
    json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "registration_endpoint": format!("{issuer}/register"),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "none"],
        "authorization_response_iss_parameter_supported": true,
        "scopes_supported": ["as.read"],
    })
}

async fn mount_json(server: &MockServer, at: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn discovery(server: &MockServer, methods: &[&str]) -> Discovery {
    let issuer = server.uri();
    Discovery {
        resource: format!("{issuer}/mcp"),
        issuer: issuer.clone(),
        authorization_endpoint: format!("{issuer}/authorize"),
        token_endpoint: format!("{issuer}/token"),
        registration_endpoint: Some(format!("{issuer}/register")),
        scopes: vec!["read".into(), "write".into()],
        token_auth_methods: methods.iter().map(|m| (*m).to_owned()).collect(),
        iss_parameter_supported: true,
    }
}

fn registration(token_auth: &str, secret: Option<&str>) -> Registration {
    Registration {
        client_id: "client:1".into(),
        client_secret: secret.map(str::to_owned),
        token_auth: token_auth.into(),
    }
}

fn form(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

#[tokio::test]
async fn discovery_follows_the_resource_metadata_in_www_authenticate() {
    let server = MockServer::start().await;
    let issuer = format!("{}/tenant", server.uri());
    mount_json(
        &server,
        "/custom/prm",
        json!({
            "resource": mcp_url(&server).as_str(),
            "authorization_servers": [issuer],
            "scopes_supported": ["mcp.read", "mcp.write"],
        }),
    )
    .await;
    let mut metadata = as_metadata(&issuer);
    metadata["authorization_endpoint"] = json!(format!("{}/authorize", server.uri()));
    mount_json(
        &server,
        "/.well-known/oauth-authorization-server/tenant",
        metadata,
    )
    .await;
    let challenge = format!(
        r#"Bearer error="invalid_token", resource_metadata="{}/custom/prm""#,
        server.uri()
    );

    let found = client()
        .discover(&mcp_url(&server), Some(&challenge))
        .await
        .unwrap();

    assert_eq!(found.resource, mcp_url(&server).as_str());
    assert_eq!(found.issuer, issuer);
    assert_eq!(
        found.authorization_endpoint,
        format!("{}/authorize", server.uri())
    );
    assert_eq!(found.token_endpoint, format!("{issuer}/token"));
    assert_eq!(
        found.registration_endpoint,
        Some(format!("{issuer}/register"))
    );
    assert_eq!(found.scopes, vec!["mcp.read", "mcp.write"]);
    assert_eq!(found.token_auth_methods, vec!["client_secret_post", "none"]);
    assert!(found.iss_parameter_supported);
}

#[tokio::test]
async fn discovery_falls_back_to_the_well_known_paths() {
    let server = MockServer::start().await;
    // No path-inserted resource metadata, so the root form is used; no
    // OAuth metadata, so OpenID configuration is.
    mount_json(
        &server,
        "/.well-known/oauth-protected-resource",
        json!({
            "resource": format!("{}/", mcp_url(&server)),
            "authorization_servers": [server.uri()],
        }),
    )
    .await;
    let mut metadata = as_metadata(&server.uri());
    metadata
        .as_object_mut()
        .unwrap()
        .remove("token_endpoint_auth_methods_supported");
    metadata
        .as_object_mut()
        .unwrap()
        .remove("authorization_response_iss_parameter_supported");
    mount_json(&server, "/.well-known/openid-configuration", metadata).await;

    let found = client().discover(&mcp_url(&server), None).await.unwrap();

    assert_eq!(found.issuer, server.uri());
    // The resource's trailing slash is tolerated; the AS scopes apply.
    assert_eq!(found.resource, format!("{}/", mcp_url(&server)));
    assert_eq!(found.scopes, vec!["as.read"]);
    // RFC 8414 defaults.
    assert_eq!(found.token_auth_methods, vec!["client_secret_basic"]);
    assert!(!found.iss_parameter_supported);
    let requested: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect();
    assert_eq!(
        requested,
        vec![
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-authorization-server",
            "/.well-known/openid-configuration",
        ]
    );
}

#[tokio::test]
async fn the_challenge_scope_wins_over_metadata_scopes() {
    let server = MockServer::start().await;
    mount_json(
        &server,
        "/.well-known/oauth-protected-resource/mcp",
        json!({
            "resource": mcp_url(&server).as_str(),
            "authorization_servers": [server.uri()],
            "scopes_supported": ["a", "b"],
        }),
    )
    .await;
    mount_json(
        &server,
        "/.well-known/oauth-authorization-server",
        as_metadata(&server.uri()),
    )
    .await;
    let found = client()
        .discover(&mcp_url(&server), Some(r#"Bearer scope="files:read""#))
        .await
        .unwrap();
    assert_eq!(found.scopes, vec!["files:read"]);
}

#[tokio::test]
async fn an_issuer_mismatch_is_rejected() {
    let server = MockServer::start().await;
    mount_json(
        &server,
        "/.well-known/oauth-protected-resource/mcp",
        json!({"resource": mcp_url(&server).as_str(), "authorization_servers": [server.uri()]}),
    )
    .await;
    mount_json(
        &server,
        "/.well-known/oauth-authorization-server",
        as_metadata("https://attacker.example"),
    )
    .await;

    let error = client()
        .discover(&mcp_url(&server), None)
        .await
        .unwrap_err();

    assert!(
        matches!(&error, OAuthError::Discovery(m) if m.contains("attacker.example")),
        "{error}"
    );
}

#[tokio::test]
async fn a_resource_naming_another_url_is_rejected() {
    let server = MockServer::start().await;
    mount_json(
        &server,
        "/.well-known/oauth-protected-resource/mcp",
        json!({
            "resource": format!("{}/other", server.uri()),
            "authorization_servers": [server.uri()],
        }),
    )
    .await;
    let error = client()
        .discover(&mcp_url(&server), None)
        .await
        .unwrap_err();
    assert!(matches!(error, OAuthError::Discovery(_)));
}

#[tokio::test]
async fn servers_without_s256_are_rejected() {
    let server = MockServer::start().await;
    mount_json(
        &server,
        "/.well-known/oauth-protected-resource/mcp",
        json!({"resource": mcp_url(&server).as_str(), "authorization_servers": [server.uri()]}),
    )
    .await;
    let mut metadata = as_metadata(&server.uri());
    metadata["code_challenge_methods_supported"] = json!(["plain"]);
    mount_json(&server, "/.well-known/oauth-authorization-server", metadata).await;
    let error = client()
        .discover(&mcp_url(&server), None)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, OAuthError::Discovery(m) if m.contains("S256")),
        "{error}"
    );
}

#[tokio::test]
async fn without_resource_metadata_the_origin_is_the_authorization_server() {
    let server = MockServer::start().await;
    mount_json(
        &server,
        "/.well-known/oauth-authorization-server",
        as_metadata(&server.uri()),
    )
    .await;
    let found = client().discover(&mcp_url(&server), None).await.unwrap();
    assert_eq!(found.issuer, server.uri());
    assert_eq!(found.resource, mcp_url(&server).as_str());
    assert_eq!(found.scopes, vec!["as.read"]);
}

#[tokio::test]
async fn registration_prefers_the_public_none_method() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "client_id": "dyn-1",
            "token_endpoint_auth_method": "none",
        })))
        .mount(&server)
        .await;

    let registered = client()
        .register(
            &discovery(&server, &["client_secret_basic", "none"]),
            "https://omni.example/gateway/oauth/callback",
            "Omni",
        )
        .await
        .unwrap();

    assert_eq!(registered, registration_with("dyn-1", None, "none"));
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body,
        json!({
            "client_name": "Omni",
            "redirect_uris": ["https://omni.example/gateway/oauth/callback"],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })
    );
}

fn registration_with(id: &str, secret: Option<&str>, auth: &str) -> Registration {
    Registration {
        client_id: id.into(),
        client_secret: secret.map(str::to_owned),
        token_auth: auth.into(),
    }
}

#[tokio::test]
async fn confidential_registration_keeps_the_secret_out_of_debug_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/register"))
        .and(body_string_contains("client_secret_basic"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "client_id": "dyn-2",
            "client_secret": "very-secret-value",
        })))
        .mount(&server)
        .await;

    let registered = client()
        .register(
            &discovery(&server, &["client_secret_basic"]),
            "https://omni.example/cb",
            "Omni",
        )
        .await
        .unwrap();

    assert_eq!(
        registered,
        registration_with("dyn-2", Some("very-secret-value"), "client_secret_basic")
    );
    assert!(!format!("{registered:?}").contains("very-secret-value"));
}

#[tokio::test]
async fn servers_without_dynamic_registration_are_not_supported() {
    let server = MockServer::start().await;
    let mut d = discovery(&server, &["none"]);
    d.registration_endpoint = None;
    let error = client()
        .register(&d, "https://omni.example/cb", "Omni")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, OAuthError::Registration(m) if m.contains("pre-registered")),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn the_authorize_url_carries_pkce_resource_state_and_scopes() {
    let d = Discovery {
        resource: "https://mcp.example/mcp".into(),
        issuer: "https://auth.example".into(),
        authorization_endpoint: "https://auth.example/authorize?tenant=1".into(),
        token_endpoint: "https://auth.example/token".into(),
        registration_endpoint: None,
        scopes: vec!["read".into(), "write".into()],
        token_auth_methods: vec!["none".into()],
        iss_parameter_supported: true,
    };
    let verifier = pkce::verifier();
    let challenge = pkce::challenge(&verifier);
    let url = client()
        .authorize_url(
            &d,
            &registration("none", None),
            "https://omni.example/cb",
            "state-1",
            &challenge,
        )
        .unwrap();
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(url.path(), "/authorize");
    assert_eq!(
        query,
        HashMap::from([
            ("tenant".to_owned(), "1".to_owned()),
            ("response_type".to_owned(), "code".to_owned()),
            ("client_id".to_owned(), "client:1".to_owned()),
            (
                "redirect_uri".to_owned(),
                "https://omni.example/cb".to_owned()
            ),
            ("state".to_owned(), "state-1".to_owned()),
            ("code_challenge".to_owned(), challenge.clone()),
            ("code_challenge_method".to_owned(), "S256".to_owned()),
            ("resource".to_owned(), "https://mcp.example/mcp".to_owned()),
            ("scope".to_owned(), "read write".to_owned()),
        ])
    );
    assert!(!url.as_str().contains(&verifier));

    let unscoped = Discovery {
        scopes: Vec::new(),
        ..d
    };
    let url = client()
        .authorize_url(
            &unscoped,
            &registration("none", None),
            "https://omni.example/cb",
            "s",
            &challenge,
        )
        .unwrap();
    assert!(!url.query_pairs().any(|(k, _)| k == "scope"));
}

fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn the_callback_checks_iss_state_and_errors() {
    let issuer = "https://auth.example";
    let ok = query(&[("code", "c-1"), ("state", "s-1"), ("iss", issuer)]);
    assert_eq!(
        OAuthClient::callback_code(&ok, "s-1", issuer, true).unwrap(),
        "c-1"
    );

    let wrong_state = query(&[("code", "c-1"), ("state", "s-2"), ("iss", issuer)]);
    assert!(matches!(
        OAuthClient::callback_code(&wrong_state, "s-1", issuer, true),
        Err(OAuthError::Protocol(m)) if m.contains("state")
    ));

    let mixed_up = query(&[
        ("code", "c-1"),
        ("state", "s-1"),
        ("iss", "https://evil.example"),
    ]);
    assert!(matches!(
        OAuthClient::callback_code(&mixed_up, "s-1", issuer, false),
        Err(OAuthError::Protocol(m)) if m.contains("issuer")
    ));

    let no_iss = query(&[("code", "c-1"), ("state", "s-1")]);
    assert!(matches!(
        OAuthClient::callback_code(&no_iss, "s-1", issuer, true),
        Err(OAuthError::Protocol(m)) if m.contains("iss")
    ));
    assert_eq!(
        OAuthClient::callback_code(&no_iss, "s-1", issuer, false).unwrap(),
        "c-1"
    );

    let denied = query(&[
        ("error", "access_denied"),
        ("error_description", "The user said no"),
        ("state", "s-1"),
        ("iss", issuer),
    ]);
    let error = OAuthClient::callback_code(&denied, "s-1", issuer, true).unwrap_err();
    assert!(matches!(
        &error,
        OAuthError::Authorization { error, description }
            if error == "access_denied" && description.as_deref() == Some("The user said no")
    ));
    assert!(error.to_string().contains("The user said no"));

    let no_code = query(&[("state", "s-1"), ("iss", issuer)]);
    assert!(matches!(
        OAuthClient::callback_code(&no_code, "s-1", issuer, true),
        Err(OAuthError::Protocol(_))
    ));
    let error = OAuthClient::callback_code(&wrong_state, "s-1", issuer, true).unwrap_err();
    assert!(!error.to_string().contains("c-1"));
}

fn token_response(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

#[tokio::test]
async fn exchange_with_client_secret_basic_url_encodes_the_credentials() {
    let server = MockServer::start().await;
    let expected = format!("Basic {}", STANDARD.encode("client%3A1:s+cr%26t"));
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(header("authorization", expected.as_str()))
        .respond_with(token_response(json!({
            "access_token": "at-1",
            "token_type": "bearer",
            "expires_in": 3600,
            "refresh_token": "rt-1",
            "scope": "read",
        })))
        .mount(&server)
        .await;
    let verifier = pkce::verifier();

    let tokens = client()
        .exchange(
            &format!("{}/token", server.uri()),
            &registration("client_secret_basic", Some("s cr&t")),
            "code-1",
            &verifier,
            "https://omni.example/cb",
            "https://mcp.example/mcp",
            NOW,
        )
        .await
        .unwrap();

    assert_eq!(
        tokens,
        TokenSet {
            access_token: "at-1".into(),
            refresh_token: Some("rt-1".into()),
            expires_at: Some(NOW + 3_600_000),
            scope: Some("read".into()),
        }
    );
    let requests = server.received_requests().await.unwrap();
    let sent = form(&requests[0].body);
    assert_eq!(
        sent,
        HashMap::from([
            ("grant_type".to_owned(), "authorization_code".to_owned()),
            ("code".to_owned(), "code-1".to_owned()),
            (
                "redirect_uri".to_owned(),
                "https://omni.example/cb".to_owned()
            ),
            ("code_verifier".to_owned(), verifier),
            ("resource".to_owned(), "https://mcp.example/mcp".to_owned()),
        ])
    );
    assert_eq!(
        requests[0].headers.get("content-type").unwrap(),
        "application/x-www-form-urlencoded"
    );
}

#[tokio::test]
async fn exchange_with_client_secret_post_and_none_send_the_client_in_the_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(token_response(
            json!({"access_token": "at", "token_type": "Bearer"}),
        ))
        .mount(&server)
        .await;
    let endpoint = format!("{}/token", server.uri());
    let c = client();
    let verifier = pkce::verifier();
    let post = c
        .exchange(
            &endpoint,
            &registration("client_secret_post", Some("pw")),
            "code",
            &verifier,
            "https://omni.example/cb",
            "https://mcp.example/mcp",
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(post.expires_at, None);
    assert_eq!(post.refresh_token, None);
    c.exchange(
        &endpoint,
        &registration("none", None),
        "code",
        &verifier,
        "https://omni.example/cb",
        "https://mcp.example/mcp",
        NOW,
    )
    .await
    .unwrap();

    let requests = server.received_requests().await.unwrap();
    let post_form = form(&requests[0].body);
    assert_eq!(
        post_form.get("client_id").map(String::as_str),
        Some("client:1")
    );
    assert_eq!(
        post_form.get("client_secret").map(String::as_str),
        Some("pw")
    );
    assert!(requests[0].headers.get("authorization").is_none());
    let none_form = form(&requests[1].body);
    assert_eq!(
        none_form.get("client_id").map(String::as_str),
        Some("client:1")
    );
    assert!(!none_form.contains_key("client_secret"));
    assert!(requests[1].headers.get("authorization").is_none());

    let missing_secret = c
        .exchange(
            &endpoint,
            &registration("client_secret_post", None),
            "code",
            &verifier,
            "https://omni.example/cb",
            "https://mcp.example/mcp",
            NOW,
        )
        .await
        .unwrap_err();
    assert!(matches!(missing_secret, OAuthError::Protocol(_)));
}

#[tokio::test]
async fn refresh_keeps_the_refresh_token_and_maps_invalid_grant() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("refresh_token=rt-good"))
        .respond_with(token_response(
            json!({"access_token": "at-2", "expires_in": "60"}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("refresh_token=rt-revoked"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant",
            "error_description": "token rt-revoked was revoked",
        })))
        .mount(&server)
        .await;
    let endpoint = format!("{}/token", server.uri());
    let c = client();

    let tokens = c
        .refresh(
            &endpoint,
            &registration("none", None),
            "rt-good",
            "https://mcp.example/mcp",
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(tokens.access_token, "at-2");
    assert_eq!(tokens.refresh_token.as_deref(), Some("rt-good"));
    assert_eq!(tokens.expires_at, Some(NOW + 60_000));
    let sent = form(&server.received_requests().await.unwrap()[0].body);
    assert_eq!(
        sent.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        sent.get("resource").map(String::as_str),
        Some("https://mcp.example/mcp")
    );

    let error = c
        .refresh(
            &endpoint,
            &registration("none", None),
            "rt-revoked",
            "https://mcp.example/mcp",
            NOW,
        )
        .await
        .unwrap_err();
    assert!(matches!(&error, OAuthError::InvalidGrant { error, .. } if error == "invalid_grant"));
    assert!(!error.is_transient());
    // The description echoed the token; neither Display nor Debug repeats it.
    assert!(!error.to_string().contains("rt-revoked"));
    assert!(!format!("{error:?}").contains("rt-revoked"));
}

#[tokio::test]
async fn token_endpoint_failures_are_classified_without_leaking_secrets() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/busy"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/ok-but-error"))
        .respond_with(token_response(
            json!({"error": "invalid_request", "error_description": "bad pw"}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/garbled"))
        .respond_with(ResponseTemplate::new(200).set_body_string("access_token=at-secret"))
        .mount(&server)
        .await;
    let c = client();
    let r = registration("client_secret_post", Some("pw"));

    let busy = c
        .exchange(
            &format!("{}/busy", server.uri()),
            &r,
            "code-x",
            &pkce::verifier(),
            "https://omni.example/cb",
            "https://mcp.example/mcp",
            NOW,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        busy,
        OAuthError::TokenEndpoint { status: 503, .. }
    ));
    assert!(busy.is_transient());

    let in_200 = c
        .exchange(
            &format!("{}/ok-but-error", server.uri()),
            &r,
            "code-x",
            &pkce::verifier(),
            "https://omni.example/cb",
            "https://mcp.example/mcp",
            NOW,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&in_200, OAuthError::TokenEndpoint { status: 200, error, .. } if error == "invalid_request")
    );
    assert!(in_200.to_string().contains("bad <redacted>"), "{in_200}");

    let garbled = c
        .exchange(
            &format!("{}/garbled", server.uri()),
            &r,
            "code-x",
            &pkce::verifier(),
            "https://omni.example/cb",
            "https://mcp.example/mcp",
            NOW,
        )
        .await
        .unwrap_err();
    assert!(matches!(garbled, OAuthError::Protocol(_)));
    assert!(!garbled.to_string().contains("at-secret"));

    let tokens = TokenSet {
        access_token: "at-visible?".into(),
        refresh_token: Some("rt-visible?".into()),
        expires_at: None,
        scope: None,
    };
    let debug = format!("{tokens:?}");
    assert!(!debug.contains("at-visible") && !debug.contains("rt-visible"));
}

#[tokio::test]
async fn the_server_echoing_a_sent_secret_is_redacted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_client",
            "error_description": "secret hunter2-secret and code code-7 are wrong",
        })))
        .mount(&server)
        .await;
    let error = client()
        .exchange(
            &format!("{}/token", server.uri()),
            &registration("client_secret_basic", Some("hunter2-secret")),
            "code-7",
            &pkce::verifier(),
            "https://omni.example/cb",
            "https://mcp.example/mcp",
            NOW,
        )
        .await
        .unwrap_err();
    let text = error.to_string();
    assert!(matches!(
        error,
        OAuthError::TokenEndpoint { status: 401, .. }
    ));
    assert!(
        !text.contains("hunter2-secret") && !text.contains("code-7"),
        "{text}"
    );
}

#[tokio::test]
async fn token_requests_never_follow_redirects() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/elsewhere", server.uri())),
        )
        .mount(&server)
        .await;
    let error = client()
        .refresh(
            &format!("{}/token", server.uri()),
            &registration("none", None),
            "rt",
            "https://mcp.example/mcp",
            NOW,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, OAuthError::Protocol(m) if m.contains("redirect")),
        "{error}"
    );
    let paths: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_owned())
        .collect();
    assert_eq!(paths, vec!["/token"]);
}

#[tokio::test]
async fn private_addresses_are_blocked() {
    let error = client()
        .discover(&Url::parse("https://10.0.0.5/mcp").unwrap(), None)
        .await
        .unwrap_err();
    assert!(matches!(error, OAuthError::Blocked(_)), "{error}");
}
