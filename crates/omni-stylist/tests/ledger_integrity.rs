//! Ledger integrity: idempotency, optimistic concurrency, supersession,
//! incompatible measurements, pagination, contradictions and namespace isolation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Harness, body_measurement, garment_measurement, id, key, stated, tee};
use serde_json::json;

const NS: &str = "fixture-integrity";

#[tokio::test(start_paused = true)]
async fn duplicate_writes_replay_and_reused_keys_are_rejected() {
    let h = Harness::new().await;
    let first = h
        .save(
            NS,
            &key("variant"),
            tee("Arket", "Heavyweight Tee", "M", "boxy"),
        )
        .await;
    let again = h
        .save(
            NS,
            &key("variant"),
            tee("Arket", "Heavyweight Tee", "M", "boxy"),
        )
        .await;
    assert_eq!(again["replayed"], json!(true));
    assert_eq!(id(&again), id(&first));
    assert_eq!(again["ledgerRevision"], first["ledgerRevision"]);

    let reused = h
        .reject(
            "stylist_record_save",
            json!({
                "namespace": NS, "idempotencyKey": key("variant"),
                "record": tee("Arket", "Heavyweight Tee", "L", "boxy"),
            }),
        )
        .await;
    assert!(reused.starts_with("[idempotency_key_reused]"), "{reused}");

    let duplicate = h
        .reject(
            "stylist_record_save",
            json!({
                "namespace": NS, "idempotencyKey": key("variant-again"),
                "record": tee("arket", "heavyweight  tee", "M", "Boxy"),
            }),
        )
        .await;
    assert!(duplicate.starts_with("[duplicate_variant]"), "{duplicate}");

    // Receipts: one acquisition per order line, and a replay is not a second purchase.
    let variant = id(&first);
    let piece = |label: &str| {
        json!({"kind": "piece", "label": label, "category": "tops/tshirt", "variantId": variant,
               "initialStatus": "undecided", "provenance": stated()})
    };
    let a = id(&h.save(NS, &key("piece-a"), piece("tee a")).await);
    let b = id(&h.save(NS, &key("piece-b"), piece("tee b")).await);
    let receipt = |piece: &str| {
        json!({"type": "acquired", "pieceId": piece, "statusAfter": "undecided",
               "order": {"retailer": "Arket", "orderRef": "AR-77", "returnBy": "2026-11-01"}})
    };
    let acquired = h
        .event(NS, &key("receipt-a"), "2026-10-11", receipt(&a))
        .await;
    let replay = h
        .event(NS, &key("receipt-a"), "2026-10-11", receipt(&a))
        .await;
    assert_eq!(replay["replayed"], json!(true));
    assert_eq!(id(&replay), id(&acquired));
    let second = h
        .reject(
            "stylist_event_append",
            json!({
                "namespace": NS, "idempotencyKey": key("receipt-b"), "occurredOn": "2026-10-11",
                "event": receipt(&b),
            }),
        )
        .await;
    assert!(second.starts_with("[duplicate_receipt]"), "{second}");
    let events = h
        .call(
            "stylist_records_query",
            json!({"namespace": NS, "collection": "events", "kinds": ["acquired"]}),
        )
        .await;
    assert_eq!(events["total"], json!(1));
}

#[tokio::test(start_paused = true)]
async fn stale_revisions_are_rejected_and_history_is_kept() {
    let h = Harness::new().await;
    let saved = h
        .save(NS, &key("pref"), json!({
            "kind": "preference", "category": "tops", "aspect": "length", "region": "body_length",
            "statement": "hits at the hip bone", "strength": "prefer", "validFrom": "2025-03-01",
            "provenance": stated(),
        }))
        .await;
    let pref = id(&saved);
    let revise = |key_label: &str, expected: u64, statement: &str, from: &str| {
        json!({
            "namespace": NS, "idempotencyKey": key(key_label), "id": pref, "expectedRevision": expected,
            "record": {"kind": "preference", "category": "tops", "aspect": "length", "region": "body_length",
                       "statement": statement, "strength": "prefer", "validFrom": from, "provenance": stated()},
        })
    };
    let second = h
        .call(
            "stylist_record_save",
            revise(
                "pref-2",
                1,
                "slightly cropped, above the hip bone",
                "2026-09-01",
            ),
        )
        .await;
    assert_eq!(second["record"]["revision"], json!(2));
    let stale = h
        .reject(
            "stylist_record_save",
            revise("pref-3", 1, "long", "2026-10-01"),
        )
        .await;
    assert!(stale.starts_with("[revision_conflict]"), "{stale}");
    let backdated = h
        .reject(
            "stylist_record_save",
            revise("pref-4", 2, "long", "2024-01-01"),
        )
        .await;
    assert!(backdated.starts_with("[validation]"), "{backdated}");

    let history = h
        .call(
            "stylist_records_query",
            json!({"namespace": NS, "ids": [pref], "includeHistory": true}),
        )
        .await;
    let statements: Vec<&str> = history["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["data"]["statement"].as_str().unwrap())
        .collect();
    assert_eq!(
        statements,
        [
            "hits at the hip bone",
            "slightly cropped, above the hip bone"
        ]
    );
    let context = h.context(NS, json!({"category": "tops/tshirt"})).await;
    let change = &context["preferences"]["recentChanges"][0];
    assert_eq!(change["previousStatement"], json!("hits at the hip bone"));
}

#[tokio::test(start_paused = true)]
async fn corrections_supersede_without_erasing() {
    let h = Harness::new().await;
    let wrong = id(&h
        .save(
            NS,
            &key("waist"),
            body_measurement("waist_trouser", 31.0, "in", "2026-10-01"),
        )
        .await);
    let immutable = h
        .reject("stylist_record_save", json!({
            "namespace": NS, "idempotencyKey": key("waist-edit"), "id": wrong, "expectedRevision": 1,
            "record": body_measurement("waist_trouser", 32.0, "in", "2026-10-01"),
        }))
        .await;
    assert!(immutable.starts_with("[immutable]"), "{immutable}");
    let corrected = h
        .call(
            "stylist_record_save",
            json!({
                "namespace": NS, "idempotencyKey": key("waist-fix"), "supersedes": wrong,
                "correctionReason": "misread the tape: 32, not 31",
                "record": body_measurement("waist_trouser", 32.0, "in", "2026-10-01"),
            }),
        )
        .await;
    let current = h
        .call(
            "stylist_records_query",
            json!({"namespace": NS, "kinds": ["measurement"]}),
        )
        .await;
    assert_eq!(current["total"], json!(1));
    assert_eq!(current["records"][0]["id"], corrected["record"]["id"]);
    let all = h
        .call(
            "stylist_records_query",
            json!({"namespace": NS, "kinds": ["measurement"], "includeSuperseded": true}),
        )
        .await;
    assert_eq!(all["total"], json!(2));
    let old = all["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(wrong))
        .unwrap();
    assert_eq!(old["status"], json!("superseded"));
    assert_eq!(old["supersededBy"], corrected["record"]["id"]);
    assert_eq!(old["data"]["value"], json!(31));

    // Events are corrected the same way.
    let variant = id(&h
        .save(
            NS,
            &key("variant"),
            tee("Cos", "Regular Tee", "M", "regular"),
        )
        .await);
    let piece = id(&h
        .save(NS, &key("piece"), json!({"kind": "piece", "label": "cos tee", "category": "tops/tshirt", "variantId": variant, "provenance": stated()}))
        .await);
    let first = id(&h
        .event(
            NS,
            &key("wear"),
            "2026-10-10",
            json!({"type": "wear", "pieceId": piece,
            "regionFit": [{"region": "chest", "verdict": "too_tight"}]}),
        )
        .await);
    let fixed = h
        .call("stylist_event_append", json!({
            "namespace": NS, "idempotencyKey": key("wear-fix"), "occurredOn": "2026-10-10",
            "supersedesEventId": first, "correctionReason": "it was the shoulders, not the chest",
            "event": {"type": "wear", "pieceId": piece, "regionFit": [{"region": "shoulders", "verdict": "too_tight"}]},
        }))
        .await;
    let events = h
        .call("stylist_records_query", json!({"namespace": NS, "collection": "events", "includeSuperseded": true, "linkedTo": piece}))
        .await;
    let superseded = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == json!(first))
        .unwrap();
    assert_eq!(superseded["supersededBy"], fixed["event"]["id"]);
    let wrong_type = h
        .reject(
            "stylist_event_append",
            json!({
                "namespace": NS, "idempotencyKey": key("wear-fix-2"), "occurredOn": "2026-10-10",
                "supersedesEventId": first, "correctionReason": "again",
                "event": {"type": "wear", "pieceId": piece},
            }),
        )
        .await;
    assert!(
        wrong_type.starts_with("[revision_conflict]"),
        "{wrong_type}"
    );
}

#[tokio::test(start_paused = true)]
async fn incompatible_measurements_are_rejected_or_kept_apart() {
    let h = Harness::new().await;
    let mut flat_body = body_measurement("chest", 50.0, "cm", "2026-10-01");
    flat_body["method"] = json!("flat_width");
    let rejected = h
        .reject(
            "stylist_record_save",
            json!({"namespace": NS, "idempotencyKey": key("flat-body"), "record": flat_body}),
        )
        .await;
    assert!(
        rejected.starts_with("[incompatible_measurement]"),
        "{rejected}"
    );

    let no_state = h
        .reject("stylist_record_save", json!({"namespace": NS, "idempotencyKey": key("no-state"), "record": {
            "kind": "measurement", "subject": {"type": "body"}, "dimension": "chest", "method": "circumference",
            "endpoints": "around", "value": 100, "unit": "cm", "observedOn": "2026-10-01",
            "garmentState": {"stretch": "relaxed", "wash": "washed"}, "provenance": stated(),
        }}))
        .await;
    assert!(no_state.starts_with("[validation]"), "{no_state}");

    let body = id(&h
        .save(
            NS,
            &key("body"),
            body_measurement("chest", 100.0, "cm", "2026-10-01"),
        )
        .await);
    let variant = id(&h
        .save(
            NS,
            &key("variant"),
            tee("Asket", "The T-Shirt", "M", "regular"),
        )
        .await);
    let piece = id(&h
        .save(NS, &key("piece"), json!({"kind": "piece", "label": "asket", "category": "tops/tshirt", "variantId": variant, "provenance": stated()}))
        .await);
    let flat = id(&h
        .save(
            NS,
            &key("flat"),
            garment_measurement(
                "piece",
                &piece,
                "chest",
                "flat_width",
                53.0,
                "cm",
                "washed",
                "2026-10-01",
            ),
        )
        .await);
    let round = id(&h
        .save(
            NS,
            &key("round"),
            garment_measurement(
                "variant_published",
                &variant,
                "chest",
                "circumference",
                106.0,
                "cm",
                "unwashed",
                "2026-10-01",
            ),
        )
        .await);
    let compared = h
        .call(
            "stylist_measurements_compare",
            json!({"namespace": NS, "referenceIds": [flat, body], "candidateIds": [round]}),
        )
        .await;
    assert_eq!(compared["comparable"], json!([]));
    let reasons: Vec<&str> = compared["incompatible"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons, ["different_method", "body_versus_garment"]);

    let future = h
        .reject(
            "stylist_record_save",
            json!({"namespace": NS, "idempotencyKey": key("future"),
            "record": body_measurement("hip", 100.0, "cm", "2027-01-01")}),
        )
        .await;
    assert!(future.contains("in the future"), "{future}");
}

#[tokio::test(start_paused = true)]
async fn pagination_is_exact_and_stable_at_a_revision() {
    let h = Harness::new().await;
    for i in 0..23 {
        h.save(
            NS,
            &key(&format!("need-{i}")),
            json!({
                "kind": "need", "category": "tops/tshirt", "reason": "restock", "priority": "low",
                "description": format!("restock tee {i}"), "status": "open", "provenance": stated(),
            }),
        )
        .await;
    }
    h.save(
        NS,
        &key("other"),
        json!({
            "kind": "need", "category": "bottoms/jeans", "reason": "gap", "priority": "high",
            "description": "dark jeans", "status": "open", "provenance": stated(),
        }),
    )
    .await;
    let page = |cursor: Option<u64>, as_of: Option<u64>| {
        let mut input = json!({"namespace": NS, "kinds": ["need"], "categoryPrefix": "tops",
                               "limit": 10, "order": "oldest"});
        if let Some(cursor) = cursor {
            input["cursor"] = json!(cursor);
        }
        if let Some(as_of) = as_of {
            input["asOfRevision"] = json!(as_of);
        }
        input
    };
    let first = h.call("stylist_records_query", page(None, None)).await;
    assert_eq!(first["total"], json!(23));
    assert_eq!(first["nextCursor"], json!(10));
    let snapshot = first["snapshotRevision"].as_u64().unwrap();
    h.save(
        NS,
        &key("late"),
        json!({
            "kind": "need", "category": "tops/tshirt", "reason": "gap", "priority": "medium",
            "description": "late addition", "status": "open", "provenance": stated(),
        }),
    )
    .await;
    let mut seen: Vec<String> = first["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect();
    let mut cursor = first["nextCursor"].as_u64();
    while let Some(next) = cursor {
        let page = h
            .call("stylist_records_query", page(Some(next), Some(snapshot)))
            .await;
        assert_eq!(page["total"], json!(23));
        assert_eq!(page["snapshotRevision"], json!(snapshot));
        seen.extend(
            page["records"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_owned()),
        );
        cursor = page["nextCursor"].as_u64();
    }
    assert_eq!(seen.len(), 23);
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 23);
    let now = h.call("stylist_records_query", page(None, None)).await;
    assert_eq!(now["total"], json!(24));
    let future = h
        .reject("stylist_records_query", page(None, Some(snapshot + 50)))
        .await;
    assert!(future.starts_with("[validation]"), "{future}");
}

#[tokio::test(start_paused = true)]
async fn contradictions_are_surfaced_until_resolved() {
    let h = Harness::new().await;
    let a = id(&h
        .save(
            NS,
            &key("chest-1"),
            body_measurement("chest", 100.0, "cm", "2026-09-20"),
        )
        .await);
    let b = id(&h
        .save(
            NS,
            &key("chest-2"),
            body_measurement("chest", 104.0, "cm", "2026-10-05"),
        )
        .await);
    h.save(
        NS,
        &key("hip-1"),
        body_measurement("hip", 100.0, "cm", "2026-09-20"),
    )
    .await;
    h.save(
        NS,
        &key("hip-2"),
        body_measurement("hip", 100.8, "cm", "2026-10-05"),
    )
    .await;

    let variant = id(&h
        .save(
            NS,
            &key("variant"),
            tee("Sunspel", "Classic Tee", "M", "regular"),
        )
        .await);
    let piece = id(&h
        .save(NS, &key("piece"), json!({"kind": "piece", "label": "sunspel", "category": "tops/tshirt", "variantId": variant, "provenance": stated()}))
        .await);
    let good = id(&h.event(NS, &key("wear-1"), "2026-10-01", json!({"type": "wear", "pieceId": piece, "regionFit": [{"region": "chest", "verdict": "good"}]})).await);
    let tight = id(&h.event(NS, &key("wear-2"), "2026-10-03", json!({"type": "wear", "pieceId": piece, "regionFit": [{"region": "chest", "verdict": "too_tight"}]})).await);
    h.save(NS, &key("pref-a"), json!({"kind": "preference", "category": "tops", "aspect": "roominess", "statement": "relaxed through the body", "strength": "prefer", "validFrom": "2026-01-01", "provenance": stated()})).await;
    h.save(NS, &key("pref-b"), json!({"kind": "preference", "category": "tops", "aspect": "roominess", "statement": "nothing baggy", "strength": "avoid", "validFrom": "2026-02-01", "provenance": stated()})).await;
    let flag = id(&h
        .event(NS, &key("flag"), "2026-10-06", json!({"type": "contradiction", "status": "open",
            "ids": [piece], "description": "owner says the tee is a medium but the label reads small"}))
        .await);

    let context = h.context(NS, json!({})).await;
    let kinds: Vec<&str> = context["contradictions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["body_measurement", "fit_report", "preference", "flagged"],
        "{:#}",
        context["contradictions"]
    );
    assert_eq!(context["contradictions"][0]["ids"], json!([a, b]));
    assert_eq!(context["contradictions"][1]["ids"], json!([good, tight]));
    assert!(
        context["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["code"] == json!("open_contradictions"))
    );

    h.event(
        NS,
        &key("resolve"),
        "2026-10-07",
        json!({"type": "contradiction", "status": "resolved",
        "ids": [piece], "description": "label checked: medium", "resolvesEventId": flag}),
    )
    .await;
    // A wash between two reports explains a change instead of contradicting it.
    h.event(
        NS,
        &key("wash"),
        "2026-10-04",
        json!({"type": "wash", "pieceId": piece, "water": "hot", "drying": "tumble_high"}),
    )
    .await;
    h.event(NS, &key("wear-3"), "2026-10-08", json!({"type": "wear", "pieceId": piece, "regionFit": [{"region": "chest", "verdict": "too_loose"}]})).await;
    let context = h.context(NS, json!({})).await;
    let kinds: Vec<&str> = context["contradictions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["body_measurement", "fit_report", "preference"]);
}

#[tokio::test(start_paused = true)]
async fn fixture_namespaces_never_reach_the_real_profile() {
    let h = Harness::new().await;
    let variant = id(&h
        .save(
            "fixture-a",
            &key("variant"),
            tee("Uniqlo", "Airism Tee", "M", "regular"),
        )
        .await);
    h.save(
        "fixture-a",
        &key("body"),
        body_measurement("chest", 99.0, "cm", "2026-10-01"),
    )
    .await;

    let real = h.context("real", json!({"category": "tops"})).await;
    assert_eq!(real["counts"]["records"]["variant"], json!(0));
    assert_eq!(real["counts"]["records"]["measurement"], json!(0));
    assert_eq!(real["snapshot"]["ledgerRevision"], json!(0));
    assert!(
        real["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["code"] == json!("no_fit_evidence") && w["severity"] == json!("blocking"))
    );

    let cross = h
        .reject("stylist_record_save", json!({"namespace": "fixture-b", "idempotencyKey": key("cross"), "record": {
            "kind": "piece", "label": "x", "category": "tops/tshirt", "variantId": variant, "provenance": stated(),
        }}))
        .await;
    assert!(cross.starts_with("[not_found]"), "{cross}");
    let same_key = h
        .save(
            "fixture-b",
            &key("variant"),
            tee("Uniqlo", "Airism Tee", "M", "regular"),
        )
        .await;
    assert_eq!(same_key["replayed"], json!(false));

    let bad = h
        .reject("stylist_context_get", json!({"namespace": "prod"}))
        .await;
    assert!(bad.contains("namespace"), "{bad}");
    let unknown_field = h
        .reject(
            "stylist_record_save",
            json!({"namespace": "fixture-a", "idempotencyKey": key("extra"),
            "record": tee("A", "B", "M", "c"), "sneaky": true}),
        )
        .await;
    assert!(unknown_field.contains("sneaky"), "{unknown_field}");
}
