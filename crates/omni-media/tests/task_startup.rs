//! Recommendation task startup, plus the disabled-task and manual-input
//! policies.
#![allow(clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use omni_config::Config;
use omni_media::task::decode_manual_input;
use omni_testkit::test_app_env;
use serde_json::json;

fn configured_env() -> std::collections::BTreeMap<String, String> {
    let mut env = test_app_env();
    for (key, value) in [
        ("RECS_SCHEDULE", "0 0 17 * * 1,3,5"),
        ("TASTE_REFLECTION_SCHEDULE", "0 0 4 * * 0"),
        ("TMDB_API_KEY", "tmdb"),
        ("TAVILY_API_KEY", "tavily"),
        ("OPENAI_API_KEY", "openai"),
        ("PLEX_URL", "http://plex.test"),
        ("PLEX_TOKEN", "plex"),
        ("RADARR_URL", "http://radarr.test"),
        ("RADARR_API_KEY", "radarr"),
        ("RADARR_ROOT_FOLDER_PATH", "/movies"),
        ("RADARR_QUALITY_PROFILE_ID", "1"),
        ("SONARR_URL", "http://sonarr.test"),
        ("SONARR_API_KEY", "sonarr"),
        ("SONARR_ROOT_FOLDER_PATH", "/series"),
        ("SONARR_QUALITY_PROFILE_ID", "1"),
    ] {
        env.insert(key.to_owned(), value.to_owned());
    }
    env
}

#[tokio::test]
async fn does_not_run_llm_backed_tasks_on_service_startup() {
    let mut h = common::Harness::new().await;
    h.services.config = Arc::new(Config::from_env(&configured_env()).expect("config"));
    let tasks = omni_media::tasks(&h.services).expect("tasks");
    let names: Vec<&str> = tasks.iter().map(|t| t.name()).collect();
    assert_eq!(names, vec!["Recommendations", "TasteReflection"]);
    assert!(tasks.iter().all(|t| !t.options().run_on_startup));
    assert_eq!(tasks[0].display_name(), Some("Media Recommendations"));
    assert_eq!(tasks[1].display_name(), Some("Media Taste Reflection"));
    assert!(tasks[0].accepts_manual_input());
    assert!(!tasks[1].accepts_manual_input());
}

#[tokio::test]
async fn disables_tasks_with_missing_settings() {
    let logs = omni_testkit::capture_logs();
    let h = common::Harness::new().await;
    let tasks = omni_media::tasks(&h.services).expect("tasks");
    assert!(tasks.is_empty());
    let events = logs.events();
    assert!(events.iter().any(|e| e.message.starts_with(
        "Recommendations disabled: missing TMDB_API_KEY, TAVILY_API_KEY, OPENAI_API_KEY, PLEX_URL"
    )));
    assert!(events
        .iter()
        .any(|e| e.message == "Taste reflection disabled: missing TMDB_API_KEY, PLEX_URL, PLEX_TOKEN, OPENAI model credential"));
}

#[test]
fn validates_manual_run_input() {
    assert_eq!(
        decode_manual_input(&json!({"maxRecommendations": 3})).ok(),
        Some(3.0)
    );
    for bad in [
        json!({"maxRecommendations": 0}),
        json!({"maxRecommendations": 11}),
        json!({"maxRecommendations": 1.5}),
        json!({"maxRecommendations": "2"}),
        json!({}),
        json!(null),
    ] {
        let error = decode_manual_input(&bad).expect_err("invalid");
        assert_eq!(
            error.to_string(),
            "maxRecommendations must be an integer from 1 to 10"
        );
    }
}

#[tokio::test]
async fn runs_the_recommendation_task_and_keeps_its_summary() {
    let mut h = common::Harness::new().await;
    h.services.config = Arc::new(Config::from_env(&configured_env()).expect("config"));
    *h.library.history.lock().expect("lock") =
        omni_media::types::FetchResult::unavailable("Plex offline");
    let tasks = omni_media::tasks(&h.services).expect("tasks");
    let task = tasks
        .iter()
        .find(|t| t.name() == "Recommendations")
        .expect("task");
    let cx = omni_tasks::RunContext {
        run_id: "Recommendations:test".to_owned(),
        task_name: "Recommendations".to_owned(),
        trigger: omni_tasks::Trigger::Manual,
        scheduled_for: None,
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    let invalid = task
        .run_manual(&cx, json!({"maxRecommendations": 0}))
        .await
        .expect_err("invalid input");
    assert!(
        invalid
            .to_string()
            .contains("maxRecommendations must be an integer from 1 to 10")
    );
    assert_eq!(*h.library.calls.lock().expect("lock"), 0);
    task.run_manual(&cx, json!({"maxRecommendations": 2}))
        .await
        .expect("run");
    assert_eq!(
        task.last_run_summary().as_deref(),
        Some("skipped: Plex offline")
    );
}
