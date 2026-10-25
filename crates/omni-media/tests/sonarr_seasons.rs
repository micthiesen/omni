//! Season-level Sonarr management: add with selected seasons, monitoring
//! changes, and searches that never repeat a search already covered.
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use omni_http::SideEffectMode;
use omni_mcp_kit::{McpTool, ToolContext, ToolOutput};
use omni_media::arr::seasons::{
    SearchOutcome, SeasonAddRequest, SeasonSearchRequest, SeasonSetRequest, SeriesProfileRequest,
    SeriesResult, SonarrSeasons,
};
use omni_media::arr::{ArrConfig, ArrHttp};
use omni_media::mcp::media_tools;
use omni_media::types::{AddToWatchlistResult, MediaType};
use omni_media::watchlist::{ArrWatchlist, Watchlist, WatchlistAddRequest};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const RHOM_TMDB: i64 = 61_000;
const RHOM_TVDB: i64 = 260_000;
const RHONY_TMDB: i64 = 4_640;
const RHONY_TVDB: i64 = 83_300;

fn config(url: &str) -> ArrConfig {
    ArrConfig {
        url: Some(url.to_owned()),
        api_key: Some("sonarr-key".to_owned()),
        root_folder_path: Some("/tv".to_owned()),
        quality_profile_id: Some(7),
    }
}

fn set(seasons: &[i64]) -> BTreeSet<i64> {
    seasons.iter().copied().collect()
}

fn season(number: i64, monitored: bool, episodes: i64, files: i64) -> Value {
    json!({
        "seasonNumber": number,
        "monitored": monitored,
        "statistics": {"episodeCount": episodes, "episodeFileCount": files, "totalEpisodeCount": episodes}
    })
}

fn series(id: i64, tmdb: i64, tvdb: i64, monitored: bool, seasons: Vec<Value>) -> Value {
    json!({
        "id": id, "title": format!("Series {tmdb}"), "titleSlug": format!("series-{tmdb}"),
        "year": 2020, "tvdbId": tvdb, "tmdbId": tmdb, "monitored": monitored,
        "monitorNewItems": "all", "qualityProfileId": 7, "path": "/tv/x",
        "seasons": seasons
    })
}

fn lookup(tmdb: i64, tvdb: i64, seasons: &[i64]) -> Value {
    json!([{
        "title": format!("Series {tmdb}"), "titleSlug": format!("series-{tmdb}"), "year": 2020,
        "tvdbId": tvdb, "tmdbId": tmdb,
        "seasons": seasons.iter().map(|n| json!({"seasonNumber": n, "monitored": true})).collect::<Vec<_>>()
    }])
}

async fn mount(server: &MockServer, verb: &str, route: &str, body: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn mount_once(server: &MockServer, verb: &str, route: &str, body: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .up_to_n_times(1)
        .mount(server)
        .await;
}

async fn mount_quiet_activity(server: &MockServer) {
    mount(server, "GET", "/api/v3/command", json!([])).await;
    mount(server, "GET", "/api/v3/queue/details", json!([])).await;
}

async fn requests(server: &MockServer, verb: &str, route: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("recording")
        .into_iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == route)
        .map(|r| serde_json::from_slice(&r.body).expect("json body"))
        .collect()
}

fn monitored_flags(body: &Value) -> Vec<(i64, bool)> {
    body["seasons"]
        .as_array()
        .expect("seasons")
        .iter()
        .map(|s| {
            (
                s["seasonNumber"].as_i64().expect("number"),
                s["monitored"].as_bool().expect("flag"),
            )
        })
        .collect()
}

async fn seasons_over(server: &MockServer) -> (common::Harness, SonarrSeasons) {
    let h = common::Harness::new().await;
    let seasons = common::sonarr_seasons(&h.app, config(&server.uri()), SideEffectMode::Live);
    (h, seasons)
}

#[tokio::test]
async fn adds_a_new_series_with_only_the_selected_seasons_monitored() {
    let server = MockServer::start().await;
    mount_once(&server, "GET", "/api/v3/series", json!([])).await;
    mount(
        &server,
        "GET",
        "/api/v3/series/lookup",
        lookup(RHOM_TMDB, RHOM_TVDB, &[0, 1, 2, 3, 4, 5]),
    )
    .await;
    let added = series(
        42,
        RHOM_TMDB,
        RHOM_TVDB,
        true,
        vec![
            season(0, false, 0, 0),
            season(1, true, 0, 0),
            season(2, true, 0, 0),
            season(3, true, 0, 0),
            season(4, false, 0, 0),
            season(5, false, 0, 0),
        ],
    );
    mount(&server, "POST", "/api/v3/series", added.clone()).await;
    mount(&server, "GET", "/api/v3/series/42", added).await;
    mount_quiet_activity(&server).await;
    let (_h, seasons) = seasons_over(&server).await;

    let outcome = seasons
        .add(SeasonAddRequest {
            tmdb_id: RHOM_TMDB,
            seasons: set(&[1, 2, 3]),
            search: false,
            monitor_new_seasons: None,
        })
        .await;

    assert_eq!(outcome.result, SeriesResult::Added);
    assert!(outcome.search.is_empty(), "search was not requested");
    let posted = requests(&server, "POST", "/api/v3/series").await;
    assert_eq!(
        monitored_flags(&posted[0]),
        vec![
            (0, false),
            (1, true),
            (2, true),
            (3, true),
            (4, false),
            (5, false)
        ]
    );
    assert_eq!(posted[0]["monitorNewItems"], "none");
    assert_eq!(
        posted[0]["addOptions"],
        json!({"searchForMissingEpisodes": false})
    );
    assert!(
        requests(&server, "PUT", "/api/v3/series/42")
            .await
            .is_empty()
    );
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty()
    );
    let status = outcome.series.expect("series read back");
    let monitored: Vec<i64> = status
        .seasons
        .iter()
        .filter(|s| s.monitored)
        .map(|s| s.season_number)
        .collect();
    assert_eq!(monitored, vec![1, 2, 3]);
}

#[tokio::test]
async fn a_post_add_search_is_never_repeated_by_a_season_search() {
    let server = MockServer::start().await;
    mount_once(&server, "GET", "/api/v3/series", json!([])).await;
    mount(
        &server,
        "GET",
        "/api/v3/series/lookup",
        lookup(RHOM_TMDB, RHOM_TVDB, &[1, 2, 3, 4]),
    )
    .await;
    let mut added = series(
        42,
        RHOM_TMDB,
        RHOM_TVDB,
        true,
        vec![
            season(1, true, 10, 0),
            season(2, true, 10, 0),
            season(3, false, 0, 0),
            season(4, false, 0, 0),
        ],
    );
    mount(&server, "POST", "/api/v3/series", added.clone()).await;
    mount(&server, "GET", "/api/v3/series", json!([added.clone()])).await;
    // The post-add refresh has finished, so Sonarr dropped addOptions.
    added["addOptions"] = Value::Null;
    mount(&server, "GET", "/api/v3/series/42", added).await;
    mount_quiet_activity(&server).await;
    let (_h, seasons) = seasons_over(&server).await;

    let outcome = seasons
        .add(SeasonAddRequest {
            tmdb_id: RHOM_TMDB,
            seasons: set(&[1, 2]),
            search: true,
            monitor_new_seasons: None,
        })
        .await;
    assert_eq!(outcome.result, SeriesResult::Added);
    assert!(
        outcome
            .search
            .iter()
            .all(|s| s.outcome == SearchOutcome::QueuedAfterAdd)
    );
    let posted = requests(&server, "POST", "/api/v3/series").await;
    assert_eq!(
        posted[0]["addOptions"],
        json!({"searchForMissingEpisodes": true})
    );

    let again = seasons
        .search(SeasonSearchRequest {
            tmdb_id: RHOM_TMDB,
            seasons: set(&[1, 2]),
            force: false,
        })
        .await;
    assert_eq!(again.result, SeriesResult::NothingSearched);
    assert!(
        again
            .search
            .iter()
            .all(|s| s.outcome == SearchOutcome::RecentlySearched)
    );
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty()
    );
    let status = again.series.expect("status");
    assert!(status.seasons[0].last_search.is_some());
}

#[tokio::test]
async fn searches_each_season_once_until_forced() {
    let server = MockServer::start().await;
    let tracked = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        true,
        vec![season(7, true, 20, 5), season(8, true, 20, 20)],
    );
    mount(&server, "GET", "/api/v3/series", json!([tracked.clone()])).await;
    mount(&server, "GET", "/api/v3/series/9", tracked).await;
    mount_quiet_activity(&server).await;
    mount(&server, "POST", "/api/v3/command", json!({"id": 501})).await;
    let (_h, seasons) = seasons_over(&server).await;
    let request = |force| SeasonSearchRequest {
        tmdb_id: RHONY_TMDB,
        seasons: set(&[7, 8]),
        force,
    };

    let first = seasons.search(request(false)).await;
    assert_eq!(first.result, SeriesResult::Searched);
    let outcomes: Vec<_> = first.search.iter().map(|s| s.outcome).collect();
    assert_eq!(
        outcomes,
        vec![SearchOutcome::Queued, SearchOutcome::NothingMissing]
    );
    assert_eq!(first.search[0].command_id, Some(501));
    assert_eq!(
        requests(&server, "POST", "/api/v3/command").await,
        vec![json!({"name": "SeasonSearch", "seriesId": 9, "seasonNumber": 7})]
    );

    let second = seasons.search(request(false)).await;
    assert_eq!(second.search[0].outcome, SearchOutcome::RecentlySearched);
    assert_eq!(requests(&server, "POST", "/api/v3/command").await.len(), 1);

    let forced = seasons.search(request(true)).await;
    assert_eq!(forced.search[0].outcome, SearchOutcome::Queued);
    assert_eq!(requests(&server, "POST", "/api/v3/command").await.len(), 2);
}

#[tokio::test]
async fn skips_pending_running_and_unmonitored_seasons() {
    let server = MockServer::start().await;
    let mut tracked = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        true,
        vec![
            season(6, false, 0, 0),
            season(7, true, 20, 0),
            season(8, true, 20, 0),
        ],
    );
    mount(&server, "GET", "/api/v3/series", json!([tracked.clone()])).await;
    mount(
        &server,
        "GET",
        "/api/v3/command",
        json!([
            {"id": 1, "name": "SeasonSearch", "status": "started", "body": {"seriesId": 9, "seasonNumber": 7}},
            {"id": 2, "name": "SeasonSearch", "status": "completed", "body": {"seriesId": 9, "seasonNumber": 8}}
        ]),
    )
    .await;
    mount(&server, "GET", "/api/v3/queue/details", json!([])).await;
    mount(&server, "POST", "/api/v3/command", json!({"id": 3})).await;
    mount_once(&server, "GET", "/api/v3/series/9", tracked.clone()).await;
    tracked["addOptions"] = json!({"searchForMissingEpisodes": true});
    mount(&server, "GET", "/api/v3/series/9", tracked).await;
    let (_h, seasons) = seasons_over(&server).await;
    let request = SeasonSearchRequest {
        tmdb_id: RHONY_TMDB,
        seasons: set(&[6, 7, 8]),
        force: true,
    };

    let outcomes: Vec<_> = seasons
        .search(request.clone())
        .await
        .search
        .iter()
        .map(|s| s.outcome)
        .collect();
    assert_eq!(
        outcomes,
        vec![
            SearchOutcome::NotMonitored,
            SearchOutcome::AlreadyRunning,
            SearchOutcome::Queued
        ]
    );

    // A series whose post-add search has not run yet is never searched again.
    let pending: Vec<_> = seasons
        .search(request)
        .await
        .search
        .iter()
        .map(|s| s.outcome)
        .collect();
    assert_eq!(
        pending,
        vec![
            SearchOutcome::NotMonitored,
            SearchOutcome::InitialSearchPending,
            SearchOutcome::InitialSearchPending
        ]
    );
    assert_eq!(requests(&server, "POST", "/api/v3/command").await.len(), 1);
}

#[tokio::test]
async fn an_uncertain_search_is_not_retried_and_a_rejected_one_is() {
    let server = MockServer::start().await;
    let tracked = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        true,
        vec![season(7, true, 20, 0)],
    );
    mount(&server, "GET", "/api/v3/series", json!([tracked.clone()])).await;
    mount(&server, "GET", "/api/v3/series/9", tracked).await;
    mount_quiet_activity(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v3/command"))
        .respond_with(ResponseTemplate::new(400))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v3/command"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let (_h, seasons) = seasons_over(&server).await;
    let request = SeasonSearchRequest {
        tmdb_id: RHONY_TMDB,
        seasons: set(&[7]),
        force: false,
    };

    let rejected = seasons.search(request.clone()).await;
    assert_eq!(rejected.search[0].outcome, SearchOutcome::Failed);
    // A 5xx may come after Sonarr queued the command.
    let uncertain = seasons.search(request.clone()).await;
    assert_eq!(uncertain.search[0].outcome, SearchOutcome::Uncertain);
    let after = seasons.search(request).await;
    assert_eq!(after.search[0].outcome, SearchOutcome::RecentlySearched);
    assert_eq!(requests(&server, "POST", "/api/v3/command").await.len(), 2);
}

#[tokio::test]
async fn a_rejected_forced_search_keeps_the_earlier_search_record() {
    let server = MockServer::start().await;
    let tracked = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        true,
        vec![season(7, true, 20, 0)],
    );
    mount(&server, "GET", "/api/v3/series", json!([tracked.clone()])).await;
    mount(&server, "GET", "/api/v3/series/9", tracked).await;
    mount_quiet_activity(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v3/command"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 5})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v3/command"))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;
    let (_h, seasons) = seasons_over(&server).await;
    let request = |force| SeasonSearchRequest {
        tmdb_id: RHONY_TMDB,
        seasons: set(&[7]),
        force,
    };

    assert_eq!(
        seasons.search(request(false)).await.search[0].outcome,
        SearchOutcome::Queued
    );
    assert_eq!(
        seasons.search(request(true)).await.search[0].outcome,
        SearchOutcome::Failed
    );
    assert_eq!(
        seasons.search(request(false)).await.search[0].outcome,
        SearchOutcome::RecentlySearched
    );
}

#[tokio::test]
async fn waits_for_the_first_refresh_and_library_wide_searches() {
    let server = MockServer::start().await;
    let mut tracked = series(9, RHONY_TMDB, RHONY_TVDB, true, vec![season(7, true, 0, 0)]);
    tracked["addOptions"] = json!({"searchForMissingEpisodes": false});
    mount(&server, "GET", "/api/v3/series", json!([tracked.clone()])).await;
    mount_once(&server, "GET", "/api/v3/series/9", tracked.clone()).await;
    tracked["addOptions"] = Value::Null;
    tracked["seasons"] = json!([season(7, true, 20, 0)]);
    mount(&server, "GET", "/api/v3/series/9", tracked).await;
    mount(
        &server,
        "GET",
        "/api/v3/command",
        json!([{"id": 4, "name": "MissingEpisodeSearch", "status": "queued", "body": {"monitored": true}}]),
    )
    .await;
    mount(&server, "GET", "/api/v3/queue/details", json!([])).await;
    let (_h, seasons) = seasons_over(&server).await;
    let request = SeasonSearchRequest {
        tmdb_id: RHONY_TMDB,
        seasons: set(&[7]),
        force: false,
    };

    let refreshing = seasons.search(request.clone()).await;
    assert_eq!(refreshing.search[0].outcome, SearchOutcome::RefreshPending);
    let library_wide = seasons.search(request).await;
    assert_eq!(
        library_wide.search[0].outcome,
        SearchOutcome::AlreadyRunning
    );
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn monitors_seasons_of_an_existing_series_without_touching_others() {
    let server = MockServer::start().await;
    let tracked = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        true,
        vec![
            season(1, false, 0, 0),
            season(6, true, 20, 20),
            season(7, true, 20, 20),
            season(8, false, 0, 0),
            season(9, false, 0, 0),
            season(10, false, 0, 0),
        ],
    );
    let written = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        true,
        vec![
            season(1, false, 0, 0),
            season(6, true, 20, 20),
            season(7, true, 20, 20),
            season(8, true, 20, 0),
            season(9, true, 20, 0),
            season(10, false, 0, 0),
        ],
    );
    mount(&server, "GET", "/api/v3/series", json!([tracked.clone()])).await;
    mount_once(&server, "GET", "/api/v3/series/9", tracked).await;
    mount(&server, "GET", "/api/v3/series/9", written.clone()).await;
    mount(&server, "PUT", "/api/v3/series/9", written).await;
    mount_quiet_activity(&server).await;
    let (_h, seasons) = seasons_over(&server).await;

    let outcome = seasons
        .add(SeasonAddRequest {
            tmdb_id: RHONY_TMDB,
            seasons: set(&[7, 8, 9]),
            search: false,
            monitor_new_seasons: None,
        })
        .await;

    assert_eq!(outcome.result, SeriesResult::Updated);
    assert_eq!(outcome.changed_seasons, vec![8, 9]);
    let put = requests(&server, "PUT", "/api/v3/series/9").await;
    assert_eq!(
        monitored_flags(&put[0]),
        vec![
            (1, false),
            (6, true),
            (7, true),
            (8, true),
            (9, true),
            (10, false)
        ]
    );
    assert_eq!(put[0]["path"], "/tv/x", "unknown fields are written back");
    assert_eq!(put[0]["monitorNewItems"], "all", "left unchanged");
    assert!(requests(&server, "POST", "/api/v3/series").await.is_empty());
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn an_already_matching_request_writes_nothing() {
    let server = MockServer::start().await;
    let tracked = series(9, RHONY_TMDB, RHONY_TVDB, true, vec![season(7, true, 1, 1)]);
    mount(&server, "GET", "/api/v3/series", json!([tracked.clone()])).await;
    mount(&server, "GET", "/api/v3/series/9", tracked).await;
    mount_quiet_activity(&server).await;
    let (_h, seasons) = seasons_over(&server).await;

    let outcome = seasons
        .set(SeasonSetRequest {
            tmdb_id: RHONY_TMDB,
            monitor: set(&[7]),
            unmonitor: BTreeSet::new(),
            monitor_new_seasons: None,
        })
        .await;

    assert_eq!(outcome.result, SeriesResult::Unchanged);
    assert!(outcome.changed_seasons.is_empty());
    assert!(
        requests(&server, "PUT", "/api/v3/series/9")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn monitoring_a_season_of_an_unmonitored_series_keeps_other_seasons_off() {
    let server = MockServer::start().await;
    let tracked = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        false,
        vec![
            season(1, true, 0, 0),
            season(2, false, 0, 0),
            season(3, true, 0, 0),
        ],
    );
    let mut written = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        true,
        vec![
            season(1, false, 0, 0),
            season(2, true, 0, 0),
            season(3, false, 0, 0),
        ],
    );
    written["monitorNewItems"] = json!("none");
    mount(&server, "GET", "/api/v3/series", json!([tracked.clone()])).await;
    mount_once(&server, "GET", "/api/v3/series/9", tracked).await;
    mount(&server, "GET", "/api/v3/series/9", written.clone()).await;
    mount(&server, "PUT", "/api/v3/series/9", written).await;
    mount_quiet_activity(&server).await;
    let (_h, seasons) = seasons_over(&server).await;

    let outcome = seasons
        .set(SeasonSetRequest {
            tmdb_id: RHONY_TMDB,
            monitor: set(&[2]),
            unmonitor: BTreeSet::new(),
            monitor_new_seasons: Some(false),
        })
        .await;

    assert_eq!(outcome.result, SeriesResult::Updated);
    let put = requests(&server, "PUT", "/api/v3/series/9").await;
    assert_eq!(put[0]["monitored"], true);
    assert_eq!(put[0]["monitorNewItems"], "none");
    assert_eq!(
        monitored_flags(&put[0]),
        vec![(1, false), (2, true), (3, false)]
    );
}

#[tokio::test]
async fn unknown_seasons_change_nothing() {
    let server = MockServer::start().await;
    mount(&server, "GET", "/api/v3/series", json!([])).await;
    mount(
        &server,
        "GET",
        "/api/v3/series/lookup",
        lookup(RHOM_TMDB, RHOM_TVDB, &[1, 2, 3]),
    )
    .await;
    let (_h, seasons) = seasons_over(&server).await;

    let outcome = seasons
        .add(SeasonAddRequest {
            tmdb_id: RHOM_TMDB,
            seasons: set(&[3, 4]),
            search: true,
            monitor_new_seasons: None,
        })
        .await;

    assert_eq!(outcome.result, SeriesResult::SeasonNotFound);
    assert_eq!(outcome.unknown_seasons, vec![4]);
    let status = outcome.series.expect("lookup");
    assert!(!status.tracked);
    assert_eq!(status.seasons.len(), 3);
    assert!(requests(&server, "POST", "/api/v3/series").await.is_empty());
}

#[tokio::test]
async fn a_whole_series_watchlist_add_counts_as_a_search_of_every_season() {
    let server = MockServer::start().await;
    let tracked = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        true,
        vec![season(1, true, 10, 0), season(2, true, 10, 0)],
    );
    mount_once(&server, "GET", "/api/v3/series", json!([])).await;
    Mock::given(method("GET"))
        .and(path("/api/v3/series/lookup"))
        .and(query_param("term", format!("tmdb:{RHONY_TMDB}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(lookup(
            RHONY_TMDB,
            RHONY_TVDB,
            &[1, 2],
        )))
        .mount(&server)
        .await;
    mount(&server, "POST", "/api/v3/series", tracked.clone()).await;
    mount(&server, "GET", "/api/v3/series", json!([tracked.clone()])).await;
    mount(&server, "GET", "/api/v3/series/9", tracked).await;
    mount_quiet_activity(&server).await;
    let (h, seasons) = seasons_over(&server).await;
    let http = ArrHttp::new(omni_testkit::no_network(), SideEffectMode::Live);
    let watchlist = ArrWatchlist::new(
        http,
        ArrConfig::default(),
        config(&server.uri()),
        seasons.clone(),
        common::radarr_movies(&h.app, ArrConfig::default(), SideEffectMode::Live),
    );

    let added = watchlist
        .add(&WatchlistAddRequest {
            tmdb_id: RHONY_TMDB,
            media_type: MediaType::Tv,
            title: "Series".to_owned(),
            year: None,
            external_ids: None,
        })
        .await;
    assert_eq!(added.result, AddToWatchlistResult::Added);

    let search = seasons
        .search(SeasonSearchRequest {
            tmdb_id: RHONY_TMDB,
            seasons: set(&[1, 2]),
            force: false,
        })
        .await;
    assert!(
        search
            .search
            .iter()
            .all(|s| s.outcome == SearchOutcome::RecentlySearched)
    );
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty()
    );
    drop(h);
}

async fn call(tools: &[McpTool], name: &str, input: Value) -> Value {
    let cx = ToolContext {
        call_id: "1".to_owned(),
        cancel: CancellationToken::new(),
    };
    let tool = tools.iter().find(|t| t.meta.name == name).expect("tool");
    match tool.handler.call(input, cx).await.expect(name) {
        ToolOutput::Structured(map)
        | ToolOutput::Custom {
            structured: map, ..
        } => Value::Object(map),
    }
}

#[tokio::test]
async fn adds_seasons_of_two_series_in_one_workflow_through_the_tools() {
    let server = MockServer::start().await;
    let rhony = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        true,
        vec![
            season(6, true, 20, 20),
            season(7, false, 0, 0),
            season(8, false, 0, 0),
            season(9, false, 0, 0),
        ],
    );
    let rhony_written = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        true,
        vec![
            season(6, true, 20, 20),
            season(7, true, 20, 0),
            season(8, true, 20, 0),
            season(9, true, 20, 0),
        ],
    );
    let rhom = series(
        42,
        RHOM_TMDB,
        RHOM_TVDB,
        true,
        vec![
            season(1, true, 0, 0),
            season(2, true, 0, 0),
            season(3, true, 0, 0),
            season(4, false, 0, 0),
        ],
    );
    // RHONY is tracked; RHOM is not until it is added.
    mount_once(&server, "GET", "/api/v3/series", json!([rhony.clone()])).await;
    mount(
        &server,
        "GET",
        "/api/v3/series",
        json!([rhony.clone(), rhom.clone()]),
    )
    .await;
    mount(
        &server,
        "GET",
        "/api/v3/series/lookup",
        lookup(RHOM_TMDB, RHOM_TVDB, &[1, 2, 3, 4]),
    )
    .await;
    mount(&server, "POST", "/api/v3/series", rhom.clone()).await;
    mount(&server, "GET", "/api/v3/series/42", rhom).await;
    mount_once(&server, "GET", "/api/v3/series/9", rhony).await;
    Mock::given(method("GET"))
        .and(path_regex("^/api/v3/series/9$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rhony_written.clone()))
        .mount(&server)
        .await;
    mount(&server, "PUT", "/api/v3/series/9", rhony_written).await;
    mount_quiet_activity(&server).await;
    mount(&server, "POST", "/api/v3/command", json!({"id": 77})).await;

    let mut h = common::Harness::new().await;
    h.services.seasons =
        common::sonarr_seasons(&h.app, config(&server.uri()), SideEffectMode::Live);
    let tools = media_tools(Arc::new(h.services.clone())).expect("tools");

    let rhom_out = call(
        &tools,
        "media_series_add",
        json!({"tmdbId": RHOM_TMDB, "seasons": [1, 2, 3], "search": true}),
    )
    .await;
    assert_eq!(rhom_out["result"], "added");
    assert_eq!(
        rhom_out["search"],
        json!([
            {"seasonNumber": 1, "outcome": "queued_after_add", "commandId": null},
            {"seasonNumber": 2, "outcome": "queued_after_add", "commandId": null},
            {"seasonNumber": 3, "outcome": "queued_after_add", "commandId": null}
        ])
    );

    let rhony_out = call(
        &tools,
        "media_series_add",
        json!({"tmdbId": RHONY_TMDB, "seasons": [7, 8, 9], "search": true}),
    )
    .await;
    assert_eq!(rhony_out["result"], "updated");
    assert_eq!(rhony_out["changedSeasons"], json!([7, 8, 9]));
    let outcomes: Vec<&str> = rhony_out["search"]
        .as_array()
        .expect("search")
        .iter()
        .map(|s| s["outcome"].as_str().expect("outcome"))
        .collect();
    assert_eq!(outcomes, vec!["queued", "queued", "queued"]);

    // Asking again for the same seasons searches nothing a second time.
    let repeat = call(
        &tools,
        "media_series_search",
        json!({"tmdbId": RHONY_TMDB, "seasons": [7, 8, 9]}),
    )
    .await;
    assert_eq!(repeat["result"], "nothing_searched");
    assert_eq!(requests(&server, "POST", "/api/v3/command").await.len(), 3);

    let status = call(&tools, "media_series_get", json!({"tmdbId": RHONY_TMDB})).await;
    assert_eq!(status["result"], "tracked");
    assert_eq!(
        status["series"]["seasons"][1]["lastSearch"]["via"],
        "search"
    );
}

#[tokio::test]
async fn a_series_profile_change_is_previewed_written_alone_and_read_back() {
    let server = MockServer::start().await;
    let before = series(
        9,
        RHONY_TMDB,
        RHONY_TVDB,
        true,
        vec![season(1, true, 10, 10)],
    );
    let mut after = before.clone();
    after["qualityProfileId"] = json!(12);
    mount(&server, "GET", "/api/v3/series", json!([before.clone()])).await;
    // Preview, unknown profile and the update each read the series once before the write.
    for _ in 0..3 {
        mount_once(&server, "GET", "/api/v3/series/9", before.clone()).await;
    }
    mount(&server, "GET", "/api/v3/series/9", after.clone()).await;
    mount(
        &server,
        "GET",
        "/api/v3/qualityprofile",
        json!([{"id": 7, "name": "HD Quality"}, {"id": 12, "name": "4K Quality"}]),
    )
    .await;
    mount(&server, "PUT", "/api/v3/series/9", after).await;
    mount_quiet_activity(&server).await;
    let (_h, seasons) = seasons_over(&server).await;
    let request = |quality_profile_id, dry_run| SeriesProfileRequest {
        tmdb_id: RHONY_TMDB,
        quality_profile_id,
        dry_run,
    };

    let preview = seasons.set_profile(request(12, true)).await;
    assert_eq!(preview.result, SeriesResult::Preview);
    assert_eq!(preview.previous_quality_profile_id, Some(7));
    assert!(
        requests(&server, "PUT", "/api/v3/series/9")
            .await
            .is_empty()
    );

    let missing = seasons.set_profile(request(99, false)).await;
    assert_eq!(missing.result, SeriesResult::ProfileNotFound);

    let applied = seasons.set_profile(request(12, false)).await;
    assert_eq!(applied.result, SeriesResult::Updated);
    assert_eq!(applied.quality_profile, Some((12, "4K Quality".to_owned())));
    let puts = requests(&server, "PUT", "/api/v3/series/9").await;
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0]["qualityProfileId"], 12);
    assert_eq!(
        monitored_flags(&puts[0]),
        vec![(1, true)],
        "monitoring untouched"
    );
    assert_eq!(
        applied.series.expect("read back").quality_profile_id,
        Some(12)
    );
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty()
    );
}
