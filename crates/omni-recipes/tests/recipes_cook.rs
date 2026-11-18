//! Cook sessions, from the app over HTTP and from agents over the tools:
//! one event log, applied once per client event, projected into done, ready,
//! timers and adjustments; finishing, idle close and timer alerts.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

mod common;

use std::time::Duration;

use common::{COOKIES, Harness, PICCATA, get, post_json};
use omni_api::events::RECIPES_COOK_FINISHED;
use serde_json::{Value, json};

fn event(id: &str, kind: Value) -> Value {
    let mut event = json!({ "clientEventId": id, "at": 1_767_225_600_000_i64, "actor": "app" });
    for (key, value) in kind.as_object().unwrap() {
        event[key] = value.clone();
    }
    event
}

#[tokio::test(start_paused = true)]
async fn the_app_and_an_agent_cook_one_session_together() {
    let h = Harness::new().await;
    let router = h.router();
    let id = h.create(PICCATA).await;
    let start =
        json!({ "id": "cook_2b7r9d4h", "recipeId": id, "revision": 1, "scale": 1.5, "at": 1 });
    let started = post_json(&router, "/api/cook", &start).await;
    assert_eq!(started.status, 200, "{}", started.text());
    let session = started.json();
    assert_eq!(session["id"], "cook_2b7r9d4h");
    assert_eq!(session["state"], "active");
    assert_eq!(
        (session["revision"].clone(), session["scale"].clone()),
        (json!(1), json!(1.5))
    );
    assert_eq!(session["events"], json!([]));
    assert_eq!(
        session["doc"],
        common::doc(PICCATA),
        "the pinned document rides along"
    );
    // Starting again with the same id returns the same session.
    let again = post_json(&router, "/api/cook", &start).await;
    assert_eq!(again.json()["startedAt"], session["startedAt"]);
    assert_eq!(
        h.ok("recipes_cook_get", json!({})).await["active"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // The app crosses things off; a repeated request applies each event once.
    let batch = json!({ "events": [
        event("ev-1", json!({ "type": "check", "target": "chicken" })),
        event("ev-2", json!({ "type": "check", "target": "dredge" })),
        event("ev-3", json!({ "type": "check", "target": "sear/oil" })),
    ] });
    let first = post_json(&router, "/api/cook/cook_2b7r9d4h/events", &batch).await;
    assert_eq!(first.status, 200, "{}", first.text());
    let second = post_json(&router, "/api/cook/cook_2b7r9d4h/events", &batch).await;
    let events = second.json()["events"].as_array().unwrap().clone();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[1],
        json!({
            "seq": 2, "clientEventId": "ev-2", "at": 1_767_225_600_000_i64, "actor": "app",
            "type": "check", "target": "dredge"
        })
    );

    // An agent sees where the cook is.
    let seen = h
        .ok("recipes_cook_get", json!({ "sessionId": "cook_2b7r9d4h" }))
        .await;
    let view = &seen["session"];
    assert_eq!(view["title"], "Chicken Piccata");
    assert_eq!(view["steps"], 6);
    assert_eq!(view["done"], json!(["dredge"]));
    assert_eq!(view["ready"], json!(["sear"]));
    assert_eq!(view["gathered"], json!(["chicken", "sear/oil"]));
    assert_eq!(view["scale"], 1.5);

    // Crossing off can be taken back.
    let unchecked = h
        .ok(
            "recipes_cook_update",
            json!({ "sessionId": "cook_2b7r9d4h", "events": [
                { "type": "uncheck", "target": "dredge" },
                { "type": "uncheck", "target": "chicken" }
            ] }),
        )
        .await;
    assert_eq!(unchecked["session"]["done"], json!([]));
    assert_eq!(unchecked["session"]["ready"], json!(["dredge"]));
    assert_eq!(unchecked["session"]["gathered"], json!(["sear/oil"]));
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": "cook_2b7r9d4h", "events": [
            { "type": "check", "target": "dredge" },
            { "type": "check", "target": "chicken" }
        ] }),
    )
    .await;

    // The agent helps mid-cook: an adjustment, a note and a timer.
    let updated = h
        .ok(
            "recipes_cook_update",
            json!({ "sessionId": "cook_2b7r9d4h", "events": [
                { "type": "check", "target": "sear" },
                { "type": "adjust", "target": "simmer", "text": "No capers: add 1 tbsp chopped green olives. Simmer as written." },
                { "type": "adjust", "target": "lemon", "text": "3 lemons, they were small" },
                { "type": "note", "text": "Pan ran hot." },
                { "type": "timer", "step": "simmer", "minutes": 5 }
            ] }),
        )
        .await;
    let view = &updated["session"];
    assert_eq!(view["done"], json!(["dredge", "sear"]));
    assert_eq!(view["ready"], json!(["deglaze"]));
    assert_eq!(view["adjustments"].as_array().unwrap().len(), 2);
    assert_eq!(view["notes"], json!(["Pan ran hot."]));
    assert_eq!(view["running"][0]["step"], "simmer");
    assert_eq!(view["running"][0]["remainingSec"], 300);

    // The app sees the agent's events on its next read.
    let read = get(&router, "/api/cook/cook_2b7r9d4h").await.json();
    let events = read["events"].as_array().unwrap();
    assert_eq!(events.len(), 12);
    assert_eq!(events[8]["actor"], "agent");
    assert_eq!(events[8]["type"], "adjust");
    let timer = &events[11];
    assert_eq!(timer["type"], "timer");
    assert_eq!(timer["endsAt"], json!(1_767_225_600_000_i64 + 300_000));

    // Events that name nothing are refused for an agent, with nothing applied.
    for (bad, code) in [
        (json!({ "type": "check", "target": "whisk" }), "[not_found]"),
        (
            json!({ "type": "adjust", "target": "sear/oil", "text": "x" }),
            "[not_found]",
        ),
        (
            json!({ "type": "timer", "step": "nope", "minutes": 1 }),
            "[not_found]",
        ),
        (
            json!({ "type": "timer", "step": "simmer", "minutes": 0 }),
            "[validation]",
        ),
        (json!({ "type": "note", "text": "  " }), "[validation]"),
        (json!({ "type": "rebase", "revision": 9 }), "[validation]"),
    ] {
        let refused = h
            .err("recipes_cook_update", json!({ "sessionId": "cook_2b7r9d4h", "events": [{ "type": "uncheck", "target": "sear" }, bad] }))
            .await;
        assert!(refused.starts_with(code), "{refused}");
    }
    assert_eq!(
        get(&router, "/api/cook/cook_2b7r9d4h").await.json()["events"]
            .as_array()
            .unwrap()
            .len(),
        12
    );

    // The app undoes one adjustment and finishes.
    let finish = json!({ "events": [
        event("ev-9", json!({ "type": "unadjust", "target": "lemon" })),
        event("ev-10", json!({ "type": "finish", "note": "Best one yet." })),
    ] });
    let finished = post_json(&router, "/api/cook/cook_2b7r9d4h/events", &finish)
        .await
        .json();
    assert_eq!(finished["state"], "finished");
    assert!(finished["finishedAt"].is_number());

    // The recipe did not change; the adjustment is on it, with the text the
    // cook saw at the session's scale.
    let recipe = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(recipe["headRevision"], 1);
    assert_eq!(recipe["cooks"]["count"], 1);
    let adjustments = recipe["openAdjustments"].as_array().unwrap();
    assert_eq!(adjustments.len(), 1);
    let adjustment = adjustments[0]["id"].as_str().unwrap().to_owned();
    assert!(adjustment.starts_with("adj_"));
    assert_eq!(adjustments[0]["target"], "simmer");
    assert_eq!(adjustments[0]["sessionId"], "cook_2b7r9d4h");
    assert_eq!(
        adjustments[0]["original"],
        "Add 270 ml chicken stock, the juice of 1½ lemons and 3 tbsp capers to the wine reduction. Simmer until reduced by about half, 4–5 minutes."
    );
    let log = h.ok("recipes_cook_get", json!({ "recipeId": id })).await;
    assert_eq!(log["cooks"][0]["note"], "Best one yet.");
    assert_eq!(log["cooks"][0]["scale"], 1.5);
    assert_eq!(log["cooks"][0]["adjustments"][0]["target"], "simmer");
    assert_eq!(log["openAdjustments"][0]["id"], adjustment);
    assert_eq!(
        h.ok("recipes_cook_get", json!({})).await["active"],
        json!([])
    );

    // One event, once.
    let published = h.events.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].name, RECIPES_COOK_FINISHED);
    assert_eq!(published[0].dedup_key, "cook:cook_2b7r9d4h");
    assert_eq!(
        Value::Object(published[0].data.clone()),
        json!({ "sessionId": "cook_2b7r9d4h", "recipeId": id, "title": "Chicken Piccata", "openAdjustments": 1 })
    );
    // Resending the finish changes nothing.
    post_json(&router, "/api/cook/cook_2b7r9d4h/events", &finish).await;
    assert_eq!(h.events.published().len(), 1);
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["cooks"]["count"],
        1
    );

    // A closed session refuses everything from an agent but a dismissal; the
    // app's late events are dropped.
    let closed = h
        .err(
            "recipes_cook_update",
            json!({ "sessionId": "cook_2b7r9d4h", "events": [{ "type": "note", "text": "late" }] }),
        )
        .await;
    assert!(
        closed.starts_with("[session_closed] cook_2b7r9d4h finished at 20"),
        "{closed}"
    );
    let late = json!({ "events": [event("ev-11", json!({ "type": "check", "target": "serve" }))] });
    let dropped = post_json(&router, "/api/cook/cook_2b7r9d4h/events", &late).await;
    assert_eq!(dropped.status, 200);
    assert_eq!(dropped.json()["events"].as_array().unwrap().len(), 14);

    // An agent folds the adjustment into the recipe.
    let unknown = h
        .err(
            "recipes_edit",
            json!({ "id": id, "expectedRevision": 1, "summary": "x", "resolves": ["adj_zzzzzzzz"],
                    "ops": [{ "op": "set", "target": "capers", "fields": { "optional": true } }] }),
        )
        .await;
    assert!(unknown.starts_with("[not_found]"), "{unknown}");
    let folded = h
        .ok(
            "recipes_edit",
            json!({ "id": id, "expectedRevision": 1, "summary": "Olives instead of capers", "resolves": [adjustment],
                    "ops": [{ "op": "set", "target": "capers", "fields": { "name": "green olives", "prep": "chopped" } }] }),
        )
        .await;
    assert_eq!(folded["revision"], 2);
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["openAdjustments"],
        json!([])
    );
    assert_eq!(
        h.ok("recipes_search", json!({ "query": "piccata" })).await["recipes"][0]["openAdjustments"],
        0
    );
}

/// What the app's outbox relies on: a refusal that can never succeed is a 4xx
/// (it drops the entry), and one that may succeed later is retried.
#[tokio::test(start_paused = true)]
async fn the_apps_outbox_gets_statuses_it_can_act_on() {
    let h = Harness::new().await;
    let router = h.router();
    let id = h.create(PICCATA).await;
    // Events that arrive before their session's start are kept for a retry.
    let early =
        json!({ "events": [event("early-1", json!({ "type": "check", "target": "dredge" }))] });
    let reply = post_json(&router, "/api/cook/cook_offline1/events", &early).await;
    assert_eq!(reply.status, 425, "{}", reply.text());
    let start =
        json!({ "id": "cook_offline1", "recipeId": id, "revision": 1, "scale": 1, "at": 1 });
    assert_eq!(post_json(&router, "/api/cook", &start).await.status, 200);
    let applied = post_json(&router, "/api/cook/cook_offline1/events", &early).await;
    assert_eq!(applied.status, 200);
    assert_eq!(applied.json()["events"].as_array().unwrap().len(), 1);

    // Permanent refusals are 4xx.
    for (path, body, status) in [
        (
            "/api/cook",
            json!({ "id": "cook_bad", "recipeId": id, "revision": 1, "scale": 1, "at": 1 }),
            400,
        ),
        (
            "/api/cook",
            json!({ "id": "cook_offline2", "recipeId": "rcp_zzzzzzzz", "revision": 1, "scale": 1, "at": 1 }),
            404,
        ),
        (
            "/api/cook",
            json!({ "id": "cook_offline2", "recipeId": id, "revision": 7, "scale": 1, "at": 1 }),
            404,
        ),
        ("/api/cook/not-a-session/events", early.clone(), 404),
        (
            "/api/cook/cook_offline1/events",
            json!({ "events": "x" }),
            400,
        ),
    ] {
        let reply = post_json(&router, path, &body).await;
        assert_eq!(reply.status, status, "{path} {body}: {}", reply.text());
    }
    assert_eq!(get(&router, "/api/cook/cook_missing1").await.status, 404);

    // A dismissal is posted to the adjustment's own, finished session.
    let finish = json!({ "events": [
        event("f-1", json!({ "type": "adjust", "target": "sear", "text": "Two batches." })),
        event("f-2", json!({ "type": "finish" })),
    ] });
    post_json(&router, "/api/cook/cook_offline1/events", &finish).await;
    let adjustment = h.ok("recipes_get", json!({ "id": id })).await["openAdjustments"][0].clone();
    assert_eq!(adjustment["sessionId"], "cook_offline1");
    assert_eq!(
        adjustment["original"],
        "Heat 2 tbsp olive oil and 2 tbsp unsalted butter in a large skillet over medium-high heat until the butter foams. Cook the dredged cutlets until golden, 3 to 4 minutes per side, then move to a plate. Keep the pan on the heat."
    );
    let dismiss = json!({ "events": [event("d-1", json!({ "type": "dismissAdjustment", "id": adjustment["id"] }))] });
    let dismissed = post_json(&router, "/api/cook/cook_offline1/events", &dismiss).await;
    assert_eq!(dismissed.status, 200, "{}", dismissed.text());
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["openAdjustments"],
        json!([])
    );

    // The ten-second undo of Delete: state active restores from the trash,
    // and unarchives.
    let op = |n: u32, state: &str| json!({ "ops": [{ "id": format!("op_state{n:06}"), "at": 1, "type": "state", "recipeId": id, "state": state }] });
    assert_eq!(
        post_json(&router, "/api/ops", &op(1, "trashed"))
            .await
            .json()["results"][0]["recipe"],
        Value::Null
    );
    let restored = post_json(&router, "/api/ops", &op(2, "active"))
        .await
        .json();
    assert_eq!(restored["results"][0]["recipe"]["state"], "active");
    post_json(&router, "/api/ops", &op(3, "archived")).await;
    let unarchived = post_json(&router, "/api/ops", &op(4, "active"))
        .await
        .json();
    assert_eq!(unarchived["results"][0]["recipe"]["state"], "active");
    // A repeated op id answers as it did the first time and applies nothing.
    let repeat = post_json(&router, "/api/ops", &op(1, "trashed"))
        .await
        .json();
    assert_eq!(repeat["results"][0]["ok"], true);
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["state"],
        "active"
    );
}

#[tokio::test(start_paused = true)]
async fn adjustments_can_be_dismissed_and_sessions_rebased() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let session = h
        .ok(
            "recipes_cook_start",
            json!({ "recipeId": id, "servings": 8 }),
        )
        .await;
    assert_eq!(session["scale"], 2.0);
    assert_eq!(session["revision"], 1);
    assert!(
        session.get("appUrl").is_none(),
        "the app is not served in this test"
    );
    let sid = session["sessionId"].as_str().unwrap().to_owned();
    assert!(sid.starts_with("cook_") && sid.len() == 13);

    // The recipe is revised while the session is open: it stays pinned until rebased.
    h.ok(
        "recipes_edit",
        json!({ "id": id, "expectedRevision": 1, "summary": "More parsley",
                "ops": [{ "op": "set", "target": "parsley", "fields": { "qty": 3 } }] }),
    )
    .await;
    let pinned = h.ok("recipes_cook_get", json!({ "sessionId": sid })).await;
    assert_eq!(
        (
            pinned["session"]["revision"].clone(),
            pinned["session"]["headRevision"].clone()
        ),
        (json!(1), json!(2))
    );
    let rebased = h
        .ok(
            "recipes_cook_update",
            json!({ "sessionId": sid, "events": [
                { "type": "check", "target": "dredge" },
                { "type": "rebase", "revision": 2 },
                { "type": "adjust", "target": "serve", "text": "Serve with rice." },
                { "type": "finish" }
            ] }),
        )
        .await;
    assert_eq!(rebased["session"]["revision"], 2);
    assert_eq!(
        rebased["session"]["done"],
        json!(["dredge"]),
        "state carries over by id"
    );
    assert_eq!(rebased["session"]["state"], "finished");
    assert_eq!(rebased["session"]["ready"], json!([]));

    let adjustment = h.ok("recipes_get", json!({ "id": id })).await["openAdjustments"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // A rename re-points an open adjustment.
    h.ok(
        "recipes_edit",
        json!({ "id": id, "expectedRevision": 2, "summary": "Rename the last step",
                "ops": [{ "op": "rename", "target": "serve", "to": "plate" }] }),
    )
    .await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["openAdjustments"][0]["target"],
        "plate"
    );
    // Dismissal is valid on the finished session.
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": sid, "events": [{ "type": "dismissAdjustment", "id": adjustment }] }),
    )
    .await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["openAdjustments"],
        json!([])
    );
    let twice = h
        .err("recipes_cook_update", json!({ "sessionId": sid, "events": [{ "type": "dismissAdjustment", "id": adjustment }] }))
        .await;
    assert!(twice.starts_with("[not_found]"), "{twice}");

    let both = h
        .err(
            "recipes_cook_start",
            json!({ "recipeId": id, "servings": 8, "scale": 2 }),
        )
        .await;
    assert!(both.starts_with("[validation]"), "{both}");
    let missing = h
        .err("recipes_cook_start", json!({ "recipeId": "rcp_zzzzzzzz" }))
        .await;
    assert!(missing.starts_with("[not_found]"), "{missing}");
    let unknown = h
        .err("recipes_cook_get", json!({ "sessionId": "cook_zzzzzzzz" }))
        .await;
    assert!(unknown.starts_with("[not_found]"), "{unknown}");
}

#[tokio::test(start_paused = true)]
async fn idle_sessions_close_after_eighteen_hours() {
    let h = Harness::new().await;
    let piccata = h.create(PICCATA).await;
    let cookies = h.create(COOKIES).await;
    let start = |recipe: &str| json!({ "recipeId": recipe });
    let half = h.ok("recipes_cook_start", start(&piccata)).await["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let barely = h.ok("recipes_cook_start", start(&cookies)).await["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    // Three of piccata's six steps, with an adjustment; one of the cookies' eight.
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": half, "events": [
            { "type": "check", "target": "dredge" },
            { "type": "check", "target": "sear" },
            { "type": "check", "target": "deglaze" },
            { "type": "adjust", "target": "sear", "text": "Seared in two batches." }
        ] }),
    )
    .await;
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": barely, "events": [{ "type": "check", "target": "oven" }] }),
    )
    .await;

    h.advance(Duration::from_secs(17 * 3600 + 3540)).await;
    assert_eq!(
        h.recipes.tick().await.unwrap().closed,
        0,
        "17 h 59 min keeps"
    );
    h.advance(Duration::from_secs(60)).await;
    assert_eq!(h.recipes.tick().await.unwrap().closed, 2);
    assert_eq!(h.recipes.tick().await.unwrap().closed, 0);

    // Half the steps done counts as a cook, with its adjustments.
    let closed = h.ok("recipes_cook_get", json!({ "sessionId": half })).await;
    assert_eq!(closed["session"]["state"], "finished");
    let recipe = h.ok("recipes_get", json!({ "id": piccata })).await;
    assert_eq!(recipe["cooks"]["count"], 1);
    assert_eq!(
        recipe["openAdjustments"][0]["text"],
        "Seared in two batches."
    );
    // Fewer does not.
    let abandoned = h
        .ok("recipes_cook_get", json!({ "sessionId": barely }))
        .await;
    assert_eq!(abandoned["session"]["state"], "abandoned");
    assert_eq!(
        h.ok("recipes_get", json!({ "id": cookies })).await["cooks"]["count"],
        0
    );
    assert_eq!(
        h.ok("recipes_cook_get", json!({ "recipeId": cookies }))
            .await["cooks"],
        json!([])
    );
    assert_eq!(
        h.ok("recipes_cook_get", json!({})).await["active"],
        json!([])
    );
    // Only the cook is announced.
    let published = h.events.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].dedup_key, format!("cook:{half}"));
    let late = h
        .err(
            "recipes_cook_update",
            json!({ "sessionId": barely, "events": [{ "type": "check", "target": "whisk" }] }),
        )
        .await;
    assert!(
        late.starts_with("[session_closed]") && late.contains("was closed"),
        "{late}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_ended_timer_is_announced_once_when_no_app_is_watching() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let sid = h.ok("recipes_cook_start", json!({ "recipeId": id })).await["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let timer = |step: &str, minutes: f64| json!({ "sessionId": sid, "events": [{ "type": "timer", "step": step, "minutes": minutes }] });
    h.ok("recipes_cook_update", timer("simmer", 5.0)).await;
    h.ok("recipes_cook_update", timer("sear", 8.0)).await;
    h.ok("recipes_cook_update", timer("deglaze", 2.0)).await;
    // One timer is cancelled and one step is crossed off before they end.
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": sid, "events": [
            { "type": "cancelTimer", "step": "deglaze" },
            { "type": "check", "target": "dredge" }
        ] }),
    )
    .await;

    h.advance(Duration::from_secs(4 * 60 + 50)).await;
    assert_eq!(h.recipes.tick().await.unwrap().alerts, 0, "not ended yet");
    assert!(h.app.pushes.all().is_empty());

    h.advance(Duration::from_secs(15)).await;
    assert_eq!(h.recipes.tick().await.unwrap().alerts, 1);
    let pushes = h.app.pushes.all();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0].message.title.as_deref(), Some("Timer done"));
    assert_eq!(pushes[0].message.message, "Chicken Piccata: simmer");
    // Reserved durably: later passes never send it again.
    for _ in 0..3 {
        h.advance(Duration::from_secs(5)).await;
        assert_eq!(h.recipes.tick().await.unwrap().alerts, 0);
    }
    assert_eq!(h.app.pushes.all().len(), 1);

    // While an app client holds the event stream open it rings by itself;
    // only a timer left unanswered for half a minute is announced.
    let client = h.recipes.hub().subscribe();
    h.advance(Duration::from_secs(3 * 60 - 20)).await;
    assert_eq!(
        h.recipes.tick().await.unwrap().alerts,
        0,
        "the sear timer just ended; the app is connected"
    );
    h.advance(Duration::from_secs(10)).await;
    assert_eq!(h.recipes.tick().await.unwrap().alerts, 0);
    h.advance(Duration::from_secs(25)).await;
    assert_eq!(
        h.recipes.tick().await.unwrap().alerts,
        1,
        "unanswered after the grace period"
    );
    assert_eq!(h.app.pushes.all().len(), 2);
    assert_eq!(
        h.app.pushes.all()[1].message.message,
        "Chicken Piccata: sear"
    );

    // A timer the app answers (cancels) in time is never announced.
    h.ok("recipes_cook_update", timer("mount", 1.0)).await;
    h.advance(Duration::from_secs(70)).await;
    assert_eq!(h.recipes.tick().await.unwrap().alerts, 0);
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": sid, "events": [{ "type": "cancelTimer", "step": "mount" }] }),
    )
    .await;
    drop(client);
    h.advance(Duration::from_secs(60)).await;
    assert_eq!(h.recipes.tick().await.unwrap().alerts, 0);
    assert_eq!(h.app.pushes.all().len(), 2);

    // A timer that ended long ago (the server was down) is not announced late.
    h.ok("recipes_cook_update", timer("serve", 1.0)).await;
    h.advance(Duration::from_secs(20 * 60)).await;
    assert_eq!(h.recipes.tick().await.unwrap().alerts, 0);
    assert_eq!(h.app.pushes.all().len(), 2);
}
