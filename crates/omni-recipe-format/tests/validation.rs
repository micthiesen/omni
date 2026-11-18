//! Each validator rule, one seeded mistake at a time, on the sample recipes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_recipe_format::{
    Code, Component, Input, Level, RecipeDoc, Report, Step, Temp, TimeOverride, validate,
};

fn piccata() -> RecipeDoc {
    serde_json::from_str(include_str!("golden/chicken-piccata.json")).unwrap()
}

fn lasagne() -> RecipeDoc {
    serde_json::from_str(include_str!("golden/lasagne-bolognese.json")).unwrap()
}

fn step_mut<'a>(doc: &'a mut RecipeDoc, id: &str) -> &'a mut Step {
    doc.components
        .iter_mut()
        .flat_map(|component| &mut component.steps)
        .find(|step| step.id == id)
        .unwrap()
}

fn input_mut<'a>(doc: &'a mut RecipeDoc, step: &str, id: &str) -> &'a mut Input {
    step_mut(doc, step)
        .inputs
        .iter_mut()
        .find(|input| input.r#ref == id)
        .unwrap()
}

/// The one problem with `code`, as its line.
fn line(report: &Report, code: Code) -> String {
    let found: Vec<_> = report
        .errors
        .iter()
        .chain(&report.warnings)
        .filter(|problem| problem.code == code)
        .collect();
    assert_eq!(found.len(), 1, "{code}: {report:#?}");
    found[0].line()
}

fn codes(report: &Report) -> Vec<Code> {
    report.errors.iter().map(|problem| problem.code).collect()
}

#[test]
fn an_id_must_match_the_pattern() {
    let mut doc = piccata();
    doc.components[0].ingredients[4].id = "Olive Oil".to_owned();
    input_mut(&mut doc, "sear", "oil").r#ref = "Olive Oil".to_owned();
    let report = validate(&doc);
    assert_eq!(
        line(&report, Code::BadId),
        "Olive Oil: [bad_id] id \"Olive Oil\" must match ^[a-z][a-z0-9_]{0,23}$"
    );
    for (id, ok) in [
        ("a", true),
        ("a_1", true),
        ("abcdefghijklmnopqrstuvwx", true),
        ("abcdefghijklmnopqrstuvwxy", false),
        ("1a", false),
        ("_a", false),
        ("", false),
        ("é", false),
    ] {
        assert_eq!(omni_recipe_format::is_valid_id(id), ok, "{id:?}");
    }
}

#[test]
fn an_id_is_unique_across_the_recipe() {
    let mut doc = lasagne();
    doc.components[1].ingredients[0].id = "oil".to_owned();
    let report = validate(&doc);
    assert_eq!(
        line(&report, Code::DuplicateId),
        "oil: [duplicate_id] id \"oil\" is used more than once Hint: ids are unique across the whole recipe"
    );
}

#[test]
fn an_input_from_another_component_names_the_component_instead() {
    let mut doc = lasagne();
    step_mut(&mut doc, "layer").inputs.push(Input::all("bmilk"));
    let report = validate(&doc);
    assert_eq!(
        line(&report, Code::ForeignRef),
        "steps.layer.inputs.bmilk: [foreign_ref] \"bmilk\" belongs to component \"bechamel\" Hint: reference the component itself: {\"ref\": \"bechamel\"}"
    );
}

#[test]
fn a_component_may_only_use_earlier_components() {
    let mut doc = lasagne();
    step_mut(&mut doc, "simmer")
        .inputs
        .push(Input::all("bechamel"));
    let report = validate(&doc);
    assert_eq!(
        line(&report, Code::ForwardRef),
        "steps.simmer.inputs.bechamel: [forward_ref] component \"bechamel\" is defined after this step Hint: a component may only use components defined before it"
    );
}

#[test]
fn a_step_result_must_be_used_unless_it_is_the_last() {
    let mut doc = piccata();
    step_mut(&mut doc, "serve")
        .inputs
        .retain(|input| input.r#ref != "sear");
    let report = validate(&doc);
    assert_eq!(
        line(&report, Code::DanglingStep),
        "steps.sear: [dangling_step] the result of \"sear\" is never used Hint: add it to the inputs of a later step; only the last step of a component may be unused"
    );
}

#[test]
fn a_component_must_be_used_by_a_later_one_unless_it_is_the_last() {
    let mut doc = lasagne();
    let layer = step_mut(&mut doc, "layer");
    layer.inputs.retain(|input| input.r#ref != "ragu");
    layer.text = layer.text.replace("{ragu|all of the ragù}", "the ragù");
    let report = validate(&doc);
    assert_eq!(codes(&report), [Code::UnusedComponent]);
    assert_eq!(
        line(&report, Code::UnusedComponent),
        "components.ragu: [unused_component] component \"ragu\" is never used by a later component"
    );
}

#[test]
fn explicit_uses_may_not_exceed_the_total() {
    let mut doc = piccata();
    input_mut(&mut doc, "sear", "butter").qty = Some(5.0);
    assert_eq!(
        line(&validate(&doc), Code::OverAllocated),
        "ingredients.butter: [over_allocated] uses add up to 5 tbsp of 4 tbsp, leaving nothing for the \"rest\" use"
    );

    let mut doc = piccata();
    input_mut(&mut doc, "serve", "lemon").qty = Some(3.0);
    assert_eq!(
        line(&validate(&doc), Code::OverAllocated),
        "ingredients.lemon: [over_allocated] uses add up to 4 but the ingredient list says 2 Hint: fix the amounts, or mark the last use {\"rest\": true}"
    );
}

#[test]
fn uses_naming_different_parts_of_the_same_items_overlap() {
    let mut doc = piccata();
    doc.components[0].ingredients[8].qty = Some(3.0);
    let juice = input_mut(&mut doc, "simmer", "lemon");
    juice.qty = Some(3.0);
    let zest = Input {
        qty: Some(2.0),
        part: Some("zest".to_owned()),
        ..Input::all("lemon")
    };
    let serve = step_mut(&mut doc, "serve");
    serve.inputs.retain(|input| input.r#ref != "lemon");
    serve.inputs.push(zest);
    let report = validate(&doc);
    assert!(report.is_valid(), "{report:#?}");
}

#[test]
fn a_split_ingredient_says_how_it_is_divided() {
    let mut doc = piccata();
    input_mut(&mut doc, "mount", "butter").rest = false;
    assert_eq!(
        line(&validate(&doc), Code::AmbiguousSplit),
        "ingredients.butter: [ambiguous_split] \"butter\" is used by 2 steps without saying how it is divided Hint: give each use a \"qty\", or mark the last one {\"rest\": true}"
    );
}

#[test]
fn to_taste_ingredients_may_be_used_twice_without_amounts() {
    let doc = piccata();
    let uses = doc.components[0]
        .steps
        .iter()
        .flat_map(|step| &step.inputs)
        .filter(|input| input.r#ref == "salt")
        .count();
    assert_eq!(uses, 2);
    assert!(validate(&doc).is_valid());
}

#[test]
fn a_step_lists_an_input_once() {
    let mut doc = piccata();
    step_mut(&mut doc, "dredge")
        .inputs
        .push(Input::all("chicken"));
    let report = validate(&doc);
    assert_eq!(
        line(&report, Code::DuplicateInput),
        "steps.dredge.inputs.chicken: [duplicate_input] \"chicken\" is listed twice in this step"
    );
}

#[test]
fn an_input_is_sized_one_way() {
    let mut doc = piccata();
    input_mut(&mut doc, "sear", "butter").fraction = Some("1/2".to_owned());
    assert_eq!(
        line(&validate(&doc), Code::ConflictingAmount),
        "steps.sear.inputs.butter: [conflicting_amount] use only one of qty, fraction or rest"
    );
}

#[test]
fn a_component_needs_a_step_and_a_step_needs_action_and_text() {
    let mut doc = piccata();
    doc.components[0].steps.clear();
    let report = validate(&doc);
    assert_eq!(codes(&report), [Code::NoSteps]);
    assert_eq!(
        line(&report, Code::NoSteps),
        "components.piccata: [no_steps] a component needs at least one step"
    );

    let mut doc = piccata();
    step_mut(&mut doc, "sear").action = "  ".to_owned();
    step_mut(&mut doc, "mount").text = String::new();
    let report = validate(&doc);
    assert_eq!(
        line(&report, Code::EmptyAction),
        "steps.sear: [empty_action] action is required: the short verb phrase shown in the chart"
    );
    assert_eq!(
        line(&report, Code::EmptyText),
        "steps.mount: [empty_text] text is required"
    );
}

#[test]
fn time_and_temp_placeholders_need_the_field_on_the_step() {
    let mut doc = piccata();
    let deglaze = step_mut(&mut doc, "deglaze");
    deglaze.time = None;
    deglaze.text.push_str(" Keep at {temp}.");
    let report = validate(&doc);
    let lines: Vec<String> = report.errors.iter().map(|problem| problem.line()).collect();
    assert_eq!(
        lines,
        [
            "steps.deglaze.text: [placeholder_unset] {time} is used but the step has no time",
            "steps.deglaze.text: [placeholder_unset] {temp} is used but the step has no temp",
        ]
    );
}

#[test]
fn a_step_named_in_prose_needs_an_output_or_a_label() {
    let mut doc = piccata();
    step_mut(&mut doc, "dredge").output = None;
    assert_eq!(
        line(&validate(&doc), Code::RefNeedsLabel),
        "steps.sear.text: [ref_needs_label] step \"dredge\" has no output name Hint: name it (\"output\": \"…\") or label the reference: {dredge|the …}"
    );
    let sear = step_mut(&mut doc, "sear");
    sear.text = sear.text.replace("{dredge}", "{dredge|the cutlets}");
    assert!(validate(&doc).is_valid());
}

#[test]
fn warnings_do_not_block_a_save() {
    let mut doc = piccata();
    step_mut(&mut doc, "serve").action = "sauce, garnish and carry it out".to_owned();
    doc.time = Some(TimeOverride {
        active_min: 90,
        total_min: 120,
    });
    doc.components[0].ingredients[5].unit = Some("tablespoon".to_owned());
    doc.components[0].ingredients[3].unit = Some("handful".to_owned());
    let report = validate(&doc);
    assert!(report.is_valid());
    assert!(report.warnings.iter().all(|w| w.level == Level::Warning));
    assert_eq!(
        line(&report, Code::LongAction),
        "steps.serve: [long_action] action \"sauce, garnish and carry it out\" is 31 characters Hint: keep chart labels to a verb or two; details belong in text"
    );
    assert_eq!(
        line(&report, Code::TimeMismatch),
        "time.activeMin: [time_mismatch] stated active time is 90 min but active steps add up to 23 min"
    );
    let units: Vec<String> = report
        .warnings
        .iter()
        .filter(|w| w.code == Code::UnknownUnit)
        .map(|w| w.line())
        .collect();
    assert_eq!(
        units,
        [
            "ingredients.flour: [unknown_unit] unit \"handful\" is not in the unit list Hint: use a listed unit, or leave unit out and put the word in the name",
            "ingredients.butter: [unknown_unit] unit \"tablespoon\" is not in the unit list Hint: did you mean \"tbsp\"?",
        ]
    );
}

#[test]
fn the_format_limits_are_enforced() {
    let mut doc = lasagne();
    doc.title = "x".repeat(121);
    doc.r#yield.qty = 0.0;
    doc.components[1].name = None;
    doc.components[0].ingredients[0].qty = Some(-2.0);
    step_mut(&mut doc, "roux").action = "a".repeat(41);
    input_mut(&mut doc, "layer", "bechamel").fraction = Some("three quarters".to_owned());
    step_mut(&mut doc, "bake").temp = Some(Temp {
        value: 375.0,
        unit: "K".to_owned(),
    });
    step_mut(&mut doc, "brown2").time.as_mut().unwrap().max = Some(5);
    let report = validate(&doc);
    let lines: Vec<String> = report.errors.iter().map(|problem| problem.line()).collect();
    assert_eq!(
        lines,
        [
            "title: [too_long] title is 121 characters; the limit is 120",
            "yield: [bad_value] yield qty must be a number above 0",
            "ingredients.oil: [bad_value] qty must be a number, 0 or more",
            "components.bechamel: [missing_name] component \"bechamel\" has no name Hint: every component needs a name when a recipe has more than one",
            "steps.roux: [too_long] action is 41 characters; the limit is 40 Hint: keep chart labels to a verb or two; details belong in text",
            "steps.layer.inputs.bechamel: [bad_value] fraction \"three quarters\" is not a share between 0 and 1 Hint: write it as a ratio such as \"2/3\"",
            "steps.bake.temp: [bad_value] temp unit \"K\" must be \"F\" or \"C\"",
            "steps.brown2.time: [bad_value] max is less than min",
        ]
    );
    assert_eq!(report.warnings, []);
}

#[test]
fn a_recipe_has_one_to_eight_components() {
    let mut doc = piccata();
    let only = doc.components[0].clone();
    doc.components.clear();
    let report = validate(&doc);
    assert_eq!(
        line(&report, Code::NoComponents),
        "components: [no_components] a recipe needs at least one component"
    );

    doc.components = (0..9)
        .map(|n| Component {
            id: format!("part{n}"),
            name: Some(format!("Part {n}")),
            ingredients: vec![],
            steps: vec![],
            ..only.clone()
        })
        .collect();
    assert_eq!(
        line(&validate(&doc), Code::TooMany),
        "components: [too_many] a recipe has at most 8 components; this one has 9"
    );

    let mut doc = piccata();
    doc.title = " ".to_owned();
    assert_eq!(
        line(&validate(&doc), Code::BadValue),
        "title: [bad_value] title is required"
    );
}

#[test]
fn problems_serialize_with_their_code_and_level() {
    let mut doc = piccata();
    doc.components[0].steps.clear();
    let report = validate(&doc);
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::json!({
            "errors": [{
                "code": "no_steps",
                "path": "components.piccata",
                "message": "a component needs at least one step",
                "level": "error"
            }],
            "warnings": []
        })
    );
}

#[test]
fn a_source_url_must_be_a_web_address() {
    use omni_recipe_format::is_web_url;
    for (url, ok) in [
        ("https://example.com/recipes/piccata", true),
        ("HTTP://Example.com", true),
        ("https://[::1]/x", true),
        ("javascript:alert(1)", false),
        ("data:text/html,<script>", false),
        ("ftp://example.com/x", false),
        ("//example.com/x", false),
        ("/recipes/piccata", false),
        ("https://", false),
        ("https:///x", false),
        ("https://example.com/a b", false),
        ("https://example.com/\n", false),
        ("", false),
    ] {
        assert_eq!(is_web_url(url), ok, "{url:?}");
        let mut doc = piccata();
        doc.source.url = Some(url.to_owned());
        let report = validate(&doc);
        assert_eq!(report.is_valid(), ok, "{url:?}");
        if !ok {
            assert_eq!(
                line(&report, Code::BadValue),
                "source.url: [bad_value] source.url must be an http:// or https:// address Hint: put anything else (a book page, a person) in source.detail"
            );
        }
    }
}
