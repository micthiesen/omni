//! Manual check against a copy of the production docstore (never the original):
//! `OMNI_PROD_COPY=/path/to/copy.db cargo test -p omni-tasks --test it prod_copy:: -- --ignored --nocapture`.
//! The file is copied into a temporary directory first. Every `task-run`,
//! `task-schedule-state` and `task-run-log` row must decode into its typed entity,
//! recompute its own primary key, and re-encode to the stored value (modulo
//! `undefined` object fields, which read like absent ones).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::sync::Arc;

use omni_core::clock::SystemClock;
use omni_store::cbor::{self, JsValue};
use omni_store::entity::{Entity, EntityDescriptor};
use omni_store::{DocOps, Store, StoreError, StoreOptions};
use omni_tasks::persistence::{TaskRunData, TaskRunLog, TaskScheduleState};

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

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a copy of the production docstore"]
async fn production_task_rows_decode_and_round_trip() {
    let source = std::env::var("OMNI_PROD_COPY").expect("set OMNI_PROD_COPY");
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("docstore.db");
    std::fs::copy(&source, &copy).unwrap();
    let store = Store::open(&copy, StoreOptions::new(Arc::new(SystemClock)))
        .await
        .unwrap();
    let reports = store
        .read(|docs| {
            Ok([
                ("task-run", check::<TaskRunData>(docs)?),
                ("task-schedule-state", check::<TaskScheduleState>(docs)?),
                ("task-run-log", check::<TaskRunLog>(docs)?),
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
            &report.decode_failures[..report.decode_failures.len().min(5)]
        );
        assert!(
            report.pk_mismatches.is_empty(),
            "{name}: {:?}",
            report.pk_mismatches
        );
        assert!(
            report.value_mismatches.is_empty(),
            "{name}: {:?}",
            &report.value_mismatches[..report.value_mismatches.len().min(5)]
        );
    }
}
