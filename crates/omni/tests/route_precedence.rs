//! The assembled application router: concrete `/pods/*` routes win over the
//! SPA's `/pods` and `/pods/:id`, unknown `/api/*` paths are JSON 404s, the
//! `/reminders` page keeps the headers omni-reminders sets, asset caching,
//! and the `/api/*` same-origin mutation guard.

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
