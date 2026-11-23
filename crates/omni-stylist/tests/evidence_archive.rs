//! Private evidence, export and restore, recoverable fixture purge, and the
//! scheduled backup of the real namespace.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::{Harness, body_measurement, garment_measurement, id, jpeg, key, stated, tee};
use omni_stylist::api::ArchiveSource;
use omni_stylist::backup::BackupOutcome;
use omni_stylist::types::Namespace;
use serde_json::{Value, json};

const SRC: &str = "fixture-src";

async fn store_photo(
    h: &Harness,
    namespace: &str,
    label: &str,
    bytes: &[u8],
    links: Value,
) -> Value {
    h.call("stylist_evidence_store", json!({
        "namespace": namespace, "idempotencyKey": key(label), "mimeType": "image/jpeg",
        "dataBase64": STANDARD.encode(bytes),
        "info": {"kind": "garment_photo", "capturedOn": "2026-10-11", "certainty": "explicit", "links": links},
    }))
    .await
}

/// A namespace with records, revisions, a superseded measurement, events and two photos.
async fn populate(h: &Harness, namespace: &str) -> (String, String) {
    let variant = id(&h
        .save(
            namespace,
            &key("variant"),
            tee("Uniqlo", "Supima Tee", "M", "regular"),
        )
        .await);
    let piece = id(&h
        .save(
            namespace,
            &key("piece"),
            json!({"kind": "piece", "label": "supima", "category": "tops/tshirt",
            "variantId": variant, "provenance": stated()}),
        )
        .await);
    let photo = id(&store_photo(h, namespace, "photo", &jpeg(7), json!([piece])).await);
    h.call("stylist_evidence_store", json!({
        "namespace": namespace, "idempotencyKey": key("chart"), "mimeType": "text/markdown",
        "text": "| size | chest |\n| M | 53 cm |",
        "info": {"kind": "size_chart", "source": "retailer product page", "certainty": "explicit", "links": [variant]},
    }))
    .await;
    let wrong = id(&h
        .save(
            namespace,
            &key("m1"),
            garment_measurement(
                "piece",
                &piece,
                "chest",
                "flat_width",
                52.0,
                "cm",
                "washed",
                "2026-10-01",
            ),
        )
        .await);
    h.call("stylist_record_save", json!({
        "namespace": namespace, "idempotencyKey": key("m2"), "supersedes": wrong, "correctionReason": "re-measured",
        "record": garment_measurement("piece", &piece, "chest", "flat_width", 53.0, "cm", "washed", "2026-10-01"),
    }))
    .await;
    h.call("stylist_record_save", json!({
        "namespace": namespace, "idempotencyKey": key("piece-2"), "id": piece, "expectedRevision": 1,
        "record": {"kind": "piece", "label": "white supima tee", "category": "tops/tshirt", "variantId": variant,
                   "reference": {"categories": ["tops/tshirt"], "role": "baseline"},
                   "provenance": {"certainty": "explicit", "source": "user_statement", "evidenceIds": [photo]}},
    }))
    .await;
    h.event(
        namespace,
        &key("wash"),
        "2026-09-30",
        json!({"type": "wash", "pieceId": piece, "water": "cold", "drying": "line_dry"}),
    )
    .await;
    h.save(
        namespace,
        &key("body"),
        body_measurement("chest", 100.0, "cm", "2026-10-01"),
    )
    .await;
    (piece, photo)
}

async fn snapshot(h: &Harness, namespace: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for collection in ["records", "events", "evidence"] {
        let page = h
            .call(
                "stylist_records_query",
                json!({
                    "namespace": namespace, "collection": collection, "includeSuperseded": true,
                    "includeHistory": collection == "records", "order": "oldest", "limit": 100,
                }),
            )
            .await;
        out.push(page[collection].clone());
    }
    out
}

#[tokio::test(start_paused = true)]
async fn evidence_is_private_verified_and_deduplicated() {
    let h = Harness::new().await;
    let variant = id(&h
        .save(SRC, &key("variant"), tee("Cos", "Tee", "M", "regular"))
        .await);
    let stored = store_photo(&h, SRC, "photo", &jpeg(1), json!([variant])).await;
    let evidence_id = id(&stored);
    assert_eq!(stored["deduplicated"], json!(false));
    let digest = stored["evidence"]["sha256"].as_str().unwrap().to_owned();

    let again = store_photo(&h, SRC, "photo-again", &jpeg(1), json!([])).await;
    assert_eq!(again["deduplicated"], json!(true));
    assert_eq!(id(&again), evidence_id);

    let got = h
        .try_call(
            "stylist_evidence_get",
            json!({"namespace": SRC, "evidenceId": evidence_id}),
        )
        .await
        .unwrap();
    assert_eq!(got.structured["verified"], json!(true));
    let resource = &got.resources[0];
    assert_eq!(resource["mimeType"], json!("image/jpeg"));
    assert_eq!(
        resource["uri"],
        json!(format!("omni-stylist-evidence:{evidence_id}"))
    );
    let bytes = STANDARD.decode(resource["blob"].as_str().unwrap()).unwrap();
    assert_eq!(bytes, jpeg(1));

    let mismatch = h
        .reject("stylist_evidence_store", json!({
            "namespace": SRC, "idempotencyKey": key("png"), "mimeType": "image/png",
            "dataBase64": STANDARD.encode(jpeg(2)), "info": {"kind": "garment_photo", "certainty": "explicit"},
        }))
        .await;
    assert!(mismatch.starts_with("[unsupported_media]"), "{mismatch}");
    let big = vec![b'a'; 3_000_001];
    let too_large = h
        .reject("stylist_evidence_store", json!({
            "namespace": SRC, "idempotencyKey": key("big"), "mimeType": "text/plain",
            "dataBase64": STANDARD.encode(&big), "info": {"kind": "note", "certainty": "explicit"},
        }))
        .await;
    assert!(too_large.starts_with("[too_large]"), "{too_large}");

    let other = h
        .reject(
            "stylist_evidence_get",
            json!({"namespace": "fixture-other", "evidenceId": evidence_id}),
        )
        .await;
    assert!(other.starts_with("[not_found]"), "{other}");

    // A rejected upload leaves no file behind.
    let orphan = jpeg(9);
    let rejected = h
        .reject(
            "stylist_evidence_store",
            json!({
                "namespace": SRC, "idempotencyKey": key("orphan"), "mimeType": "image/jpeg",
                "dataBase64": STANDARD.encode(&orphan),
                "info": {"kind": "body_photo", "certainty": "explicit",
                         "links": ["pc_00000000000000000000000000000000"]},
            }),
        )
        .await;
    assert!(rejected.starts_with("[not_found]"), "{rejected}");
    let orphan_digest = omni_stylist::evidence::sha256_hex(&orphan);
    let orphan_path = h
        .private
        .join("evidence/sha256")
        .join(&orphan_digest[..2])
        .join(&orphan_digest);
    assert!(!orphan_path.exists());

    // Tampered bytes are detected on read.
    let path = h
        .private
        .join("evidence/sha256")
        .join(&digest[..2])
        .join(&digest);
    std::fs::write(&path, b"tampered").unwrap();
    let tampered = h
        .reject(
            "stylist_evidence_get",
            json!({"namespace": SRC, "evidenceId": evidence_id}),
        )
        .await;
    assert!(tampered.starts_with("[integrity]"), "{tampered}");
    let metadata_only = h
        .call(
            "stylist_evidence_get",
            json!({"namespace": SRC, "evidenceId": evidence_id, "includeBytes": false}),
        )
        .await;
    assert_eq!(metadata_only["verified"], json!(false));
}

#[tokio::test(start_paused = true)]
async fn export_and_restore_round_trip_bytes_and_relationships() {
    let h = Harness::new().await;
    let (_, photo) = populate(&h, SRC).await;
    let exported = h
        .try_call(
            "stylist_export",
            json!({"namespace": SRC, "returnDocument": true}),
        )
        .await
        .unwrap();
    let manifest = exported.structured["manifest"].clone();
    assert_eq!(exported.structured["verified"], json!(true));
    assert_eq!(manifest["includesBytes"], json!(true));
    assert_eq!(manifest["counts"]["evidence"], json!(2));
    assert!(manifest["counts"]["relationships"].as_u64().unwrap() >= 6);
    let document = STANDARD
        .decode(exported.resources[0]["blob"].as_str().unwrap())
        .unwrap();
    let parsed: Value = serde_json::from_slice(&document).unwrap();
    assert_eq!(parsed["format"], json!("omni-stylist-export"));
    assert_eq!(parsed["runbooks"][0]["version"], json!("fit-first-v1"));

    let archive_id = manifest["archiveId"].as_str().unwrap().to_owned();
    let restored = h
        .call(
            "stylist_restore",
            json!({"namespace": "fixture-copy", "archiveId": archive_id}),
        )
        .await;
    assert_eq!(restored["verified"], json!(true));
    assert_eq!(restored["contentSha256"], manifest["checksums"]["content"]);
    assert_eq!(restored["evidenceVerified"], json!(2));
    assert_eq!(snapshot(&h, SRC).await, snapshot(&h, "fixture-copy").await);
    assert_eq!(h.evidence_bytes(SRC, &photo).await, jpeg(7));
    assert_eq!(h.evidence_bytes("fixture-copy", &photo).await, jpeg(7));
    let source_context = h.context(SRC, json!({"category": "tops"})).await;
    let copy_context = h.context("fixture-copy", json!({"category": "tops"})).await;
    assert_eq!(source_context["references"], copy_context["references"]);
    assert_eq!(source_context["history"], copy_context["history"]);

    let not_empty = h
        .reject(
            "stylist_restore",
            json!({"namespace": "fixture-copy", "archiveId": archive_id}),
        )
        .await;
    assert!(not_empty.starts_with("[namespace]"), "{not_empty}");
    let real = h
        .reject(
            "stylist_restore",
            json!({"namespace": "real", "archiveId": archive_id}),
        )
        .await;
    assert!(real.starts_with("[namespace]"), "{real}");

    // A tampered archive restores nothing.
    let path = h
        .private
        .join("archives")
        .join(format!("{archive_id}.json"));
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replacen("white supima tee", "black supima tee", 1),
    )
    .unwrap();
    let tampered = h
        .reject(
            "stylist_restore",
            json!({"namespace": "fixture-tampered", "archiveId": archive_id}),
        )
        .await;
    assert!(tampered.starts_with("[integrity]"), "{tampered}");
    let empty = h
        .call(
            "stylist_records_query",
            json!({"namespace": "fixture-tampered"}),
        )
        .await;
    assert_eq!(empty["total"], json!(0));
}

#[tokio::test(start_paused = true)]
async fn purging_a_fixture_is_recoverable() {
    let h = Harness::new().await;
    let (_, photo) = populate(&h, SRC).await;
    let before = snapshot(&h, SRC).await;
    let purge = h
        .reject(
            "stylist_fixture_purge",
            json!({"namespace": SRC, "confirmNamespace": "fixture-other"}),
        )
        .await;
    assert!(purge.starts_with("[validation]"), "{purge}");
    let real = h
        .reject(
            "stylist_fixture_purge",
            json!({"namespace": "real", "confirmNamespace": "real"}),
        )
        .await;
    assert!(real.starts_with("[namespace]"), "{real}");

    let purged = h
        .call(
            "stylist_fixture_purge",
            json!({"namespace": SRC, "confirmNamespace": SRC}),
        )
        .await;
    assert_eq!(purged["archive"]["reason"], json!("pre_purge"));
    assert_eq!(purged["evidenceFilesRemoved"], json!(2));
    assert_eq!(
        h.call("stylist_records_query", json!({"namespace": SRC}))
            .await["total"],
        json!(0)
    );
    let gone = h
        .reject(
            "stylist_evidence_get",
            json!({"namespace": SRC, "evidenceId": photo}),
        )
        .await;
    assert!(gone.starts_with("[not_found]"), "{gone}");

    let listed = h
        .call("stylist_archives_list", json!({"namespace": SRC}))
        .await;
    assert_eq!(
        listed["archives"][0]["archiveId"],
        purged["archive"]["archiveId"]
    );
    h.call(
        "stylist_restore",
        json!({"namespace": SRC, "archiveId": purged["archive"]["archiveId"]}),
    )
    .await;
    assert_eq!(snapshot(&h, SRC).await, before);
    assert_eq!(h.evidence_bytes(SRC, &photo).await, jpeg(7));
}

#[tokio::test(start_paused = true)]
async fn the_backup_covers_real_and_restores_after_database_loss() {
    let h = Harness::new().await;
    assert_eq!(h.stylist.backup(true).await.unwrap(), BackupOutcome::Empty);
    populate(&h, "real").await;
    h.save(
        "fixture-noise",
        &key("noise"),
        tee("Noise", "Fixture Tee", "S", "slim"),
    )
    .await;
    let BackupOutcome::Written {
        archive_id,
        ledger_revision,
    } = h.stylist.backup(false).await.unwrap()
    else {
        panic!("expected a backup");
    };
    assert!(archive_id.starts_with("real-r"));
    let BackupOutcome::UpToDate {
        evidence_verified, ..
    } = h.stylist.backup(false).await.unwrap()
    else {
        panic!("expected up to date");
    };
    assert_eq!(evidence_verified, None);
    let BackupOutcome::UpToDate {
        evidence_verified, ..
    } = h.stylist.backup(true).await.unwrap()
    else {
        panic!("expected up to date");
    };
    assert_eq!(evidence_verified, Some(2));
    let listed = h
        .call("stylist_archives_list", json!({"namespace": "real"}))
        .await;
    assert_eq!(listed["archives"][0]["reason"], json!("backup"));
    assert_eq!(listed["archives"][0]["includesBytes"], json!(false));
    let before = snapshot(&h, "real").await;

    // A new, empty database with the same private directory. Before the
    // operator restores real, fixture work runs and purges, even a fixture
    // holding the same photo bytes: the backup still names those bytes, so
    // nothing the backup needs is deleted.
    let fresh_harness = Harness::with_private(h.private.clone()).await;
    store_photo(
        &fresh_harness,
        "fixture-dr",
        "same-photo",
        &jpeg(7),
        json!([]),
    )
    .await;
    let purged = fresh_harness
        .call(
            "stylist_fixture_purge",
            json!({"namespace": "fixture-dr", "confirmNamespace": "fixture-dr"}),
        )
        .await;
    assert_eq!(purged["evidenceFilesRemoved"], json!(0));
    fresh_harness
        .call(
            "stylist_fixture_purge",
            json!({"namespace": "fixture-never-used", "confirmNamespace": "fixture-never-used"}),
        )
        .await;
    let fresh = fresh_harness.stylist.clone();
    let path = h
        .private
        .join("archives")
        .join(format!("{archive_id}.json"));
    let wrong_way = fresh_harness
        .reject(
            "stylist_restore",
            json!({"namespace": "fixture-copy-of-real", "archiveId": archive_id}),
        )
        .await;
    assert!(wrong_way.starts_with("[namespace]"), "{wrong_way}");
    let refused = fresh
        .restore(ArchiveSource::Path(path.clone()), &Namespace::real(), false)
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "namespace");
    let restored = fresh
        .restore(ArchiveSource::Path(path), &Namespace::real(), true)
        .await
        .unwrap();
    assert!(restored.verified);
    assert_eq!(restored.ledger_revision, ledger_revision);
    let state = fresh.state(&Namespace::real(), None).await.unwrap();
    assert_eq!(state.current_revision, ledger_revision);
    assert_eq!(
        before[0].as_array().unwrap().len(),
        state.history.values().map(Vec::len).sum::<usize>()
    );

    // A corrupt evidence file fails the backup run.
    let digest = before[2][0]["sha256"].as_str().unwrap();
    std::fs::write(
        h.private
            .join("evidence/sha256")
            .join(&digest[..2])
            .join(digest),
        b"rot",
    )
    .unwrap();
    let failed = h.stylist.backup(true).await.unwrap_err();
    assert_eq!(failed.code(), "integrity");
}
