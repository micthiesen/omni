//! Only a reported fit is fit evidence: comfort, style or value without a
//! known region verdict, or only `unknown` regions, never count as a fit
//! success or as proof for an exact repeat.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Harness, id, key, stated};
use serde_json::{Value, json};

const NS: &str = "fixture-fit-signals";

fn tee(model: &str) -> Value {
    json!({
        "kind": "variant", "brand": "SyntheticSignal", "model": model, "category": "tops/tshirt",
        "sizeLabel": "M", "sizeSystem": "alpha", "cut": "regular",
        "fabric": {"composition": [{"fiber": "cotton", "percent": 100}], "construction": "knit"},
        "provenance": {"certainty": "explicit", "source": "label"},
    })
}

/// A variant and a piece of it with one event; returns the variant id.
async fn reported(h: &Harness, model: &str, event: Value) -> String {
    let variant = id(&h
        .save(NS, &key(&format!("{model}-variant")), tee(model))
        .await);
    let piece = id(&h
        .save(
            NS,
            &key(&format!("{model}-piece")),
            json!({"kind": "piece", "label": model, "category": "tops/tshirt",
                   "variantId": variant, "provenance": stated()}),
        )
        .await);
    let mut event = event;
    if event.get("pieceId").is_some() {
        event["pieceId"] = json!(piece);
    } else {
        event["targetId"] = json!(piece);
    }
    h.event(NS, &key(&format!("{model}-event")), "2026-10-05", event)
        .await;
    variant
}

async fn exact_repeat(h: &Harness, label: &str, variant: &str) -> Result<Value, String> {
    let snapshot = h.context(NS, json!({})).await["snapshot"].clone();
    h.try_call(
        "stylist_record_save",
        json!({"namespace": NS, "idempotencyKey": key(label), "record": {
            "kind": "prediction", "variantId": variant, "recommendedSize": "M",
            "decision": "recommend", "basis": "exact_repeat",
            "regionExpectations": [{"region": "chest", "expected": "good", "rationale": "rebuy"}],
            "likelyFailureMode": {"description": "none known"},
            "returnRisk": {"level": "low", "rationale": "rebuy"},
            "contextRevision": snapshot["ledgerRevision"],
            "contextToken": snapshot["contextToken"],
        }}),
    )
    .await
    .map(|called| called.structured)
}

#[tokio::test(start_paused = true)]
async fn comfort_style_and_unknown_reports_are_not_fit_success() {
    let h = Harness::new().await;
    let comfort = reported(
        &h,
        "ComfortOnly",
        json!({"type": "feedback", "targetId": "", "timing": "after_wear", "regionFit": [],
               "comfort": {"rating": "positive", "note": "soft, wears all day"}}),
    )
    .await;
    let style = reported(
        &h,
        "StyleOnly",
        json!({"type": "feedback", "targetId": "", "timing": "delayed", "regionFit": [],
               "style": {"rating": "positive"}}),
    )
    .await;
    let unknown = reported(
        &h,
        "UnknownOnly",
        json!({"type": "wear", "pieceId": "", "comfort": {"rating": "positive"},
               "regionFit": [{"region": "chest", "verdict": "unknown"}]}),
    )
    .await;
    // Without a known verdict the category still has no fit evidence.
    let focused = h.context(NS, json!({"category": "tops/tshirt"})).await;
    assert!(
        focused["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["code"] == json!("no_fit_evidence") && w["severity"] == json!("blocking")),
        "{:#}",
        focused["warnings"]
    );
    let fitted = reported(
        &h,
        "Fitted",
        json!({"type": "wear", "pieceId": "",
               "regionFit": [{"region": "chest", "verdict": "good"}, {"region": "body_length", "verdict": "slightly_long"}]}),
    )
    .await;

    let context = h.context(NS, json!({})).await;
    let history = &context["history"];
    let subjects = |list: &str| -> Vec<String> {
        history[list]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["model"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    assert_eq!(subjects("successes"), ["Fitted"], "{history:#}");
    let unresolved = subjects("unresolved");
    for model in ["ComfortOnly", "StyleOnly", "UnknownOnly"] {
        assert!(
            unresolved.iter().any(|m| m == model),
            "{model}: {history:#}"
        );
    }
    let comfort_entry = history["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["model"] == json!("ComfortOnly"))
        .unwrap();
    assert_eq!(
        comfort_entry["detail"],
        json!("no regional fit reported (comfort positive)")
    );
    let fitted_entry = &history["successes"][0];
    assert_eq!(
        fitted_entry["detail"],
        json!("slight: body_length slightly_long")
    );

    for (label, variant) in [
        ("comfort", &comfort),
        ("style", &style),
        ("unknown", &unknown),
    ] {
        let error = exact_repeat(&h, label, variant).await.unwrap_err();
        assert!(error.starts_with("[basis_mismatch]"), "{label}: {error}");
        assert!(error.contains("no recorded fit"), "{label}: {error}");
    }
    let proven = exact_repeat(&h, "fitted", &fitted).await.unwrap();
    assert_eq!(
        proven["record"]["derived"]["computedBasis"],
        json!("exact_repeat")
    );
    assert_eq!(
        proven["record"]["derived"]["basisRule"],
        json!("fit-evidence-v4")
    );
}
