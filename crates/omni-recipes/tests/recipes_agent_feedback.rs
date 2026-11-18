//! What the first agent to fill a real library ran into: exact amounts,
//! challenge pages, archived copies, dry runs that tell everything, and
//! results that say what changed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

mod common;

use common::{COOKIES, Harness, PICCATA, doc, get, post_json};
use omni_runtime::ports::FetchedPage;
use serde_json::{Value, json};

const URL: &str = "https://example.com/recipes/chicken-piccata";

fn recipe_html(image: &str) -> String {
    let data = json!({
        "@context": "https://schema.org", "@type": "Recipe", "name": "Chicken Piccata",
        "image": [image],
        "recipeIngredient": ["500 g chicken breasts", "4 tbsp butter"],
        "recipeInstructions": [{ "@type": "HowToStep", "text": "Sear the chicken 6 minutes." }]
    });
    format!(
        "<html><head><title>Chicken Piccata</title><script type=\"application/ld+json\">{data}</script></head><body><h1>Chicken Piccata</h1></body></html>"
    )
}

fn challenge() -> FetchedPage {
    FetchedPage {
        source: "fetch".to_owned(),
        url: URL.to_owned(),
        title: Some("Just a moment...".to_owned()),
        html: Some("<html><head><title>Just a moment...</title></head><body><div id=\"challenge-form\">Enable JavaScript and cookies to continue</div><script src=\"/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1\"></script></body></html>".to_owned()),
        text: Some("Just a moment...\n\nEnable JavaScript and cookies to continue".to_owned()),
        ..FetchedPage::default()
    }
}

#[tokio::test(start_paused = true)]
async fn stored_amounts_read_exactly() {
    let h = Harness::new().await;
    let mut document = doc(PICCATA);
    document["components"][0]["ingredients"][0]["qty"] = json!(326);
    document["components"][0]["ingredients"][7]["qty"] = json!(113);
    let created = h
        .ok("recipes_create", json!({ "document": document }))
        .await;
    let text = created["text"].as_str().unwrap();
    assert!(
        text.contains("- 326 g boneless, skinless chicken breasts"),
        "{text}"
    );
    assert!(text.contains("Season 326 g boneless"), "{text}");
    assert!(text.contains("- 113 ml (¾ cup) chicken stock"), "{text}");
    let id = created["id"].as_str().unwrap();
    let chart = h
        .ok("recipes_get", json!({ "id": id, "view": "chart" }))
        .await;
    assert!(
        chart["chart"]
            .as_str()
            .unwrap()
            .contains("326 g boneless, skinless")
    );
    assert!(
        chart["chart"]
            .as_str()
            .unwrap()
            .contains("113 ml chicken stock")
    );
    // Scaled, a metric amount is rounded to something a cook can weigh.
    let scaled = h
        .ok(
            "recipes_get",
            json!({ "id": id, "view": "text", "scale": 1.5 }),
        )
        .await;
    assert!(
        scaled["text"]
            .as_str()
            .unwrap()
            .contains("- 490 g boneless")
    );
    // A cook's adjustment record quotes the amounts as stored.
    let session = h.ok("recipes_cook_start", json!({ "recipeId": id })).await["sessionId"].clone();
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": session, "events": [
            { "type": "note", "text": "Pan was crowded." },
            { "type": "adjust", "target": "chicken", "text": "350 g" },
            { "type": "note", "text": "Sauce broke once." },
            { "type": "finish", "note": "Good." }
        ] }),
    )
    .await;
    let recipe = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(
        recipe["openAdjustments"][0]["original"],
        "326 g boneless, skinless chicken breasts, halved horizontally into cutlets"
    );
    // The cook log keeps the notes taken along the way, not only the last.
    let log = h.ok("recipes_cook_get", json!({ "recipeId": id })).await;
    assert_eq!(log["cooks"][0]["note"], "Good.");
    assert_eq!(
        log["cooks"][0]["notes"],
        json!(["Pan was crowded.", "Sauce broke once."])
    );
}

#[tokio::test(start_paused = true)]
async fn a_bot_challenge_is_never_saved_as_a_page() {
    let h = Harness::new().await;
    // Only the challenge: nothing is stored.
    h.pages.serve(vec![challenge()]);
    let refused = h.err("recipes_import_url", json!({ "url": URL })).await;
    assert!(
        refused.starts_with("[fetch_failed] the host served a bot challenge"),
        "{refused}"
    );
    assert!(refused.contains("render \"browser\"") && refused.contains("sourceText"));
    for state in ["pending", "imported", "discarded"] {
        assert_eq!(
            h.ok("recipes_inbox_list", json!({ "state": state })).await["total"],
            0
        );
    }
    // Other blocked or empty answers are refused too.
    for (title, text) in [
        (
            "Attention Required! | Cloudflare",
            "Sorry, you have been blocked",
        ),
        (
            "Access Denied",
            "You don't have permission to access this page",
        ),
        ("Recipes", "Menu"),
    ] {
        h.pages.serve(vec![FetchedPage {
            source: "fetch".to_owned(),
            url: URL.to_owned(),
            title: Some(title.to_owned()),
            text: Some(text.to_owned()),
            ..FetchedPage::default()
        }]);
        let refused = h.err("recipes_import_url", json!({ "url": URL })).await;
        assert!(refused.starts_with("[fetch_failed]"), "{title}: {refused}");
    }
    assert_eq!(h.ok("recipes_inbox_list", json!({})).await["total"], 0);

    // With an archived copy beside the challenge, the import falls through
    // to it: the data comes from the archive, the addresses are the
    // originals, and the challenge text is nowhere.
    let archived_image =
        "https://web.archive.org/web/20250101000000im_/https://images.example.com/piccata.jpg";
    h.pages.serve(vec![
        challenge(),
        FetchedPage {
            source: "wayback".to_owned(),
            url: format!("https://web.archive.org/web/20250101000000/{URL}"),
            html: Some(recipe_html(archived_image)),
            title: Some("Chicken Piccata".to_owned()),
            ..FetchedPage::default()
        },
        FetchedPage {
            source: "readability".to_owned(),
            url: URL.to_owned(),
            text: Some(format!(
                "[Home](https://example.com/)\n* [Recipes](https://example.com/recipes)\nSkip to content\n\n# Chicken Piccata\n\nA weeknight classic. {}",
                "Season, dredge and sear the cutlets. ".repeat(20)
            )),
            ..FetchedPage::default()
        },
    ]);
    let imported = h.ok("recipes_import_url", json!({ "url": URL })).await;
    assert_eq!(imported["title"], "Chicken Piccata");
    assert_eq!(imported["finalUrl"], URL, "never the archive's address");
    assert_eq!(
        imported["structured"]["images"],
        json!(["https://images.example.com/piccata.jpg"])
    );
    let text = imported["text"].as_str().unwrap();
    assert!(
        text.starts_with("# Chicken Piccata\n\nA weeknight classic."),
        "{text}"
    );
    assert!(!text.contains("Just a moment") && !text.contains("Skip to content"));
    // The test app has no network, so the image is not stored, and says why.
    assert_eq!(imported["imageStored"], false);
    let reason = imported["imageReason"].as_str().unwrap();
    assert!(
        reason.starts_with("[fetch_failed] the image at"),
        "{reason}"
    );
    // The inbox list names where a page item came from.
    let listed = h.ok("recipes_inbox_list", json!({})).await;
    assert_eq!(listed["items"][0]["url"], URL);

    // The page itself is preferred over the archive when both hold data.
    h.pages.serve(vec![
        FetchedPage {
            source: "wayback".to_owned(),
            url: format!("https://web.archive.org/web/2025/{URL}"),
            html: Some(recipe_html(archived_image)),
            ..FetchedPage::default()
        },
        FetchedPage {
            source: "fetch".to_owned(),
            url: URL.to_owned(),
            html: Some(recipe_html("https://images.example.com/fresh.jpg")),
            ..FetchedPage::default()
        },
    ]);
    let direct = h.ok("recipes_import_url", json!({ "url": URL })).await;
    assert_eq!(
        direct["structured"]["images"],
        json!(["https://images.example.com/fresh.jpg"])
    );
    // A page with no image says so.
    h.pages.serve(vec![FetchedPage {
        source: "fetch".to_owned(),
        url: URL.to_owned(),
        text: Some("Chicken Piccata. ".repeat(40)),
        ..FetchedPage::default()
    }]);
    let plain = h.ok("recipes_import_url", json!({ "url": URL })).await;
    assert_eq!(plain["imageReason"], "the page names no image");
}

#[tokio::test(start_paused = true)]
async fn a_dry_run_reports_everything_the_real_call_would_refuse() {
    let h = Harness::new().await;
    h.tag("weeknight", "occasion").await;
    let existing = h.create(PICCATA).await;

    // An invalid document, two unknown tags and a duplicate, in one answer.
    let mut broken = doc(PICCATA);
    broken["components"][0]["steps"][3]["inputs"]
        .as_array_mut()
        .unwrap()
        .retain(|input| input["ref"] != "capers");
    let dry = h
        .ok(
            "recipes_create",
            json!({ "document": broken, "tags": ["weeknite", "zzz", "weeknight"], "dryRun": true }),
        )
        .await;
    assert_eq!(dry["saved"], false);
    let problems = dry["problems"].as_array().unwrap();
    let codes: Vec<&str> = problems
        .iter()
        .map(|p| p["code"].as_str().unwrap())
        .collect();
    assert_eq!(
        codes,
        [
            "placeholder_not_input",
            "unused_ingredient",
            "unknown_tag",
            "unknown_tag"
        ]
    );
    assert_eq!(problems[2]["path"], "tags.weeknite");
    assert_eq!(
        problems[2]["hint"],
        "nearest: \"weeknight\"; use an existing tag, or pass createTags"
    );
    assert_eq!(problems[3]["path"], "tags.zzz");

    // A valid document that already exists, with an unknown tag.
    let dry = h
        .ok(
            "recipes_create",
            json!({ "document": doc(PICCATA), "tags": ["weeknite"], "dryRun": true }),
        )
        .await;
    let codes: Vec<&str> = dry["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["unknown_tag", "duplicate_source"]);
    assert_eq!(dry["problems"][1]["path"], "source");
    assert!(
        dry["problems"][1]["message"]
            .as_str()
            .unwrap()
            .contains(&existing)
    );
    assert!(
        dry["text"]
            .as_str()
            .unwrap()
            .starts_with("# Chicken Piccata")
    );
    // What the flags waive is not reported.
    let waived = h
        .ok(
            "recipes_create",
            json!({ "document": doc(PICCATA), "tags": ["weeknite"], "createTags": true, "allowDuplicate": true, "dryRun": true }),
        )
        .await;
    assert!(waived.get("problems").is_none(), "{waived}");
    assert_eq!(
        h.ok("recipes_guide", json!({})).await["tags"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Organize previews the change and names the tags it would be refused for.
    let dry = h
        .ok(
            "recipes_organize",
            json!({ "ids": [existing], "addTags": ["weeknite", "weeknight"], "dryRun": true }),
        )
        .await;
    assert_eq!(dry["problems"][0]["code"], "unknown_tag");
    assert_eq!(dry["problems"].as_array().unwrap().len(), 1);
    assert_eq!(dry["results"][0]["tags"], json!(["weeknight", "weeknite"]));
    assert_eq!(
        h.ok("recipes_get", json!({ "id": existing })).await["tags"],
        json!([])
    );
    let real = h
        .err(
            "recipes_organize",
            json!({ "ids": [existing], "addTags": ["weeknite"] }),
        )
        .await;
    assert!(
        real.starts_with(
            "[unknown_tag] tag \"weeknite\" is not in the registry; nearest: \"weeknight\""
        ),
        "{real}"
    );

    // New warnings never block: a step without a time, a web source without an address.
    let mut loose = doc(COOKIES);
    loose["source"] = json!({ "kind": "web", "name": "Example" });
    loose["components"][0]["steps"][1]
        .as_object_mut()
        .unwrap()
        .remove("time");
    let created = h.ok("recipes_create", json!({ "document": loose })).await;
    assert_eq!(created["saved"], true);
    let codes: Vec<&str> = created["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["missing_time", "missing_source_url"]);
    assert_eq!(created["time"]["complete"], false);
}

#[tokio::test(start_paused = true)]
async fn results_say_what_changed() {
    let h = Harness::new().await;
    h.tag("weeknight", "occasion").await;
    let id = h.create(PICCATA).await;
    let session = h.ok("recipes_cook_start", json!({ "recipeId": id })).await["sessionId"].clone();
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": session, "events": [
            { "type": "adjust", "target": "sear", "text": "Two batches." },
            { "type": "finish" }
        ] }),
    )
    .await;
    let adjustment =
        h.ok("recipes_get", json!({ "id": id })).await["openAdjustments"][0]["id"].clone();

    // A rename is one line, and the edit echoes what it resolved.
    let edited = h
        .ok(
            "recipes_edit",
            json!({ "id": id, "expectedRevision": 1, "summary": "Name the oil", "resolves": [adjustment],
                    "ops": [
                        { "op": "rename", "target": "oil", "to": "olive_oil" },
                        { "op": "set", "target": "olive_oil", "fields": { "qty": 3 } }
                    ] }),
        )
        .await;
    assert_eq!(
        edited["changes"],
        json!(["~ rename oil → olive_oil", "~ olive_oil.qty: 2 → 3"])
    );
    assert_eq!(edited["resolved"], json!([adjustment]));

    // Undoing a content change returns the recipe as it reads now.
    let undone = h
        .ok("recipes_undo", json!({ "changeId": edited["changeId"] }))
        .await;
    assert_eq!(undone["ids"], json!([id]));
    assert_eq!(undone["revision"], 3);
    assert!(
        undone["text"]
            .as_str()
            .unwrap()
            .contains("- 2 tbsp olive oil")
    );
    let dry = h
        .ok(
            "recipes_undo",
            json!({ "changeId": undone["undoChangeId"], "dryRun": true }),
        )
        .await;
    assert!(dry.get("revision").is_none());
    assert!(dry["text"].as_str().unwrap().contains("- 3 tbsp olive oil"));

    // Undoing a metadata change returns the recipes it wrote.
    let other = h.create(COOKIES).await;
    let organized = h
        .ok(
            "recipes_organize",
            json!({ "ids": [id, other], "addTags": ["weeknight"] }),
        )
        .await;
    let undone = h
        .ok("recipes_undo", json!({ "changeId": organized["changeId"] }))
        .await;
    let mut ids: Vec<&str> = undone["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    ids.sort_unstable();
    let mut expected = vec![id.as_str(), other.as_str()];
    expected.sort_unstable();
    assert_eq!(ids, expected);
    assert!(undone.get("text").is_none());

    // The library change log names the recipes each entry touched.
    let log = h.ok("recipes_history", json!({})).await;
    let entry = log["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["changeId"] == organized["changeId"])
        .unwrap();
    assert_eq!(entry["recipes"], 2);
    assert_eq!(entry["recipeIds"].as_array().unwrap().len(), 2);

    // "weeknite" finds "weeknight".
    let found = h
        .ok("recipes_search", json!({ "tags": ["weeknite"] }))
        .await;
    assert_eq!(
        found["unknownTags"],
        json!([{ "name": "weeknite", "nearest": ["weeknight"] }])
    );
}

fn event(id: &str, kind: Value) -> Value {
    let mut event = json!({ "clientEventId": id, "at": 1, "actor": "app" });
    for (key, value) in kind.as_object().unwrap() {
        event[key] = value.clone();
    }
    event
}

#[tokio::test(start_paused = true)]
async fn a_cook_can_be_discarded() {
    let h = Harness::new().await;
    let router = h.router();
    let id = h.create(PICCATA).await;
    // From the app: started, adjusted, a timer running, then discarded.
    let start =
        json!({ "id": "cook_discard1", "recipeId": id, "revision": 1, "scale": 1, "at": 1 });
    assert_eq!(post_json(&router, "/api/cook", &start).await.status, 200);
    let now = 1_767_225_600_000_i64;
    let discarded = post_json(
        &router,
        "/api/cook/cook_discard1/events",
        &json!({ "events": [
            event("d-1", json!({ "type": "check", "target": "dredge" })),
            event("d-2", json!({ "type": "adjust", "target": "sear", "text": "Two batches." })),
            event("d-3", json!({ "type": "timer", "step": "simmer", "endsAt": now + 60_000 })),
            event("d-4", json!({ "type": "abandon" })),
        ] }),
    )
    .await;
    assert_eq!(discarded.status, 200, "{}", discarded.text());
    let session = discarded.json();
    assert_eq!(session["state"], "abandoned");
    assert!(session["finishedAt"].is_number());
    assert_eq!(
        session["events"][3],
        json!({
            "seq": 4, "clientEventId": "d-4", "at": 1, "actor": "app", "type": "abandon"
        })
    );

    // No cook, no adjustment, no event, and it is gone from the open sessions.
    let recipe = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(recipe["cooks"]["count"], 0);
    assert_eq!(recipe["openAdjustments"], json!([]));
    assert_eq!(
        h.ok("recipes_cook_get", json!({ "recipeId": id })).await["cooks"],
        json!([])
    );
    assert_eq!(
        h.ok("recipes_cook_get", json!({})).await["active"],
        json!([])
    );
    assert_eq!(
        get(&router, "/api/sync").await.json()["sessions"],
        json!([])
    );
    assert!(h.events.published().is_empty());
    // Its timer never rings.
    h.advance(std::time::Duration::from_secs(90)).await;
    assert_eq!(h.recipes.tick().await.unwrap().alerts, 0);
    assert!(h.app.pushes.all().is_empty());
    // Late events are answered as on any closed session.
    let late = post_json(
        &router,
        "/api/cook/cook_discard1/events",
        &json!({ "events": [event("d-5", json!({ "type": "finish" }))] }),
    )
    .await;
    assert_eq!(late.status, 200);
    assert_eq!(late.json()["state"], "abandoned");
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["cooks"]["count"],
        0
    );

    // An agent discards one too, and is refused afterwards.
    let sid = h.ok("recipes_cook_start", json!({ "recipeId": id })).await["sessionId"].clone();
    let gone = h
        .ok(
            "recipes_cook_update",
            json!({ "sessionId": sid, "events": [{ "type": "abandon" }] }),
        )
        .await;
    assert_eq!(gone["session"]["state"], "abandoned");
    assert_eq!(gone["session"]["running"], json!([]));
    let closed = h
        .err(
            "recipes_cook_update",
            json!({ "sessionId": sid, "events": [{ "type": "check", "target": "dredge" }] }),
        )
        .await;
    assert!(closed.starts_with("[session_closed]"), "{closed}");
}
