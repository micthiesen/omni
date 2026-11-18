//! What cook mode and the app read from the graph: readiness, what to do
//! while a passive step runs, per-step inputs, the gather list, equipment,
//! text segments, the structured list view and search.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_recipe_format::{
    CellKind, DerivedTime, Index, Kind, Mark, RecipeDoc, Segment, chart_blocks, cook_marks,
    depends_on, derived_time, equipment, gather_list, list_view, matches, meanwhile, ready_steps,
    render_step_segments, render_text, search_text, step_inputs, step_sources, timeline, tokens,
};

fn piccata() -> RecipeDoc {
    serde_json::from_str(include_str!("golden/chicken-piccata.json")).unwrap()
}

fn lasagne() -> RecipeDoc {
    serde_json::from_str(include_str!("golden/lasagne-bolognese.json")).unwrap()
}

const NOTHING: [&str; 0] = [];

#[test]
fn a_step_is_ready_when_the_steps_producing_its_inputs_are_done() {
    let doc = lasagne();
    assert_eq!(ready_steps(&doc, &NOTHING), ["soffritto", "roux", "oven"]);
    assert_eq!(
        ready_steps(&doc, &["soffritto", "roux"]),
        ["brown", "thicken", "oven"]
    );
    let ragu_and_bechamel = [
        "soffritto",
        "brown",
        "reduce",
        "milkdown",
        "simmer",
        "roux",
        "thicken",
        "season",
    ];
    assert_eq!(ready_steps(&doc, &ragu_and_bechamel), ["oven", "layer"]);

    let marks = cook_marks(&doc, &["soffritto"]);
    assert_eq!(marks.get("soffritto"), Some(&Mark::Done));
    assert_eq!(marks.get("brown"), Some(&Mark::Ready));
    assert_eq!(marks.get("reduce"), None);
}

#[test]
fn a_by_product_waits_for_the_step_that_makes_it() {
    let doc = piccata();
    assert_eq!(ready_steps(&doc, &["dredge"]), ["sear"]);
    assert_eq!(ready_steps(&doc, &["dredge", "sear"]), ["deglaze"]);
    let ix = Index::build(&doc);
    assert_eq!(
        step_sources(&ix, ix.step("serve").unwrap()),
        ["sear", "mount"]
    );
    assert_eq!(step_sources(&ix, ix.step("deglaze").unwrap()), ["sear"]);
    assert!(depends_on(&ix, "serve", "dredge"));
    assert!(depends_on(&ix, "deglaze", "sear"));
    assert!(!depends_on(&ix, "sear", "deglaze"));
}

#[test]
fn while_the_ragu_simmers_the_bechamel_can_be_made() {
    let doc = lasagne();
    let done = ["soffritto", "brown", "reduce", "milkdown"];
    assert_eq!(meanwhile(&doc, "simmer", &done), ["roux", "oven"]);
    let later = [
        "soffritto",
        "brown",
        "reduce",
        "milkdown",
        "roux",
        "thicken",
    ];
    assert_eq!(meanwhile(&doc, "simmer", &later), ["season", "oven"]);
    let ix = Index::build(&doc);
    assert!(depends_on(&ix, "layer", "simmer"));
    assert!(!depends_on(&ix, "season", "simmer"));
}

#[test]
fn step_inputs_carry_the_amount_for_that_use_and_the_chart_key() {
    let doc = piccata();
    let ix = Index::build(&doc);
    let inputs = step_inputs(&ix, ix.step("simmer").unwrap(), 1.5);
    let shown: Vec<(&str, &str, Option<Kind>, bool)> = inputs
        .iter()
        .map(|i| (i.key.as_str(), i.label.as_str(), i.target, i.optional))
        .collect();
    assert_eq!(
        shown,
        [
            ("simmer/deglaze", "wine reduction", Some(Kind::Step), false),
            (
                "simmer/stock",
                "270 ml chicken stock",
                Some(Kind::Ingredient),
                false
            ),
            (
                "simmer/lemon:juice",
                "juice of 1½ lemons",
                Some(Kind::Ingredient),
                false
            ),
            (
                "simmer/capers",
                "3 tbsp capers, drained (optional)",
                Some(Kind::Ingredient),
                true
            ),
        ]
    );

    let blocks = chart_blocks(&doc, 1.5);
    for (_, step) in ix.steps() {
        for input in step_inputs(&ix, step, 1.5) {
            let cell = blocks[0].cells.iter().find(|cell| cell.key == input.key);
            match input.target {
                Some(Kind::Ingredient) => {
                    let cell = cell.unwrap();
                    assert_eq!(cell.kind, CellKind::Leaf);
                    assert_eq!(cell.label, input.label);
                }
                Some(Kind::Byproduct) => assert_eq!(cell.unwrap().kind, CellKind::Ref),
                _ => assert!(cell.is_none()),
            }
        }
    }
}

#[test]
fn the_gather_list_is_the_ingredient_list_grouped_by_component() {
    let doc = lasagne();
    let groups = gather_list(&doc, 1.0);
    let headings: Vec<_> = groups.iter().map(|g| g.heading.as_deref()).collect();
    assert_eq!(headings, [Some("Ragù"), Some("Béchamel"), Some("Assembly")]);
    assert_eq!(
        groups[0].make_ahead.as_deref(),
        Some("make ahead: keeps 3 days in the fridge")
    );
    assert_eq!(groups[2].from_above, ["Ragù", "Béchamel"]);
    let assembly: Vec<(&str, &str)> = groups[2]
        .lines
        .iter()
        .map(|line| (line.id.as_str(), line.text.as_str()))
        .collect();
    assert_eq!(
        assembly,
        [
            ("dishbutter", "1 tbsp butter (for the dish)"),
            ("sheets", "250 g fresh lasagne sheets"),
            ("parm", "100 g Parmigiano Reggiano, finely grated, divided"),
        ]
    );
    let doubled = gather_list(&doc, 2.0);
    assert_eq!(doubled[2].lines[0].text, "1 tbsp butter (for the dish)");
    assert_eq!(doubled[2].lines[1].text, "500 g fresh lasagne sheets");

    let single = gather_list(&piccata(), 1.0);
    assert_eq!(single.len(), 1);
    assert_eq!(single[0].heading, None);
    assert!(single[0].lines[9].optional);
}

#[test]
fn equipment_is_listed_once_in_order_of_first_use() {
    assert_eq!(
        equipment(&lasagne()),
        ["Dutch oven", "23 x 33 cm baking dish"]
    );
    let mut doc = piccata();
    doc.components[0].steps[4].equipment = vec![" 12-inch skillet ".to_owned(), "whisk".to_owned()];
    assert_eq!(equipment(&doc), ["12-inch skillet", "whisk"]);
}

#[test]
fn step_text_splits_into_plain_words_inputs_time_and_temperature() {
    let doc = piccata();
    let ix = Index::build(&doc);
    let step = ix.step("deglaze").unwrap();
    let text = |text: &str| Segment::Text {
        text: text.to_owned(),
    };
    assert_eq!(
        render_step_segments(&ix, step, 1.0),
        [
            text("Pour "),
            Segment::Input {
                text: "120 ml dry white wine".to_owned(),
                r#ref: "wine".to_owned(),
                key: "deglaze/wine".to_owned(),
                target: Some(Kind::Ingredient),
            },
            text(" into "),
            Segment::Input {
                text: "the hot pan".to_owned(),
                r#ref: "fond".to_owned(),
                key: "deglaze/fond".to_owned(),
                target: Some(Kind::Byproduct),
            },
            text(" and scrape up the browned bits. Boil until reduced by half, about "),
            Segment::Time {
                text: "2 minutes".to_owned()
            },
            text("."),
        ]
    );

    let lasagne = lasagne();
    let ix = Index::build(&lasagne);
    let oven = render_step_segments(&ix, ix.step("oven").unwrap(), 1.0);
    assert_eq!(
        oven[1],
        Segment::Temp {
            text: "375°F (190°C)".to_owned()
        }
    );
    assert_eq!(
        serde_json::to_value(&oven[1]).unwrap(),
        serde_json::json!({"kind": "temp", "text": "375°F (190°C)"})
    );
    for (_, step) in ix.steps() {
        let joined: String = render_step_segments(&ix, step, 2.0)
            .iter()
            .map(Segment::text)
            .collect();
        assert_eq!(joined, render_text(&ix, step, 2.0));
    }
}

#[test]
fn a_placeholder_that_cannot_be_filled_stays_as_written() {
    let mut doc = piccata();
    let mount = &mut doc.components[0].steps[4];
    mount.text = "Swirl {buter} into {simmer} for {time} at {temp}, {nope|then serve}.".to_owned();
    mount.time = None;
    let ix = Index::build(&doc);
    assert_eq!(
        render_text(&ix, ix.step("mount").unwrap(), 1.0),
        "Swirl {buter} into the sauce base for {time} at {temp}, then serve."
    );
}

#[test]
fn the_list_view_is_available_as_data() {
    let doc = lasagne();
    let view = list_view(&doc, 1.5);
    assert_eq!(view.title, "Lasagne alla Bolognese");
    assert_eq!(view.makes, "Makes 12 servings");
    assert_eq!(view.scaled.as_deref(), Some("1.5"));
    assert_eq!((view.active_min, view.total_min), (65, 265));
    assert_eq!(view.time, "65 min hands-on, 4 h 25 min start to finish");
    assert_eq!(
        view.meta(),
        "Makes 12 servings (scaled ×1.5) · 65 min hands-on, 4 h 25 min start to finish"
    );
    assert_eq!(view.groups.len(), 3);
    assert_eq!(view.steps.len(), 14);
    let top = &view.steps[10];
    assert_eq!(
        (top.id.as_str(), top.number, top.component.as_str()),
        ("top", 11, "assembly")
    );
    assert_eq!(
        top.text,
        "Spread the remaining béchamel over the top and finish with the remaining 45 g Parmigiano Reggiano."
    );
    assert_eq!(list_view(&doc, 1.0).scaled, None);
}

#[test]
fn derived_time_rounds_the_schedule_up_to_five_minutes() {
    assert_eq!(
        derived_time(&lasagne()),
        DerivedTime {
            active_min: 65,
            total_min: 265
        }
    );
    let rows = timeline(&lasagne());
    let oven = rows.iter().find(|row| row.step == "oven").unwrap();
    assert_eq!((oven.start, oven.end), (187, 202));
    assert_eq!(oven.component_name.as_deref(), Some("Assembly"));
    assert_eq!(oven.label, "heat oven 375°F 15 min");
}

#[test]
fn search_text_holds_the_words_a_recipe_is_found_by() {
    let doc = lasagne();
    let text = search_text(&doc);
    assert!(text.starts_with("Lasagne alla Bolognese\nSlow-cooked ragù"));
    assert!(text.contains("\nHouse recipe\n"));
    assert!(text.contains("\nParmigiano Reggiano\n"));
    assert!(
        text.contains("Pour dry white wine over browned meat and boil until the pan is nearly dry")
    );
    assert!(!text.contains('{'));

    let haystack = tokens(&text);
    assert!(matches(&tokens("ragu bechamel"), &haystack));
    assert!(matches(&tokens("LASAG parm"), &haystack));
    assert!(!matches(&tokens("lasagne chicken"), &haystack));
}
