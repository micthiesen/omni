//! Manual check against a copy of the production docstore (never the original):
//! `OMNI_PROD_COPY=/path/to/copy.db cargo test -p omni-calendar --test it prod_copy:: -- --ignored --nocapture`.
//! The file is copied into a temporary directory first. Every
//! `calendar-created-event` row must decode into the typed entity, recompute its
//! own primary key, re-encode to the stored value (modulo `undefined` object
//! fields, which read like absent ones), and already carry the current
//! event hash key (so the boot reconcile is a no-op).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::sync::Arc;

use omni_calendar::persistence::{self, CreatedCalendarEvent, compute_event_hash};
use omni_core::clock::SystemClock;
use omni_store::cbor::{self, JsValue};
use omni_store::entity::{Entity, EntityDescriptor};
use omni_store::{DocOps, Store, StoreOptions};

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

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a copy of the production docstore"]
async fn production_calendar_rows_decode_and_round_trip() {
    let source = std::env::var("OMNI_PROD_COPY").expect("set OMNI_PROD_COPY");
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("docstore.db");
    std::fs::copy(&source, &copy).unwrap();
    let store = Store::open(&copy, StoreOptions::new(Arc::new(SystemClock)))
        .await
        .unwrap();
    let descriptor = EntityDescriptor::of::<CreatedCalendarEvent>();
    let (rows, failures, pk_mismatch, value_mismatch, byte_identical, stale_hash, cancelled) =
        store
            .read(move |docs| {
                let mut rows = 0;
                let mut failures = Vec::new();
                let mut pk_mismatch = Vec::new();
                let mut value_mismatch = Vec::new();
                let mut byte_identical = 0;
                let mut stale_hash = Vec::new();
                let mut cancelled = 0;
                let mut shapes = std::collections::BTreeMap::new();
                for row in
                    docs.get_raw_rows_by_prefix(&format!("${}#", CreatedCalendarEvent::NAME))?
                {
                    rows += 1;
                    let Some(bytes) = row.data.as_deref() else {
                        failures.push(format!("{}: NULL data", row.pk));
                        continue;
                    };
                    let value = match cbor::decode(bytes) {
                        Ok(value) => value,
                        Err(e) => {
                            failures.push(format!("{}: {e}", row.pk));
                            continue;
                        }
                    };
                    if let Some(object) = value.as_object() {
                        let shape = object
                            .iter()
                            .filter(|(_, v)| !matches!(v, JsValue::Undefined))
                            .map(|(k, _)| k.as_str())
                            .collect::<Vec<_>>()
                            .join(",");
                        *shapes.entry(shape).or_insert(0usize) += 1;
                    }
                    if (descriptor.recompute_pk)(&value).as_deref() != Ok(row.pk.as_str()) {
                        pk_mismatch.push(row.pk.clone());
                    }
                    let typed: CreatedCalendarEvent = match cbor::from_value(value.clone()) {
                        Ok(typed) => typed,
                        Err(e) => {
                            failures.push(format!("{}: {e}", row.pk));
                            continue;
                        }
                    };
                    if typed.is_cancelled() {
                        cancelled += 1;
                    }
                    if compute_event_hash(
                        &typed.title,
                        &typed.start_date,
                        typed.start_time.as_deref(),
                    ) != typed.event_hash
                    {
                        stale_hash.push(row.pk.clone());
                    }
                    let reencoded = cbor::encode(&cbor::to_value(&typed).unwrap());
                    if reencoded == bytes {
                        byte_identical += 1;
                    }
                    if strip_undefined(&cbor::decode(&reencoded).unwrap())
                        != strip_undefined(&value)
                    {
                        value_mismatch.push(row.pk.clone());
                    }
                }
                for (shape, count) in &shapes {
                    println!("{count:>4} {shape}");
                }
                Ok((
                    rows,
                    failures,
                    pk_mismatch,
                    value_mismatch,
                    byte_identical,
                    stale_hash,
                    cancelled,
                ))
            })
            .await
            .unwrap();
    println!(
        "calendar-created-event: rows {rows}, cancelled {cancelled}, decode failures {}, pk mismatches {}, value mismatches {}, byte-identical {byte_identical}, stale hashes {}",
        failures.len(),
        pk_mismatch.len(),
        value_mismatch.len(),
        stale_hash.len()
    );
    assert!(rows > 0);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(pk_mismatch.is_empty(), "{pk_mismatch:?}");
    assert!(value_mismatch.is_empty(), "{value_mismatch:?}");
    assert!(stale_hash.is_empty(), "{stale_hash:?}");

    // The boot reconcile (run on the temp copy) moves nothing, and the typed
    // collection read sees every row.
    assert_eq!(
        persistence::reconcile_event_hashes(&store).await.unwrap(),
        0
    );
    assert_eq!(
        persistence::get_tracked_events(&store).await.unwrap().len(),
        rows
    );
}
