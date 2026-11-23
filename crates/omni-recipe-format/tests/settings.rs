//! Settings a step works to as data: sizes (thickness, cutter, pan) and
//! heat in stages, rendered through placeholders and checked.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_recipe_format::{
    Code, HeatLevel, HeatStage, Index, RecipeDoc, Size, SizeKind, Step, Temp, chart_label,
    render_text, validate,
};

pub(crate) fn cookies() -> RecipeDoc {
    serde_json::from_str(include_str!("golden/chocolate-chip-cookies.json")).unwrap()
}

pub(crate) fn step_mut<'a>(doc: &'a mut RecipeDoc, id: &str) -> &'a mut Step {
    doc.components
        .iter_mut()
        .flat_map(|component| &mut component.steps)
        .find(|step| step.id == id)
        .unwrap()
}

pub(crate) fn text_at(doc: &RecipeDoc, id: &str, factor: f64) -> String {
    let ix = Index::build(doc);
    render_text(&ix, ix.step(id).unwrap(), factor)
}

fn size(kind: SizeKind, value: f64, max: Option<f64>, by: Option<f64>, unit: &str) -> Size {
    Size {
        kind,
        value,
        max,
        by,
        unit: unit.to_owned(),
    }
}

fn errors_at(doc: &RecipeDoc, path_start: &str) -> Vec<String> {
    validate(doc)
        .errors
        .iter()
        .filter(|problem| problem.path.starts_with(path_start))
        .map(|problem| problem.line())
        .collect()
}

#[test]
fn sizes_render_in_both_systems_and_a_pan_says_when_it_is_for_the_unscaled_recipe() {
    let mut doc = cookies();
    let scoop = step_mut(&mut doc, "scoop");
    scoop.text = "Roll {fold} {size:thickness} thick and cut rounds with a {size:diameter} cutter."
        .to_owned();
    scoop.size = vec![
        size(SizeKind::Thickness, 3.0, Some(4.0), None, "mm"),
        size(SizeKind::Diameter, 7.0, None, None, "cm"),
    ];
    let oven = step_mut(&mut doc, "oven");
    oven.text = "Heat the oven to {temp} and line a {size} tin.".to_owned();
    oven.size = vec![size(SizeKind::Pan, 23.0, None, Some(33.0), "cm")];
    assert!(validate(&doc).is_valid(), "{:?}", validate(&doc).errors);
    assert_eq!(
        text_at(&doc, "scoop", 1.0),
        "Roll the cookie dough 3–4 mm (⅛ in) thick and cut rounds with a 7 cm (3 in) cutter."
    );
    assert_eq!(
        text_at(&doc, "scoop", 2.0),
        "Roll the cookie dough 3–4 mm (⅛ in) thick and cut rounds with a 7 cm (3 in) cutter."
    );
    assert_eq!(
        text_at(&doc, "oven", 1.0),
        "Heat the oven to 375°F (190°C) and line a 23 × 33 cm (9 × 13 in) tin."
    );
    assert_eq!(
        text_at(&doc, "oven", 2.0),
        "Heat the oven to 375°F (190°C) and line a 23 × 33 cm (9 × 13 in; for the unscaled recipe) tin."
    );
}

#[test]
fn a_size_placeholder_needs_a_size_of_that_kind() {
    let mut doc = cookies();
    let scoop = step_mut(&mut doc, "scoop");
    scoop.text = "Roll {fold} {size:thickness} thick in a {size:pan} tin.".to_owned();
    scoop.size = vec![size(SizeKind::Thickness, 3.0, None, None, "mm")];
    let found = errors_at(&doc, "steps.scoop.text");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("[placeholder_unset] {size:pan}"),
        "{}",
        found[0]
    );
}

#[test]
fn sizes_are_checked() {
    let mut doc = cookies();
    step_mut(&mut doc, "scoop").size = vec![
        size(SizeKind::Thickness, 3.0, Some(2.0), Some(4.0), "mm"),
        size(SizeKind::Pan, 0.0, None, None, "ft"),
    ];
    let found = errors_at(&doc, "steps.scoop.size");
    assert_eq!(
        found,
        [
            "steps.scoop.size.0: [bad_value] max must be a number at least value",
            "steps.scoop.size.0: [bad_value] a thickness has no second side (by) Hint: by is for a pan or a length: 23 by 33 cm",
            "steps.scoop.size.1: [bad_value] value must be a number above 0",
            "steps.scoop.size.1: [bad_value] size unit \"ft\" must be \"mm\", \"cm\" or \"in\"",
        ]
    );
    step_mut(&mut doc, "scoop").size = vec![size(SizeKind::Length, 5.0, None, None, "cm"); 5];
    assert!(
        validate(&doc)
            .errors
            .iter()
            .any(|p| p.code == Code::TooMany)
    );
}

fn level(level: HeatLevel, label: Option<&str>) -> HeatStage {
    HeatStage {
        temp: None,
        level: Some(level),
        label: label.map(str::to_owned),
    }
}

fn oven(value: f64, label: Option<&str>) -> HeatStage {
    HeatStage {
        temp: Some(Temp {
            value,
            unit: "C".to_owned(),
        }),
        level: None,
        label: label.map(str::to_owned),
    }
}

#[test]
fn heat_in_stages_renders_in_text_and_on_the_chart() {
    let mut doc = cookies();
    let bake = step_mut(&mut doc, "bake");
    bake.text = "Bake {scoop} in {oven|the hot oven} at {heat}, about {time}.".to_owned();
    bake.temp = None;
    bake.heat = vec![oven(200.0, Some("bake")), oven(150.0, Some("dry"))];
    let cool = step_mut(&mut doc, "cool");
    cool.text = "Sear {bake} over {heat:sear} then finish on {heat:finish}, {time}.".to_owned();
    cool.heat = vec![
        level(HeatLevel::High, Some("sear")),
        level(HeatLevel::MediumLow, Some("finish")),
    ];
    assert!(validate(&doc).is_valid(), "{:?}", validate(&doc).errors);
    assert_eq!(
        text_at(&doc, "bake", 1.0),
        "Bake the scooped dough in the hot oven at 200°C (390°F), then 150°C (300°F), about 9–11 minutes."
    );
    assert_eq!(
        text_at(&doc, "cool", 1.0),
        "Sear the baked cookies over high heat then finish on medium-low heat, 5 minutes."
    );
    let ix = Index::build(&doc);
    assert_eq!(
        chart_label(ix.step("bake").unwrap()),
        "bake 200→150°C 9–11 min"
    );
    assert_eq!(
        chart_label(ix.step("cool").unwrap()),
        "cool high→med-low 5 min"
    );
}

#[test]
fn heat_stages_are_checked() {
    let mut doc = cookies();
    let bake = step_mut(&mut doc, "bake");
    bake.temp = Some(Temp {
        value: 375.0,
        unit: "F".to_owned(),
    });
    bake.heat = vec![
        HeatStage {
            temp: None,
            level: None,
            label: Some("sear".to_owned()),
        },
        level(HeatLevel::High, Some("sear")),
        level(HeatLevel::Low, Some("a:b")),
    ];
    bake.text = "Bake {scoop} in {oven|the hot oven} on {heat:dry} for {time}.".to_owned();
    let found: Vec<String> = validate(&doc)
        .errors
        .iter()
        .filter(|problem| problem.path.starts_with("steps.bake"))
        .map(|problem| format!("{} [{}]", problem.path, problem.code))
        .collect();
    assert_eq!(
        found,
        [
            "steps.bake.heat [bad_value]",
            "steps.bake.heat.0 [bad_value]",
            "steps.bake.heat.1 [duplicate_id]",
            "steps.bake.heat.2 [bad_value]",
            "steps.bake.text [placeholder_unset]",
        ]
    );
    step_mut(&mut doc, "bake").heat = vec![level(HeatLevel::High, None); 5];
    assert!(
        validate(&doc)
            .errors
            .iter()
            .any(|p| p.code == Code::TooMany)
    );
}
