//! Settings kept by Omni: the environment is copied in once (secrets
//! sealed), stored values win, changes validate the whole configuration and
//! need a trusted browser, and secret values never come back.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use omni::settings::{SettingRow, Settings, read_stored, router};
use omni_store::entity::EntityOps as _;
use omni_testkit::TestApp;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const KEY: &str = "sk-test-openai-0123456789abcdef";

fn env() -> BTreeMap<String, String> {
    let mut env = omni_testkit::test_app_env();
    env.insert("OPENAI_API_KEY".into(), KEY.into());
    env.insert("PLEX_URL".into(), "http://plex.example".into());
    env
}

async fn send(router: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn request(method: &str, path: &str, cookie: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "omni.boris")
        .header("origin", "http://omni.boris")
        .header("content-type", "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

fn setting<'a>(view: &'a Value, key: &str) -> &'a Value {
    view["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == key)
        .unwrap()
}

#[tokio::test]
async fn the_environment_moves_in_once_and_secrets_stay_sealed_and_write_only() {
    let app = TestApp::new().await;
    let settings = Settings::with_environment(&app.ctx, env(), env()).unwrap();
    assert!(settings.import_environment().await.unwrap() > 0);
    assert_eq!(settings.import_environment().await.unwrap(), 0, "once");
    let rows = app
        .ctx
        .store
        .read(|docs| docs.get_all::<SettingRow>())
        .await
        .unwrap();
    let openai = rows.iter().find(|r| r.key == "OPENAI_API_KEY").unwrap();
    assert!(openai.value.is_none());
    assert!(!openai.sealed.as_deref().unwrap().contains(KEY));
    let plex = rows.iter().find(|r| r.key == "PLEX_URL").unwrap();
    assert_eq!(plex.value.as_deref(), Some("http://plex.example"));
    assert!(
        !rows.iter().any(|r| r.key == "DB_NAME"),
        "bootstrap keys stay in the environment"
    );

    let router = router(settings.clone());
    // An untrusted browser sees names and sources, no values.
    let (status, view) = send(&router, request("GET", "/api/settings", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["valuesHidden"], true);
    assert_eq!(setting(&view, "PLEX_URL")["value"], Value::Null);
    assert_eq!(setting(&view, "PLEX_URL")["source"], "stored");
    assert!(!view.to_string().contains(KEY));

    let cookie = app.owner_cookie.clone();
    let (_, view) = send(
        &router,
        request("GET", "/api/settings", Some(&cookie), None),
    )
    .await;
    assert_eq!(setting(&view, "PLEX_URL")["value"], "http://plex.example");
    assert_eq!(setting(&view, "OPENAI_API_KEY")["value"], Value::Null);
    assert_eq!(setting(&view, "OPENAI_API_KEY")["secret"], true);
    assert_eq!(setting(&view, "OPENAI_API_KEY")["set"], true);
    assert_eq!(view["restartNeeded"], false);
    assert!(!view.to_string().contains(KEY));

    // Changes need a trusted browser and validate the whole configuration.
    let change = json!({"value": "http://plex.boris"});
    let (status, _) = send(
        &router,
        request("PUT", "/api/settings/PLEX_URL", None, Some(change.clone())),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, saved) = send(
        &router,
        request("PUT", "/api/settings/PLEX_URL", Some(&cookie), Some(change)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["restartNeeded"], true);
    assert_eq!(saved["environmentDiffers"], true);
    assert_eq!(saved["environmentSet"], true);
    let (status, _) = send(
        &router,
        request(
            "PUT",
            "/api/settings/PLEX_URL",
            Some(&cookie),
            Some(json!({"value": ""})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an empty value is refused");
    let (status, refused) = send(
        &router,
        request(
            "PUT",
            "/api/settings/LOG_LEVEL",
            Some(&cookie),
            Some(json!({"value": "loud"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let (status, _) = send(
        &router,
        request(
            "PUT",
            "/api/settings/DB_NAME",
            Some(&cookie),
            Some(json!({"value": "other.db"})),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "bootstrap keys are not editable"
    );

    // A rotated secret is sealed again; removing a stored value falls back.
    let (status, rotated) = send(
        &router,
        request(
            "PUT",
            "/api/settings/OPENAI_API_KEY",
            Some(&cookie),
            Some(json!({"value": "sk-rotated-0123456789abcdef"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rotated["value"], Value::Null);
    let (status, cleared) = send(
        &router,
        request("DELETE", "/api/settings/PLEX_URL", Some(&cookie), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cleared["source"], "environment");
    // Removed on the page: the environment value is not copied in again.
    assert_eq!(settings.import_environment().await.unwrap(), 0);
}

#[tokio::test]
async fn the_next_boot_reads_stored_values_and_refusals_never_quote_a_secret() {
    let app = TestApp::new().await;
    let settings = Settings::with_environment(&app.ctx, env(), env()).unwrap();
    settings.import_environment().await.unwrap();
    settings.set("PLEX_URL", "http://plex.boris").await.unwrap();
    settings
        .set("OPENAI_API_KEY", "sk-rotated-0123456789abcdef")
        .await
        .unwrap();

    let data_dir = &app.ctx.paths.data_dir;
    let db = data_dir.parent().unwrap().join("docstore.db");
    let stored = read_stored(&db, data_dir).await;
    assert_eq!(stored["PLEX_URL"], "http://plex.boris");
    assert_eq!(stored["OPENAI_API_KEY"], "sk-rotated-0123456789abcdef");
    // Without its key a sealed value is unreadable, and the boot falls back.
    let elsewhere = tempfile::tempdir().unwrap();
    let stored = read_stored(&db, elsewhere.path()).await;
    assert_eq!(stored["PLEX_URL"], "http://plex.boris");
    assert!(!stored.contains_key("OPENAI_API_KEY"));

    let weak = "short-device-token";
    let refused = settings
        .set("OMNI_DEVICE_LINK_TOKEN", weak)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(refused, "OMNI_DEVICE_LINK_TOKEN is not valid");

    // Nothing would start Omni again outside its container.
    let router = router(settings);
    let (status, _) = send(
        &router,
        request(
            "POST",
            "/api/settings/restart",
            Some(&app.owner_cookie),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(!app.ctx.shutdown.is_cancelled());
}
