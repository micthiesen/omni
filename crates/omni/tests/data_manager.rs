//! The data manager, plus the `/api/data/entities/:slug` route contract.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use omni::data_manager::{DataManager, DeleteResult};
use omni_runtime::{AfterDelete, CanDelete, ManagedEntity};
use omni_store::cbor::{Extra, JsValue};
use omni_store::entity::{Entity, EntityDescriptor, UpsertOpts};
use omni_store::{EntityOps as _, EntityWrite as _, Store};
use omni_testkit::{TestStore, test_clock};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestRow {
    group: String,
    id: String,
    status: String,
    value: f64,
    #[serde(flatten)]
    extra: Extra,
}

impl Entity for TestRow {
    const NAME: &'static str = "data-manager-test";
    type Key = (String, String);
    fn key(&self) -> (String, String) {
        (self.group.clone(), self.id.clone())
    }
}

fn row(group: &str, id: &str, status: &str, value: f64) -> TestRow {
    TestRow {
        group: group.to_owned(),
        id: id.to_owned(),
        status: status.to_owned(),
        value,
        extra: Extra::new(),
    }
}

async fn upsert(store: &Store, row: TestRow) {
    store
        .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
        .await
        .unwrap();
}

async fn get(store: &Store, group: &str, id: &str) -> Option<TestRow> {
    let key = (group.to_owned(), id.to_owned());
    store
        .read(move |docs| docs.get::<TestRow>(&key))
        .await
        .unwrap()
}

fn managed(can_delete: Option<CanDelete>, after_delete: Option<AfterDelete>) -> ManagedEntity {
    ManagedEntity {
        slug: TestRow::NAME,
        label: "Test rows",
        description: "Test data",
        warning: None,
        entity: EntityDescriptor::of::<TestRow>(),
        primary_key: &["group", "id"],
        can_delete,
        after_delete,
    }
}

fn key(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

async fn store() -> TestStore {
    TestStore::new(test_clock(1_000)).await
}

#[tokio::test]
async fn exposes_metadata_and_rows_without_losing_composite_keys() {
    let test = store().await;
    upsert(&test.store, row("a#b", "1", "ready", 42.0)).await;
    let manager = DataManager::new(test.store.clone(), vec![managed(None, None)]);

    let list = manager.list().await.unwrap();
    assert_eq!(list[0].primary_key, vec!["group", "id"]);
    assert_eq!(list[0].count, 1);
    assert!(list[0].storage_bytes > 0);
    let rows = manager.rows(TestRow::NAME).await.unwrap().unwrap();
    assert_eq!(
        Value::Array(rows.rows.into_iter().map(Value::Object).collect()),
        json!([{ "group": "a#b", "id": "1", "status": "ready", "value": 42 }])
    );
}

#[tokio::test]
async fn requires_the_exact_primary_key_and_deletes_only_the_matching_row() {
    let test = store().await;
    upsert(&test.store, row("a", "1", "ready", 1.0)).await;
    upsert(&test.store, row("a", "2", "ready", 2.0)).await;
    let manager = DataManager::new(test.store.clone(), vec![managed(None, None)]);

    assert_eq!(
        manager
            .delete(TestRow::NAME, &key(json!({"group": "a"})))
            .await
            .unwrap(),
        Some(DeleteResult::InvalidKey)
    );
    assert_eq!(
        manager
            .delete(
                TestRow::NAME,
                &key(json!({"group": "a", "id": "1", "extra": true}))
            )
            .await
            .unwrap(),
        Some(DeleteResult::InvalidKey)
    );
    assert!(matches!(
        manager
            .delete(TestRow::NAME, &key(json!({"group": "a", "id": "1"})))
            .await,
        Ok(Some(DeleteResult::Deleted(_)))
    ));
    assert!(get(&test.store, "a", "1").await.is_none());
    assert_eq!(get(&test.store, "a", "2").await.map(|r| r.value), Some(2.0));
    assert_eq!(
        manager
            .delete(TestRow::NAME, &key(json!({"group": "a", "id": "1"})))
            .await
            .unwrap(),
        Some(DeleteResult::NotFound)
    );
}

#[tokio::test]
async fn supports_deletion_guards_and_post_delete_cleanup() {
    let test = store().await;
    upsert(&test.store, row("a", "1", "running", 1.0)).await;
    upsert(&test.store, row("a", "2", "ready", 2.0)).await;
    let cleaned: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = cleaned.clone();
    let guard: CanDelete = Arc::new(|row: &JsValue| {
        (row.get("status").and_then(JsValue::as_str) == Some("running"))
            .then(|| "Running rows are protected.".to_owned())
    });
    let after: AfterDelete = Arc::new(move |row: JsValue, _store: Store| {
        let seen = seen.clone();
        Box::pin(async move {
            if let Some(id) = row.get("id").and_then(JsValue::as_str) {
                seen.lock().unwrap().push(id.to_owned());
            }
            Ok(())
        })
    });
    let manager = DataManager::new(test.store.clone(), vec![managed(Some(guard), Some(after))]);

    assert_eq!(
        manager
            .delete(TestRow::NAME, &key(json!({"group": "a", "id": "1"})))
            .await
            .unwrap(),
        Some(DeleteResult::Blocked(
            "Running rows are protected.".to_owned()
        ))
    );
    assert!(matches!(
        manager
            .delete(TestRow::NAME, &key(json!({"group": "a", "id": "2"})))
            .await,
        Ok(Some(DeleteResult::Deleted(_)))
    ));
    assert_eq!(*cleaned.lock().unwrap(), vec!["2".to_owned()]);
}

#[tokio::test]
async fn isolates_malformed_blobs_and_allows_exact_raw_key_deletion() {
    let test = store().await;
    upsert(&test.store, row("a", "1", "ready", 1.0)).await;
    let raw_key = omni_store::entity::pk::<TestRow>(&("a".to_owned(), "1".to_owned())).unwrap();
    let target = raw_key.clone();
    test.store
        .write(move |tx| {
            tx.connection()
                .execute(
                    "UPDATE blobs SET data = ? WHERE pk = ?",
                    rusqlite::params![Vec::<u8>::new(), target],
                )
                .map_err(|e| omni_store::StoreError::Sqlite(e.to_string()))
        })
        .await
        .unwrap();
    let manager = DataManager::new(test.store.clone(), vec![managed(None, None)]);

    let rows = manager.rows(TestRow::NAME).await.unwrap().unwrap().rows;
    assert_eq!(rows.len(), 1);
    let meta = &rows[0][omni_api::data::MALFORMED_ROW_KEY];
    assert_eq!(meta["rawKey"], raw_key.as_str());
    assert!(matches!(
        manager.delete(TestRow::NAME, &rows[0]).await,
        Ok(Some(DeleteResult::Deleted(_)))
    ));
    assert_eq!(manager.list().await.unwrap()[0].count, 0);
}

#[tokio::test]
async fn routes_answer_with_the_documented_bodies() {
    let app = omni_testkit::TestApp::new().await;
    upsert(&app.ctx.store, row("a", "1", "ready", 1.0)).await;
    let manager = DataManager::new(app.ctx.store.clone(), vec![managed(None, None)]);
    let router = omni::ops::router(omni::ops::OpsState::new(app.ctx.clone(), manager));

    let (status, body) = app.get_json(&router, "/api/data/entities/nope").await;
    assert_eq!(
        (status, body),
        (StatusCode::NOT_FOUND, json!({"error": "Unknown entity"}))
    );
    let (status, body) = app
        .get_json(&router, "/api/data/entities/data-manager-test")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["summary"]["count"], 1);
    assert_eq!(body["rows"][0]["value"], 1);

    let delete = |body: Value| {
        axum::http::Request::delete("/api/data/entities/data-manager-test")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    };
    let send = |request: axum::http::Request<axum::body::Body>| {
        let router = router.clone();
        async move {
            use tower::ServiceExt as _;
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            )
        }
    };
    assert_eq!(
        send(delete(json!({"nokey": true}))).await,
        (
            StatusCode::BAD_REQUEST,
            json!({"error": "A primary key object is required"})
        )
    );
    assert_eq!(
        send(delete(json!({"key": {"group": "a"}}))).await,
        (
            StatusCode::BAD_REQUEST,
            json!({"error": "The primary key does not match this entity"})
        )
    );
    assert_eq!(
        send(delete(json!({"key": {"group": "a", "id": "9"}}))).await,
        (StatusCode::NOT_FOUND, json!({"error": "Row not found"}))
    );
    assert_eq!(
        send(delete(json!({"key": {"group": "a", "id": "1"}}))).await,
        (StatusCode::OK, json!({"deleted": true}))
    );
}
