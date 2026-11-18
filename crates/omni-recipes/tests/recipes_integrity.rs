//! Data that must survive the awkward paths: merges with cook sessions and
//! shared sources, undo of tag changes, stale notes, the archive before a
//! purge.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

mod common;

use common::{COOKIES, ENCHILADAS, Harness, LASAGNE, PICCATA, doc, get, post_json};
use omni_recipes::archive::{BackupOutcome, verify_file};
use omni_runtime::ports::FetchedPage;
use serde_json::{Value, json};

async fn bump(h: &Harness, id: &str, revision: u32) {
    h.ok(
        "recipes_edit",
        json!({ "id": id, "expectedRevision": revision, "summary": format!("Parsley {revision}"),
                "ops": [{ "op": "set", "target": "parsley", "fields": { "qty": revision + 2 } }] }),
    )
    .await;
}

async fn start(h: &Harness, recipe: &str) -> String {
    h.ok("recipes_cook_start", json!({ "recipeId": recipe }))
        .await["sessionId"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test(start_paused = true)]
async fn a_merge_moves_sessions_without_changing_what_they_show() {
    let h = Harness::new().await;
    let router = h.router();
    let keep = h.create(PICCATA).await;
    bump(&h, &keep, 1).await;
    bump(&h, &keep, 2).await;
    let absorbed = h.create(COOKIES).await;
    let finished = start(&h, &absorbed).await;
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": finished, "events": [
            { "type": "adjust", "target": "bake", "text": "Baked 12 minutes." },
            { "type": "finish" }
        ] }),
    )
    .await;
    let active = start(&h, &absorbed).await;
    let rebased = start(&h, &absorbed).await;
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": active, "events": [{ "type": "check", "target": "oven" }] }),
    )
    .await;

    h.ok(
        "recipes_merge",
        json!({ "keep": keep, "absorb": [absorbed] }),
    )
    .await;

    // The active session belongs to the kept recipe and still shows the
    // cookies it started on, at the revision it pinned.
    let seen = h
        .ok("recipes_cook_get", json!({ "sessionId": active }))
        .await;
    let view = &seen["session"];
    assert_eq!(view["recipeId"], keep);
    assert_eq!(view["title"], "Chocolate Chip Cookies");
    assert_eq!(view["revision"], 1);
    assert_eq!(view["headRevision"], 3);
    assert_eq!(view["mergedFrom"], absorbed);
    assert_eq!(view["steps"], 9);
    assert_eq!(view["done"], json!(["oven"]));
    let wire = get(&router, &format!("/api/cook/{active}")).await.json();
    assert_eq!(wire["doc"], doc(COOKIES));
    assert_eq!(wire["recipeId"], keep);
    // No session is hidden from the list.
    let listed = h.ok("recipes_cook_get", json!({})).await;
    assert_eq!(listed["active"].as_array().unwrap().len(), 2);
    assert_eq!(listed["active"][0]["of"], 9);

    // It keeps working on its own document and finishes against the kept recipe.
    let done = h
        .ok(
            "recipes_cook_update",
            json!({ "sessionId": active, "events": [
                { "type": "check", "target": "dry" },
                { "type": "adjust", "target": "cream", "text": "Creamed 6 minutes." },
                { "type": "finish", "note": "Still cookies." }
            ] }),
        )
        .await;
    assert_eq!(done["session"]["state"], "finished");
    let kept = h.ok("recipes_get", json!({ "id": keep })).await;
    assert_eq!(kept["cooks"]["count"], 2);
    assert_eq!(kept["headRevision"], 3);
    // The moved adjustments stay listed with their own text, flagged: their
    // steps are not in the kept recipe.
    let adjustments = kept["openAdjustments"].as_array().unwrap();
    assert_eq!(adjustments.len(), 2);
    for adjustment in adjustments {
        assert_eq!(adjustment["targetMissing"], true, "{adjustment}");
        assert!(adjustment["original"].as_str().unwrap().len() > 20);
    }
    let mut texts: Vec<&str> = adjustments
        .iter()
        .map(|adjustment| adjustment["text"].as_str().unwrap())
        .collect();
    texts.sort_unstable();
    assert_eq!(texts, ["Baked 12 minutes.", "Creamed 6 minutes."]);

    // A moved session can move on to the kept recipe's document.
    let moved = h
        .ok(
            "recipes_cook_update",
            json!({ "sessionId": rebased, "events": [{ "type": "rebase", "revision": 3 }] }),
        )
        .await;
    assert_eq!(moved["session"]["title"], "Chicken Piccata");
    assert_eq!(moved["session"]["revision"], 3);
    assert!(moved["session"].get("mergedFrom").is_none());

    // A new cook asked for the merged-away id starts on the kept recipe's
    // newest revision, whatever revision was asked for.
    let by_tool = h
        .ok("recipes_cook_start", json!({ "recipeId": absorbed }))
        .await;
    assert_eq!(
        (by_tool["recipeId"].clone(), by_tool["revision"].clone()),
        (json!(keep), json!(3))
    );
    let by_app = post_json(
        &router,
        "/api/cook",
        &json!({ "id": "cook_merged01", "recipeId": absorbed, "revision": 1, "scale": 1, "at": 1 }),
    )
    .await;
    assert_eq!(by_app.status, 200, "{}", by_app.text());
    assert_eq!(by_app.json()["revision"], 3);
    assert_eq!(by_app.json()["doc"]["title"], "Chicken Piccata");

    // After the absorbed recipe is purged the cook log is whole and every
    // session still reads.
    h.advance_days(31).await;
    assert_eq!(h.recipes.purge().await.unwrap().recipes, 1);
    let log = h.ok("recipes_cook_get", json!({ "recipeId": keep })).await;
    assert_eq!(log["cooks"].as_array().unwrap().len(), 2);
    assert_eq!(log["openAdjustments"].as_array().unwrap().len(), 2);
    let old = h
        .ok("recipes_cook_get", json!({ "sessionId": finished }))
        .await;
    assert_eq!(old["session"]["state"], "finished");
    assert_eq!(
        get(&router, &format!("/api/cook/{finished}")).await.status,
        200
    );
}

#[tokio::test(start_paused = true)]
async fn an_app_state_change_never_lands_on_the_recipe_that_absorbed_its_target() {
    let h = Harness::new().await;
    let router = h.router();
    let keep = h.create(PICCATA).await;
    let absorbed = h.create(ENCHILADAS).await;
    h.ok(
        "recipes_merge",
        json!({ "keep": keep, "absorb": [absorbed] }),
    )
    .await;
    for (n, state) in [(1, "trashed"), (2, "archived"), (3, "active")] {
        let reply = post_json(
            &router,
            "/api/ops",
            &json!({ "ops": [{ "id": format!("op_merged{n:06}"), "at": 1, "type": "state", "recipeId": absorbed, "state": state }] }),
        )
        .await
        .json();
        assert_eq!(reply["results"][0]["ok"], false, "{state}");
        assert!(
            reply["results"][0]["error"]
                .as_str()
                .unwrap()
                .contains("merged")
        );
    }
    assert_eq!(
        h.ok("recipes_get", json!({ "id": keep })).await["state"],
        "active"
    );
    // A note still follows the recipe to where it lives.
    let note = post_json(
        &router,
        "/api/ops",
        &json!({ "ops": [{ "id": "op_mergednote1", "at": 1, "type": "notes", "recipeId": absorbed, "text": "Less chili.", "baseRev": 0 }] }),
    )
    .await
    .json();
    assert_eq!(note["results"][0]["ok"], true);
    assert_eq!(
        h.ok("recipes_get", json!({ "id": keep })).await["notes"],
        "Less chili."
    );
}

#[tokio::test(start_paused = true)]
async fn a_cook_cannot_be_undone() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let session = start(&h, &id).await;
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": session, "events": [{ "type": "finish" }] }),
    )
    .await;
    let log = h.ok("recipes_history", json!({})).await;
    assert_eq!(log["changes"][0]["summary"], "Cooked Chicken Piccata");
    let refused = h
        .err(
            "recipes_undo",
            json!({ "changeId": log["changes"][0]["changeId"] }),
        )
        .await;
    assert!(refused.starts_with("[not_undoable]"), "{refused}");
    let dry = h
        .err(
            "recipes_undo",
            json!({ "changeId": log["changes"][0]["changeId"], "dryRun": true }),
        )
        .await;
    assert!(dry.starts_with("[not_undoable]"), "{dry}");
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["cooks"]["count"],
        1
    );
}

fn registry(guide: &Value) -> Vec<String> {
    guide["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| tag["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn undoing_a_tag_change_leaves_newer_tags_alone() {
    let h = Harness::new().await;
    h.tag("quick", "other").await;
    let a = h.create(PICCATA).await;
    let b = h.create(COOKIES).await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [a], "addTags": ["quick"] }),
    )
    .await;
    let renamed = h
        .ok(
            "recipes_tags_edit",
            json!({ "ops": [{ "op": "rename", "from": "quick", "to": "weeknight" }] }),
        )
        .await;
    // Since then: a new tag, registered and used.
    h.tag("dessert", "course").await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [b], "addTags": ["dessert"] }),
    )
    .await;

    h.ok("recipes_undo", json!({ "changeId": renamed["changeId"] }))
        .await;
    let guide = h.ok("recipes_guide", json!({})).await;
    assert_eq!(
        registry(&guide),
        ["dessert", "quick"],
        "the newer tag is still registered"
    );
    assert_eq!(
        h.ok("recipes_get", json!({ "id": a })).await["tags"],
        json!(["quick"])
    );
    assert_eq!(
        h.ok("recipes_get", json!({ "id": b })).await["tags"],
        json!(["dessert"])
    );

    // A later use of a name the change wrote blocks the undo.
    let again = h
        .ok(
            "recipes_tags_edit",
            json!({ "ops": [{ "op": "rename", "from": "quick", "to": "weeknight" }] }),
        )
        .await;
    let used = h
        .ok(
            "recipes_organize",
            json!({ "ids": [b], "addTags": ["weeknight"] }),
        )
        .await;
    let conflict = h
        .err("recipes_undo", json!({ "changeId": again["changeId"] }))
        .await;
    assert!(conflict.starts_with("[undo_conflict]"), "{conflict}");
    assert!(
        conflict.contains(used["changeId"].as_str().unwrap()),
        "{conflict}"
    );
    assert_eq!(
        h.ok("recipes_get", json!({ "id": b })).await["tags"],
        json!(["dessert", "weeknight"])
    );

    // So does a later createTags that registered a name the change deleted.
    let deleted = h
        .ok(
            "recipes_tags_edit",
            json!({ "ops": [{ "op": "delete", "name": "dessert" }] }),
        )
        .await;
    h.ok(
        "recipes_create",
        json!({ "document": doc(LASAGNE), "tags": ["dessert"], "createTags": true }),
    )
    .await;
    let conflict = h
        .err("recipes_undo", json!({ "changeId": deleted["changeId"] }))
        .await;
    assert!(conflict.starts_with("[undo_conflict]"), "{conflict}");
    // Two changes that only used the same tag do not block each other.
    let first = h
        .ok(
            "recipes_organize",
            json!({ "ids": [a], "addTags": ["dessert"] }),
        )
        .await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [b], "addTags": ["dessert"] }),
    )
    .await;
    h.ok("recipes_undo", json!({ "changeId": first["changeId"] }))
        .await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": a })).await["tags"],
        json!(["weeknight"])
    );
}

fn notes_op(n: u32, recipe: &str, text: &str, base: u32) -> Value {
    json!({ "ops": [{ "id": format!("op_notes{n:07}"), "at": 1, "type": "notes", "recipeId": recipe, "text": text, "baseRev": base }] })
}

#[tokio::test(start_paused = true)]
async fn stale_notes_are_merged_without_repeating_or_cutting_text() {
    let h = Harness::new().await;
    let router = h.router();
    let id = h.create(PICCATA).await;
    let notes = || async { h.ok("recipes_get", json!({ "id": id })).await["notes"].clone() };
    post_json(
        &router,
        "/api/ops",
        &notes_op(1, &id, "Use the big skillet.", 0),
    )
    .await;
    // A stale edit that already holds the server's text replaces it.
    post_json(
        &router,
        "/api/ops",
        &notes_op(2, &id, "Use the big skillet. Double the sauce.", 0),
    )
    .await;
    assert_eq!(notes().await, "Use the big skillet. Double the sauce.");
    // A stale edit the server's text already holds changes nothing.
    let same = post_json(
        &router,
        "/api/ops",
        &notes_op(3, &id, "Double the sauce.", 0),
    )
    .await
    .json();
    assert_eq!(same["results"][0]["ok"], true);
    assert_eq!(notes().await, "Use the big skillet. Double the sauce.");
    assert_eq!(same["results"][0]["recipe"]["notesRev"], 2);

    // Two long texts that do not fit together: the server's stays whole and
    // the app is told, so nothing is cut.
    let server = "a".repeat(15_000);
    let rev = same["results"][0]["recipe"]["notesRev"].as_u64().unwrap() as u32;
    post_json(&router, "/api/ops", &notes_op(4, &id, &server, rev)).await;
    let refused = post_json(
        &router,
        "/api/ops",
        &notes_op(5, &id, &"b".repeat(15_000), rev),
    )
    .await
    .json();
    assert_eq!(refused["results"][0]["ok"], false);
    assert!(
        refused["results"][0]["error"]
            .as_str()
            .unwrap()
            .contains("do not fit")
    );
    assert_eq!(notes().await, server);
}

#[tokio::test(start_paused = true)]
async fn a_source_shared_by_two_recipes_outlives_either() {
    let h = Harness::new().await;
    let url = "https://example.com/recipes/piccata";
    h.pages.serve(vec![FetchedPage {
        source: "fetch".to_owned(),
        url: url.to_owned(),
        text: Some("Chicken Piccata\n4 tbsp butter".to_owned()),
        ..FetchedPage::default()
    }]);
    let item = h.ok("recipes_import_url", json!({ "url": url })).await["inboxItemId"]
        .as_str()
        .unwrap()
        .to_owned();
    let first = h
        .ok(
            "recipes_create",
            json!({ "document": doc(PICCATA), "fromInbox": item }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut variation = doc(PICCATA);
    variation["title"] = json!("Piccata, lighter");
    let second = h
        .ok(
            "recipes_create",
            json!({ "document": variation, "fromInbox": item, "allowDuplicate": true }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The item still belongs to the recipe it first became.
    let stored = h.ok("recipes_inbox_get", json!({ "itemId": item })).await;
    assert_eq!(stored["recipeId"], first);

    h.ok(
        "recipes_organize",
        json!({ "ids": [first], "state": "trashed" }),
    )
    .await;
    h.advance_days(31).await;
    assert_eq!(h.recipes.purge().await.unwrap().recipes, 1);
    let kept = h.ok("recipes_get", json!({ "id": second })).await;
    assert_eq!(kept["source"]["itemId"], item);
    assert!(
        h.call_full("recipes_inbox_get", json!({ "itemId": item }))
            .await
            .is_ok()
    );

    // A merge hands the kept recipe the source it lacks; undo takes it back.
    let bare = h.create(COOKIES).await;
    let sourced = h
        .ok(
            "recipes_create",
            json!({ "document": doc(ENCHILADAS), "sourceText": "Enchiladas\n4 flour tortillas" }),
        )
        .await;
    let source =
        h.ok("recipes_get", json!({ "id": sourced["id"] })).await["source"]["itemId"].clone();
    let merged = h
        .ok(
            "recipes_merge",
            json!({ "keep": bare, "absorb": [sourced["id"]] }),
        )
        .await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": bare })).await["source"]["itemId"],
        source
    );
    h.ok("recipes_undo", json!({ "changeId": merged["changeId"] }))
        .await;
    assert!(
        h.ok("recipes_get", json!({ "id": bare }))
            .await
            .get("source")
            .is_none()
    );
    h.ok(
        "recipes_merge",
        json!({ "keep": bare, "absorb": [sourced["id"]] }),
    )
    .await;
    h.advance_days(31).await;
    h.recipes.purge().await.unwrap();
    assert_eq!(
        h.ok("recipes_get", json!({ "id": bare })).await["source"]["itemId"],
        source
    );
    assert!(
        h.call_full("recipes_inbox_get", json!({ "itemId": source }))
            .await
            .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn nothing_is_purged_before_a_verified_archive_holds_it() {
    let h = Harness::new().await;
    let gone = h.create(PICCATA).await;
    h.create(COOKIES).await;
    // A purge with nothing due writes no archive.
    h.recipes.purge().await.unwrap();
    let purge_dir = h.recipes.archives_dir().join("purge");
    assert!(!purge_dir.exists());

    h.ok(
        "recipes_organize",
        json!({ "ids": [gone], "state": "trashed" }),
    )
    .await;
    h.advance_days(31).await;
    assert_eq!(h.recipes.purge().await.unwrap().recipes, 1);
    let kept: Vec<_> = std::fs::read_dir(&purge_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(kept.len(), 1);
    let name = kept[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("fond-purge-") && name.ends_with(".json.gz"),
        "{name}"
    );
    let verified = verify_file(&kept[0], None).unwrap();
    assert_eq!(
        verified.counts["recipes_recipe"], 2,
        "the archive still holds the purged recipe"
    );

    // The hourly rotation never sees it.
    assert!(h.recipes.archive_files().unwrap().is_empty());
    assert!(matches!(
        h.recipes.backup().await.unwrap(),
        BackupOutcome::Written { .. }
    ));
    assert_eq!(h.recipes.archive_files().unwrap().len(), 1);
    assert!(kept[0].exists());

    // It is kept 180 days, then replaced by the next purge's.
    let other = h.create(LASAGNE).await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [other], "state": "trashed" }),
    )
    .await;
    h.advance_days(100).await;
    h.recipes.purge().await.unwrap();
    assert_eq!(std::fs::read_dir(&purge_dir).unwrap().count(), 2);
    let third = h.create(ENCHILADAS).await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [third], "state": "trashed" }),
    )
    .await;
    h.advance_days(100).await;
    h.recipes.purge().await.unwrap();
    assert!(!kept[0].exists(), "older than 180 days");
    assert_eq!(std::fs::read_dir(&purge_dir).unwrap().count(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_archive_follows_sessions_and_keeps_the_newest_by_time() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    assert!(matches!(
        h.recipes.backup().await.unwrap(),
        BackupOutcome::Written { .. }
    ));
    // Starting a session alone is something the next archive must hold.
    h.advance(std::time::Duration::from_secs(3600)).await;
    start(&h, &id).await;
    assert!(matches!(
        h.recipes.backup().await.unwrap(),
        BackupOutcome::Written { .. }
    ));

    // Rotation goes by the time in the name, not the sequence number: a
    // library restored from an older archive counts from a lower number.
    let dir = h.recipes.archives_dir();
    let stale = dir.join("fond-0000009999-20200101T000000Z.json.gz");
    std::fs::write(&stale, b"old").unwrap();
    let files = h.recipes.archive_files().unwrap();
    assert_eq!(files[0], stale, "the oldest by time sorts first");
    assert_eq!(files.len(), 3);

    // Orphaned renditions (no original beside them) are swept by the purge.
    let shard = h.recipes.files().root().join("ab");
    std::fs::create_dir_all(&shard).unwrap();
    let orphan = shard.join(format!("ab{}.thumb.jpg", "0".repeat(62)));
    std::fs::write(&orphan, b"jpeg").unwrap();
    h.recipes.purge().await.unwrap();
    assert!(!orphan.exists());
}
