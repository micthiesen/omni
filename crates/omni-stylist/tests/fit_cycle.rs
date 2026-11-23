//! The full fit cycle: a measured reference, a candidate, a frozen forecast,
//! a try-on outcome, a post-wash adverse outcome and a narrowly revised rule,
//! recovered intact by a cold session.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Harness, body_measurement, garment_measurement, id, key, measured, stated, tee};
use serde_json::{Value, json};

const NS: &str = "fixture-fit-cycle";

struct Cycle {
    reference_piece: String,
    candidate: String,
    prediction: String,
    piece: String,
    try_on: String,
    wash: String,
    after_wash: String,
    prediction_input: Value,
}

async fn run_cycle(h: &Harness) -> Cycle {
    h.save(
        NS,
        &key("chest"),
        body_measurement("chest", 100.0, "cm", "2026-10-01"),
    )
    .await;

    let reference_variant = id(&h
        .save(
            NS,
            &key("ref-variant"),
            tee("Uniqlo", "Supima Cotton Crew Neck T-Shirt", "M", "regular"),
        )
        .await);
    let reference_piece = id(&h
        .save(
            NS,
            &key("ref-piece"),
            json!({
                "kind": "piece",
                "label": "white Supima tee",
                "category": "tops/tshirt",
                "variantId": reference_variant,
                "reference": {"categories": ["tops/tshirt"], "role": "baseline", "regions": ["chest", "body_length"]},
                "provenance": stated(),
            }),
        )
        .await);
    let reference_wash = id(&h
        .event(
            NS,
            &key("ref-wash"),
            "2026-09-01",
            json!({
                "type": "wash", "pieceId": reference_piece, "water": "cold", "drying": "line_dry",
            }),
        )
        .await);
    let mut chest = garment_measurement(
        "piece",
        &reference_piece,
        "chest",
        "flat_width",
        53.0,
        "cm",
        "washed",
        "2026-09-02",
    );
    chest["garmentState"]["afterEventId"] = json!(reference_wash);
    let reference_chest = id(&h.save(NS, &key("ref-chest"), chest).await);
    let reference_length = id(&h
        .save(
            NS,
            &key("ref-length"),
            garment_measurement(
                "piece",
                &reference_piece,
                "body_length",
                "linear",
                70.0,
                "cm",
                "washed",
                "2026-09-02",
            ),
        )
        .await);

    let candidate = id(&h
        .save(
            NS,
            &key("candidate"),
            tee("Uniqlo", "U Crew Neck T-Shirt", "M", "relaxed"),
        )
        .await);
    h.save(
        NS,
        &key("pub-chest"),
        garment_measurement(
            "variant_published",
            &candidate,
            "chest",
            "flat_width",
            55.0,
            "cm",
            "unwashed",
            "2026-10-10",
        ),
    )
    .await;
    h.save(
        NS,
        &key("pub-length"),
        garment_measurement(
            "variant_published",
            &candidate,
            "body_length",
            "linear",
            69.0,
            "cm",
            "unwashed",
            "2026-10-10",
        ),
    )
    .await;

    let compared = h
        .call(
            "stylist_measurements_compare",
            json!({
                "namespace": NS, "referenceIds": [reference_piece], "candidateIds": [candidate],
            }),
        )
        .await;
    let deltas: Vec<(String, f64)> = compared["comparable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["reference"]["dimension"].as_str().unwrap().to_owned(),
                p["deltaMm"].as_f64().unwrap(),
            )
        })
        .collect();
    assert!(deltas.contains(&("chest".to_owned(), 20.0)), "{deltas:?}");
    assert!(
        deltas.contains(&("body_length".to_owned(), -10.0)),
        "{deltas:?}"
    );

    let context = h.context(NS, json!({"variantId": candidate})).await;
    let revision = context["snapshot"]["ledgerRevision"].as_u64().unwrap();
    let token = context["snapshot"]["contextToken"].clone();
    assert_eq!(
        context["references"][0]["piece"]["id"],
        json!(reference_piece)
    );
    assert_eq!(context["references"][0]["washedBaseline"], json!(true));

    let prediction_record = json!({
        "kind": "prediction",
        "variantId": candidate,
        "recommendedSize": "M",
        "decision": "recommend_with_caution",
        "basis": "known_brand_new_cut",
        "regionExpectations": [
            {"region": "chest", "expected": "slightly_loose", "rationale": "published flat chest 2 cm over the washed reference", "evidenceRefs": [reference_chest]},
            {"region": "body_length", "expected": "good", "rationale": "1 cm shorter than the reference before washing", "evidenceRefs": [reference_length]},
        ],
        "likelyFailureMode": {"region": "body_length", "description": "untreated cotton knit may shrink in length after hot washing"},
        "evidenceGaps": ["no post-wash measurements for this model"],
        "referencesUsed": [reference_piece, reference_chest, reference_length],
        "returnRisk": {"level": "medium", "returnBy": "2026-11-11", "rationale": "free in-store returns, unwashed only"},
        "contextRevision": revision,
        "contextToken": token,
    });

    let mut overclaimed = prediction_record.clone();
    overclaimed["basis"] = json!("exact_repeat");
    let overclaim = h
        .reject(
            "stylist_record_save",
            json!({
                "namespace": NS, "idempotencyKey": key("overclaim"), "record": overclaimed,
            }),
        )
        .await;
    assert!(overclaim.starts_with("[basis_mismatch]"), "{overclaim}");

    let prediction = id(&h
        .save(NS, &key("prediction"), prediction_record.clone())
        .await);

    let piece = id(&h
        .save(
            NS,
            &key("piece"),
            json!({
                "kind": "piece", "label": "black U tee", "category": "tops/tshirt",
                "variantId": candidate, "initialStatus": "undecided", "provenance": stated(),
            }),
        )
        .await);
    h.event(NS, &key("acquired"), "2026-10-12", json!({
        "type": "acquired", "pieceId": piece, "predictionId": prediction, "statusAfter": "undecided",
        "order": {"retailer": "Uniqlo", "orderRef": "UQ-1001", "price": {"amount": 19.9, "currency": "CAD"}, "returnBy": "2026-11-11"},
    }))
    .await;
    let try_on = id(&h
        .event(NS, &key("try-on"), "2026-10-12", json!({
            "type": "try_on", "pieceId": piece, "predictionId": prediction, "overall": "fits",
            "regionFit": [{"region": "chest", "verdict": "slightly_loose"}, {"region": "body_length", "verdict": "good"}],
        }))
        .await);
    h.event(
        NS,
        &key("decision"),
        "2026-10-12",
        json!({
            "type": "decision", "pieceId": piece, "decision": "keep",
            "reasons": [{"aspect": "fit", "note": "matches the forecast"}],
        }),
    )
    .await;
    let immediate = h
        .event(NS, &key("outcome-1"), "2026-10-12", json!({
            "type": "outcome", "predictionId": prediction, "pieceId": piece, "stage": "immediate",
            "result": "matched", "basedOnEventIds": [try_on],
            "regionResults": [{"region": "chest", "verdict": "slightly_loose"}, {"region": "body_length", "verdict": "good"}],
        }))
        .await;
    let comparison = &immediate["event"]["derived"]["forecastComparison"];
    assert!(
        comparison
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["match"] == json!("exact"))
    );

    let wash = id(&h
        .event(
            NS,
            &key("wash"),
            "2026-10-12",
            json!({
                "type": "wash", "pieceId": piece, "water": "hot", "drying": "tumble_high",
                "observedChanges": [{"region": "body_length", "change": "shrank"}],
            }),
        )
        .await);
    let mut post = garment_measurement(
        "piece",
        &piece,
        "body_length",
        "linear",
        66.0,
        "cm",
        "washed",
        "2026-10-12",
    );
    post["garmentState"]["afterEventId"] = json!(wash);
    post["garmentState"]["washCount"] = json!(1);
    h.save(NS, &key("post-length"), post).await;
    let after_wash = h
        .event(NS, &key("outcome-2"), "2026-10-12", json!({
            "type": "outcome", "predictionId": prediction, "pieceId": piece, "stage": "after_wash",
            "result": "missed", "basedOnEventIds": [wash],
            "regionResults": [{"region": "chest", "verdict": "good"}, {"region": "body_length", "verdict": "too_short"}],
        }))
        .await;
    let misses = after_wash["event"]["derived"]["forecastMisses"]
        .as_u64()
        .unwrap();
    assert_eq!(misses, 1);

    Cycle {
        reference_piece,
        candidate,
        prediction,
        piece,
        try_on,
        wash,
        after_wash: id(&after_wash),
        prediction_input: prediction_record,
    }
}

#[tokio::test(start_paused = true)]
async fn measured_reference_to_revised_rule_keeps_the_forecast_frozen() {
    let h = Harness::new().await;
    let cycle = run_cycle(&h).await;

    // One outcome per stage: a second after-wash outcome is a correction, not a new fact.
    let duplicate = h
        .reject("stylist_event_append", json!({
            "namespace": NS, "idempotencyKey": key("outcome-3"), "occurredOn": "2026-10-12",
            "event": {"type": "outcome", "predictionId": cycle.prediction, "stage": "after_wash", "result": "missed"},
        }))
        .await;
    assert!(duplicate.starts_with("[duplicate_outcome]"), "{duplicate}");

    // A narrow rule cannot go active before every in-scope case is checked.
    let rule = json!({
        "kind": "rule",
        "statement": "Uniqlo U crew tees lose about 3 cm of body length after a hot wash and tumble dry",
        "scope": {"brand": "Uniqlo", "model": "U Crew Neck T-Shirt", "region": "body_length"},
        "adjustment": {"region": "body_length", "direction": "shrinks", "magnitude": 3, "unit": "cm"},
        "basis": "known_brand_new_cut",
        "status": "active",
        "evidence": [cycle.after_wash, cycle.wash],
    });
    let incomplete = h
        .reject(
            "stylist_record_save",
            json!({"namespace": NS, "idempotencyKey": key("rule-early"), "record": rule}),
        )
        .await;
    assert!(
        incomplete.starts_with("[regression_incomplete]"),
        "{incomplete}"
    );
    assert!(incomplete.contains(&cycle.try_on) && incomplete.contains(&cycle.after_wash));

    let mut proposed = rule.clone();
    proposed["status"] = json!("proposed");
    let saved = h.save(NS, &key("rule"), proposed).await;
    let rule_id = id(&saved);
    let cases: Vec<String> = saved["record"]["derived"]["inScopeCases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        cases.len(),
        4,
        "try-on, immediate outcome, wash, after-wash outcome: {cases:?}"
    );
    let regression: Vec<Value> = cases
        .iter()
        .map(|case| {
            let verdict = if *case == cycle.try_on {
                "not_applicable"
            } else {
                "consistent"
            };
            json!({"caseId": case, "verdict": verdict})
        })
        .collect();
    let mut active = rule.clone();
    active["regression"] = json!(regression);
    let activated = h
        .call(
            "stylist_record_save",
            json!({
                "namespace": NS, "idempotencyKey": key("rule-activate"), "id": rule_id,
                "expectedRevision": 1, "record": active,
            }),
        )
        .await;
    assert_eq!(activated["record"]["status"], json!("active"));
    assert_eq!(activated["record"]["revision"], json!(2));

    // A later in-scope case puts the rule up for review before reuse.
    h.event(
        NS,
        &key("delayed"),
        "2026-10-12",
        json!({
            "type": "feedback", "targetId": cycle.piece, "timing": "delayed",
            "regionFit": [{"region": "body_length", "verdict": "too_short"}],
            "regret": {"note": "too short to wear untucked"},
        }),
    )
    .await;
    let context = h.context(NS, json!({"category": "tops/tshirt"})).await;
    assert_eq!(context["rules"]["needsReview"], json!([rule_id]));

    // The forecast is exactly what was frozen, with a single revision.
    let history = h
        .call(
            "stylist_records_query",
            json!({
                "namespace": NS, "ids": [cycle.prediction], "includeHistory": true,
            }),
        )
        .await;
    let revisions = history["records"].as_array().unwrap();
    assert_eq!(revisions.len(), 1);
    let frozen = &revisions[0];
    assert_eq!(frozen["revision"], json!(1));
    assert_eq!(frozen["data"], cycle.prediction_input);
    assert_eq!(frozen["derived"]["runbookVersion"], json!("fit-first-v3"));
    let revise = h
        .reject(
            "stylist_record_save",
            json!({
                "namespace": NS, "idempotencyKey": key("revise-forecast"), "id": cycle.prediction,
                "expectedRevision": 1, "record": cycle.prediction_input,
            }),
        )
        .await;
    assert!(revise.starts_with("[immutable]"), "{revise}");
}

#[tokio::test(start_paused = true)]
async fn a_cold_session_recovers_evidence_and_counterexamples() {
    let h = Harness::new().await;
    let cycle = run_cycle(&h).await;
    let cold = h.cold();
    let context = cold.context(NS, json!({"category": "tops/tshirt"})).await;

    let failures = context["history"]["failures"].as_array().unwrap();
    let failure_events: Vec<&str> = failures
        .iter()
        .filter_map(|f| f["eventId"].as_str())
        .collect();
    assert!(
        failure_events.contains(&cycle.after_wash.as_str()),
        "{failures:#?}"
    );
    assert!(failure_events.contains(&cycle.wash.as_str()));
    let after_wash = failures
        .iter()
        .find(|f| f["eventId"] == json!(cycle.after_wash))
        .unwrap();
    assert_eq!(after_wash["predictionId"], json!(cycle.prediction));
    assert!(
        after_wash["comparison"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["region"] == json!("body_length") && c["match"] == json!("miss"))
    );

    let successes = context["history"]["successes"].as_array().unwrap();
    assert!(
        successes
            .iter()
            .any(|s| s["eventId"] == json!(cycle.try_on))
    );
    assert_eq!(
        context["references"][0]["piece"]["id"],
        json!(cycle.reference_piece)
    );
    assert_eq!(
        context["profile"]["bodyMeasurements"][0]["latest"]["valueMm"],
        json!(1000)
    );

    // The same context as of the forecast's revision has no outcomes yet.
    let frozen = cold
        .call(
            "stylist_records_query",
            json!({"namespace": NS, "ids": [cycle.prediction]}),
        )
        .await;
    let at = frozen["records"][0]["ledgerRevision"].as_u64().unwrap();
    let earlier = cold
        .call("stylist_context_get", json!({
            "namespace": NS, "focus": {"variantId": cycle.candidate}, "asOfRevision": at, "includeRunbook": false,
        }))
        .await;
    assert_eq!(earlier["snapshot"]["ledgerRevision"], json!(at));
    assert_eq!(earlier["history"]["failures"], json!([]));
    assert!(
        earlier["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["code"] == json!("historical_snapshot"))
    );

    // Replaying the context reproduces its digest.
    let again = cold
        .call("stylist_context_get", json!({
            "namespace": NS, "focus": {"variantId": cycle.candidate}, "asOfRevision": at, "includeRunbook": false,
        }))
        .await;
    assert_eq!(
        earlier["snapshot"]["contextSha256"],
        again["snapshot"]["contextSha256"]
    );
}

#[tokio::test(start_paused = true)]
async fn kept_and_unworn_are_unresolved_not_success() {
    let h = Harness::new().await;
    let variant = id(&h
        .save(
            NS,
            &key("variant"),
            tee("Muji", "Organic Cotton Tee", "L", "regular"),
        )
        .await);
    let piece = id(&h
        .save(
            NS,
            &key("piece"),
            json!({
                "kind": "piece", "label": "grey Muji tee", "category": "tops/tshirt",
                "variantId": variant, "initialStatus": "undecided", "provenance": stated(),
            }),
        )
        .await);
    h.event(
        NS,
        &key("keep"),
        "2026-10-10",
        json!({"type": "decision", "pieceId": piece, "decision": "keep"}),
    )
    .await;
    let context = h.context(NS, json!({"category": "tops"})).await;
    assert_eq!(context["history"]["successes"], json!([]));
    let unresolved = context["history"]["unresolved"].as_array().unwrap();
    assert!(
        unresolved
            .iter()
            .any(|u| u["kind"] == json!("kept_unassessed") && u["pieceId"] == json!(piece))
    );
    let _ = measured();
}
