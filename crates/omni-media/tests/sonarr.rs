//! Sonarr acquisition.
#![allow(clippy::expect_used)]

use crate::common;

use std::time::Duration;

use omni_http::SideEffectMode;
use omni_media::arr::sonarr::{add_sonarr_series, fetch_sonarr_series};
use omni_media::arr::{ArrConfig, ArrHttp};
use omni_media::types::{
    AddToWatchlistResult, ExternalIds, MediaItem, MediaType, WatchlistAddOutcome,
};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(url: &str) -> ArrConfig {
    ArrConfig {
        url: Some(url.to_owned()),
        api_key: Some("sonarr-key".to_owned()),
        root_folder_path: Some("/tv".to_owned()),
        quality_profile_id: Some(7),
    }
}

fn http() -> ArrHttp {
    ArrHttp::new(omni_testkit::no_network(), SideEffectMode::Live)
}

async fn mount_once(
    server: &MockServer,
    verb: &str,
    route: &str,
    status: u16,
    body: serde_json::Value,
) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .up_to_n_times(1)
        .mount(server)
        .await;
}

fn outcome(result: AddToWatchlistResult, slug: Option<&str>) -> WatchlistAddOutcome {
    WatchlistAddOutcome {
        result,
        title_slug: slug.map(str::to_owned),
    }
}

#[tokio::test]
async fn normalizes_tracked_series_with_tvdb_and_available_tmdb_ids() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        "GET",
        "/api/v3/series",
        200,
        json!([{"id": 12, "title": "Severance", "year": 2022, "tvdbId": 371980, "tmdbId": 95396, "imdbId": "tt11280740"}]),
    )
    .await;
    assert_eq!(
        fetch_sonarr_series(&http(), &config(&server.uri())).await,
        Some(vec![MediaItem {
            guid: "sonarr:12".to_owned(),
            title: "Severance".to_owned(),
            year: Some(2022),
            media_type: MediaType::Tv,
            external_ids: Some(ExternalIds {
                tvdb: Some(371980),
                tmdb: Some(95396),
                imdb: Some("tt11280740".to_owned()),
            }),
            title_slug: None,
        }])
    );
}

#[tokio::test]
async fn treats_malformed_tracked_series_payloads_as_unavailable() {
    let server = MockServer::start().await;
    mount_once(&server, "GET", "/api/v3/series", 200, json!(null)).await;
    assert_eq!(
        fetch_sonarr_series(&http(), &config(&server.uri())).await,
        None
    );
    mount_once(
        &server,
        "GET",
        "/api/v3/series",
        200,
        json!([{"id": 12, "title": "Severance", "tvdbId": 371980}, null]),
    )
    .await;
    assert_eq!(
        fetch_sonarr_series(&http(), &config(&server.uri())).await,
        None
    );
}

#[tokio::test]
async fn cancels_an_in_progress_response_read_when_interrupted() {
    let head =
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n"
            .to_owned();
    let (url, sent, closed) = common::raw_http_server(head, vec![common::chunk(b"[")]).await;
    let fetch = tokio::spawn(async move { fetch_sonarr_series(&http(), &config(&url)).await });
    tokio::time::timeout(Duration::from_secs(5), sent)
        .await
        .expect("headers sent")
        .expect("sent");
    tokio::time::sleep(Duration::from_millis(50)).await;
    fetch.abort();
    let joined = fetch.await;
    assert!(joined.is_err_and(|e| e.is_cancelled()));
    tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .expect("body read cancelled and connection released")
        .expect("closed");
}

#[tokio::test]
async fn looks_up_by_tmdb_adds_with_search_enabled_and_verifies_by_tvdb() {
    let server = MockServer::start().await;
    let lookup = json!({"title": "Severance", "year": 2022, "tvdbId": 371980});
    mount_once(&server, "GET", "/api/v3/series", 200, json!([])).await;
    Mock::given(method("GET"))
        .and(path("/api/v3/series/lookup"))
        .and(query_param("term", "tmdb:95396"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([lookup])))
        .mount(&server)
        .await;
    mount_once(
        &server,
        "POST",
        "/api/v3/series",
        201,
        json!({"id": 12, "title": "Severance", "year": 2022, "tvdbId": 371980}),
    )
    .await;
    mount_once(
        &server,
        "GET",
        "/api/v3/series",
        200,
        json!([{"id": 12, "titleSlug": "severance", "title": "Severance", "year": 2022, "tvdbId": 371980}]),
    )
    .await;
    assert_eq!(
        add_sonarr_series(&http(), &config(&server.uri()), 95396).await,
        outcome(AddToWatchlistResult::Added, Some("severance"))
    );
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests[1].url.path(), "/api/v3/series/lookup");
    assert_eq!(requests[1].url.query(), Some("term=tmdb%3A95396"));
    let post = &requests[2];
    assert_eq!(post.method.as_str(), "POST");
    assert_eq!(post.url.path(), "/api/v3/series");
    let body: serde_json::Value = serde_json::from_slice(&post.body).expect("json body");
    assert_eq!(body["tvdbId"], json!(371980));
    assert_eq!(body["qualityProfileId"], json!(7));
    assert_eq!(body["rootFolderPath"], json!("/tv"));
    assert_eq!(body["monitored"], json!(true));
    assert_eq!(body["seasonFolder"], json!(true));
    assert_eq!(
        body["addOptions"],
        json!({"searchForMissingEpisodes": true})
    );
}

#[tokio::test]
async fn reports_already_tracked_by_tmdb_without_a_lookup() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        "GET",
        "/api/v3/series",
        200,
        json!([{"id": 12, "title": "Severance", "titleSlug": "severance", "tvdbId": 371980, "tmdbId": 95396}]),
    )
    .await;
    assert_eq!(
        add_sonarr_series(&http(), &config(&server.uri()), 95396).await,
        outcome(AddToWatchlistResult::AlreadyExists, Some("severance"))
    );
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn recognizes_an_existing_series_by_the_tvdb_id_returned_from_lookup() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        "GET",
        "/api/v3/series",
        200,
        json!([{"id": 12, "title": "Severance", "tvdbId": 371980}]),
    )
    .await;
    mount_once(
        &server,
        "GET",
        "/api/v3/series/lookup",
        200,
        json!([{"title": "Severance", "tvdbId": 371980}]),
    )
    .await;
    assert_eq!(
        add_sonarr_series(&http(), &config(&server.uri()), 95396).await,
        outcome(AddToWatchlistResult::AlreadyExists, None)
    );
    assert_eq!(server.received_requests().await.expect("requests").len(), 2);
}

#[tokio::test]
async fn reports_a_lookup_miss_distinctly() {
    let server = MockServer::start().await;
    mount_once(&server, "GET", "/api/v3/series", 200, json!([])).await;
    mount_once(&server, "GET", "/api/v3/series/lookup", 200, json!([])).await;
    assert_eq!(
        add_sonarr_series(&http(), &config(&server.uri()), 1).await,
        outcome(AddToWatchlistResult::NotFound, None)
    );
}

#[tokio::test]
async fn does_not_claim_success_until_the_write_is_visible() {
    let server = MockServer::start().await;
    mount_once(&server, "GET", "/api/v3/series", 200, json!([])).await;
    mount_once(
        &server,
        "GET",
        "/api/v3/series/lookup",
        200,
        json!([{"title": "Severance", "tvdbId": 371980}]),
    )
    .await;
    mount_once(
        &server,
        "POST",
        "/api/v3/series",
        201,
        json!({"id": 12, "title": "Severance", "tvdbId": 371980}),
    )
    .await;
    mount_once(&server, "GET", "/api/v3/series", 200, json!([])).await;
    assert_eq!(
        add_sonarr_series(&http(), &config(&server.uri()), 95396).await,
        outcome(AddToWatchlistResult::Error, None)
    );
}

#[tokio::test]
async fn does_not_inspect_properties_on_a_malformed_lookup_response() {
    let server = MockServer::start().await;
    mount_once(&server, "GET", "/api/v3/series", 200, json!([])).await;
    mount_once(&server, "GET", "/api/v3/series/lookup", 200, json!([null])).await;
    assert_eq!(
        add_sonarr_series(&http(), &config(&server.uri()), 95396).await,
        outcome(AddToWatchlistResult::Unavailable, None)
    );
}

#[tokio::test]
async fn records_instead_of_writing_in_record_mode() {
    let server = MockServer::start().await;
    mount_once(&server, "GET", "/api/v3/series", 200, json!([])).await;
    mount_once(
        &server,
        "GET",
        "/api/v3/series/lookup",
        200,
        json!([{"title": "Severance", "titleSlug": "severance", "tvdbId": 371980, "tmdbId": 95396}]),
    )
    .await;
    let http = ArrHttp::new(omni_testkit::no_network(), SideEffectMode::Record);
    let result = add_sonarr_series(&http, &config(&server.uri()), 95396).await;
    // No slug: only Sonarr's own list may supply one, and nothing was written.
    assert_eq!(result, outcome(AddToWatchlistResult::Added, None));
    let requests = server.received_requests().await.expect("requests");
    assert!(requests.iter().all(|r| r.method.as_str() == "GET"));
    let recorded = http.recorded();
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].url.ends_with("/api/v3/series"));
    assert_eq!(recorded[0].body["tvdbId"], json!(371980));
    assert_eq!(
        recorded[0].body["addOptions"],
        json!({"searchForMissingEpisodes": true})
    );
}
