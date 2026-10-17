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
