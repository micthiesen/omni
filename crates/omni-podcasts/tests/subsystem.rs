//! `omni_podcasts::subsystem` wiring: what the app receives with and without
//! the optional configuration.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use omni_config::Config;
use omni_podcasts::subsystem;
use omni_testkit::{TestApp, test_app_env};

#[tokio::test]
async fn disables_every_task_without_configuration_but_keeps_routes_tools_and_gate() {
    let app = TestApp::new().await;
    let built = subsystem(&app.ctx).unwrap();
    assert_eq!(built.name, "podcasts");
    assert!(built.tasks.is_empty());
    assert_eq!(built.mcp_tools.len(), 10);
    let names: Vec<&str> = built.entities.iter().map(|e| e.name).collect();
    assert_eq!(
        names,
        vec![
            "podcast-recommendation-attempt",
            "podcast-run-state",
            "podcast-taste-evidence",
            "podcast-taste-profile"
        ]
    );
    assert_eq!(built.managed_entities.len(), 3);
    assert_eq!(built.alert_gates.len(), 1);
    assert!(built.alert_gates[0].applies("Error running task \"CastroInboxCleanup\""));
    assert!(!built.alert_gates[0].applies("Error running task \"PodcastRecs\""));
}

#[tokio::test]
async fn registers_the_three_tasks_when_fully_configured() {
    let app = TestApp::new().await;
    let dir = tempfile::tempdir().unwrap();
    let seed = dir.path().join("taste.md");
    std::fs::write(&seed, "# Taste").unwrap();
    let mut env: BTreeMap<String, String> = test_app_env();
    for (key, value) in [
        ("PODCAST_TASTE_PATH", seed.to_str().unwrap()),
        ("TAVILY_API_KEY", "tvly-test"),
        ("OPENAI_API_KEY", "sk-test"),
        ("CASTRO_ACCESS_ID", "550e8400-e29b-41d4-a716-446655440000"),
        ("CASTRO_SECRET_KEY", "secret"),
    ] {
        env.insert(key.into(), value.into());
    }
    let mut ctx = app.ctx.clone();
    ctx.config = Arc::new(Config::from_env(&env).unwrap());
    let built = subsystem(&ctx).unwrap();
    let tasks: Vec<(String, String, Option<String>)> = built
        .tasks
        .iter()
        .map(|t| {
            (
                t.name().to_owned(),
                t.schedule().as_str().to_owned(),
                t.display_name().map(str::to_owned),
            )
        })
        .collect();
    assert_eq!(
        tasks,
        vec![
            (
                "PodcastRecs".into(),
                "0 0 11 * * 1,3,5".into(),
                Some("Podcast Recommendations".into())
            ),
            ("CastroInboxCleanup".into(), "0 */6 * * *".into(), None),
            (
                "PodcastTasteReflection".into(),
                "0 0 5 * * 0".into(),
                Some("Podcast Taste Reflection".into())
            ),
        ]
    );
    assert!(built.tasks[0].accepts_manual_input());
    assert_eq!(built.tasks[1].options().jitter.as_secs(), 300);
}
