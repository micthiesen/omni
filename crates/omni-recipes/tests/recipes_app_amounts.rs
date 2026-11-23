//! The app's one content edit: the amount of one ingredient, written as a
//! revision through the path an agent's edit takes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

use crate::common;

use common::{COOKIES, Harness, PICCATA, post_json};
use serde_json::{Value, json};

fn amount(
    op: &str,
    recipe: &str,
    ingredient: &str,
    qty: Value,
    unit: Option<&str>,
    base: u32,
) -> Value {
    let mut op = json!({
        "id": op, "at": 1, "type": "amount", "recipeId": recipe,
        "ingredientId": ingredient, "qty": qty, "baseRevision": base
    });
    if let Some(unit) = unit {
        op["unit"] = json!(unit);
    }
    json!({ "ops": [op] })
}

fn ingredient(doc: &Value, id: &str) -> Value {
    doc["components"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|component| component["ingredients"].as_array().unwrap())
        .find(|ingredient| ingredient["id"] == id)
        .unwrap()
        .clone()
}

fn input(doc: &Value, step: &str, id: &str) -> Value {
    doc["components"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|component| component["steps"].as_array().unwrap())
        .find(|found| found["id"] == step)
        .unwrap()["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|input| input["ref"] == id)
        .unwrap()
        .clone()
}

#[tokio::test(start_paused = true)]
async fn an_amount_change_is_a_revision_like_any_other() {
    let h = Harness::new().await;
    let router = h.router();
    let id = h.create(PICCATA).await;

    // Halve the butter: the 2 tbsp the sear takes follows, the rest needs nothing.
    let body = amount("op_amount000001", &id, "butter", json!(2), Some("tbsp"), 1);
    let saved = post_json(&router, "/api/ops", &body).await;
    assert_eq!(saved.status, 200, "{}", saved.text());
    let saved = saved.json();
    assert_eq!(saved["results"][0]["ok"], true, "{saved}");
    let record = &saved["results"][0]["recipe"];
    assert_eq!(record["revision"], 2);
    assert_eq!(ingredient(&record["doc"], "butter")["qty"], 2);
    assert_eq!(input(&record["doc"], "sear", "butter")["qty"], 1);
    assert_eq!(
        input(&record["doc"], "mount", "butter"),
        json!({ "ref": "butter", "rest": true })
    );

    // Sent again by the outbox: applied once.
    let again = post_json(&router, "/api/ops", &body).await.json();
    assert_eq!(again["seq"], saved["seq"]);
    assert_eq!(again["results"][0]["recipe"]["revision"], 2);

    // An agent sees an ordinary revision, written by the app.
    let read = h
        .ok("recipes_get", json!({ "id": id, "view": "text" }))
        .await;
    assert_eq!(read["revision"], 2);
    let text = read["text"].as_str().unwrap();
    assert!(
        text.contains("- 2 tbsp unsalted butter, cold, divided"),
        "{text}"
    );
    assert!(
        text.contains("1 tbsp unsalted butter in a large skillet"),
        "{text}"
    );
    assert!(
        text.contains("the remaining 1 tbsp unsalted butter"),
        "{text}"
    );
    let history = h.ok("recipes_history", json!({ "id": id })).await;
    let newest = &history["revisions"][0];
    assert_eq!(newest["revision"], 2);
    assert_eq!(newest["tool"], "app");
    assert_eq!(newest["summary"], "App: unsalted butter 4 tbsp → 2 tbsp");
    // And can undo it.
    let log = h.ok("recipes_history", json!({})).await;
    let change = &log["changes"][0];
    assert_eq!(change["tool"], "app");
    assert_eq!(change["recipeIds"], json!([id]));
    let undone = h
        .ok("recipes_undo", json!({ "changeId": change["changeId"] }))
        .await;
    assert_eq!(undone["revision"], 3);
    assert!(
        undone["text"]
            .as_str()
            .unwrap()
            .contains("- 4 tbsp unsalted butter")
    );

    // The alternative measure follows too, and the app's own undo (the old
    // amount sent back) restores it exactly.
    let third = post_json(
        &router,
        "/api/ops",
        &amount("op_amount000002", &id, "stock", json!(60), Some("ml"), 3),
    )
    .await
    .json();
    let stock = ingredient(&third["results"][0]["recipe"]["doc"], "stock");
    assert_eq!(stock["qty"], 60);
    assert_eq!(stock["alt"], json!({ "qty": 0.25, "unit": "cup" }));
    let back = post_json(
        &router,
        "/api/ops",
        &amount("op_amount000003", &id, "stock", json!(180), Some("ml"), 4),
    )
    .await
    .json();
    let stock = ingredient(&back["results"][0]["recipe"]["doc"], "stock");
    assert_eq!(stock["alt"], json!({ "qty": 0.75, "unit": "cup" }));
    assert_eq!(back["results"][0]["recipe"]["revision"], 5);

    // The same amount again writes nothing.
    let same = post_json(
        &router,
        "/api/ops",
        &amount("op_amount000004", &id, "stock", json!(180), Some("ml"), 5),
    )
    .await
    .json();
    assert_eq!(same["results"][0]["ok"], true);
    assert_eq!(same["results"][0]["recipe"]["revision"], 5);
    assert_eq!(same["seq"], back["seq"]);
}

#[tokio::test(start_paused = true)]
async fn a_unit_changes_only_where_nothing_is_divided_by_amount() {
    let h = Harness::new().await;
    let router = h.router();
    let id = h.create(PICCATA).await;
    let send = async |op: &str, ingredient: &str, qty: Value, unit: Option<&str>, base: u32| {
        post_json(
            &router,
            "/api/ops",
            &amount(op, &id, ingredient, qty, unit, base),
        )
        .await
        .json()["results"][0]
            .clone()
    };

    // A new unit drops the alternative measure, which no longer matches.
    let wine = send("op_unit00000001", "wine", json!(0.5), Some("cup"), 1).await;
    assert_eq!(wine["ok"], true, "{wine}");
    let changed = ingredient(&wine["recipe"]["doc"], "wine");
    assert_eq!(changed["unit"], "cup");
    assert!(changed.get("alt").is_none());
    // A unit can be given up for a plain count, and taken again.
    let capers = send("op_unit00000002", "capers", json!(12), None, 2).await;
    assert_eq!(capers["ok"], true, "{capers}");
    assert!(
        ingredient(&capers["recipe"]["doc"], "capers")
            .get("unit")
            .is_none()
    );
    let capers = send("op_unit00000003", "capers", json!(2), Some("tbsp"), 3).await;
    assert_eq!(
        ingredient(&capers["recipe"]["doc"], "capers")["unit"],
        "tbsp"
    );
    assert_eq!(capers["recipe"]["revision"], 4);

    // Refused, each with the recipe as it is and nothing written.
    let refused = [
        (
            "butter",
            json!(60),
            Some("g"),
            "this ingredient is divided between steps by amount; ask an agent to change its unit",
        ),
        (
            "salt",
            json!(1),
            Some("tsp"),
            "this ingredient has no amount to change",
        ),
        (
            "nothing",
            json!(1),
            None,
            "the recipe no longer has this ingredient",
        ),
        (
            "wine",
            json!(0),
            Some("cup"),
            "the amount must be a number above 0, at most 100000",
        ),
        (
            "wine",
            json!(100_001),
            Some("cup"),
            "the amount must be a number above 0, at most 100000",
        ),
        (
            "wine",
            json!(1),
            Some("glug"),
            "choose one of the listed units",
        ),
        (
            "wine",
            json!(1),
            Some("a-unit-longer-than-sixteen"),
            "choose one of the listed units",
        ),
    ];
    for (n, (target, qty, unit, message)) in refused.into_iter().enumerate() {
        let answer = send(&format!("op_refused0000{n}"), target, qty, unit, 4).await;
        assert_eq!(answer["ok"], false, "{target}");
        assert_eq!(answer["error"], message);
        assert_eq!(answer["recipe"]["revision"], 4);
    }
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["revision"],
        4
    );
    // A refusal is remembered like any answer: the resend is refused the same.
    let again = send("op_refused00000", "butter", json!(60), Some("g"), 4).await;
    assert_eq!(again["ok"], false);

    // A number JSON cannot carry is refused before any op runs.
    let bad = post_json(
        &router,
        "/api/ops",
        &json!({ "ops": [{ "id": "op_bad000000001", "at": 1, "type": "amount", "recipeId": id,
                           "ingredientId": "wine", "qty": "lots", "baseRevision": 4 }] }),
    )
    .await;
    assert!(bad.status.is_client_error(), "{}", bad.status);
    // No other part of an ingredient can be reached through the app.
    let extra = post_json(
        &router,
        "/api/ops",
        &json!({ "ops": [{ "id": "op_bad000000002", "at": 1, "type": "amount", "recipeId": id,
                           "ingredientId": "wine", "qty": 1, "unit": "cup", "name": "gin", "baseRevision": 4 }] }),
    )
    .await;
    if extra.status == 200 {
        let doc = h
            .ok("recipes_get", json!({ "id": id, "view": "document" }))
            .await;
        assert_eq!(
            ingredient(&doc["document"], "wine")["name"],
            "dry white wine"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_change_made_against_an_older_revision_applies_only_to_an_unmoved_amount() {
    let h = Harness::new().await;
    let router = h.router();
    let id = h.create(COOKIES).await;
    // An agent edits the recipe while the phone still shows revision 1.
    h.ok(
        "recipes_edit",
        json!({ "id": id, "expectedRevision": 1, "summary": "More salt",
                "ops": [{ "op": "set", "target": "salt", "fields": { "qty": 1.5 } }] }),
    )
    .await;

    // The salt moved under the cook: refused, with the recipe as it is now.
    let stale = post_json(
        &router,
        "/api/ops",
        &amount("op_stale0000001", &id, "salt", json!(2), Some("tsp"), 1),
    )
    .await
    .json();
    assert_eq!(stale["results"][0]["ok"], false);
    assert_eq!(
        stale["results"][0]["error"],
        "the recipe changed; reload and try again"
    );
    assert_eq!(
        ingredient(&stale["results"][0]["recipe"]["doc"], "salt")["qty"],
        1.5
    );
    assert_eq!(stale["results"][0]["recipe"]["revision"], 2);

    // The butter did not: the change lands on the newest revision.
    let fine = post_json(
        &router,
        "/api/ops",
        &amount("op_stale0000002", &id, "butter", json!(200), Some("g"), 1),
    )
    .await
    .json();
    assert_eq!(fine["results"][0]["ok"], true, "{fine}");
    let doc = &fine["results"][0]["recipe"]["doc"];
    assert_eq!(fine["results"][0]["recipe"]["revision"], 3);
    assert_eq!(ingredient(doc, "butter")["qty"], 200);
    assert_eq!(
        ingredient(doc, "salt")["qty"],
        1.5,
        "the agent's edit stays"
    );

    // A session cooking the older revision keeps the document it pinned
    // until a rebase moves it.
    let start =
        json!({ "id": "cook_amount01", "recipeId": id, "revision": 3, "scale": 1, "at": 1 });
    assert_eq!(post_json(&router, "/api/cook", &start).await.status, 200);
    post_json(
        &router,
        "/api/ops",
        &amount("op_stale0000003", &id, "butter", json!(225), Some("g"), 3),
    )
    .await;
    let session = common::get(&router, "/api/cook/cook_amount01").await.json();
    assert_eq!(session["revision"], 3);
    assert_eq!(ingredient(&session["doc"], "butter")["qty"], 200);

    // A trashed recipe takes no amount.
    post_json(
        &router,
        "/api/ops",
        &json!({ "ops": [{ "id": "op_stale0000004", "at": 2, "type": "state", "recipeId": id, "state": "trashed" }] }),
    )
    .await;
    let trashed = post_json(
        &router,
        "/api/ops",
        &amount("op_stale0000005", &id, "butter", json!(100), Some("g"), 4),
    )
    .await
    .json();
    assert_eq!(trashed["results"][0]["ok"], false);
    assert!(
        trashed["results"][0]["error"]
            .as_str()
            .unwrap()
            .contains("trash"),
        "{trashed}"
    );
}
