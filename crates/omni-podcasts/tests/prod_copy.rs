//! Manual check against a copy of the production docstore (never the original):
//! `OMNI_PROD_COPY=/path/to/copy.db cargo test -p omni-podcasts --test prod_copy -- --ignored --nocapture`.
//!
//! The file is copied into a temporary directory first. Every row of the four
//! podcast entities must decode into its typed entity, recompute its own primary
//! key, and re-encode to the stored value (modulo `undefined` object
//! fields, which read like absent ones). The evidence fingerprint and the
//! MCP/REST serializers are exercised on the real rows too.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::collections::HashSet;
use std::sync::Arc;

use omni_core::clock::{SharedClock, SystemClock};
use omni_mcp_kit::{ToolContext, ToolOutput};
use omni_podcasts::account::FixedAccount;
use omni_podcasts::mcp::{McpState, tools};
use omni_podcasts::persistence::{PodcastRecommendationData, PodcastRunState};
use omni_podcasts::reflection::store::get_latest_podcast_taste_profile;
use omni_podcasts::reflection::{
    PodcastTasteEvidenceData, PodcastTasteProfileData, derive_recommendation_evidence,
    fingerprint_evidence,
};
use omni_podcasts::routes::{serialize_profile, serialize_recommendation};
use omni_store::cbor::{self, JsValue};
use omni_store::entity::{Entity, EntityDescriptor, EntityOps as _};
use omni_store::{DocOps, Store, StoreError, StoreOptions};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn strip_undefined(value: &JsValue) -> JsValue {
    match value {
        JsValue::Object(map) => JsValue::Object(
            map.iter()
                .filter(|(_, v)| !matches!(v, JsValue::Undefined))
                .map(|(k, v)| (k.clone(), strip_undefined(v)))
                .collect(),
        ),
        JsValue::Array(items) => JsValue::Array(items.iter().map(strip_undefined).collect()),
        other => other.clone(),
    }
}

#[derive(Debug, Default)]
struct Report {
    rows: usize,
    decode_failures: Vec<String>,
    pk_mismatches: Vec<String>,
    value_mismatches: Vec<String>,
    byte_identical: usize,
}

fn check<E: Entity>(docs: &omni_store::Docs<'_>) -> Result<Report, StoreError> {
    let descriptor = EntityDescriptor::of::<E>();
    let mut report = Report::default();
    for row in docs.get_raw_rows_by_prefix(&format!("${}#", E::NAME))? {
        report.rows += 1;
        let Some(bytes) = row.data.as_deref() else {
            report
                .decode_failures
                .push(format!("{}: NULL data", row.pk));
            continue;
        };
        let value = match cbor::decode(bytes) {
            Ok(value) => value,
            Err(e) => {
                report.decode_failures.push(format!("{}: {e}", row.pk));
                continue;
            }
        };
        if (descriptor.recompute_pk)(&value).as_deref() != Ok(row.pk.as_str()) {
            report.pk_mismatches.push(row.pk.clone());
        }
        let typed: E = match cbor::from_value(value.clone()) {
            Ok(typed) => typed,
            Err(e) => {
                report.decode_failures.push(format!("{}: {e}", row.pk));
                continue;
            }
        };
        let reencoded = cbor::to_value(&typed)
            .map(|v| cbor::encode(&v))
            .map_err(|e| StoreError::Sqlite(e.to_string()))?;
        if reencoded == bytes {
            report.byte_identical += 1;
        }
        let reread = cbor::decode(&reencoded).map_err(|e| StoreError::Sqlite(e.to_string()))?;
        if strip_undefined(&reread) != strip_undefined(&value) {
            report.value_mismatches.push(row.pk.clone());
        }
    }
    Ok(report)
}

async fn open_copy() -> (tempfile::TempDir, Store) {
    let source = std::env::var("OMNI_PROD_COPY").expect("set OMNI_PROD_COPY");
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("docstore.db");
    std::fs::copy(&source, &copy).unwrap();
    let store = Store::open(&copy, StoreOptions::new(Arc::new(SystemClock)))
        .await
        .unwrap();
    (dir, store)
}

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a copy of the production docstore"]
async fn production_podcast_rows_decode_and_round_trip() {
    let (_dir, store) = open_copy().await;
    let reports = store
        .read(|docs| {
            Ok([
                (
                    "podcast-recommendation-attempt",
                    check::<PodcastRecommendationData>(docs)?,
                ),
                ("podcast-run-state", check::<PodcastRunState>(docs)?),
                (
                    "podcast-taste-evidence",
                    check::<PodcastTasteEvidenceData>(docs)?,
                ),
                (
                    "podcast-taste-profile",
                    check::<PodcastTasteProfileData>(docs)?,
                ),
            ])
        })
        .await
        .unwrap();
    for (name, report) in &reports {
        println!(
            "{name}: rows {}, decode failures {}, pk mismatches {}, value mismatches {}, byte-identical {}",
            report.rows,
            report.decode_failures.len(),
            report.pk_mismatches.len(),
            report.value_mismatches.len(),
            report.byte_identical
        );
        assert!(report.rows > 0, "{name} has rows");
        assert!(
            report.decode_failures.is_empty(),
            "{name}: {:?}",
            report.decode_failures
        );
        assert!(
            report.pk_mismatches.is_empty(),
            "{name}: {:?}",
            report.pk_mismatches
        );
        assert!(
            report.value_mismatches.is_empty(),
            "{name}: {:?}",
            report.value_mismatches
        );
    }
}

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a copy of the production docstore"]
async fn production_evidence_ids_fingerprint_and_serializers_round_trip() {
    let (_dir, store) = open_copy().await;
    let evidence = store
        .read(|docs| docs.get_all::<PodcastTasteEvidenceData>())
        .await
        .unwrap();
    let recommendations = store
        .read(|docs| docs.get_all::<PodcastRecommendationData>())
        .await
        .unwrap();
    let latest = get_latest_podcast_taste_profile(&store)
        .await
        .unwrap()
        .unwrap();

    // The newest checkpoint's fingerprint covers the evidence present when it
    // was written; recompute over exactly those rows.
    let fingerprint = fingerprint_evidence(&evidence).unwrap();
    println!(
        "evidence {} rows, fingerprint {fingerprint}, latest profile v{} fingerprint {} over {} rows",
        evidence.len(),
        latest.version,
        latest.evidence_fingerprint,
        latest.evidence_count
    );
    if evidence.len() as u64 == latest.evidence_count {
        assert_eq!(fingerprint, latest.evidence_fingerprint);
    }

    // Re-deriving recommendation evidence reproduces stored ids for rows
    // whose state has not changed since the last reflection.
    let stored: HashSet<&str> = evidence.iter().map(|e| e.evidence_id.as_str()).collect();
    let derived = derive_recommendation_evidence(&recommendations);
    let matched = derived
        .iter()
        .filter(|e| stored.contains(e.evidence_id.as_str()))
        .count();
    println!(
        "derived recommendation evidence: {matched}/{} ids already stored",
        derived.len()
    );
    assert!(matched > 0);
    // Every id not stored yet comes from an observation newer than the last
    // reflection (the weekly run has not seen it), never from an id drift.
    let unexplained: Vec<&str> = derived
        .iter()
        .filter(|e| {
            !stored.contains(e.evidence_id.as_str()) && e.observed_at <= latest.generated_at
        })
        .map(|e| e.evidence_id.as_str())
        .collect();
    assert!(unexplained.is_empty(), "{unexplained:?}");

    for rec in &recommendations {
        serde_json::to_value(serialize_recommendation(rec)).unwrap();
    }
    serde_json::to_value(serialize_profile(&latest)).unwrap();

    // MCP outputs validate against the golden output schemas on real rows.
    let clock: SharedClock = Arc::new(SystemClock);
    let tools = tools(McpState {
        store: store.clone(),
        clock,
        accounts: Arc::new(FixedAccount(None)),
        directory: Arc::new(omni_podcasts::sources::OfflineDirectory),
        public_url: "https://recs.example".to_owned(),
    })
    .unwrap();
    let cx = || ToolContext {
        call_id: "prod".into(),
        cancel: CancellationToken::new(),
    };
    for (name, input) in [
        ("podcast_recommendations_list", json!({ "limit": 100 })),
        ("podcast_taste_read", json!({ "resource": "profile" })),
        (
            "podcast_taste_read",
            json!({ "resource": "evidence", "limit": 100 }),
        ),
    ] {
        let tool = tools.iter().find(|t| t.meta.name == name).unwrap();
        let output = tool.handler.call(input, cx()).await.unwrap();
        assert!(matches!(output, ToolOutput::Structured(_)), "{name}");
    }
}
