//! Typed decode and re-encode of every livestream intelligence row in a production docstore copy.
//!
//! Ignored by default. Run with
//! `OMNI_PROD_COPY=/path/to/docstore.db cargo test -p omni-live-intel --test it prod_copy:: -- --ignored --nocapture`.
//! The file is copied into a temporary directory first; the original is never opened.
//! For each entity: every row must decode into the typed model, recompute its
//! primary key, and re-encode to the same value (fields stored as JS
//! `undefined` compare as absent).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stdout)]

use std::sync::Arc;

use omni_api::intelligence as api;
use omni_core::clock::SystemClock;
use omni_live_intel::types::{
    LivestreamDiagnosticsData, LivestreamFeedbackData, LivestreamIntelligenceData,
    LivestreamIntelligenceEventData, StreamSessionsData,
};
use omni_store::cbor::{self, JsValue};
use omni_store::entity::{Entity, EntityDescriptor};
use omni_store::{DocOps, Store, StoreOptions};
use serde::Serialize;
use serde::de::DeserializeOwned;

fn without_undefined(value: &JsValue) -> JsValue {
    match value {
        JsValue::Object(map) => JsValue::Object(
            map.iter()
                .filter(|(_, v)| !matches!(v, JsValue::Undefined))
                .map(|(k, v)| (k.clone(), without_undefined(v)))
                .collect(),
        ),
        JsValue::Array(items) => JsValue::Array(items.iter().map(without_undefined).collect()),
        other => other.clone(),
    }
}

#[derive(Default, Debug)]
struct Report {
    rows: usize,
    decode_failures: Vec<String>,
    pk_mismatches: Vec<String>,
    value_mismatches: Vec<String>,
    dto_failures: Vec<String>,
    byte_identical: usize,
}

/// The entity's JSON (as the routes serve it) must decode into the omni-api DTO.
fn dto_check<E: Serialize, D: DeserializeOwned>(typed: &E) -> Result<(), String> {
    let json = serde_json::to_value(typed).map_err(|e| e.to_string())?;
    serde_json::from_value::<D>(json)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

type DtoCheck<E> = fn(&E) -> Result<(), String>;

fn check<E: Entity + Serialize + DeserializeOwned>(
    rows: &[omni_store::RawRow],
    dto: Option<DtoCheck<E>>,
) -> Report {
    let descriptor = EntityDescriptor::of::<E>();
    let mut report = Report::default();
    for row in rows.iter().filter(|r| r.entity.as_deref() == Some(E::NAME)) {
        report.rows += 1;
        let bytes = row.data.as_deref().expect("data");
        let original = cbor::decode(bytes).expect("cbor");
        let typed: E = match cbor::from_value(original.clone()) {
            Ok(typed) => typed,
            Err(e) => {
                report.decode_failures.push(format!("{}: {e}", row.pk));
                continue;
            }
        };
        if let Some(dto) = dto
            && let Err(e) = dto(&typed)
        {
            report.dto_failures.push(format!("{}: {e}", row.pk));
        }
        match (descriptor.recompute_pk)(&original) {
            Ok(pk) if pk == row.pk => {}
            other => report.pk_mismatches.push(format!("{}: {other:?}", row.pk)),
        }
        let encoded = cbor::encode(&cbor::to_value(&typed).expect("to_value"));
        if encoded == bytes {
            report.byte_identical += 1;
        }
        let reread = cbor::decode(&encoded).expect("re-decode");
        if without_undefined(&reread) != without_undefined(&original) {
            report.value_mismatches.push(row.pk.clone());
        }
    }
    report
}

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a production docstore copy"]
async fn every_wp05_row_decodes_and_round_trips() {
    let source = std::env::var("OMNI_PROD_COPY").expect("set OMNI_PROD_COPY");
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
    let rows = store
        .read(|docs| docs.get_raw_rows_by_prefix("$"))
        .await
        .expect("rows");

    let reports = [
        (
            "livestream-intelligence",
            check::<LivestreamIntelligenceData>(
                &rows,
                Some(dto_check::<_, api::LivestreamIntelligence>),
            ),
        ),
        (
            "livestream-feedback",
            check::<LivestreamFeedbackData>(&rows, Some(dto_check::<_, api::LivestreamFeedback>)),
        ),
        (
            "livestream-diagnostics",
            check::<LivestreamDiagnosticsData>(
                &rows,
                Some(dto_check::<_, api::LivestreamDiagnostics>),
            ),
        ),
        (
            "livestream-intelligence-event",
            check::<LivestreamIntelligenceEventData>(
                &rows,
                Some(dto_check::<_, api::LivestreamEvent>),
            ),
        ),
        (
            "streamer-sessions (read-only view)",
            check::<StreamSessionsData>(&rows, None),
        ),
    ];
    let mut failed = false;
    for (name, report) in &reports {
        println!(
            "{name}: rows={} decode_failures={} pk_mismatches={} value_mismatches={} dto_failures={} byte_identical={}",
            report.rows,
            report.decode_failures.len(),
            report.pk_mismatches.len(),
            report.value_mismatches.len(),
            report.dto_failures.len(),
            report.byte_identical
        );
        for line in report
            .decode_failures
            .iter()
            .chain(&report.pk_mismatches)
            .chain(&report.value_mismatches)
            .chain(&report.dto_failures)
            .take(10)
        {
            println!("  {line}");
        }
        failed |= !report.decode_failures.is_empty()
            || !report.pk_mismatches.is_empty()
            || !report.value_mismatches.is_empty()
            || !report.dto_failures.is_empty();
    }
    assert!(!failed, "livestream intelligence rows did not round-trip");
}
