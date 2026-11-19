//! The one-time removal of what the Crouton import left about itself: the
//! source kind, the block in the notes and the stored Crouton text. The
//! owner's own notes and the recipe stay, and every change can be undone.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{COOKIES, Harness, LASAGNE, PICCATA, get};
use omni_recipes::library::{without_import_block, without_import_provenance};
use serde_json::{Value, json};

const BLOCK: &str = "Imported from Crouton on 2026-11-18.\nChanges: 2 cups flour -> 240 g; boiling the water made its own step.";

async fn imported(h: &Harness, sample: &str, source: Value, own_notes: &str) -> String {
    let mut document = common::doc(sample);
    document["source"] = source;
    let notes = if own_notes.is_empty() {
        BLOCK.to_owned()
    } else {
        format!("{own_notes}\n\n{BLOCK}")
    };
    let made = h
        .ok(
            "recipes_create",
            json!({
                "keepDietConflicts": true, "allowDuplicate": true, "review": false,
                "document": document, "notes": notes,
                "sourceText": "TITLE\nfrom the crumb file\n2 cups flour"
            }),
        )
        .await;
    made["id"].as_str().unwrap().to_owned()
}

#[test]
fn the_importers_block_is_cut_and_the_owners_words_are_kept() {
    let own = "Day-two soup is better.\n\n\t•\tGo easy on salt.";
    assert_eq!(
        without_import_block(&format!("{own}\n\n{BLOCK}")).as_deref(),
        Some(own)
    );
    assert_eq!(without_import_block(BLOCK).as_deref(), Some(""));
    // Something the owner wrote after it stays too.
    assert_eq!(
        without_import_block(&format!("Mine.\n\n{BLOCK}\n\nAdded later.")).as_deref(),
        Some("Mine.\n\nAdded later.")
    );
    // Notes without the block, even ones that mention Crouton, are left alone.
    assert_eq!(
        without_import_block("I used to keep this in Crouton."),
        None
    );
    assert_eq!(without_import_block(""), None);
}

#[test]
fn a_source_names_where_the_recipe_came_from_not_the_app_it_passed_through() {
    let source = |value: Value| -> Value {
        let source: omni_recipe_format::Source = serde_json::from_value(value).unwrap();
        serde_json::to_value(without_import_provenance(&source)).unwrap()
    };
    assert_eq!(
        source(
            json!({ "kind": "crouton", "name": "SuperValu", "url": "https://supervalu.ie/r", "author": "A" })
        ),
        json!({ "kind": "web", "name": "SuperValu", "url": "https://supervalu.ie/r", "author": "A" })
    );
    assert_eq!(
        source(json!({ "kind": "crouton", "name": "Crouton" })),
        json!({ "kind": "original" })
    );
    assert_eq!(
        source(json!({ "kind": "crouton", "name": "Crouton", "detail": "Courtesy of Veganuary" })),
        json!({ "kind": "person", "detail": "Courtesy of Veganuary" })
    );
}

#[tokio::test(start_paused = true)]
async fn import_history_is_removed_once_as_undoable_changes() {
    let h = Harness::new().await;
    let router = h.router();
    let web = imported(
        &h,
        PICCATA,
        json!({ "kind": "crouton", "name": "SuperValu", "url": "https://supervalu.ie/recipes/piccata" }),
        "Michael's own note.\nSecond line.",
    )
    .await;
    let own = imported(
        &h,
        COOKIES,
        json!({ "kind": "crouton", "name": "Crouton" }),
        "",
    )
    .await;
    // A recipe that never came through Crouton is not touched.
    let other = h
        .ok(
            "recipes_create",
            json!({
                "keepDietConflicts": true, "review": false, "document": common::doc(LASAGNE),
                "notes": "Mine.", "sourceText": "a pasted original"
            }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let before = h.ok("recipes_get", json!({ "id": web })).await;
    assert_eq!(before["source"]["kind"], "text");
    let untouched = h.ok("recipes_get", json!({ "id": other })).await;

    let removed = h.recipes.remove_import_history().await.unwrap().unwrap();
    assert_eq!(
        (
            removed.recipes,
            removed.sources,
            removed.notes,
            removed.originals,
            removed.skipped
        ),
        (2, 2, 2, 2, 0)
    );

    let after = h.ok("recipes_get", json!({ "id": web })).await;
    assert_eq!(after["notes"], "Michael's own note.\nSecond line.");
    assert_eq!(
        after["document"]["source"],
        json!({ "kind": "web", "name": "SuperValu", "url": "https://supervalu.ie/recipes/piccata" })
    );
    assert!(after.get("source").is_none(), "{}", after["source"]);
    assert_eq!(after["headRevision"], 2);
    // The recipe itself is as it was.
    assert_eq!(
        after["document"]["components"],
        before["document"]["components"]
    );
    assert_eq!(after["document"]["title"], before["document"]["title"]);
    let plain = h.ok("recipes_get", json!({ "id": own })).await;
    assert_eq!(plain["notes"], "");
    assert_eq!(plain["document"]["source"], json!({ "kind": "original" }));
    assert_eq!(h.ok("recipes_get", json!({ "id": other })).await, untouched);
    // The app no longer offers the Crouton text as the original.
    assert_eq!(
        get(&router, &format!("/api/original/{web}")).await.status,
        404
    );
    assert_eq!(
        get(&router, &format!("/api/original/{other}")).await.status,
        200
    );
    let record = get(&router, "/api/sync").await.json()["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|recipe| recipe["id"] == web.as_str())
        .unwrap()
        .clone();
    assert_eq!(record["source"], Value::Null);
    assert_eq!(record["notes"], "Michael's own note.\nSecond line.");

    // It runs once.
    assert!(h.recipes.remove_import_history().await.unwrap().is_none());
    assert_eq!(
        h.ok("recipes_get", json!({ "id": web })).await["headRevision"],
        2
    );

    // Each part is an ordinary logged change, newest first, and comes back.
    let history = h.ok("recipes_history", json!({ "id": web })).await;
    assert_eq!(
        history["revisions"][0]["summary"],
        "Remove Crouton import provenance"
    );
    assert_eq!(history["revisions"][0]["tool"], "system");
    let log = h.ok("recipes_history", json!({ "limit": 20 })).await;
    let changes: Vec<&Value> = log["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|change| {
            change["recipeIds"]
                .as_array()
                .unwrap()
                .contains(&json!(web))
        })
        .filter(|change| change["tool"] == "system")
        .collect();
    assert_eq!(changes.len(), 2, "{log}");
    assert!(
        changes[0]["summary"]
            .as_str()
            .unwrap()
            .starts_with("Crouton import notes and stored original removed")
    );
    h.ok(
        "recipes_undo",
        json!({ "changeId": changes[0]["changeId"] }),
    )
    .await;
    let restored = h.ok("recipes_get", json!({ "id": web })).await;
    assert_eq!(restored["notes"], before["notes"]);
    assert_eq!(restored["source"]["kind"], "text");
    assert_eq!(
        get(&router, &format!("/api/original/{web}")).await.status,
        200
    );
    h.ok(
        "recipes_undo",
        json!({ "changeId": changes[1]["changeId"] }),
    )
    .await;
    let restored = h.ok("recipes_get", json!({ "id": web })).await;
    assert_eq!(restored["document"]["source"]["kind"], "crouton");
    assert_eq!(restored["headRevision"], 3);

    // Left alone, the Crouton text is purged with the other discarded items.
    h.advance_days(31).await;
    h.recipes.purge().await.unwrap();
    let left = h
        .ok("recipes_inbox_list", json!({ "state": "discarded" }))
        .await;
    assert_eq!(left["total"], 0);
    assert_eq!(
        get(&router, &format!("/api/original/{web}")).await.status,
        200
    );
}
