//! JSON projections of tool values onto their schemas.
//!
//! Tool input gets defaults applied and keys emitted in schema order before the
//! handler runs; tool output is put in schema order with unknown keys stripped
//! before it is serialized into the `text` content block. The endpoint applies
//! these projections to every tool, whichever package owns it, so served
//! values keep their established shape. [`json_response`] writes HTTP bodies
//! with the same `JSON.stringify` semantics.

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::{Map, Value};

/// A JSON body serialized with JS `JSON.stringify` semantics.
pub fn json_response(status: StatusCode, value: &Value) -> Response {
    let mut response = Response::new(Body::from(omni_core::js::json_stringify(value)));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

/// A JS number as JSON: integral values within the safe range print without a
/// fraction (`6`, not `6.0`), like `JSON.stringify`.
pub use omni_core::js::number_value as js_number;

/// The type name reported in validation issues (`undefined` when absent).
pub fn received_type(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn type_matches(schema: &Value, value: &Value) -> bool {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    let accepts = |t: &str| t == kind || (t == "number" && kind == "integer");
    match schema.get("type") {
        Some(Value::String(t)) => accepts(t),
        Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).any(accepts),
        _ => {
            // Untyped branch: an object schema is recognizable by its properties.
            !(schema.get("properties").is_some() && kind != "object")
        }
    }
}

/// Whether every `const` property of an object branch matches the value
/// (the discriminator of a discriminated union).
fn discriminators_match(schema: &Value, value: &Value) -> bool {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return true;
    };
    properties
        .iter()
        .all(|(key, property)| match property.get("const") {
            Some(expected) => value.get(key) == Some(expected),
            None => true,
        })
}

fn branch<'a>(schema: &'a Value, value: &Value) -> &'a Value {
    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = schema.get(key) {
            let found = branches
                .iter()
                .find(|b| type_matches(b, value) && discriminators_match(b, value))
                .or_else(|| branches.iter().find(|b| type_matches(b, value)));
            if let Some(found) = found {
                return branch(found, value);
            }
        }
    }
    schema
}

/// Output projection: object keys in schema order, unknown keys dropped where
/// the schema forbids them, recursively through arrays, records and unions.
pub fn order_by_schema(value: Value, schema: &Value) -> Value {
    let schema = branch(schema, &value);
    match value {
        Value::Object(map) => Value::Object(order_object(map, schema)),
        Value::Array(items) => match schema.get("items") {
            Some(item_schema) if item_schema.is_object() => Value::Array(
                items
                    .into_iter()
                    .map(|item| order_by_schema(item, item_schema))
                    .collect(),
            ),
            _ => Value::Array(items),
        },
        other => other,
    }
}

fn order_object(mut map: Map<String, Value>, schema: &Value) -> Map<String, Value> {
    let properties = schema.get("properties").and_then(Value::as_object);
    let additional = schema.get("additionalProperties");
    if properties.is_none() && additional.is_none() {
        return map;
    }
    let mut out = Map::new();
    if let Some(properties) = properties {
        for (key, property) in properties {
            if let Some(value) = map.remove(key) {
                out.insert(key.clone(), order_by_schema(value, property));
            }
        }
    }
    match additional {
        Some(Value::Bool(false)) => {}
        Some(extra) if extra.is_object() => {
            for (key, value) in map {
                out.insert(key, order_by_schema(value, extra));
            }
        }
        _ => out.extend(map),
    }
    out
}

/// Input projection: defaults applied for absent keys, keys in schema order.
pub fn apply_defaults(value: Value, schema: &Value) -> Value {
    let schema = branch(schema, &value);
    let Value::Object(mut map) = value else {
        return value;
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Value::Object(map);
    };
    let mut out = Map::new();
    for (key, property) in properties {
        match map.remove(key) {
            Some(value) => {
                out.insert(key.clone(), apply_defaults(value, property));
            }
            None => {
                if let Some(default) = property.get("default") {
                    out.insert(key.clone(), default.clone());
                }
            }
        }
    }
    out.extend(map);
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_print_like_javascript() {
        assert_eq!(js_number(6.0).to_string(), "6");
        assert_eq!(js_number(1.5).to_string(), "1.5");
        assert_eq!(js_number(f64::NAN), Value::Null);
    }

    #[test]
    fn output_follows_schema_order_and_strips_unknown_keys() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": {"type": "number"},
                "b": {"anyOf": [{"type": "object", "properties": {"x": {}, "y": {}}, "additionalProperties": false}, {"type": "null"}]},
                "r": {"type": "object", "additionalProperties": {"type": "string"}},
                "list": {"type": "array", "items": {"type": "object", "properties": {"k": {}, "j": {}}, "additionalProperties": false}}
            },
            "additionalProperties": false
        });
        let value = json!({
            "list": [{"j": 1, "k": 2, "z": 3}],
            "r": {"z": "1", "a": "2"},
            "b": {"y": 1, "x": 2},
            "extra": true,
            "a": 1
        });
        let ordered = order_by_schema(value, &schema);
        assert_eq!(
            omni_core::js::json_stringify(&ordered),
            r#"{"a":1,"b":{"x":2,"y":1},"r":{"z":"1","a":"2"},"list":[{"k":2,"j":1}]}"#
        );
        assert_eq!(
            order_by_schema(json!({"b": null}), &schema),
            json!({"b": null})
        );
    }

    #[test]
    fn unions_pick_the_branch_whose_discriminator_matches() {
        let schema = json!({
            "type": "object",
            "oneOf": [
                {"type": "object", "properties": {"action": {"const": "enqueue"}, "position": {"default": "next"}}},
                {"type": "object", "properties": {"action": {"const": "dequeue"}, "episodeGuid": {"type": "string"}}}
            ]
        });
        assert_eq!(
            apply_defaults(json!({"episodeGuid": "g", "action": "dequeue"}), &schema),
            json!({"action": "dequeue", "episodeGuid": "g"})
        );
        assert_eq!(
            apply_defaults(json!({"action": "enqueue"}), &schema),
            json!({"action": "enqueue", "position": "next"})
        );
    }

    #[test]
    fn input_defaults_fill_absent_keys_in_schema_order() {
        let schema = json!({
            "type": "object",
            "properties": {
                "cursor": {"type": "integer", "default": 0},
                "limit": {"type": "integer", "default": 25},
                "q": {"type": "string"}
            },
            "additionalProperties": false
        });
        assert_eq!(
            omni_core::js::json_stringify(&apply_defaults(json!({"q": "x", "limit": 5}), &schema)),
            r#"{"cursor":0,"limit":5,"q":"x"}"#
        );
    }
}
