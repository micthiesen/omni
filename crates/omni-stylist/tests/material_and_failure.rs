//! The material and failure rules behind a forecast's basis: stretch content
//! is part of a variant's identity, label rounding is not, and a piece whose
//! latest fit report is adverse is a known failure even after earlier
//! successes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Harness, id, key, stated};
use serde_json::{Value, json};

const NS: &str = "fixture-material";

fn fabric(composition: &[(&str, f64)], stretch: &str) -> Value {
    let composition: Vec<Value> = composition
        .iter()
        .map(|(fiber, percent)| json!({"fiber": fiber, "percent": percent}))
        .collect();
    json!({"composition": composition, "construction": "knit", "stretch": stretch})
}

fn tee(model: &str, fabric: Option<Value>) -> Value {
    let mut variant = json!({
        "kind": "variant", "brand": "SyntheticFitV2", "model": model, "category": "tops/tshirt",
        "sizeLabel": "M", "sizeSystem": "alpha", "cut": "regular", "version": "2026-A",
        "provenance": {"certainty": "explicit", "source": "label"},
    });
    if let Some(fabric) = fabric {
        variant["fabric"] = fabric;
    }
    variant
}

async fn save_variant(h: &Harness, label: &str, record: Value) -> Result<String, String> {
    h.try_call(
        "stylist_record_save",
        json!({"namespace": NS, "idempotencyKey": key(label), "record": record}),
    )
    .await
    .map(|called| id(&called.structured))
}

async fn worn_piece(h: &Harness, label: &str, variant: &str, verdicts: &[(&str, &str)]) -> String {
    let piece = id(&h
        .save(
            NS,
            &key(&format!("piece-{label}")),
            json!({"kind": "piece", "label": label, "category": "tops/tshirt",
                   "variantId": variant, "provenance": stated()}),
        )
        .await);
    for (i, (day, verdict)) in verdicts.iter().enumerate() {
        h.event(
            NS,
            &key(&format!("wear-{label}-{i}")),
            day,
            json!({"type": "wear", "pieceId": piece,
                   "regionFit": [{"region": "chest", "verdict": verdict}]}),
        )
        .await;
    }
    piece
}

async fn forecast(
    h: &Harness,
    label: &str,
    variant: &str,
    basis: &str,
    decision: &str,
) -> Result<Value, String> {
    let snapshot = h.context(NS, json!({})).await["snapshot"].clone();
    h.try_call(
        "stylist_record_save",
        json!({"namespace": NS, "idempotencyKey": key(label), "record": {
            "kind": "prediction", "variantId": variant, "recommendedSize": "M",
            "decision": decision, "basis": basis,
            "regionExpectations": [{"region": "chest", "expected": "good", "rationale": "history"}],
            "likelyFailureMode": {"description": "fabric behaves differently"},
            "returnRisk": {"level": "medium", "rationale": "returns accepted"},
            "contextRevision": snapshot["ledgerRevision"],
            "contextToken": snapshot["contextToken"],
        }}),
    )
    .await
    .map(|called| called.structured)
}

#[tokio::test(start_paused = true)]
async fn elastane_added_to_the_same_tee_is_a_new_variant_and_not_an_exact_repeat() {
    let h = Harness::new().await;
    let cotton = fabric(&[("cotton", 100.0)], "none");
    let stretch = fabric(&[("cotton", 95.0), ("elastane", 5.0)], "elastane");
    let original = save_variant(&h, "original", tee("ReferenceTee", Some(cotton.clone())))
        .await
        .unwrap();
    worn_piece(&h, "original", &original, &[("2026-10-01", "good")]).await;

    // A different material is a different product: no allowDuplicate needed.
    let candidate = save_variant(&h, "stretch", tee("ReferenceTee", Some(stretch.clone())))
        .await
        .unwrap();
    let again = save_variant(&h, "stretch-again", tee("ReferenceTee", Some(stretch)))
        .await
        .unwrap_err();
    assert!(again.starts_with("[duplicate_variant]"), "{again}");
    let unrecorded = save_variant(&h, "unrecorded", tee("ReferenceTee", None))
        .await
        .unwrap_err();
    assert!(
        unrecorded.starts_with("[duplicate_variant]"),
        "{unrecorded}"
    );

    let exact = forecast(&h, "exact", &candidate, "exact_repeat", "recommend")
        .await
        .unwrap_err();
    assert!(exact.starts_with("[basis_mismatch]"), "{exact}");
    assert!(exact.contains("supports known_brand_new_cut"), "{exact}");
    assert!(
        exact.contains("elastane 0% vs 5% (stretch content)"),
        "{exact}"
    );
    assert!(exact.contains("stretch none vs elastane"), "{exact}");
    let honest = forecast(&h, "honest", &candidate, "known_brand_new_cut", "recommend")
        .await
        .unwrap();
    let matched = &honest["record"]["derived"]["basisAssessment"]["identityMatches"][0];
    assert_eq!(matched["material"], json!("differs"));
    assert_eq!(matched["fit"], json!("proven"));
    assert_eq!(
        honest["record"]["derived"]["variantSnapshot"]["fabric"]["composition"][1]["fiber"],
        json!("elastane")
    );

    // A used variant's recorded material cannot be rewritten.
    let rewrite = h
        .reject(
            "stylist_record_save",
            json!({"namespace": NS, "idempotencyKey": key("rewrite"), "id": original,
                   "expectedRevision": 1,
                   "record": tee("ReferenceTee", Some(fabric(&[("cotton", 95.0), ("elastane", 5.0)], "elastane")))}),
        )
        .await;
    assert!(rewrite.starts_with("[immutable]"), "{rewrite}");
    assert!(cotton.is_object());
}

#[tokio::test(start_paused = true)]
async fn label_rounding_matches_but_meaningful_composition_changes_do_not() {
    let h = Harness::new().await;
    let blend = save_variant(
        &h,
        "blend",
        tee(
            "BlendTee",
            Some(fabric(&[("cotton", 60.0), ("polyester", 40.0)], "none")),
        ),
    )
    .await
    .unwrap();
    worn_piece(&h, "blend", &blend, &[("2026-10-01", "good")]).await;
    let rounded = save_variant(
        &h,
        "rounded",
        tee(
            "BlendTee",
            Some(fabric(&[("cotton", 62.0), ("polyester", 38.0)], "none")),
        ),
    )
    .await;
    // Within label rounding the material agrees, so this is the same product.
    assert!(rounded.unwrap_err().starts_with("[duplicate_variant]"));
    let mut rounded = tee(
        "BlendTee",
        Some(fabric(&[("cotton", 62.0), ("polyester", 38.0)], "none")),
    );
    rounded["allowDuplicate"] = json!(true);
    let rounded = save_variant(&h, "rounded-dup", rounded).await.unwrap();
    let exact = forecast(&h, "rounded-exact", &rounded, "exact_repeat", "recommend")
        .await
        .unwrap();
    assert_eq!(
        exact["record"]["derived"]["computedBasis"],
        json!("exact_repeat")
    );

    let shifted = save_variant(
        &h,
        "shifted",
        tee(
            "BlendTee",
            Some(fabric(&[("cotton", 50.0), ("polyester", 50.0)], "none")),
        ),
    )
    .await
    .unwrap();
    let error = forecast(&h, "shifted-exact", &shifted, "exact_repeat", "recommend")
        .await
        .unwrap_err();
    assert!(error.contains("cotton 60% vs 50%"), "{error}");

    let low = save_variant(
        &h,
        "low",
        tee(
            "StretchTee",
            Some(fabric(&[("cotton", 98.0), ("elastane", 2.0)], "elastane")),
        ),
    )
    .await
    .unwrap();
    worn_piece(&h, "low", &low, &[("2026-10-01", "good")]).await;
    let high = save_variant(
        &h,
        "high",
        tee(
            "StretchTee",
            Some(fabric(&[("cotton", 97.0), ("elastane", 3.0)], "elastane")),
        ),
    )
    .await
    .unwrap();
    let error = forecast(&h, "high-exact", &high, "exact_repeat", "recommend")
        .await
        .unwrap_err();
    assert!(
        error.contains("elastane 2% vs 3% (stretch content)"),
        "{error}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_later_adverse_report_is_not_cancelled_by_an_earlier_success() {
    let h = Harness::new().await;
    let cotton = fabric(&[("cotton", 100.0)], "none");
    let shrank = save_variant(&h, "shrank", tee("ReferenceTee", Some(cotton.clone())))
        .await
        .unwrap();
    let piece = worn_piece(&h, "shrank", &shrank, &[("2026-10-01", "good")]).await;
    h.event(
        NS,
        &key("after-wash"),
        "2026-10-08",
        json!({"type": "feedback", "targetId": piece, "timing": "after_wash",
               "regionFit": [{"region": "chest", "verdict": "too_tight"}]}),
    )
    .await;
    let plain = forecast(&h, "plain", &shrank, "known_brand_new_cut", "recommend")
        .await
        .unwrap_err();
    assert!(plain.starts_with("[known_failure]"), "{plain}");
    let cautious = forecast(
        &h,
        "cautious",
        &shrank,
        "known_brand_new_cut",
        "recommend_with_caution",
    )
    .await
    .unwrap();
    let assessment = &cautious["record"]["derived"]["basisAssessment"];
    assert_eq!(assessment["knownFailures"], json!([piece]));
    assert_eq!(assessment["identityMatches"][0]["fit"], json!("mixed"));
    assert_eq!(assessment["identityMatches"][0]["latest"], json!("adverse"));
    let exact = forecast(
        &h,
        "exact",
        &shrank,
        "exact_repeat",
        "recommend_with_caution",
    )
    .await
    .unwrap_err();
    assert!(exact.contains("latest fit report is adverse"), "{exact}");

    // The reverse order (too tight once, fine on every later wear) is mixed
    // but not a known failure: it warns instead of refusing.
    let recovered = save_variant(&h, "recovered", tee("OtherTee", Some(cotton)))
        .await
        .unwrap();
    worn_piece(
        &h,
        "recovered",
        &recovered,
        &[("2026-09-01", "too_tight"), ("2026-10-05", "good")],
    )
    .await;
    let plain = forecast(
        &h,
        "recovered-plain",
        &recovered,
        "known_brand_new_cut",
        "recommend",
    )
    .await
    .unwrap();
    assert_eq!(
        plain["record"]["derived"]["basisAssessment"]["knownFailures"],
        json!([])
    );
    assert!(
        plain["warnings"].as_array().unwrap().iter().any(|w| w
            .as_str()
            .unwrap()
            .contains("mixed fit evidence, latest positive")),
        "{plain:#}"
    );
}
