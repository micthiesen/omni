//! The hourly archive, the restore drill and the retention of archives.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

mod common;

use std::time::Duration;

use common::{COOKIES, Harness, LASAGNE, PICCATA, jpeg, post_json, upload};
use omni_recipes::Recipes;
use omni_recipes::archive::{BackupOutcome, verify_file};
use omni_testkit::TestApp;
use serde_json::json;

/// A library with every kind of row: revisions, tags, images, an inbox item
/// of each state, a finished cook with adjustments, an active cook, a merge
/// and a change log.
async fn populated() -> Harness {
    let h = Harness::new().await;
    let router = h.router();
    h.tag("italian", "cuisine").await;
    h.tag("dessert", "course").await;
    let piccata = h
        .ok(
            "recipes_create",
            json!({ "document": common::doc(PICCATA), "tags": ["italian"], "sourceText": "Piccata\n4 tbsp butter", "notes": "Good." }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let cookies = h.create(COOKIES).await;
    let lasagne = h.create(LASAGNE).await;
    h.ok(
        "recipes_edit",
        json!({ "id": piccata, "expectedRevision": 1, "summary": "More parsley",
                "ops": [{ "op": "set", "target": "parsley", "fields": { "qty": 3 } }] }),
    )
    .await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [cookies], "addTags": ["dessert"], "state": "archived" }),
    )
    .await;
    for (item, count) in [("a", 2), ("b", 1)] {
        for index in 0..count {
            let reply = upload(
                &router,
                &format!("/api/inbox?item={item}&index={index}&count={count}"),
                jpeg(index as u8 + 10),
            )
            .await;
            assert_eq!(reply.status, 200, "{}", reply.text());
        }
    }
    let items = h.ok("recipes_inbox_list", json!({})).await;
    h.ok(
        "recipes_inbox_resolve",
        json!({ "itemIds": [items["items"][0]["itemId"]], "resolution": "discarded" }),
    )
    .await;
    let start = post_json(
        &router,
        "/api/cook",
        &json!({ "id": "cook_aaaaaaaa", "recipeId": piccata, "revision": 2, "scale": 2.0, "at": 5 }),
    )
    .await;
    assert_eq!(start.status, 200);
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": "cook_aaaaaaaa", "events": [
            { "type": "check", "target": "dredge" },
            { "type": "adjust", "target": "sear", "text": "Two batches." },
            { "type": "finish", "note": "Fine." }
        ] }),
    )
    .await;
    let photo = upload(&router, "/api/cook/cook_aaaaaaaa/photo?id=p1", jpeg(40)).await;
    assert_eq!(photo.status, 200);
    h.ok("recipes_cook_start", json!({ "recipeId": lasagne }))
        .await;
    let mut twin = common::doc(LASAGNE);
    twin["title"] = json!("Lasagne (duplicate)");
    let twin = h.ok("recipes_create", json!({ "document": twin })).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    h.ok(
        "recipes_merge",
        json!({ "keep": lasagne, "absorb": [twin] }),
    )
    .await;
    h
}

#[tokio::test(start_paused = true)]
async fn an_archive_restores_into_an_empty_library_table_for_table() {
    let h = populated().await;
    let outcome = h.recipes.backup().await.unwrap();
    let BackupOutcome::Written { archive_id, seq } = outcome else {
        panic!("expected an archive: {outcome:?}");
    };
    assert!(archive_id.starts_with("fond-"));
    assert!(seq > 10);
    let files = h.recipes.archive_files().unwrap();
    assert_eq!(files.len(), 1);
    let path = files[0].clone();
    assert!(path.to_string_lossy().ends_with(".json.gz"));

    // The archive verifies, and so does every image it refers to.
    let verified = verify_file(&path, Some(h.recipes.dir())).unwrap();
    assert_eq!(verified.archive_id, archive_id);
    assert_eq!(verified.images_checked, Some(3));
    assert_eq!(verified.counts["recipes_recipe"], 4);
    assert_eq!(verified.counts["recipes_revision"], 5);
    assert_eq!(verified.counts["recipes_cook"], 2);
    assert_eq!(verified.counts["recipes_adjustment"], 1);
    assert_eq!(verified.counts["recipes_inbox"], 3);

    // Restore into a second, empty store.
    let other = TestApp::new().await;
    let restored_to = Recipes::from_context(&other.ctx);
    let restored = restored_to.restore(path.clone()).await.unwrap();
    assert_eq!(restored.archive_id, archive_id);
    assert_eq!(restored.seq, seq);
    let (original, copy) = (
        h.recipes.dump().await.unwrap(),
        restored_to.dump().await.unwrap(),
    );
    assert_eq!(original.len(), 12);
    for (table, rows) in &original {
        assert_eq!(copy[table], *rows, "{table}");
        if table != "recipes_redirect" {
            assert!(
                !rows.as_array().unwrap().is_empty(),
                "{table} was empty in the drill"
            );
        }
    }

    // The restored library works: search, reads, history, undo data, sequence.
    let tools = omni_recipes::mcp::tools(std::sync::Arc::new(restored_to.clone())).unwrap();
    let call = |name: &'static str, arguments: serde_json::Value| {
        let tool = tools
            .iter()
            .find(|tool| tool.meta.name == name)
            .unwrap()
            .clone();
        async move {
            let context = omni_mcp_kit::ToolContext {
                call_id: "restore".to_owned(),
                cancel: tokio_util::sync::CancellationToken::new(),
            };
            match tool.handler.call(arguments, context).await.unwrap() {
                omni_mcp_kit::ToolOutput::Structured(map) => serde_json::Value::Object(map),
                omni_mcp_kit::ToolOutput::Custom { structured, .. } => {
                    serde_json::Value::Object(structured)
                }
            }
        }
    };
    let found = call("recipes_search", json!({ "query": "piccata parsley" })).await;
    assert_eq!(found["total"], 1);
    assert_eq!(found["recipes"][0]["revision"], 2);
    assert_eq!(found["recipes"][0]["cookCount"], 1);
    assert_eq!(found["recipes"][0]["openAdjustments"], 1);
    let history = call("recipes_history", json!({})).await;
    assert!(history["total"].as_u64().unwrap() >= 8);
    assert_eq!(restored_to.sync(None, None).await.unwrap().seq, seq);

    // A restore never lands on a library that holds anything.
    let refused = restored_to.restore(path.clone()).await.unwrap_err();
    assert_eq!(refused.code(), "archive");
    assert!(refused.to_string().contains("not empty"), "{refused}");
    let refused = h.recipes.restore(path.clone()).await.unwrap_err();
    assert_eq!(refused.code(), "archive");

    // A damaged archive is refused whole.
    let damaged = h.dist.path().join("damaged.json.gz");
    let mut bytes = std::fs::read(&path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(&damaged, &bytes).unwrap();
    assert!(verify_file(&damaged, None).is_err());
    let third = TestApp::new().await;
    assert!(
        Recipes::from_context(&third.ctx)
            .restore(damaged)
            .await
            .is_err()
    );
    // A missing image fails verification with the directory, not without.
    let digest = h.recipes.files().digests().unwrap()[0].clone();
    h.recipes.files().remove(&digest).unwrap();
    assert!(verify_file(&path, Some(h.recipes.dir())).is_err());
    assert!(verify_file(&path, None).is_ok());
}

#[tokio::test(start_paused = true)]
async fn the_backup_runs_only_when_the_library_moved() {
    let h = Harness::new().await;
    assert_eq!(h.recipes.backup().await.unwrap(), BackupOutcome::Empty);
    let id = h.create(PICCATA).await;
    let BackupOutcome::Written {
        archive_id: first, ..
    } = h.recipes.backup().await.unwrap()
    else {
        panic!("expected an archive");
    };
    assert_eq!(
        h.recipes.backup().await.unwrap(),
        BackupOutcome::UpToDate {
            archive_id: first.clone()
        }
    );
    // A read changes nothing.
    h.ok("recipes_get", json!({ "id": id })).await;
    h.ok("recipes_search", json!({ "query": "piccata" })).await;
    assert!(matches!(
        h.recipes.backup().await.unwrap(),
        BackupOutcome::UpToDate { .. }
    ));

    // A write does, and so does a cook event (which leaves the library's
    // sequence number alone).
    h.advance(Duration::from_secs(3600)).await;
    h.tag("italian", "cuisine").await;
    let BackupOutcome::Written {
        archive_id: second, ..
    } = h.recipes.backup().await.unwrap()
    else {
        panic!("expected an archive");
    };
    assert_ne!(first, second);
    let session = h.ok("recipes_cook_start", json!({ "recipeId": id })).await["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    h.advance(Duration::from_secs(3600)).await;
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": session, "events": [{ "type": "check", "target": "dredge" }] }),
    )
    .await;
    assert!(matches!(
        h.recipes.backup().await.unwrap(),
        BackupOutcome::Written { .. }
    ));
    assert_eq!(h.recipes.archive_files().unwrap().len(), 3);

    // A lost archive file is written again.
    for file in h.recipes.archive_files().unwrap() {
        std::fs::remove_file(file).unwrap();
    }
    assert!(matches!(
        h.recipes.backup().await.unwrap(),
        BackupOutcome::Written { .. }
    ));

    // The newest 72 are kept.
    for round in 0..75 {
        h.advance(Duration::from_secs(3600)).await;
        h.ok(
            "recipes_organize",
            json!({ "ids": [id], "state": if round % 2 == 0 { "archived" } else { "active" } }),
        )
        .await;
        assert!(matches!(
            h.recipes.backup().await.unwrap(),
            BackupOutcome::Written { .. }
        ));
    }
    let kept = h.recipes.archive_files().unwrap();
    assert_eq!(kept.len(), 72);
    // No file a file-level backup would skip.
    for entry in std::fs::read_dir(h.recipes.archives_dir()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(name.ends_with(".json.gz"), "{name}");
    }
    assert!(verify_file(kept.last().unwrap(), Some(h.recipes.dir())).is_ok());
}

#[tokio::test(start_paused = true)]
async fn the_subsystem_registers_its_tools_tasks_and_host_app() {
    let app = TestApp::new().await;
    let subsystem = omni_recipes::subsystem(&app.ctx).unwrap();
    assert_eq!(subsystem.name, "recipes");
    assert_eq!(subsystem.mcp_tools.len(), 21);
    let tasks: Vec<&str> = subsystem.tasks.iter().map(|task| task.name()).collect();
    assert_eq!(tasks, ["RecipesBackup", "RecipesPurge"]);
    assert_eq!(subsystem.services.len(), 1);
    assert_eq!(subsystem.boot_steps.len(), 1);
    // Without RECIPES_APP_URL no app is served, and the tools still work.
    assert!(subsystem.host_apps.is_empty());

    let mut env = omni_testkit::test_app_env();
    env.insert(
        "RECIPES_APP_URL".to_owned(),
        "https://recipes.example.com".to_owned(),
    );
    let ctx = omni_runtime::AppContext {
        config: std::sync::Arc::new(omni_config::Config::from_env(&env).unwrap()),
        ..app.ctx.clone()
    };
    let subsystem = omni_recipes::subsystem(&ctx).unwrap();
    assert_eq!(subsystem.host_apps.len(), 1);
    assert_eq!(subsystem.host_apps[0].host(), "recipes.example.com");
    // An agent is told where the session opens.
    let recipes = omni_recipes::service(&ctx);
    let tools = omni_recipes::mcp::tools(std::sync::Arc::new(recipes)).unwrap();
    assert_eq!(tools.len(), 21);
}
