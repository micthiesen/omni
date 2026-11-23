//! `GET /api/stylist`: the read-only overview behind the web page.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{Harness, body_measurement, garment_measurement, id, jpeg, key, stated, tee};
use omni_api::stylist::StylistOverview;
use serde_json::json;
use tower::ServiceExt as _;

async fn get(h: &Harness, path: &str) -> (StatusCode, String) {
    let response = omni_stylist::ui::router(h.stylist.clone())
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test(start_paused = true)]
async fn an_empty_ledger_explains_what_is_missing() {
    let h = Harness::new().await;
    let (status, body) = get(&h, "/api/stylist").await;
    assert_eq!(status, StatusCode::OK);
    let overview: StylistOverview = serde_json::from_str(&body).unwrap();
    assert_eq!(overview.namespace, "real");
    assert_eq!(overview.ledger_revision, 0.0);
    assert_eq!(overview.namespaces[0].namespace, "real");
    assert!(overview.warnings.iter().any(|w| w.code == "no_profile"));
    assert!(overview.wardrobe.is_empty());
    let (status, _) = get(&h, "/api/stylist?namespace=prod").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(start_paused = true)]
async fn the_overview_shows_the_ledger_without_private_bytes() {
    let h = Harness::new().await;
    const NS: &str = "fixture-overview";
    h.save(
        NS,
        &key("chest"),
        body_measurement("chest", 101.5, "cm", "2026-10-01"),
    )
    .await;
    let variant = id(&h
        .save(
            NS,
            &key("variant"),
            tee("Asket", "The T-Shirt", "M", "regular"),
        )
        .await);
    let piece = id(&h
        .save(
            NS,
            &key("piece"),
            json!({"kind": "piece", "label": "white tee", "category": "tops/tshirt",
            "variantId": variant, "reference": {"categories": ["tops/tshirt"], "role": "baseline"},
            "provenance": stated()}),
        )
        .await);
    h.save(
        NS,
        &key("flat"),
        garment_measurement(
            "piece",
            &piece,
            "chest",
            "flat_width",
            21.25,
            "in",
            "washed",
            "2026-10-01",
        ),
    )
    .await;
    h.call(
        "stylist_evidence_store",
        json!({"namespace": NS, "idempotencyKey": key("photo"), "mimeType": "image/jpeg",
               "dataBase64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpeg(3)),
               "info": {"kind": "body_photo", "caption": "front", "certainty": "explicit"}}),
    )
    .await;
    let (status, body) = get(&h, &format!("/api/stylist?namespace={NS}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("dataBase64") && !body.contains("blob") && !body.contains("front"));
    let overview: StylistOverview = serde_json::from_str(&body).unwrap();
    assert_eq!(overview.counts.evidence, 1.0);
    assert_eq!(overview.profile.body[0].latest.value_mm, 1015.0);
    let tee = &overview.wardrobe[0];
    assert_eq!(tee.reference_role.as_deref(), Some("baseline"));
    assert_eq!(tee.fit, "proven");
    assert_eq!(tee.fabric.as_deref(), Some("100% cotton"));
    assert_eq!(tee.measurements[0].original, "21.25 in");
    assert_eq!(tee.measurements[0].value_mm, 539.8);
    assert!(
        overview
            .namespaces
            .iter()
            .any(|n| n.namespace == NS && n.fixture)
    );
}
