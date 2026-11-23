//! Manual check against a copy of the production docstore (never the original):
//! `OMNI_PROD_COPY=/path/to/copy.db cargo test -p omni-recipes --test it prod_copy:: -- --ignored --nocapture`.
//!
//! Runs the one-time removal of the Crouton import's history on the copy and
//! checks, recipe by recipe, that only that history went: the owner's own
//! notes are what stood before the importer's block, the document differs in
//! its source alone, and the stored original is no longer linked.
#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use omni_api::fond::RecipeRecord;
use omni_recipes::Recipes;
use omni_testkit::{TestStore, test_clock};
use serde_json::Value;

/// After every timestamp the copy can hold.
const NOW_MS: i64 = 1_800_000_000_000;

async fn library(recipes: &Recipes) -> BTreeMap<String, RecipeRecord> {
    recipes
        .sync(None, None)
        .await
        .unwrap()
        .recipes
        .into_iter()
        .map(|record| (record.id.clone(), record))
        .collect()
}

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a copy of the production docstore"]
async fn import_history_leaves_and_everything_else_stays() {
    let source = std::env::var("OMNI_PROD_COPY").unwrap();
    let copy = TestStore::from_fixture(std::path::Path::new(&source)).await;
    let dir = tempfile::tempdir().unwrap();
    let recipes = Recipes::new(
        copy.store.clone(),
        test_clock(NOW_MS),
        jiff::tz::TimeZone::UTC,
        dir.path().to_path_buf(),
    );
    let before = library(&recipes).await;
    let removed = recipes.remove_import_history().await.unwrap().unwrap();
    println!(
        "{} recipes in the library; changed {} ({} sources, {} notes, {} stored originals), skipped {}",
        before.len(),
        removed.recipes,
        removed.sources,
        removed.notes,
        removed.originals,
        removed.skipped
    );
    assert_eq!(removed.skipped, 0);
    let after = library(&recipes).await;
    assert_eq!(before.len(), after.len());
    let mut shown = 0;
    for (id, old) in &before {
        let new = &after[id];
        let imported = old.doc["source"]["kind"] == "crouton";
        let marked = old.notes.contains("Imported from Crouton on ");
        // The document: nothing but the source.
        let strip = |doc: &Value| {
            let mut doc = doc.clone();
            doc.as_object_mut().unwrap().remove("source");
            doc
        };
        assert_eq!(
            strip(&old.doc),
            strip(&new.doc),
            "{id}: the recipe itself changed"
        );
        assert_eq!(old.tags, new.tags, "{id}");
        assert_eq!(old.images, new.images, "{id}");
        assert_eq!(
            (old.state, old.cook_count),
            (new.state, new.cook_count),
            "{id}"
        );
        if !imported && !marked {
            assert_eq!(old.doc, new.doc, "{id}");
            assert_eq!(old.notes, new.notes, "{id}");
            assert_eq!(old.source, new.source, "{id}");
            assert_eq!(old.revision, new.revision, "{id}");
            continue;
        }
        // The owner's notes: exactly what stood before the block.
        let own = old
            .notes
            .split("Imported from Crouton on ")
            .next()
            .unwrap()
            .trim_end();
        assert_eq!(new.notes, own, "{id}: notes");
        assert!(!new.notes.contains("Crouton"), "{id}: {}", new.notes);
        // The source: where it came from, and not through what.
        let source = &new.doc["source"];
        assert_ne!(source["kind"], "crouton", "{id}");
        assert_ne!(source["name"], "Crouton", "{id}");
        assert_eq!(source.get("url"), old.doc["source"].get("url"), "{id}");
        assert_eq!(
            source.get("author"),
            old.doc["source"].get("author"),
            "{id}"
        );
        assert_eq!(
            source.get("detail"),
            old.doc["source"].get("detail"),
            "{id}"
        );
        if old.doc["source"]["name"] != "Crouton" {
            assert_eq!(source.get("name"), old.doc["source"].get("name"), "{id}");
        }
        if imported {
            assert_eq!(new.revision, old.revision + 1, "{id}");
            assert!(
                new.source.is_none(),
                "{id}: the Crouton text is still linked"
            );
        }
        if shown < 2 && !own.is_empty() {
            shown += 1;
            println!("--- {} ({id})", old.doc["title"]);
            println!("source before: {}", old.doc["source"]);
            println!("source after:  {}", new.doc["source"]);
            println!("notes before:\n{}", old.notes);
            println!("notes after:\n{}", new.notes);
        }
    }
    // It runs once, and one of its changes comes back with an undo.
    assert!(recipes.remove_import_history().await.unwrap().is_none());
    let tools = omni_recipes::mcp::tools(std::sync::Arc::new(recipes.clone())).unwrap();
    let call = |name: &'static str, arguments: Value| {
        let tool = tools
            .iter()
            .find(|tool| tool.meta.name == name)
            .unwrap()
            .clone();
        async move {
            let context = omni_mcp_kit::ToolContext {
                call_id: "prod-copy".to_owned(),
                cancel: tokio_util::sync::CancellationToken::new(),
            };
            match tool.handler.call(arguments, context).await {
                Ok(omni_mcp_kit::ToolOutput::Structured(map)) => Value::Object(map),
                Ok(omni_mcp_kit::ToolOutput::Custom { structured, .. }) => {
                    Value::Object(structured)
                }
                Err(error) => panic!("{name}: {error:?}"),
            }
        }
    };
    let log = call("recipes_history", serde_json::json!({ "limit": 2 })).await;
    let newest = &log["changes"][0];
    let id = newest["recipeIds"][0].as_str().unwrap().to_owned();
    call(
        "recipes_undo",
        serde_json::json!({ "changeId": newest["changeId"] }),
    )
    .await;
    let restored = library(&recipes).await;
    assert_eq!(restored[&id].notes, before[&id].notes);
    assert_eq!(restored[&id].source, before[&id].source);
}
