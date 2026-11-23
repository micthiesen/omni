//! Variants through the tools: shown in the text and chart, read as one
//! variant, held to the household rules, edited by operation, and cooked.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;

use common::Harness;
use serde_json::{Value, json};

const SHORTBREAD: &str = include_str!("golden/hardtack-shortbread.json");

fn codes(problems: &Value) -> Vec<String> {
    problems
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            format!(
                "{} [{}]",
                p["path"].as_str().unwrap(),
                p["code"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_text_and_chart_show_variants_and_one_variant_reads_on_its_own() {
    let h = Harness::new().await;
    let id = h.create(SHORTBREAD).await;
    let text = h
        .ok(
            "recipes_get",
            json!({ "id": id, "view": "text", "ids": true }),
        )
        .await;
    let text = text["text"].as_str().unwrap();
    assert!(
        text.contains("## Variants\n\n**Hardtack style** [hardtack]"),
        "{text}"
    );
    assert!(
        text.contains("6. [bake] Bake the rounds in the hot oven in 3 batches at"),
        "{text}"
    );

    let hardtack = h
        .ok(
            "recipes_get",
            json!({ "id": id, "view": "text", "variant": "hardtack", "scale": 2 }),
        )
        .await;
    assert_eq!(hardtack["variant"], "hardtack");
    let shown = hardtack["text"].as_str().unwrap();
    assert!(shown.contains("Makes 72 rounds (scaled ×2)"), "{shown}");
    assert!(!shown.contains("## Variants"), "{shown}");

    let chart = h
        .ok("recipes_get", json!({ "id": id, "view": "chart" }))
        .await;
    assert!(
        chart["chart"]
            .as_str()
            .unwrap()
            .ends_with("Variants: Hardtack style [hardtack] (changes yield, oven, roll, bake)\n"),
        "{}",
        chart["chart"]
    );

    let refused = h
        .err("recipes_get", json!({ "id": id, "variant": "hardtack" }))
        .await;
    assert!(refused.starts_with("[validation]"), "{refused}");
    let unknown = h
        .err(
            "recipes_get",
            json!({ "id": id, "view": "text", "variant": "crisp" }),
        )
        .await;
    assert!(unknown.contains("no variant \"crisp\""), "{unknown}");
}

#[tokio::test(start_paused = true)]
async fn a_variant_is_held_to_the_household_diet() {
    let h = Harness::new().await;
    let id = h.create(SHORTBREAD).await;
    let lard = json!({
        "op": "setVariant",
        "variant": {
            "id": "lard", "name": "With lard",
            "ops": [{ "op": "set", "target": "butter", "fields": { "name": "lard" } }],
        },
    });
    let refused = h
        .err(
            "recipes_edit",
            json!({ "id": id, "expectedRevision": 1, "summary": "Lard", "ops": [lard.clone()] }),
        )
        .await;
    assert!(
        refused.contains("variants.lard.ingredients.butter: [diet_conflict]"),
        "{refused}"
    );
    let kept = h
        .ok(
            "recipes_edit",
            json!({ "id": id, "expectedRevision": 1, "summary": "Lard",
                    "ops": [lard], "keepDietConflicts": true }),
        )
        .await;
    assert_eq!(kept["changes"], json!(["+ variant lard"]));
    assert!(
        codes(&kept["warnings"])
            .contains(&"variants.lard.ingredients.butter [diet_conflict]".to_owned())
    );

    // Later edits are not refused for the conflict the recipe already had.
    let later = h
        .ok(
            "recipes_edit",
            json!({ "id": id, "expectedRevision": 2, "summary": "Saltier",
                    "ops": [{ "op": "set", "target": "salt", "fields": { "qty": 0.5 } }] }),
        )
        .await;
    assert_eq!(later["saved"], true);
}

#[tokio::test(start_paused = true)]
async fn a_cook_picks_a_variant_and_switches_keeping_what_is_crossed_off() {
    let h = Harness::new().await;
    let id = h.create(SHORTBREAD).await;
    let unknown = h
        .err(
            "recipes_cook_start",
            json!({ "recipeId": id, "variant": "crisp" }),
        )
        .await;
    assert!(unknown.contains("no variant \"crisp\""), "{unknown}");
    assert_eq!(
        h.ok("recipes_cook_get", json!({})).await["active"],
        json!([]),
        "a refused start starts nothing"
    );

    let started = h
        .ok(
            "recipes_cook_start",
            json!({ "recipeId": id, "variant": "hardtack" }),
        )
        .await;
    assert_eq!(started["variant"], "hardtack");
    let sid = started["sessionId"].as_str().unwrap().to_owned();
    let update = |events: Value| json!({ "sessionId": sid, "events": events });
    let checked = h
        .ok(
            "recipes_cook_update",
            update(json!([{ "type": "check", "target": "cream" }])),
        )
        .await;
    assert_eq!(checked["session"]["variant"], "hardtack");
    assert_eq!(checked["session"]["done"], json!(["cream"]));

    let back = h
        .ok(
            "recipes_cook_update",
            update(json!([{ "type": "variant" }])),
        )
        .await;
    assert!(back["session"].get("variant").is_none(), "{back}");
    assert_eq!(back["session"]["done"], json!(["cream"]));

    let wrong = h
        .err(
            "recipes_cook_update",
            update(json!([{ "type": "variant", "variant": "crisp" }])),
        )
        .await;
    assert!(wrong.contains("no variant \"crisp\""), "{wrong}");
}
