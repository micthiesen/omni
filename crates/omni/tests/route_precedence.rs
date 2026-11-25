//! The assembled application router: concrete `/pods/*` routes win over the
//! SPA's `/pods` and `/pods/:id`, unknown `/api/*` paths are JSON 404s, the
//! `/reminders` page keeps the headers omni-reminders sets, asset caching,
//! the `/api/*` same-origin mutation guard, and host apps: an app's host
//! reaches only that app, and no other host reaches it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use omni_runtime::{AppContext, Ports};
use omni_testkit::TestApp;
use tower::ServiceExt as _;

async fn app_with_presspods() -> (TestApp, AppContext, tempfile::TempDir) {
    let app = TestApp::new().await;
    let mut env: BTreeMap<String, String> = omni_testkit::test_app_env();
    env.insert(
        "PRESSPODS_AUTH_TOKEN".to_owned(),
        "pods-token-0123456789".to_owned(),
    );
    let config = omni_config::Config::from_env(&env).unwrap();
    let ctx = AppContext {
        config: Arc::new(config),
        ports: Ports::default(),
        ..app.ctx.clone()
    };
    let dist = tempfile::tempdir().unwrap();
    std::fs::write(
        dist.path().join("index.html"),
        "<!doctype html><title>Omni</title>",
    )
    .unwrap();
    std::fs::create_dir_all(dist.path().join("assets")).unwrap();
    std::fs::write(dist.path().join("assets/app-abc.js"), "console.log(1)").unwrap();
    (app, ctx, dist)
}

async fn send(router: &axum::Router, request: Request<Body>) -> axum::response::Response {
    router.clone().oneshot(request).await.unwrap()
}

fn get(path: &str) -> Request<Body> {
    Request::get(path)
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap()
}

fn header_of(response: &axum::response::Response, name: header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

#[tokio::test]
async fn concrete_routes_win_over_the_spa() {
    let (_app, ctx, dist) = app_with_presspods().await;
    let mut wired = omni::wiring::wire(&ctx, 0).await.unwrap();
    let (router, _) = omni::boot::app_router(&ctx, &mut wired.subsystems, dist.path());

    let spa = send(&router, get("/pods")).await;
    assert_eq!(spa.status(), StatusCode::OK);
    assert!(
        header_of(&spa, header::CONTENT_TYPE)
            .unwrap()
            .starts_with("text/html")
    );
    assert_eq!(
        header_of(&spa, header::CACHE_CONTROL).as_deref(),
        Some("no-cache")
    );
    let deep = send(&router, get("/pods/some-episode")).await;
    assert!(
        header_of(&deep, header::CONTENT_TYPE)
            .unwrap()
            .starts_with("text/html")
    );

    let rss = send(&router, get("/pods/rss")).await;
    let content_type = header_of(&rss, header::CONTENT_TYPE).unwrap_or_default();
    assert!(!content_type.starts_with("text/html"), "{content_type}");

    let asset = send(&router, get("/assets/app-abc.js")).await;
    assert_eq!(asset.status(), StatusCode::OK);
    assert_eq!(
        header_of(&asset, header::CACHE_CONTROL).as_deref(),
        Some("public, max-age=31536000, immutable")
    );

    let unknown = send(&router, get("/api/definitely-not-a-route")).await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(unknown.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"error": "Not found"})
    );
}

#[tokio::test]
async fn reminders_page_keeps_its_own_headers() {
    let (_app, ctx, dist) = app_with_presspods().await;
    let mut wired = omni::wiring::wire(&ctx, 0).await.unwrap();
    let (router, _) = omni::boot::app_router(&ctx, &mut wired.subsystems, dist.path());
    let through_app = send(&router, get("/reminders")).await;
    let direct = send(&omni_reminders::page::page_router(), get("/reminders")).await;
    assert_eq!(through_app.status(), direct.status());
    for name in [
        header::CONTENT_SECURITY_POLICY,
        header::CACHE_CONTROL,
        header::REFERRER_POLICY,
        header::CONTENT_TYPE,
    ] {
        assert_eq!(
            header_of(&through_app, name.clone()),
            header_of(&direct, name.clone()),
            "{name}"
        );
    }
    assert!(header_of(&through_app, header::CONTENT_SECURITY_POLICY).is_some());
}

#[tokio::test]
async fn api_mutations_require_the_same_origin() {
    let (_app, ctx, dist) = app_with_presspods().await;
    let mut wired = omni::wiring::wire(&ctx, 0).await.unwrap();
    let (router, _) = omni::boot::app_router(&ctx, &mut wired.subsystems, dist.path());
    let cross = Request::post("/api/tasks/X/run")
        .header(header::HOST, "omni.boris")
        .header(header::ORIGIN, "https://evil.example")
        .body(Body::empty())
        .unwrap();
    let response = send(&router, cross).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let health = send(&router, get("/api/health")).await;
    assert_eq!(
        header_of(&health, header::X_CONTENT_TYPE_OPTIONS).as_deref(),
        Some("nosniff")
    );
}

fn get_from(host: &str, path: &str) -> Request<Body> {
    Request::get(path)
        .header(header::HOST, host)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn the_public_gateway_host_reaches_only_the_gateways_public_paths() {
    let (_app, ctx, dist) = app_with_presspods().await;
    let mut env = omni_testkit::test_app_env();
    env.insert(
        "PRESSPODS_AUTH_TOKEN".to_owned(),
        "pods-token-0123456789".to_owned(),
    );
    env.insert(
        "OMNI_GATEWAY_PUBLIC_URL".to_owned(),
        "https://gateway.example.com".to_owned(),
    );
    let ctx = AppContext {
        config: Arc::new(omni_config::Config::from_env(&env).unwrap()),
        ..ctx
    };
    let mut wired = omni::wiring::wire(&ctx, 0).await.unwrap();
    let (router, _) = omni::boot::app_router(&ctx, &mut wired.subsystems, dist.path());
    let public = "gateway.example.com";

    let mcp = send(
        &router,
        Request::post("/gateway/mcp")
            .header(header::HOST, public)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(mcp.status(), StatusCode::UNAUTHORIZED);
    for path in [
        "/.well-known/oauth-authorization-server",
        "/.well-known/oauth-protected-resource/gateway/mcp",
    ] {
        let metadata = send(&router, get_from(public, path)).await;
        assert_eq!(metadata.status(), StatusCode::OK, "{path}");
        assert!(
            header_of(&metadata, header::CONTENT_TYPE)
                .unwrap()
                .starts_with("application/json"),
            "{path}"
        );
    }

    // The management page and API, the SPA fallback, other subsystems and
    // every variant of an allowed path are not served on the public host.
    for path in [
        "/gateway",
        "/gateway/",
        "/gateway//",
        "/gateway/mcp/",
        "//gateway/mcp",
        "/gateway//mcp",
        "/gateway/./mcp",
        "/x/../gateway/mcp",
        "/gateway/%6Dcp",
        "/GATEWAY/mcp",
        "/api/gateway",
        "/api/gateway/integrations/..%2F..%2F..%2Fgateway%2Fmcp",
        "/api/health",
        "/mcp",
        "/pods",
        "/",
        "/index.html",
        "/assets/app-abc.js",
        "/.well-known/oauth-foo",
        "/.well-known/oauth-authorization-server/",
    ] {
        let response = send(&router, get_from(public, path)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        let content_type = header_of(&response, header::CONTENT_TYPE).unwrap_or_default();
        assert!(
            !content_type.starts_with("text/html"),
            "{path}: {content_type}"
        );
    }
    let named = send(&router, get_from("Gateway.Example.com.:443", "/gateway")).await;
    assert_eq!(named.status(), StatusCode::NOT_FOUND);

    // The home network is unaffected.
    let page = send(&router, get_from("omni.boris", "/gateway")).await;
    assert_eq!(page.status(), StatusCode::OK);
    let api = send(&router, get_from("omni.boris", "/api/gateway")).await;
    assert_eq!(api.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_public_pods_host_reaches_only_the_feed_audio_and_artwork() {
    let (_app, ctx, dist) = app_with_presspods().await;
    let mut env = omni_testkit::test_app_env();
    env.insert(
        "PRESSPODS_AUTH_TOKEN".to_owned(),
        "pods-token-0123456789".to_owned(),
    );
    env.insert(
        "PRESSPODS_PUBLIC_URL".to_owned(),
        "https://pods.example.com".to_owned(),
    );
    let ctx = AppContext {
        config: Arc::new(omni_config::Config::from_env(&env).unwrap()),
        ..ctx
    };
    let mut wired = omni::wiring::wire(&ctx, 0).await.unwrap();
    let (router, _) = omni::boot::app_router(&ctx, &mut wired.subsystems, dist.path());
    let public = "pods.example.com";

    let feed = send(&router, get_from(public, "/pods/rss")).await;
    assert_eq!(
        feed.status(),
        StatusCode::UNAUTHORIZED,
        "the feed still needs its token"
    );
    let authed = send(
        &router,
        get_from(public, "/pods/rss?authToken=pods-token-0123456789"),
    )
    .await;
    assert_eq!(authed.status(), StatusCode::OK);
    let audio = send(&router, get_from(public, "/pods/audio/missing.mp3")).await;
    assert_eq!(audio.status(), StatusCode::NOT_FOUND);

    for path in [
        "/pods",
        "/pods/",
        "/pods/some-episode",
        "/x/../pods/",
        "/api/gateway/integrations/..%2F..%2F..%2Fpods%2Fx",
        "/api/press-pods/episodes",
        "/api/health",
        "/",
        "/index.html",
        "/assets/app-abc.js",
        "/gateway",
    ] {
        let response = send(&router, get_from(public, path)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        let content_type = header_of(&response, header::CONTENT_TYPE).unwrap_or_default();
        assert!(
            !content_type.starts_with("text/html"),
            "{path}: {content_type}"
        );
    }
    let submit = send(
        &router,
        Request::post("/pods/episodes")
            .header(header::HOST, public)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        submit.status(),
        StatusCode::NOT_FOUND,
        "public submission is closed"
    );

    // The home network is unaffected.
    let page = send(&router, get_from("omni.boris", "/pods")).await;
    assert_eq!(page.status(), StatusCode::OK);
}

const RECIPES_ORIGIN: &str = "https://recipes.example.com";
const RECIPES_HOST: &str = "recipes.example.com";

/// Omni wired with the recipe app on its own host: the real Fond router of
/// `omni-recipes`, served from a built app directory beside Omni's own.
async fn router_with_recipes_app() -> (TestApp, axum::Router, tempfile::TempDir) {
    let (app, ctx, dist) = app_with_presspods().await;
    let mut env = omni_testkit::test_app_env();
    for (key, value) in [
        ("PRESSPODS_AUTH_TOKEN", "pods-token-0123456789"),
        ("OMNI_GATEWAY_PUBLIC_URL", "https://gateway.example.com"),
        ("PRESSPODS_PUBLIC_URL", "https://pods.example.com"),
        ("RECIPES_APP_URL", RECIPES_ORIGIN),
    ] {
        env.insert(key.to_owned(), value.to_owned());
    }
    let fond = dist.path().join("fond");
    std::fs::create_dir_all(&fond).unwrap();
    std::fs::write(
        fond.join("index.html"),
        "<!doctype html><title>Fond</title>",
    )
    .unwrap();
    std::fs::write(fond.join("sw.js"), "// fond worker").unwrap();
    let ctx = AppContext {
        config: Arc::new(omni_config::Config::from_env(&env).unwrap()),
        paths: omni_runtime::AppPaths {
            recipes_web_dist: fond,
            ..ctx.paths.clone()
        },
        ..ctx
    };
    let mut wired = omni::wiring::wire(&ctx, 0).await.unwrap();
    // The app is a host app of the recipes subsystem and nothing else.
    let hosts: Vec<(&str, usize)> = wired
        .subsystems
        .iter()
        .filter(|subsystem| !subsystem.host_apps.is_empty())
        .map(|subsystem| (subsystem.name, subsystem.host_apps.len()))
        .collect();
    assert_eq!(hosts, [("recipes", 1)]);
    let (router, _) = omni::boot::app_router(&ctx, &mut wired.subsystems, dist.path());
    (app, router, dist)
}

async fn text(response: axum::response::Response) -> String {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

#[tokio::test]
async fn the_recipes_host_reaches_only_the_recipe_app() {
    let (_app, router, _dist) = router_with_recipes_app().await;

    for host in [
        RECIPES_HOST,
        "recipes.example.com:8080",
        "RECIPES.EXAMPLE.COM.",
    ] {
        for (path, body) in [
            ("/", "<title>Fond</title>"),
            ("/inbox", "<title>Fond</title>"),
            ("/sw.js", "// fond worker"),
            ("/api/sync", "\"liveCount\":0"),
        ] {
            let response = send(&router, get_from(host, path)).await;
            assert_eq!(response.status(), StatusCode::OK, "{host}{path}");
            let found = text(response).await;
            assert!(found.contains(body), "{host}{path}: {found}");
        }
    }

    // Nothing of Omni exists there: not the gateway, the data API, the feed,
    // the Reminders page, the UI or its assets.
    for path in [
        "/gateway/mcp",
        "/gateway",
        "/api/data/entities",
        "/api/health",
        "/pods/rss",
        "/pods/rss?authToken=pods-token-0123456789",
        "/reminders",
        "/index.html",
        "/assets/app-abc.js",
        "/.well-known/oauth-authorization-server",
    ] {
        let response = send(&router, get_from(RECIPES_HOST, path)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        let content_type = header_of(&response, header::CONTENT_TYPE).unwrap_or_default();
        assert!(
            !content_type.starts_with("text/html"),
            "{path}: {content_type}"
        );
        let body = text(response).await;
        if path.starts_with("/api/") {
            assert_eq!(body, r#"{"error":"Not found"}"#, "{path}");
        } else {
            assert_eq!(body, "Not found", "{path}");
        }
    }
    let mcp = send(
        &router,
        Request::post("/gateway/mcp")
            .header(header::HOST, RECIPES_HOST)
            .header(header::ORIGIN, RECIPES_ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(mcp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn no_other_host_reaches_the_recipe_app() {
    let (_app, router, _dist) = router_with_recipes_app().await;

    // The public hosts answer their JSON 404 for the app's paths.
    for host in ["gateway.example.com", "pods.example.com"] {
        for path in [
            "/",
            "/sw.js",
            "/api/sync",
            "/img/0123456789abcdef/thumb.jpg",
        ] {
            let response = send(&router, get_from(host, path)).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{host}{path}");
            assert_eq!(
                text(response).await,
                r#"{"error":"Not found"}"#,
                "{host}{path}"
            );
        }
    }

    // A name Omni does not answer to (DNS rebinding) reaches nothing.
    for path in ["/", "/api/sync", "/api/gateway"] {
        let response = send(&router, get_from("rebound.example.net", path)).await;
        assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST, "{path}");
    }

    // The home network gets Omni's own answers: its JSON 404 and its SPA.
    for host in [
        "omni.boris",
        "omni.syas.ca",
        "10.10.1.100:8080",
        "localhost",
    ] {
        let api = send(&router, get_from(host, "/api/sync")).await;
        assert_eq!(api.status(), StatusCode::NOT_FOUND, "{host}");
        assert_eq!(text(api).await, r#"{"error":"Not found"}"#, "{host}");
        for path in ["/", "/sw.js", "/inbox", "/r/rcp_7h2k9m4x"] {
            let page = send(&router, get_from(host, path)).await;
            let body = text(page).await;
            assert!(
                !body.to_lowercase().contains("fond"),
                "{host}{path}: {body}"
            );
        }
        let root = send(&router, get_from(host, "/")).await;
        assert!(text(root).await.contains("<title>Omni</title>"), "{host}");
        let write = send(
            &router,
            Request::post("/api/ops")
                .header(header::HOST, host)
                .header(header::ORIGIN, RECIPES_ORIGIN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"ops":[]}"#))
                .unwrap(),
        )
        .await;
        assert_ne!(write.status(), StatusCode::OK, "{host}");
    }
    // A request with no host at all is Omni's.
    let bare = send(&router, Request::get("/").body(Body::empty()).unwrap()).await;
    assert!(text(bare).await.contains("<title>Omni</title>"));
}

#[tokio::test]
async fn the_recipe_app_closes_when_reached_through_the_public_edge() {
    let (_app, router, _dist) = router_with_recipes_app().await;
    // The app's DNS record is private. A request for its host that carries
    // Cloudflare's headers means that protection is gone.
    for (header, path) in [
        ("cf-connecting-ip", "/"),
        ("cf-ray", "/api/sync"),
        ("cf-ray", "/sw.js"),
        ("cf-connecting-ip", "/img/0123456789abcdef/thumb.jpg"),
    ] {
        let response = send(
            &router,
            Request::get(path)
                .header(header::HOST, RECIPES_HOST)
                .header(header, "203.0.113.9")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{header} {path}");
        assert_eq!(
            text(response).await,
            r#"{"error":"This app is not available from the public internet"}"#
        );
    }
    let write = send(
        &router,
        Request::post("/api/ops")
            .header(header::HOST, RECIPES_HOST)
            .header(header::ORIGIN, RECIPES_ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .header("cf-ray", "8a1b2c3d4e5f-YVR")
            .body(Body::from(r#"{"ops":[]}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(write.status(), StatusCode::FORBIDDEN);
    // The public gateway host, which does come through the edge, still works.
    let gateway = send(
        &router,
        Request::get("/.well-known/oauth-authorization-server")
            .header(header::HOST, "gateway.example.com")
            .header("cf-ray", "8a1b2c3d4e5f-YVR")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_ne!(gateway.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_recipe_app_host_cannot_be_confused_with_another() {
    let (_app, router, _dist) = router_with_recipes_app().await;
    // HEAD reads like GET; OPTIONS is never answered as a CORS preflight.
    let head = send(
        &router,
        Request::head("/api/sync")
            .header(header::HOST, RECIPES_HOST)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);
    for path in ["/api/ops", "/api/inbox", "/"] {
        let options = send(
            &router,
            Request::options(path)
                .header(header::HOST, RECIPES_HOST)
                .header(header::ORIGIN, "https://evil.example")
                .header("access-control-request-method", "POST")
                .header("access-control-request-headers", "x-fond-upload")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(options.status(), StatusCode::FORBIDDEN, "{path}");
        assert!(header_of(&options, header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
        assert!(header_of(&options, header::ACCESS_CONTROL_ALLOW_HEADERS).is_none());
    }
    // An absolute-form URI that disagrees with Host, and two Host headers,
    // are refused whenever either names the app: no proxy and Omni can
    // disagree about where such a request goes.
    let conflicting = [
        Request::get("http://recipes.example.com/api/sync")
            .header(header::HOST, "gateway.example.com")
            .body(Body::empty())
            .unwrap(),
        Request::get("http://gateway.example.com/gateway/mcp")
            .header(header::HOST, RECIPES_HOST)
            .body(Body::empty())
            .unwrap(),
        Request::get("/api/sync")
            .header(header::HOST, "omni.boris")
            .header(header::HOST, RECIPES_HOST)
            .body(Body::empty())
            .unwrap(),
        Request::get("/api/sync")
            .header(header::HOST, RECIPES_HOST)
            .header(header::HOST, "gateway.example.com")
            .body(Body::empty())
            .unwrap(),
    ];
    for request in conflicting {
        let response = send(&router, request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(text(response).await, r#"{"error":"Conflicting hosts"}"#);
    }
}

#[tokio::test]
async fn recipe_app_mutations_need_the_app_origin() {
    let (_app, router, _dist) = router_with_recipes_app().await;
    let post = |origin: Option<&str>| {
        let mut request = Request::post("/api/ops")
            .header(header::HOST, RECIPES_HOST)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        request.body(Body::from(r#"{"ops":[]}"#)).unwrap()
    };

    let own = send(&router, post(Some(RECIPES_ORIGIN))).await;
    assert_eq!(own.status(), StatusCode::OK);
    assert_eq!(text(own).await, r#"{"seq":0,"results":[]}"#);

    for origin in [
        None,
        Some("https://evil.example"),
        Some("https://gateway.example.com"),
        Some("http://recipes.example.com"),
        Some("null"),
    ] {
        let response = send(&router, post(origin)).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin:?}");
        assert_eq!(
            text(response).await,
            r#"{"error":"Cross-origin requests are not allowed"}"#,
            "{origin:?}"
        );
    }
}
