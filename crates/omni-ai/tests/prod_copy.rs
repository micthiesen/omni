//! Manual check against a copy of the production docstore (never the original):
//! `OMNI_PROD_COPY=/path/to/copy.db cargo test -p omni-ai --test it prod_copy:: -- --ignored`.
//! Every `cost-event` and `cost-migration` row must decode into the typed entities, and
//! re-encoding a decoded event must read back as the stored JS value.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use omni_ai::costs::{CostEventData, CostMigrationData};
use omni_core::clock::SharedClock;
use omni_store::{DocOps, EntityOps};
use omni_testkit::{TEST_EPOCH_MS, TestStore, test_clock};

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a copy of the production docstore"]
async fn production_cost_rows_decode_and_round_trip() {
    let source = std::env::var("OMNI_PROD_COPY").expect("OMNI_PROD_COPY");
    let _clock: SharedClock = test_clock(TEST_EPOCH_MS);
    let copy = TestStore::from_fixture(std::path::Path::new(&source)).await;
    let (raw, typed, migrations, equal) =
        copy.store
            .read(|docs| {
                let raw = docs.get_docs_by_entity("cost-event")?;
                let typed = docs.get_all::<CostEventData>()?;
                let migrations = docs.get_all::<CostMigrationData>()?;
                let mut equal = 0usize;
                for (pk, value) in &raw {
                    let event: CostEventData = omni_store::cbor::from_value(value.clone())
                        .map_err(|e| omni_store::StoreError::CorruptRow {
                            pk: pk.clone(),
                            reason: e.to_string(),
                        })?;
                    let reread = omni_store::cbor::to_value(&event)
                        .ok()
                        .and_then(|v| omni_store::cbor::decode(&omni_store::cbor::encode(&v)).ok());
                    if reread.is_some_and(|reread| omni_store::cbor::same_value(&reread, value)) {
                        equal += 1;
                    }
                }
                Ok((raw.len(), typed.len(), migrations.len(), equal))
            })
            .await
            .unwrap();
    println!("cost-event rows {raw}, typed {typed}, value-equal {equal}");
    assert!(raw > 0);
    assert_eq!(raw, typed, "every cost-event row decodes");
    assert_eq!(migrations, 1);
    assert_eq!(equal, raw, "every row re-encodes to the same JS value");
}
