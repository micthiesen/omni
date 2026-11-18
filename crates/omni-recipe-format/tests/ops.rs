//! Edit operations: what each changes, what each refuses, and how a refusal
//! names the operation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_recipe_format::{
    FORMAT, Index, Op, RecipeDoc, UpgradeError, apply_ops, diff_summary, render_text, scaled,
    upgrade, validate,
};
use serde_json::{Value, json};

fn piccata() -> RecipeDoc {
    serde_json::from_str(include_str!("golden/chicken-piccata.json")).unwrap()
}

fn lasagne() -> RecipeDoc {
    serde_json::from_str(include_str!("golden/lasagne-bolognese.json")).unwrap()
}

fn ops(value: Value) -> Vec<Op> {
    serde_json::from_value(value).unwrap()
}

fn refusal(doc: &RecipeDoc, value: Value) -> String {
    apply_ops(doc, &ops(value)).unwrap_err().to_string()
}

fn ids<'a>(items: impl IntoIterator<Item = &'a String>) -> Vec<&'a str> {
    items.into_iter().map(String::as_str).collect()
}

#[test]
fn a_refusal_names_the_operation_and_changes_nothing() {
    let doc = piccata();
    let error = apply_ops(
        &doc,
        &ops(json!([
            {"op": "set", "target": "lemon", "fields": {"qty": 3}},
            {"op": "remove", "target": "nothing"},
        ])),
    )
    .unwrap_err();
    assert_eq!((error.index, error.op), (1, "remove"));
    assert_eq!(
        error.to_string(),
        "ops[1] (remove): no ingredient, step or component has id \"nothing\""
    );
}

#[test]
fn set_refuses_unknown_and_malformed_fields() {
    let doc = piccata();
    let unknown = refusal(
        &doc,
        json!([{"op": "set", "target": "lemon", "fields": {"colour": "yellow"}}]),
    );
    assert!(
        unknown.starts_with("ops[0] (set): unknown field `colour`, expected one of `id`, `qty`"),
        "{unknown}"
    );
    let shape = refusal(
        &doc,
        json!([{"op": "set", "target": "sear", "fields": {"time": {"min": "six"}}}]),
    );
    assert!(
        shape.starts_with("ops[0] (set): invalid type: string \"six\", expected u32"),
        "{shape}"
    );
    let required = refusal(
        &doc,
        json!([{"op": "set", "target": "sear", "unset": ["text"]}]),
    );
    assert_eq!(required, "ops[0] (set): missing field `text`");
    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "set", "target": "lemon", "fields": {"id": "lime"}}])
        ),
        "ops[0] (set): set cannot change an id; use {\"op\": \"rename\", \"target\": \"lemon\", \"to\": \"lime\"}"
    );
}

#[test]
fn set_merges_fields_and_unset_restores_defaults() {
    let doc = piccata();
    let edited = apply_ops(
        &doc,
        &ops(json!([
            {"op": "set", "target": "capers", "fields": {"qty": 3, "scales": false}, "unset": ["optional", "prep"]},
            {"op": "set", "target": "sear", "fields": {"temp": {"value": 200, "unit": "C"}, "tip": "Do not crowd the pan."}},
            {"op": "set", "target": "piccata", "fields": {"name": "Piccata", "makeAhead": {"keeps": "1 day", "where": "fridge"}}},
        ])),
    )
    .unwrap();
    let ix = Index::build(&edited);
    let capers = ix.ingredient("capers").unwrap();
    assert_eq!(
        (capers.qty, capers.scales, capers.optional, &capers.prep),
        (Some(3.0), false, false, &None)
    );
    assert_eq!(
        diff_summary(&doc, &edited),
        [
            "~ piccata.makeAhead: null → {\"keeps\": \"1 day\", \"where\": \"fridge\"}",
            "~ piccata.name: null → \"Piccata\"",
            "~ capers.optional: true → null",
            "~ capers.prep: \"drained\" → null",
            "~ capers.qty: 2 → 3",
            "~ capers.scales: null → false",
            "~ sear.temp: null → {\"value\": 200, \"unit\": \"C\"}",
            "~ sear.tip: null → \"Do not crowd the pan.\"",
        ]
    );
}

#[test]
fn set_recipe_changes_the_header_and_nothing_else() {
    let doc = piccata();
    let edited = apply_ops(
        &doc,
        &ops(json!([{"op": "setRecipe",
            "fields": {"title": "Veal Piccata", "yield": {"qty": 2, "unit": "servings"},
                       "time": {"activeMin": 20, "totalMin": 30},
                       "source": {"kind": "book", "name": "Essentials", "author": "M. Hazan"}},
            "unset": ["summary"]}])),
    )
    .unwrap();
    assert_eq!(
        diff_summary(&doc, &edited),
        [
            "title: \"Chicken Piccata\" → \"Veal Piccata\"",
            "summary: \"Pan-seared cutlets in a lemon, caper and butter pan sauce.\" → null",
            "yield: {\"qty\": 4, \"unit\": \"servings\"} → {\"qty\": 2, \"unit\": \"servings\"}",
            "time: null → {\"activeMin\": 20, \"totalMin\": 30}",
            "source: {\"kind\": \"original\", \"name\": \"House recipe\"} → {\"kind\": \"book\", \"name\": \"Essentials\", \"author\": \"M. Hazan\"}",
        ]
    );
    assert_eq!(edited.components, doc.components);

    let components = refusal(
        &doc,
        json!([{"op": "setRecipe", "fields": {"components": []}}]),
    );
    assert!(
        components.starts_with("ops[0] (setRecipe): unknown field `components`"),
        "{components}"
    );
}

#[test]
fn set_input_adds_changes_and_removes_one_input() {
    let doc = piccata();
    let edited = apply_ops(
        &doc,
        &ops(json!([
            {"op": "setInput", "step": "serve", "ref": "capers", "fields": {"prep": "fried"}},
            {"op": "setInput", "step": "simmer", "ref": "lemon", "fields": {"qty": 0.5}, "unset": ["part"]},
            {"op": "setInput", "step": "simmer", "ref": "capers", "remove": true},
        ])),
    )
    .unwrap();
    let ix = Index::build(&edited);
    let serve = ix.step("serve").unwrap().inputs.last().unwrap();
    assert_eq!(
        (serve.r#ref.as_str(), serve.prep.as_deref()),
        ("capers", Some("fried"))
    );
    let simmer = ix.step("simmer").unwrap();
    assert_eq!(
        ids(simmer.inputs.iter().map(|i| &i.r#ref)),
        ["deglaze", "stock", "lemon"]
    );
    assert_eq!(
        (simmer.inputs[2].qty, &simmer.inputs[2].part),
        (Some(0.5), &None)
    );

    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "setInput", "step": "sear", "ref": "wine", "remove": true}])
        ),
        "ops[0] (setInput): step \"sear\" has no input \"wine\""
    );
    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "setInput", "step": "butter", "ref": "wine"}])
        ),
        "ops[0] (setInput): \"butter\" is not a step"
    );
    let bad = refusal(
        &doc,
        json!([{"op": "setInput", "step": "sear", "ref": "oil", "fields": {"qty": "some"}}]),
    );
    assert!(
        bad.starts_with("ops[0] (setInput): invalid type: string \"some\""),
        "{bad}"
    );
}

#[test]
fn nodes_are_added_after_an_anchor_or_first() {
    let doc = piccata();
    let shallot = json!({"id": "shallot", "qty": 1, "name": "shallot", "prep": "minced"});
    let sweat = json!({"id": "sweat", "action": "sweat", "output": "softened shallot",
        "inputs": [{"ref": "shallot"}, {"ref": "fond"}],
        "text": "Soften {shallot} in {fond|the pan}."});
    let edited = apply_ops(
        &doc,
        &ops(json!([
            {"op": "addIngredient", "component": "piccata", "after": "butter", "ingredient": shallot},
            {"op": "addIngredient", "component": "piccata",
             "ingredient": {"id": "garlic", "qty": 1, "unit": "clove", "name": "garlic"}},
            {"op": "addStep", "component": "piccata", "after": "sear", "step": sweat},
        ])),
    )
    .unwrap();
    let component = &edited.components[0];
    assert_eq!(
        ids(component.ingredients.iter().map(|i| &i.id))[..3],
        ["garlic", "chicken", "salt"]
    );
    assert_eq!(component.ingredients[7].id, "shallot");
    assert_eq!(
        ids(component.steps.iter().map(|s| &s.id)),
        [
            "dredge", "sear", "sweat", "deglaze", "simmer", "mount", "serve"
        ]
    );
    assert_eq!(
        diff_summary(&doc, &edited)[..3],
        [
            "+ ingredient garlic",
            "+ ingredient shallot",
            "+ step sweat"
        ]
    );

    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "addIngredient", "component": "piccata", "after": "sear",
                "ingredient": {"id": "x", "name": "x"}}])
        ),
        "ops[0] (addIngredient): \"sear\" is not an ingredient of component \"piccata\""
    );
    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "addStep", "component": "sear",
                "step": {"id": "x", "action": "x", "inputs": [], "text": "x"}}])
        ),
        "ops[0] (addStep): \"sear\" is not a component"
    );
    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "addComponent", "after": "butter",
                "component": {"id": "x", "steps": []}}])
        ),
        "ops[0] (addComponent): \"butter\" is not a component"
    );
}

#[test]
fn move_reorders_within_a_kind() {
    let doc = lasagne();
    let edited = apply_ops(
        &doc,
        &ops(json!([
            {"op": "move", "target": "paste", "after": "oil"},
            {"op": "move", "target": "bsalt"},
            {"op": "move", "target": "parm", "after": "bflour"},
        ])),
    )
    .unwrap();
    assert_eq!(
        diff_summary(&doc, &edited),
        [
            "~ parm: moved from assembly to bechamel",
            "~ ragu.ingredients: order → oil, paste, onion, carrot, celery, beef, pork, ragusalt, wine, ragumilk, passata",
            "~ bechamel.ingredients: order → bsalt, bbutter, bflour, parm, bmilk, nutmeg",
        ]
    );
    assert_eq!(
        ids(edited.components[2].ingredients.iter().map(|i| &i.id)),
        ["dishbutter", "sheets"]
    );

    let components = apply_ops(&doc, &ops(json!([{"op": "move", "target": "bechamel"}]))).unwrap();
    assert_eq!(
        diff_summary(&doc, &components),
        ["~ components: order → bechamel, ragu, assembly"]
    );

    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "move", "target": "paste", "after": "simmer"}])
        ),
        "ops[0] (move): \"simmer\" is not an ingredient; \"paste\" can only move after another ingredient"
    );
    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "move", "target": "paste", "after": "paste"}])
        ),
        "ops[0] (move): no ingredient, step or component has id \"paste\""
    );
}

#[test]
fn replace_text_needs_exactly_one_match_in_a_step() {
    let doc = piccata();
    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "replaceText", "target": "sear", "find": "the", "replace": "a"}])
        ),
        "ops[0] (replaceText): \"the\" occurs 4 times in the text of \"sear\"; it must occur exactly once"
    );
    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "replaceText", "target": "sear", "find": "wok", "replace": "pan"}])
        ),
        "ops[0] (replaceText): \"wok\" occurs 0 times in the text of \"sear\"; it must occur exactly once"
    );
    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "replaceText", "target": "butter", "find": "a", "replace": "b"}])
        ),
        "ops[0] (replaceText): \"butter\" is not a step; only step text can be replaced"
    );
    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "replaceText", "target": "sear", "find": "", "replace": "b"}])
        ),
        "ops[0] (replaceText): find must not be empty"
    );
}

#[test]
fn rename_rewrites_every_reference_and_placeholder() {
    let doc = lasagne();
    let edited = apply_ops(
        &doc,
        &ops(json!([
            {"op": "rename", "target": "bechamel", "to": "white_sauce"},
            {"op": "rename", "target": "top", "to": "topping"},
            {"op": "rename", "target": "parm", "to": "cheese"},
        ])),
    )
    .unwrap();
    assert!(validate(&edited).is_valid());
    let ix = Index::build(&edited);
    let layer = ix.step("layer").unwrap();
    assert!(layer.text.contains("{white_sauce} and {cheese}"));
    assert_eq!(
        ix.step("topping").unwrap().text,
        "Spread {white_sauce} over {layer|the top} and finish with {cheese}."
    );
    assert_eq!(
        ix.step("bake").unwrap().text,
        "Cover {topping} with foil and bake in {oven|the hot oven} for {time}."
    );
    for (_, step) in Index::build(&doc).steps() {
        let renamed = match step.id.as_str() {
            "top" => "topping",
            other => other,
        };
        assert_eq!(
            render_text(&ix, ix.step(renamed).unwrap(), 1.0),
            render_text(&Index::build(&doc), step, 1.0)
        );
    }

    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "rename", "target": "parm", "to": "sheets"}])
        ),
        "ops[0] (rename): id \"sheets\" is already in use"
    );
    assert_eq!(
        refusal(
            &doc,
            json!([{"op": "rename", "target": "nothing", "to": "x"}])
        ),
        "ops[0] (rename): no ingredient, step or component has id \"nothing\""
    );
}

#[test]
fn rename_leaves_longer_ids_with_the_same_prefix_alone() {
    let doc = lasagne();
    let edited = apply_ops(
        &doc,
        &ops(json!([{"op": "rename", "target": "brown", "to": "browned"}])),
    )
    .unwrap();
    let ix = Index::build(&edited);
    assert!(ix.step("brown2").is_some());
    assert_eq!(
        ix.step("rest").unwrap().text,
        "Rest {brown2} for {time} before cutting."
    );
    assert_eq!(
        ix.step("reduce").unwrap().text,
        "Pour {wine} over {browned} and boil until the pan is nearly dry, about {time}."
    );
}

#[test]
fn rescale_multiplies_amounts_that_scale() {
    let doc = lasagne();
    let edited = apply_ops(&doc, &ops(json!([{"op": "rescale", "factor": 1.5}]))).unwrap();
    assert_eq!(edited, scaled(&doc, 1.5));
    assert_eq!(edited.r#yield.qty, 12.0);
    let ix = Index::build(&edited);
    assert_eq!(ix.ingredient("parm").unwrap().qty, Some(150.0));
    assert_eq!(ix.ingredient("dishbutter").unwrap().qty, Some(1.0));
    let layer = ix.step("layer").unwrap();
    assert_eq!(layer.inputs[4].qty, Some(105.0));
    assert_eq!(layer.inputs[2].fraction.as_deref(), Some("3/4"));
    assert!(validate(&edited).is_valid());
    assert_eq!(
        diff_summary(&doc, &edited)[0],
        "yield: {\"qty\": 8, \"unit\": \"servings\"} → {\"qty\": 12, \"unit\": \"servings\"}"
    );

    assert_eq!(
        refusal(&doc, json!([{"op": "rescale", "factor": 0}])),
        "ops[0] (rescale): factor must be a number above 0"
    );
}

#[test]
fn a_use_of_a_fixed_ingredient_keeps_its_amount_when_rescaled() {
    let mut doc = lasagne();
    doc.components[2].steps[1].inputs[0].qty = Some(1.0);
    assert!(validate(&doc).is_valid());
    let doubled = scaled(&doc, 2.0);
    assert_eq!(doubled.components[2].steps[1].inputs[0].qty, Some(1.0));
    assert!(validate(&doubled).is_valid());
}

#[test]
fn operations_refuse_unknown_names_and_fields() {
    let parse = |value: Value| serde_json::from_value::<Op>(value).map_err(|e| e.to_string());
    assert!(
        parse(json!({"op": "explode", "target": "x"}))
            .unwrap_err()
            .starts_with("unknown variant `explode`, expected one of `set`, `setRecipe`")
    );
    assert!(
        parse(json!({"op": "remove", "target": "x", "force": true}))
            .unwrap_err()
            .starts_with("unknown field `force`")
    );
    assert!(parse(json!({"target": "x"})).is_err());
    let op = parse(json!({"op": "setInput", "step": "a", "ref": "b"})).unwrap();
    assert_eq!(
        serde_json::to_value(&op).unwrap(),
        json!({"op": "setInput", "step": "a", "ref": "b"})
    );
    assert_eq!(op.name(), "setInput");
}

#[test]
fn stored_documents_are_read_through_the_format_version() {
    let stored: Value = serde_json::from_str(include_str!("golden/chicken-piccata.json")).unwrap();
    assert_eq!(FORMAT, 1);
    assert_eq!(upgrade(FORMAT, stored.clone()).unwrap(), piccata());
    assert!(matches!(
        upgrade(2, stored.clone()),
        Err(UpgradeError::UnsupportedFormat { found: 2 })
    ));
    assert!(matches!(
        upgrade(0, stored),
        Err(UpgradeError::UnsupportedFormat { found: 0 })
    ));
    let error = upgrade(FORMAT, json!({"title": "x"})).unwrap_err();
    assert!(matches!(error, UpgradeError::Invalid { format: 1, .. }));
    assert_eq!(
        error.to_string(),
        "the stored recipe does not match format 1: missing field `yield`"
    );
}

#[test]
fn documents_refuse_unknown_fields_and_omit_absent_ones() {
    let mut value: Value =
        serde_json::from_str(include_str!("golden/chicken-piccata.json")).unwrap();
    value["components"][0]["steps"][0]["colour"] = json!("red");
    let error = serde_json::from_value::<RecipeDoc>(value).unwrap_err();
    assert!(error.to_string().starts_with("unknown field `colour`"));

    let written = serde_json::to_string(&piccata()).unwrap();
    assert!(!written.contains("null"));
    assert!(!written.contains("\"scales\""));
    assert!(written.contains("\"qty\":4,"));
    assert!(written.contains("\"qty\":0.5,"));
    assert!(written.contains("\"optional\":true"));
    assert!(!written.contains("\"optional\":false"));
}
