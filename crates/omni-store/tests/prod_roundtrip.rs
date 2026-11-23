//! Every row of a production docstore copy decodes, and its re-encoding (plain
//! and through serde) reads back as the same JS value.
//!
//! Ignored by default (needs a database). Run with
//! `OMNI_PROD_COPY=/path/to/docstore.db cargo test -p omni-store --test it prod_roundtrip:: -- --ignored --nocapture`.
//! The file is copied into a temporary directory first; the original is never
//! opened.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stdout)]

use std::collections::BTreeMap;
use std::sync::Arc;

use omni_core::clock::SystemClock;
use omni_store::cbor::{self, JsValue};
use omni_store::{DocOps, Store, StoreOptions};

fn tally(v: &JsValue, counts: &mut BTreeMap<&'static str, u64>) {
    let kind = match v {
        JsValue::Undefined => "undefined",
        JsValue::Null => "null",
        JsValue::Bool(_) => "bool",
        JsValue::Int(_) => "int",
        JsValue::Float(_) => "float",
        JsValue::String(_) => "string",
        JsValue::Bytes(_) => "bytes",
        JsValue::Array(_) => "array",
        JsValue::Object(_) => "object",
        JsValue::Map(_) => "map",
        JsValue::Date(_) => "date",
        JsValue::Set(_) => "set",
        JsValue::BigInt(_) => "bigint",
        JsValue::Simple(_) => "simple",
        JsValue::Tagged(_, _) => "tagged",
    };
    *counts.entry(kind).or_default() += 1;
    match v {
        JsValue::Array(items) | JsValue::Set(items) => items.iter().for_each(|i| tally(i, counts)),
        JsValue::Object(map) => map.values().for_each(|i| tally(i, counts)),
        JsValue::Map(entries) => entries.iter().for_each(|(k, i)| {
            tally(k, counts);
            tally(i, counts);
        }),
        JsValue::Tagged(_, inner) => tally(inner, counts),
        _ => {}
    }
}

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a production docstore copy"]
async fn every_production_row_decodes_and_round_trips() {
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
    let rows = store
        .read(|docs| docs.get_raw_rows_by_prefix(""))
        .await
        .expect("read rows");

    let mut decode_failures = Vec::new();
    let mut value_mismatches = Vec::new();
    let mut serde_mismatches = Vec::new();
    let mut kinds = BTreeMap::new();
    let mut by_entity: BTreeMap<String, u64> = BTreeMap::new();
    for row in &rows {
        *by_entity
            .entry(row.entity.clone().unwrap_or_else(|| "<null>".to_owned()))
            .or_default() += 1;
        let Some(data) = row.data.as_deref() else {
            decode_failures.push(format!("{}: NULL data", row.pk));
            continue;
        };
        let value = match cbor::decode(data) {
            Ok(value) => value,
            Err(e) => {
                decode_failures.push(format!("{}: {e}", row.pk));
                continue;
            }
        };
        tally(&value, &mut kinds);
        let reread = cbor::decode(&cbor::encode(&value)).expect("own encoding decodes");
        if !cbor::same_value(&reread, &value) {
            value_mismatches.push(row.pk.clone());
        }
        let via_serde: JsValue = cbor::from_value(value.clone()).expect("JsValue from_value");
        let back = cbor::to_value(&via_serde).expect("JsValue to_value");
        if !cbor::same_value(&back, &value) {
            serde_mismatches.push(row.pk.clone());
        }
    }

    println!("rows: {}", rows.len());
    println!("rows by entity: {by_entity:?}");
    println!("value kinds: {kinds:?}");
    println!(
        "decode failures ({}): {decode_failures:?}",
        decode_failures.len()
    );
    println!(
        "value mismatches ({}): {:?}",
        value_mismatches.len(),
        &value_mismatches[..value_mismatches.len().min(20)]
    );
    println!(
        "serde round-trip mismatches ({}): {serde_mismatches:?}",
        serde_mismatches.len()
    );
    assert!(decode_failures.is_empty());
    assert!(value_mismatches.is_empty());
    assert!(serde_mismatches.is_empty());
}
