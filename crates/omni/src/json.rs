//! JSON responses with `JSON.stringify` semantics: integral numbers print
//! without `.0` and stored values convert the way decoded JS values stringify.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use omni_store::cbor::JsValue;
use serde::Serialize;
use serde_json::{Map, Value};

/// A `JSON.stringify` body with `content-type: application/json`.
pub fn js_json(status: StatusCode, value: &Value) -> Response {
    let body = omni_core::js::json_stringify(&omni_core::js::normalize_numbers(value.clone()));
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Serializes `value` and responds with [`js_json`]; a serialization failure is a 500.
pub fn js_json_of<T: Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_value(value) {
        Ok(value) => js_json(status, &value),
        Err(error) => omni_server_kit::ApiError::internal(error).into_response(),
    }
}

/// `{"error": message}` with `status`.
pub fn error(status: StatusCode, message: impl Into<String>) -> Response {
    omni_server_kit::api_error(status, message)
}

/// Why a stored value has no JSON form (`JSON.stringify` throws on BigInt).
#[derive(Debug, thiserror::Error)]
#[error("Do not know how to serialize a BigInt")]
pub struct BigIntNotSerializable;

fn js_number(n: f64) -> Value {
    omni_core::js::number_value(n)
}

/// The JSON value `JSON.stringify` produces for a node-cbor decoded value;
/// `None` for values JSON omits (`undefined`).
pub fn js_value_to_json(value: &JsValue) -> Result<Option<Value>, BigIntNotSerializable> {
    Ok(Some(match value {
        JsValue::Undefined => return Ok(None),
        JsValue::Null => Value::Null,
        JsValue::Bool(b) => Value::Bool(*b),
        #[allow(clippy::cast_precision_loss)]
        JsValue::Int(i) => js_number(*i as f64),
        JsValue::Float(f) => js_number(*f),
        JsValue::String(s) => Value::String(s.clone()),
        // A node `Buffer` stringifies through `Buffer#toJSON`.
        JsValue::Bytes(bytes) => {
            let mut map = Map::new();
            map.insert("type".to_owned(), Value::String("Buffer".to_owned()));
            map.insert(
                "data".to_owned(),
                Value::Array(bytes.iter().map(|b| Value::from(*b)).collect()),
            );
            Value::Object(map)
        }
        JsValue::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| js_value_to_json(item).map(|v| v.unwrap_or(Value::Null)))
                .collect::<Result<_, _>>()?,
        ),
        JsValue::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                if let Some(item) = js_value_to_json(item)? {
                    out.insert(key.clone(), item);
                }
            }
            Value::Object(out)
        }
        // `Map` and `Set` have no own enumerable properties: `{}`.
        JsValue::Map(_) | JsValue::Set(_) => Value::Object(Map::new()),
        // `Date#toJSON`: ISO string, or null for an invalid date.
        JsValue::Date(ms) => {
            #[allow(clippy::cast_precision_loss)]
            let max = omni_core::js::MAX_DATE_MS as f64;
            if ms.is_finite() && ms.abs() <= max {
                #[allow(clippy::cast_possible_truncation)]
                Value::String(omni_core::js::to_iso_string(ms.trunc() as i64))
            } else {
                Value::Null
            }
        }
        JsValue::BigInt(_) => return Err(BigIntNotSerializable),
        // node-cbor `Tagged { tag, value, err }` (err undefined).
        JsValue::Tagged(tag, inner) => {
            let mut out = Map::new();
            #[allow(clippy::cast_precision_loss)]
            out.insert("tag".to_owned(), js_number(*tag as f64));
            if let Some(inner) = js_value_to_json(inner)? {
                out.insert("value".to_owned(), inner);
            }
            Value::Object(out)
        }
        // node-cbor `Simple { value }`.
        JsValue::Simple(v) => {
            let mut out = Map::new();
            out.insert("value".to_owned(), Value::from(*v));
            Value::Object(out)
        }
    }))
}

/// JSON (as decoded from a request body) to a JS value as node would hold it.
pub fn json_to_js_value(value: &Value) -> JsValue {
    match value {
        Value::Null => JsValue::Null,
        Value::Bool(b) => JsValue::Bool(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) if i.unsigned_abs() <= 9_007_199_254_740_991 => JsValue::Int(i128::from(i)),
            _ => JsValue::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => JsValue::String(s.clone()),
        Value::Array(items) => JsValue::Array(items.iter().map(json_to_js_value).collect()),
        Value::Object(map) => JsValue::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_js_value(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use serde_json::json;

    #[test]
    fn stringifies_like_json_stringify() {
        let mut obj = IndexMap::new();
        obj.insert("a".to_owned(), JsValue::Float(42.0));
        obj.insert("gone".to_owned(), JsValue::Undefined);
        obj.insert(
            "list".to_owned(),
            JsValue::Array(vec![JsValue::Undefined, JsValue::Float(f64::NAN)]),
        );
        obj.insert("at".to_owned(), JsValue::Date(1_000.0));
        obj.insert("set".to_owned(), JsValue::Set(vec![JsValue::Int(1)]));
        obj.insert("buf".to_owned(), JsValue::Bytes(vec![1, 2]));
        let value = js_value_to_json(&JsValue::Object(obj)).unwrap().unwrap();
        assert_eq!(
            omni_core::js::json_stringify(&value),
            r#"{"a":42,"list":[null,null],"at":"1970-01-01T00:00:01.000Z","set":{},"buf":{"type":"Buffer","data":[1,2]}}"#
        );
        assert!(js_value_to_json(&JsValue::BigInt(1)).is_err());
    }

    #[test]
    fn json_numbers_become_js_numbers() {
        assert_eq!(json_to_js_value(&json!(3)), JsValue::Int(3));
        assert_eq!(json_to_js_value(&json!(1.5)), JsValue::Float(1.5));
    }
}
