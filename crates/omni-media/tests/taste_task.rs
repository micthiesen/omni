//! The `TasteReflection` task end to end over fakes:
//! an unavailable history skips the run, a full run resolves completed
//! watches, persists evidence and a profile, and reports its run summaries.
#![allow(clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use omni_ai::{GenerateResponse, ModelRole};
use omni_config::Config;
use omni_media::taste::get_latest_taste_profile;
use omni_media::types::{FetchResult, MediaType, WatchedItem};
use omni_tasks::{RunContext, Task, Trigger};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn configured(h: &mut common::Harness) {
    let mut env = omni_testkit::test_app_env();
    for (key, value) in [
        ("TMDB_API_KEY", "tmdb"),
        ("OPENAI_API_KEY", "openai"),
        ("PLEX_URL", "http://plex.test"),
        ("PLEX_TOKEN", "plex"),
    ] {
        env.insert(key.to_owned(), value.to_owned());
    }
    h.services.config = Arc::new(Config::from_env(&env).expect("config"));
}

fn taste_task(h: &common::Harness) -> Arc<dyn Task> {
    omni_media::tasks(&h.services)
        .expect("tasks")
        .into_iter()
        .find(|task| task.name() == "TasteReflection")
        .expect("taste task enabled")
}

fn context() -> RunContext {
    RunContext {
        run_id: "TasteReflection:test".to_owned(),
        task_name: "TasteReflection".to_owned(),
        trigger: Trigger::Manual,
        scheduled_for: None,
        cancel: CancellationToken::new(),
    }
}

fn raw_profile() -> GenerateResponse {
    let commitment = json!({"preference": "uncertain", "confidence": 0.5, "evidence_ids": []});
    GenerateResponse::text(
        json!({
            "stable_preferences": [], "conditional_preferences": [], "aversions": [],
            "current_saturation": [], "exploration_targets": [], "uncertainties": [],
            "commitment_preferences": {
                "movies": commitment, "limited_series": commitment, "long_series": commitment
            }
        })
        .to_string(),
    )
}

#[tokio::test]
async fn skips_the_reflection_when_plex_history_is_unavailable() {
    let mut h = common::Harness::new().await;
    configured(&mut h);
    *h.library.history.lock().expect("lock") = FetchResult::unavailable("Plex offline");
    let task = taste_task(&h);
    let (result, degraded) = omni_tasks::collect_degraded(task.run(&context())).await;
    result.expect("run");
    assert_eq!(
        task.last_run_summary().as_deref(),
        Some("skipped: Plex offline")
    );
    assert_eq!(degraded, vec!["watch history unavailable: Plex offline"]);
    assert!(h.app.ai.requests().is_empty());
}

#[tokio::test]
async fn creates_a_profile_from_completed_watches_then_reports_unchanged() {
    let mut h = common::Harness::new().await;
    configured(&mut h);
    *h.library.history.lock().expect("lock") = FetchResult::Ok(vec![
        WatchedItem {
            item: common::media("plex://movie/1", "Arrival", MediaType::Movie, Some(1)),
            viewed_at: 1_700_000_000_000,
            view_count: 1,
            completion: Some(1.0),
        },
        // Partial watches are not taste evidence.
        WatchedItem {
            item: common::media("plex://movie/2", "Partial", MediaType::Movie, Some(2)),
            viewed_at: 1_700_000_000_001,
            view_count: 1,
            completion: Some(0.2),
        },
    ]);
    h.app.ai.script(
        ModelRole::TasteReflection,
        vec![raw_profile(), raw_profile()],
    );
    let task = taste_task(&h);

    task.run(&context()).await.expect("first run");
    assert_eq!(
        task.last_run_summary().as_deref(),
        Some("profile v1: 1 evidence items, 3 unsupported claims removed")
    );
    let profile = get_latest_taste_profile(&h.services.store)
        .await
        .expect("profile")
        .expect("created");
    assert_eq!(profile.profile.evidence_count, 1);
    assert_eq!(h.app.ai.requests().len(), 2, "draft and revision");

    task.run(&context()).await.expect("second run");
    assert_eq!(
        task.last_run_summary().as_deref(),
        Some("unchanged: profile v1, no model call")
    );
    assert_eq!(h.app.ai.requests().len(), 2);
}
