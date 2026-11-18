//! Record metadata: the tag registry, tags and state in bulk, merging, the
//! change log's undo, and the trash with its 30-day purge.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

mod common;

use common::{COOKIES, ENCHILADAS, Harness, LASAGNE, PICCATA, doc, png};
use serde_json::{Value, json};

fn names(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| {
            format!(
                "{}:{}",
                tag["group"].as_str().unwrap(),
                tag["name"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn unknown_tags_are_refused_with_the_nearest_names() {
    let h = Harness::new().await;
    // Nothing is seeded.
    let guide = h.ok("recipes_guide", json!({})).await;
    assert_eq!(guide["tags"], json!([]));
    assert_eq!(
        guide["tagGroups"],
        json!([
            "course", "cuisine", "main", "method", "diet", "occasion", "other"
        ])
    );
    h.tag("weeknight", "occasion").await;
    h.tag("weekend", "occasion").await;
    h.tag("italian", "cuisine").await;

    let refused = h
        .err(
            "recipes_create",
            json!({ "document": doc(PICCATA), "tags": ["Italian", "weeknights"] }),
        )
        .await;
    assert!(
        refused.starts_with(
            "[unknown_tag] tag \"weeknights\" is not in the registry; nearest: \"weeknight\""
        ),
        "{refused}"
    );
    assert!(refused.contains("createTags"));
    assert_eq!(h.ok("recipes_search", json!({})).await["total"], 0);

    let created = h
        .ok(
            "recipes_create",
            json!({ "document": doc(PICCATA), "tags": ["Italian", "weeknights", " Date Night "], "createTags": true }),
        )
        .await;
    let id = created["id"].as_str().unwrap();
    let recipe = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(
        recipe["tags"],
        json!(["date-night", "italian", "weeknights"])
    );
    let guide = h.ok("recipes_guide", json!({})).await;
    assert_eq!(
        names(&guide["tags"]),
        [
            "cuisine:italian",
            "occasion:weekend",
            "occasion:weeknight",
            "other:date-night",
            "other:weeknights"
        ]
    );
    let italian = guide["tags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tag| tag["name"] == "italian")
        .unwrap();
    assert_eq!(italian["count"], 1);
    assert_eq!(guide["library"]["active"], 1);
    assert_eq!(guide["library"]["neverCooked"], 1);
    assert_eq!(guide["version"], "3");

    let organize = h
        .err(
            "recipes_organize",
            json!({ "ids": [id], "addTags": ["quick"] }),
        )
        .await;
    assert!(
        organize.starts_with("[unknown_tag] tag \"quick\""),
        "{organize}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_registry_renames_merges_regroups_and_deletes() {
    let h = Harness::new().await;
    for (name, group) in [
        ("quick", "other"),
        ("weeknight", "occasion"),
        ("fast", "other"),
        ("chicken", "main"),
    ] {
        h.tag(name, group).await;
    }
    let a = h.create(PICCATA).await;
    let b = h.create(ENCHILADAS).await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [a], "addTags": ["quick", "chicken"] }),
    )
    .await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [b], "addTags": ["fast", "weeknight", "chicken"] }),
    )
    .await;

    let edited = h
        .ok(
            "recipes_tags_edit",
            json!({ "ops": [
                { "op": "merge", "from": ["quick", "fast"], "into": "weeknight" },
                { "op": "rename", "from": "chicken", "to": "poultry" },
                { "op": "setGroup", "name": "poultry", "group": "diet" },
                { "op": "create", "name": "thai", "group": "cuisine", "description": "Thai dishes" },
                { "op": "delete", "name": "thai" }
            ] }),
        )
        .await;
    assert_eq!(
        names(&edited["tags"]),
        ["diet:poultry", "occasion:weeknight"]
    );
    assert_eq!(edited["tags"][1]["count"], 2);
    let change = edited["changeId"].as_str().unwrap().to_owned();
    for id in [&a, &b] {
        let recipe = h.ok("recipes_get", json!({ "id": id })).await;
        assert_eq!(recipe["tags"], json!(["poultry", "weeknight"]));
    }
    assert_eq!(
        h.ok("recipes_search", json!({ "tags": ["poultry"] })).await["total"],
        2
    );
    assert_eq!(
        h.ok(
            "recipes_search",
            json!({ "query": "chicken", "tags": ["quick"] })
        )
        .await["total"],
        0
    );

    for (ops, code) in [
        (
            json!([{ "op": "rename", "from": "nope", "to": "x" }]),
            "[unknown_tag]",
        ),
        (
            json!([{ "op": "rename", "from": "poultry", "to": "weeknight" }]),
            "[validation]",
        ),
        (
            json!([{ "op": "create", "name": "poultry", "group": "main" }]),
            "[validation]",
        ),
        (
            json!([{ "op": "delete", "name": "weeknight" }, { "op": "delete", "name": "weeknight" }]),
            "[unknown_tag]",
        ),
    ] {
        let error = h.err("recipes_tags_edit", json!({ "ops": ops })).await;
        assert!(error.starts_with(code), "{error}");
    }
    // A failed call changed nothing, not even its first operation.
    assert_eq!(
        names(&h.ok("recipes_guide", json!({})).await["tags"]),
        ["diet:poultry", "occasion:weeknight"]
    );

    // One undo puts the registry and every recipe's tags back.
    let undone = h.ok("recipes_undo", json!({ "changeId": change })).await;
    assert_eq!(undone["undone"], true);
    assert_eq!(undone["restored"], 2);
    assert_eq!(
        names(&h.ok("recipes_guide", json!({})).await["tags"]),
        [
            "main:chicken",
            "occasion:weeknight",
            "other:fast",
            "other:quick"
        ]
    );
    assert_eq!(
        h.ok("recipes_get", json!({ "id": a })).await["tags"],
        json!(["chicken", "quick"])
    );
    assert_eq!(
        h.ok("recipes_get", json!({ "id": b })).await["tags"],
        json!(["chicken", "fast", "weeknight"])
    );
}

#[tokio::test(start_paused = true)]
async fn organize_changes_listed_recipes_in_one_undoable_change() {
    let h = Harness::new().await;
    h.tag("weeknight", "occasion").await;
    h.tag("quick", "other").await;
    let ids = [
        h.create(PICCATA).await,
        h.create(COOKIES).await,
        h.create(LASAGNE).await,
    ];
    h.ok(
        "recipes_organize",
        json!({ "ids": [ids[0]], "addTags": ["weeknight"] }),
    )
    .await;

    let organized = h
        .ok(
            "recipes_organize",
            json!({ "ids": ids, "addTags": ["weeknight"], "removeTags": ["quick"], "summary": "Fold quick into weeknight" }),
        )
        .await;
    assert_eq!(
        (organized["changed"].clone(), organized["unchanged"].clone()),
        (json!(2), json!(1))
    );
    assert_eq!(organized["results"].as_array().unwrap().len(), 3);
    assert_eq!(
        organized["results"][1],
        json!({
            "id": ids[1], "title": "Chocolate Chip Cookies", "tags": ["weeknight"], "state": "active"
        })
    );
    let change = organized["changeId"].as_str().unwrap().to_owned();
    assert!(change.starts_with("chg_"));
    let log = h.ok("recipes_history", json!({})).await;
    assert_eq!(log["changes"][0]["summary"], "Fold quick into weeknight");
    assert_eq!(log["changes"][0]["recipes"], 2);
    assert_eq!(log["changes"][0]["undone"], false);

    // Nothing to change is not a change.
    let again = h
        .ok(
            "recipes_organize",
            json!({ "ids": ids, "addTags": ["weeknight"] }),
        )
        .await;
    assert_eq!(again["changed"], 0);
    assert!(again.get("changeId").is_none());

    // Archive and trash; the trash is searchable only by state.
    h.ok(
        "recipes_organize",
        json!({ "ids": [ids[1]], "state": "archived" }),
    )
    .await;
    let trashed = h
        .ok(
            "recipes_organize",
            json!({ "ids": [ids[2]], "state": "trashed" }),
        )
        .await;
    assert_eq!(h.ok("recipes_search", json!({})).await["total"], 1);
    assert_eq!(
        h.ok("recipes_search", json!({ "state": "archived" })).await["recipes"][0]["id"],
        ids[1]
    );
    assert_eq!(
        h.ok("recipes_search", json!({ "state": "trashed" })).await["recipes"][0]["id"],
        ids[2]
    );
    assert_eq!(
        h.ok("recipes_search", json!({ "state": "all" })).await["total"],
        3
    );
    let edit_trashed = h
        .err(
            "recipes_edit",
            json!({ "id": ids[2], "expectedRevision": 1, "summary": "x", "ops": [{ "op": "rescale", "factor": 2 }] }),
        )
        .await;
    assert!(
        edit_trashed.starts_with("[trashed]") && edit_trashed.contains("recipes_undo"),
        "{edit_trashed}"
    );

    // Undo the tag change: a newer change touched other things, so it goes through.
    let undone = h.ok("recipes_undo", json!({ "changeId": change })).await;
    assert_eq!(undone["restored"], 2);
    assert_eq!(
        h.ok("recipes_get", json!({ "id": ids[1] })).await["tags"],
        json!([])
    );
    assert_eq!(
        h.ok("recipes_get", json!({ "id": ids[0] })).await["tags"],
        json!(["weeknight"])
    );
    // Restore from the trash, either way.
    h.ok("recipes_undo", json!({ "changeId": trashed["changeId"] }))
        .await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": ids[2] })).await["state"],
        "active"
    );
    h.ok(
        "recipes_organize",
        json!({ "ids": [ids[2]], "state": "trashed" }),
    )
    .await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [ids[2]], "state": "active" }),
    )
    .await;
    assert_eq!(h.ok("recipes_search", json!({})).await["total"], 2);

    let missing = h
        .err(
            "recipes_organize",
            json!({ "ids": [ids[0], "rcp_zzzzzzzz"], "state": "archived" }),
        )
        .await;
    assert!(missing.starts_with("[not_found]"), "{missing}");
    assert_eq!(
        h.ok("recipes_get", json!({ "id": ids[0] })).await["state"],
        "active"
    );
}

#[tokio::test(start_paused = true)]
async fn undo_is_refused_when_something_newer_touched_the_same_things() {
    let h = Harness::new().await;
    for name in ["a", "b", "c"] {
        h.tag(name, "other").await;
    }
    let id = h.create(PICCATA).await;
    let first = h
        .ok("recipes_organize", json!({ "ids": [id], "addTags": ["a"] }))
        .await;
    let second = h
        .ok("recipes_organize", json!({ "ids": [id], "addTags": ["b"] }))
        .await;
    let (first, second) = (
        first["changeId"].as_str().unwrap().to_owned(),
        second["changeId"].as_str().unwrap().to_owned(),
    );

    let conflict = h.err("recipes_undo", json!({ "changeId": first })).await;
    assert_eq!(
        conflict,
        format!(
            "[undo_conflict] newer changes touched what {first} touched: {second}; undo those first, newest first"
        )
    );
    // A dry run says what would happen and keeps nothing.
    let dry = h
        .ok(
            "recipes_undo",
            json!({ "changeId": second, "dryRun": true }),
        )
        .await;
    assert_eq!(dry["undone"], false);
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["tags"],
        json!(["a", "b"])
    );

    let undo_second = h.ok("recipes_undo", json!({ "changeId": second })).await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["tags"],
        json!(["a"])
    );
    let twice = h.err("recipes_undo", json!({ "changeId": second })).await;
    assert!(
        twice.starts_with("[undo_conflict]") && twice.contains("already undone"),
        "{twice}"
    );
    // With the newer change undone, the older one can go too.
    h.ok("recipes_undo", json!({ "changeId": first })).await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["tags"],
        json!([])
    );
    // Undoing an undo redoes the change.
    h.ok(
        "recipes_undo",
        json!({ "changeId": undo_second["undoChangeId"] }),
    )
    .await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["tags"],
        json!(["a", "b"])
    );

    // Content: an edit is undone as a new revision, unless a newer one exists.
    let edit = |expected: u32, qty: u32| {
        json!({ "id": id, "expectedRevision": expected, "summary": format!("{qty} lemons"),
                "ops": [{ "op": "set", "target": "parsley", "fields": { "qty": qty } }] })
    };
    h.ok("recipes_edit", edit(1, 3)).await;
    h.ok("recipes_edit", edit(2, 4)).await;
    let log = h.ok("recipes_history", json!({})).await;
    let (newest, older) = (
        log["changes"][0]["changeId"].as_str().unwrap().to_owned(),
        log["changes"][1]["changeId"].as_str().unwrap().to_owned(),
    );
    let stale = h.err("recipes_undo", json!({ "changeId": older })).await;
    assert!(
        stale.starts_with("[undo_conflict]") && stale.contains(&newest),
        "{stale}"
    );
    h.ok("recipes_undo", json!({ "changeId": newest })).await;
    let head = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(head["headRevision"], 4);
    assert_eq!(
        head["document"]["components"][0]["ingredients"][10]["qty"],
        3
    );

    // Undoing a create sends the recipe to the trash.
    let cookies = h.create(COOKIES).await;
    let created = h.ok("recipes_history", json!({})).await["changes"][0]["changeId"]
        .as_str()
        .unwrap()
        .to_owned();
    h.ok("recipes_undo", json!({ "changeId": created })).await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": cookies })).await["state"],
        "trashed"
    );

    // After 30 days a change can no longer be undone.
    h.advance_days(31).await;
    let old = h.err("recipes_undo", json!({ "changeId": first })).await;
    assert!(old.starts_with("[not_found]"), "{old}");
    let unknown = h
        .err("recipes_undo", json!({ "changeId": "chg_zzzzzzzz" }))
        .await;
    assert!(unknown.starts_with("[not_found]"), "{unknown}");
}

async fn add_image(h: &Harness, id: &str, seed: u8) -> String {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(png(seed));
    let added = h
        .ok(
            "recipes_images_edit",
            json!({ "id": id, "add": [{ "dataBase64": data, "mimeType": "image/png" }] }),
        )
        .await;
    added["images"].as_array().unwrap().last().unwrap()["imageId"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test(start_paused = true)]
async fn a_merge_moves_everything_and_leaves_a_pointer() {
    let h = Harness::new().await;
    for name in ["italian", "chicken", "weeknight"] {
        h.tag(name, "other").await;
    }
    let keep = h.create(PICCATA).await;
    let mut other = doc(PICCATA);
    other["title"] = json!("Piccata (old import)");
    let absorbed = h
        .ok(
            "recipes_create",
            json!({ "document": other, "tags": ["chicken", "weeknight"], "notes": "Use more capers." }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    h.ok(
        "recipes_organize",
        json!({ "ids": [keep], "addTags": ["italian", "chicken"] }),
    )
    .await;
    let image = add_image(&h, &absorbed, 3).await;
    // One finished cook with an adjustment on the recipe that will be absorbed.
    let session = h
        .ok("recipes_cook_start", json!({ "recipeId": absorbed }))
        .await["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": session, "events": [
            { "type": "adjust", "target": "simmer", "text": "Simmer 8 minutes." },
            { "type": "finish" }
        ] }),
    )
    .await;

    let dry = h
        .ok(
            "recipes_merge",
            json!({ "keep": keep, "absorb": [absorbed], "dryRun": true }),
        )
        .await;
    assert_eq!(
        dry["moved"],
        json!({ "images": 1, "cooks": 1, "adjustments": 1, "tags": 1 })
    );
    assert!(dry.get("changeId").is_none());
    assert_eq!(
        h.ok("recipes_get", json!({ "id": absorbed })).await["state"],
        "active"
    );

    let merged = h
        .ok(
            "recipes_merge",
            json!({ "keep": keep, "absorb": [absorbed] }),
        )
        .await;
    assert_eq!(merged["id"], keep);
    assert_eq!(merged["trashed"], json!([absorbed]));
    assert_eq!(merged["moved"], dry["moved"]);

    let kept = h.ok("recipes_get", json!({ "id": keep })).await;
    assert_eq!(kept["tags"], json!(["chicken", "italian", "weeknight"]));
    assert_eq!(
        kept["notes"],
        "From Piccata (old import):\nUse more capers."
    );
    assert_eq!(kept["images"][0]["imageId"], image);
    assert_eq!(kept["cooks"]["count"], 1);
    assert_eq!(kept["openAdjustments"][0]["text"], "Simmer 8 minutes.");
    assert!(kept.get("redirectedFrom").is_none());

    // The absorbed id reads as the kept recipe and refuses writes.
    let redirected = h.ok("recipes_get", json!({ "id": absorbed })).await;
    assert_eq!(redirected["id"], keep);
    assert_eq!(redirected["redirectedFrom"], absorbed);
    assert_eq!(redirected["title"], "Chicken Piccata");
    for (tool, arguments) in [
        (
            "recipes_edit",
            json!({ "id": absorbed, "expectedRevision": 1, "summary": "x", "ops": [{ "op": "rescale", "factor": 2 }] }),
        ),
        (
            "recipes_organize",
            json!({ "ids": [absorbed], "state": "active" }),
        ),
        (
            "recipes_images_edit",
            json!({ "id": absorbed, "remove": [image] }),
        ),
        (
            "recipes_merge",
            json!({ "keep": absorbed, "absorb": [keep] }),
        ),
    ] {
        let refused = h.err(tool, arguments).await;
        assert_eq!(
            refused,
            format!("[merged] {absorbed} was merged into {keep}; apply the change to {keep}"),
            "{tool}"
        );
    }
    assert_eq!(
        h.ok("recipes_search", json!({ "state": "all" })).await["total"],
        2
    );
    assert_eq!(h.ok("recipes_search", json!({})).await["total"], 1);

    // The pointer outlives the trash.
    h.advance_days(31).await;
    let purged = h.recipes.purge().await.unwrap();
    assert_eq!(purged.recipes, 1);
    assert_eq!(
        h.ok("recipes_search", json!({ "state": "all" })).await["total"],
        1
    );
    let after = h.ok("recipes_get", json!({ "id": absorbed })).await;
    assert_eq!(
        (after["id"].clone(), after["redirectedFrom"].clone()),
        (json!(keep), json!(absorbed))
    );
    let write = h
        .err(
            "recipes_organize",
            json!({ "ids": [absorbed], "state": "active" }),
        )
        .await;
    assert!(write.starts_with("[merged]"), "{write}");
    // The moved image and cook survived the purge of the recipe they came from.
    let kept = h.ok("recipes_get", json!({ "id": keep })).await;
    assert_eq!(kept["images"][0]["imageId"], image);
    let (_, content) = h
        .call_full("recipes_image_get", json!({ "imageId": image }))
        .await
        .unwrap();
    assert_eq!(common::image_blocks(&content).len(), 1);
    assert_eq!(
        h.ok("recipes_cook_get", json!({ "recipeId": keep })).await["cooks"][0]["sessionId"],
        session
    );
}

#[tokio::test(start_paused = true)]
async fn a_merge_can_be_undone() {
    let h = Harness::new().await;
    h.tag("chicken", "main").await;
    let keep = h.create(PICCATA).await;
    let absorbed = h
        .ok(
            "recipes_create",
            json!({ "document": doc(ENCHILADAS), "tags": ["chicken"], "notes": "Mild." }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let image = add_image(&h, &absorbed, 9).await;
    let merged = h
        .ok(
            "recipes_merge",
            json!({ "keep": keep, "absorb": [absorbed], "notes": "keepOnly" }),
        )
        .await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": keep })).await["notes"],
        ""
    );

    let undone = h
        .ok("recipes_undo", json!({ "changeId": merged["changeId"] }))
        .await;
    assert_eq!(undone["restored"], 2);
    let back = h.ok("recipes_get", json!({ "id": absorbed })).await;
    assert_eq!(back["id"], absorbed);
    assert!(back.get("redirectedFrom").is_none());
    assert_eq!(back["state"], "active");
    assert_eq!(back["tags"], json!(["chicken"]));
    assert_eq!(back["notes"], "Mild.");
    assert_eq!(back["images"][0]["imageId"], image);
    let kept = h.ok("recipes_get", json!({ "id": keep })).await;
    assert_eq!(kept["tags"], json!([]));
    assert_eq!(kept["images"], json!([]));
}

#[tokio::test(start_paused = true)]
async fn the_trash_is_emptied_after_thirty_days_and_referenced_images_survive() {
    let h = Harness::new().await;
    let gone = h.create(PICCATA).await;
    let stays = h.create(COOKIES).await;
    let later = h.create(LASAGNE).await;
    // The same picture on a recipe that goes and one that stays, and one only
    // on the recipe that goes.
    add_image(&h, &gone, 1).await;
    add_image(&h, &stays, 1).await;
    add_image(&h, &gone, 2).await;
    let digests = || h.recipes.files().digests().unwrap().len();
    assert_eq!(digests(), 2);

    h.ok(
        "recipes_organize",
        json!({ "ids": [gone], "state": "trashed" }),
    )
    .await;
    h.advance_days(1).await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [later], "state": "trashed" }),
    )
    .await;

    h.advance_days(28).await;
    let early = h.recipes.purge().await.unwrap();
    assert_eq!(early.recipes, 0, "29 days keeps");
    assert_eq!(
        h.ok("recipes_get", json!({ "id": gone })).await["state"],
        "trashed"
    );
    assert_eq!(digests(), 2);

    h.advance_days(1).await;
    let purged = h.recipes.purge().await.unwrap();
    assert_eq!(purged.recipes, 1, "30 days removes");
    let missing = h.err("recipes_get", json!({ "id": gone })).await;
    assert!(missing.starts_with("[not_found]"), "{missing}");
    assert!(
        h.err("recipes_history", json!({ "id": gone }))
            .await
            .starts_with("[not_found]")
    );
    // The recipe trashed a day later is still there, and restorable.
    assert_eq!(
        h.ok("recipes_get", json!({ "id": later })).await["state"],
        "trashed"
    );
    h.ok(
        "recipes_organize",
        json!({ "ids": [later], "state": "active" }),
    )
    .await;
    // The shared picture's file stays; the other one's change-log entry is
    // now older than 30 days, so its file goes.
    assert_eq!(purged.files, 1);
    assert_eq!(digests(), 1);
    let kept = h.ok("recipes_get", json!({ "id": stays })).await;
    let image = kept["images"][0]["imageId"].as_str().unwrap();
    assert!(
        h.call_full("recipes_image_get", json!({ "imageId": image }))
            .await
            .is_ok()
    );
    assert!(purged.changes >= 4);
    assert_eq!(
        h.ok("recipes_search", json!({ "state": "all" })).await["total"],
        2
    );
}
