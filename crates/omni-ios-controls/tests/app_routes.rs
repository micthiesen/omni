//! The app's device, widget push and widget timeline routes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use omni_core::clock::SharedClock;
use omni_ios_controls::IosControls;
use omni_ios_controls::devices::{list_devices, list_widget_pushes};
use omni_ios_controls::routes::{canonical_request, sign};
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

fn device_body(overrides: Value) -> String {
    let mut body = json!({
        "deviceId": "0f8fad5b-d9cb-469f-a165-70867728950e",
        "name": "Michael's iPhone",
        "kind": "iphone",
        "apnsToken": "ab".repeat(32),
        "environment": "sandbox",
        "alertsEnabled": true,
        "appVersion": "1.0 (1)"
    });
    for (key, value) in overrides.as_object().unwrap() {
        body[key] = value.clone();
    }
    body.to_string()
}

#[tokio::test]
async fn registers_and_replaces_an_alert_device() {
    let h = harness(Some(TOKEN)).await;
    let (status, body, cache) = send(
        &h.router,
        signed(
            &h.clock,
            "/api/ios-controls/device",
            "PUT",
            &device_body(json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache.as_deref(), Some("no-store"));
    assert_eq!(
        body,
        json!({"registered": true, "alertsEnabled": true, "apnsEnabled": false})
    );
    // The iPad toggle off and a null token keep the device, un-pushable.
    let update = device_body(
        json!({"apnsToken": null, "alertsEnabled": false, "kind": "ipad", "name": "  iPad  "}),
    );
    let (status, body, _) = send(
        &h.router,
        signed(&h.clock, "/api/ios-controls/device", "PUT", &update),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["alertsEnabled"], false);
    let devices = list_devices(&h.store.store).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "iPad");
    assert_eq!(devices[0].apns_token, None);
    assert!(!devices[0].receives_alerts());

    let (_, diagnostics, _) = send(
        &h.router,
        signed(&h.clock, "/api/ios-controls/diagnostics", "GET", ""),
    )
    .await;
    assert_eq!(diagnostics["deviceCount"], 1);
    assert_eq!(diagnostics["alertDeviceCount"], 0);
}

#[tokio::test]
async fn rejects_invalid_device_registrations() {
    let h = harness(Some(TOKEN)).await;
    for overrides in [
        json!({"deviceId": "short"}),
        json!({"kind": "mac"}),
        json!({"apnsToken": "zz"}),
        json!({"apnsToken": "ab".repeat(300)}),
        json!({"environment": "staging"}),
        json!({"alertsEnabled": "yes"}),
        json!({"name": "   "}),
        json!({"appVersion": "v".repeat(65)}),
    ] {
        let body = device_body(overrides);
        let (status, response, _) = send(
            &h.router,
            signed(&h.clock, "/api/ios-controls/device", "PUT", &body),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(response, json!({"error": "Invalid device registration"}));
    }
    assert!(list_devices(&h.store.store).await.unwrap().is_empty());
    let unsigned = Request::builder()
        .method("PUT")
        .uri("/api/ios-controls/device")
        .body(Body::from(device_body(json!({}))))
        .unwrap();
    assert_eq!(send(&h.router, unsigned).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bounds_the_number_of_devices() {
    let h = harness(Some(TOKEN)).await;
    for i in 0..omni_ios_controls::devices::MAX_DEVICES {
        let body = device_body(json!({"deviceId": format!("device-{i:04}")}));
        let request = signed(&h.clock, "/api/ios-controls/device", "PUT", &body);
        assert_eq!(send(&h.router, request).await.0, StatusCode::OK);
    }
    let body = device_body(json!({"deviceId": "device-overflow"}));
    let (status, _, _) = send(
        &h.router,
        signed(&h.clock, "/api/ios-controls/device", "PUT", &body),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // An existing device can still update.
    let body = device_body(json!({"deviceId": "device-0000", "alertsEnabled": false}));
    let request = signed(&h.clock, "/api/ios-controls/device", "PUT", &body);
    assert_eq!(send(&h.router, request).await.0, StatusCode::OK);
}

#[tokio::test]
async fn registers_one_widget_push_token_per_device() {
    let h = harness(Some(TOKEN)).await;
    for token in ["ab", "cd"] {
        let body = json!({
            "deviceId": "0f8fad5b-d9cb-469f-a165-70867728950e",
            "pushToken": token.repeat(32),
            "environment": "production"
        })
        .to_string();
        let (status, response, _) = send(
            &h.router,
            signed(&h.clock, "/api/ios-controls/widget-push", "PUT", &body),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response, json!({"registered": true}));
    }
    let rows = list_widget_pushes(&h.store.store).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].push_token, "cd".repeat(32));
    for body in [
        json!({"deviceId": "0f8fad5b-d9cb-469f-a165-70867728950e", "pushToken": "nothex", "environment": "sandbox"}),
        json!({"deviceId": "0f8fad5b-d9cb-469f-a165-70867728950e", "pushToken": "ab".repeat(32)}),
    ] {
        let (status, response, _) = send(
            &h.router,
            signed(
                &h.clock,
                "/api/ios-controls/widget-push",
                "PUT",
                &body.to_string(),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            response,
            json!({"error": "Invalid widget push registration"})
        );
    }
}

#[tokio::test]
async fn serves_the_widget_timelines() {
    let h = harness(Some(TOKEN)).await;
    let (status, on_air, cache) = send(
        &h.router,
        signed(&h.clock, "/api/ios-controls/widgets/on-air", "GET", ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache.as_deref(), Some("no-store"));
    let mut on_air = on_air;
    assert!(on_air["generatedAt"].as_i64().unwrap() >= NOW_MS);
    on_air["generatedAt"] = json!(0);
    assert_eq!(
        on_air,
        json!({"generatedAt": 0, "liveCount": 0, "totalViewers": 0, "live": [], "recent": []})
    );
}
