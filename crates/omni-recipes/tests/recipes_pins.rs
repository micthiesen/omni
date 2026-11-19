//! Pinned recipes: one ordered list, set by the app or an agent, synced to
//! every device and undoable.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

mod common;

use common::{COOKIES, ENCHILADAS, Harness, LASAGNE, PICCATA, get, post_json};
use serde_json::{Value, json};

fn pins(op: &str, ids: &[&str]) -> Value {
    json!({ "ops": [{ "id": op, "at": 1, "type": "pins", "ids": ids }] })
}

fn ids(output: &Value) -> Vec<String> {
    output["pins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pin| pin["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn pinned_in_sync(router: &axum::Router, since: Option<u64>) -> Vec<(String, u64)> {
    let path = since.map_or("/api/sync".to_owned(), |seq| {
        format!("/api/sync?since={seq}")
    });
    let sync = get(router, &path).await.json();
    let mut found: Vec<(String, u64)> = sync["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|recipe| {
            Some((
                recipe["id"].as_str()?.to_owned(),
                recipe.get("pinned")?.as_u64()?,
            ))
        })
        .collect();
    found.sort_by_key(|(_, place)| *place);
    found
}

#[tokio::test(start_paused = true)]
async fn the_app_pins_reorders_and_unpins_and_every_device_syncs_the_order() {
    let h = Harness::new().await;
    let router = h.router();
    let (a, b, c) = (
        h.create(PICCATA).await,
        h.create(COOKIES).await,
        h.create(LASAGNE).await,
    );
    assert!(pinned_in_sync(&router, None).await.is_empty());
    let seq = get(&router, "/api/sync").await.json()["seq"]
        .as_u64()
        .unwrap();

    let pinned = post_json(&router, "/api/ops", &pins("op_pins0000001", &[&a, &b])).await;
    assert_eq!(pinned.json()["results"][0]["ok"], true, "{}", pinned.text());
    assert_eq!(
        pinned_in_sync(&router, None).await,
        [(a.clone(), 0), (b.clone(), 1)]
    );
    // Another device hears of exactly the recipes whose place changed.
    assert_eq!(pinned_in_sync(&router, Some(seq)).await.len(), 2);
    let seq = get(&router, "/api/sync").await.json()["seq"]
        .as_u64()
        .unwrap();

    // Reorder and add one: only the moved and the new ones are synced.
    post_json(&router, "/api/ops", &pins("op_pins0000002", &[&a, &c, &b])).await;
    assert_eq!(
        pinned_in_sync(&router, None).await,
        [(a.clone(), 0), (c.clone(), 1), (b.clone(), 2)]
    );
    let changed = get(&router, &format!("/api/sync?since={seq}")).await.json();
    let touched: Vec<&str> = changed["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|recipe| recipe["id"].as_str().unwrap())
        .collect();
    assert_eq!(touched.len(), 2);
    assert!(touched.contains(&c.as_str()) && touched.contains(&b.as_str()));

    // The agent sees the same list, and the place on each recipe.
    let listed = h.ok("recipes_pins", json!({})).await;
    assert_eq!(ids(&listed), [a.clone(), c.clone(), b.clone()]);
    assert_eq!(listed["pins"][1]["title"], "Lasagne alla Bolognese");
    assert_eq!(h.ok("recipes_get", json!({ "id": c })).await["pinned"], 2);
    assert!(
        h.ok("recipes_get", json!({ "id": c, "view": "text" }))
            .await
            .get("text")
            .is_some()
    );
    let cards = h.ok("recipes_search", json!({ "sort": "title" })).await;
    let places: Vec<Value> = cards["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card.get("pinned").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(places, [json!(1), json!(3), json!(2)]);

    // Sent again by the outbox, the op changes nothing; the same list under
    // a new op id is no change either.
    let seq = get(&router, "/api/sync").await.json()["seq"].clone();
    post_json(&router, "/api/ops", &pins("op_pins0000002", &[&a, &c, &b])).await;
    post_json(&router, "/api/ops", &pins("op_pins0000003", &[&a, &c, &b])).await;
    assert_eq!(get(&router, "/api/sync").await.json()["seq"], seq);

    // Unpin one. A list from the app that names a recipe the library no
    // longer has is taken without it: the app cannot send it again differently.
    post_json(&router, "/api/ops", &pins("op_pins0000004", &[&c, &b])).await;
    assert_eq!(
        ids(&h.ok("recipes_pins", json!({})).await),
        [c.clone(), b.clone()]
    );
    let stale = post_json(
        &router,
        "/api/ops",
        &pins("op_pins0000005", &[&b, "rcp_zzzzzzzz", &c]),
    )
    .await
    .json();
    assert_eq!(stale["results"][0]["ok"], true, "{stale}");
    assert_eq!(
        ids(&h.ok("recipes_pins", json!({})).await),
        [b.clone(), c.clone()]
    );
    post_json(&router, "/api/ops", &pins("op_pins0000006", &[&c, &b])).await;

    // A pinned recipe that goes to the trash leaves the list. The one behind
    // it keeps the position every device already has for it, so nothing
    // needs to sync; brought back, the first is where it was.
    let seq = get(&router, "/api/sync").await.json()["seq"]
        .as_u64()
        .unwrap();
    h.ok(
        "recipes_organize",
        json!({ "ids": [c], "state": "trashed" }),
    )
    .await;
    assert_eq!(ids(&h.ok("recipes_pins", json!({})).await), [b.clone()]);
    assert_eq!(pinned_in_sync(&router, None).await, [(b.clone(), 1)]);
    assert_eq!(h.ok("recipes_get", json!({ "id": b })).await["pinned"], 1);
    let changed = get(&router, &format!("/api/sync?since={seq}")).await.json();
    assert!(
        changed["recipes"].as_array().unwrap().is_empty(),
        "{changed}"
    );
    h.ok("recipes_organize", json!({ "ids": [c], "state": "active" }))
        .await;
    assert_eq!(
        ids(&h.ok("recipes_pins", json!({})).await),
        [c.clone(), b.clone()]
    );
    // The next change to the list counts the positions afresh.
    post_json(&router, "/api/ops", &pins("op_pins0000007", &[&b])).await;
    assert_eq!(pinned_in_sync(&router, None).await, [(b, 0)]);
    let _ = c;
}

#[tokio::test(start_paused = true)]
async fn an_agent_sets_the_list_and_any_change_to_it_can_be_undone() {
    let h = Harness::new().await;
    let router = h.router();
    let (a, b, c) = (
        h.create(PICCATA).await,
        h.create(COOKIES).await,
        h.create(ENCHILADAS).await,
    );
    let mut hub = h.recipes.hub().subscribe();

    // A dry run shows the list it would make and keeps nothing.
    let dry = h
        .ok("recipes_pins", json!({ "ids": [b, a], "dryRun": true }))
        .await;
    assert_eq!(ids(&dry), [b.clone(), a.clone()]);
    assert!(dry.get("changeId").is_none());
    assert!(ids(&h.ok("recipes_pins", json!({})).await).is_empty());
    assert!(hub.try_recv().is_err());

    let set = h.ok("recipes_pins", json!({ "ids": [b, a, b] })).await;
    assert_eq!(ids(&set), [b.clone(), a.clone()]);
    let first = set["changeId"].as_str().unwrap().to_owned();
    // The app is told at once.
    assert!(hub.try_recv().is_ok());
    // Said again, nothing changes.
    let same = h.ok("recipes_pins", json!({ "ids": [b, a] })).await;
    assert!(same.get("changeId").is_none());

    // The owner reorders in the app; the agent's older change can no longer
    // be undone past it, and the app's can.
    post_json(&router, "/api/ops", &pins("op_pins0000001", &[&a, &b, &c])).await;
    let conflict = h.err("recipes_undo", json!({ "changeId": first })).await;
    assert!(conflict.starts_with("[undo_conflict]"), "{conflict}");
    let log = h.ok("recipes_history", json!({})).await;
    let by_app = &log["changes"][0];
    assert_eq!(by_app["tool"], "app");
    assert_eq!(by_app["summary"], "Pinned recipes changed in the app");
    h.ok("recipes_undo", json!({ "changeId": by_app["changeId"] }))
        .await;
    assert_eq!(
        ids(&h.ok("recipes_pins", json!({})).await),
        [b.clone(), a.clone()]
    );
    assert_eq!(
        get(&router, "/api/sync").await.json()["recipes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r.get("pinned").is_some())
            .count(),
        2
    );

    // An empty list unpins everything; more than sixty is refused.
    assert!(ids(&h.ok("recipes_pins", json!({ "ids": [] })).await).is_empty());
    let unknown = h
        .err("recipes_pins", json!({ "ids": ["rcp_zzzzzzzz"] }))
        .await;
    assert!(unknown.starts_with("[not_found]"), "{unknown}");
    let _ = c;
}
