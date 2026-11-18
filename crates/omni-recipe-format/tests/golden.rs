//! The Rust format reproduces the design prototype byte for byte: the four
//! sample recipes render to the prototype's committed output, and its report
//! (scaling, cook mode, the broken draft, edit operations) reads the same.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_recipe_format::{
    Index, Input, Marks, Op, RecipeDoc, Step, apply_ops, chart_ascii, chart_blocks, chart_html,
    cook_marks, diff_summary, render_list, render_text, render_timeline, validate,
};
use serde_json::json;

const REPORT: &str = include_str!("golden/report.txt");

struct Sample {
    name: &'static str,
    json: &'static str,
    list: &'static str,
    chart: &'static str,
    html: &'static str,
}

macro_rules! sample {
    ($name:literal) => {
        Sample {
            name: $name,
            json: include_str!(concat!("golden/", $name, ".json")),
            list: include_str!(concat!("golden/", $name, ".list.md")),
            chart: include_str!(concat!("golden/", $name, ".chart.txt")),
            html: include_str!(concat!("golden/", $name, ".chart.html")),
        }
    };
}

const SAMPLES: [Sample; 4] = [
    sample!("chocolate-chip-cookies"),
    sample!("chicken-piccata"),
    sample!("lasagne-bolognese"),
    sample!("enchiladas"),
];

/// The stylesheet of the prototype's chart pages.
const PAGE_CSS: &str = "
body { font: 16px/1.4 ui-sans-serif, system-ui; margin: 2rem; color: #1d1b16; background: #faf7f0; }
table.chart { border-collapse: collapse; margin: 0 0 2rem; }
table.chart td { border: 1px solid #b9b2a3; padding: .35rem .6rem; vertical-align: middle; }
table.chart td.step { text-align: center; background: #f1ecdf; font-weight: 600; }
table.chart td.setup, table.chart td.ref { font-style: italic; color: #6b6558; }
table.chart td.optional { color: #6b6558; }
";

fn doc(sample: &Sample) -> RecipeDoc {
    serde_json::from_str(sample.json).unwrap()
}

fn piccata() -> RecipeDoc {
    doc(&SAMPLES[1])
}

/// The body of one section of the prototype's report.
fn section(title: &str) -> &'static str {
    let header = format!("======== {title} ========\n\n");
    let start = REPORT.find(&header).unwrap() + header.len();
    let rest = &REPORT[start..];
    match rest.find("\n\n======== ") {
        Some(end) => &rest[..end],
        None => rest.strip_suffix('\n').unwrap(),
    }
}

fn step_mut<'a>(doc: &'a mut RecipeDoc, id: &str) -> &'a mut Step {
    doc.components
        .iter_mut()
        .flat_map(|component| &mut component.steps)
        .find(|step| step.id == id)
        .unwrap()
}

fn sized(id: &str, qty: f64, unit: &str) -> Input {
    Input {
        qty: Some(qty),
        unit: Some(unit.to_owned()),
        ..Input::all(id)
    }
}

fn problem_lines(doc: &RecipeDoc) -> Vec<String> {
    let report = validate(doc);
    report
        .errors
        .iter()
        .chain(&report.warnings)
        .map(|problem| problem.line())
        .collect()
}

#[test]
fn sample_recipes_validate_clean() {
    for sample in &SAMPLES {
        let report = validate(&doc(sample));
        assert_eq!(report.errors, [], "{}", sample.name);
        assert_eq!(report.warnings, [], "{}", sample.name);
        assert_eq!(
            section(&format!("{}: validation", sample.name)),
            "0 errors, 0 warnings"
        );
    }
}

#[test]
fn sample_recipes_serialize_back_to_the_same_json() {
    for sample in &SAMPLES {
        let stored: serde_json::Value = serde_json::from_str(sample.json).unwrap();
        let written = serde_json::to_value(doc(sample)).unwrap();
        assert_eq!(written, stored, "{}", sample.name);
    }
}

#[test]
fn list_views_match_the_prototype() {
    for sample in &SAMPLES {
        assert_eq!(
            render_list(&doc(sample), 1.0),
            sample.list,
            "{}",
            sample.name
        );
    }
}

#[test]
fn ascii_charts_match_the_prototype() {
    for sample in &SAMPLES {
        let charts: Vec<String> = chart_blocks(&doc(sample), 1.0)
            .iter()
            .map(|block| chart_ascii(block, &Marks::new()))
            .collect();
        assert_eq!(
            format!("{}\n", charts.join("\n\n")),
            sample.chart,
            "{}",
            sample.name
        );
    }
}

#[test]
fn html_charts_match_the_prototype() {
    for sample in &SAMPLES {
        let doc = doc(sample);
        let tables: Vec<String> = chart_blocks(&doc, 1.0).iter().map(chart_html).collect();
        let page = format!(
            "<!doctype html><meta charset=utf-8><title>{title}</title><style>{PAGE_CSS}</style><h2>{title}</h2>{}",
            tables.join("\n"),
            title = doc.title,
        );
        assert_eq!(page, sample.html, "{}", sample.name);
    }
    assert_eq!(
        chart_html(&chart_blocks(&piccata(), 1.0)[0]),
        section("piccata chart HTML")
    );
}

#[test]
fn timelines_match_the_prototype() {
    for sample in &SAMPLES {
        assert_eq!(
            render_timeline(&doc(sample)),
            section(&format!("{}: timeline", sample.name)),
            "{}",
            sample.name
        );
    }
}

#[test]
fn scaling_by_one_and_a_half_matches_the_prototype() {
    let doc = piccata();
    assert_eq!(
        render_list(&doc, 1.5),
        section("piccata scaled x1.5: list view")
    );
    assert_eq!(
        chart_ascii(&chart_blocks(&doc, 1.5)[0], &Marks::new()),
        section("piccata scaled x1.5: chart")
    );
}

#[test]
fn cook_mode_marks_done_and_ready_cells() {
    let doc = piccata();
    let marks = cook_marks(&doc, &["dredge", "sear"]);
    assert_eq!(
        chart_ascii(&chart_blocks(&doc, 1.0)[0], &marks),
        section("piccata in cook mode: dredge and sear done")
    );
}

#[test]
fn a_broken_draft_lists_every_problem() {
    let mut broken = piccata();
    step_mut(&mut broken, "sear").inputs[2] = sized("buter", 2.0, "tbsp");
    let mount = step_mut(&mut broken, "mount");
    mount.inputs[1] = sized("butter", 3.0, "tbsp");
    mount.text =
        "Take {simmer} off the heat and swirl in 3 tbsp cold butter. Taste and adjust with {salt}."
            .to_owned();
    let simmer = step_mut(&mut broken, "simmer");
    simmer.inputs.retain(|input| input.r#ref != "capers");
    simmer.text = "Add {stock} and {lemon} to {deglaze}. Simmer {time}.".to_owned();
    let deglaze = step_mut(&mut broken, "deglaze");
    deglaze.inputs[0] = Input::all("mount");
    deglaze.text =
        "Pour {wine} into {mount|the hot pan} and scrape up the browned bits.".to_owned();

    let report = validate(&broken);
    assert_eq!(report.errors.len(), 6);
    assert_eq!(report.warnings.len(), 2);
    assert!(!report.is_valid());
    assert_eq!(report.refusal_text(), section("broken draft: validation"));
}

#[test]
fn the_edit_example_applies_and_reads_back() {
    let body = section("edit ops on piccata");
    let (ops_json, expected) = body.split_once("\nvalidation after ops:").unwrap();
    let ops: Vec<Op> = serde_json::from_str(ops_json).unwrap();
    assert_eq!(ops.len(), 8);

    let piccata = piccata();
    let edited = apply_ops(&piccata, &ops).unwrap();
    let report = validate(&edited);
    let mut lines = vec![format!(
        " {} errors, {} warnings",
        report.errors.len(),
        report.warnings.len()
    )];
    lines.extend(problem_lines(&edited));
    lines.push("diff:".to_owned());
    lines.extend(diff_summary(&piccata, &edited));
    let ix = Index::build(&edited);
    for id in ["deglaze", "simmer"] {
        let step = ix.step(id).unwrap();
        lines.push(format!("{id}: {}", render_text(&ix, step, 1.0)));
    }
    assert_eq!(lines.join("\n"), expected);
    assert!(expected.starts_with(" 0 errors, 0 warnings\ndiff:\n− ingredient wine\n"));
}

#[test]
fn rename_unset_move_and_add_component_match_the_prototype() {
    let piccata = piccata();
    let ops: Vec<Op> = serde_json::from_value(json!([
        {"op": "rename", "target": "mount", "to": "finish"},
        {"op": "rename", "target": "fond", "to": "drippings"},
        {"op": "set", "target": "flour", "fields": {"unit": "grams"}, "unset": ["note", "alt"]},
        {"op": "move", "target": "lemon"},
        {"op": "addComponent", "component": {
            "id": "rice", "name": "Rice",
            "ingredients": [{"id": "basmati", "qty": 300, "unit": "g", "name": "basmati rice"}],
            "steps": [{"id": "boil", "action": "boil", "output": "rice",
                       "inputs": [{"ref": "basmati"}],
                       "time": {"min": 12, "attention": "passive"},
                       "text": "Boil {basmati} for {time}, then drain."}]}},
        {"op": "set", "target": "piccata", "fields": {"name": "Piccata"}},
        {"op": "setInput", "step": "serve", "ref": "rice"},
        {"op": "replaceText", "target": "serve", "find": "serve with {lemon}.",
         "replace": "serve with {lemon} over {rice}."},
    ]))
    .unwrap();
    let more = apply_ops(&piccata, &ops).unwrap();
    let report = validate(&more);
    let mut lines = vec![format!(
        "validation: {} errors, {} warnings",
        report.errors.len(),
        report.warnings.len()
    )];
    lines.extend(problem_lines(&more));
    let main = &more.components[1];
    for step in &main.steps {
        if ["deglaze", "finish", "serve"].contains(&step.id.as_str()) {
            lines.push(format!("{}: {}", step.id, step.text));
        }
    }
    let ids = |ids: Vec<&str>| ids.join(", ");
    lines.push(format!(
        "ingredient order: {}",
        ids(main.ingredients.iter().map(|i| i.id.as_str()).collect())
    ));
    lines.push(format!(
        "components: {}",
        ids(more.components.iter().map(|c| c.id.as_str()).collect())
    ));

    let failed: Vec<Op> = serde_json::from_value(json!([
        {"op": "remove", "target": "capers"},
        {"op": "set", "target": "caper", "fields": {"qty": 3}},
    ]))
    .unwrap();
    let error = apply_ops(&piccata, &failed).unwrap_err();
    assert_eq!((error.index, error.op), (1, "set"));
    lines.push(format!("op error: {error}"));

    let removed = apply_ops(&piccata, &failed[..1]).unwrap();
    lines.push("removing an ingredient that a step still uses:".to_owned());
    lines.extend(validate(&removed).errors.iter().map(|p| p.line()));

    assert_eq!(
        lines.join("\n"),
        section("more ops: rename, unset, move, addComponent, unknown unit")
    );
}
