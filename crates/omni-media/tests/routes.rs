//! `/api/recommendations*` routes and the
//! on-deck port.
#![allow(clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use omni_api::media::{RecommendationFeedback, RecommendationStatus, WatchlistResult};
use omni_media::persistence::{RecommendationData, ShortlistScoresData};
use omni_media::types::{CandidateSource, MediaType};
use omni_media::{MediaOnDeck, subsystem_with};
use omni_runtime::ports::OnDeckSource as _;
use omni_store::entity::{EntityWrite as _, UpsertOpts};
use serde_json::json;

fn row(id: &str, tmdb_id: i64, update: impl FnOnce(&mut RecommendationData)) -> RecommendationData {
    let mut rec = RecommendationData {
        recommendation_id: id.to_owned(),
        canonical_id: format!("tmdb:movie:{tmdb_id}"),
        tmdb_id,
        media_type: MediaType::Movie,
        title: format!("Movie {tmdb_id}"),
        status: RecommendationStatus::Notified,
        run_date: "2026-01-01".to_owned(),
        recommended_at: 1_767_000_000_000 + tmdb_id,
        ..RecommendationData::default()
    };
    update(&mut rec);
    rec
}

async fn seed(h: &common::Harness, rows: Vec<RecommendationData>) {
    h.services
        .store
        .write(move |tx| {
            for row in &rows {
                tx.upsert(row, UpsertOpts::default())?;
            }
            Ok::<_, omni_store::StoreError>(())
        })
        .await
        .expect("seed");
}

#[tokio::test]
async fn lists_recommendations_newest_first_with_links_and_js_numbers() {
    let h = common::Harness::new().await;
    seed(
        &h,
        vec![
            row("old", 1, |r| {
                r.watchlist_result = Some(WatchlistResult::Added)
            }),
            row("new", 2, |r| {
                r.media_type = MediaType::Tv;
                r.canonical_id = "tmdb:tv:2".to_owned();
                r.title = "Show & Tell".to_owned();
                r.manager_slug = Some("show-and-tell".to_owned());
                r.runtime_minutes = Some(52.0);
                r.source = Some(CandidateSource::Similar);
                r.shortlist_scores = Some(ShortlistScoresData {
                    taste_match: 80.0,
                    novelty: 60.0,
                    effort_fit: 90.0,
                    composite: 73.15,
                    risks: vec![],
                    extra: Default::default(),
                });
            }),
        ],
    )
    .await;
    let subsystem = subsystem_with(&h.app.ctx, h.services.clone()).expect("subsystem");
    let router = h.app.router(&subsystem);
    let (status, body) = h.app.get_json(&router, "/api/recommendations").await;
    assert_eq!(status, StatusCode::OK);
    let list = body["recommendations"].as_array().expect("list");
    assert_eq!(list[0]["recommendationId"], json!("new"));
    assert_eq!(list[0]["tmdbId"], json!(2));
    assert_eq!(list[0]["runtimeMinutes"], json!(52));
    assert_eq!(list[0]["source"], json!("similar"));
    assert_eq!(list[0]["shortlistScores"]["composite"], json!(73.15));
    assert_eq!(
        list[0]["links"],
        json!({
            "tmdb": "https://www.themoviedb.org/tv/2",
            "plex": "http://plex.boris/web/index.html#!/search?pivot=top&query=Show%20%26%20Tell",
            "manager": "http://sonarr.boris/series/show-and-tell"
        })
    );
    assert_eq!(
        list[1]["links"]["manager"],
        json!("http://radarr.boris/movie/1")
    );
    assert_eq!(list[1]["year"], json!(null));
    assert_eq!(list[1]["caveats"], json!([]));
    assert_eq!(list[1]["shortlistScores"], json!(null));

    let (status, body) = h.app.get_json(&router, "/api/recommendations/old").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["recommendation"]["recommendationId"], json!("old"));
    let (status, body) = h
        .app
        .get_json(&router, "/api/recommendations/missing")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "Recommendation not found"}));
    // `GET /run` and `POST /taste-profile/feedback` reach the `:id` routes.
    let (status, body) = h.app.get_json(&router, "/api/recommendations/run").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "Recommendation not found"}));
    let (status, body) = h
        .app
        .post_json(
            &router,
            "/api/recommendations/taste-profile/feedback",
            &json!({"feedback": "good_pick"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "Recommendation not found"}));
}

#[tokio::test]
async fn links_untracked_movies_to_the_radarr_search() {
    let rec = row("x", 7, |_| {});
    assert_eq!(
        omni_media::routes::build_manager_link(&rec),
        "http://radarr.boris/add/new?term=tmdb%3A7"
    );
}

#[tokio::test]
async fn serves_a_null_taste_profile_before_the_first_reflection() {
    let h = common::Harness::new().await;
    let subsystem = subsystem_with(&h.app.ctx, h.services.clone()).expect("subsystem");
    let router = h.app.router(&subsystem);
    let (status, body) = h
        .app
        .get_json(&router, "/api/recommendations/taste-profile")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"profile": null}));
}

#[tokio::test]
async fn validates_and_records_feedback() {
    let h = common::Harness::new().await;
    seed(
        &h,
        vec![
            row("delivered", 1, |_| {}),
            row("pending", 2, |r| r.status = RecommendationStatus::Pending),
        ],
    )
    .await;
    let subsystem = subsystem_with(&h.app.ctx, h.services.clone()).expect("subsystem");
    let router = h.app.router(&subsystem);
    let post = |path: &'static str, body: serde_json::Value| {
        let router = router.clone();
        let app = &h.app;
        async move { app.post_json(&router, path, &body).await }
    };
    let (status, body) = post(
        "/api/recommendations/delivered/feedback",
        json!({"feedback": "meh"}),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::BAD_REQUEST,
            json!({"error": "Invalid recommendation feedback"})
        )
    );
    let (status, body) = post(
        "/api/recommendations/delivered/feedback",
        json!({"note": "   "}),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::BAD_REQUEST,
            json!({"error": "A rating or a note is required"})
        )
    );
    let (status, _) = post(
        "/api/recommendations/delivered/feedback",
        json!({"note": "x".repeat(1001)}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = post(
        "/api/recommendations/missing/feedback",
        json!({"feedback": "good_pick"}),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::NOT_FOUND,
            json!({"error": "Recommendation not found"})
        )
    );
    let (status, body) = post(
        "/api/recommendations/pending/feedback",
        json!({"feedback": "good_pick"}),
    )
    .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::CONFLICT,
            json!({"error": "Undelivered recommendations cannot be rated"})
        )
    );
    let (status, body) = post(
        "/api/recommendations/delivered/feedback",
        json!({"feedback": "not_for_me", "note": "  too slow  "}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["recommendation"]["feedback"], json!("not_for_me"));
    assert_eq!(body["recommendation"]["feedbackNote"], json!("too slow"));
    assert!(body["recommendation"]["feedbackAt"].is_i64());
    let stored = omni_media::persistence::get_recommendation(&h.services.store, "delivered")
        .await
        .expect("get")
        .expect("row");
    assert_eq!(stored.feedback, Some(RecommendationFeedback::NotForMe));
}

#[tokio::test]
async fn validates_manual_runs_and_reports_a_missing_task() {
    let h = common::Harness::new().await;
    let subsystem = subsystem_with(&h.app.ctx, h.services.clone()).expect("subsystem");
    let router = h.app.router(&subsystem);
    let (status, body) = h
        .app
        .post_json(
            &router,
            "/api/recommendations/run",
            &json!({"maxRecommendations": 0}),
        )
        .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::BAD_REQUEST,
            json!({"error": "maxRecommendations must be an integer from 1 to 10"})
        )
    );
    let (status, body) = h
        .app
        .post_json(
            &router,
            "/api/recommendations/run",
            &json!({"maxRecommendations": 2}),
        )
        .await;
    assert_eq!(
        (status, body),
        (
            StatusCode::NOT_FOUND,
            json!({"error": "Unknown task \"Recommendations\""})
        )
    );
}

#[tokio::test]
async fn starts_a_manual_run_for_a_registered_task() {
    let mut h = common::Harness::new().await;
    let mut env = omni_testkit::test_app_env();
    for (key, value) in [
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
    h.services.config = Arc::new(omni_config::Config::from_env(&env).expect("config"));
    // An unavailable history makes the background run skip immediately.
    *h.library.history.lock().expect("lock") =
        omni_media::types::FetchResult::unavailable("Plex offline");
    let subsystem = subsystem_with(&h.app.ctx, h.services.clone()).expect("subsystem");
    for task in &subsystem.tasks {
        h.app.ctx.tasks.track(task.clone()).expect("track");
    }
    let router = h.app.router(&subsystem);
    let (status, body) = h
        .app
        .post_json(
            &router,
            "/api/recommendations/run",
            &json!({"maxRecommendations": 2}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        body["runId"]
            .as_str()
            .is_some_and(|id| id.starts_with("Recommendations:"))
    );
}

#[tokio::test]
async fn on_deck_lists_the_newest_delivered_open_recommendations() {
    let h = common::Harness::new().await;
    seed(
        &h,
        vec![
            row("a", 1, |_| {}),
            row("b", 2, |r| r.why_for_user = Some("Because".to_owned())),
            row("c", 3, |r| r.status = RecommendationStatus::Pending),
            row("d", 4, |r| {
                r.feedback = Some(RecommendationFeedback::NotForMe)
            }),
            row("e", 5, |r| r.status = RecommendationStatus::Watched),
        ],
    )
    .await;
    let items = MediaOnDeck::new(h.services.store.clone())
        .on_deck()
        .await
        .expect("on deck");
    assert_eq!(
        items,
        vec![
            json!({"recommendationId": "b", "title": "Movie 2", "mediaType": "movie", "year": null,
                   "posterPath": null, "whyForUser": "Because", "recommendedAt": 1_767_000_000_002_i64}),
            json!({"recommendationId": "a", "title": "Movie 1", "mediaType": "movie", "year": null,
                   "posterPath": null, "whyForUser": null, "recommendedAt": 1_767_000_000_001_i64}),
        ]
    );
}

#[tokio::test]
async fn builds_the_production_subsystem_from_the_app_context() {
    let h = common::Harness::new().await;
    assert!(h.app.ctx.ports.on_deck_source().is_none());
    let subsystem = omni_media::subsystem(&h.app.ctx).expect("subsystem");
    assert_eq!(subsystem.name, "media");
    let on_deck = h
        .app
        .ctx
        .ports
        .on_deck_source()
        .expect("on-deck port installed");
    assert_eq!(
        on_deck.on_deck().await.expect("on deck"),
        Vec::<serde_json::Value>::new()
    );
    assert!(
        subsystem.tasks.is_empty(),
        "unconfigured tasks stay disabled"
    );
    assert_eq!(subsystem.mcp_tools.len(), 26);
    assert_eq!(
        subsystem
            .entities
            .iter()
            .map(|e| e.name)
            .collect::<Vec<_>>(),
        vec![
            "recs-recommendation-attempt",
            "recs-identity-alias",
            "recs-taste-evidence",
            "recs-taste-profile",
            "media-season-search",
            "media-movie-search",
            "media-download-removal"
        ]
    );
    assert_eq!(subsystem.managed_entities.len(), 4);
    // Unconfigured Plex reads as unavailable, never as an empty library.
    let services = omni_media::services_from_context(&h.app.ctx);
    assert_eq!(
        services.library.watch_history().await,
        omni_media::types::FetchResult::Unavailable {
            reason: "PLEX_URL is not configured".to_owned()
        }
    );
    assert_eq!(
        services.watchlist.fetch().await,
        omni_media::types::FetchResult::Unavailable {
            reason: "Radarr or Sonarr is unavailable".to_owned()
        }
    );
}
