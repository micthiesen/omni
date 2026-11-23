//! Guards that keep history honest: forecasts cannot be scored against what
//! they could see, frozen inputs cannot be reinterpreted, lifecycle status
//! follows dates, computed contradictions can be resolved, archives that lost
//! rows fail, and the real namespace is append-only even to raw SQL.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{Harness, body_measurement, id, key, stated, tee};
use omni_stylist::api::ArchiveSource;
use omni_stylist::types::Namespace;
use serde_json::{Value, json};

const NS: &str = "fixture-guards";

async fn variant_and_piece(h: &Harness, label: &str) -> (String, String) {
    let slug = label.replace(' ', "-");
    let variant = id(&h
        .save(
            NS,
            &key(&format!("{slug}-variant")),
            tee("Arket", label, "M", "regular"),
        )
        .await);
    let piece = id(&h
        .save(
            NS,
            &key(&format!("{slug}-piece")),
            json!({"kind": "piece", "label": label, "category": "tops/tshirt",
                   "variantId": variant, "provenance": stated()}),
        )
        .await);
    (variant, piece)
}

/// A context's revision and token.
type Seen = (u64, String);

fn forecast(variant: &str, at: &Seen) -> Value {
    json!({
        "kind": "prediction", "variantId": variant, "recommendedSize": "M",
        "decision": "recommend", "basis": "new_brand_inference",
        "regionExpectations": [{"region": "chest", "expected": "good", "rationale": "history"}],
        "likelyFailureMode": {"description": "length"},
        "returnRisk": {"level": "low", "rationale": "free returns"},
        "contextRevision": at.0,
        "contextToken": at.1,
    })
}

async fn seen(h: &Harness) -> Seen {
    let context = h.context(NS, json!({})).await;
    (
        context["snapshot"]["ledgerRevision"].as_u64().unwrap(),
        context["snapshot"]["contextToken"]
            .as_str()
            .unwrap()
            .to_owned(),
    )
}

#[tokio::test(start_paused = true)]
async fn forecasts_cannot_be_scored_against_what_they_could_see() {
    let h = Harness::new().await;
    let (variant, piece) = variant_and_piece(&h, "Seen Tee").await;
    // Worn and judged before any forecast exists.
    let wear = id(&h
        .event(
            NS,
            &key("wear"),
            "2026-10-11",
            json!({"type": "wear", "pieceId": piece,
            "regionFit": [{"region": "chest", "verdict": "good"}]}),
        )
        .await);
    let at = seen(&h).await;
    let prediction = id(&h.save(NS, &key("forecast"), forecast(&variant, &at)).await);
    let outcome = h
        .reject(
            "stylist_event_append",
            json!({"namespace": NS, "idempotencyKey": key("outcome"), "occurredOn": "2026-10-12",
                   "event": {"type": "outcome", "predictionId": prediction, "pieceId": piece,
                             "stage": "immediate", "result": "matched"}}),
        )
        .await;
    assert!(outcome.starts_with("[post_hoc]"), "{outcome}");

    // Correcting the earlier report after the freeze does not hide it.
    h.call(
        "stylist_event_append",
        json!({"namespace": NS, "idempotencyKey": key("wear-fix"), "occurredOn": "2026-10-11",
               "supersedesEventId": wear, "correctionReason": "typo",
               "event": {"type": "wear", "pieceId": piece,
                         "regionFit": [{"region": "chest", "verdict": "slightly_tight"}]}}),
    )
    .await;
    let try_on = h
        .reject(
            "stylist_event_append",
            json!({"namespace": NS, "idempotencyKey": key("try-on"), "occurredOn": "2026-10-12",
                   "event": {"type": "try_on", "pieceId": piece, "predictionId": prediction,
                             "overall": "fits", "regionFit": [{"region": "chest", "verdict": "good"}]}}),
        )
        .await;
    assert!(try_on.starts_with("[post_hoc]"), "{try_on}");

    // An event dated before the freeze, and a piece of another variant.
    let (other_variant, other_piece) = variant_and_piece(&h, "Other Tee").await;
    let at = seen(&h).await;
    let fresh = id(&h
        .save(NS, &key("forecast-2"), forecast(&other_variant, &at))
        .await);
    let backdated = h
        .reject(
            "stylist_event_append",
            json!({"namespace": NS, "idempotencyKey": key("backdated"), "occurredOn": "2026-10-01",
                   "event": {"type": "feedback", "targetId": fresh, "timing": "immediate",
                             "regionFit": [{"region": "chest", "verdict": "good"}]}}),
        )
        .await;
    assert!(backdated.starts_with("[post_hoc]"), "{backdated}");
    let mismatch = h
        .reject(
            "stylist_event_append",
            json!({"namespace": NS, "idempotencyKey": key("mismatch"), "occurredOn": "2026-10-12",
                   "event": {"type": "try_on", "pieceId": piece, "predictionId": fresh,
                             "overall": "fits", "regionFit": [{"region": "chest", "verdict": "good"}]}}),
        )
        .await;
    assert!(mismatch.contains("not of variant"), "{mismatch}");

    // Once an outcome exists the forecast cannot be replaced.
    h.event(
        NS,
        &key("fresh-try"),
        "2026-10-12",
        json!({"type": "try_on", "pieceId": other_piece, "predictionId": fresh, "overall": "fits",
               "regionFit": [{"region": "chest", "verdict": "good"}]}),
    )
    .await;
    let replace = h
        .reject(
            "stylist_record_save",
            json!({"namespace": NS, "idempotencyKey": key("replace"), "supersedes": fresh,
                   "correctionReason": "better forecast", "record": forecast(&other_variant, &at)}),
        )
        .await;
    assert!(replace.starts_with("[post_hoc]"), "{replace}");
}

#[tokio::test(start_paused = true)]
async fn frozen_inputs_cannot_be_reinterpreted() {
    let h = Harness::new().await;
    let (variant, piece) = variant_and_piece(&h, "Frozen Tee").await;
    let at = seen(&h).await;
    let saved = h.save(NS, &key("forecast"), forecast(&variant, &at)).await;
    let derived = &saved["record"]["derived"];
    assert_eq!(derived["variantRevision"], json!(1));
    assert_eq!(derived["variantSnapshot"]["sizeLabel"], json!("M"));

    let mut resized = tee("Arket", "Frozen Tee", "L", "regular");
    let relabel = h
        .reject(
            "stylist_record_save",
            json!({"namespace": NS, "idempotencyKey": key("resize"), "id": variant,
                   "expectedRevision": 1, "record": resized.clone()}),
        )
        .await;
    assert!(relabel.starts_with("[immutable]"), "{relabel}");
    resized["sizeLabel"] = json!("M");
    resized["notes"] = json!("care label says cold wash");
    let noted = h
        .call(
            "stylist_record_save",
            json!({"namespace": NS, "idempotencyKey": key("note"), "id": variant,
                   "expectedRevision": 1, "record": resized}),
        )
        .await;
    assert_eq!(noted["record"]["revision"], json!(2));

    h.event(
        NS,
        &key("wear"),
        "2026-10-12",
        json!({"type": "wear", "pieceId": piece}),
    )
    .await;
    let other = id(&h
        .save(NS, &key("other"), tee("Arket", "Other", "M", "regular"))
        .await);
    let repoint = h
        .reject(
            "stylist_record_save",
            json!({"namespace": NS, "idempotencyKey": key("repoint"), "id": piece, "expectedRevision": 1,
                   "record": {"kind": "piece", "label": "x", "category": "tops/tshirt",
                              "variantId": other, "provenance": stated()}}),
        )
        .await;
    assert!(repoint.starts_with("[immutable]"), "{repoint}");
}

#[tokio::test(start_paused = true)]
async fn status_follows_dates_and_creation_status_is_kept() {
    let h = Harness::new().await;
    let variant = id(&h
        .save(
            NS,
            &key("variant"),
            tee("Cos", "Backfill Tee", "M", "regular"),
        )
        .await);
    let piece = id(&h
        .save(
            NS,
            &key("piece"),
            json!({"kind": "piece", "label": "old tee", "category": "tops/tshirt",
            "variantId": variant, "initialStatus": "undecided", "provenance": stated()}),
        )
        .await);
    // History backfilled out of order: the return before the decision that led to it.
    h.event(
        NS,
        &key("returned"),
        "2025-05-08",
        json!({"type": "returned", "pieceId": piece}),
    )
    .await;
    h.event(
        NS,
        &key("decision"),
        "2025-05-05",
        json!({"type": "decision", "pieceId": piece, "decision": "return",
        "reasons": [{"aspect": "fit", "region": "shoulders"}]}),
    )
    .await;
    let revised = h
        .call(
            "stylist_record_save",
            json!({"namespace": NS, "idempotencyKey": key("rename"), "id": piece, "expectedRevision": 1,
                   "record": {"kind": "piece", "label": "old tee (returned)", "category": "tops/tshirt",
                              "variantId": variant, "provenance": stated()}}),
        )
        .await;
    assert_eq!(revised["record"]["status"], json!("returned"));
    assert_eq!(
        revised["record"]["data"]["initialStatus"],
        json!("undecided")
    );
    let context = h.context(NS, json!({})).await;
    assert!(
        context["due"].as_array().unwrap().is_empty(),
        "{:#}",
        context["due"]
    );
}

#[tokio::test(start_paused = true)]
async fn computed_contradictions_can_be_resolved() {
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
    assert_eq!(
        h.context(NS, json!({})).await["contradictions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    h.event(
        NS,
        &key("resolve"),
        "2026-10-06",
        json!({"type": "contradiction", "status": "resolved",
        "ids": [a, b], "description": "the second was taken over a sweater"}),
    )
    .await;
    assert_eq!(h.context(NS, json!({})).await["contradictions"], json!([]));
}

#[tokio::test(start_paused = true)]
async fn an_archive_missing_rows_fails_even_with_recomputed_checksums() {
    let h = Harness::new().await;
    variant_and_piece(&h, "Archive Tee").await;
    h.save(
        NS,
        &key("chest"),
        body_measurement("chest", 100.0, "cm", "2026-10-01"),
    )
    .await;
    let exported = h.call("stylist_export", json!({"namespace": NS})).await;
    let archive_id = exported["manifest"]["archiveId"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = h
        .private
        .join("archives")
        .join(format!("{archive_id}.json"));
    let mut doc: omni_stylist::archive::ArchiveDoc =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc.records.pop();
    doc.checksums = omni_stylist::archive::checksums(&doc).unwrap();
    let edited = h.private.join("edited.json");
    std::fs::write(&edited, serde_json::to_vec(&doc).unwrap()).unwrap();
    let error = h
        .stylist
        .restore(
            ArchiveSource::Path(edited),
            &Namespace::parse("fixture-edited").unwrap(),
            false,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "integrity");
    // The stored copy is pinned by its manifest's file digest.
    std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
    let pinned = h
        .reject(
            "stylist_restore",
            json!({"namespace": "fixture-edited", "archiveId": archive_id}),
        )
        .await;
    assert!(pinned.starts_with("[integrity]"), "{pinned}");
}

#[tokio::test(start_paused = true)]
async fn the_real_ledger_is_append_only_even_to_sql() {
    let h = Harness::new().await;
    h.save(
        "real",
        &key("variant"),
        tee("Asket", "The T-Shirt", "M", "regular"),
    )
    .await;
    h.save(
        NS,
        &key("variant"),
        tee("Asket", "The T-Shirt", "M", "regular"),
    )
    .await;
    let store = h.stylist.store().clone();
    let delete_real = store
        .write(|tx| {
            tx.connection()
                .execute("DELETE FROM stylist_records WHERE namespace = 'real'", [])
                .map_err(|e| omni_store::StoreError::Sqlite(e.to_string()))
        })
        .await;
    assert!(
        delete_real
            .unwrap_err()
            .to_string()
            .contains("never deleted")
    );
    let update = store
        .write(|tx| {
            tx.connection()
                .execute("UPDATE stylist_records SET payload = '{}'", [])
                .map_err(|e| omni_store::StoreError::Sqlite(e.to_string()))
        })
        .await;
    assert!(update.unwrap_err().to_string().contains("append-only"));
    let purged = h
        .call(
            "stylist_fixture_purge",
            json!({"namespace": NS, "confirmNamespace": NS}),
        )
        .await;
    assert_eq!(purged["rowsRemoved"]["stylist_records"], json!(1));
    let real = h
        .call("stylist_records_query", json!({"namespace": "real"}))
        .await;
    assert_eq!(real["total"], json!(1));
}
