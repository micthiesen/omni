//! Creating recipes: from a fetched page, from pasted text, the four sample
//! recipes rendering exactly, duplicates, and the model review of a source.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

use crate::common;

use common::{COOKIES, ENCHILADAS, Harness, LASAGNE, PICCATA, doc, jpeg, only_kept_diet_conflicts};
use omni_ai::{ContentPart, GenerateResponse, ModelRole};
use omni_http::public::PublicHttpClient;
use omni_runtime::AppContext;
use omni_runtime::ports::FetchedPage;
use omni_testkit::FakeFailure;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const PAGE_URL: &str = "https://example.com/recipes/chicken-piccata";

const INGREDIENTS: [&str; 11] = [
    "500 g boneless, skinless chicken breasts, halved horizontally into cutlets",
    "kosher salt",
    "freshly ground black pepper",
    "1/2 cup (60 g) all-purpose flour, for dredging",
    "2 tablespoons olive oil",
    "4 tablespoons cold unsalted butter, divided",
    "120 ml dry white wine",
    "180 ml chicken stock",
    "2 lemons",
    "2 tbsp capers, drained",
    "2 tbsp chopped flat-leaf parsley",
];

fn page_html() -> String {
    let recipe = json!({
        "@context": "https://schema.org",
        "@graph": [
            { "@type": "WebSite", "name": "Example Kitchen" },
            {
                "@type": "Recipe",
                "name": "Chicken Piccata",
                "author": { "@type": "Person", "name": "Sam Example" },
                "recipeYield": ["4", "4 servings"],
                "prepTime": "PT10M",
                "cookTime": "PT15M",
                "totalTime": "PT25M",
                "image": ["https://images.example.com/piccata.jpg"],
                "recipeIngredient": INGREDIENTS,
                "recipeInstructions": [
                    { "@type": "HowToStep", "text": "Season the chicken and dredge in the flour, about 5 minutes." },
                    { "@type": "HowToStep", "text": "Sear in the oil and half the butter until golden, 3 to 4 minutes per side." },
                    { "@type": "HowToSection", "name": "Sauce", "itemListElement": [
                        { "@type": "HowToStep", "text": "Deglaze with the wine and reduce by half, about 2 minutes." },
                        { "@type": "HowToStep", "text": "Add the stock, lemon juice and capers and simmer 4 to 5 minutes." },
                        { "@type": "HowToStep", "text": "Swirl in the rest of the butter." }
                    ]},
                    { "@type": "HowToStep", "text": "Return the chicken, scatter with parsley and serve with lemon wedges." }
                ]
            }
        ]
    });
    format!(
        "<html><head><title>Chicken Piccata | Example Kitchen</title><script type=\"application/ld+json\">{recipe}</script></head><body><h1>Chicken Piccata</h1></body></html>"
    )
}

fn pages() -> Vec<FetchedPage> {
    vec![
        FetchedPage {
            source: "fetch".to_owned(),
            url: PAGE_URL.to_owned(),
            html: Some(page_html()),
            title: Some("Chicken Piccata | Example Kitchen".to_owned()),
            ..FetchedPage::default()
        },
        FetchedPage {
            source: "readability".to_owned(),
            url: PAGE_URL.to_owned(),
            title: Some("Chicken Piccata".to_owned()),
            text: Some(format!(
                "Chicken Piccata\n\nA weeknight classic.\n\n{}",
                INGREDIENTS.join("\n")
            )),
            ..FetchedPage::default()
        },
    ]
}

/// The piccata document as an agent would draft it from the page.
fn web_piccata() -> Value {
    let mut document = doc(PICCATA);
    document["source"] = json!({
        "kind": "web", "name": "Example Kitchen", "url": PAGE_URL, "author": "Sam Example"
    });
    document
}

/// A harness whose image fetches reach a local server holding one JPEG.
async fn harness_with_image_host() -> (Harness, wiremock::MockServer) {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("GET"))
        .and(path("/piccata.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/jpeg")
                .set_body_bytes(jpeg(7)),
        )
        .mount(&server)
        .await;
    let http = omni_testkit::mock_http(&server, &["https://images.example.com"]);
    let h = Harness::with(move |ctx| AppContext {
        public_http: PublicHttpClient::new(&http).allow_loopback_for_tests(),
        ..ctx
    })
    .await;
    (h, server)
}

// Real time: the lead image is fetched over a local socket.
#[tokio::test]
async fn a_url_import_becomes_a_recipe_in_two_calls() {
    let (h, _server) = harness_with_image_host().await;
    h.pages.serve(pages());
    for (name, group) in [("main", "course"), ("italian", "cuisine")] {
        h.tag(name, group).await;
    }

    let imported = h.ok("recipes_import_url", json!({ "url": PAGE_URL })).await;
    assert_eq!(h.pages.asked.lock().unwrap().as_slice(), [PAGE_URL]);
    let item = imported["inboxItemId"].as_str().unwrap().to_owned();
    assert!(item.starts_with("inb_"));
    assert_eq!(imported["finalUrl"], PAGE_URL);
    assert_eq!(imported["title"], "Chicken Piccata");
    let structured = &imported["structured"];
    assert_eq!(structured["yield"], "4 servings");
    assert_eq!(structured["prepMin"], 10);
    assert_eq!(structured["cookMin"], 15);
    assert_eq!(structured["totalMin"], 25);
    assert_eq!(structured["author"], "Sam Example");
    assert_eq!(structured["ingredients"].as_array().unwrap().len(), 11);
    assert_eq!(
        structured["instructions"][2],
        json!({ "section": "Sauce", "text": "Deglaze with the wine and reduce by half, about 2 minutes." })
    );
    assert_eq!(structured["instructions"].as_array().unwrap().len(), 6);
    assert!(
        imported["text"]
            .as_str()
            .unwrap()
            .starts_with("Chicken Piccata\n\nA weeknight classic.")
    );
    assert_eq!(imported["truncated"], false);
    assert_eq!(imported["imageStored"], true);
    assert_eq!(imported["duplicateOf"], json!([]));
    let pending = h.ok("recipes_inbox_list", json!({})).await;
    assert_eq!(pending["total"], 1);
    assert_eq!(pending["items"][0]["kind"], "url");

    let created = h
        .ok(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": web_piccata(), "tags": ["main", "italian"], "fromInbox": item }),
        )
        .await;
    assert_eq!(created["saved"], true);
    assert_eq!(created["revision"], 1);
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("rcp_") && id.len() == 12);
    assert_eq!(
        created["time"],
        json!({ "activeMin": 25, "totalMin": 25, "complete": true })
    );
    assert!(only_kept_diet_conflicts(&created["warnings"]));
    let fidelity = &created["fidelity"];
    assert_eq!(fidelity["checked"], true);
    assert_eq!(fidelity["sourceIngredientLines"], 11);
    assert_eq!(fidelity["matched"], 11);
    assert_eq!(fidelity["issues"], json!([]), "{fidelity}");
    assert_eq!(
        created["text"].as_str().unwrap(),
        include_str!("golden/chicken-piccata.list.md")
    );

    // The source is linked, the item resolved, and the lead image is the cover.
    let recipe = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(
        recipe["source"],
        json!({ "itemId": item, "kind": "url", "url": PAGE_URL })
    );
    assert_eq!(recipe["tags"], json!(["italian", "main"]));
    let images = recipe["images"].as_array().unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0]["role"], "dish");
    assert_eq!(images[0]["cover"], true);
    assert_eq!(
        (images[0]["width"].clone(), images[0]["height"].clone()),
        (json!(640), json!(480))
    );
    let imported_items = h
        .ok("recipes_inbox_list", json!({ "state": "imported" }))
        .await;
    assert_eq!(imported_items["items"][0]["recipeId"], id);
    assert_eq!(h.ok("recipes_inbox_list", json!({})).await["total"], 0);

    // A second create from the same source is refused, whichever way it comes.
    let again = h
        .err(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": web_piccata(), "fromInbox": item }),
        )
        .await;
    assert!(again.starts_with("[duplicate_source]"), "{again}");
    assert!(again.contains(&id) && again.contains("Chicken Piccata"));
    let mut retitled = web_piccata();
    retitled["title"] = json!("Piccata Again");
    let by_url = h
        .err(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": retitled }),
        )
        .await;
    assert!(
        by_url.starts_with("[duplicate_source] a recipe from this source URL"),
        "{by_url}"
    );
    let reimport = h.ok("recipes_import_url", json!({ "url": PAGE_URL })).await;
    assert_eq!(
        reimport["duplicateOf"],
        json!([{ "id": id, "title": "Chicken Piccata" }])
    );
    let allowed = h
        .ok(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": retitled, "allowDuplicate": true }),
        )
        .await;
    assert_eq!(allowed["saved"], true);
}

#[tokio::test(start_paused = true)]
async fn the_browser_render_is_preferred_when_asked_for() {
    let h = Harness::new().await;
    let mut all = pages();
    all[0].html = Some("<html><body>Loading...</body></html>".to_owned());
    all.push(FetchedPage {
        source: "obscura".to_owned(),
        url: format!("{PAGE_URL}?rendered=1"),
        html: Some(page_html()),
        text: Some("Rendered page text".to_owned()),
        ..FetchedPage::default()
    });
    h.pages.serve(all);
    let auto = h.ok("recipes_import_url", json!({ "url": PAGE_URL })).await;
    assert_eq!(auto["finalUrl"], PAGE_URL);
    assert_eq!(auto["structured"]["name"], "Chicken Piccata");
    assert!(auto["text"].as_str().unwrap().contains("weeknight classic"));
    let browser = h
        .ok(
            "recipes_import_url",
            json!({ "url": PAGE_URL, "render": "browser" }),
        )
        .await;
    assert_eq!(browser["finalUrl"], format!("{PAGE_URL}?rendered=1"));
    assert_eq!(browser["text"], "Rendered page text");

    // HTML alone is turned into readable text.
    h.pages.serve(vec![FetchedPage {
        source: "fetch".to_owned(),
        url: PAGE_URL.to_owned(),
        html: Some(format!(
            "<html><body><h1>Toast</h1><p>Toast the <b>bread</b>.</p><p>{}</p><script>var x = 1;</script></body></html>",
            "Butter it while it is hot and eat it at once. ".repeat(15)
        )),
        ..FetchedPage::default()
    }]);
    let plain = h.ok("recipes_import_url", json!({ "url": PAGE_URL })).await;
    assert!(plain.get("structured").is_none());
    let text = plain["text"].as_str().unwrap();
    assert!(
        text.contains("# Toast") && text.contains("Toast the **bread**."),
        "{text}"
    );
    assert!(!text.contains("var x"));
    assert_eq!(plain["imageStored"], false);
}

#[tokio::test(start_paused = true)]
async fn a_page_that_cannot_be_read_is_a_fetch_failure() {
    let h = Harness::new().await;
    h.pages
        .fail("The host refused the request (HTTP 403); all page sources failed");
    let refused = h
        .err("recipes_import_url", json!({ "url": PAGE_URL }))
        .await;
    assert_eq!(
        refused,
        "[fetch_failed] The host refused the request (HTTP 403); all page sources failed"
    );
    h.pages.serve(vec![FetchedPage {
        source: "fetch".to_owned(),
        url: PAGE_URL.to_owned(),
        html: Some("<html><body></body></html>".to_owned()),
        ..FetchedPage::default()
    }]);
    let empty = h
        .err("recipes_import_url", json!({ "url": PAGE_URL }))
        .await;
    assert!(
        empty.starts_with("[fetch_failed] the page was fetched but returned no recipe"),
        "{empty}"
    );
    let not_http = h
        .err(
            "recipes_import_url",
            json!({ "url": "ftp://example.com/recipe" }),
        )
        .await;
    assert!(not_http.starts_with("[validation]"), "{not_http}");
    assert_eq!(h.ok("recipes_inbox_list", json!({})).await["total"], 0);

    let unwired = Harness::without_pages().await;
    let unavailable = unwired
        .err("recipes_import_url", json!({ "url": PAGE_URL }))
        .await;
    assert!(
        unavailable.starts_with("[fetch_failed] page fetching is not available"),
        "{unavailable}"
    );
}

const PASTED: &str = "Chicken Piccata (serves 4)

500 g chicken breasts, halved into cutlets
salt and pepper
60 g flour for dredging
2 tbsp olive oil
5 tbsp butter, divided
120 ml dry white wine
180 ml chicken stock
2 lemons
2 tbsp capers
2 tbsp chopped parsley
1 shallot, minced

Season and dredge the chicken (5 minutes). Sear in oil and 2 tbsp butter, 3 to 4 minutes per side.
Deglaze with wine, 2 minutes. Add stock, the juice of 1 lemon and capers; simmer 4-5 minutes.
Swirl in the rest of the butter, 1 minute. Sauce the chicken and garnish, 2 minutes.
";

#[tokio::test(start_paused = true)]
async fn pasted_text_is_stored_as_the_source_and_compared() {
    let h = Harness::new().await;
    let created = h
        .ok(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": doc(PICCATA), "sourceText": PASTED, "notes": "Mum's version." }),
        )
        .await;
    let fidelity = &created["fidelity"];
    assert_eq!(fidelity["checked"], true);
    assert_eq!(fidelity["sourceIngredientLines"], 10);
    assert_eq!(fidelity["matched"], 8);
    assert_eq!(
        fidelity["issues"],
        json!([
            "source line has no matching ingredient: \"1 shallot, minced\"",
            "amount differs: the source says \"5 tbsp butter, divided\", the document has \"4 tbsp unsalted butter\""
        ])
    );
    // A discrepancy is reported, never a refusal.
    assert_eq!(created["saved"], true);
    let id = created["id"].as_str().unwrap();
    let recipe = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(recipe["notes"], "Mum's version.");
    assert_eq!(recipe["source"]["kind"], "text");
    let item = recipe["source"]["itemId"].as_str().unwrap();
    let stored = h.ok("recipes_inbox_get", json!({ "itemId": item })).await;
    assert_eq!(stored["text"], PASTED);
    assert_eq!(stored["state"], "imported");
    assert_eq!(stored["recipeId"], id);

    // Without a source there is nothing to compare, and nothing is reviewed.
    let bare = h
        .ok(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": doc(COOKIES) }),
        )
        .await;
    assert_eq!(
        bare["fidelity"],
        json!({
            "checked": false, "reason": "no source", "issues": [],
            "review": { "ran": false, "reason": "no source", "issues": [] }
        })
    );
    let both = h
        .err(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": doc(LASAGNE), "sourceText": "x", "fromInbox": item }),
        )
        .await;
    assert!(both.starts_with("[validation]"), "{both}");
}

#[tokio::test(start_paused = true)]
async fn the_four_sample_recipes_render_byte_for_byte() {
    let h = Harness::new().await;
    for (json, list, chart) in [
        (
            PICCATA,
            include_str!("golden/chicken-piccata.list.md"),
            include_str!("golden/chicken-piccata.chart.txt"),
        ),
        (
            COOKIES,
            include_str!("golden/chocolate-chip-cookies.list.md"),
            include_str!("golden/chocolate-chip-cookies.chart.txt"),
        ),
        (
            LASAGNE,
            include_str!("golden/lasagne-bolognese.list.md"),
            include_str!("golden/lasagne-bolognese.chart.txt"),
        ),
        (
            ENCHILADAS,
            include_str!("golden/enchiladas.list.md"),
            include_str!("golden/enchiladas.chart.txt"),
        ),
    ] {
        let created = h
            .ok(
                "recipes_create",
                json!({ "keepDietConflicts": true, "document": doc(json) }),
            )
            .await;
        assert!(only_kept_diet_conflicts(&created["warnings"]));
        assert_eq!(created["text"].as_str().unwrap(), list);
        let id = created["id"].as_str().unwrap();
        let text = h
            .ok("recipes_get", json!({ "id": id, "view": "text" }))
            .await;
        assert_eq!(text["text"].as_str().unwrap(), list);
        assert!(text.get("document").is_none() && text.get("chart").is_none());
        let charted = h
            .ok("recipes_get", json!({ "id": id, "view": "chart" }))
            .await;
        assert_eq!(charted["chart"].as_str().unwrap(), chart);
        let stored = h.ok("recipes_get", json!({ "id": id })).await;
        assert_eq!(stored["document"], doc(json));
    }
    let lasagne = h.ok("recipes_search", json!({ "query": "lasagne" })).await;
    assert_eq!(lasagne["recipes"][0]["totalMin"], 265);
    assert_eq!(lasagne["recipes"][0]["activeMin"], 65);

    // The exact same document again is a duplicate: a retried create is harmless.
    let retried = h
        .err(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": doc(PICCATA) }),
        )
        .await;
    assert!(
        retried.starts_with("[duplicate_source] this exact document"),
        "{retried}"
    );
}

fn review_json(issues: Value) -> GenerateResponse {
    GenerateResponse::text(json!({ "issues": issues }).to_string())
}

#[tokio::test(start_paused = true)]
async fn the_model_review_reports_issues_and_never_blocks_a_save() {
    let h = Harness::new().await;
    h.app.ai.script(
        ModelRole::RecipeReview,
        vec![review_json(json!([
            { "severity": "high", "text": "Butter: the source says 5 tbsp, the recipe says 4 tbsp." },
            { "severity": "medium", "text": "The shallot in the source is missing." }
        ]))],
    );
    let created = h
        .ok(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": doc(PICCATA), "sourceText": PASTED }),
        )
        .await;
    assert_eq!(
        created["fidelity"]["review"],
        json!({
            "ran": true,
            "issues": [
                { "severity": "high", "text": "Butter: the source says 5 tbsp, the recipe says 4 tbsp." },
                { "severity": "medium", "text": "The shallot in the source is missing." }
            ],
            "model": "openai:gpt-6-luna"
        })
    );
    let requests = h.app.ai.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, Some(ModelRole::RecipeReview));
    let request = &requests[0].1;
    assert_eq!(request.max_retries, 0);
    assert!(request.timeout <= std::time::Duration::from_secs(45));
    let texts: Vec<&str> = request.messages[0]
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(texts[0].starts_with("SOURCE:\nChicken Piccata (serves 4)"));
    assert!(texts[1].starts_with("RENDERED RECIPE:\n# Chicken Piccata"));

    // A failing model, a malformed answer and a missing script all leave the
    // save alone.
    let id = created["id"].as_str().unwrap().to_owned();
    h.app.ai.script_failure(
        ModelRole::RecipeReview,
        FakeFailure {
            status: 500,
            message: "upstream".to_owned(),
        },
    );
    h.app.ai.script(
        ModelRole::RecipeReview,
        vec![GenerateResponse::text("not json")],
    );
    let mut revision = 1;
    for _ in 0..3 {
        let mut document = doc(PICCATA);
        document["summary"] = json!(format!("Attempt {revision}"));
        let replaced = h
            .ok(
                "recipes_replace",
                json!({ "id": id, "expectedRevision": revision, "summary": "Reword", "keepDietConflicts": true, "document": document }),
            )
            .await;
        revision += 1;
        assert_eq!(replaced["saved"], true);
        assert_eq!(replaced["revision"], revision);
        assert_eq!(replaced["fidelity"]["checked"], true);
        assert_eq!(
            replaced["fidelity"]["review"],
            json!({ "ran": false, "reason": "the review model call failed", "issues": [] })
        );
    }

    // A dry run reviews only when asked; review false skips it on a real write.
    let before = h.app.ai.requests().len();
    let dry = |review: Option<bool>| {
        let mut arguments = json!({
            "id": id, "expectedRevision": revision, "summary": "x",
            "keepDietConflicts": true, "document": doc(PICCATA), "dryRun": true,
        });
        if let Some(review) = review {
            arguments["review"] = json!(review);
        }
        arguments
    };
    let skipped = h.ok("recipes_replace", dry(None)).await;
    assert_eq!(
        skipped["fidelity"]["review"],
        json!({ "ran": false, "reason": "not requested", "issues": [] })
    );
    assert_eq!(h.app.ai.requests().len(), before);
    h.app
        .ai
        .script(ModelRole::RecipeReview, vec![review_json(json!([]))]);
    let asked = h.ok("recipes_replace", dry(Some(true))).await;
    assert_eq!(asked["saved"], false);
    assert_eq!(asked["fidelity"]["review"]["ran"], true);
    assert_eq!(asked["fidelity"]["review"]["issues"], json!([]));
    let declined = h
        .ok(
            "recipes_replace",
            json!({
                "id": id, "expectedRevision": revision, "summary": "Back to the original",
                "keepDietConflicts": true, "document": doc(PICCATA), "review": false,
            }),
        )
        .await;
    assert_eq!(declined["saved"], true);
    assert_eq!(declined["fidelity"]["review"]["reason"], "not requested");
}

#[tokio::test(start_paused = true)]
async fn a_photo_source_is_reviewed_from_the_photos() {
    let h = Harness::new().await;
    let router = h.router();
    for index in 0..2 {
        let reply = common::upload(
            &router,
            &format!("/api/inbox?item=page-1&index={index}&count=2&title=Piccata%20page"),
            jpeg(index),
        )
        .await;
        assert_eq!(reply.status, 200, "{}", reply.text());
    }
    let item = h.ok("recipes_inbox_list", json!({ "kind": "photo" })).await["items"][0]["itemId"]
        .as_str()
        .unwrap()
        .to_owned();
    h.app.ai.script(
        ModelRole::RecipeReview,
        vec![review_json(json!([
            { "severity": "low", "text": "The page says flat-leaf parsley, finely chopped." }
        ]))],
    );
    let created = h
        .ok(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": doc(PICCATA), "fromInbox": item }),
        )
        .await;
    // Nothing a program can compare; the model looked at the photos.
    assert_eq!(created["fidelity"]["checked"], false);
    assert_eq!(created["fidelity"]["reason"], "photo source");
    assert_eq!(created["fidelity"]["review"]["ran"], true);
    assert_eq!(
        created["fidelity"]["review"]["issues"][0]["severity"],
        "low"
    );
    let request = &h.app.ai.requests()[0].1;
    let images = request.messages[0]
        .content
        .iter()
        .filter(|part| matches!(part, ContentPart::Image { mime_type, .. } if mime_type == "image/jpeg"))
        .count();
    assert_eq!(images, 2);

    // The photos became the recipe's source images.
    let id = created["id"].as_str().unwrap();
    let recipe = h.ok("recipes_get", json!({ "id": id })).await;
    let roles: Vec<&str> = recipe["images"]
        .as_array()
        .unwrap()
        .iter()
        .map(|image| image["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["source", "source"]);
    assert_eq!(recipe["source"]["kind"], "photo");
}
