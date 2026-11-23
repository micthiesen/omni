//! Every `arr-recovery-state` and `observer-repair-state` row of a production
//! copy decodes through the typed models, recomputes its primary key, and
//! re-encodes to the same value (explicit `undefined` properties, which read
//! like absent ones, are the only allowed difference).
//!
//! Ignored by default. Run with
//! `OMNI_PROD_COPY=/path/to/docstore.db cargo test -p omni-arr --test it prod_copy:: -- --ignored --nocapture`.
//! The database is copied into a temporary directory; the original is never opened.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::print_stdout)]

use std::sync::Arc;

use omni_core::clock::SystemClock;
use omni_store::cbor::{self, JsValue};
use omni_store::entity::EntityDescriptor;
use omni_store::{DocOps as _, Store, StoreOptions};

fn without_undefined(value: JsValue) -> JsValue {
    match value {
        JsValue::Object(map) => JsValue::Object(
            map.into_iter()
                .filter(|(_, v)| *v != JsValue::Undefined)
                .map(|(k, v)| (k, without_undefined(v)))
                .collect(),
        ),
        JsValue::Array(items) => JsValue::Array(items.into_iter().map(without_undefined).collect()),
        other => other,
    }
}

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a production docstore copy"]
async fn every_production_row_decodes_and_reencodes_to_the_same_js_value() {
    let source = std::env::var("OMNI_PROD_COPY").expect("set OMNI_PROD_COPY");
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("docstore.db");
    std::fs::copy(&source, &copy).unwrap();
    let store = Store::open(&copy, StoreOptions::new(Arc::new(SystemClock)))
        .await
        .unwrap();

    let descriptors: Vec<EntityDescriptor> = omni_arr::entities();
    for descriptor in descriptors {
        let name = descriptor.name;
        let rows = store
            .read(move |docs| docs.get_raw_rows_by_prefix(&format!("${name}#")))
            .await
            .unwrap();
        let mut checked = 0;
        for row in &rows {
            assert_eq!(row.entity.as_deref(), Some(name), "{}", row.pk);
            let original = row.decode().unwrap();
            assert_eq!((descriptor.recompute_pk)(&original).unwrap(), row.pk);
            let reencoded = match name {
                "arr-recovery-state" => {
                    let typed: omni_arr::arr_recovery::persistence::RecoveryState =
                        cbor::from_value(original.clone()).unwrap();
                    cbor::to_value(&typed).unwrap()
                }
                _ => {
                    let typed: omni_arr::observer_repair::persistence::ObserverRepairState =
                        cbor::from_value(original.clone()).unwrap();
                    cbor::to_value(&typed).unwrap()
                }
            };
            let bytes = cbor::encode(&reencoded);
            let roundtrip = cbor::decode(&bytes).unwrap();
            assert_eq!(
                without_undefined(roundtrip),
                without_undefined(original.clone()),
                "{}",
                row.pk
            );
            let identical = row.data.as_deref() == Some(bytes.as_slice());
            println!(
                "{}: decoded, value-equal, byte-identical={identical}",
                row.pk
            );
            checked += 1;
        }
        println!(
            "{name}: {checked}/{} rows decoded and re-encoded",
            rows.len()
        );
    }
}
