//! A step that runs in batches: the count follows the scale (or the yield
//! over what one batch holds), and its time counts once per batch in the
//! derived totals, the timeline and the chart.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_recipe_format::{
    Code, Index, Marks, RecipeDoc, Repeat, Step, chart_ascii, chart_blocks, derived_time,
    derived_time_at, render_text, scaled, timeline_at, validate,
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

/// Cookies baked 12 to a sheet, one sheet at a time.
fn sheets() -> RecipeDoc {
    let mut doc = cookies();
    let bake = step_mut(&mut doc, "bake");
    bake.repeat = Some(Repeat {
        count: None,
        capacity: Some(12.0),
    });
    bake.text = "Bake {scoop} in {oven|the hot oven} in {batches}, {time} each.".to_owned();
    doc
}

fn text_at(doc: &RecipeDoc, id: &str, factor: f64) -> String {
    let ix = Index::build(doc);
    render_text(&ix, ix.step(id).unwrap(), factor)
}

#[test]
fn a_capacity_sets_the_batches_from_the_scaled_yield() {
    let doc = sheets();
    assert!(validate(&doc).is_valid(), "{:?}", validate(&doc).errors);
    // 28 cookies at 12 a sheet.
    assert_eq!(
        text_at(&doc, "bake", 1.0),
        "Bake the scooped dough in the hot oven in 3 batches, 9–11 minutes each."
    );
    assert_eq!(
        text_at(&doc, "bake", 0.4),
        "Bake the scooped dough in the hot oven in 1 batch, 9–11 minutes each."
    );
    let chart: String = chart_blocks(&doc, 2.0)
        .iter()
        .map(|block| chart_ascii(block, &Marks::new()))
        .collect();
    // Cells wrap their labels; the count is on the bake cell.
    assert!(chart.contains("×5"), "{chart}");
}

#[test]
fn batches_multiply_the_step_time_in_the_totals_and_the_timeline() {
    let plain = cookies();
    let doc = sheets();
    let row = |doc: &RecipeDoc, factor: f64| {
        timeline_at(doc, factor)
            .into_iter()
            .find(|row| row.step == "bake")
            .unwrap()
    };
    let once = row(&plain, 1.0);
    assert_eq!(once.end - once.start, 11);
    let three = row(&doc, 1.0);
    assert_eq!(three.end - three.start, 33);
    assert_eq!(three.label, "bake 375°F 9–11 min ×3");
    let five = row(&doc, 2.0);
    assert_eq!(five.end - five.start, 55);
    let base = derived_time(&plain);
    let batched = derived_time(&doc);
    // 22 more minutes, rounded up to five.
    assert_eq!(
        batched.total_min,
        base.total_min + 20,
        "{base:?} {batched:?}"
    );
    assert_eq!(derived_time_at(&plain, 2.0), base, "no batches, no change");
    assert!(derived_time_at(&doc, 2.0).total_min > batched.total_min);
}

#[test]
fn a_fixed_count_follows_the_scale_and_rescale_moves_it() {
    let mut doc = cookies();
    step_mut(&mut doc, "bake").repeat = Some(Repeat {
        count: Some(3),
        capacity: None,
    });
    step_mut(&mut doc, "bake").text =
        "Bake {scoop} in {oven|the hot oven} in {batches}.".to_owned();
    assert!(text_at(&doc, "bake", 1.5).ends_with("in 5 batches."));
    assert!(text_at(&doc, "bake", 0.5).ends_with("in 2 batches."));
    let doubled = scaled(&doc, 2.0);
    let ix = Index::build(&doubled);
    assert_eq!(
        ix.step("bake").unwrap().repeat.as_ref().unwrap().count,
        Some(6)
    );
}

#[test]
fn repeat_is_checked() {
    let mut doc = cookies();
    step_mut(&mut doc, "bake").repeat = Some(Repeat {
        count: Some(2),
        capacity: Some(12.0),
    });
    step_mut(&mut doc, "cool").repeat = Some(Repeat {
        count: Some(0),
        capacity: None,
    });
    step_mut(&mut doc, "scoop").repeat = Some(Repeat {
        count: None,
        capacity: Some(-1.0),
    });
    step_mut(&mut doc, "fold").text =
        "Fold {chips} and, if using, {walnuts} into {combine} in {batches}.".to_owned();
    let found: Vec<String> = validate(&doc)
        .errors
        .iter()
        .map(|problem| format!("{} [{}]", problem.path, problem.code))
        .collect();
    assert_eq!(
        found,
        [
            "steps.scoop.repeat [bad_value]",
            "steps.bake.repeat [bad_value]",
            "steps.cool.repeat [bad_value]",
            "steps.fold.text [placeholder_unset]",
        ]
    );
    assert!(
        validate(&doc)
            .errors
            .iter()
            .all(|p| p.code != Code::TooMany)
    );
}
