//! The JSON Schemas derived for the MCP tool contracts stay within what the
//! gateway serves: every subschema inlined, unit enums as one string `enum`,
//! and operations as a union discriminated by `op`.

#![cfg(feature = "schema")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_recipe_format::{Block, ListView, Op, RecipeDoc, Report, Segment, TimelineRow};
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde_json::Value;

fn inlined<T: JsonSchema>() -> Value {
    let settings = SchemaSettings::draft2020_12().with(|s| s.inline_subschemas = true);
    settings
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value()
}

fn contains_key(value: &Value, wanted: &str) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| key == wanted || contains_key(value, wanted)),
        Value::Array(items) => items.iter().any(|item| contains_key(item, wanted)),
        _ => false,
    }
}

#[test]
fn schemas_inline_every_subschema() {
    let schemas = [
        inlined::<RecipeDoc>(),
        inlined::<Op>(),
        inlined::<Report>(),
        inlined::<ListView>(),
        inlined::<Block>(),
        inlined::<TimelineRow>(),
        inlined::<Segment>(),
    ];
    for schema in &schemas {
        assert!(!contains_key(schema, "$ref"));
        assert!(!contains_key(schema, "$defs"));
    }
}

#[test]
fn the_document_schema_refuses_unknown_fields_and_keeps_enums_plain() {
    let schema = inlined::<RecipeDoc>();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["required"],
        serde_json::json!(["title", "yield", "source", "components"])
    );
    let step = &schema["properties"]["components"]["items"]["properties"]["steps"]["items"];
    assert_eq!(step["additionalProperties"], false);
    assert_eq!(
        step["properties"]["time"]["properties"]["attention"]["enum"],
        serde_json::json!(["active", "passive"])
    );
    assert_eq!(
        schema["properties"]["source"]["properties"]["kind"]["enum"],
        serde_json::json!(["web", "book", "person", "original", "crouton"])
    );
    let code = &inlined::<Report>()["properties"]["errors"]["items"]["properties"]["code"];
    assert_eq!(code["enum"].as_array().unwrap().len(), 37);
}

#[test]
fn operations_are_a_union_discriminated_by_op() {
    let schema = inlined::<Op>();
    let names: Vec<&str> = schema["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|variant| {
            assert_eq!(variant["additionalProperties"], false);
            variant["properties"]["op"]["const"].as_str().unwrap()
        })
        .collect();
    assert_eq!(
        names,
        [
            "set",
            "setRecipe",
            "setInput",
            "addIngredient",
            "addStep",
            "addComponent",
            "remove",
            "move",
            "replaceText",
            "rename",
            "rescale"
        ]
    );
}
