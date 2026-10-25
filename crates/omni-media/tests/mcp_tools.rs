//! The media MCP tools: inputs and outputs are
//! validated against the golden `tools/list` schemas by `typed_tool`.
#![allow(clippy::expect_used)]

mod common;

use std::sync::Arc;

use omni_api::media::{RecommendationStatus, WatchlistResult};
use omni_mcp_kit::{McpTool, ToolContext, ToolOutput, ToolPhase};
use omni_media::mcp::media_tools;
use omni_media::persistence::RecommendationData;
use omni_media::types::{
    AddToWatchlistResult, FetchResult, MediaType, WatchedItem, WatchlistAddOutcome,
};
use omni_store::entity::{EntityWrite as _, UpsertOpts};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn tool(tools: &[McpTool], name: &str) -> McpTool {
    tools
        .iter()
        .find(|t| t.meta.name == name)
        .cloned()
        .unwrap_or_else(|| panic!("missing tool {name}"))
}

async fn call(
    tools: &[McpTool],
    name: &str,
    input: Value,
) -> Result<Value, omni_mcp_kit::ToolError> {
    let cx = ToolContext {
        call_id: "1".to_owned(),
        cancel: CancellationToken::new(),
    };
    match tool(tools, name).handler.call(input, cx).await? {
        ToolOutput::Structured(map)
        | ToolOutput::Custom {
            structured: map, ..
        } => Ok(Value::Object(map)),
    }
}

fn tools(h: &common::Harness) -> Vec<McpTool> {
    media_tools(Arc::new(h.services.clone())).expect("tools")
}

#[tokio::test]
async fn registers_the_media_tools_in_serving_order() {
    let h = common::Harness::new().await;
    let names: Vec<String> = tools(&h).iter().map(|t| t.meta.name.clone()).collect();
    assert_eq!(
        names,
        vec![
            "media_catalog_search",
            "media_catalog_get",
            "media_catalog_browse",
            "media_library_list",
            "media_watchlist_list",
            "media_watchlist_add",
            "media_series_get",
            "media_series_add",
            "media_series_seasons_set",
            "media_series_search",
            "media_series_profile_set",
            "media_arr_profiles",
            "media_arr_status",
            "media_downloads_list",
            "media_movies_get",
            "media_movie_collections",
            "media_movies_profile_set",
            "media_movies_search",
            "media_movie_releases",
            "media_movie_release_grab",
            "media_recommendations_list",
            "media_recommendation_get",
            "media_recommendation_feedback",
            "media_taste_read",
        ]
    );
}

#[tokio::test]
async fn searches_the_catalog_with_a_trimmed_query_and_paginates() {
    let h = common::Harness::new().await;
    *h.catalog.search.lock().expect("lock") = (1..=3)
        .map(|id| common::tmdb_title(id, MediaType::Movie))
        .collect();
    let tools = tools(&h);
    let out = call(
        &tools,
        "media_catalog_search",
        json!({"query": "  dune ", "mediaType": "movie", "limit": 2}),
    )
    .await
    .expect("search");
    assert_eq!(out["total"], json!(3));
    assert_eq!(out["nextCursor"], json!(2));
    assert_eq!(
        out["items"][0]["tmdbUrl"],
        json!("https://www.themoviedb.org/movie/1")
    );
    assert_eq!(out["items"][0]["voteCount"], json!(500));
    assert_eq!(out["items"][0]["year"], json!(null));
    assert!(
        h.catalog
            .calls()
            .contains(&"search:dune:movie:None".to_owned())
    );
    let blank = call(
        &tools,
        "media_catalog_search",
        json!({"query": "   ", "mediaType": "movie"}),
    )
    .await;
    assert!(matches!(blank, Err(e) if e.phase == ToolPhase::Input));
}

#[tokio::test]
async fn reports_an_unavailable_plex_view_as_an_error_and_filters_items() {
    let h = common::Harness::new().await;
    let tools = tools(&h);
    *h.library.history.lock().expect("lock") =
        FetchResult::unavailable("Plex watch history failed: down");
    let error = call(&tools, "media_library_list", json!({"view": "history"}))
        .await
        .expect_err("unavailable");
    assert_eq!(error.message, "Plex watch history failed: down");
    *h.library.history.lock().expect("lock") = FetchResult::Ok(vec![
        WatchedItem {
            item: common::media("a", "Arrival", MediaType::Movie, Some(1)),
            viewed_at: 5,
            view_count: 1,
            completion: None,
        },
        WatchedItem {
            item: common::media("b", "Severance", MediaType::Tv, Some(2)),
            viewed_at: 6,
            view_count: 1,
            completion: Some(0.5),
        },
    ]);
    let out = call(
        &tools,
        "media_library_list",
        json!({"view": "history", "query": "ARR"}),
    )
    .await
    .expect("list");
    assert_eq!(out["total"], json!(1));
    assert_eq!(
        out["items"][0],
        json!({"guid": "a", "title": "Arrival", "year": null, "mediaType": "movie",
               "externalIds": {"tmdb": 1}, "viewedAt": 5, "viewCount": 1})
    );
    let tv = call(
        &tools,
        "media_library_list",
        json!({"view": "history", "mediaType": "tv"}),
    )
    .await
    .expect("list");
    assert_eq!(tv["items"][0]["completion"], json!(0.5));
}

#[tokio::test]
async fn adds_to_the_watchlist_through_the_arr_adapter() {
    let h = common::Harness::new().await;
    h.watchlist.always(WatchlistAddOutcome {
        result: AddToWatchlistResult::Added,
        title_slug: Some("severance".to_owned()),
    });
    let tools = tools(&h);
    let out = call(
        &tools,
        "media_watchlist_add",
        json!({"tmdbId": 95396, "mediaType": "tv", "title": "Severance"}),
    )
    .await
    .expect("add");
    assert_eq!(out, json!({"result": "added", "titleSlug": "severance"}));
    assert_eq!(h.watchlist.adds()[0].tmdb_id, 95396);
}

#[tokio::test]
async fn lists_gets_and_rates_recommendations() {
    let h = common::Harness::new().await;
    let rows = vec![
        RecommendationData {
            recommendation_id: "r1".to_owned(),
            canonical_id: "tmdb:movie:1".to_owned(),
            tmdb_id: 1,
            media_type: MediaType::Movie,
            title: "One".to_owned(),
            status: RecommendationStatus::Notified,
            run_date: "2026-01-01".to_owned(),
            recommended_at: 10,
            watchlist_result: Some(WatchlistResult::Available),
            ..RecommendationData::default()
        },
        RecommendationData {
            recommendation_id: "r2".to_owned(),
            canonical_id: "tmdb:movie:2".to_owned(),
            tmdb_id: 2,
            media_type: MediaType::Movie,
            title: "Two".to_owned(),
            status: RecommendationStatus::Failed,
            run_date: "2026-01-01".to_owned(),
            recommended_at: 20,
            ..RecommendationData::default()
        },
    ];
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
    let tools = tools(&h);
    let all = call(&tools, "media_recommendations_list", json!({}))
        .await
        .expect("list");
    assert_eq!(all["total"], json!(2));
    assert_eq!(all["items"][0]["recommendationId"], json!("r2"));
    let notified = call(
        &tools,
        "media_recommendations_list",
        json!({"status": "notified", "feedback": "none"}),
    )
    .await
    .expect("list");
    assert_eq!(notified["items"][0]["watchlistResult"], json!("available"));
    let missing = call(
        &tools,
        "media_recommendation_get",
        json!({"recommendationId": "nope"}),
    )
    .await
    .expect_err("missing");
    assert_eq!(missing.message, "Media recommendation not found");
    let refine = call(
        &tools,
        "media_recommendation_feedback",
        json!({"recommendationId": "r1"}),
    )
    .await
    .expect_err("refine");
    assert_eq!(
        (refine.phase, refine.message.as_str()),
        (ToolPhase::Input, "feedback or note is required")
    );
    let rated = call(
        &tools,
        "media_recommendation_feedback",
        json!({"recommendationId": "r1", "feedback": "good_pick", "note": " loved it "}),
    )
    .await
    .expect("rate");
    assert_eq!(rated["recommendation"]["feedback"], json!("good_pick"));
    assert_eq!(rated["recommendation"]["feedbackNote"], json!("loved it"));
}

#[tokio::test]
async fn reads_the_taste_profile_and_evidence_pages() {
    let h = common::Harness::new().await;
    let tools = tools(&h);
    let profile = call(&tools, "media_taste_read", json!({"resource": "profile"}))
        .await
        .expect("profile");
    assert_eq!(profile, json!({"resource": "profile", "profile": null}));
    let evidence = call(
        &tools,
        "media_taste_read",
        json!({"resource": "evidence", "limit": 5}),
    )
    .await
    .expect("evidence");
    assert_eq!(
        evidence,
        json!({"resource": "evidence", "items": [], "nextCursor": null, "total": 0})
    );
}
