//! Subsystem wiring: what PressPods contributes with and without its
//! configuration.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use omni_config::Config;
use omni_testkit::{TestApp, test_app_env};

fn with_env(app: &TestApp, extra: &[(&str, &str)]) -> omni_runtime::AppContext {
    let mut env = test_app_env();
    for (key, value) in extra {
        env.insert((*key).to_owned(), (*value).to_owned());
    }
    let mut ctx = app.ctx.clone();
    ctx.config = Arc::new(Config::from_env(&env).unwrap());
    ctx
}

#[tokio::test]
async fn without_a_token_only_tools_entities_and_data_rows_are_registered() {
    let app = TestApp::new().await;
    let subsystem = omni_presspods::subsystem(&app.ctx).unwrap();
    assert_eq!(subsystem.name, "PressPods");
    assert!(subsystem.tasks.is_empty());
    assert_eq!(subsystem.mcp_tools.len(), 6);
    let names: Vec<&str> = subsystem.entities.iter().map(|e| e.name).collect();
    assert_eq!(
        names,
        [
            "press-pods-episode",
            "press-pods-job",
            "press-pods-job-attempt"
        ]
    );
    let slugs: Vec<&str> = subsystem.managed_entities.iter().map(|e| e.slug).collect();
    assert_eq!(slugs, ["press-pods-episode", "press-pods-job"]);
    assert_eq!(subsystem.managed_entities[0].primary_key, ["episodeId"]);
}

#[tokio::test]
async fn a_token_without_worker_credentials_mounts_routes_but_no_task() {
    let app = TestApp::new().await;
    let ctx = with_env(&app, &[("PRESSPODS_AUTH_TOKEN", "token")]);
    let subsystem = omni_presspods::subsystem(&ctx).unwrap();
    assert!(subsystem.tasks.is_empty());
    let (status, _) = app
        .get_json(&subsystem.router, "/api/press-pods/episodes")
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn complete_configuration_registers_the_queue_worker() {
    let app = TestApp::new().await;
    let ctx = with_env(
        &app,
        &[
            ("PRESSPODS_AUTH_TOKEN", "token"),
            ("PRESSPODS_TTS_URL", "http://127.0.0.1:9"),
            ("OPENAI_API_KEY", "sk-test"),
        ],
    );
    let subsystem = omni_presspods::subsystem(&ctx).unwrap();
    assert_eq!(subsystem.tasks.len(), 1);
    let task = &subsystem.tasks[0];
    assert_eq!(task.name(), "PressPods");
    assert_eq!(task.schedule().as_str(), "0 */5 * * * *");
    assert!(task.options().run_on_startup);
    assert!(task.options().jitter.is_zero());
}
