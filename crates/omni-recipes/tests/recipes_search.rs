//! Search: full text over every field, prefix, phrases, accent folding,
//! filters, sorts, paging, and queries that try to be FTS syntax.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

mod common;

use common::{COOKIES, ENCHILADAS, Harness, LASAGNE, PICCATA};
use serde_json::{Value, json};

struct Library {
    h: Harness,
    piccata: String,
    cookies: String,
    lasagne: String,
    enchiladas: String,
}

async fn library() -> Library {
    let h = Harness::new().await;
    for (name, group) in [
        ("main", "course"),
        ("dessert", "course"),
        ("italian", "cuisine"),
        ("mexican", "cuisine"),
        ("weeknight", "occasion"),
    ] {
        h.tag(name, group).await;
    }
    let piccata = h.create(PICCATA).await;
    h.advance(std::time::Duration::from_secs(60)).await;
    let cookies = h.create(COOKIES).await;
    h.advance(std::time::Duration::from_secs(60)).await;
    let lasagne = h.create(LASAGNE).await;
    h.advance(std::time::Duration::from_secs(60)).await;
    let enchiladas = h.create(ENCHILADAS).await;
    for (id, tags) in [
        (&piccata, json!(["main", "italian", "weeknight"])),
        (&cookies, json!(["dessert"])),
        (&lasagne, json!(["main", "italian"])),
        (&enchiladas, json!(["main", "mexican", "weeknight"])),
    ] {
        h.ok("recipes_organize", json!({ "ids": [id], "addTags": tags }))
            .await;
    }
    Library {
        h,
        piccata,
        cookies,
        lasagne,
        enchiladas,
    }
}

async fn ids(h: &Harness, arguments: Value) -> Vec<String> {
    h.ok("recipes_search", arguments).await["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn text_search_covers_every_field() {
    let l = library().await;
    let h = &l.h;
    // Title, with a prefix and whatever the case.
    assert_eq!(
        ids(h, json!({ "query": "PICC" })).await,
        [l.piccata.clone()]
    );
    // Ingredient names.
    assert_eq!(
        ids(h, json!({ "query": "capers" })).await,
        [l.piccata.clone()]
    );
    // Tags.
    assert_eq!(
        ids(h, json!({ "query": "mexican" })).await,
        [l.enchiladas.clone()]
    );
    // Step text.
    assert_eq!(
        ids(h, json!({ "query": "scrape browned bits" })).await,
        [l.piccata.clone()]
    );
    // Source.
    assert_eq!(
        ids(h, json!({ "query": "house recipe" })).await.len(),
        ids(h, json!({ "query": "\"house recipe\"" })).await.len()
    );
    // Notes, after the app (or creation) set them.
    let router = h.router();
    let reply = common::post_json(
        &router,
        "/api/ops",
        &json!({ "ops": [{ "id": "op_aaaaaaaaaaaa", "at": 1, "type": "notes", "recipeId": l.cookies, "text": "Grandma's holiday tin", "baseRev": 0 }] }),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(
        ids(h, json!({ "query": "grandmas tin" })).await,
        [l.cookies.clone()]
    );

    // Every term must match; a phrase must be adjacent.
    assert_eq!(
        ids(h, json!({ "query": "chicken tortillas" })).await,
        [l.enchiladas.clone()]
    );
    assert!(
        ids(h, json!({ "query": "chicken walnuts" }))
            .await
            .is_empty()
    );
    assert_eq!(ids(h, json!({ "query": "\"white wine\"" })).await.len(), 2);
    assert!(
        ids(h, json!({ "query": "\"wine white\"" }))
            .await
            .is_empty()
    );
    assert_eq!(
        ids(h, json!({ "query": "choc* chip*" })).await,
        [l.cookies.clone()]
    );

    // Accents are ignored both ways.
    assert_eq!(
        ids(h, json!({ "query": "bechamel" })).await,
        [l.lasagne.clone()]
    );
    assert_eq!(
        ids(h, json!({ "query": "Béchamel ragù" })).await,
        [l.lasagne.clone()]
    );
    assert_eq!(
        ids(h, json!({ "query": "RAGU" })).await,
        [l.lasagne.clone()]
    );

    // Relevance: a title match outranks a step-text match.
    let chicken = ids(h, json!({ "query": "chicken" })).await;
    assert_eq!(chicken.len(), 2);
    let found = h.ok("recipes_search", json!({ "query": "piccata" })).await;
    let card = &found["recipes"][0];
    assert_eq!(card["title"], "Chicken Piccata");
    assert_eq!(card["yield"], "4 servings");
    assert_eq!(card["tags"], json!(["italian", "main", "weeknight"]));
    assert_eq!(card["revision"], 1);
    assert_eq!(card["hasImage"], false);
    assert_eq!(card["cookCount"], 0);
    assert!(card.get("openAdjustments").is_none());
    assert!(
        card["match"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("piccata")
    );
    assert!(card.get("lastCookedOn").is_none());
}

#[tokio::test(start_paused = true)]
async fn filters_sorts_and_pages() {
    let l = library().await;
    let h = &l.h;
    assert_eq!(
        ids(h, json!({ "tags": ["main", "italian"] })).await.len(),
        2
    );
    assert_eq!(
        ids(h, json!({ "tags": ["main"], "notTags": ["italian"] })).await,
        [l.enchiladas.clone()]
    );
    assert_eq!(
        ids(h, json!({ "anyTags": ["dessert", "mexican"] }))
            .await
            .len(),
        2
    );
    assert!(ids(h, json!({ "tags": ["nonexistent"] })).await.is_empty());
    assert_eq!(
        ids(
            h,
            json!({ "ingredients": ["lemon", "chicken"], "maxTotalMin": 45 })
        )
        .await,
        [l.piccata.clone()]
    );
    assert_eq!(ids(h, json!({ "ingredients": ["chicken"] })).await.len(), 2);
    assert!(
        ids(h, json!({ "ingredients": ["chicken", "saffron"] }))
            .await
            .is_empty()
    );
    assert_eq!(
        ids(h, json!({ "maxTotalMin": 30 })).await,
        [l.piccata.clone()]
    );
    assert_eq!(ids(h, json!({ "hasImage": true })).await.len(), 0);
    assert_eq!(ids(h, json!({ "hasImage": false })).await.len(), 4);
    assert_eq!(ids(h, json!({ "cooked": "never" })).await.len(), 4);

    // A cook changes the history filters and sorts.
    h.advance(std::time::Duration::from_secs(60)).await;
    let session = h
        .ok("recipes_cook_start", json!({ "recipeId": l.cookies }))
        .await["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": session, "events": [{ "type": "finish" }] }),
    )
    .await;
    assert_eq!(
        ids(h, json!({ "cooked": "ever" })).await,
        [l.cookies.clone()]
    );
    assert_eq!(ids(h, json!({ "sort": "cookCount" })).await[0], l.cookies);
    assert_eq!(ids(h, json!({ "sort": "lastCooked" })).await[0], l.cookies);
    // Default order: most recent activity, the later of cooked and added.
    assert_eq!(ids(h, json!({})).await[0], l.cookies);
    assert_eq!(
        ids(h, json!({ "sort": "added" })).await,
        [
            l.enchiladas.clone(),
            l.lasagne.clone(),
            l.cookies.clone(),
            l.piccata.clone()
        ]
    );
    assert_eq!(
        ids(h, json!({ "sort": "title" })).await,
        [
            l.enchiladas.clone(),
            l.piccata.clone(),
            l.cookies.clone(),
            l.lasagne.clone()
        ]
    );
    let card = &h.ok("recipes_search", json!({ "query": "cookies" })).await["recipes"][0];
    assert_eq!(card["cookCount"], 1);
    assert!(card["lastCookedOn"].as_str().unwrap().starts_with("20"));

    // Paging.
    let first = h
        .ok("recipes_search", json!({ "sort": "title", "limit": 3 }))
        .await;
    assert_eq!(
        (first["total"].clone(), first["nextCursor"].clone()),
        (json!(4), json!(3))
    );
    assert_eq!(first["recipes"].as_array().unwrap().len(), 3);
    let rest = h
        .ok(
            "recipes_search",
            json!({ "sort": "title", "limit": 3, "cursor": 3 }),
        )
        .await;
    assert_eq!(rest["recipes"].as_array().unwrap().len(), 1);
    assert!(rest.get("nextCursor").is_none());
    assert_eq!(rest["recipes"][0]["id"], l.lasagne);
}

#[tokio::test(start_paused = true)]
async fn hostile_queries_never_reach_fts_as_syntax() {
    let l = library().await;
    let h = &l.h;
    for query in [
        "\"",
        "\"\"\"",
        "chicken\"",
        "\"chicken",
        "(chicken",
        "chicken)",
        "chicken OR",
        "AND",
        "NOT chicken",
        "NEAR(chicken lemon, 5)",
        "title:chicken",
        "ingredients : {lemon}",
        "chicken*lemon",
        "*",
        "^chicken",
        "chicken -lemon +stock",
        "col:{a b} : c",
        "'; DROP TABLE recipes_recipe; --",
        "\\",
        "🍋",
        "a\u{0}b",
        "ＣＨＩＣＫＥＮ",
        "",
        "   ",
    ] {
        let result = h
            .call_full("recipes_search", json!({ "query": query }))
            .await;
        assert!(result.is_ok(), "{query:?}: {:?}", result.err());
        let ingredient = h
            .call_full("recipes_search", json!({ "ingredients": [query] }))
            .await;
        assert!(ingredient.is_ok(), "{query:?}: {:?}", ingredient.err());
    }
    // Operators are words, not operators.
    assert_eq!(
        ids(h, json!({ "query": "chicken OR walnuts" })).await.len(),
        0
    );
    assert_eq!(ids(h, json!({ "query": "NOT chicken" })).await.len(), 0);
    assert_eq!(ids(h, json!({ "query": "title:piccata" })).await.len(), 0);
    assert_eq!(
        ids(h, json!({ "query": "(piccata)" })).await,
        [l.piccata.clone()]
    );
    // The library is still there.
    assert_eq!(h.ok("recipes_search", json!({})).await["total"], 4);
}
