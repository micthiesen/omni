//! Radarr acquisition (wiremock and a raw socket; nothing leaves localhost).
#![allow(clippy::expect_used)]

mod common;

use std::time::Duration;

use omni_http::SideEffectMode;
use omni_media::arr::radarr::{add_radarr_movie, fetch_radarr_movies};
use omni_media::arr::{ArrConfig, ArrHttp, Limits};

/// The movie list's body bound (it runs to several MiB in a real library).
const MOVIE_LIST_MAX_BYTES: usize = Limits::LIBRARY.max_bytes;
use omni_media::types::{AddToWatchlistResult, ExternalIds, MediaItem, MediaType};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(url: &str) -> ArrConfig {
    ArrConfig {
        url: Some(url.to_owned()),
        api_key: Some("radarr-key".to_owned()),
        root_folder_path: Some("/movies".to_owned()),
        quality_profile_id: Some(4),
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

#[tokio::test]
async fn normalizes_tracked_movies() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/movie"))
        .and(header("X-Api-Key", "radarr-key"))
        .and(header("Accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 8, "title": "Arrival", "year": 2016, "tmdbId": 329865, "imdbId": "tt2543164"},
            {"id": 9, "title": "Malformed entry"}
        ])))
        .mount(&server)
        .await;
    let movies = fetch_radarr_movies(&http(), &config(&server.uri())).await;
    assert_eq!(
        movies,
        Some(vec![MediaItem {
            guid: "radarr:8".to_owned(),
            title: "Arrival".to_owned(),
            year: Some(2016),
            media_type: MediaType::Movie,
            external_ids: Some(ExternalIds {
                tmdb: Some(329865),
                imdb: Some("tt2543164".to_owned()),
                tvdb: None,
            }),
            title_slug: None,
        }])
    );
}

#[tokio::test]
async fn treats_malformed_tracked_movie_payloads_as_unavailable() {
    let server = MockServer::start().await;
    mount_once(&server, "GET", "/api/v3/movie", 200, json!(null)).await;
    assert_eq!(
        fetch_radarr_movies(&http(), &config(&server.uri())).await,
        None
    );
    mount_once(
        &server,
        "GET",
        "/api/v3/movie",
        200,
        json!([{"id": 8, "title": "Arrival", "tmdbId": 329865}, null]),
    )
    .await;
    assert_eq!(
        fetch_radarr_movies(&http(), &config(&server.uri())).await,
        None
    );
}

#[tokio::test]
async fn rejects_an_oversized_declared_response_before_buffering_it() {
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        MOVIE_LIST_MAX_BYTES + 1
    );
    let (url, _sent, closed) = common::raw_http_server(head, Vec::new()).await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        fetch_radarr_movies(&http(), &config(&url)),
    )
    .await
    .expect("rejected before the body arrives");
    assert_eq!(result, None);
    tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .expect("connection released")
        .expect("closed");
}

#[tokio::test]
async fn rejects_a_chunked_response_as_soon_as_it_crosses_the_byte_limit() {
    let head =
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n"
            .to_owned();
    let body = vec![
        common::chunk(&vec![b' '; MOVIE_LIST_MAX_BYTES]),
        common::chunk(b" "),
    ];
    let (url, _sent, closed) = common::raw_http_server(head, body).await;
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        fetch_radarr_movies(&http(), &config(&url)),
    )
    .await
    .expect("rejected without waiting for the stream to end");
    assert_eq!(result, None);
    tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .expect("connection released")
        .expect("closed");
}

#[tokio::test]
async fn reports_an_existing_movie_without_looking_it_up_or_writing() {
    let server = MockServer::start().await;
    mount_once(
        &server,
        "GET",
        "/api/v3/movie",
        200,
        json!([{"id": 8, "title": "Arrival", "tmdbId": 329865}]),
    )
    .await;
    let result = add_radarr_movie(&http(), &config(&server.uri()), 329865).await;
    assert_eq!(result, AddToWatchlistResult::AlreadyExists);
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn adds_a_looked_up_movie_with_acquisition_defaults_and_verifies_it() {
    let server = MockServer::start().await;
    let lookup = json!({"title": "Arrival", "year": 2016, "tmdbId": 329865});
    mount_once(&server, "GET", "/api/v3/movie", 200, json!([])).await;
    Mock::given(method("GET"))
        .and(path("/api/v3/movie/lookup/tmdb"))
        .and(query_param("tmdbId", "329865"))
        .respond_with(ResponseTemplate::new(200).set_body_json(lookup.clone()))
        .mount(&server)
        .await;
    mount_once(
        &server,
        "POST",
        "/api/v3/movie",
        201,
        json!({"id": 8, "title": "Arrival", "year": 2016, "tmdbId": 329865}),
    )
    .await;
    mount_once(
        &server,
        "GET",
        "/api/v3/movie",
        200,
        json!([{"id": 8, "title": "Arrival", "year": 2016, "tmdbId": 329865}]),
    )
    .await;
    let result = add_radarr_movie(&http(), &config(&server.uri()), 329865).await;
    assert_eq!(result, AddToWatchlistResult::Added);
    let requests = server.received_requests().await.expect("requests");
    let post = &requests[2];
    assert_eq!(post.method.as_str(), "POST");
    assert_eq!(post.url.path(), "/api/v3/movie");
    let body: serde_json::Value = serde_json::from_slice(&post.body).expect("json body");
    assert_eq!(body["tmdbId"], json!(329865));
    assert_eq!(body["qualityProfileId"], json!(4));
    assert_eq!(body["rootFolderPath"], json!("/movies"));
    assert_eq!(body["monitored"], json!(true));
    assert_eq!(body["addOptions"], json!({"searchForMovie": true}));
}

#[tokio::test]
async fn distinguishes_lookup_misses_service_failure_and_rejected_writes() {
    let missing = MockServer::start().await;
    mount_once(&missing, "GET", "/api/v3/movie", 200, json!([])).await;
    mount_once(&missing, "GET", "/api/v3/movie/lookup/tmdb", 200, json!({})).await;
    assert_eq!(
        add_radarr_movie(&http(), &config(&missing.uri()), 1).await,
        AddToWatchlistResult::NotFound
    );

    // Nothing listens on a closed port: the service is unavailable.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let closed = format!("http://{}", listener.local_addr().expect("addr"));
    drop(listener);
    assert_eq!(
        add_radarr_movie(&http(), &config(&closed), 1).await,
        AddToWatchlistResult::Unavailable
    );

    let rejected = MockServer::start().await;
    mount_once(&rejected, "GET", "/api/v3/movie", 200, json!([])).await;
    mount_once(
        &rejected,
        "GET",
        "/api/v3/movie/lookup/tmdb",
        200,
        json!({"title": "Movie", "tmdbId": 1}),
    )
    .await;
    mount_once(
        &rejected,
        "POST",
        "/api/v3/movie",
        400,
        json!({"message": "invalid"}),
    )
    .await;
    assert_eq!(
        add_radarr_movie(&http(), &config(&rejected.uri()), 1).await,
        AddToWatchlistResult::Error
    );
}

#[tokio::test]
async fn does_not_inspect_properties_on_a_malformed_lookup_response() {
    let server = MockServer::start().await;
    mount_once(&server, "GET", "/api/v3/movie", 200, json!([])).await;
    mount_once(
        &server,
        "GET",
        "/api/v3/movie/lookup/tmdb",
        200,
        json!(null),
    )
    .await;
    assert_eq!(
        add_radarr_movie(&http(), &config(&server.uri()), 1).await,
        AddToWatchlistResult::Unavailable
    );
}

#[tokio::test]
async fn records_instead_of_writing_in_record_mode() {
    let server = MockServer::start().await;
    mount_once(&server, "GET", "/api/v3/movie", 200, json!([])).await;
    mount_once(
        &server,
        "GET",
        "/api/v3/movie/lookup/tmdb",
        200,
        json!({"title": "Arrival", "tmdbId": 329865}),
    )
    .await;
    let http = ArrHttp::new(omni_testkit::no_network(), SideEffectMode::Record);
    let result = add_radarr_movie(&http, &config(&server.uri()), 329865).await;
    assert_eq!(result, AddToWatchlistResult::Added);
    let requests = server.received_requests().await.expect("requests");
    assert!(requests.iter().all(|r| r.method.as_str() == "GET"));
    let recorded = http.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].body["tmdbId"], json!(329865));
}
