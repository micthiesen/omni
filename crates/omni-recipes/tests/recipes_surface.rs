//! What the tools promise an agent: every unknown tag at once, change ids on
//! every write, distinct codes, limits on what the open app surface can pile
//! up, and no fetch on a dry run.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use common::{
    COOKIES, ENCHILADAS, HOST, Harness, LASAGNE, PICCATA, doc, jpeg, post_json, send, upload,
};
use omni_runtime::ports::FetchedPage;
use serde_json::{Value, json};

#[tokio::test(start_paused = true)]
async fn every_unknown_tag_is_reported_at_once() {
    let h = Harness::new().await;
    h.tag("weeknight", "occasion").await;
    h.tag("italian", "cuisine").await;
    let refused = h
        .err(
            "recipes_create",
            json!({ "document": doc(PICCATA), "tags": ["weeknights", "italien", "italian", "zzz"] }),
        )
        .await;
    assert_eq!(
        refused,
        "[unknown_tag] tag \"italien\" is not in the registry; nearest: \"italian\". tag \"weeknights\" is not in the registry; nearest: \"weeknight\". tag \"zzz\" is not in the registry; no registered tag is close to it. Use existing tags, or pass createTags to add these to the registry (group \"other\"; recipes_tags_edit sets the group)."
    );
    let bad = h
        .err(
            "recipes_create",
            json!({ "document": doc(PICCATA), "tags": ["a/b"] }),
        )
        .await;
    assert!(bad.starts_with("[validation] tag \"a/b\" must be 1 to 40 characters: letters, digits, hyphens, underscores"), "{bad}");

    // Search says which tag filters name nothing instead of returning nothing silently.
    h.ok(
        "recipes_create",
        json!({ "document": doc(PICCATA), "tags": ["italian"] }),
    )
    .await;
    let found = h
        .ok(
            "recipes_search",
            json!({ "tags": ["italien"], "notTags": ["weeknigt"] }),
        )
        .await;
    assert_eq!(found["total"], 0);
    assert_eq!(
        found["unknownTags"],
        json!([
            { "name": "italien", "nearest": ["italian"] },
            { "name": "weeknigt", "nearest": ["weeknight"] }
        ])
    );
    let known = h.ok("recipes_search", json!({ "tags": ["italian"] })).await;
    assert!(known.get("unknownTags").is_none());
    let invalid = h.err("recipes_search", json!({ "anyTags": ["a/b"] })).await;
    assert!(invalid.starts_with("[validation]"), "{invalid}");
}

#[tokio::test(start_paused = true)]
async fn every_content_write_returns_a_change_id_that_undo_takes() {
    let h = Harness::new().await;
    let base = h.create(LASAGNE).await;
    let created = h
        .ok(
            "recipes_create",
            json!({ "document": doc(PICCATA), "derivedFrom": base }),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(created["changeId"].as_str().unwrap().starts_with("chg_"));
    // A variation remembers what it was derived from.
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["derivedFrom"],
        base
    );
    let missing = h
        .err(
            "recipes_create",
            json!({ "document": doc(COOKIES), "derivedFrom": "rcp_zzzzzzzz" }),
        )
        .await;
    assert!(missing.starts_with("[not_found]"), "{missing}");

    // A cook leaves an adjustment for an edit to fold in.
    let session = h.ok("recipes_cook_start", json!({ "recipeId": id })).await["sessionId"].clone();
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": session, "events": [
            { "type": "adjust", "target": "simmer", "text": "Simmer 8 minutes." },
            { "type": "finish" }
        ] }),
    )
    .await;
    let adjustment =
        h.ok("recipes_get", json!({ "id": id })).await["openAdjustments"][0]["id"].clone();
    let edited = h
        .ok(
            "recipes_edit",
            json!({ "id": id, "expectedRevision": 1, "summary": "Longer simmer", "resolves": [adjustment],
                    "ops": [{ "op": "set", "target": "simmer", "fields": { "time": { "min": 8, "attention": "active" } } }] }),
        )
        .await;
    assert_eq!(edited["revision"], 2);
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["openAdjustments"],
        json!([])
    );
    // Undoing that edit puts the document back and reopens the adjustment.
    h.ok("recipes_undo", json!({ "changeId": edited["changeId"] }))
        .await;
    let undone = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(undone["headRevision"], 3);
    assert_eq!(undone["document"], doc(PICCATA));
    assert_eq!(undone["openAdjustments"][0]["id"], adjustment);

    let mut rewritten = doc(PICCATA);
    rewritten["title"] = json!("Piccata, rewritten");
    let replaced = h
        .ok(
            "recipes_replace",
            json!({ "id": id, "expectedRevision": 3, "summary": "Retitle", "document": rewritten }),
        )
        .await;
    h.ok("recipes_undo", json!({ "changeId": replaced["changeId"] }))
        .await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["title"],
        "Chicken Piccata"
    );
    let reverted = h
        .ok(
            "recipes_revert",
            json!({ "id": id, "expectedRevision": 5, "toRevision": 4 }),
        )
        .await;
    assert_eq!(reverted["revision"], 6);
    h.ok("recipes_undo", json!({ "changeId": reverted["changeId"] }))
        .await;
    let head = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(
        (head["headRevision"].clone(), head["title"].clone()),
        (json!(7), json!("Chicken Piccata"))
    );

    // Dry runs and unchanged writes carry no change id.
    let dry = h
        .ok(
            "recipes_revert",
            json!({ "id": id, "expectedRevision": 7, "toRevision": 4, "dryRun": true }),
        )
        .await;
    assert!(dry.get("changeId").is_none() && dry.get("problems").is_none());
    // Undoing the create sends the recipe to the trash; a write to it says so.
    h.ok("recipes_undo", json!({ "changeId": created["changeId"] }))
        .await;
    let trashed = h
        .err(
            "recipes_revert",
            json!({ "id": id, "expectedRevision": 7, "toRevision": 1 }),
        )
        .await;
    assert!(
        trashed.starts_with("[trashed]") && trashed.contains("recipes_organize"),
        "{trashed}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_revert_to_a_revision_the_rules_now_refuse_lists_the_problems() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    // A revision from before a rule existed: capers used by no step.
    let mut old = doc(PICCATA);
    let steps = old["components"][0]["steps"].as_array_mut().unwrap();
    for step in steps.iter_mut() {
        step["inputs"]
            .as_array_mut()
            .unwrap()
            .retain(|input| input["ref"] != "capers");
        let text = step["text"].as_str().unwrap().replace(" and {capers}", "");
        step["text"] = json!(text);
    }
    let (old, now, recipe) = (old.to_string(), doc(PICCATA).to_string(), id.clone());
    h.recipes
        .store()
        .write(move |tx| {
            let conn = tx.connection();
            for (rev, text) in [(2, &old), (3, &now)] {
                conn.execute(
                    "INSERT INTO recipes_revision VALUES (?1, ?2, 1, ?3, 'x', 'seeded', 'test', '', 0)",
                    rusqlite::params![recipe, rev, text],
                )
                .unwrap();
            }
            conn.execute("UPDATE recipes_recipe SET head_rev = 3 WHERE id = ?1", [&recipe])
                .unwrap();
            Ok::<_, omni_store::StoreError>(())
        })
        .await
        .unwrap();
    let arguments =
        |dry: bool| json!({ "id": id, "expectedRevision": 3, "toRevision": 2, "dryRun": dry });
    let dry = h.ok("recipes_revert", arguments(true)).await;
    assert_eq!(dry["saved"], false);
    assert_eq!(dry["problems"][0]["code"], "unused_ingredient");
    let refused = h.err("recipes_revert", arguments(false)).await;
    assert!(
        refused.starts_with("[invalid_recipe] 1 problems"),
        "{refused}"
    );
}

#[tokio::test(start_paused = true)]
async fn duplicates_are_found_by_either_url_and_follow_an_edited_source() {
    let h = Harness::new().await;
    let page = "https://example.com/blog/piccata";
    h.pages.serve(vec![FetchedPage {
        source: "fetch".to_owned(),
        url: page.to_owned(),
        text: Some(format!(
            "Chicken Piccata\n{}",
            "Season, dredge and sear the cutlets. ".repeat(20)
        )),
        ..FetchedPage::default()
    }]);
    let item = h.ok("recipes_import_url", json!({ "url": page })).await["inboxItemId"].clone();
    let with_url = |title: &str, url: &str| {
        let mut document = doc(PICCATA);
        document["title"] = json!(title);
        document["source"] = json!({ "kind": "web", "name": "Example", "url": url });
        document
    };
    // The document names the canonical URL; the page was read somewhere else.
    let created = h
        .ok(
            "recipes_create",
            json!({ "document": with_url("Piccata", "https://example.com/recipes/piccata"), "fromInbox": item }),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    for url in [
        "https://example.com/recipes/piccata/",
        "https://www.example.com/blog/piccata#top",
    ] {
        let refused = h
            .err(
                "recipes_create",
                json!({ "document": with_url("Again", url) }),
            )
            .await;
        assert!(
            refused.starts_with("[duplicate_source] a recipe from this source URL"),
            "{url}: {refused}"
        );
        assert!(refused.contains(&id));
    }
    // A revision that names another source is found by it.
    h.ok(
        "recipes_edit",
        json!({ "id": id, "expectedRevision": 1, "summary": "Moved",
                "ops": [{ "op": "setRecipe", "fields": { "source": { "kind": "web", "name": "Example", "url": "https://example.com/new/piccata" } } }] }),
    )
    .await;
    let moved = h
        .err(
            "recipes_create",
            json!({ "document": with_url("Again", "https://example.com/new/piccata") }),
        )
        .await;
    assert!(moved.starts_with("[duplicate_source]"), "{moved}");
    // A link that is not a web address is refused by the format.
    let script = h
        .err(
            "recipes_create",
            json!({ "document": with_url("Script", "javascript:alert(1)") }),
        )
        .await;
    assert!(script.contains("source.url: [bad_value]"), "{script}");

    // Neither an import by an agent nor a create announces an inbox item.
    assert!(h.events.published().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_dry_run_fetches_and_stores_nothing() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    use base64::Engine as _;
    let inline = base64::engine::general_purpose::STANDARD.encode(common::png(1));
    // The test app has no network: a real add by URL fails, a dry run does not try.
    let dry = h
        .ok(
            "recipes_images_edit",
            json!({ "id": id, "dryRun": true, "add": [
                { "url": "https://images.example.com/a.jpg" },
                { "dataBase64": inline, "mimeType": "image/png" }
            ] }),
        )
        .await;
    assert_eq!(dry["images"], json!([]));
    assert!(
        h.recipes.files().digests().unwrap().is_empty(),
        "no file was written"
    );
    let bad = h
        .err(
            "recipes_images_edit",
            json!({ "id": id, "dryRun": true, "add": [{ "url": "file:///etc/passwd" }] }),
        )
        .await;
    assert!(bad.starts_with("[validation]"), "{bad}");
    let mismatch = h
        .err(
            "recipes_images_edit",
            json!({ "id": id, "dryRun": true, "add": [{ "dataBase64": inline, "mimeType": "image/jpeg" }] }),
        )
        .await;
    assert!(mismatch.starts_with("[unsupported_media]"), "{mismatch}");

    // The scale bounds are the ones the schema states.
    let tiny = h
        .err(
            "recipes_get",
            json!({ "id": id, "view": "text", "servings": 0.1 }),
        )
        .await;
    assert!(
        tiny.starts_with("[validation]") && tiny.contains("between 0.05 and 100"),
        "{tiny}"
    );
    let schema = h
        .err(
            "recipes_get",
            json!({ "id": id, "view": "text", "scale": 0.01 }),
        )
        .await;
    assert!(
        !schema.starts_with('['),
        "the schema refuses it first: {schema}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_open_app_surface_is_bounded() {
    let h = Harness::new().await;
    let router = h.router();
    let id = h.create(PICCATA).await;

    // Twenty open sessions; the next start is told to come back.
    for n in 0..20 {
        let reply = post_json(
            &router,
            "/api/cook",
            &json!({ "id": format!("cook_limit{n:03}"), "recipeId": id, "revision": 1, "scale": 1, "at": 1 }),
        )
        .await;
        assert_eq!(reply.status, 200, "{n}: {}", reply.text());
    }
    let over = post_json(
        &router,
        "/api/cook",
        &json!({ "id": "cook_limit999", "recipeId": id, "revision": 1, "scale": 1, "at": 1 }),
    )
    .await;
    assert_eq!(over.status, 429, "{}", over.text());
    let tool = h.err("recipes_cook_start", json!({ "recipeId": id })).await;
    assert!(
        tool.starts_with("[too_many] 20 cook sessions are open"),
        "{tool}"
    );
    // An existing session still answers, and finishing one makes room.
    let again = post_json(
        &router,
        "/api/cook",
        &json!({ "id": "cook_limit000", "recipeId": id, "revision": 1, "scale": 1, "at": 1 }),
    )
    .await;
    assert_eq!(again.status, 200);
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": "cook_limit000", "events": [{ "type": "finish" }] }),
    )
    .await;
    assert!(
        h.call_full("recipes_cook_start", json!({ "recipeId": id }))
            .await
            .is_ok()
    );

    // Sixteen event streams; the seventeenth is refused, none is dropped.
    let held: Vec<_> = (0..16).map(|_| h.recipes.hub().subscribe()).collect();
    let stream = send(
        &router,
        Request::get("/api/events")
            .header(header::HOST, HOST)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(stream.status, 503);
    assert_eq!(h.recipes.hub().clients(), 16);
    drop(held);

    // Two hundred waiting inbox items; a new capture and a new import wait.
    h.recipes
        .store()
        .write(|tx| {
            for n in 0..200 {
                tx.connection()
                    .execute(
                        "INSERT INTO recipes_inbox (id, kind, state, payload, created_at)
                         VALUES (?1, 'text', 'pending', '{}', 0)",
                        [format!("inb_fill{n:04}")],
                    )
                    .unwrap();
            }
            Ok::<_, omni_store::StoreError>(())
        })
        .await
        .unwrap();
    let full = upload(&router, "/api/inbox?item=one-more&index=0&count=1", jpeg(1)).await;
    assert_eq!(full.status, 429, "{}", full.text());
    assert!(
        h.recipes.files().digests().unwrap().is_empty(),
        "refused before storing"
    );
    h.pages.serve(vec![FetchedPage {
        source: "fetch".to_owned(),
        url: "https://example.com/x".to_owned(),
        text: Some("A recipe with enough text to keep. ".repeat(20)),
        ..FetchedPage::default()
    }]);
    let import = h
        .err(
            "recipes_import_url",
            json!({ "url": "https://example.com/x" }),
        )
        .await;
    assert!(import.starts_with("[too_many]"), "{import}");
    h.ok(
        "recipes_inbox_resolve",
        json!({ "itemIds": ["inb_fill0000"], "resolution": "discarded" }),
    )
    .await;
    assert_eq!(
        upload(&router, "/api/inbox?item=one-more&index=0&count=1", jpeg(1))
            .await
            .status,
        200
    );
}

#[tokio::test(start_paused = true)]
async fn timers_and_their_alerts_are_bounded() {
    let h = Harness::new().await;
    // One recipe with many steps to time: the cookies have nine, the others add up.
    let mut sessions = Vec::new();
    for json in [COOKIES, PICCATA, LASAGNE, ENCHILADAS] {
        let id = h.create(json).await;
        let session = h.ok("recipes_cook_start", json!({ "recipeId": id })).await["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let steps: Vec<String> = doc(json)["components"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|component| component["steps"].as_array().unwrap().clone())
            .map(|step| step["id"].as_str().unwrap().to_owned())
            .collect();
        sessions.push((session, steps));
    }
    let mut started = 0;
    for (session, steps) in &sessions {
        let events: Vec<Value> = steps
            .iter()
            .map(|step| json!({ "type": "timer", "step": step, "minutes": 1 }))
            .collect();
        started += events.len();
        h.ok(
            "recipes_cook_update",
            json!({ "sessionId": session, "events": events }),
        )
        .await;
    }
    assert!(started > 20, "{started}");
    h.advance(Duration::from_secs(65)).await;
    // Ten alerts reach the phone in an hour; the rest are recorded and dropped.
    assert_eq!(h.recipes.tick().await.unwrap().alerts, 10);
    assert_eq!(h.app.pushes.all().len(), 10);
    h.advance(Duration::from_secs(30)).await;
    assert_eq!(h.recipes.tick().await.unwrap().alerts, 0);
    h.advance(Duration::from_secs(2 * 3600)).await;
    assert_eq!(
        h.recipes.tick().await.unwrap().alerts,
        0,
        "suppressed alerts are never sent later"
    );
    assert_eq!(h.app.pushes.all().len(), 10);

    // A session runs at most thirty timers. The lasagne has fourteen steps:
    // not enough to reach the limit by step ids, so the limit is checked on
    // the stored events directly.
    let (session, steps) = &sessions[0];
    let step = steps[0].clone();
    let sid = session.clone();
    h.recipes
        .store()
        .write(move |tx| {
            for n in 0..30 {
                let payload = json!({ "type": "timer", "step": format!("extra{n}"), "endsAt": 9_999_999_999_999_i64 });
                tx.connection()
                    .execute(
                        "INSERT INTO recipes_cook_event (session_id, seq, client_event_id, at, actor, type, payload)
                         VALUES (?1, ?2, ?3, 0, 'app', 'timer', ?4)",
                        rusqlite::params![sid, 1000 + n, format!("fill-{n}"), payload.to_string()],
                    )
                    .unwrap();
            }
            Ok::<_, omni_store::StoreError>(())
        })
        .await
        .unwrap();
    // Restarting a running timer is not one more; a new one is.
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": session, "events": [
            { "type": "timer", "step": step, "minutes": 5 },
            { "type": "cancelTimer", "step": step }
        ] }),
    )
    .await;
    let refused = h
        .err(
            "recipes_cook_update",
            json!({ "sessionId": session, "events": [{ "type": "timer", "step": step, "minutes": 5 }] }),
        )
        .await;
    assert!(
        refused.starts_with("[too_many] a session runs at most 30 timers"),
        "{refused}"
    );
}
