//! Counts that follow the scale: `{yield}`, a step's `portions`, and the
//! `unscaled_number` warning for counts written into prose.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_recipe_format::{
    Code, Index, Input, Portions, RecipeDoc, Step, render_text, scaled, validate,
};

fn cookies() -> RecipeDoc {
    serde_json::from_str(include_str!("golden/chocolate-chip-cookies.json")).unwrap()
}

fn step_mut<'a>(doc: &'a mut RecipeDoc, id: &str) -> &'a mut Step {
    doc.components
        .iter_mut()
        .flat_map(|component| &mut component.steps)
        .find(|step| step.id == id)
        .unwrap()
}

fn text_at(doc: &RecipeDoc, id: &str, factor: f64) -> String {
    let ix = Index::build(doc);
    let step = ix.step(id).unwrap();
    render_text(&ix, step, factor)
}

fn warnings(doc: &RecipeDoc, code: Code) -> Vec<String> {
    validate(doc)
        .warnings
        .iter()
        .filter(|problem| problem.code == code)
        .map(|problem| problem.line())
        .collect()
}

#[test]
fn a_count_equal_to_the_yield_warns_and_points_at_yield() {
    let mut doc = cookies();
    doc.r#yield.qty = 8.0;
    doc.r#yield.unit = "rolls".to_owned();
    step_mut(&mut doc, "scoop").text = "Divide the dough into 8 pieces.".to_owned();
    // The step no longer mentions its input; only the count matters here.
    let found = warnings(&doc, Code::UnscaledNumber);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].starts_with(
            "steps.scoop.text: [unscaled_number] \"8 pieces\" will not change when the recipe is scaled"
        ),
        "{}",
        found[0]
    );
    assert!(found[0].contains("write {yield}"), "{}", found[0]);
    assert!(found[0].contains("8 rolls"), "{}", found[0]);
}

#[test]
fn any_other_count_points_at_portions_and_batches_at_repeat() {
    let mut doc = cookies();
    step_mut(&mut doc, "scoop").text = "Scoop {fold} into 24 balls.".to_owned();
    step_mut(&mut doc, "bake").text =
        "Bake {scoop} in {oven|the hot oven} in 2 batches for {time}.".to_owned();
    step_mut(&mut doc, "cool").tip = Some("Makes about 30 crumbs.".to_owned());
    doc.components[0].ingredients[0].note = Some("enough for 3 trays".to_owned());
    let found = warnings(&doc, Code::UnscaledNumber);
    assert_eq!(found.len(), 4, "{found:?}");
    assert!(
        found[0].starts_with("ingredients.flour.note:") && found[0].contains("make it the amount"),
        "{}",
        found[0]
    );
    assert!(
        found[1].contains("\"portions\": {\"qty\": 24, \"unit\": \"balls\"}"),
        "{}",
        found[1]
    );
    assert!(found[2].starts_with("steps.bake.text:"), "{}", found[2]);
    assert!(found[2].contains("\"repeat\""), "{}", found[2]);
    assert!(found[3].starts_with("steps.cool.tip:"), "{}", found[3]);
    assert!(
        found[3].contains("not filled in until or tip"),
        "{}",
        found[3]
    );
}

#[test]
fn the_samples_have_no_stale_counts() {
    for json in [
        include_str!("golden/chocolate-chip-cookies.json"),
        include_str!("golden/chicken-piccata.json"),
        include_str!("golden/lasagne-bolognese.json"),
        include_str!("golden/enchiladas.json"),
    ] {
        let doc: RecipeDoc = serde_json::from_str(json).unwrap();
        assert_eq!(warnings(&doc, Code::UnscaledNumber), Vec::<String>::new());
    }
}

#[test]
fn yield_and_portions_render_scaled_whole_counts() {
    let mut doc = cookies();
    let scoop = step_mut(&mut doc, "scoop");
    scoop.text = "Scoop {fold} into {yield}, then press into {portions}.".to_owned();
    scoop.portions = Some(Portions {
        qty: 4.0,
        unit: "rows".to_owned(),
    });
    assert!(validate(&doc).is_valid(), "{:?}", validate(&doc).errors);
    assert_eq!(
        text_at(&doc, "scoop", 1.0),
        "Scoop the cookie dough into 28 cookies, then press into 4 rows."
    );
    assert_eq!(
        text_at(&doc, "scoop", 1.5),
        "Scoop the cookie dough into 42 cookies, then press into 6 rows."
    );
    assert_eq!(
        text_at(&doc, "scoop", 0.6),
        "Scoop the cookie dough into 17 cookies, then press into about 2 rows."
    );
    // The rescale operation moves the base count with the yield.
    let doubled = scaled(&doc, 2.0);
    assert_eq!(
        text_at(&doubled, "scoop", 1.0),
        "Scoop the cookie dough into 56 cookies, then press into 8 rows."
    );
}

#[test]
fn portions_must_be_set_and_sensible() {
    let mut doc = cookies();
    step_mut(&mut doc, "scoop").text = "Scoop {fold} into {portions}.".to_owned();
    let report = validate(&doc);
    assert!(
        report
            .errors
            .iter()
            .any(|p| p.code == Code::PlaceholderUnset && p.path == "steps.scoop.text")
    );
    step_mut(&mut doc, "scoop").portions = Some(Portions {
        qty: 0.0,
        unit: " ".to_owned(),
    });
    let report = validate(&doc);
    let bad: Vec<&str> = report
        .errors
        .iter()
        .filter(|p| p.code == Code::BadValue)
        .map(|p| p.path.as_str())
        .collect();
    assert_eq!(bad, ["steps.scoop.portions", "steps.scoop.portions"]);
}

#[test]
fn an_input_named_yield_keeps_its_meaning() {
    let mut doc = cookies();
    doc.components[0].ingredients[0].id = "yield".to_owned();
    for step in &mut doc.components[0].steps {
        for input in &mut step.inputs {
            if input.r#ref == "flour" {
                *input = Input::all("yield");
            }
        }
        step.text = step.text.replace("{flour}", "{yield}");
    }
    assert!(validate(&doc).is_valid(), "{:?}", validate(&doc).errors);
    assert!(
        text_at(&doc, "dry", 1.0).starts_with("Whisk 280 g"),
        "{}",
        text_at(&doc, "dry", 1.0)
    );
}
