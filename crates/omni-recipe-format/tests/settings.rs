//! Settings a step works to as data: sizes (thickness, cutter, pan) and
//! heat in stages, rendered through placeholders and checked.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_recipe_format::{Code, Index, RecipeDoc, Size, SizeKind, Step, render_text, validate};

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
