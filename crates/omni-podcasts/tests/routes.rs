//! `/api/podcast-recommendations` route behavior: serializers, feedback
//! validation and manual runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use common::rec;
use futures::future::BoxFuture;
use jiff::tz::TimeZone;
use omni_podcasts::persistence::{
    PodcastQueueResult, PodcastRecommendationData, PodcastRecommendationStatus, ShortlistScores,
    insert_podcast_recommendation,
};
use omni_podcasts::routes::{RoutesState, router};
use omni_store::cbor::Extra;
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};
use omni_testkit::TestApp;
use serde_json::{Value, json};

async fn setup() -> (TestApp, axum::Router) {
    let app = TestApp::new().await;
    let routes = router(RoutesState {
        store: app.ctx.store.clone(),
        clock: app.ctx.clock.clone(),
        tasks: app.ctx.tasks.clone(),
    });
    (app, routes)
}

#[tokio::test]
async fn lists_recommendations_newest_first_in_the_wire_shape() {
    let (app, routes) = setup().await;
    insert_podcast_recommendation(&app.ctx.store, rec())
        .await
        .unwrap();
    insert_podcast_recommendation(
        &app.ctx.store,
        PodcastRecommendationData {
            recommendation_id: "r2".into(),
            recommended_at: rec().recommended_at + 1,
            confidence: Some(1.0),
            queue_result: Some(PodcastQueueResult::Queued),
            shortlist_scores: Some(ShortlistScores {
                taste_match: 80.0,
                novelty: 60.5,
                composite: 59.85,
                risks: vec!["promo".into()],
                extra: Extra::default(),
            }),
            ..rec()
        },
    )
    .await
    .unwrap();
    let (status, body) = app.get_json(&routes, "/api/podcast-recommendations").await;
    assert_eq!(status, StatusCode::OK);
    let list = body["recommendations"].as_array().unwrap();
    assert_eq!(list[0]["recommendationId"], "r2");
    assert_eq!(list[0]["confidence"], json!(1));
    assert_eq!(list[0]["queueResult"], "queued");
    assert_eq!(list[0]["shortlistScores"]["composite"], json!(59.85));
    let first_keys: Vec<&str> = list[1]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        first_keys,
        vec![
            "recommendationId",
            "showTitle",
            "episodeTitle",
            "feedUrl",
            "itunesId",
            "artworkUrl",
            "episodeUrl",
            "publishedAt",
            "durationMinutes",
            "status",
            "whyForUser",
            "caveats",
            "confidence",
            "shortlistScores",
            "discoveredVia",
            "sourceUrl",
            "matchedVoices",
            "recommendedAt",
            "notifiedAt",
            "queueResult",
            "feedback",
            "feedbackAt",
            "feedbackNote"
        ]
    );
    assert_eq!(list[1]["itunesId"], Value::Null);
    assert_eq!(list[1]["caveats"], json!([]));
}

#[tokio::test]
async fn returns_null_profile_and_404_for_unknown_ids() {
    let (app, routes) = setup().await;
    let (status, body) = app
        .get_json(&routes, "/api/podcast-recommendations/taste-profile")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "profile": null }));
    let (status, body) = app
        .get_json(&routes, "/api/podcast-recommendations/nope")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({ "error": "Recommendation not found" }));
}

#[tokio::test]
async fn validates_and_records_feedback() {
    let (app, routes) = setup().await;
    insert_podcast_recommendation(&app.ctx.store, rec())
        .await
        .unwrap();
    insert_podcast_recommendation(
        &app.ctx.store,
        PodcastRecommendationData {
            recommendation_id: "pending".into(),
            status: PodcastRecommendationStatus::Pending,
            ..rec()
        },
    )
    .await
    .unwrap();
    let post = |path: &'static str, body: Value| {
        let routes = routes.clone();
        let app = &app;
        async move { app.post_json(&routes, path, &body).await }
    };
    let feedback = "/api/podcast-recommendations/r1/feedback";
    assert_eq!(
        post(feedback, json!({ "feedback": "meh" })).await,
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": "Invalid recommendation feedback" })
        )
    );
    assert_eq!(
        post(feedback, json!({ "note": "x".repeat(1001) })).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(feedback, json!({ "note": "   " })).await,
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": "A rating or a note is required" })
        )
    );
    assert_eq!(
        post(
            "/api/podcast-recommendations/pending/feedback",
            json!({ "feedback": "good_pick" })
        )
        .await,
        (
            StatusCode::CONFLICT,
            json!({ "error": "Undelivered recommendations cannot be rated" })
        )
    );
    assert_eq!(
        post(
            "/api/podcast-recommendations/missing/feedback",
            json!({ "feedback": "good_pick" })
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, body) = post(
        feedback,
        json!({ "feedback": "not_for_me", "note": "  too loud ", "extra": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["recommendation"]["feedback"], "not_for_me");
    assert_eq!(body["recommendation"]["feedbackNote"], "too loud");
    assert!(body["recommendation"]["feedbackAt"].is_i64());
}

struct ManualTask {
    schedule: CronSchedule,
    inputs: Arc<Mutex<Vec<Value>>>,
}

impl Task for ManualTask {
    fn name(&self) -> &str {
        "PodcastRecs"
    }
    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }
    fn options(&self) -> TaskOptions {
        TaskOptions::default()
    }
    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async { Ok(()) })
    }
    fn accepts_manual_input(&self) -> bool {
        true
    }
    fn run_manual<'a>(
        &'a self,
        _cx: &'a RunContext,
        input: Value,
    ) -> BoxFuture<'a, Result<(), TaskError>> {
        self.inputs.lock().unwrap().push(input);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn validates_manual_runs_and_queues_them() {
    let (app, routes) = setup().await;
    let run = "/api/podcast-recommendations/run";
    let range = json!({ "error": "maxRecommendations must be an integer from 1 to 5" });
    assert_eq!(
        app.post_json(&routes, run, &json!({ "maxRecommendations": 6 }))
            .await,
        (StatusCode::BAD_REQUEST, range.clone())
    );
    assert_eq!(
        app.post_json(&routes, run, &json!({ "maxRecommendations": 1.5 }))
            .await,
        (StatusCode::BAD_REQUEST, range)
    );
    assert_eq!(
        app.post_json(&routes, run, &json!({ "maxRecommendations": 2 }))
            .await,
        (
            StatusCode::NOT_FOUND,
            json!({ "error": "Unknown task \"PodcastRecs\"" })
        )
    );
    let inputs = Arc::new(Mutex::new(Vec::new()));
    app.ctx
        .tasks
        .track(Arc::new(ManualTask {
            schedule: CronSchedule::parse("0 0 11 * * 1,3,5", &TimeZone::UTC).unwrap(),
            inputs: inputs.clone(),
        }))
        .unwrap();
    let (status, body) = app
        .post_json(
            &routes,
            run,
            &json!({ "maxRecommendations": 2, "ignored": true }),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body["runId"].as_str().unwrap().starts_with("PodcastRecs:"));
    for _ in 0..100 {
        if !inputs.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        *inputs.lock().unwrap(),
        vec![json!({ "maxRecommendations": 2 })]
    );
}
