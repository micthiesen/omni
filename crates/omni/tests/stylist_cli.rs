//! The operator commands: export a namespace to a file, verify it, and restore
//! it into an empty namespace of another database.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use omni::stylist_cli;
use omni_core::clock::{SharedClock, SystemClock};
use omni_store::{Store, StoreOptions};
use omni_stylist::Stylist;
use serde_json::{Value, json};

fn args(list: &[String]) -> Vec<String> {
    list.to_vec()
}

async fn stylist(db: &std::path::Path, private: &std::path::Path) -> Stylist {
    let clock: SharedClock = Arc::new(SystemClock);
    let store = Store::open(db, StoreOptions::new(clock.clone()))
        .await
        .unwrap();
    Stylist::new(store, clock, jiff::tz::TimeZone::UTC, private.to_path_buf())
}

#[tokio::test]
async fn export_verify_and_restore_round_trip_through_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let source_db = dir.path().join("source.db");
    let private = dir.path().join("private");
    let source = stylist(&source_db, &private).await;
    let input: omni_stylist::api::SaveRecordInput = serde_json::from_value(json!({
        "namespace": "real",
        "idempotencyKey": "cli-test-variant-0001",
        "record": {"kind": "variant", "brand": "Asket", "model": "The T-Shirt", "category": "tops/tshirt",
                   "sizeLabel": "M", "sizeSystem": "alpha",
                   "provenance": {"certainty": "explicit", "source": "label"}},
    }))
    .unwrap();
    source.save_record(input).await.unwrap();
    let evidence: omni_stylist::api::StoreEvidenceInput = serde_json::from_value(json!({
        "namespace": "real",
        "idempotencyKey": "cli-test-photo-00001",
        "mimeType": "text/plain",
        "text": "care label: machine wash cold",
        "info": {"kind": "label", "certainty": "explicit"},
    }))
    .unwrap();
    source.store_evidence(evidence).await.unwrap();

    let archive = dir.path().join("exports").join("real.json");
    let s = |v: &str| v.to_owned();
    let db_flag = |db: &std::path::Path| vec![s("--db"), db.display().to_string()];
    let private_flag = vec![s("--private"), private.display().to_string()];

    let mut export = vec![
        s("--namespace"),
        s("real"),
        s("--out"),
        archive.display().to_string(),
    ];
    export.extend(db_flag(&source_db));
    export.extend(private_flag.clone());
    let report: Value = serde_json::from_str(
        &stylist_cli::run(&stylist_cli::parse("stylist-export", &args(&export)).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(report["manifest"]["counts"]["evidence"], json!(1));
    assert_eq!(report["manifest"]["includesBytes"], json!(true));

    let mut verify = vec![s("--archive"), archive.display().to_string()];
    verify.extend(private_flag.clone());
    let verified: Value = serde_json::from_str(
        &stylist_cli::run(&stylist_cli::parse("stylist-verify", &args(&verify)).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(verified["verified"], json!(true));
    assert_eq!(verified["evidenceVerified"], json!(1));

    let target_db = dir.path().join("target.db");
    let mut restore = vec![
        s("--archive"),
        archive.display().to_string(),
        s("--namespace"),
        s("real"),
    ];
    restore.extend(db_flag(&target_db));
    restore.extend(private_flag.clone());
    let refused =
        stylist_cli::run(&stylist_cli::parse("stylist-restore", &args(&restore)).unwrap()).await;
    assert!(refused.unwrap_err().to_string().contains("[namespace]"));
    restore.push(s("--allow-real"));
    let restored: Value = serde_json::from_str(
        &stylist_cli::run(&stylist_cli::parse("stylist-restore", &args(&restore)).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(restored["verified"], json!(true));
    assert_eq!(
        restored["contentSha256"],
        report["manifest"]["checksums"]["content"]
    );

    let target = stylist(&target_db, &private).await;
    let state = target
        .state(&omni_stylist::types::Namespace::real(), None)
        .await
        .unwrap();
    assert_eq!(state.records.len(), 1);
    assert_eq!(state.evidence.len(), 1);
}
