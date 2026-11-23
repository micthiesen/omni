//! Signed iOS control routes, including the 503/400 paths and a
//! percent-encoded path signature.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::stream;
use omni_core::clock::SharedClock;
use omni_ios_controls::IosControls;
use omni_ios_controls::persistence::list_registrations;
use omni_ios_controls::routes::{IOS_CONTROL_MAX_SIGNED_BODY_BYTES, canonical_request, sign};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const TOKEN: &str = "test-token-that-is-definitely-long-enough";
const NOW_MS: i64 = 1_790_000_000_000;

struct Harness {
    store: omni_testkit::TestStore,
    router: axum::Router,
    clock: SharedClock,
}

async fn harness(token: Option<&str>) -> Harness {
    let (store, clock) = common::store(NOW_MS).await;
    let service = Arc::new(common::service(&store.store, &clock, vec![], None));
    let controls = IosControls::from_service(service, token.map(str::to_owned), clock.clone());
    let router = controls.into_subsystem().router;
    Harness {
        store,
        router,
        clock,
    }
}

fn signed(clock: &SharedClock, path: &str, method: &str, body: &str) -> Request<Body> {
    let timestamp = clock.now_ms() / 1_000;
    let nonce = omni_core::ids::uuid_v4();
    let body_hash = hex::encode(Sha256::digest(body.as_bytes()));
    #[allow(clippy::cast_precision_loss)]
    let canonical = canonical_request(timestamp as f64, &nonce, method, path, &body_hash);
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(
            "authorization",
            format!("Omni-HMAC {}", sign(TOKEN, &canonical)),
        )
        .header("x-omni-timestamp", timestamp.to_string())
        .header("x-omni-nonce", nonce);
    if !body.is_empty() {
        builder = builder.header("content-type", "application/json");
    }
    builder.body(Body::from(body.to_owned())).unwrap()
}

async fn send(
    router: &axum::Router,
    request: Request<Body>,
) -> (StatusCode, Value, Option<String>) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let cache = response
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        cache,
    )
}

fn registration_body() -> String {
    json!({
        "deviceId": "device-12345",
        "controls": [{"controlId": "control-one", "slot": 1, "pushToken": "ab".repeat(32), "environment": "sandbox"}]
    })
    .to_string()
}

#[tokio::test]
async fn requires_signed_authentication() {
    let h = harness(Some(TOKEN)).await;
    let request = Request::builder()
        .uri("/api/ios-controls/slots/1")
        .body(Body::empty())
        .unwrap();
    let (status, body, _) = send(&h.router, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({"error": "Unauthorized"}));
}

#[tokio::test]
async fn returns_a_slot_using_the_authenticated_wire_contract() {
    let h = harness(Some(TOKEN)).await;
    let (status, body, cache) = send(
        &h.router,
        signed(&h.clock, "/api/ios-controls/slots/1", "GET", ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache.as_deref(), Some("no-store"));
    assert_eq!(body["slot"], 1);
    assert_eq!(body["isLive"], false);
    assert_eq!(body["displayName"], "Nobody Live");
    assert_eq!(body["url"], "http://omni.boris");
}

#[tokio::test]
async fn reports_non_secret_server_diagnostics() {
    let h = harness(Some(TOKEN)).await;
    let (status, body, cache) = send(
        &h.router,
        signed(&h.clock, "/api/ios-controls/diagnostics", "GET", ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache.as_deref(), Some("no-store"));
    assert_eq!(
        body,
        json!({
            "apnsEnabled": false, "registrationCount": 0, "undeliveredCount": 0,
            "lastReconciledAt": null, "deviceCount": 0, "alertDeviceCount": 0,
            "widgetPushCount": 0
        })
    );
}

#[tokio::test]
async fn validates_and_persists_a_complete_device_registration_set() {
    let h = harness(Some(TOKEN)).await;
    let request = signed(
        &h.clock,
        "/api/ios-controls/registrations",
        "PUT",
        &registration_body(),
    );
    let (status, body, _) = send(&h.router, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"registered": 1}));
    assert_eq!(list_registrations(&h.store.store).await.unwrap().len(), 1);
}

#[tokio::test]
async fn reports_persistence_failures_as_server_errors() {
    let h = harness(Some(TOKEN)).await;
    h.store
        .store
        .write(|tx| {
            tx.connection()
                .execute_batch("DROP TABLE blobs")
                .map_err(|e| omni_store::StoreError::Sqlite(e.to_string()))
        })
        .await
        .unwrap();
    let request = signed(
        &h.clock,
        "/api/ios-controls/registrations",
        "PUT",
        &registration_body(),
    );
    let (status, body, _) = send(&h.router, request).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        json!({"error": "Could not save control registration"})
    );
}

#[tokio::test]
async fn rejects_replayed_signed_requests() {
    let h = harness(Some(TOKEN)).await;
    let first = signed(&h.clock, "/api/ios-controls/slots/1", "GET", "");
    let (parts, _) = first.into_parts();
    let rebuild = || {
        let mut builder = Request::builder()
            .method(parts.method.clone())
            .uri(parts.uri.clone());
        for (name, value) in &parts.headers {
            builder = builder.header(name, value);
        }
        builder.body(Body::empty()).unwrap()
    };
    assert_eq!(send(&h.router, rebuild()).await.0, StatusCode::OK);
    assert_eq!(send(&h.router, rebuild()).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rejects_an_oversized_declared_body_before_reading_it() {
    let h = harness(Some(TOKEN)).await;
    let mut request = signed(&h.clock, "/api/ios-controls/registrations", "PUT", "{}");
    request.headers_mut().insert(
        "content-length",
        (IOS_CONTROL_MAX_SIGNED_BODY_BYTES + 1)
            .to_string()
            .parse()
            .unwrap(),
    );
    assert_eq!(
        send(&h.router, request).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn rejects_a_chunked_body_that_crosses_the_signed_body_limit() {
    let h = harness(Some(TOKEN)).await;
    let first = "a".repeat(IOS_CONTROL_MAX_SIGNED_BODY_BYTES);
    let body = format!("{first}b");
    let (parts, _) = signed(&h.clock, "/api/ios-controls/registrations", "PUT", &body).into_parts();
    let chunks = vec![
        Ok::<_, std::io::Error>(first.into_bytes()),
        Ok(b"b".to_vec()),
    ];
    let request = Request::from_parts(parts, Body::from_stream(stream::iter(chunks)));
    assert_eq!(
        send(&h.router, request).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn answers_503_without_an_auth_token() {
    let h = harness(None).await;
    let (status, body, _) = send(
        &h.router,
        signed(&h.clock, "/api/ios-controls/slots/1", "GET", ""),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, json!({"error": "iOS controls are not configured"}));
}

#[tokio::test]
async fn rejects_invalid_registrations_and_slots() {
    let h = harness(Some(TOKEN)).await;
    for body in [
        "not json".to_owned(),
        json!({"deviceId": " padded-device ", "controls": []}).to_string(),
        json!({"deviceId": "device-12345", "controls": [{"controlId": "c", "slot": 5, "pushToken": "ab".repeat(32), "environment": "sandbox"}]}).to_string(),
        json!({"deviceId": "device-12345", "controls": [{"controlId": "c", "slot": 1, "pushToken": "zz", "environment": "sandbox"}]}).to_string(),
    ] {
        let (status, response, _) = send(&h.router, signed(&h.clock, "/api/ios-controls/registrations", "PUT", &body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(response, json!({"error": "Invalid control registration"}));
    }
    let (status, response, _) = send(
        &h.router,
        signed(&h.clock, "/api/ios-controls/slots/9", "GET", ""),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        response,
        json!({"error": "Slot must be an integer from 1 to 4"})
    );
}

#[tokio::test]
async fn rejects_stale_timestamps_and_wrong_signatures() {
    let h = harness(Some(TOKEN)).await;
    let mut stale = signed(&h.clock, "/api/ios-controls/slots/1", "GET", "");
    stale.headers_mut().insert(
        "x-omni-timestamp",
        ((NOW_MS / 1000) - 301).to_string().parse().unwrap(),
    );
    assert_eq!(send(&h.router, stale).await.0, StatusCode::UNAUTHORIZED);
    // Signed for another path.
    let mut moved = signed(&h.clock, "/api/ios-controls/slots/2", "GET", "");
    *moved.uri_mut() = "/api/ios-controls/slots/1".parse().unwrap();
    assert_eq!(send(&h.router, moved).await.0, StatusCode::UNAUTHORIZED);
    // The raw percent-encoded path is what gets signed.
    let encoded = signed(&h.clock, "/api/ios-controls/slots/%31", "GET", "");
    assert_eq!(send(&h.router, encoded).await.0, StatusCode::OK);
}

#[tokio::test]
async fn authenticates_every_path_under_the_prefix_before_routing() {
    let h = harness(Some(TOKEN)).await;
    let unsigned = Request::builder()
        .uri("/api/ios-controls/unknown")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&h.router, unsigned).await.0, StatusCode::UNAUTHORIZED);
    let (status, body, _) = send(
        &h.router,
        signed(&h.clock, "/api/ios-controls/unknown", "GET", ""),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "Not Found"}));
    // The signed path is the full path even though the routes are nested.
    assert_eq!(
        send(
            &h.router,
            signed(&h.clock, "/api/ios-controls/slots/1", "GET", "")
        )
        .await
        .0,
        StatusCode::OK
    );
}
