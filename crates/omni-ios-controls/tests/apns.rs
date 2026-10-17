//! APNs token signing and pushes, plus the HTTP behavior of the client against
//! a mock APNs origin. reqwest owns the connection pool and a dropped request
//! future aborts the stream, so stalls are covered by "times out a stalled
//! request as a transport error".
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use base64::Engine;
use omni_api::ios::ApnsEnvironment;
use omni_http::SideEffectMode;
use omni_ios_controls::apns::{
    ApnsConfig, ApnsControlClient, ApnsPush, ApnsPushKind, ApnsPushResult, ApnsSender,
    build_apns_control_request, build_apns_request, create_apns_provider_token,
};
use omni_ios_controls::persistence::IosControlRegistration;
use omni_store::cbor::Extra;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn config() -> ApnsConfig {
    ApnsConfig {
        team_id: "TEAM123".into(),
        key_id: "KEY123".into(),
        bundle_id: "com.example.OmniLive".into(),
        private_key_path: "/tmp/omni-apns-key-does-not-exist.p8".into(),
    }
}

fn registration() -> IosControlRegistration {
    IosControlRegistration {
        registration_id: "registration".into(),
        device_id: "device".into(),
        control_id: "control".into(),
        slot: 1,
        push_token: "token".into(),
        environment: ApnsEnvironment::Sandbox,
        last_delivered_hash: None,
        created_at: 1,
        updated_at: 1,
        extra: Extra::new(),
    }
}

fn b64(segment: &str) -> Value {
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(segment)
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn builds_an_apple_controls_request_with_the_required_topic_and_payload() {
    let request = build_apns_control_request("com.example.OmniLive", "abc", "jwt");
    assert_eq!(request.path, "/3/device/abc");
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(header("authorization"), Some("bearer jwt"));
    assert_eq!(header("apns-push-type"), Some("controls"));
    assert_eq!(
        header("apns-topic"),
        Some("com.example.OmniLive.push-type.controls")
    );
    assert_eq!(header("apns-priority"), Some("10"));
    assert_eq!(header("apns-expiration"), Some("0"));
    assert_eq!(
        serde_json::from_str::<Value>(&request.body).unwrap(),
        json!({"aps": {"content-changed": true}})
    );
}

#[test]
fn creates_a_valid_es256_provider_token() {
    let token =
        create_apns_provider_token("TEAM123", "KEY123", common::TEST_PRIVATE_KEY, 123_456).unwrap();
    let parts: Vec<&str> = token.split('.').collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(b64(parts[0]), json!({"alg": "ES256", "kid": "KEY123"}));
    assert_eq!(b64(parts[1]), json!({"iss": "TEAM123", "iat": 123_456}));
    let key = jsonwebtoken::DecodingKey::from_ec_pem(common::TEST_PUBLIC_KEY.as_bytes()).unwrap();
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    let decoded = jsonwebtoken::decode::<Value>(&token, &key, &validation).unwrap();
    assert_eq!(decoded.claims["iss"], "TEAM123");
}

#[tokio::test]
async fn reports_an_unreadable_signing_key_as_a_typed_construction_failure() {
    let clock = omni_testkit::test_clock(0);
    let error = ApnsControlClient::create(
        config(),
        omni_testkit::no_network(),
        clock,
        SideEffectMode::Live,
    )
    .await
    .err()
    .expect("missing key");
    assert_eq!(error.operation, "signing key read");
    assert!(error.to_string().contains("os error 2"), "{error}");
}

async fn client(server: &wiremock::MockServer, mode: SideEffectMode) -> ApnsControlClient {
    let http = omni_testkit::mock_http(
        server,
        &[
            "https://api.sandbox.push.apple.com",
            "https://api.push.apple.com",
        ],
    );
    ApnsControlClient::with_key(
        config(),
        common::TEST_PRIVATE_KEY.into(),
        http,
        omni_testkit::test_clock(1_000_000),
        mode,
    )
}

#[tokio::test]
async fn posts_a_signed_controls_push_and_maps_apple_responses() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .and(path("/3/device/token"))
        .respond_with(ResponseTemplate::new(200))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/3/device/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"reason": "BadDeviceToken"})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/3/device/token"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(json!({"reason": "InvalidProviderToken"})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/3/device/token"))
        .respond_with(ResponseTemplate::new(410))
        .mount(&server)
        .await;
    let apns = client(&server, SideEffectMode::Live).await;
    assert_eq!(
        apns.send_control_changed(&registration()).await.unwrap(),
        ApnsPushResult::Sent
    );
    assert_eq!(
        apns.send_control_changed(&registration()).await.unwrap(),
        ApnsPushResult::InvalidToken {
            reason: "BadDeviceToken".into()
        }
    );
    assert_eq!(
        apns.send_control_changed(&registration()).await.unwrap(),
        ApnsPushResult::Failed {
            status: 403,
            reason: "InvalidProviderToken".into()
        }
    );
    assert_eq!(
        apns.send_control_changed(&registration()).await.unwrap(),
        ApnsPushResult::InvalidToken {
            reason: "HTTP 410".into()
        }
    );
    let requests = server.received_requests().await.unwrap();
    let first = &requests[0];
    let header = |name: &str| {
        first
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(header("apns-push-type").as_deref(), Some("controls"));
    assert_eq!(
        header("apns-topic").as_deref(),
        Some("com.example.OmniLive.push-type.controls")
    );
    assert!(header("authorization").unwrap().starts_with("bearer "));
    // The provider token is cached between pushes.
    assert_eq!(
        header("authorization"),
        requests[3]
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    );
}

#[tokio::test]
async fn records_instead_of_pushing_in_record_mode() {
    let server = omni_testkit::mock_server().await;
    let apns = client(&server, SideEffectMode::Record).await;
    assert_eq!(
        apns.send_control_changed(&registration()).await.unwrap(),
        ApnsPushResult::Sent
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn times_out_a_stalled_request_as_a_transport_error() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let apns = client(&server, SideEffectMode::Live).await;
    let started = std::time::Instant::now();
    let error = apns
        .send_control_changed(&registration())
        .await
        .unwrap_err();
    assert_eq!(error.operation, "request");
    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn keeps_the_controls_request_byte_identical() {
    let request = build_apns_control_request("com.example.OmniLive", "abc", "jwt");
    assert_eq!(
        request.headers,
        vec![
            ("authorization", "bearer jwt".to_owned()),
            ("apns-push-type", "controls".to_owned()),
            (
                "apns-topic",
                "com.example.OmniLive.push-type.controls".to_owned()
            ),
            ("apns-priority", "10".to_owned()),
            ("apns-expiration", "0".to_owned()),
            ("content-type", "application/json".to_owned()),
        ]
    );
    assert_eq!(request.body, r#"{"aps":{"content-changed":true}}"#);
    assert_eq!(
        build_apns_request(
            "com.example.OmniLive",
            &ApnsPush::controls(&registration()),
            "jwt"
        ),
        build_apns_control_request("com.example.OmniLive", "token", "jwt")
    );
}

#[test]
fn builds_alert_and_widget_requests() {
    let alert = build_apns_request(
        "com.example.OmniLive",
        &ApnsPush::alert(
            "abc",
            ApnsEnvironment::Sandbox,
            r#"{"aps":{}}"#.into(),
            1_790_003_600,
        ),
        "jwt",
    );
    assert_eq!(alert.path, "/3/device/abc");
    assert_eq!(
        alert.headers,
        vec![
            ("authorization", "bearer jwt".to_owned()),
            ("apns-push-type", "alert".to_owned()),
            ("apns-topic", "com.example.OmniLive".to_owned()),
            ("apns-priority", "10".to_owned()),
            ("apns-expiration", "1790003600".to_owned()),
            ("content-type", "application/json".to_owned()),
        ]
    );
    assert_eq!(alert.body, r#"{"aps":{}}"#);
    let widgets = build_apns_request(
        "com.example.OmniLive",
        &ApnsPush::widgets("def", ApnsEnvironment::Production, 1_790_003_600),
        "jwt",
    );
    assert_eq!(widgets.headers[1], ("apns-push-type", "widgets".to_owned()));
    assert_eq!(
        widgets.headers[2],
        (
            "apns-topic",
            "com.example.OmniLive.push-type.widgets".to_owned()
        )
    );
    assert_eq!(
        widgets.headers[4],
        ("apns-expiration", "1790003600".to_owned())
    );
    assert_eq!(widgets.body, r#"{"aps":{"content-changed":true}}"#);
}

#[tokio::test]
async fn posts_an_alert_push_to_the_device_environment() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .and(path("/3/device/abc"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let apns = client(&server, SideEffectMode::Live).await;
    let push = ApnsPush::alert(
        "abc",
        ApnsEnvironment::Production,
        r#"{"aps":{}}"#.into(),
        1,
    );
    assert_eq!(apns.send_push(&push).await.unwrap(), ApnsPushResult::Sent);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].headers.get("apns-push-type").unwrap(), "alert");
    assert_eq!(requests[0].body, br#"{"aps":{}}"#);
    assert!(apns.recorded().is_empty());
}

#[tokio::test]
async fn records_alert_pushes_in_record_mode() {
    let server = omni_testkit::mock_server().await;
    let apns = client(&server, SideEffectMode::Record).await;
    let push = ApnsPush::alert("abc", ApnsEnvironment::Sandbox, "{}".into(), 1);
    assert_eq!(apns.send_push(&push).await.unwrap(), ApnsPushResult::Sent);
    assert_eq!(apns.recorded().len(), 1);
    assert_eq!(apns.recorded()[0].kind, ApnsPushKind::Alert);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_bad_signing_key_fails_before_sending_anything() {
    let server = omni_testkit::mock_server().await;
    let http = omni_testkit::mock_http(&server, &["https://api.sandbox.push.apple.com"]);
    let apns = ApnsControlClient::with_key(
        config(),
        "not a key".into(),
        http,
        omni_testkit::test_clock(1_000_000),
        SideEffectMode::Live,
    );
    let push = ApnsPush::alert("abc", ApnsEnvironment::Sandbox, "{}".into(), 1);
    let error = apns.send_push(&push).await.unwrap_err();
    assert!(error.not_sent);
    assert_eq!(error.operation, "provider token creation");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_stalled_request_is_not_known_unsent() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let apns = client(&server, SideEffectMode::Live).await;
    let push = ApnsPush::alert("abc", ApnsEnvironment::Sandbox, "{}".into(), 1);
    assert!(!apns.send_push(&push).await.unwrap_err().not_sent);
}

#[tokio::test]
async fn keeps_only_the_most_recent_recorded_pushes() {
    let server = omni_testkit::mock_server().await;
    let apns = client(&server, SideEffectMode::Record).await;
    for expiration in 0..510 {
        let push = ApnsPush::alert("abc", ApnsEnvironment::Sandbox, "{}".into(), expiration);
        apns.send_push(&push).await.unwrap();
    }
    let recorded = apns.recorded();
    assert_eq!(recorded.len(), 500);
    assert_eq!(recorded[0].expiration, 10);
}
