//! Requests written in the app for the kitchen agent: stored before anything
//! is announced, announced once with retries, and answered by an agent with
//! ordinary revisions.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{COOKIES, Harness, ORIGIN, PICCATA, get, post_json};
use futures::future::BoxFuture;
use omni_config::ClaudeNotifyTarget;
use omni_recipes::hub::HubEvent;
use omni_recipes::requests::{
    GIVE_UP_AFTER_MS, LiveTransport, MAX_OPEN, RequestDelivery, RequestTransport, WebhookError,
    WebhookResponse,
};
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

/// A webhook that answers from a script and remembers what it was sent.
#[derive(Default)]
struct FakeWebhook {
    answers: Mutex<VecDeque<Result<WebhookResponse, WebhookError>>>,
    /// The answer once the script runs out.
    fallback: Mutex<Option<Result<WebhookResponse, WebhookError>>>,
    posts: Mutex<Vec<(String, Value)>>,
}

impl FakeWebhook {
    fn scripted(answers: Vec<Result<WebhookResponse, WebhookError>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            ..Self::default()
        })
    }

    fn always(answer: Result<WebhookResponse, WebhookError>) -> Arc<Self> {
        Arc::new(Self {
            fallback: Mutex::new(Some(answer)),
            ..Self::default()
        })
    }

    fn posts(&self) -> Vec<(String, Value)> {
        self.posts.lock().unwrap().clone()
    }
}

impl RequestTransport for FakeWebhook {
    fn post<'a>(
        &'a self,
        _target: &'a ClaudeNotifyTarget,
        idempotency_key: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<WebhookResponse, WebhookError>> {
        self.posts.lock().unwrap().push((
            idempotency_key.to_owned(),
            serde_json::from_str(body).unwrap(),
        ));
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .or_else(|| self.fallback.lock().unwrap().clone())
            .unwrap_or(Ok(status(200)));
        Box::pin(async move { answer })
    }
}

fn status(status: u16) -> WebhookResponse {
    WebhookResponse {
        status,
        ignored: false,
    }
}

fn target(url: &str) -> ClaudeNotifyTarget {
    ClaudeNotifyTarget {
        url: url.parse().unwrap(),
        token: "kitchen-key-0123456789".to_owned(),
        connect: None,
    }
}

async fn with_webhook(webhook: &Arc<FakeWebhook>) -> Harness {
    Harness::with_delivery(RequestDelivery::new(
        Some(target("https://bots.example/hooks/kitchen")),
        webhook.clone(),
    ))
    .await
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
    assert_eq!(requests[0]["delivery"], "unconfigured");
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
async fn a_failing_webhook_is_retried_with_one_key_and_the_request_stays_queued() {
    let webhook = FakeWebhook::scripted(vec![
        Err(WebhookError::Network),
        Ok(status(503)),
        Ok(status(200)),
    ]);
    let h = with_webhook(&webhook).await;
    let router = h.router();
    let id = h.create(PICCATA).await;
    let body = request(
        "op_request00001",
        "req_aaaaaaaa",
        "Halve the butter.",
        json!({ "origin": "cook", "recipeId": id, "revision": 1, "scale": 1.5, "step": "sear" }),
    );
    post_json(&router, "/api/ops", &body).await;
    // Stored before anything is sent.
    assert_eq!(queued(&h).await[0]["delivery"], "pending");
    assert!(webhook.posts().is_empty());

    // The first attempt fails: the request is still queued, visible and listed.
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 1);
    let waiting = queued(&h).await;
    assert_eq!(waiting[0]["status"], "queued");
    assert_eq!(
        (
            waiting[0]["delivery"].clone(),
            waiting[0]["deliveryAttempts"].clone()
        ),
        (json!("pending"), json!(1))
    );
    assert_eq!(
        get(&router, "/api/requests").await.json()["items"][0]["status"],
        "queued"
    );
    // Not again before the backoff has passed.
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);
    h.advance(Duration::from_secs(29)).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);
    h.advance(Duration::from_secs(1)).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 1);
    assert_eq!(queued(&h).await[0]["delivery"], "pending");
    // The wait doubles.
    h.advance(Duration::from_secs(59)).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);
    h.advance(Duration::from_secs(1)).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 1);
    let delivered = queued(&h).await;
    assert_eq!(
        (
            delivered[0]["delivery"].clone(),
            delivered[0]["deliveryAttempts"].clone()
        ),
        (json!("delivered"), json!(3))
    );
    // Delivered is final.
    h.advance_days(2).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);

    // Every attempt carried the same key and the same event, so the receiver
    // can collapse a resend whose first answer was lost.
    let posts = webhook.posts();
    assert_eq!(posts.len(), 3);
    assert!(posts.iter().all(|(key, _)| key == "req_aaaaaaaa"));
    assert!(posts.iter().all(|(_, body)| body == &posts[0].1));
    let event = &posts[0].1;
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

    // The app sending the request again does not announce it again.
    post_json(&router, "/api/ops", &body).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);
    assert_eq!(webhook.posts().len(), 3);

    // A library request links to the Requests page.
    post_json(
        &router,
        "/api/ops",
        &library("op_request00002", "req_bbbbbbbb", "Add a flatbread recipe."),
    )
    .await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 1);
    let event = webhook.posts()[3].1.clone();
    assert_eq!(event["link"], format!("{ORIGIN}/requests"));
    assert_eq!(event["recipeTitle"], Value::Null);
    assert_eq!(event["context"]["origin"], "library");
}

#[tokio::test(start_paused = true)]
async fn a_webhook_that_stays_down_is_given_up_after_a_day_and_nothing_is_lost() {
    let webhook = FakeWebhook::always(Err(WebhookError::Timeout));
    let h = with_webhook(&webhook).await;
    let router = h.router();
    post_json(
        &router,
        "/api/ops",
        &library("op_request00001", "req_aaaaaaaa", "Add a flatbread recipe."),
    )
    .await;
    let mut passed = 0;
    // The attempt that gives up is the first one a day after the request.
    while passed <= GIVE_UP_AFTER_MS + 31 * 60_000 {
        h.recipes.deliver_requests().await.unwrap();
        h.advance(Duration::from_secs(60)).await;
        passed += 60_000;
    }
    let attempts = webhook.posts().len();
    // 30 s, 1, 2, 4, 8 and 16 minutes, then every half hour.
    assert!((45..=60).contains(&attempts), "{attempts}");
    let left = queued(&h).await;
    assert_eq!(left[0]["status"], "queued");
    assert_eq!(left[0]["delivery"], "failed");
    h.advance_days(1).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);
    assert_eq!(webhook.posts().len(), attempts);
    assert_eq!(
        get(&router, "/api/requests").await.json()["items"][0]["status"],
        "queued"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_an_ignored_and_an_already_taken_notice_settle_without_a_retry() {
    let webhook = FakeWebhook::scripted(vec![
        Ok(status(401)),
        Ok(WebhookResponse {
            status: 200,
            ignored: true,
        }),
    ]);
    let h = with_webhook(&webhook).await;
    let router = h.router();
    for (n, id) in ["req_aaaaaaaa", "req_bbbbbbbb", "req_cccccccc"]
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
    // An agent sweeping the queue took the third before its notice went out.
    h.ok(
        "recipes_request_update",
        json!({ "requestId": "req_cccccccc", "status": "working" }),
    )
    .await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 2);
    let all = h
        .ok("recipes_requests_list", json!({ "status": "all" }))
        .await;
    let states: Vec<&str> = all["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|request| request["delivery"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["failed", "ignored", "skipped"]);
    h.advance_days(1).await;
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);
    assert_eq!(webhook.posts().len(), 2);
    // All three are still there to be handled.
    assert_eq!(all["requests"][0]["status"], "queued");
    assert_eq!(all["requests"][1]["status"], "queued");
}

// Real time: the service waits on the store's own thread, which a paused
// clock would run ahead of.
#[tokio::test]
async fn the_delivery_service_sends_as_soon_as_a_request_is_stored() {
    let webhook = FakeWebhook::always(Ok(status(200)));
    let h = with_webhook(&webhook).await;
    let router = h.router();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (service, token) = (h.recipes.clone(), shutdown.clone());
    let running = tokio::spawn(async move { service.run_requests(token).await });
    // Its first pass finds nothing.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(webhook.posts().is_empty());
    post_json(
        &router,
        "/api/ops",
        &library("op_request00001", "req_aaaaaaaa", "Add a flatbread recipe."),
    )
    .await;
    // Well inside the service's own fifteen-second tick: the request woke it.
    let delivered = async {
        while queued(&h).await[0]["delivery"] != "delivered" {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), delivered)
        .await
        .expect("the notice goes out at once");
    assert_eq!(webhook.posts().len(), 1);
    shutdown.cancel();
    running.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn requests_written_before_the_webhook_was_set_up_are_announced_once_it_is() {
    // Written with no webhook configured.
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
    h.ok(
        "recipes_request_update",
        json!({ "requestId": "req_bbbbbbbb", "status": "working" }),
    )
    .await;
    assert_eq!(h.recipes.announce_unannounced().await.unwrap(), 0);
    assert_eq!(h.recipes.deliver_requests().await.unwrap(), 0);

    // The same library after a restart with the webhook configured.
    let webhook = FakeWebhook::always(Ok(status(200)));
    let configured = h
        .recipes
        .clone()
        .with_request_delivery(RequestDelivery::new(
            Some(target("https://bots.example/hooks/kitchen")),
            webhook.clone(),
        ));
    // Only the request nobody has taken yet.
    assert_eq!(configured.announce_unannounced().await.unwrap(), 1);
    assert_eq!(configured.deliver_requests().await.unwrap(), 1);
    assert_eq!(webhook.posts().len(), 1);
    assert_eq!(webhook.posts()[0].0, "req_aaaaaaaa");
    assert_eq!(configured.announce_unannounced().await.unwrap(), 0);
}

#[tokio::test]
async fn the_live_transport_posts_json_with_the_key_and_the_bearer() {
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .and(path("/hooks/kitchen"))
        .and(header("authorization", "Bearer kitchen-key-0123456789"))
        .and(header("idempotency-key", "req_aaaaaaaa"))
        .and(header("x-fond-event", "recipe.request"))
        .and(header("content-type", "application/json"))
        .and(body_partial_json(json!({ "event": "recipe.request" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .expect(1)
        .mount(&server)
        .await;
    let client = omni_testkit::mock_http(&server, &["https://bots.example"]);
    let live = LiveTransport::new(client.clone(), omni_http::SideEffectMode::Live);
    let kitchen = target("https://bots.example/hooks/kitchen");
    let body = r#"{"event":"recipe.request"}"#;
    let sent = live.post(&kitchen, "req_aaaaaaaa", body).await.unwrap();
    assert_eq!(sent, status(200));

    // A page that is not a webhook's answer (a proxy's error page) is not
    // worth sending to again.
    Mock::given(method("POST"))
        .and(path("/hooks/proxy"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(100_000)))
        .mount(&server)
        .await;
    let page = live
        .post(
            &target("https://bots.example/hooks/proxy"),
            "req_aaaaaaaa",
            body,
        )
        .await;
    assert_eq!(page, Err(WebhookError::Unusable));

    // A preview or a shadow run records and sends nothing.
    let recorded = LiveTransport::new(client, omni_http::SideEffectMode::Record);
    assert_eq!(
        recorded.post(&kitchen, "req_aaaaaaaa", body).await.unwrap(),
        status(200)
    );
    server.verify().await;
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
