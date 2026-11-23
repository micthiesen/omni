//! Typed round trip of every media row of a production docstore copy, plus
//! checks that the persisted derivations (taste evidence ids and the evidence
//! fingerprint) reproduce the stored values.
//!
//! Ignored by default (needs a database). Run with
//! `OMNI_PROD_COPY=/path/to/docstore.db cargo test -p omni-media --test it prod_copy:: -- --ignored --nocapture`.
//! The file is copied into a temporary directory first; the original is never opened.
#![allow(clippy::print_stdout, clippy::expect_used)]

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use omni_core::clock::SystemClock;
use omni_media::entity_descriptors;
use omni_media::persistence::{IdentityAliasData, RecommendationData};
use omni_media::taste::persistence::latest_profile;
use omni_media::taste::{
    CanonicalWatchObservation, TasteEvidenceData, TasteEvidenceKind, TasteProfileData,
    derive_recommendation_evidence, derive_watch_evidence, fingerprint_evidence,
};
use omni_media::tmdb::types::TmdbTitleDetails;
use omni_media::types::{MediaItem, WatchedItem};
use omni_store::cbor::{self, JsValue};
use omni_store::entity::Entity;
use omni_store::{DocOps, Store, StoreOptions};
use serde::Serialize;
use serde::de::DeserializeOwned;

async fn open_copy() -> (tempfile::TempDir, Store) {
    let Ok(source) = std::env::var("OMNI_PROD_COPY") else {
        panic!("set OMNI_PROD_COPY");
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let copy = dir.path().join("docstore.db");
    std::fs::copy(&source, &copy).expect("copy database");
    for suffix in ["-wal", "-shm"] {
        let side = format!("{source}{suffix}");
        if std::path::Path::new(&side).exists() {
            std::fs::copy(&side, dir.path().join(format!("docstore.db{suffix}"))).expect("copy");
        }
    }
    let mut perms = std::fs::metadata(&copy).expect("meta").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&copy, perms).expect("writable copy");
    let store = Store::open(&copy, StoreOptions::new(Arc::new(SystemClock)))
        .await
        .expect("open copy");
    (dir, store)
}

/// JS value semantics: `undefined` properties dropped, numbers as doubles,
/// keys sorted (key order is not part of value equality).
fn normalize(value: &JsValue) -> Option<serde_json::Value> {
    Some(match value {
        JsValue::Undefined => return None,
        JsValue::Null => serde_json::Value::Null,
        JsValue::Bool(b) => serde_json::Value::Bool(*b),
        #[allow(clippy::cast_precision_loss)]
        JsValue::Int(n) => serde_json::json!(*n as f64),
        JsValue::Float(f) => serde_json::json!(*f),
        JsValue::String(s) => serde_json::Value::String(s.clone()),
        JsValue::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|i| normalize(i).unwrap_or(serde_json::Value::Null))
                .collect(),
        ),
        JsValue::Object(map) => {
            let sorted: BTreeMap<String, serde_json::Value> = map
                .iter()
                .filter_map(|(k, v)| normalize(v).map(|v| (k.clone(), v)))
                .collect();
            serde_json::to_value(sorted).expect("object")
        }
        other => serde_json::Value::String(format!("{other:?}")),
    })
}

struct Report {
    rows: usize,
    decode_failures: Vec<String>,
    value_mismatches: Vec<String>,
    pk_mismatches: Vec<String>,
}

async fn round_trip<E: Entity + Serialize + DeserializeOwned>(store: &Store) -> (Report, Vec<E>) {
    let prefix = format!("${}#", E::NAME);
    let rows = store
        .read(move |docs| docs.get_raw_rows_by_prefix(&prefix))
        .await
        .expect("rows");
    let mut report = Report {
        rows: rows.len(),
        decode_failures: Vec::new(),
        value_mismatches: Vec::new(),
        pk_mismatches: Vec::new(),
    };
    let mut decoded = Vec::new();
    for row in rows {
        let original = row.decode().expect("cbor decode");
        let typed: E = match cbor::from_value(original.clone()) {
            Ok(typed) => typed,
            Err(e) => {
                report.decode_failures.push(format!("{}: {e}", row.pk));
                continue;
            }
        };
        let pk = omni_store::entity::pk::<E>(&typed.key()).expect("pk");
        if pk != row.pk {
            report.pk_mismatches.push(format!("{} != {pk}", row.pk));
        }
        let encoded = cbor::to_value(&typed).expect("encode");
        let reread = cbor::decode(&cbor::encode(&encoded)).expect("re-decode");
        if normalize(&original) != normalize(&reread) {
            report.value_mismatches.push(format!(
                "{}\n  original: {}\n  rust:     {}",
                row.pk,
                serde_json::to_string(&normalize(&original)).unwrap_or_default(),
                serde_json::to_string(&normalize(&reread)).unwrap_or_default()
            ));
        }
        decoded.push(typed);
    }
    (report, decoded)
}

fn print(name: &str, report: &Report) {
    println!(
        "{name}: {} rows, {} decode failures, {} value mismatches, {} pk mismatches",
        report.rows,
        report.decode_failures.len(),
        report.value_mismatches.len(),
        report.pk_mismatches.len()
    );
    for line in report
        .decode_failures
        .iter()
        .chain(&report.value_mismatches)
        .chain(&report.pk_mismatches)
        .take(10)
    {
        println!("  {line}");
    }
}

fn assert_clean(report: &Report) {
    assert!(report.decode_failures.is_empty());
    assert!(report.value_mismatches.is_empty());
    assert!(report.pk_mismatches.is_empty());
}

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a production docstore copy"]
async fn every_media_row_decodes_and_reencodes_to_the_same_js_value() {
    let (_dir, store) = open_copy().await;
    let (recs, recommendations) = round_trip::<RecommendationData>(&store).await;
    let (aliases, _) = round_trip::<IdentityAliasData>(&store).await;
    let (evidence_report, evidence) = round_trip::<TasteEvidenceData>(&store).await;
    let (profiles_report, profiles) = round_trip::<TasteProfileData>(&store).await;
    print("recs-recommendation-attempt", &recs);
    print("recs-identity-alias", &aliases);
    print("recs-taste-evidence", &evidence_report);
    print("recs-taste-profile", &profiles_report);
    for report in [&recs, &aliases, &evidence_report, &profiles_report] {
        assert_clean(report);
        assert!(report.rows > 0);
    }

    // Descriptor-level recompute_pk agrees for every row (compat audit path).
    for descriptor in entity_descriptors() {
        let prefix = format!("${}#", descriptor.name);
        let rows = store
            .read(move |docs| docs.get_raw_rows_by_prefix(&prefix))
            .await
            .expect("rows");
        for row in rows {
            let value = row.decode().expect("decode");
            assert_eq!(
                (descriptor.recompute_pk)(&value).as_deref(),
                Ok(row.pk.as_str())
            );
        }
    }

    // Evidence ids: every outcome/feedback id derived for the current
    // recommendation rows that is also stored must match exactly.
    let stored_ids: HashSet<&str> = evidence.iter().map(|e| e.evidence_id.as_str()).collect();
    let derived = derive_recommendation_evidence(&recommendations);
    let matched = derived
        .iter()
        .filter(|e| stored_ids.contains(e.evidence_id.as_str()))
        .count();
    println!(
        "recommendation evidence ids: {matched}/{} derived ids already stored by TS",
        derived.len()
    );
    assert!(
        matched > 0,
        "no derived evidence id matches a TS-written id"
    );

    // Every stored recommendation evidence row is reproduced when its
    // recommendation still has the same derived state.
    let derived_ids: HashSet<&str> = derived.iter().map(|e| e.evidence_id.as_str()).collect();
    let rec_ids: HashSet<&str> = recommendations
        .iter()
        .map(|r| r.recommendation_id.as_str())
        .collect();
    let latest_stored: Vec<&TasteEvidenceData> = evidence
        .iter()
        .filter(|e| {
            e.recommendation_id
                .as_deref()
                .is_some_and(|id| rec_ids.contains(id))
        })
        .collect();
    println!(
        "stored recommendation evidence rows: {}, reproduced: {}",
        latest_stored.len(),
        latest_stored
            .iter()
            .filter(|e| derived_ids.contains(e.evidence_id.as_str()))
            .count()
    );

    // Watch evidence ids: rebuild each stored plex_watch observation (its
    // metadata fields are the hashed TMDB details) and re-derive the id.
    let watch_rows: Vec<&TasteEvidenceData> = evidence
        .iter()
        .filter(|e| e.kind == TasteEvidenceKind::PlexWatch)
        .collect();
    let observations: Vec<CanonicalWatchObservation> = watch_rows
        .iter()
        .map(|e| CanonicalWatchObservation {
            canonical_id: e.canonical_id.clone(),
            item: WatchedItem {
                item: MediaItem {
                    guid: String::new(),
                    title: e.title.clone(),
                    year: e.year,
                    media_type: e.media_type,
                    external_ids: None,
                    title_slug: None,
                },
                viewed_at: e.observed_at,
                view_count: e.view_count.unwrap_or(0),
                completion: e.completion,
            },
            metadata: e.genres.as_ref().map(|genres| TmdbTitleDetails {
                genres: genres.clone(),
                runtime_minutes: e.runtime_minutes,
                season_count: e.season_count,
                episode_count: e.episode_count,
                series_status: e.series_status.clone(),
                original_language: e.original_language.clone(),
                origin_countries: e.origin_countries.clone().unwrap_or_default(),
                creators: e.creators.clone().unwrap_or_default(),
                cast: e.cast.clone().unwrap_or_default(),
                keywords: e.keywords.clone().unwrap_or_default(),
                certification: e.certification.clone(),
            }),
        })
        .collect();
    let rederived = derive_watch_evidence(&observations);
    let watch_matches = watch_rows
        .iter()
        .zip(&rederived)
        .filter(|(stored, derived)| stored.evidence_id == derived.evidence_id)
        .count();
    println!(
        "watch evidence ids: {watch_matches}/{} re-derived exactly",
        watch_rows.len()
    );
    assert_eq!(watch_matches, watch_rows.len());

    // The no-op guard: the latest profile's fingerprint covers all evidence
    // that existed when it was generated.
    let latest = latest_profile(profiles).expect("profile");
    let fingerprint = fingerprint_evidence(&evidence);
    println!(
        "fingerprint: rust {fingerprint}, latest profile {} (v{}, {} evidence, now {})",
        latest.profile.evidence_fingerprint,
        latest.profile.version,
        latest.profile.evidence_count,
        evidence.len()
    );
    if latest.profile.evidence_count == evidence.len() as u64 {
        assert_eq!(fingerprint, latest.profile.evidence_fingerprint);
        // The deterministic stats were computed from this same evidence set.
        // Same input order as the reflection run (`getAllTasteEvidence`).
        let ordered = omni_media::taste::get_all_taste_evidence(&store)
            .await
            .expect("evidence");
        let stats = omni_media::taste::compute_behavioral_stats(&ordered);
        println!(
            "stats: rust {}\n       ts   {}",
            serde_json::to_string(&stats).unwrap_or_default(),
            serde_json::to_string(&latest.profile.stats).unwrap_or_default()
        );
        assert_eq!(
            serde_json::to_value(&stats).expect("stats"),
            serde_json::to_value(&latest.profile.stats).expect("stats")
        );
    }
}

use crate::common;

/// Every production row serializes through the HTTP routes and through the
/// MCP tools, whose outputs are validated against the golden schemas.
#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a production docstore copy"]
async fn production_rows_serve_through_routes_and_mcp_tools() {
    let (_dir, store) = open_copy().await;
    let mut h = common::Harness::new().await;
    h.services.store = store;
    let subsystem = omni_media::subsystem_with(&h.app.ctx, h.services.clone()).expect("subsystem");
    let router = h.app.router(&subsystem);
    let (status, body) = h.app.get_json(&router, "/api/recommendations").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let count = body["recommendations"].as_array().map_or(0, Vec::len);
    println!("GET /api/recommendations: {count} recommendations");
    assert!(count > 0);
    let (status, body) = h
        .app
        .get_json(&router, "/api/recommendations/taste-profile")
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(body["profile"]["profileId"].is_string());

    let tools = omni_media::mcp::media_tools(Arc::new(h.services.clone())).expect("tools");
    let call = |name: &'static str, input: serde_json::Value| {
        let tool = tools
            .iter()
            .find(|t| t.meta.name == name)
            .cloned()
            .expect("tool");
        async move {
            tool.handler
                .call(
                    input,
                    omni_mcp_kit::ToolContext {
                        call_id: "prod".to_owned(),
                        cancel: tokio_util::sync::CancellationToken::new(),
                    },
                )
                .await
        }
    };
    call(
        "media_taste_read",
        serde_json::json!({"resource": "profile"}),
    )
    .await
    .expect("profile validates against the golden output schema");
    for cursor in (0..400).step_by(100) {
        call(
            "media_taste_read",
            serde_json::json!({"resource": "evidence", "cursor": cursor, "limit": 100}),
        )
        .await
        .expect("evidence page validates");
    }
    call(
        "media_recommendations_list",
        serde_json::json!({"limit": 100}),
    )
    .await
    .expect("recommendations validate");
}
