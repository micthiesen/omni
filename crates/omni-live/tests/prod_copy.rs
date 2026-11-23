//! Production-copy round trip (ignored; run with `OMNI_PROD_COPY=<copy of
//! docstore.db> cargo test -p omni-live --test it prod_copy:: -- --ignored --nocapture`).
//! Every livestream row must decode into its typed entity, recompute its primary
//! key, and re-encode to the same value (explicit `undefined` members
//! removed, which read identically).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use omni_live::identity::ProfileIdentityLink;
use omni_live::metrics::{PlatformViewerMetrics, ViewerMetrics};
use omni_live::sessions::StreamSessions;
use omni_live::status::StreamerStatus;
use omni_store::cbor::{self, JsValue};
use omni_store::entity::{Entity, pk};
use omni_store::{DocOps, Store, StoreOptions};

fn strip_undefined(value: JsValue) -> JsValue {
    match value {
        JsValue::Object(fields) => JsValue::Object(
            fields
                .into_iter()
                .filter(|(_, v)| !matches!(v, JsValue::Undefined))
                .map(|(k, v)| (k, strip_undefined(v)))
                .collect(),
        ),
        JsValue::Array(items) => JsValue::Array(items.into_iter().map(strip_undefined).collect()),
        other => other,
    }
}

async fn check<E: Entity>(store: &Store) -> (usize, usize) {
    let rows = store
        .read(|docs| docs.get_raw_rows_by_prefix(&format!("${}#", E::NAME)))
        .await
        .unwrap();
    let mut identical = 0;
    for row in &rows {
        let original = row.decode().unwrap_or_else(|e| panic!("{}: {e}", row.pk));
        let typed: E =
            cbor::from_value(original.clone()).unwrap_or_else(|e| panic!("{}: {e}", row.pk));
        assert_eq!(pk::<E>(&typed.key()).unwrap(), row.pk, "primary key");
        let back = cbor::to_value(&typed).unwrap();
        assert_eq!(
            strip_undefined(back.clone()),
            strip_undefined(original),
            "{} value",
            row.pk
        );
        if row.data.as_deref() == Some(cbor::encode(&back).as_slice()) {
            identical += 1;
        }
    }
    println!(
        "{}: {} rows decoded and round-tripped, {identical} byte-identical",
        E::NAME,
        rows.len()
    );
    (rows.len(), identical)
}

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY"]
async fn prod_copy_round_trips_every_wp04_row() {
    let source = std::env::var("OMNI_PROD_COPY").expect("OMNI_PROD_COPY");
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("docstore.db");
    std::fs::copy(&source, &copy).unwrap();
    for suffix in ["-wal", "-shm"] {
        let side = format!("{source}{suffix}");
        if std::path::Path::new(&side).exists() {
            std::fs::copy(&side, dir.path().join(format!("docstore.db{suffix}"))).unwrap();
        }
    }
    let clock = omni_testkit::test_clock(omni_testkit::TEST_EPOCH_MS);
    let store = Store::open(&copy, StoreOptions::new(clock)).await.unwrap();
    let mut total = 0;
    total += check::<StreamerStatus>(&store).await.0;
    total += check::<StreamSessions>(&store).await.0;
    total += check::<ViewerMetrics>(&store).await.0;
    total += check::<PlatformViewerMetrics>(&store).await.0;
    total += check::<ProfileIdentityLink>(&store).await.0;
    assert!(total > 0, "the copy has livestream rows");
}
