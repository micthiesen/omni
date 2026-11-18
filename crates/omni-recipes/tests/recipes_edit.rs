//! Editing a recipe: id-addressed operations, dry runs, refusals that list
//! every problem, revision conflicts, replace, revert and history.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

mod common;

use common::{Harness, PICCATA, doc};
use serde_json::{Value, json};

/// The edit of the design's worked example: no wine, more stock, double the
/// lemon juice.
fn piccata_ops() -> Value {
    json!([
        { "op": "set", "target": "lemon", "fields": { "qty": 3 } },
        { "op": "setInput", "step": "simmer", "ref": "lemon", "fields": { "qty": 2 } },
        { "op": "set", "target": "stock", "fields": { "qty": 300, "alt": { "qty": 1.25, "unit": "cup" } } },
        { "op": "setInput", "step": "deglaze", "ref": "wine", "remove": true },
        { "op": "setInput", "step": "deglaze", "ref": "stock", "fields": { "qty": 120, "unit": "ml" } },
        { "op": "setInput", "step": "simmer", "ref": "stock", "fields": { "rest": true } },
        { "op": "replaceText", "target": "deglaze", "find": "{wine}", "replace": "{stock}" },
        { "op": "remove", "target": "wine" }
    ])
}

/// The design's broken draft: four seeded mistakes that surface as six errors
/// and two warnings.
fn broken_draft() -> Value {
    let mut draft = doc(PICCATA);
    let steps = draft["components"][0]["steps"].as_array_mut().unwrap();
    let step = |steps: &Vec<Value>, id: &str| -> usize {
        steps.iter().position(|step| step["id"] == id).unwrap()
    };
    let sear = step(steps, "sear");
    steps[sear]["inputs"][2] = json!({ "ref": "buter", "qty": 2, "unit": "tbsp" });
    let mount = step(steps, "mount");
    steps[mount]["inputs"][1] = json!({ "ref": "butter", "qty": 3, "unit": "tbsp" });
    steps[mount]["text"] = json!(
        "Take {simmer} off the heat and swirl in 3 tbsp cold butter. Taste and adjust with {salt}."
    );
    let simmer = step(steps, "simmer");
    steps[simmer]["inputs"]
        .as_array_mut()
        .unwrap()
        .retain(|input| input["ref"] != "capers");
    steps[simmer]["text"] = json!("Add {stock} and {lemon} to {deglaze}. Simmer {time}.");
    let deglaze = step(steps, "deglaze");
    steps[deglaze]["inputs"][0] = json!({ "ref": "mount" });
    steps[deglaze]["text"] =
        json!("Pour {wine} into {mount|the hot pan} and scrape up the browned bits.");
    draft
}

#[tokio::test(start_paused = true)]
async fn the_piccata_edit_writes_one_revision_with_the_exact_change_list() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let edited = h
        .ok(
            "recipes_edit",
            json!({
                "id": id, "expectedRevision": 1,
                "summary": "No wine: deglaze with stock, double the lemon juice",
                "ops": piccata_ops(),
            }),
        )
        .await;
    assert_eq!(edited["saved"], true);
    assert_eq!(edited["revision"], 2);
    assert_eq!(edited["warnings"], json!([]));
    assert_eq!(
        edited["changes"],
        json!([
            "− ingredient wine",
            "~ stock.alt: {\"qty\": 0.75, \"unit\": \"cup\"} → {\"qty\": 1.25, \"unit\": \"cup\"}",
            "~ stock.qty: 180 → 300",
            "~ lemon.qty: 2 → 3",
            "~ deglaze.inputs: [{\"ref\": \"fond\"}, {\"ref\": \"wine\"}] → [{\"ref\": \"fond\"}, {\"ref\": \"stock\", \"qty\": 120, \"unit\": \"ml\"}]",
            "~ deglaze.text: \"Pour {wine} into {fond|the hot pan} and scrape up the browned bits. Boil until reduced by half, about {time}.\" → \"Pour {stock} into {fond|the hot pan} and scrape up the browned bits. Boil until reduced by half, about {time}.\"",
            "~ simmer.inputs: [{\"ref\": \"deglaze\"}, {\"ref\": \"stock\"}, {\"ref\": \"lemon\", \"qty\": 1, \"part\": \"juice\"}, {\"ref\": \"capers\"}] → [{\"ref\": \"deglaze\"}, {\"ref\": \"stock\", \"rest\": true}, {\"ref\": \"lemon\", \"qty\": 2, \"part\": \"juice\"}, {\"ref\": \"capers\"}]"
        ])
    );
    let text = edited["text"].as_str().unwrap();
    assert!(text.contains(
        "3. Pour 120 ml chicken stock into the hot pan and scrape up the browned bits. Boil until reduced by half, about 2 minutes."
    ));
    assert!(text.contains(
        "4. Add the remaining 180 ml chicken stock, the juice of 2 lemons and 2 tbsp capers to the wine reduction. Simmer until reduced by about half, 4–5 minutes."
    ));

    // The stored recipe is the edited one, and the first revision is intact.
    let now = h
        .ok("recipes_get", json!({ "id": id, "view": "text" }))
        .await;
    assert_eq!(now["text"], edited["text"]);
    assert_eq!(now["headRevision"], 2);
    let first = h
        .ok("recipes_get", json!({ "id": id, "revision": 1 }))
        .await;
    assert_eq!(first["document"], doc(PICCATA));
}

#[tokio::test(start_paused = true)]
async fn a_dry_run_returns_the_same_result_and_saves_nothing() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let edit = |dry: bool| {
        json!({
            "id": id, "expectedRevision": 1, "summary": "No wine",
            "ops": piccata_ops(), "dryRun": dry,
        })
    };
    let dry = h.ok("recipes_edit", edit(true)).await;
    assert_eq!(dry["saved"], false);
    assert_eq!(dry["revision"], 1);
    let real = h.ok("recipes_edit", edit(false)).await;
    assert_eq!(dry["changes"], real["changes"]);
    assert_eq!(dry["text"], real["text"]);

    // Every tool that changes a recipe or the registry: nothing is kept.
    let before = h.recipes.dump().await.unwrap();
    for (tool, arguments) in [
        (
            "recipes_create",
            json!({ "document": doc(common::COOKIES), "dryRun": true }),
        ),
        (
            "recipes_replace",
            json!({ "id": id, "expectedRevision": 2, "summary": "x", "document": doc(PICCATA), "dryRun": true }),
        ),
        (
            "recipes_revert",
            json!({ "id": id, "expectedRevision": 2, "toRevision": 1, "dryRun": true }),
        ),
        (
            "recipes_organize",
            json!({ "ids": [id], "state": "archived", "dryRun": true }),
        ),
        (
            "recipes_tags_edit",
            json!({ "ops": [{ "op": "create", "name": "quick", "group": "occasion" }], "dryRun": true }),
        ),
    ] {
        let result = h.ok(tool, arguments).await;
        assert_ne!(result["saved"], true, "{tool}");
        assert!(result.get("changeId").is_none(), "{tool}: {result}");
        assert_eq!(h.recipes.dump().await.unwrap(), before, "{tool}");
    }
    let history = h.ok("recipes_history", json!({ "id": id })).await;
    assert_eq!(history["total"], 2);
}

#[tokio::test(start_paused = true)]
async fn a_broken_draft_is_refused_with_every_problem_at_once() {
    let h = Harness::new().await;
    let refused = h
        .err("recipes_create", json!({ "document": broken_draft() }))
        .await;
    // Byte for byte the design prototype's refusal.
    assert_eq!(
        format!("{refused}\n"),
        include_str!("golden/broken-draft.txt")
    );

    // A dry run returns the same list as a normal result.
    let dry = h
        .ok(
            "recipes_create",
            json!({ "document": broken_draft(), "dryRun": true }),
        )
        .await;
    assert_eq!(dry["saved"], false);
    let codes: Vec<&str> = dry["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| problem["code"].as_str().unwrap())
        .collect();
    assert_eq!(
        codes,
        [
            "unknown_ref",
            "placeholder_not_input",
            "forward_ref",
            "under_allocated",
            "unused_ingredient",
            "ambiguous_split"
        ]
    );
    assert_eq!(dry["problems"][0]["path"], "steps.sear.inputs.buter");
    assert_eq!(dry["problems"][0]["hint"], "did you mean \"butter\"?");
    assert_eq!(dry["warnings"].as_array().unwrap().len(), 2);
    assert!(dry.get("id").is_none());
    let found = h.ok("recipes_search", json!({})).await;
    assert_eq!(found["total"], 0);

    // The same holds for an edit that breaks a valid recipe.
    let id = h.create(PICCATA).await;
    let breaking = |dry: bool| {
        json!({
            "id": id, "expectedRevision": 1, "summary": "Drop the capers from the sauce",
            "ops": [{ "op": "setInput", "step": "simmer", "ref": "capers", "remove": true }],
            "dryRun": dry,
        })
    };
    let refused = h.err("recipes_edit", breaking(false)).await;
    assert!(
        refused.starts_with("[invalid_recipe] 2 problems"),
        "{refused}"
    );
    assert!(refused.contains("ingredients.capers: [unused_ingredient]"));
    let dry = h.ok("recipes_edit", breaking(true)).await;
    assert_eq!(dry["saved"], false);
    assert_eq!(dry["problems"].as_array().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_stale_revision_is_refused_with_what_changed_since() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let rename = |expected: u32, to: &str| {
        json!({
            "id": id, "expectedRevision": expected, "summary": format!("Rename to {to}"),
            "ops": [{ "op": "setRecipe", "fields": { "title": to } }],
        })
    };
    h.ok("recipes_edit", rename(1, "Piccata")).await;
    let conflict = h.err("recipes_edit", rename(1, "Chicken")).await;
    assert!(
        conflict.starts_with(&format!(
            "[revision_conflict] {id} is at revision 2, not 1; since revision 1: revision 2 ("
        )),
        "{conflict}"
    );
    assert!(conflict.contains("recipes_edit): Rename to Piccata"));
    assert!(conflict.ends_with("Re-read it and apply your change to revision 2"));

    // An operation that cannot apply names its index.
    let failed = h
        .err(
            "recipes_edit",
            json!({
                "id": id, "expectedRevision": 2, "summary": "x",
                "ops": [
                    { "op": "set", "target": "lemon", "fields": { "qty": 3 } },
                    { "op": "set", "target": "caper", "fields": { "qty": 3 } }
                ],
            }),
        )
        .await;
    assert!(failed.starts_with("[op_failed] ops[1] (set): "), "{failed}");
    assert!(failed.contains("\"caper\""));
    let unknown = h
        .err("recipes_edit", rename(1, "x").tap_id("rcp_00000000"))
        .await;
    assert!(unknown.starts_with("[not_found]"), "{unknown}");
}

trait TapId {
    fn tap_id(self, id: &str) -> Value;
}

impl TapId for Value {
    fn tap_id(mut self, id: &str) -> Value {
        self["id"] = json!(id);
        self
    }
}

#[tokio::test(start_paused = true)]
async fn replace_revert_history_and_diff() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let mut rewritten = doc(PICCATA);
    rewritten["title"] = json!("Chicken Piccata for Six");
    rewritten["yield"]["qty"] = json!(6);
    let replaced = h
        .ok(
            "recipes_replace",
            json!({
                "id": id, "expectedRevision": 1, "summary": "Retitle for six", "document": rewritten,
            }),
        )
        .await;
    assert_eq!(replaced["revision"], 2);
    assert_eq!(
        replaced["changes"],
        json!([
            "title: \"Chicken Piccata\" → \"Chicken Piccata for Six\"",
            "yield: {\"qty\": 4, \"unit\": \"servings\"} → {\"qty\": 6, \"unit\": \"servings\"}"
        ])
    );
    assert!(
        replaced["text"]
            .as_str()
            .unwrap()
            .starts_with("# Chicken Piccata for Six\n")
    );
    // No stored source: nothing to compare.
    assert_eq!(replaced["fidelity"]["checked"], false);

    let reverted = h
        .ok(
            "recipes_revert",
            json!({ "id": id, "expectedRevision": 2, "toRevision": 1 }),
        )
        .await;
    assert_eq!(reverted["revision"], 3);
    assert_eq!(reverted["changes"].as_array().unwrap().len(), 2);
    let head = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(head["document"], doc(PICCATA));
    assert_eq!(head["title"], "Chicken Piccata");

    let history = h
        .ok(
            "recipes_history",
            json!({ "id": id, "diff": { "from": 1, "to": 2 } }),
        )
        .await;
    let summaries: Vec<&str> = history["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|revision| revision["summary"].as_str().unwrap())
        .collect();
    assert_eq!(
        summaries,
        ["Revert to revision 1", "Retitle for six", "Created"]
    );
    assert_eq!(history["revisions"][1]["tool"], "recipes_replace");
    assert_eq!(history["diff"], replaced["changes"]);

    // An unchanged document writes no revision.
    let same = h
        .ok(
            "recipes_replace",
            json!({ "id": id, "expectedRevision": 3, "summary": "Nothing", "document": doc(PICCATA) }),
        )
        .await;
    assert_eq!(same["saved"], false);
    assert_eq!(same["revision"], 3);

    // The library change log lists the writes, newest first.
    let log = h.ok("recipes_history", json!({})).await;
    let tools: Vec<&str> = log["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| change["tool"].as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        ["recipes_revert", "recipes_replace", "recipes_create"]
    );
    assert_eq!(log["changes"][0]["recipes"], 1);
}

#[tokio::test(start_paused = true)]
async fn views_scale_without_changing_the_recipe() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let scaled = h
        .ok(
            "recipes_get",
            json!({ "id": id, "view": "text", "scale": 1.5 }),
        )
        .await;
    let text = scaled["text"].as_str().unwrap();
    assert!(text.contains("- 750 g boneless, skinless chicken breasts"));
    assert!(text.contains("270 ml (1⅛ cups) chicken stock"));
    assert!(text.contains("the juice of 1½ lemons"));
    assert_eq!(scaled["scale"], 1.5);
    let six = h
        .ok(
            "recipes_get",
            json!({ "id": id, "view": "text", "servings": 6 }),
        )
        .await;
    assert_eq!(six["text"], scaled["text"]);
    let both = h
        .err(
            "recipes_get",
            json!({ "id": id, "view": "text", "servings": 6, "scale": 2 }),
        )
        .await;
    assert!(both.starts_with("[validation]"), "{both}");
    let document = h.err("recipes_get", json!({ "id": id, "scale": 2 })).await;
    assert!(document.contains("never scaled"), "{document}");
    let stored = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(stored["document"], doc(PICCATA));
    assert_eq!(
        stored["time"],
        json!({ "activeMin": 25, "totalMin": 25, "complete": true })
    );
}
