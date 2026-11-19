//! The household's standing rules: the diet every write is checked against
//! and the measuring guideline, as agents meet them through the tools.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Harness, PICCATA, doc};
use serde_json::{Value, json};

fn vegetarian() -> Value {
    serde_json::from_str(omni_recipes::guide::EXAMPLE).unwrap()
}

fn codes(problems: &Value) -> Vec<&str> {
    problems
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| problem["code"].as_str().unwrap())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_guide_tells_every_agent_the_diet_and_how_to_measure() {
    let h = Harness::new().await;
    let guide = h.ok("recipes_guide", json!({})).await;
    let diet = guide["household"]["diet"].as_str().unwrap();
    assert!(diet.starts_with("Bivalve vegetarian"));
    for word in [
        "mussels",
        "fish sauce",
        "Worcestershire",
        "gelatin",
        "keepDietConflicts",
    ] {
        assert!(diet.contains(word), "{word}");
    }
    let measuring = guide["household"]["measuring"].as_str().unwrap();
    assert!(measuring.contains("kitchen scale") && measuring.contains("awkward_amount"));
    let text = guide["guide"].as_str().unwrap();
    assert!(text.contains("## The household"));
    assert!(text.contains("\"title\": \"Cauliflower Piccata\""));
}

#[tokio::test(start_paused = true)]
async fn a_meat_recipe_is_refused_naming_each_ingredient() {
    let h = Harness::new().await;
    let refused = h
        .err("recipes_create", json!({ "document": doc(PICCATA) }))
        .await;
    assert!(
        refused.contains("2 problems; nothing was saved"),
        "{refused}"
    );
    assert!(refused.contains("ingredients.chicken: [diet_conflict]"));
    assert!(refused.contains("ingredients.stock: [diet_conflict]"));
    assert!(refused.contains("use vegetable stock"));

    let dry = h
        .ok(
            "recipes_create",
            json!({ "document": doc(PICCATA), "dryRun": true }),
        )
        .await;
    assert_eq!(dry["saved"], false);
    assert_eq!(codes(&dry["problems"]), ["diet_conflict", "diet_conflict"]);
    assert_eq!(h.ok("recipes_search", json!({})).await["total"], 0);
}

#[tokio::test(start_paused = true)]
async fn the_owner_can_keep_an_ingredient_and_later_edits_are_not_refused() {
    let h = Harness::new().await;
    let kept = h
        .ok(
            "recipes_create",
            json!({ "document": doc(PICCATA), "keepDietConflicts": true }),
        )
        .await;
    assert_eq!(kept["saved"], true);
    assert_eq!(codes(&kept["warnings"]), ["diet_conflict", "diet_conflict"]);
    let id = kept["id"].as_str().unwrap();

    // An edit that leaves the kept ingredients alone needs no flag.
    let edited = h
        .ok(
            "recipes_edit",
            json!({
                "id": id, "expectedRevision": 1, "summary": "More capers",
                "ops": [{ "op": "set", "target": "capers", "fields": { "qty": 3 } }],
            }),
        )
        .await;
    assert_eq!(edited["saved"], true);

    // A new one is refused, and the recipe is unchanged.
    let anchovies = json!({
        "id": id, "expectedRevision": 2, "summary": "Anchovies",
        "ops": [{ "op": "set", "target": "capers", "fields": { "name": "anchovy fillets" } }],
    });
    let refused = h.err("recipes_edit", anchovies.clone()).await;
    assert!(
        refused.contains("ingredients.capers: [diet_conflict]"),
        "{refused}"
    );
    let mut keep = anchovies;
    keep["keepDietConflicts"] = json!(true);
    assert_eq!(h.ok("recipes_edit", keep).await["revision"], 3);

    // A read lists what the recipe keeps outside the diet.
    let read = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(codes(&read["warnings"]).len(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_vegetarian_recipe_with_friendly_amounts_saves_clean() {
    let h = Harness::new().await;
    let created = h
        .ok("recipes_create", json!({ "document": vegetarian() }))
        .await;
    assert_eq!(created["saved"], true);
    assert_eq!(created["warnings"], json!([]));
}

#[tokio::test(start_paused = true)]
async fn awkward_amounts_are_warnings_with_what_to_write() {
    let h = Harness::new().await;
    let mut document = vegetarian();
    let ingredients = document["components"][0]["ingredients"]
        .as_array_mut()
        .unwrap();
    for ingredient in ingredients.iter_mut() {
        match ingredient["id"].as_str().unwrap() {
            "wine" => ingredient["qty"] = json!(67),
            "flour" => ingredient["qty"] = json!(113),
            "parsley" => ingredient["qty"] = json!(2.7),
            _ => {}
        }
    }
    let created = h
        .ok("recipes_create", json!({ "document": document }))
        .await;
    // A warning never blocks the save.
    assert_eq!(created["saved"], true);
    let warnings = created["warnings"].as_array().unwrap();
    assert_eq!(codes(&created["warnings"]), ["awkward_amount"; 3]);
    let hint = |id: &str| {
        warnings
            .iter()
            .find(|warning| warning["path"] == format!("ingredients.{id}"))
            .and_then(|warning| warning["hint"].as_str())
            .unwrap()
            .to_owned()
    };
    assert!(hint("wine").contains("75 ml"));
    assert!(hint("flour").contains("115 g"));
    assert!(hint("parsley").contains("2½ tbsp"));
}

#[tokio::test(start_paused = true)]
async fn scaled_views_show_amounts_a_cook_can_measure() {
    let h = Harness::new().await;
    let id = h
        .ok("recipes_create", json!({ "document": vegetarian() }))
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let half = h
        .ok(
            "recipes_get",
            json!({ "id": id, "view": "text", "scale": 0.5 }),
        )
        .await;
    let text = half["text"].as_str().unwrap();
    // 60 g flour, 2 tbsp oil, 120 ml wine (alt ½ cup), 180 ml stock (alt ¾ cup).
    assert!(text.contains("- 30 g (¼ cup) all-purpose flour"), "{text}");
    assert!(text.contains("- 1 tbsp olive oil"), "{text}");
    assert!(text.contains("- 60 ml (¼ cup) dry white wine"), "{text}");
    assert!(
        text.contains("- 90 ml (⅓ cup + ½ tbsp) vegetable stock"),
        "{text}"
    );
}
