//! The recommendation pipeline over a temp store with in-process fakes for
//! Plex, Arr, TMDB, Tavily and Pushover and scripted models, so the assertions
//! observe persisted rows, recorded pushes and model prompts. A notifier hook
//! that leaves the row undecodable makes the final patch fail after
//! notification.
#![allow(clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use omni_ai::{GenerateResponse, ModelRole};
use omni_api::media::{RecommendationStatus, WatchlistResult};
use omni_media::error::RecommendationError;
use omni_media::persistence::{NotificationState, RecommendationData, get_all_recommendations};
use omni_media::pipeline::{PipelineOptions, run_recommendation_pipeline};
use omni_media::tmdb::types::TmdbTitle;
use omni_media::types::{
    AddToWatchlistResult, FetchResult, InProgressItem, MediaType, WatchedItem, WatchlistAddOutcome,
};
use omni_store::cbor::JsValue;
use omni_store::entity::{EntityWrite as _, UpsertOpts};
use omni_store::{DocMeta, DocOps as _, DocWrite as _};
use serde_json::json;

fn title(tmdb_id: i64, name: &str) -> TmdbTitle {
    TmdbTitle {
        title: name.to_owned(),
        vote_average: 8.0,
        vote_count: 1000.0,
        popularity: 10.0,
        ..common::tmdb_title(tmdb_id, MediaType::Movie)
    }
}

async fn harness(trending: Vec<TmdbTitle>) -> common::Harness {
    let h = common::Harness::new().await;
    *h.library.history.lock().expect("lock") = FetchResult::Ok(vec![WatchedItem {
        item: common::media("watched", "Watched", MediaType::Movie, Some(1)),
        viewed_at: 100,
        view_count: 1,
        completion: Some(1.0),
    }]);
    *h.library.library.lock().expect("lock") = FetchResult::Ok(vec![common::media(
        "library",
        "Library",
        MediaType::Movie,
        Some(2),
    )]);
    *h.watchlist.items.lock().expect("lock") = FetchResult::Ok(vec![common::media(
        "tracked",
        "Tracked",
        MediaType::Movie,
        Some(3),
    )]);
    *h.catalog.trending.lock().expect("lock") = trending;
    h
}

fn scores(entries: &[(&str, f64)]) -> GenerateResponse {
    let scores: Vec<serde_json::Value> = entries
        .iter()
        .map(|(id, taste)| {
            json!({"candidate_id": id, "taste_match": taste, "novelty": 60, "effort_fit": 90,
                   "confidence": 0.9, "risks": []})
        })
        .collect();
    GenerateResponse::text(json!({ "scores": scores }).to_string())
}

fn select(id: &str) -> GenerateResponse {
    GenerateResponse::text(
        json!({
            "decision": "select",
            "selected": {"candidate_id": id, "why_for_user": "A fit", "caveats": [], "confidence": 0.9,
                         "notification": {"title": "Candidate", "message": "A fit"}},
            "backup": null,
            "no_add_reason": null
        })
        .to_string(),
    )
}

fn no_add(reason: &str) -> GenerateResponse {
    GenerateResponse::text(
        json!({"decision": "no_add", "selected": null, "backup": null, "no_add_reason": reason})
            .to_string(),
    )
}

async fn run(h: &common::Harness, options: PipelineOptions) -> Result<String, RecommendationError> {
    run_recommendation_pipeline(&h.services, None, options).await
}

fn prompts(h: &common::Harness, role: ModelRole) -> Vec<String> {
    h.app
        .ai
        .requests()
        .iter()
        .filter(|(r, _)| *r == Some(role))
        .map(|(_, req)| common::prompt_text(req))
        .collect()
}

async fn recs(h: &common::Harness) -> Vec<RecommendationData> {
    get_all_recommendations(&h.services.store)
        .await
        .expect("recs")
}

#[tokio::test]
async fn keeps_plex_availability_and_arr_tracked_state_in_their_correct_roles() {
    let h = harness(vec![
        title(2, "Library"),
        title(3, "Tracked"),
        title(4, "Candidate"),
    ])
    .await;
    h.app.ai.script(
        ModelRole::RecsShortlist,
        vec![scores(&[("tmdb:movie:2", 80.0), ("tmdb:movie:4", 70.0)])],
    );
    h.app
        .ai
        .script(ModelRole::RecsSelection, vec![no_add("not today")]);
    let summary = run(&h, PipelineOptions::default()).await.expect("run");
    assert_eq!(summary, "no_add: not today");
    let shortlist = prompts(&h, ModelRole::RecsShortlist).join("\n");
    assert!(shortlist.contains("[tmdb:movie:2] Library [movie]"));
    assert!(shortlist.contains("| IN LOCAL LIBRARY"));
    assert!(
        !shortlist.contains("[tmdb:movie:3]"),
        "watchlisted titles are filtered"
    );
    assert!(shortlist.contains("[tmdb:movie:4] Candidate [movie]"));
}

#[tokio::test]
async fn fails_closed_and_does_not_notify_when_acquisition_fails() {
    let h = harness(vec![title(4, "Candidate")]).await;
    h.app.ai.script(
        ModelRole::RecsShortlist,
        vec![scores(&[("tmdb:movie:4", 80.0)])],
    );
    h.app
        .ai
        .script(ModelRole::RecsSelection, vec![select("tmdb:movie:4")]);
    h.watchlist
        .always(WatchlistAddOutcome::of(AddToWatchlistResult::Error));
    let error = run(&h, PipelineOptions::default())
        .await
        .expect_err("commit fails");
    assert!(
        error
            .to_string()
            .contains("acquisition or notification failed")
    );
    assert!(h.notifier.pushes().is_empty());
    let rows = recs(&h).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        row.source,
        Some(omni_media::types::CandidateSource::Trending)
    );
    assert_eq!(row.genres, Some(Vec::new()));
    let scores = row.shortlist_scores.as_ref().expect("scores");
    assert_eq!(
        (scores.taste_match, scores.novelty, scores.effort_fit),
        (80.0, 60.0, 90.0)
    );
    assert!((scores.composite - 73.15).abs() < 1e-9);
    assert!(scores.risks.is_empty());
    assert_eq!(row.status, RecommendationStatus::Failed);
    assert_eq!(row.watchlist_result, Some(WatchlistResult::Error));
}

#[tokio::test]
async fn timestamps_an_already_tracked_recommendation_when_closing_its_pending_row() {
    let h = harness(vec![title(4, "Candidate")]).await;
    h.app.ai.script(
        ModelRole::RecsShortlist,
        vec![scores(&[("tmdb:movie:4", 80.0)])],
    );
    h.app
        .ai
        .script(ModelRole::RecsSelection, vec![select("tmdb:movie:4")]);
    h.watchlist
        .always(WatchlistAddOutcome::of(AddToWatchlistResult::AlreadyExists));
    let summary = run(&h, PipelineOptions::default()).await.expect("run");
    assert_eq!(summary, "no_add: selected and backup are already tracked");
    let row = &recs(&h).await[0];
    assert_eq!(row.status, RecommendationStatus::Failed);
    assert_eq!(row.watchlist_result, Some(WatchlistResult::AlreadyExists));
    assert!(row.resolved_at.is_some());
}

#[tokio::test]
async fn does_not_resend_when_delivery_succeeded_before_the_notified_patch_failed() {
    let h = harness(vec![title(4, "Candidate")]).await;
    h.app.ai.script(
        ModelRole::RecsShortlist,
        vec![scores(&[("tmdb:movie:4", 80.0)])],
    );
    h.app
        .ai
        .script(ModelRole::RecsSelection, vec![select("tmdb:movie:4")]);
    h.watchlist
        .always(WatchlistAddOutcome::of(AddToWatchlistResult::Added));
    // After delivery, make the row undecodable so the `notified` patch fails.
    let store = h.services.store.clone();
    *h.notifier.hook.lock().expect("lock") = Some(Arc::new(move |push| {
        let store = store.clone();
        Box::pin(async move {
            let id = push.url.rsplit('/').next().unwrap_or_default().to_owned();
            store
                .write(move |tx| {
                    let pk = format!("$recs-recommendation-attempt#s{}:{id}", id.len());
                    let mut doc = tx.get_doc(&pk)?.expect("pending row");
                    if let Some(object) = doc.as_object_mut() {
                        object.insert("mediaType".to_owned(), JsValue::String("bogus".to_owned()));
                    }
                    tx.upsert_doc(
                        &pk,
                        &doc,
                        DocMeta {
                            entity: Some("recs-recommendation-attempt".to_owned()),
                            version: 0,
                            expires_at: None,
                            updated_at: None,
                        },
                    )
                })
                .await
                .expect("corrupt row");
        })
    }));
    assert!(run(&h, PipelineOptions::default()).await.is_err());
    assert_eq!(h.notifier.pushes().len(), 1);
    *h.notifier.hook.lock().expect("lock") = None;

    // The next run sees a stale pending row whose notification was reserved.
    let pushed_id = h.notifier.pushes()[0]
        .url
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let now = h.services.now();
    let two_hours_ago = now - 2 * 60 * 60 * 1000;
    h.services
        .store
        .write(move |tx| {
            let pk = format!(
                "$recs-recommendation-attempt#s{}:{pushed_id}",
                pushed_id.len()
            );
            let mut doc = tx.get_doc(&pk)?.expect("row");
            if let Some(object) = doc.as_object_mut() {
                object.insert("mediaType".to_owned(), JsValue::String("movie".to_owned()));
                object.insert("status".to_owned(), JsValue::String("pending".to_owned()));
                object.insert(
                    "recommendedAt".to_owned(),
                    JsValue::Int(two_hours_ago.into()),
                );
                object.insert(
                    "watchlistResult".to_owned(),
                    JsValue::String("added".to_owned()),
                );
                object.insert(
                    "notificationState".to_owned(),
                    JsValue::String("reserved".to_owned()),
                );
                object.insert(
                    "notificationReservedAt".to_owned(),
                    JsValue::Int(two_hours_ago.into()),
                );
            }
            let row: RecommendationData = omni_store::cbor::from_value(doc).expect("typed");
            tx.upsert(&row, UpsertOpts::default())
        })
        .await
        .expect("reset row");
    // A new GUID: aliases are cached per GUID, so reusing "tracked" would
    // keep resolving to tmdb:movie:3.
    *h.watchlist.items.lock().expect("lock") = FetchResult::Ok(vec![common::media(
        "tracked-4",
        "Candidate",
        MediaType::Movie,
        Some(4),
    )]);
    h.app.ai.script(
        ModelRole::RecsShortlist,
        vec![scores(&[("tmdb:movie:4", 80.0)])],
    );
    h.app
        .ai
        .script(ModelRole::RecsSelection, vec![no_add("not today")]);
    run(&h, PipelineOptions::default())
        .await
        .expect("second run");
    assert_eq!(h.notifier.pushes().len(), 1, "never re-sent");
    let row = &recs(&h).await[0];
    assert_eq!(row.status, RecommendationStatus::Notified);
    assert_eq!(row.notification_state, Some(NotificationState::Unknown));
    assert_eq!(row.notified_at, Some(two_hours_ago));
}

#[tokio::test]
async fn supports_a_full_dry_run_without_acquisition_or_notification() {
    let h = harness(vec![title(4, "Candidate")]).await;
    h.app.ai.script(
        ModelRole::RecsShortlist,
        vec![scores(&[("tmdb:movie:4", 80.0)])],
    );
    h.app
        .ai
        .script(ModelRole::RecsSelection, vec![select("tmdb:movie:4")]);
    let summary = run(
        &h,
        PipelineOptions {
            dry_run: true,
            max_recommendations: None,
        },
    )
    .await
    .expect("run");
    assert_eq!(summary, "dry_run: would recommend Candidate");
    assert!(h.watchlist.adds().is_empty());
    assert!(h.notifier.pushes().is_empty());
    assert!(recs(&h).await.is_empty());
}

#[tokio::test]
async fn researches_once_and_repeatedly_selects_from_the_shrinking_shortlist() {
    let titles: Vec<TmdbTitle> = (4..=10)
        .map(|id| title(id, &format!("Candidate {id}")))
        .collect();
    let h = harness(titles).await;
    let scored: Vec<(String, f64)> = (4..=10)
        .map(|id| {
            (
                format!("tmdb:movie:{id}"),
                f64::from(100 - u32::try_from(id).expect("id")),
            )
        })
        .collect();
    let scored_refs: Vec<(&str, f64)> = scored.iter().map(|(id, s)| (id.as_str(), *s)).collect();
    h.app
        .ai
        .script(ModelRole::RecsShortlist, vec![scores(&scored_refs)]);
    h.app.ai.script(
        ModelRole::RecsSelection,
        vec![
            select("tmdb:movie:4"),
            select("tmdb:movie:5"),
            no_add("remaining fit is weak"),
        ],
    );
    h.watchlist
        .always(WatchlistAddOutcome::of(AddToWatchlistResult::Added));
    let summary = run(
        &h,
        PipelineOptions {
            dry_run: false,
            max_recommendations: Some(3.0),
        },
    )
    .await
    .expect("run");
    assert_eq!(
        summary,
        "recommended 2/3: Candidate 4, Candidate 5; stopped: no_add: remaining fit is weak"
    );
    // max(5, 3 picks × 2) finalists, researched exactly once each.
    assert_eq!(h.research.queries.lock().expect("lock").len(), 6);
    let selections = prompts(&h, ModelRole::RecsSelection);
    assert_eq!(selections.len(), 3);
    assert!(selections[0].contains("[tmdb:movie:4]"));
    assert!(!selections[1].contains("[tmdb:movie:4]"));
    assert!(selections[1].contains("[tmdb:movie:5]"));
    assert!(!selections[2].contains("[tmdb:movie:5]"));
    assert!(
        !selections.iter().any(|p| p.contains("[tmdb:movie:10]")),
        "7th candidate cut"
    );
    assert_eq!(h.watchlist.adds().len(), 2);
    assert_eq!(h.notifier.pushes().len(), 2);
    let push = &h.notifier.pushes()[0];
    assert_eq!(push.url_title, "Rate this pick");
    assert!(
        push.url
            .starts_with("http://omni.boris/feedback/recommendations/")
    );
    let rows = recs(&h).await;
    assert!(
        rows.iter()
            .all(|r| r.status == RecommendationStatus::Notified
                && r.notification_state == Some(NotificationState::Sent)
                && r.watchlist_result == Some(WatchlistResult::Added))
    );
}

#[tokio::test]
async fn rejects_batch_limits_outside_1_through_10() {
    let h = harness(Vec::new()).await;
    let error = run(
        &h,
        PipelineOptions {
            dry_run: false,
            max_recommendations: Some(11.0),
        },
    )
    .await
    .expect_err("invalid");
    assert_eq!(
        error.to_string(),
        "maxRecommendations must be an integer from 1 to 10"
    );
    assert_eq!(*h.library.calls.lock().expect("lock"), 0);
}

#[tokio::test]
async fn records_the_first_passive_playback_signal() {
    let h = harness(vec![title(4, "Candidate")]).await;
    let open = RecommendationData {
        recommendation_id: "rec-1".to_owned(),
        canonical_id: "tmdb:movie:4".to_owned(),
        tmdb_id: 4,
        media_type: MediaType::Movie,
        title: "Candidate".to_owned(),
        status: RecommendationStatus::Notified,
        run_date: "2026-07-15".to_owned(),
        recommended_at: 1,
        ..RecommendationData::default()
    };
    h.services
        .store
        .write(move |tx| tx.upsert(&open, UpsertOpts::default()))
        .await
        .expect("seed");
    *h.library.in_progress.lock().expect("lock") = FetchResult::Ok(vec![InProgressItem {
        item: common::media("candidate-progress", "Candidate", MediaType::Movie, Some(4)),
        progress: 0.2,
        last_viewed_at: 200,
    }]);
    let summary = run(&h, PipelineOptions::default()).await.expect("run");
    assert_eq!(summary, "no eligible candidates after filtering");
    let row = &recs(&h).await[0];
    assert_eq!(row.started_at, Some(200));
    assert_eq!(row.status, RecommendationStatus::Notified);
}

async fn skips_when_view_unavailable(view: &str) {
    let h = harness(vec![title(4, "Candidate")]).await;
    match view {
        "in-progress" => {
            *h.library.in_progress.lock().expect("lock") = FetchResult::unavailable("Plex offline");
        }
        _ => *h.library.library.lock().expect("lock") = FetchResult::unavailable("Plex offline"),
    }
    let (summary, degraded) =
        omni_tasks::collect_degraded(run(&h, PipelineOptions::default())).await;
    assert_eq!(summary.expect("run"), "skipped: Plex offline");
    assert_eq!(degraded, vec!["media state unavailable: Plex offline"]);
    assert!(!h.catalog.calls().iter().any(|c| c == "trending"));
}

#[tokio::test]
async fn skips_when_the_plex_in_progress_view_is_unavailable() {
    skips_when_view_unavailable("in-progress").await;
}

#[tokio::test]
async fn skips_when_the_plex_library_view_is_unavailable() {
    skips_when_view_unavailable("library").await;
}

#[tokio::test]
async fn reconciles_a_stale_pending_row_by_sending_its_missed_notification_once() {
    let h = harness(Vec::new()).await;
    let now = h.services.now();
    let stale = RecommendationData {
        recommendation_id: "rec-stale".to_owned(),
        canonical_id: "tmdb:movie:3".to_owned(),
        tmdb_id: 3,
        media_type: MediaType::Movie,
        title: "Tracked".to_owned(),
        year: Some(2001),
        status: RecommendationStatus::Pending,
        why_for_user: Some("Because".to_owned()),
        run_date: "2026-01-01".to_owned(),
        recommended_at: now - 2 * 60 * 60 * 1000,
        ..RecommendationData::default()
    };
    h.services
        .store
        .write(move |tx| tx.upsert(&stale, UpsertOpts::default()))
        .await
        .expect("seed");
    let summary = run(&h, PipelineOptions::default()).await.expect("run");
    assert_eq!(summary, "no eligible candidates after filtering");
    let pushes = h.notifier.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0].title, "🎬 Tracked (2001)");
    assert_eq!(pushes[0].message, "Because");
    let row = &recs(&h).await[0];
    assert_eq!(row.status, RecommendationStatus::Notified);
    assert_eq!(row.notification_state, Some(NotificationState::Sent));
    assert!(row.notified_at.is_some_and(|at| at >= now));
}
