//! Requests written in the app for the kitchen agent: stored before anything
//! is announced, handed to the event journal once (which sends and retries
//! the notice), and answered by an agent with ordinary revisions.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;

use std::time::Duration;

use common::{COOKIES, Harness, ORIGIN, PICCATA, get, post_json};
use omni_recipes::hub::HubEvent;
use omni_recipes::requests::{KITCHEN_TARGET, MAX_OPEN, RequestDelivery};
use omni_runtime::ports::NoticeStatus;
use serde_json::{Value, json};

fn request(op: &str, id: &str, text: &str, context: Value) -> Value {
    let mut op = json!({ "id": op, "at": 1, "type": "request", "requestId": id, "text": text });
    for (key, value) in context.as_object().unwrap() {
        op[key] = value.clone();
    }
    json!({ "ops": [op] })
}

fn library(op: &str, id: &str, text: &str) -> Value {
    request(op, id, text, json!({ "origin": "library" }))
}

async fn queued(h: &Harness) -> Vec<Value> {
    h.ok("recipes_requests_list", json!({})).await["requests"]
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test(start_paused = true)]
async fn a_request_is_stored_once_with_what_the_cook_was_looking_at() {
    let h = Harness::new().await;
    let router = h.router();
    let id = h.create(PICCATA).await;
    let mut hub = h.recipes.hub().subscribe();

    let on_recipe = request(
        "op_request00001",
        "req_aaaaaaaa",
        "  Halve the butter next time.  ",
        json!({ "origin": "recipe", "recipeId": id, "revision": 1, "scale": 2 }),
    );
    let stored = post_json(&router, "/api/ops", &on_recipe).await;
    assert_eq!(stored.status, 200, "{}", stored.text());
    assert_eq!(stored.json()["results"][0]["ok"], true);
    assert_eq!(hub.try_recv().unwrap(), HubEvent::Requests);
    h.advance(Duration::from_secs(1)).await;
    let in_cook = request(
        "op_request00002",
        "req_bbbbbbbb",
        "The sear took longer than this says.",
        json!({ "origin": "cook", "recipeId": id, "revision": 1, "scale": 1, "step": "sear" }),
    );
    assert_eq!(post_json(&router, "/api/ops", &in_cook).await.status, 200);
    h.advance(Duration::from_secs(1)).await;
    let from_library = library("op_request00003", "req_cccccccc", "Add a flatbread recipe.");
    assert_eq!(
        post_json(&router, "/api/ops", &from_library).await.status,
        200
    );

    // The outbox sends an op again after a lost answer, and a reinstalled app
    // could send the request under a new op id: stored once either way.
    let seq = post_json(&router, "/api/ops", &on_recipe).await.json()["seq"].clone();
    let renamed = request(
        "op_request00009",
        "req_aaaaaaaa",
        "Something else entirely.",
        json!({ "origin": "library" }),
    );
    let again = post_json(&router, "/api/ops", &renamed).await.json();
    assert_eq!(again["results"][0]["ok"], true);
    // A request is not a library change: nothing for the app to sync.
    assert_eq!(again["seq"], seq);

    // The app's list: newest first, with the recipe's title.
    let listed = get(&router, "/api/requests").await.json();
    let items = listed["items"].as_array().unwrap();
    let ids: Vec<&str> = items
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["req_cccccccc", "req_bbbbbbbb", "req_aaaaaaaa"]);
    assert_eq!(items[2]["text"], "Halve the butter next time.");
    assert_eq!(items[2]["status"], "queued");
    assert_eq!(items[2]["recipeId"], id.as_str());
    assert_eq!(items[2]["recipeTitle"], "Chicken Piccata");
    assert!(items[0].get("recipeId").is_none());
    assert_eq!(items[0]["origin"], "library");

    // The agent's list: oldest first, with the context and where the notice stands.
    let requests = queued(&h).await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["requestId"], "req_aaaaaaaa");
    assert_eq!(requests[0]["origin"], "recipe");
    assert_eq!(requests[0]["revision"], 1);
    assert_eq!(requests[0]["headRevision"], 1);
    assert_eq!(requests[0]["scale"], 2);
    assert_eq!(requests[0]["delivery"], "pending");
    assert_eq!(requests[1]["origin"], "cook");
    assert_eq!(requests[1]["step"], "sear");
    assert!(
        requests[1]["stepAction"]
            .as_str()
            .is_some_and(|action| !action.is_empty())
    );
    assert!(requests[2].get("recipeId").is_none());

    let about = h
        .ok(
            "recipes_requests_list",
            json!({ "recipeId": id, "status": "open" }),
        )
        .await;
    assert_eq!(about["total"], 2);
    let paged = h
        .ok(
            "recipes_requests_list",
            json!({ "status": "all", "limit": 2 }),
        )
        .await;
    assert_eq!(
        (paged["total"].clone(), paged["nextCursor"].clone()),
        (json!(3), json!(2))
    );
}

#[tokio::test(start_paused = true)]
async fn a_request_that_can_never_be_stored_is_refused_with_a_line_for_the_cook() {
    let h = Harness::new().await;
    let router = h.router();
    let refused = |body: Value| {
        let router = router.clone();
        async move {
            let answer = post_json(&router, "/api/ops", &body).await;
            assert_eq!(answer.status, 200, "{}", answer.text());
            let result = answer.json()["results"][0].clone();
            assert_eq!(result["ok"], false, "{result}");
            result["error"].as_str().unwrap().to_owned()
        }
    };
    assert!(
        refused(library("op_refused00001", "req_aaaaaaaa", "   "))
            .await
            .contains("empty")
    );
    let long = "x".repeat(2001);
    assert!(
        refused(library("op_refused00002", "req_aaaaaaaa", &long))
            .await
            .contains("2,000")
    );
    assert!(
        refused(library("op_refused00003", "request-1", "Hello"))
            .await
            .contains("id")
    );
    let missing = request(
        "op_refused00004",
        "req_aaaaaaaa",
        "Hello",
        json!({ "origin": "recipe", "recipeId": "rcp_zzzzzzzz" }),
    );
    assert!(queued(&h).await.is_empty());
    // A recipe that has gone (the request waited offline) does not cost the
    // text: it is kept as a library request.
    let stored = post_json(&router, "/api/ops", &missing).await.json();
    assert_eq!(stored["results"][0]["ok"], true, "{stored}");
    let kept = &queued(&h).await[0];
    assert_eq!(kept["origin"], "library");
    assert!(kept.get("recipeId").is_none());
    h.ok(
        "recipes_request_update",
        json!({ "requestId": "req_aaaaaaaa", "status": "declined" }),
    )
    .await;

    // Context that does not check out is dropped, not a reason to lose the text.
    let id = h.create(COOKIES).await;
    let odd = request(
        "op_refused00005",
        "req_dddddddd",
        "Less sugar.",
        json!({ "origin": "cook", "recipeId": id, "revision": 99, "scale": -3, "step": "no such step!" }),
    );
    assert_eq!(
        post_json(&router, "/api/ops", &odd).await.json()["results"][0]["ok"],
        true
    );
    let stored = &queued(&h).await[0];
    assert_eq!(stored["recipeId"], id.as_str());
    for absent in ["revision", "scale", "step", "stepAction"] {
        assert!(stored.get(absent).is_none(), "{absent}: {stored}");
    }

    // The app has no login: the queue is bounded.
    for n in 1..MAX_OPEN {
        let body = library(&format!("op_fill{n:08}"), &format!("req_{n:08}"), "More.");
        assert_eq!(
            post_json(&router, "/api/ops", &body).await.json()["results"][0]["ok"],
            true
        );
    }
    assert!(
        refused(library("op_refused00006", "req_overflow", "One more."))
            .await
            .contains("too many")
    );
    // Closing one makes room.
    h.ok(
        "recipes_request_update",
        json!({ "requestId": "req_00000001", "status": "declined", "note": "Not now." }),
    )
    .await;
    let body = library("op_refused00007", "req_overflow", "One more.");
    assert_eq!(
        post_json(&router, "/api/ops", &body).await.json()["results"][0]["ok"],
        true
    );
}

#[tokio::test(start_paused = true)]
async fn an_agent_takes_a_request_and_closes_it_with_the_revision() {
    let h = Harness::new().await;
    let router = h.router();
    let id = h.create(PICCATA).await;
    let body = request(
        "op_request00001",
        "req_aaaaaaaa",
        "Halve the butter.",
        json!({ "origin": "recipe", "recipeId": id, "revision": 1, "scale": 1 }),
    );
    post_json(&router, "/api/ops", &body).await;
    let mut hub = h.recipes.hub().subscribe();

    let taken = h
        .ok(
            "recipes_request_update",
            json!({ "requestId": "req_aaaaaaaa", "status": "working" }),
        )
        .await;
    assert_eq!(taken["changed"], true);
    assert_eq!(taken["request"]["status"], "working");
    assert_eq!(hub.try_recv().unwrap(), HubEvent::Requests);
    // Said twice, it changes nothing and announces nothing.
    let same = h
        .ok(
            "recipes_request_update",
            json!({ "requestId": "req_aaaaaaaa", "status": "working" }),
        )
        .await;
    assert_eq!(same["changed"], false);
    assert!(hub.try_recv().is_err());
    assert!(queued(&h).await.is_empty());

    // The change itself is an ordinary edit, with the household checks.
    let edited = h
        .ok(
            "recipes_edit",
            json!({
                "id": id, "expectedRevision": 1, "summary": "Less butter",
                "ops": [{ "op": "set", "target": "butter", "fields": { "qty": 3 } }]
            }),
        )
        .await;
    let revision = edited["revision"].as_u64().unwrap();
    assert_eq!(revision, 2);
    let _ = hub.try_recv();

    let done = h
        .ok(
            "recipes_request_update",
            json!({
                "requestId": "req_aaaaaaaa", "status": "done", "resultRevision": revision,
                "note": "Butter is now 3 tbsp."
            }),
        )
        .await;
    assert_eq!(done["request"]["status"], "done");
    assert_eq!(done["request"]["resultRevision"], 2);
    assert_eq!(done["request"]["resultRecipeId"], id.as_str());
    assert_eq!(hub.try_recv().unwrap(), HubEvent::Requests);

    let shown = get(&router, "/api/requests").await.json()["items"][0].clone();
    assert_eq!(shown["status"], "done");
    assert_eq!(shown["note"], "Butter is now 3 tbsp.");
    assert_eq!(shown["resultRevision"], 2);
    assert_eq!(shown["resultRecipeId"], id.as_str());

    // What cannot be recorded says why.
    let unknown = h
        .err(
            "recipes_request_update",
            json!({ "requestId": "req_zzzzzzzz", "status": "done" }),
        )
        .await;
    assert!(unknown.starts_with("[not_found]"), "{unknown}");
    let no_revision = h
        .err(
            "recipes_request_update",
            json!({ "requestId": "req_aaaaaaaa", "resultRevision": 9 }),
        )
        .await;
    assert!(no_revision.starts_with("[not_found]"), "{no_revision}");
    let nothing = h
        .err(
            "recipes_request_update",
            json!({ "requestId": "req_aaaaaaaa" }),
        )
        .await;
    assert!(nothing.starts_with("[validation]"), "{nothing}");

    // A library request is answered by a recipe of its own.
    post_json(
        &router,
        "/api/ops",
        &library("op_request00002", "req_bbbbbbbb", "Add cookies."),
    )
    .await;
    let orphan = h
        .err(
            "recipes_request_update",
            json!({ "requestId": "req_bbbbbbbb", "status": "done", "resultRevision": 1 }),
        )
        .await;
    assert!(orphan.contains("resultRecipeId"), "{orphan}");
    let cookies = h.create(COOKIES).await;
    let answered = h
        .ok(
            "recipes_request_update",
            json!({ "requestId": "req_bbbbbbbb", "status": "done", "resultRecipeId": cookies }),
        )
        .await;
    assert_eq!(answered["request"]["resultRecipeId"], cookies.as_str());
    assert_eq!(answered["request"]["resultRevision"], 1);
    // An empty note removes the note.
    let cleared = h
        .ok(
            "recipes_request_update",
            json!({ "requestId": "req_aaaaaaaa", "note": " " }),
        )
        .await;
    assert!(cleared["request"].get("note").is_none());
}

#[tokio::test(start_paused = true)]
async fn a_request_is_handed_to_the_journal_once_keyed_by_its_id() {
    let h = Harness::with_delivery(RequestDelivery::none()).await;
    let router = h.router();
    let id = h.create(PICCATA).await;
    let body = request(
        "op_request00001",
        "req_aaaaaaaa",
        "Halve the butter.",
        json!({ "origin": "cook", "recipeId": id, "revision": 1, "scale": 1.5, "step": "sear" }),
    );
    post_json(&router, "/api/ops", &body).await;
    // Stored before anything is announced.
    assert_eq!(queued(&h).await[0]["delivery"], "pending");
    assert!(h.events.notices().is_empty());

    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 1);
    let notices = h.events.notices();
    assert_eq!(notices.len(), 1);
    let notice = &notices[0];
    assert_eq!(notice.target, KITCHEN_TARGET);
    // The key is the request id: the journal sends it as Idempotency-Key.
    assert_eq!(notice.key, "req_aaaaaaaa");
    assert_eq!(notice.name, "recipe.request");
    let event = &notice.body;
    assert_eq!(event["event"], "recipe.request");
    assert_eq!(event["requestId"], "req_aaaaaaaa");
    assert_eq!(event["text"], "Halve the butter.");
    assert_eq!(event["recipeTitle"], "Chicken Piccata");
    assert_eq!(event["link"], format!("{ORIGIN}/r/{id}"));
    assert_eq!(
        event["context"],
        json!({ "origin": "cook", "recipeId": id, "revision": 1, "scale": 1.5, "step": "sear" })
    );
    assert!(
        event["createdAt"]
            .as_str()
            .is_some_and(|at| at.ends_with('Z'))
    );

    // The tool reports the journal's state of the notice.
    assert_eq!(queued(&h).await[0]["delivery"], "pending");
    h.events
        .settle_notice("req_aaaaaaaa", NoticeStatus::Delivered, 3);
    let delivered = queued(&h).await;
    assert_eq!(
        (
            delivered[0]["delivery"].clone(),
            delivered[0]["deliveryAttempts"].clone()
        ),
        (json!("delivered"), json!(3))
    );
    assert_eq!(delivered[0]["status"], "queued");

    // Handed over once: neither a later pass nor the app resending it hands
    // it over again.
    h.advance_days(2).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);
    post_json(&router, "/api/ops", &body).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);
    assert_eq!(h.events.notices().len(), 1);

    // A library request links to the Requests page.
    post_json(
        &router,
        "/api/ops",
        &library("op_request00002", "req_bbbbbbbb", "Add a flatbread recipe."),
    )
    .await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 1);
    let event = h.events.notices()[1].body.clone();
    assert_eq!(event["link"], format!("{ORIGIN}/requests"));
    assert_eq!(event["recipeTitle"], Value::Null);
    assert_eq!(event["context"]["origin"], "library");
}

#[tokio::test(start_paused = true)]
async fn a_journal_that_cannot_take_the_notice_is_asked_again_and_nothing_is_lost() {
    let h = Harness::with_delivery(RequestDelivery::none()).await;
    let router = h.router();
    h.events.fail();
    post_json(
        &router,
        "/api/ops",
        &library("op_request00001", "req_aaaaaaaa", "Add a flatbread recipe."),
    )
    .await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);
    let left = queued(&h).await;
    assert_eq!(left[0]["status"], "queued");
    assert_eq!(left[0]["delivery"], "pending");
    // Not again before the wait has passed, then again.
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);
    h.events.recover();
    h.advance(Duration::from_secs(29)).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);
    h.advance(Duration::from_secs(1)).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 1);
    assert_eq!(h.events.notices().len(), 1);
    assert_eq!(
        get(&router, "/api/requests").await.json()["items"][0]["status"],
        "queued"
    );
}

#[tokio::test(start_paused = true)]
async fn an_ignored_a_failed_and_an_already_taken_notice_are_reported_as_they_stand() {
    let h = Harness::with_delivery(RequestDelivery::none()).await;
    let router = h.router();
    for (n, id) in [
        "req_aaaaaaaa",
        "req_bbbbbbbb",
        "req_cccccccc",
        "req_dddddddd",
    ]
    .iter()
    .enumerate()
    {
        post_json(
            &router,
            "/api/ops",
            &library(&format!("op_request0000{n}"), id, "More."),
        )
        .await;
        h.advance(Duration::from_secs(1)).await;
    }
    // An agent sweeping the queue took the third before its notice was
    // handed over: it never is.
    h.ok(
        "recipes_request_update",
        json!({ "requestId": "req_cccccccc", "status": "working" }),
    )
    .await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 3);
    h.events
        .settle_notice("req_aaaaaaaa", NoticeStatus::Failed, 1);
    h.events
        .settle_notice("req_bbbbbbbb", NoticeStatus::Ignored, 1);
    // The fourth is taken while its notice still waits: the notice is
    // cancelled.
    h.ok(
        "recipes_request_update",
        json!({ "requestId": "req_dddddddd", "status": "working" }),
    )
    .await;
    let all = h
        .ok("recipes_requests_list", json!({ "status": "all" }))
        .await;
    let states: Vec<&str> = all["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|request| request["delivery"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["failed", "ignored", "skipped", "skipped"]);
    assert_eq!(h.events.notices().len(), 3);
    // All four are still there to be handled.
    assert_eq!(all["requests"][0]["status"], "queued");
    assert_eq!(all["requests"][1]["status"], "queued");
}

// Real time: the service waits on the store's own thread, which a paused
// clock would run ahead of.
#[tokio::test]
async fn the_hand_over_service_runs_as_soon_as_a_request_is_stored() {
    let h = Harness::with_delivery(RequestDelivery::none()).await;
    let router = h.router();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (service, token) = (h.recipes.clone(), shutdown.clone());
    let running = tokio::spawn(async move { service.run_requests(token).await });
    // Its first pass finds nothing.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.events.notices().is_empty());
    post_json(
        &router,
        "/api/ops",
        &library("op_request00001", "req_aaaaaaaa", "Add a flatbread recipe."),
    )
    .await;
    // Well inside the service's own fifteen-second tick: the request woke it.
    let handed = async {
        while h.events.notices().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), handed)
        .await
        .expect("the notice is handed over at once");
    assert_eq!(h.events.notices().len(), 1);
    shutdown.cancel();
    running.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn requests_written_before_the_journal_are_announced_once_it_is_there() {
    let h = Harness::new().await;
    let router = h.router();
    for (n, id) in ["req_aaaaaaaa", "req_bbbbbbbb"].iter().enumerate() {
        post_json(
            &router,
            "/api/ops",
            &library(&format!("op_request0000{n}"), id, "More."),
        )
        .await;
    }
    // As the webhook-era service stored them when no webhook was configured.
    h.recipes
        .store()
        .write(|tx| {
            tx.connection()
                .execute("UPDATE recipes_request SET delivery = 'unconfigured'", [])
                .map_err(|e| omni_store::StoreError::Sqlite(e.to_string()))
        })
        .await
        .unwrap();
    h.ok(
        "recipes_request_update",
        json!({ "requestId": "req_bbbbbbbb", "status": "working" }),
    )
    .await;
    // Only the request nobody has taken yet.
    assert_eq!(h.recipes.announce_unannounced().await.unwrap(), 1);
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 1);
    assert_eq!(h.events.notices().len(), 1);
    assert_eq!(h.events.notices()[0].key, "req_aaaaaaaa");
    assert_eq!(h.recipes.announce_unannounced().await.unwrap(), 0);
}

#[tokio::test(start_paused = true)]
async fn requests_are_archived_restored_and_purged_a_month_after_they_close() {
    let h = Harness::new().await;
    let router = h.router();
    h.create(PICCATA).await;
    for (n, id) in ["req_aaaaaaaa", "req_bbbbbbbb"].iter().enumerate() {
        post_json(
            &router,
            "/api/ops",
            &library(&format!("op_request0000{n}"), id, "More."),
        )
        .await;
    }
    h.ok(
        "recipes_request_update",
        json!({ "requestId": "req_aaaaaaaa", "status": "declined", "note": "Not now." }),
    )
    .await;

    // The hourly archive sees a request as a change and carries it.
    let outcome = h.recipes.backup().await.unwrap();
    assert!(matches!(
        outcome,
        omni_recipes::archive::BackupOutcome::Written { .. }
    ));
    let path = h.recipes.archive_files().unwrap()[0].clone();
    let other = omni_testkit::TestApp::new().await;
    let copy = omni_recipes::Recipes::from_context(&other.ctx);
    let restored = copy.restore(path).await.unwrap();
    assert_eq!(restored.counts["recipes_request"], 2);
    assert_eq!(
        copy.dump().await.unwrap()["recipes_request"],
        h.recipes.dump().await.unwrap()["recipes_request"]
    );
    post_json(
        &router,
        "/api/ops",
        &library("op_request00005", "req_cccccccc", "And another."),
    )
    .await;
    assert!(matches!(
        h.recipes.backup().await.unwrap(),
        omni_recipes::archive::BackupOutcome::Written { .. }
    ));

    // A closed request leaves after 30 days; an open one never does.
    h.advance_days(29).await;
    h.recipes.purge().await.unwrap();
    assert_eq!(
        h.ok("recipes_requests_list", json!({ "status": "all" }))
            .await["total"],
        3
    );
    h.advance_days(2).await;
    h.recipes.purge().await.unwrap();
    let left = h
        .ok("recipes_requests_list", json!({ "status": "all" }))
        .await;
    assert_eq!(left["total"], 2);
    assert!(
        left["requests"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == "queued")
    );
}
