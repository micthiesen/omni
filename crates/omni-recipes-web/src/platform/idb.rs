//! IndexedDB through `js/idb.js`. Storage failing (private browsing, quota)
//! never stops the app: reads come back empty and writes are dropped, and the
//! library is rebuilt from the server.

use js_sys::Array;
use serde::Serialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::prelude::*;

pub const RECIPES: &str = "recipes";
pub const SESSIONS: &str = "sessions";
pub const KV: &str = "kv";
pub const OUTBOX: &str = "outbox";
pub const BLOBS: &str = "blobs";

#[wasm_bindgen(module = "/js/idb.js")]
extern "C" {
    #[wasm_bindgen(catch, js_name = idbAll)]
    async fn idb_all(store: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch, js_name = idbGet)]
    async fn idb_get(store: &str, key: &JsValue) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch, js_name = idbWrite)]
    async fn idb_write(
        store: &str,
        puts: &Array,
        deletes: &Array,
        clear: bool,
    ) -> Result<JsValue, JsValue>;
}

/// Every value of `store` that decodes, in key order.
pub async fn all<T: DeserializeOwned>(store: &str) -> Vec<T> {
    let Ok(rows) = idb_all(store).await else {
        return Vec::new();
    };
    Array::from(&rows)
        .iter()
        .filter_map(|row| Array::from(&row).get(1).as_string())
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect()
}

pub async fn get<T: DeserializeOwned>(store: &str, key: &str) -> Option<T> {
    let value = idb_get(store, &JsValue::from_str(key)).await.ok()?;
    serde_json::from_str(&value.as_string()?).ok()
}

pub async fn get_bytes(key: &str) -> Option<js_sys::Uint8Array> {
    idb_get(BLOBS, &JsValue::from_str(key))
        .await
        .ok()?
        .dyn_into()
        .ok()
}

fn pair(key: &JsValue, value: &JsValue) -> JsValue {
    Array::of2(key, value).into()
}

fn spawn_write(store: &'static str, puts: Array, deletes: Array, clear: bool) {
    leptos::task::spawn_local(async move {
        let _ = idb_write(store, &puts, &deletes, clear).await;
    });
}

/// Stores `value` as JSON; the write happens in the background, in call order.
pub fn put<T: Serialize>(store: &'static str, key: impl Into<JsValue>, value: &T) {
    if let Ok(text) = serde_json::to_string(value) {
        let puts = Array::of1(&pair(&key.into(), &JsValue::from_str(&text)));
        spawn_write(store, puts, Array::new(), false);
    }
}

pub fn delete(store: &'static str, key: impl Into<JsValue>) {
    spawn_write(store, Array::new(), Array::of1(&key.into()), false);
}

/// Replaces or patches a store in one transaction.
pub fn write_all<T: Serialize>(
    store: &'static str,
    puts: impl IntoIterator<Item = (String, T)>,
    deletes: impl IntoIterator<Item = String>,
    clear: bool,
) {
    let puts: Array = puts
        .into_iter()
        .filter_map(|(key, value)| {
            let text = serde_json::to_string(&value).ok()?;
            Some(pair(&JsValue::from_str(&key), &JsValue::from_str(&text)))
        })
        .collect();
    let deletes: Array = deletes
        .into_iter()
        .map(|key| JsValue::from_str(&key))
        .collect();
    spawn_write(store, puts, deletes, clear);
}

/// Stores photo bytes and waits, so an outbox entry never names bytes that
/// are not there yet.
pub async fn put_bytes(key: &str, bytes: &js_sys::Uint8Array) -> bool {
    let puts = Array::of1(&pair(&JsValue::from_str(key), bytes));
    idb_write(BLOBS, &puts, &Array::new(), false).await.is_ok()
}
