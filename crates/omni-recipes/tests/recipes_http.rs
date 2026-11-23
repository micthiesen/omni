//! The app's HTTP API on its own host: sync, the event stream, record edits
//! applied once, the mutation guard, images served only when referenced, and
//! the static files of the app with no catch-all.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

use crate::common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use common::{
    COOKIES, HOST, Harness, LASAGNE, ORIGIN, PICCATA, get, jpeg, post_json, send, upload,
};
use futures::StreamExt as _;
use omni_server_kit::host_app::{HostApp, dispatch};
use serde_json::{Value, json};

/// Fond behind the host dispatch, as Omni serves it.
fn served(h: &Harness) -> Router {
    let app = HostApp::new(ORIGIN, h.router()).unwrap();
    dispatch(Router::new().fallback(|| async { "omni" }), vec![app])
}

fn ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|recipe| recipe["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn sync_is_full_first_and_incremental_after() {
    let h = Harness::new().await;
    let router = served(&h);
    h.tag("italian", "cuisine").await;
    let piccata = h.create(PICCATA).await;
    let cookies = h.create(COOKIES).await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [piccata], "addTags": ["italian"] }),
    )
    .await;

    let full = get(&router, "/api/sync").await;
    assert_eq!(full.status, 200);
    let full = full.json();
    assert_eq!(full["full"], true);
    assert_eq!(full["liveCount"], 2);
    assert_eq!(full["removed"], json!([]));
    assert_eq!(
        full["tags"],
        json!([{ "name": "italian", "group": "cuisine", "description": null, "count": 1 }])
    );
    assert_eq!(full["sessions"], json!([]));
    assert_eq!(full["inboxPending"], 0);
    assert_eq!(full["build"], "0fed18e0aaaa", "the running build");
    let record = full["recipes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|recipe| recipe["id"] == piccata)
        .unwrap();
    assert_eq!(record["revision"], 1);
    assert_eq!(record["format"], 1);
    assert_eq!(record["state"], "active");
    assert_eq!(record["doc"], common::doc(PICCATA));
    assert_eq!(record["tags"], json!(["italian"]));
    assert_eq!(
        (record["notes"].clone(), record["notesRev"].clone()),
        (json!(""), json!(0))
    );
    assert_eq!(
        (record["activeMin"].clone(), record["totalMin"].clone()),
        (json!(25), json!(25))
    );
    assert_eq!(record["cookCount"], 0);
    assert_eq!(record["images"], json!([]));
    // Kept empty for apps installed before adjustments were removed.
    assert_eq!(record["adjustments"], json!([]));
    let seq = full["seq"].as_u64().unwrap();

    // Nothing changed: nothing comes back.
    let idle = get(&router, &format!("/api/sync?since={seq}")).await.json();
    assert_eq!(idle["full"], false);
    assert_eq!(idle["recipes"], json!([]));
    assert_eq!(idle["seq"], seq);

    // An edit, a new recipe, and one sent to the trash.
    h.ok(
        "recipes_edit",
        json!({ "id": piccata, "expectedRevision": 1, "summary": "More parsley",
                "ops": [{ "op": "set", "target": "parsley", "fields": { "qty": 3 } }] }),
    )
    .await;
    let lasagne = h.create(LASAGNE).await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [cookies], "state": "trashed" }),
    )
    .await;
    let delta = get(&router, &format!("/api/sync?since={seq}")).await.json();
    assert_eq!(delta["full"], false);
    let mut changed = ids(&delta["recipes"]);
    changed.sort();
    let mut expected = vec![piccata.clone(), lasagne.clone()];
    expected.sort();
    assert_eq!(changed, expected);
    assert_eq!(delta["removed"], json!([cookies]));
    assert_eq!(delta["liveCount"], 2);
    assert!(delta["seq"].as_u64().unwrap() > seq);

    // A sequence number from another database starts over.
    let reset = get(&router, "/api/sync?since=999999").await.json();
    assert_eq!(reset["full"], true);
    assert_eq!(reset["recipes"].as_array().unwrap().len(), 2);
    assert_eq!(reset["removed"], json!([]));

    // An open cook session and a pending inbox item ride along.
    h.ok("recipes_cook_start", json!({ "recipeId": piccata }))
        .await;
    let reply = upload(&router, "/api/inbox?item=p1&index=0&count=1", jpeg(1)).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let now = get(&router, "/api/sync").await.json();
    assert_eq!(now["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(now["sessions"][0]["recipeId"], piccata);
    assert_eq!(now["inboxPending"], 1);
    let inbox = get(&router, "/api/inbox").await.json();
    assert_eq!(inbox["items"][0]["kind"], "photo");
    assert_eq!(inbox["items"][0]["state"], "pending");
    assert_eq!(inbox["items"][0]["files"], 1);
}

fn notes_op(id: &str, recipe: &str, text: &str, base: u32) -> Value {
    json!({ "id": id, "at": 1, "type": "notes", "recipeId": recipe, "text": text, "baseRev": base })
}

#[tokio::test(start_paused = true)]
async fn the_apps_edits_apply_once_and_agents_can_undo_them() {
    let h = Harness::new().await;
    let router = served(&h);
    let id = h.create(PICCATA).await;

    let first = json!({ "ops": [notes_op("op_000000000001", &id, "Use the big skillet.", 0)] });
    let saved = post_json(&router, "/api/ops", &first).await;
    assert_eq!(saved.status, 200, "{}", saved.text());
    let saved = saved.json();
    assert_eq!(saved["results"][0]["ok"], true);
    assert_eq!(
        saved["results"][0]["recipe"]["notes"],
        "Use the big skillet."
    );
    assert_eq!(saved["results"][0]["recipe"]["notesRev"], 1);
    // The outbox sends it again: applied once.
    let resent = post_json(&router, "/api/ops", &first).await.json();
    assert_eq!(resent["seq"], saved["seq"]);
    assert_eq!(resent["results"][0]["recipe"]["notesRev"], 1);

    // Two devices edit from the same base: both texts are kept, the server's first.
    let phone = post_json(
        &router,
        "/api/ops",
        &json!({ "ops": [notes_op("op_000000000002", &id, "Use the big skillet. Double the sauce.", 1)] }),
    )
    .await
    .json();
    assert_eq!(phone["results"][0]["recipe"]["notesRev"], 2);
    let tablet = post_json(
        &router,
        "/api/ops",
        &json!({ "ops": [notes_op("op_000000000003", &id, "Use the big skillet. Serve with orzo.", 1)] }),
    )
    .await
    .json();
    assert_eq!(
        tablet["results"][0]["recipe"]["notes"],
        "Use the big skillet. Double the sauce.\n\nUse the big skillet. Serve with orzo."
    );
    assert_eq!(tablet["results"][0]["recipe"]["notesRev"], 3);

    // Archive, delete, and an op that can never apply.
    let states = post_json(
        &router,
        "/api/ops",
        &json!({ "ops": [
            { "id": "op_000000000004", "at": 2, "type": "state", "recipeId": id, "state": "archived" },
            { "id": "op_000000000005", "at": 3, "type": "state", "recipeId": "rcp_zzzzzzzz", "state": "trashed" },
            { "id": "op_000000000006", "at": 4, "type": "state", "recipeId": id, "state": "trashed" }
        ] }),
    )
    .await
    .json();
    assert_eq!(states["results"][0]["recipe"]["state"], "archived");
    assert_eq!(states["results"][1]["ok"], false);
    assert_eq!(states["results"][1]["error"], "no such recipe");
    assert_eq!(states["results"][2]["ok"], true);
    assert_eq!(
        states["results"][2]["recipe"],
        Value::Null,
        "a trashed recipe is not sent back"
    );
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["state"],
        "trashed"
    );

    // The app's changes are in the change log, and an agent can undo them.
    let log = h.ok("recipes_history", json!({})).await;
    let changes = log["changes"].as_array().unwrap();
    assert_eq!(changes[0]["tool"], "app");
    assert_eq!(
        changes[0]["summary"],
        "Chicken Piccata set to trashed in the app"
    );
    h.ok(
        "recipes_undo",
        json!({ "changeId": changes[0]["changeId"] }),
    )
    .await;
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["state"],
        "archived"
    );
    let notes_change = changes
        .iter()
        .find(|change| change["summary"].as_str().unwrap().starts_with("Notes of"))
        .unwrap();
    h.ok(
        "recipes_undo",
        json!({ "changeId": notes_change["changeId"] }),
    )
    .await;
    let recipe = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(recipe["notes"], "Use the big skillet. Double the sauce.");

    // The stored original, for a recipe that has one.
    assert_eq!(
        get(&router, &format!("/api/original/{id}")).await.status,
        404
    );
    let with_source = h
        .ok(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": common::doc(COOKIES), "sourceText": "Cookies\n225 g butter" }),
        )
        .await;
    let original = get(
        &router,
        &format!("/api/original/{}", with_source["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(original.status, 200);
    let original = original.json();
    assert_eq!(original["kind"], "text");
    assert_eq!(original["text"], "Cookies\n225 g butter");
    assert_eq!(original["files"], 0);
}

#[tokio::test(start_paused = true)]
async fn mutations_need_the_apps_origin() {
    let h = Harness::new().await;
    let router = served(&h);
    let id = h.create(PICCATA).await;
    let body = json!({ "ops": [notes_op("op_00000000000a", &id, "Hello", 0)] }).to_string();
    let post = |headers: &[(&str, &str)]| {
        let mut request = Request::post("/api/ops").header(header::HOST, HOST);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(Body::from(body.clone())).unwrap()
    };
    let json_type = ("content-type", "application/json");
    for headers in [
        &[json_type][..],
        &[("origin", "https://evil.example"), json_type],
        &[("origin", "http://recipes.example.com"), json_type],
        &[("origin", "null"), json_type],
        &[
            ("origin", ORIGIN),
            json_type,
            ("sec-fetch-site", "cross-site"),
        ],
        &[("origin", ORIGIN), ("content-type", "text/plain")],
        &[("origin", ORIGIN)],
    ] {
        let reply = send(&router, post(headers)).await;
        assert_eq!(reply.status, 403, "{headers:?}");
        assert!(reply.json()["error"].is_string());
    }
    assert_eq!(h.ok("recipes_get", json!({ "id": id })).await["notes"], "");
    let own = send(
        &router,
        post(&[
            ("origin", ORIGIN),
            json_type,
            ("sec-fetch-site", "same-origin"),
        ]),
    )
    .await;
    assert_eq!(own.status, 200);

    // Uploads take the custom header instead of a JSON type, and still the origin.
    let upload_request = |headers: &[(&str, &str)]| {
        let mut request =
            Request::post("/api/inbox?item=x&index=0&count=1").header(header::HOST, HOST);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(Body::from(jpeg(1))).unwrap()
    };
    for headers in [
        &[("origin", ORIGIN)][..],
        &[("origin", ORIGIN), json_type],
        &[("x-fond-upload", "1")],
        &[("origin", "https://evil.example"), ("x-fond-upload", "1")],
    ] {
        let reply = send(&router, upload_request(headers)).await;
        assert_eq!(reply.status, 403, "{headers:?}");
    }
    let stored = send(
        &router,
        upload_request(&[("origin", ORIGIN), ("x-fond-upload", "1")]),
    )
    .await;
    assert_eq!(stored.status, 200, "{}", stored.text());
    // The same rule covers the photo of a cook.
    let session = h.ok("recipes_cook_start", json!({ "recipeId": id })).await["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let refused = send(
        &router,
        Request::post(format!("/api/cook/{session}/photo"))
            .header(header::HOST, HOST)
            .header(header::ORIGIN, ORIGIN)
            .body(Body::from(jpeg(2)))
            .unwrap(),
    )
    .await;
    assert_eq!(refused.status, 403);
    let photo_path = format!("/api/cook/{session}/photo?id=up-1");
    let photo = upload(&router, &photo_path, jpeg(2)).await;
    assert_eq!(photo.status, 200, "{}", photo.text());
    // The session comes back with the document it is pinned to.
    assert_eq!(photo.json()["doc"], common::doc(PICCATA));
    // A retried upload stores the photo once.
    let retried = upload(&router, &photo_path, jpeg(2)).await;
    assert_eq!(retried.status, 200);
    let recipe = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(recipe["images"].as_array().unwrap().len(), 1);
    assert_eq!(recipe["images"][0]["role"], "dish");
    let bad = upload(
        &router,
        &format!("/api/cook/{session}/photo?id=a%20b"),
        jpeg(2),
    )
    .await;
    assert_eq!(bad.status, 400);
    // Reads carry no such requirement.
    let read = send(
        &router,
        Request::get("/api/sync")
            .header(header::HOST, HOST)
            .header(header::ORIGIN, "https://evil.example")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(read.status, 200);
}

#[tokio::test(start_paused = true)]
async fn images_are_served_only_while_something_refers_to_them() {
    let h = Harness::new().await;
    let router = served(&h);
    let id = h.create(PICCATA).await;
    let session = h.ok("recipes_cook_start", json!({ "recipeId": id })).await["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let reply = upload(&router, &format!("/api/cook/{session}/photo"), jpeg(3)).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let record = &get(&router, "/api/sync").await.json()["recipes"][0];
    let image = &record["images"][0];
    let digest = image["digest"].as_str().unwrap().to_owned();
    assert_eq!(digest.len(), 64);
    assert_eq!(record["cover"], image["id"]);
    assert!(image["color"].as_str().unwrap().starts_with('#'));
    assert_eq!(
        (image["width"].clone(), image["height"].clone()),
        (json!(640), json!(480))
    );

    for (variant, edge) in [("thumb", 480), ("full", 640)] {
        let served = get(&router, &format!("/img/{digest}/{variant}.jpg")).await;
        assert_eq!(served.status, 200, "{variant}");
        assert_eq!(served.header("content-type"), Some("image/jpeg"));
        assert_eq!(
            served.header("cache-control"),
            Some("public, max-age=31536000, immutable")
        );
        assert_eq!(image::load_from_memory(&served.body).unwrap().width(), edge);
    }
    for path in [
        format!("/img/{digest}/original.jpg"),
        format!("/img/{digest}/thumb.png"),
        format!("/img/{digest}/thumb"),
        format!("/img/{}/thumb.jpg", "0".repeat(64)),
        "/img/..%2F..%2Fdocstore.db/thumb.jpg".to_owned(),
        "/img/short/thumb.jpg".to_owned(),
    ] {
        assert_eq!(get(&router, &path).await.status, 404, "{path}");
    }
    // A stored file nothing refers to does not exist for the app.
    let stray = h.recipes.files().put(jpeg(99)).await.unwrap();
    assert_eq!(
        get(&router, &format!("/img/{}/thumb.jpg", stray.sha256))
            .await
            .status,
        404
    );
    // Removing the image makes its URL stop answering.
    h.ok(
        "recipes_images_edit",
        json!({ "id": id, "remove": [image["id"]] }),
    )
    .await;
    assert_eq!(
        get(&router, &format!("/img/{digest}/thumb.jpg"))
            .await
            .status,
        404
    );

    // An inbox item's photos, for the Original view.
    let item = upload(&router, "/api/inbox?item=p9&index=0&count=1", jpeg(4))
        .await
        .json()["itemId"]
        .as_str()
        .unwrap()
        .to_owned();
    let photo = get(&router, &format!("/src/{item}/0.jpg")).await;
    assert_eq!(photo.status, 200);
    assert_eq!(photo.header("content-type"), Some("image/jpeg"));
    for path in [
        format!("/src/{item}/1.jpg"),
        format!("/src/{item}/0.png"),
        "/src/inb_zzzzzzzz/0.jpg".to_owned(),
    ] {
        assert_eq!(get(&router, &path).await.status, 404, "{path}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_app_shell_is_served_on_its_routes_and_nothing_else() {
    let h = Harness::new().await;
    let router = served(&h);

    // Not built yet: the API works and the page says so.
    let unbuilt = get(&router, "/").await;
    assert_eq!(unbuilt.status, 503);
    assert!(unbuilt.text().contains("not built"));
    assert_eq!(get(&router, "/api/sync").await.status, 200);
    assert_eq!(get(&router, "/sw.js").await.status, 404);

    let dist = h.dist.path();
    std::fs::create_dir_all(dist.join("assets")).unwrap();
    std::fs::create_dir_all(dist.join("icons")).unwrap();
    std::fs::write(
        dist.join("index.html"),
        "<!doctype html><title>Fond</title>",
    )
    .unwrap();
    std::fs::write(
        dist.join("sw.js"),
        "self.addEventListener('fetch', () => {});",
    )
    .unwrap();
    std::fs::write(
        dist.join("manifest.webmanifest"),
        r#"{"name":"Fond","scope":"/"}"#,
    )
    .unwrap();
    std::fs::write(dist.join("assets/app-1a2b.js"), "export default 1;").unwrap();
    std::fs::write(dist.join("assets/app-1a2b.js.br"), b"brotli bytes").unwrap();
    std::fs::write(dist.join("assets/app-1a2b.js.gz"), b"gzip bytes").unwrap();
    std::fs::write(dist.join("assets/fond_bg-9f.wasm"), b"\0asm").unwrap();
    std::fs::write(dist.join("icons/icon-192-v1.png"), b"png").unwrap();
    std::fs::write(dist.join("secret.txt"), "not served").unwrap();

    for path in ["/", "/r/rcp_7h2k9m4x", "/cook/cook_2b7r9d4h", "/inbox"] {
        let page = get(&router, path).await;
        assert_eq!(page.status, 200, "{path}");
        assert_eq!(page.text(), "<!doctype html><title>Fond</title>");
        assert_eq!(
            page.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(page.header("cache-control"), Some("no-cache"));
        let csp = page.header("content-security-policy").unwrap();
        for part in [
            "script-src 'self' 'wasm-unsafe-eval'",
            "img-src 'self' blob: data:",
            "connect-src 'self'",
            "worker-src 'self'",
            "manifest-src 'self'",
        ] {
            assert!(csp.contains(part), "{csp}");
        }
    }
    let worker = get(&router, "/sw.js").await;
    assert_eq!(worker.status, 200);
    assert_eq!(
        worker.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(worker.header("cache-control"), Some("no-cache"));
    let manifest = get(&router, "/manifest.webmanifest").await;
    assert_eq!(
        manifest.header("content-type"),
        Some("application/manifest+json")
    );
    assert_eq!(manifest.header("cache-control"), Some("no-cache"));

    let asset = get(&router, "/assets/app-1a2b.js").await;
    assert_eq!(asset.text(), "export default 1;");
    assert_eq!(
        asset.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(asset.header("content-encoding"), None);
    for (accept, encoding, body) in [
        ("gzip, deflate, br", "br", "brotli bytes"),
        ("gzip;q=0.8", "gzip", "gzip bytes"),
    ] {
        let compressed = send(
            &router,
            Request::get("/assets/app-1a2b.js")
                .header(header::HOST, HOST)
                .header(header::ACCEPT_ENCODING, accept)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(compressed.header("content-encoding"), Some(encoding));
        assert_eq!(compressed.text(), body);
        assert_eq!(
            compressed.header("content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(compressed.header("vary"), Some("Accept-Encoding"));
    }
    assert_eq!(
        get(&router, "/assets/fond_bg-9f.wasm")
            .await
            .header("content-type"),
        Some("application/wasm")
    );
    assert_eq!(
        get(&router, "/icons/icon-192-v1.png")
            .await
            .header("content-type"),
        Some("image/png")
    );

    // No catch-all: everything else is a 404, as JSON under /api.
    for path in [
        "/index.html",
        "/secret.txt",
        "/assets/missing.js",
        "/assets/../secret.txt",
        "/assets/..%2Fsecret.txt",
        "/icons/..%2F..%2Fsecret.txt",
        "/r",
        "/r/a/b",
        "/recipes",
        "/gateway/mcp",
        "/favicon.ico",
    ] {
        let reply = get(&router, path).await;
        assert_eq!(reply.status, 404, "{path}");
        assert!(
            !reply.text().contains("Fond") && !reply.text().contains("not served"),
            "{path}"
        );
    }
    for path in ["/api", "/api/data/entities", "/api/cook", "/api/sync/x"] {
        let reply = get(&router, path).await;
        assert!(
            reply.status == 404 || reply.status == 405,
            "{path}: {}",
            reply.status
        );
    }
    assert_eq!(
        get(&router, "/api/data/entities").await.text(),
        r#"{"error":"Not found"}"#
    );
    // Another host never reaches any of it.
    let other = send(
        &router,
        Request::get("/api/sync")
            .header(header::HOST, "omni.boris")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(other.text(), "omni");

    // Nested asset paths (wasm-bindgen snippets) are served too, and are
    // revalidated: their path does not change when the file does.
    std::fs::create_dir_all(dist.join("assets/snippets/fond-1a/js")).unwrap();
    std::fs::write(
        dist.join("assets/snippets/fond-1a/js/idb.js"),
        "export const x = 1;",
    )
    .unwrap();
    let snippet = get(&router, "/assets/snippets/fond-1a/js/idb.js").await;
    assert_eq!(snippet.status, 200);
    assert_eq!(snippet.header("cache-control"), Some("no-cache"));
    assert_eq!(
        get(&router, "/assets/app-1a2b.js")
            .await
            .header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
}

async fn next_frame(
    stream: &mut (impl futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin),
) -> String {
    loop {
        let chunk = stream.next().await.unwrap().unwrap();
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        if !text.starts_with("event: ping") {
            return text;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_event_stream_says_when_to_sync() {
    let h = Harness::new().await;
    let router = h.router();
    let id = h.create(PICCATA).await;
    let response = tower::ServiceExt::oneshot(
        router.clone(),
        Request::get("/api/events")
            .header(header::HOST, HOST)
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    let mut stream = response.into_body().into_data_stream();

    let hello = next_frame(&mut stream).await;
    assert!(hello.starts_with("event: hello\n"), "{hello}");
    let seq = get(&router, "/api/sync").await.json()["seq"]
        .as_u64()
        .unwrap();
    assert!(
        hello.contains(&format!(
            "data: {{\"seq\":{seq},\"build\":\"0fed18e0aaaa\"}}"
        )),
        "{hello}"
    );
    assert_eq!(h.recipes.hub().clients(), 1);

    // A tool write and an app write both announce the library.
    h.tag("italian", "cuisine").await;
    let library = next_frame(&mut stream).await;
    assert!(library.starts_with("event: library\n"), "{library}");
    assert!(
        library.contains(&format!("\"seq\":{}", seq + 1)),
        "{library}"
    );
    post_json(
        &router,
        "/api/ops",
        &json!({ "ops": [notes_op("op_0000000000ff", &id, "Hi", 0)] }),
    )
    .await;
    assert!(
        next_frame(&mut stream)
            .await
            .starts_with("event: library\n")
    );

    // A cook session announces itself and each change.
    let session = h.ok("recipes_cook_start", json!({ "recipeId": id })).await["sessionId"]
        .as_str()
        .unwrap()
        .to_owned();
    let started = next_frame(&mut stream).await;
    assert!(started.starts_with("event: session\n"), "{started}");
    assert!(
        started.contains(&format!("data: {{\"id\":\"{session}\",\"seq\":0}}")),
        "{started}"
    );
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": session, "events": [{ "type": "check", "target": "dredge" }] }),
    )
    .await;
    let changed = next_frame(&mut stream).await;
    assert!(
        changed.contains(&format!("data: {{\"id\":\"{session}\",\"seq\":1}}")),
        "{changed}"
    );
    // Finishing changes the session and the library.
    h.ok(
        "recipes_cook_update",
        json!({ "sessionId": session, "events": [{ "type": "finish" }] }),
    )
    .await;
    assert!(
        next_frame(&mut stream)
            .await
            .starts_with("event: session\n")
    );
    assert!(
        next_frame(&mut stream)
            .await
            .starts_with("event: library\n")
    );

    // A dry run announces nothing; the next real change is the next frame.
    h.ok(
        "recipes_organize",
        json!({ "ids": [id], "state": "archived", "dryRun": true }),
    )
    .await;
    h.ok(
        "recipes_organize",
        json!({ "ids": [id], "state": "archived" }),
    )
    .await;
    let after = next_frame(&mut stream).await;
    assert!(after.starts_with("event: library\n"), "{after}");
    drop(stream);
    assert_eq!(h.recipes.hub().clients(), 0);
}
