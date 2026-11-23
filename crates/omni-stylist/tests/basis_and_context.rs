//! What a forecast may claim: the evidence basis is earned by recorded fit on
//! a compatible piece, and the context it cites is proven by a server token.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Harness, id, key, stated};
use serde_json::{Value, json};

const NS: &str = "fixture-basis";

fn variant(model: &str, version: &str, fiber: &str) -> Value {
    json!({
        "kind": "variant", "brand": "Synthetic Edge", "model": model, "category": "tops/tshirt",
        "sizeLabel": "M", "sizeSystem": "alpha", "cut": "regular", "version": version,
        "fabric": {"composition": [{"fiber": fiber, "percent": 100}], "construction": "knit"},
        "provenance": {"certainty": "explicit", "source": "label"},
        "allowDuplicate": true,
    })
}

async fn piece(h: &Harness, label: &str, variant: &str, status: &str) -> String {
    id(&h
        .save(
            NS,
            &key(&format!("piece-{label}")),
            json!({"kind": "piece", "label": label, "category": "tops/tshirt",
                   "variantId": variant, "initialStatus": status, "provenance": stated()}),
        )
        .await)
}

async fn snapshot(h: &Harness) -> Value {
    h.context(NS, json!({})).await["snapshot"].clone()
}

fn forecast(variant: &str, basis: &str, decision: &str, snapshot: &Value) -> Value {
    json!({
        "kind": "prediction", "variantId": variant, "recommendedSize": "M",
        "decision": decision, "basis": basis,
        "regionExpectations": [{"region": "chest", "expected": "good", "rationale": "same model"}],
        "likelyFailureMode": {"description": "fabric behaves differently"},
        "returnRisk": {"level": "medium", "rationale": "returns accepted"},
        "contextRevision": snapshot["ledgerRevision"],
        "contextToken": snapshot["contextToken"],
    })
}

async fn try_forecast(h: &Harness, label: &str, record: Value) -> Result<Value, String> {
    h.try_call(
        "stylist_record_save",
        json!({"namespace": NS, "idempotencyKey": key(label), "record": record}),
    )
    .await
    .map(|called| called.structured)
}

#[tokio::test(start_paused = true)]
async fn owning_an_untested_piece_of_another_version_and_fabric_is_not_an_exact_repeat() {
    let h = Harness::new().await;
    let reference = id(&h
        .save(NS, &key("ref"), variant("Same Tee", "2025", "cotton"))
        .await);
    piece(&h, "untested", &reference, "undecided").await;
    let candidate = id(&h
        .save(NS, &key("cand"), variant("Same Tee", "2026", "polyester"))
        .await);
    let at = snapshot(&h).await;
    let refused = try_forecast(
        &h,
        "exact",
        forecast(&candidate, "exact_repeat", "recommend", &at),
    )
    .await
    .unwrap_err();
    assert!(refused.starts_with("[basis_mismatch]"), "{refused}");
    assert!(
        refused.contains("supports new_brand_inference"),
        "{refused}"
    );
    assert!(
        refused.contains("version 2025 differs from 2026"),
        "{refused}"
    );
    assert!(
        refused.contains("the material differs (cotton 100% vs 0%; polyester 0% vs 100%)"),
        "{refused}"
    );
    assert!(refused.contains("no recorded fit"), "{refused}");
    let saved = try_forecast(
        &h,
        "honest",
        forecast(
            &candidate,
            "new_brand_inference",
            "recommend_with_caution",
            &at,
        ),
    )
    .await
    .unwrap();
    let assessment = &saved["record"]["derived"]["basisAssessment"];
    assert_eq!(
        saved["record"]["derived"]["computedBasis"],
        json!("new_brand_inference")
    );
    assert_eq!(
        assessment["identityMatches"][0]["version"],
        json!("differs")
    );
    assert_eq!(
        assessment["identityMatches"][0]["material"],
        json!("differs")
    );
    assert_eq!(assessment["identityMatches"][0]["fit"], json!("untested"));
}

#[tokio::test(start_paused = true)]
async fn proven_fit_on_the_same_variant_is_an_exact_repeat_and_changes_downgrade_it() {
    let h = Harness::new().await;
    let same = id(&h
        .save(NS, &key("same"), variant("Proven Tee", "2025", "cotton"))
        .await);
    let worn = piece(&h, "worn", &same, "owned").await;
    h.event(
        NS,
        &key("wear"),
        "2026-10-10",
        json!({"type": "wear", "pieceId": worn,
               "regionFit": [{"region": "chest", "verdict": "good"}, {"region": "body_length", "verdict": "slightly_long"}]}),
    )
    .await;
    let newer = id(&h
        .save(NS, &key("newer"), variant("Proven Tee", "2026", "cotton"))
        .await);
    let at = snapshot(&h).await;
    let exact = try_forecast(
        &h,
        "rebuy",
        forecast(&same, "exact_repeat", "recommend", &at),
    )
    .await
    .unwrap();
    assert_eq!(
        exact["record"]["derived"]["computedBasis"],
        json!("exact_repeat")
    );
    assert_eq!(
        exact["record"]["derived"]["basisAssessment"]["identityMatches"][0]["fit"],
        json!("proven")
    );
    let changed = try_forecast(
        &h,
        "forecast-newer",
        forecast(&newer, "exact_repeat", "recommend", &at),
    )
    .await
    .unwrap_err();
    assert!(
        changed.contains("supports known_brand_new_cut"),
        "{changed}"
    );
    assert!(
        changed.contains("version 2025 differs from 2026"),
        "{changed}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_known_failed_exact_variant_cannot_be_plainly_recommended() {
    let h = Harness::new().await;
    let failed = id(&h
        .save(NS, &key("failed"), variant("Tight Tee", "2025", "cotton"))
        .await);
    let tried = piece(&h, "tried", &failed, "undecided").await;
    h.event(
        NS,
        &key("try"),
        "2026-10-10",
        json!({"type": "try_on", "pieceId": tried, "overall": "wrong_size",
               "regionFit": [{"region": "chest", "verdict": "too_tight"}]}),
    )
    .await;
    let at = snapshot(&h).await;
    let plain = try_forecast(
        &h,
        "plain",
        forecast(&failed, "known_brand_new_cut", "recommend", &at),
    )
    .await
    .unwrap_err();
    assert!(plain.starts_with("[known_failure]"), "{plain}");
    let exact = try_forecast(
        &h,
        "exact",
        forecast(&failed, "exact_repeat", "abstain", &at),
    )
    .await
    .unwrap_err();
    assert!(exact.starts_with("[basis_mismatch]"), "{exact}");
    let cautious = try_forecast(
        &h,
        "cautious",
        forecast(
            &failed,
            "known_brand_new_cut",
            "recommend_with_caution",
            &at,
        ),
    )
    .await
    .unwrap();
    assert!(
        cautious["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("failed before")),
        "{cautious:#}"
    );
    assert_eq!(
        cautious["record"]["derived"]["basisAssessment"]["knownFailures"],
        json!([tried])
    );
}

#[tokio::test(start_paused = true)]
async fn a_forecast_must_cite_a_context_the_server_can_reproduce() {
    let h = Harness::new().await;
    let candidate = id(&h
        .save(NS, &key("cand"), variant("Token Tee", "2025", "cotton"))
        .await);
    let at = snapshot(&h).await;
    let token = at["contextToken"].as_str().unwrap().to_owned();

    let mut missing = forecast(&candidate, "new_brand_inference", "recommend", &at);
    missing.as_object_mut().unwrap().remove("contextToken");
    let error = try_forecast(&h, "missing", missing).await.unwrap_err();
    assert!(error.contains("contextToken is required"), "{error}");

    let (head, _) = token.rsplit_once('.').unwrap();
    let mut zeros = forecast(&candidate, "new_brand_inference", "recommend", &at);
    zeros["contextToken"] = json!(format!("{head}.{}", "0".repeat(64)));
    let error = try_forecast(&h, "zeros", zeros).await.unwrap_err();
    assert!(error.starts_with("[context_mismatch]"), "{error}");

    let other = h.context("fixture-other", json!({})).await["snapshot"].clone();
    let mut foreign = forecast(&candidate, "new_brand_inference", "recommend", &at);
    foreign["contextToken"] = other["contextToken"].clone();
    let error = try_forecast(&h, "foreign", foreign).await.unwrap_err();
    assert!(error.starts_with("[context_mismatch]"), "{error}");

    let mut wrong_revision = forecast(&candidate, "new_brand_inference", "recommend", &at);
    wrong_revision["contextRevision"] = json!(0);
    let error = try_forecast(&h, "wrong-rev", wrong_revision)
        .await
        .unwrap_err();
    assert!(error.starts_with("[context_mismatch]"), "{error}");

    // A stale but genuine token is accepted with a warning, and the freeze
    // records exactly which context it was made from.
    h.save(NS, &key("later"), variant("Later Tee", "2025", "cotton"))
        .await;
    let saved = try_forecast(
        &h,
        "stale",
        forecast(&candidate, "new_brand_inference", "recommend", &at),
    )
    .await
    .unwrap();
    let context = &saved["record"]["derived"]["context"];
    assert_eq!(context["verified"], json!(true));
    assert_eq!(context["revision"], at["ledgerRevision"]);
    assert_eq!(context["sha256"], at["contextSha256"]);
    assert_eq!(context["runbookVersion"], json!("fit-first-v3"));
    assert!(
        saved["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("ledger writes happened after")),
        "{saved:#}"
    );

    // The token re-renders the same context later, on another day.
    h.clock.set(common::NOW_MS + 3 * 86_400_000);
    let again = h
        .call(
            "stylist_context_get",
            json!({"namespace": NS, "contextToken": token, "includeRunbook": false}),
        )
        .await;
    assert_eq!(again["snapshot"]["contextSha256"], at["contextSha256"]);
    assert_eq!(again["snapshot"]["today"], at["today"]);
    assert_eq!(again["snapshot"]["ledgerRevision"], at["ledgerRevision"]);
    let mixed = h
        .reject(
            "stylist_context_get",
            json!({"namespace": NS, "contextToken": token, "asOfRevision": 1}),
        )
        .await;
    assert!(mixed.starts_with("[validation]"), "{mixed}");
}
