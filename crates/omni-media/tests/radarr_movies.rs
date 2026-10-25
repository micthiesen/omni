//! Movie-level Radarr management: inspection of files and quality, profile
//! changes, searches and grabs that never repeat, and upgrade verification
//! that demands exactly one final library file.
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use omni_http::SideEffectMode;
use omni_mcp_kit::{McpTool, ToolContext, ToolOutput};
use omni_media::arr::ArrConfig;
use omni_media::arr::movies::{
    GrabOutcome, GrabRequest, MovieResult, MovieSearchOutcome, MovieSearchRequest, MoviesQuery,
    ProfileSetRequest, RadarrMovies, SearchPurpose, UpgradeState,
};
use omni_media::mcp::media_tools;
use omni_media::types::{AddToWatchlistResult, MediaType};
use omni_media::watchlist::{ArrWatchlist, Watchlist, WatchlistAddRequest};
use omni_testkit::TEST_EPOCH_MS;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const HD: i64 = 10;
const UHD: i64 = 9;
const PHANTOM: i64 = 1893;
const CLONES: i64 = 1894;
const SOLO: i64 = 348_350;
const ROGUE: i64 = 330_459;

fn config(url: &str) -> ArrConfig {
    ArrConfig {
        url: Some(url.to_owned()),
        api_key: Some("radarr-key".to_owned()),
        root_folder_path: Some("/movies".to_owned()),
        quality_profile_id: Some(10),
    }
}

fn ids(values: &[i64]) -> BTreeSet<i64> {
    values.iter().copied().collect()
}

fn iso(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .expect("timestamp")
        .to_string()
}

fn profiles() -> Value {
    let item = |id: i64, name: &str, resolution: i64, allowed: bool| json!({"quality": {"id": id, "name": name, "resolution": resolution}, "items": [], "allowed": allowed});
    json!([
        {
            "id": UHD, "name": "4K Quality", "upgradeAllowed": true, "cutoff": 31,
            "minFormatScore": 0, "cutoffFormatScore": 10000, "minUpgradeFormatScore": 1,
            "items": [item(7, "Bluray-1080p", 1080, true), item(31, "Remux-2160p", 2160, true)],
            "formatItems": [{"format": 3, "name": "HDR", "score": 500}],
            "language": {"id": -2, "name": "Original"}
        },
        {
            "id": HD, "name": "HD Quality", "upgradeAllowed": true, "cutoff": 7,
            "items": [item(7, "Bluray-1080p", 1080, true), item(31, "Remux-2160p", 2160, false)],
            "formatItems": []
        }
    ])
}

fn file(id: i64, quality: &str, resolution: i64, cutoff_not_met: bool) -> Value {
    json!({
        "id": id, "relativePath": format!("Movie {quality}.mkv"), "size": 15_000_000_000_i64,
        "dateAdded": "2022-10-28T05:32:49Z", "sceneName": "Movie.1080p.BluRay-GRP",
        "releaseGroup": "GRP", "edition": "", "languages": [{"id": 1, "name": "English"}],
        "quality": {"quality": {"id": 7, "name": quality, "source": "bluray", "resolution": resolution}},
        "mediaInfo": {
            "audioChannels": 7.1, "audioCodec": "EAC3 Atmos", "audioLanguages": "eng",
            "videoBitDepth": 10, "videoCodec": "x265", "videoDynamicRange": "HDR",
            "videoDynamicRangeType": "HDR10", "resolution": "1920x804", "runTime": "2:16:11"
        },
        "qualityCutoffNotMet": cutoff_not_met
    })
}

struct Movie {
    id: i64,
    tmdb: i64,
    title: &'static str,
    collection: Option<&'static str>,
    profile: i64,
    monitored: bool,
    file: Option<Value>,
}

impl Movie {
    fn json(&self) -> Value {
        let mut movie = json!({
            "id": self.id, "title": self.title, "year": 1999, "tmdbId": self.tmdb,
            "imdbId": format!("tt{}", self.tmdb), "monitored": self.monitored,
            "qualityProfileId": self.profile, "hasFile": self.file.is_some(),
            "isAvailable": true, "path": format!("/movies/{}", self.title),
            "statistics": {"movieFileCount": i64::from(self.file.is_some())},
        });
        if let Some(collection) = self.collection {
            movie["collection"] = json!({"title": collection, "tmdbId": 10});
        }
        if let Some(file) = &self.file {
            movie["movieFile"] = file.clone();
            movie["movieFileId"] = file["id"].clone();
        }
        movie
    }
}

fn phantom(profile: i64, file_id: i64) -> Movie {
    Movie {
        id: 958,
        tmdb: PHANTOM,
        title: "Star Wars: Episode I - The Phantom Menace",
        collection: Some("Star Wars Collection"),
        profile,
        monitored: true,
        file: Some(file(file_id, "Bluray-1080p", 1080, profile == UHD)),
    }
}

fn library() -> Vec<Movie> {
    vec![
        phantom(UHD, 964),
        Movie {
            id: 959,
            tmdb: CLONES,
            title: "Star Wars: Episode II - Attack of the Clones",
            collection: Some("Star Wars Collection"),
            profile: UHD,
            monitored: true,
            file: Some(file(965, "Remux-2160p", 2160, false)),
        },
        Movie {
            id: 961,
            tmdb: SOLO,
            title: "Solo: A Star Wars Story",
            collection: Some("Star Wars Anthology"),
            profile: HD,
            monitored: true,
            file: None,
        },
        Movie {
            id: 1210,
            tmdb: ROGUE,
            title: "Rogue One: A Star Wars Story",
            collection: Some("Star Wars Anthology"),
            profile: HD,
            monitored: false,
            file: Some(file(970, "Bluray-1080p", 1080, false)),
        },
        Movie {
            id: 3,
            tmdb: 290_250,
            title: "The Nice Guys",
            collection: None,
            profile: HD,
            monitored: true,
            file: Some(file(672, "Bluray-1080p", 1080, false)),
        },
    ]
}

fn movies_json(movies: &[Movie]) -> Value {
    Value::Array(movies.iter().map(Movie::json).collect())
}

async fn mount(server: &MockServer, verb: &str, route: &str, body: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// A Radarr with `movies`, both profiles, an empty queue, no commands and no history.
async fn radarr(movies: &[Movie]) -> MockServer {
    let server = MockServer::start().await;
    mount(&server, "GET", "/api/v3/movie", movies_json(movies)).await;
    for movie in movies {
        mount(
            &server,
            "GET",
            &format!("/api/v3/movie/{}", movie.id),
            movie.json(),
        )
        .await;
    }
    mount(&server, "GET", "/api/v3/qualityprofile", profiles()).await;
    mount(&server, "GET", "/api/v3/history/movie", json!([])).await;
    server
}

/// Overrides a [`radarr`] movie for reads after a write.
async fn written(server: &MockServer, movie: &Movie) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v3/movie/{}", movie.id)))
        .respond_with(ResponseTemplate::new(200).set_body_json(movie.json()))
        .with_priority(1)
        .mount(server)
        .await;
}

async fn quiet(server: &MockServer) {
    mount(
        server,
        "GET",
        "/api/v3/queue",
        json!({"totalRecords": 0, "records": []}),
    )
    .await;
    mount(server, "GET", "/api/v3/command", json!([])).await;
}

async fn requests(server: &MockServer, verb: &str, route: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("recording")
        .into_iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == route)
        .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
        .collect()
}

async fn movies_over(server: &MockServer) -> (common::Harness, RadarrMovies) {
    let h = common::Harness::new().await;
    let movies = common::radarr_movies(&h.app, config(&server.uri()), SideEffectMode::Live);
    (h, movies)
}

fn query(text: &str) -> MoviesQuery {
    MoviesQuery {
        tmdb_ids: None,
        query: Some(text.to_owned()),
        cutoff_unmet: false,
        missing: false,
        detail: false,
        cursor: 0,
        limit: 25,
    }
}

fn search(tmdb_ids: &[i64], purpose: SearchPurpose) -> MovieSearchRequest {
    MovieSearchRequest {
        tmdb_ids: ids(tmdb_ids),
        purpose,
        force: false,
    }
}

async fn call(tools: &[McpTool], name: &str, input: Value) -> Value {
    let tool = tools
        .iter()
        .find(|t| t.meta.name == name)
        .unwrap_or_else(|| panic!("missing tool {name}"));
    let cx = ToolContext {
        call_id: "1".to_owned(),
        cancel: CancellationToken::new(),
    };
    match tool.handler.call(input, cx).await.expect("tool call") {
        ToolOutput::Structured(map)
        | ToolOutput::Custom {
            structured: map, ..
        } => Value::Object(map),
    }
}

#[tokio::test]
async fn a_query_lists_every_matching_movie_with_its_profile_and_actual_file_quality() {
    let server = radarr(&library()).await;
    mount(
        &server,
        "GET",
        "/api/v3/queue",
        json!({"totalRecords": 1, "records": [{
            "id": 77, "movieId": 959, "title": "Clones.2160p.Remux", "status": "downloading",
            "trackedDownloadStatus": "ok", "trackedDownloadState": "downloading",
            "quality": {"quality": {"name": "Remux-2160p", "resolution": 2160}},
            "size": 60.0e9, "sizeleft": 10.0e9, "timeleft": "00:20:00", "statusMessages": []
        }]}),
    )
    .await;
    mount(&server, "GET", "/api/v3/command", json!([])).await;
    let (_h, movies) = movies_over(&server).await;

    let page = movies.get(query("star wars")).await.expect("page");

    let titles: Vec<&str> = page.items.iter().map(|m| m.title.as_str()).collect();
    assert_eq!(
        page.total, 4,
        "titles and collection names both match: {titles:?}"
    );
    assert!(!titles.contains(&"The Nice Guys"));
    let phantom = page
        .items
        .iter()
        .find(|m| m.tmdb_id == PHANTOM)
        .expect("phantom");
    assert_eq!(phantom.quality_profile.as_deref(), Some("4K Quality"));
    let file = phantom.file.as_ref().expect("file");
    assert_eq!(file.quality.as_deref(), Some("Bluray-1080p"));
    assert_eq!(file.resolution, Some(1080));
    assert_eq!(file.dynamic_range_type.as_deref(), Some("HDR10"));
    assert_eq!(file.audio_codec.as_deref(), Some("EAC3 Atmos"));
    assert_eq!(file.cutoff_not_met, Some(true));
    let solo = page.items.iter().find(|m| m.tmdb_id == SOLO).expect("solo");
    assert!(!solo.has_file);
    assert!(solo.file.is_none(), "a missing file is explicit");
    let clones = page
        .items
        .iter()
        .find(|m| m.tmdb_id == CLONES)
        .expect("clones");
    assert_eq!(clones.downloads.len(), 1);
    assert_eq!(clones.downloads[0].quality.as_deref(), Some("Remux-2160p"));

    let by_id = movies
        .get(MoviesQuery {
            tmdb_ids: Some(ids(&[PHANTOM, 42])),
            query: None,
            ..query("")
        })
        .await
        .expect("by id");
    assert_eq!(by_id.total, 1);
    assert_eq!(by_id.not_tracked, vec![42]);

    let below_cutoff = movies
        .get(MoviesQuery {
            cutoff_unmet: true,
            ..query("")
        })
        .await
        .expect("cutoff");
    let below: Vec<i64> = below_cutoff.items.iter().map(|m| m.tmdb_id).collect();
    assert_eq!(below, vec![PHANTOM]);
}

#[tokio::test]
async fn a_profile_preview_writes_nothing_and_an_update_changes_only_the_listed_movies() {
    let server = radarr(&library()).await;
    quiet(&server).await;
    mount(&server, "PUT", "/api/v3/movie/editor", json!([])).await;
    let mut solo = library().remove(2);
    solo.profile = UHD;
    written(&server, &solo).await;
    let (_h, movies) = movies_over(&server).await;

    let preview = movies
        .set_profile(ProfileSetRequest {
            tmdb_ids: ids(&[SOLO, CLONES, 42]),
            quality_profile_id: UHD,
            monitored: None,
            dry_run: true,
        })
        .await;
    assert_eq!(preview.result, MovieResult::Preview);
    assert_eq!(preview.not_tracked, vec![42]);
    let changed: Vec<(i64, bool)> = preview
        .movies
        .iter()
        .map(|c| (c.tmdb_id, c.changed))
        .collect();
    assert_eq!(changed, vec![(CLONES, false), (SOLO, true)]);
    assert_eq!(preview.movies[1].before.quality_profile_id, Some(HD));
    assert_eq!(preview.movies[1].after.quality_profile_id, Some(UHD));
    assert!(
        requests(&server, "PUT", "/api/v3/movie/editor")
            .await
            .is_empty()
    );

    let applied = movies
        .set_profile(ProfileSetRequest {
            tmdb_ids: ids(&[SOLO, CLONES]),
            quality_profile_id: UHD,
            monitored: None,
            dry_run: false,
        })
        .await;
    assert_eq!(applied.result, MovieResult::Updated);
    let puts = requests(&server, "PUT", "/api/v3/movie/editor").await;
    assert_eq!(
        puts,
        vec![json!({"movieIds": [961], "qualityProfileId": UHD, "moveFiles": false})]
    );
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty(),
        "a profile change never searches"
    );
}

#[tokio::test]
async fn a_profile_change_radarr_did_not_apply_is_an_error() {
    let server = radarr(&library()).await;
    quiet(&server).await;
    mount(&server, "PUT", "/api/v3/movie/editor", json!([])).await;
    // Read back unchanged.
    mount(
        &server,
        "GET",
        "/api/v3/movie/961",
        library().remove(2).json(),
    )
    .await;
    let (_h, movies) = movies_over(&server).await;

    let outcome = movies
        .set_profile(ProfileSetRequest {
            tmdb_ids: ids(&[SOLO]),
            quality_profile_id: UHD,
            monitored: None,
            dry_run: false,
        })
        .await;
    assert_eq!(outcome.result, MovieResult::Error);
    assert_eq!(outcome.movies[0].after.quality_profile_id, Some(HD));

    let unknown = movies
        .set_profile(ProfileSetRequest {
            tmdb_ids: ids(&[SOLO]),
            quality_profile_id: 99,
            monitored: None,
            dry_run: false,
        })
        .await;
    assert_eq!(unknown.result, MovieResult::ProfileNotFound);
}

#[tokio::test]
async fn an_upgrade_search_runs_once_and_only_for_movies_below_their_cutoff() {
    let server = radarr(&library()).await;
    quiet(&server).await;
    mount(&server, "POST", "/api/v3/command", json!({"id": 501})).await;
    let (_h, movies) = movies_over(&server).await;

    let first = movies
        .search(search(
            &[PHANTOM, CLONES, SOLO, ROGUE, 42],
            SearchPurpose::Upgrade,
        ))
        .await;
    assert_eq!(first.result, MovieResult::Searched);
    let outcomes: Vec<(i64, MovieSearchOutcome)> = first
        .movies
        .iter()
        .map(|m| (m.tmdb_id, m.outcome))
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (42, MovieSearchOutcome::NotTracked),
            (PHANTOM, MovieSearchOutcome::Queued),
            (CLONES, MovieSearchOutcome::CutoffMet),
            (ROGUE, MovieSearchOutcome::NotMonitored),
            (SOLO, MovieSearchOutcome::NoFile),
        ]
    );
    assert_eq!(first.movies[1].command_id, Some(501));
    assert_eq!(
        requests(&server, "POST", "/api/v3/command").await,
        vec![json!({"name": "MoviesSearch", "movieIds": [958]})]
    );

    let repeat = movies
        .search(search(&[PHANTOM], SearchPurpose::Upgrade))
        .await;
    assert_eq!(repeat.result, MovieResult::NothingSearched);
    assert_eq!(
        repeat.movies[0].outcome,
        MovieSearchOutcome::RecentlySearched
    );
    assert_eq!(requests(&server, "POST", "/api/v3/command").await.len(), 1);

    let missing = movies
        .search(search(&[SOLO, PHANTOM], SearchPurpose::Missing))
        .await;
    let outcomes: Vec<MovieSearchOutcome> = missing.movies.iter().map(|m| m.outcome).collect();
    assert_eq!(
        outcomes,
        vec![MovieSearchOutcome::HasFile, MovieSearchOutcome::Queued]
    );

    let forced = movies
        .search(MovieSearchRequest {
            force: true,
            ..search(&[PHANTOM], SearchPurpose::Upgrade)
        })
        .await;
    assert_eq!(forced.movies[0].outcome, MovieSearchOutcome::Queued);
    assert_eq!(requests(&server, "POST", "/api/v3/command").await.len(), 3);
}

#[tokio::test]
async fn a_download_a_running_search_or_a_recent_grab_blocks_a_search_even_when_forced() {
    let server = MockServer::start().await;
    mount(&server, "GET", "/api/v3/movie", movies_json(&library())).await;
    mount(&server, "GET", "/api/v3/qualityprofile", profiles()).await;
    mount(
        &server,
        "GET",
        "/api/v3/queue",
        json!({"records": [{"id": 1, "movieId": 958, "status": "downloading"}]}),
    )
    .await;
    mount(
        &server,
        "GET",
        "/api/v3/command",
        json!([
            {"id": 9, "name": "MoviesSearch", "status": "started", "body": {"movieIds": [1210, 3]}},
            {"id": 8, "name": "MoviesSearch", "status": "completed", "body": {"movieIds": [961]}}
        ]),
    )
    .await;
    // Radarr grabbed for Solo two hours ago (RSS, the UI or another client).
    Mock::given(method("GET"))
        .and(path("/api/v3/history/movie"))
        .and(query_param("movieId", "961"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"eventType": "grabbed", "date": iso(TEST_EPOCH_MS - 2 * 3_600_000),
             "sourceTitle": "Solo.2018.1080p", "data": {"downloadUrl": "https://indexer/?apikey=secret"}}
        ])))
        .mount(&server)
        .await;
    mount(&server, "GET", "/api/v3/history/movie", json!([])).await;
    mount(&server, "POST", "/api/v3/command", json!({"id": 1})).await;
    let (_h, movies) = movies_over(&server).await;

    let blocked = movies
        .search(search(&[PHANTOM, SOLO, 290_250], SearchPurpose::Missing))
        .await;
    let outcomes: Vec<(i64, MovieSearchOutcome)> = blocked
        .movies
        .iter()
        .map(|m| (m.tmdb_id, m.outcome))
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (PHANTOM, MovieSearchOutcome::HasFile),
            (290_250, MovieSearchOutcome::HasFile),
            (SOLO, MovieSearchOutcome::RecentlyGrabbed),
        ]
    );
    let upgrade = movies
        .search(MovieSearchRequest {
            force: true,
            ..search(&[PHANTOM], SearchPurpose::Upgrade)
        })
        .await;
    assert_eq!(upgrade.movies[0].outcome, MovieSearchOutcome::Downloading);
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty()
    );

    // Forcing past the history check still searches Solo exactly once.
    let forced = movies
        .search(MovieSearchRequest {
            force: true,
            ..search(&[SOLO], SearchPurpose::Missing)
        })
        .await;
    assert_eq!(forced.movies[0].outcome, MovieSearchOutcome::Queued);
    assert_eq!(requests(&server, "POST", "/api/v3/command").await.len(), 1);
}

#[tokio::test]
async fn a_rejected_search_releases_its_reservation_and_an_uncertain_one_keeps_it() {
    let server = radarr(&library()).await;
    quiet(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v3/command"))
        .respond_with(ResponseTemplate::new(400))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v3/command"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let (_h, movies) = movies_over(&server).await;

    let rejected = movies
        .search(search(&[PHANTOM], SearchPurpose::Upgrade))
        .await;
    assert_eq!(rejected.movies[0].outcome, MovieSearchOutcome::Failed);
    let uncertain = movies
        .search(search(&[PHANTOM], SearchPurpose::Upgrade))
        .await;
    assert_eq!(uncertain.movies[0].outcome, MovieSearchOutcome::Uncertain);
    assert_eq!(uncertain.result, MovieResult::Searched);
    let after = movies
        .search(search(&[PHANTOM], SearchPurpose::Upgrade))
        .await;
    assert_eq!(
        after.movies[0].outcome,
        MovieSearchOutcome::RecentlySearched
    );
    assert_eq!(requests(&server, "POST", "/api/v3/command").await.len(), 2);
}

#[tokio::test]
async fn a_movie_added_through_the_watchlist_is_not_searched_again() {
    let server = MockServer::start().await;
    let before: Vec<Movie> = library().into_iter().filter(|m| m.tmdb != SOLO).collect();
    Mock::given(method("GET"))
        .and(path("/api/v3/movie"))
        .respond_with(ResponseTemplate::new(200).set_body_json(movies_json(&before)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(&server, "GET", "/api/v3/movie", movies_json(&library())).await;
    mount(
        &server,
        "GET",
        "/api/v3/movie/lookup/tmdb",
        json!({"title": "Solo: A Star Wars Story", "year": 2018, "tmdbId": SOLO}),
    )
    .await;
    mount(&server, "POST", "/api/v3/movie", json!({"id": 961})).await;
    mount(&server, "GET", "/api/v3/qualityprofile", profiles()).await;
    mount(&server, "GET", "/api/v3/history/movie", json!([])).await;
    quiet(&server).await;
    let (h, movies) = movies_over(&server).await;
    let watchlist = ArrWatchlist::new(
        omni_media::arr::ArrHttp::new(omni_testkit::no_network(), SideEffectMode::Live),
        config(&server.uri()),
        ArrConfig::default(),
        common::sonarr_seasons(&h.app, ArrConfig::default(), SideEffectMode::Live),
        movies.clone(),
    );

    let added = watchlist
        .add(&WatchlistAddRequest {
            tmdb_id: SOLO,
            media_type: MediaType::Movie,
            title: "Solo".to_owned(),
            year: None,
            external_ids: None,
        })
        .await;
    assert_eq!(added.result, AddToWatchlistResult::Added);
    assert_eq!(
        requests(&server, "POST", "/api/v3/movie").await[0]["addOptions"],
        json!({"searchForMovie": true})
    );

    let again = movies.search(search(&[SOLO], SearchPurpose::Missing)).await;
    assert_eq!(
        again.movies[0].outcome,
        MovieSearchOutcome::RecentlySearched
    );
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_post_add_search_radarr_has_not_run_blocks_searches_and_grabs() {
    let mut movie = library().remove(2);
    movie.monitored = true;
    let mut raw = movie.json();
    raw["addOptions"] = json!({"searchForMovie": true});
    let server = MockServer::start().await;
    mount(&server, "GET", "/api/v3/movie", json!([raw])).await;
    mount(&server, "GET", "/api/v3/qualityprofile", profiles()).await;
    mount(&server, "GET", "/api/v3/history/movie", json!([])).await;
    quiet(&server).await;
    let (_h, movies) = movies_over(&server).await;

    let searched = movies.search(search(&[SOLO], SearchPurpose::Missing)).await;
    assert_eq!(
        searched.movies[0].outcome,
        MovieSearchOutcome::InitialSearchPending
    );
    let grabbed = movies
        .grab(GrabRequest {
            tmdb_id: SOLO,
            release_id: release_id("g"),
            force: true,
        })
        .await;
    assert_eq!(grabbed.outcome, GrabOutcome::InitialSearchPending);
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty()
    );
    assert!(
        requests(&server, "POST", "/api/v3/release")
            .await
            .is_empty()
    );
}

/// The release id Omni derives for `guid` on indexer 8.
fn release_id(guid: &str) -> String {
    omni_core::digest::sha256_hex(format!("8\n{guid}"))[..32].to_owned()
}

/// A torrent indexer's guid can be its download link, API key included.
const KEYED_GUID: &str = "https://indexer.example/download/1?apikey=secret";

fn release(guid: &str, quality: &str, approved: bool, rejections: &[&str]) -> Value {
    json!({
        "guid": guid, "indexerId": 8, "indexer": "Indexer (Prowlarr)", "title": format!("Phantom.{quality}"),
        "size": 65.0e9, "age": 2397, "protocol": "usenet",
        "quality": {"quality": {"name": quality, "resolution": 2160}},
        "customFormats": [{"id": 3, "name": "HDR"}], "customFormatScore": 3500,
        "languages": [{"id": 1, "name": "English"}], "releaseGroup": "GRP",
        "approved": approved, "rejected": !approved, "temporarilyRejected": false,
        "downloadAllowed": true, "rejections": rejections,
        "downloadUrl": "https://indexer/download?apikey=secret", "infoUrl": "https://indexer/details"
    })
}

#[tokio::test]
async fn releases_list_approved_first_and_a_grab_takes_only_an_approved_release_once() {
    let server = radarr(&library()).await;
    quiet(&server).await;
    mount(
        &server,
        "GET",
        "/api/v3/release",
        json!([
            release("rejected-1", "Remux-2160p", false, &["Not an upgrade"]),
            release(KEYED_GUID, "Remux-2160p", true, &[]),
            release("second", "Bluray-2160p", true, &[]),
        ]),
    )
    .await;
    mount(&server, "POST", "/api/v3/release", json!({})).await;
    let (_h, movies) = movies_over(&server).await;

    let listed = movies.releases(PHANTOM).await;
    assert_eq!(listed.result, MovieResult::Listed);
    let ids: Vec<&str> = listed
        .releases
        .iter()
        .map(|r| r.release_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec![
            release_id(KEYED_GUID),
            release_id("second"),
            release_id("rejected-1")
        ]
    );
    assert_eq!((listed.total, listed.approved), (3, 2));
    assert_eq!(listed.releases[2].rejections, vec!["Not an upgrade"]);
    let wire = serde_json::to_string(&listed).expect("json");
    assert!(
        !wire.contains("apikey") && !wire.contains("indexer.example"),
        "guids and indexer URLs never leave Omni"
    );

    let grab = |guid: &str| GrabRequest {
        tmdb_id: PHANTOM,
        release_id: release_id(guid),
        force: false,
    };
    assert_eq!(
        movies.grab(grab("rejected-1")).await.outcome,
        GrabOutcome::ReleaseRejected
    );
    assert_eq!(
        movies.grab(grab("gone")).await.outcome,
        GrabOutcome::ReleaseNotFound
    );
    assert!(
        requests(&server, "POST", "/api/v3/release")
            .await
            .is_empty()
    );

    let grabbed = movies.grab(grab(KEYED_GUID)).await;
    assert_eq!(grabbed.outcome, GrabOutcome::Grabbed);
    // Only Radarr gets the raw guid back.
    assert_eq!(
        requests(&server, "POST", "/api/v3/release").await,
        vec![json!({"guid": KEYED_GUID, "indexerId": 8, "movieId": 958})]
    );

    assert_eq!(
        movies.grab(grab("second")).await.outcome,
        GrabOutcome::RecentlyGrabbed
    );
    let search_after = movies
        .search(search(&[PHANTOM], SearchPurpose::Upgrade))
        .await;
    assert_eq!(
        search_after.movies[0].outcome,
        MovieSearchOutcome::RecentlyGrabbed
    );
    assert_eq!(requests(&server, "POST", "/api/v3/release").await.len(), 1);
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_search_that_found_nothing_does_not_block_picking_a_release_by_hand() {
    let server = radarr(&library()).await;
    quiet(&server).await;
    mount(&server, "POST", "/api/v3/command", json!({"id": 3})).await;
    mount(
        &server,
        "GET",
        "/api/v3/release",
        json!([release("best", "Remux-2160p", true, &[])]),
    )
    .await;
    mount(&server, "POST", "/api/v3/release", json!({})).await;
    let (_h, movies) = movies_over(&server).await;

    let searched = movies
        .search(search(&[PHANTOM], SearchPurpose::Upgrade))
        .await;
    assert_eq!(searched.movies[0].outcome, MovieSearchOutcome::Queued);
    let grabbed = movies
        .grab(GrabRequest {
            tmdb_id: PHANTOM,
            release_id: release_id("best"),
            force: false,
        })
        .await;
    assert_eq!(grabbed.outcome, GrabOutcome::Grabbed);
}

#[tokio::test]
async fn a_search_whose_outcome_is_uncertain_blocks_a_grab() {
    let server = radarr(&library()).await;
    quiet(&server).await;
    mount(
        &server,
        "GET",
        "/api/v3/release",
        json!([release("best", "Remux-2160p", true, &[])]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/v3/command"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let (_h, movies) = movies_over(&server).await;

    let searched = movies
        .search(search(&[PHANTOM], SearchPurpose::Upgrade))
        .await;
    assert_eq!(searched.movies[0].outcome, MovieSearchOutcome::Uncertain);
    let grabbed = movies
        .grab(GrabRequest {
            tmdb_id: PHANTOM,
            release_id: release_id("best"),
            force: false,
        })
        .await;
    assert_eq!(grabbed.outcome, GrabOutcome::RecentlySearched);
    assert!(
        requests(&server, "POST", "/api/v3/release")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_grab_by_someone_else_during_the_indexer_search_stops_the_grab() {
    let server = MockServer::start().await;
    mount(&server, "GET", "/api/v3/movie", movies_json(&library())).await;
    mount(
        &server,
        "GET",
        "/api/v3/movie/958",
        phantom(UHD, 964).json(),
    )
    .await;
    mount(&server, "GET", "/api/v3/qualityprofile", profiles()).await;
    mount(&server, "GET", "/api/v3/history/movie", json!([])).await;
    mount(&server, "GET", "/api/v3/command", json!([])).await;
    // Empty before the indexer search; RSS has grabbed by the time it ends.
    Mock::given(method("GET"))
        .and(path("/api/v3/queue"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"totalRecords": 0, "records": []})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        "GET",
        "/api/v3/queue",
        json!({"totalRecords": 1, "records": [{"id": 1, "movieId": 958, "status": "downloading"}]}),
    )
    .await;
    mount(
        &server,
        "GET",
        "/api/v3/release",
        json!([release("best", "Remux-2160p", true, &[])]),
    )
    .await;
    mount(&server, "POST", "/api/v3/release", json!({})).await;
    let (_h, movies) = movies_over(&server).await;

    let grabbed = movies
        .grab(GrabRequest {
            tmdb_id: PHANTOM,
            release_id: release_id("best"),
            force: true,
        })
        .await;
    assert_eq!(grabbed.outcome, GrabOutcome::Downloading);
    assert!(
        requests(&server, "POST", "/api/v3/release")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_queue_too_long_to_read_whole_blocks_searches() {
    let server = radarr(&library()).await;
    mount(
        &server,
        "GET",
        "/api/v3/queue",
        json!({"totalRecords": 501, "records": [{"id": 1, "movieId": 3}]}),
    )
    .await;
    mount(&server, "GET", "/api/v3/command", json!([])).await;
    let (_h, movies) = movies_over(&server).await;

    let searched = movies
        .search(search(&[PHANTOM], SearchPurpose::Upgrade))
        .await;
    assert_eq!(searched.result, MovieResult::Unavailable);
    assert!(
        requests(&server, "POST", "/api/v3/command")
            .await
            .is_empty()
    );
}

/// Radarr after an upgrade search: the movie list returns `after`.
async fn upgraded(
    after: Movie,
    managed: Value,
    unmanaged: Value,
) -> (MockServer, RadarrMovies, common::Harness) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/movie"))
        .respond_with(ResponseTemplate::new(200).set_body_json(movies_json(&[phantom(UHD, 964)])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(&server, "GET", "/api/v3/movie", movies_json(&[after])).await;
    mount(&server, "GET", "/api/v3/qualityprofile", profiles()).await;
    mount(&server, "GET", "/api/v3/history/movie", json!([])).await;
    mount(&server, "GET", "/api/v3/moviefile", managed).await;
    mount(&server, "GET", "/api/v3/manualimport", unmanaged).await;
    mount(&server, "POST", "/api/v3/command", json!({"id": 5})).await;
    quiet(&server).await;
    let (h, movies) = Box::pin(movies_over(&server)).await;
    let searched = Box::pin(movies.search(search(&[PHANTOM], SearchPurpose::Upgrade))).await;
    assert_eq!(searched.movies[0].outcome, MovieSearchOutcome::Queued);
    (server, movies, h)
}

fn detail() -> MoviesQuery {
    MoviesQuery {
        tmdb_ids: Some(ids(&[PHANTOM])),
        query: None,
        detail: true,
        ..query("")
    }
}

#[tokio::test]
async fn an_upgrade_is_verified_only_when_the_new_file_is_the_only_one_in_the_library() {
    let mut after = phantom(UHD, 1200);
    after.file = Some(file(1200, "Remux-2160p", 2160, false));
    let (server, movies, _h) = upgraded(
        after,
        json!([file(1200, "Remux-2160p", 2160, false)]),
        json!([]),
    )
    .await;

    let page = movies.get(detail()).await.expect("page");
    let movie = &page.items[0];
    let upgrade = movie.upgrade.as_ref().expect("upgrade");
    assert_eq!(upgrade.state, UpgradeState::Verified);
    assert_eq!(
        (upgrade.previous_file_id, upgrade.current_file_id),
        (964, Some(1200))
    );
    let files = movie.library_files.as_ref().expect("files");
    assert!(files.exactly_one_file);
    let scans: Vec<String> = server
        .received_requests()
        .await
        .expect("recording")
        .into_iter()
        .filter(|r| r.url.path() == "/api/v3/manualimport")
        .map(|r| r.url.query().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        scans,
        vec![
            "folder=%2Fmovies%2FStar%20Wars%3A%20Episode%20I%20-%20The%20Phantom%20Menace&filterExistingFiles=true"
        ]
    );
}

#[tokio::test]
async fn an_old_file_left_beside_the_upgrade_keeps_it_incomplete() {
    let mut after = phantom(UHD, 1200);
    after.file = Some(file(1200, "Remux-2160p", 2160, false));
    let (_server, movies, _h) = upgraded(
        after,
        json!([file(1200, "Remux-2160p", 2160, false)]),
        json!([{
            "path": "/movies/Phantom/Phantom Bluray-1080p.mkv", "size": 15.0e9,
            "quality": {"quality": {"name": "Bluray-1080p"}},
            "rejections": [{"reason": "Not an upgrade for existing movie file", "type": "permanent"}]
        }]),
    )
    .await;

    let page = movies.get(detail()).await.expect("page");
    let movie = &page.items[0];
    assert_eq!(
        movie.upgrade.as_ref().expect("upgrade").state,
        UpgradeState::ExtraFiles
    );
    let files = movie.library_files.as_ref().expect("files");
    assert!(!files.exactly_one_file);
    let extra = &files.unmanaged.as_ref().expect("scanned")[0];
    assert_eq!(
        extra.rejections,
        vec!["Not an upgrade for existing movie file"]
    );
}

#[tokio::test]
async fn an_upgrade_still_waiting_keeps_the_original_file_and_reports_pending() {
    let (_server, movies, _h) = upgraded(
        phantom(UHD, 964),
        json!([file(964, "Bluray-1080p", 1080, true)]),
        json!([]),
    )
    .await;
    let page = movies.get(detail()).await.expect("page");
    let movie = &page.items[0];
    assert_eq!(
        movie.upgrade.as_ref().expect("upgrade").state,
        UpgradeState::Pending
    );
    assert_eq!(movie.file.as_ref().and_then(|f| f.id), Some(964));
}

#[tokio::test]
async fn record_mode_records_profile_changes_searches_and_grabs_without_sending() {
    let server = radarr(&library()).await;
    quiet(&server).await;
    mount(
        &server,
        "GET",
        "/api/v3/release",
        json!([release("best", "Remux-2160p", true, &[])]),
    )
    .await;
    let h = common::Harness::new().await;
    let movies = common::radarr_movies(&h.app, config(&server.uri()), SideEffectMode::Record);

    let set = movies
        .set_profile(ProfileSetRequest {
            tmdb_ids: ids(&[SOLO]),
            quality_profile_id: UHD,
            monitored: Some(true),
            dry_run: false,
        })
        .await;
    assert_eq!(set.result, MovieResult::Updated);
    let searched = movies
        .search(search(&[PHANTOM], SearchPurpose::Upgrade))
        .await;
    assert_eq!(searched.movies[0].outcome, MovieSearchOutcome::Queued);
    let grabbed = movies
        .grab(GrabRequest {
            tmdb_id: PHANTOM,
            release_id: release_id("best"),
            force: false,
        })
        .await;
    assert_eq!(grabbed.outcome, GrabOutcome::Grabbed);
    for (verb, route) in [
        ("PUT", "/api/v3/movie/editor"),
        ("POST", "/api/v3/command"),
        ("POST", "/api/v3/release"),
    ] {
        assert!(
            requests(&server, verb, route).await.is_empty(),
            "{verb} {route}"
        );
    }
}

#[tokio::test]
async fn the_movie_and_shared_arr_tools_validate_against_their_schemas() {
    let server = radarr(&library()).await;
    mount(
        &server,
        "GET",
        "/api/v3/queue",
        json!({"totalRecords": 1, "records": [{
            "id": 7, "movieId": 959, "title": "Clones.2160p", "status": "warning",
            "trackedDownloadStatus": "warning", "trackedDownloadState": "importBlocked",
            "statusMessages": [{"title": "Clones.mkv", "messages": ["Not an upgrade"]}],
            "movie": {"title": "Star Wars: Episode II", "tmdbId": CLONES}
        }]}),
    )
    .await;
    mount(&server, "GET", "/api/v3/command", json!([])).await;
    mount(&server, "POST", "/api/v3/command", json!({"id": 11})).await;
    mount(
        &server,
        "GET",
        "/api/v3/moviefile",
        json!([file(964, "Bluray-1080p", 1080, true)]),
    )
    .await;
    mount(&server, "GET", "/api/v3/manualimport", json!([])).await;
    mount(
        &server,
        "GET",
        "/api/v3/system/status",
        json!({"version": "6.4.4.10685"}),
    )
    .await;
    mount(&server, "GET", "/api/v3/health", json!([{"source": "AllowedHostsCheck", "type": "warning", "message": "Allowed Hosts is not configured"}])).await;
    mount(
        &server,
        "GET",
        "/api/v3/rootfolder",
        json!([{"path": "/movies", "accessible": true, "freeSpace": 27_822_028_357_632_i64}]),
    )
    .await;
    mount(&server, "GET", "/api/v3/config/mediamanagement", json!({"recycleBin": "", "copyUsingHardlinks": true, "downloadPropersAndRepacks": "doNotPrefer", "minimumFreeSpaceWhenImporting": 100})).await;
    mount(&server, "GET", "/api/v3/downloadclient", json!([{"name": "NZBGet", "implementation": "Nzbget", "protocol": "usenet", "enable": true, "removeCompletedDownloads": true, "removeFailedDownloads": true, "fields": [{"name": "password", "value": "secret"}]}])).await;
    mount(&server, "GET", "/api/v3/collection", json!([{
        "title": "Star Wars Collection", "tmdbId": 10, "monitored": false,
        "movies": [
            {"tmdbId": PHANTOM, "title": "Star Wars: Episode I", "year": 1999, "isExisting": true, "isExcluded": false},
            {"tmdbId": 140_607, "title": "The Force Awakens", "year": 2015, "isExisting": false, "isExcluded": false}
        ]
    }])).await;
    mount(
        &server,
        "GET",
        "/api/v3/release",
        json!([release("best", "Remux-2160p", true, &[])]),
    )
    .await;
    mount(&server, "POST", "/api/v3/release", json!({})).await;
    mount(&server, "PUT", "/api/v3/movie/editor", json!([])).await;
    let mut solo = library().remove(2);
    solo.profile = UHD;
    written(&server, &solo).await;

    let mut h = common::Harness::new().await;
    h.services.movies = common::radarr_movies(&h.app, config(&server.uri()), SideEffectMode::Live);
    h.services.arr = common::arr_inspect(config(&server.uri()), ArrConfig::default());
    let tools = media_tools(Arc::new(h.services.clone())).expect("tools");

    let profiles = call(&tools, "media_arr_profiles", json!({"service": "radarr"})).await;
    let names: Vec<&str> = profiles["profiles"]
        .as_array()
        .expect("profiles")
        .iter()
        .map(|p| p["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["4K Quality", "HD Quality"]);
    assert_eq!(profiles["profiles"][1]["isDefault"], true);
    assert_eq!(profiles["profiles"][0]["maxResolution"], 2160);

    let status = call(&tools, "media_arr_status", json!({"service": "radarr"})).await;
    assert_eq!(status["version"], "6.4.4.10685");
    assert_eq!(status["importSettings"]["recycleBin"], Value::Null);
    assert!(
        !status.to_string().contains("secret"),
        "client fields never leave Radarr"
    );

    let downloads = call(&tools, "media_downloads_list", json!({"service": "radarr"})).await;
    assert_eq!(
        downloads["items"][0]["trackedDownloadState"],
        "importBlocked"
    );
    assert_eq!(
        downloads["items"][0]["messages"],
        json!(["Clones.mkv: Not an upgrade"])
    );
    assert_eq!(downloads["items"][0]["tmdbId"], CLONES);

    let got = call(
        &tools,
        "media_movies_get",
        json!({"query": "Star Wars", "detail": true}),
    )
    .await;
    assert_eq!(got["total"], 4);
    assert_eq!(got["items"][0]["libraryFiles"]["managed"][0]["id"], 964);

    let collections = call(
        &tools,
        "media_movie_collections",
        json!({"query": "force awakens"}),
    )
    .await;
    assert_eq!(collections["items"][0]["untracked"], 1);

    let preview = call(
        &tools,
        "media_movies_profile_set",
        json!({"tmdbIds": [SOLO], "qualityProfileId": UHD, "dryRun": true}),
    )
    .await;
    assert_eq!(preview["result"], "preview");
    let updated = call(
        &tools,
        "media_movies_profile_set",
        json!({"tmdbIds": [SOLO], "qualityProfileId": UHD}),
    )
    .await;
    assert_eq!(updated["result"], "updated");

    let searched = call(
        &tools,
        "media_movies_search",
        json!({"tmdbIds": [PHANTOM], "purpose": "upgrade"}),
    )
    .await;
    assert_eq!(searched["movies"][0]["outcome"], "queued");
    assert_eq!(searched["movies"][0]["commandId"], 11);

    let movie = call(&tools, "media_movies_get", json!({"tmdbIds": [PHANTOM]})).await;
    assert_eq!(movie["items"][0]["lastSearch"]["purpose"], "upgrade");
    assert_eq!(movie["items"][0]["upgrade"]["state"], "pending");

    let releases = call(&tools, "media_movie_releases", json!({"tmdbId": CLONES})).await;
    assert_eq!(releases["releases"][0]["releaseId"], release_id("best"));
    let grabbed = call(
        &tools,
        "media_movie_release_grab",
        json!({"tmdbId": SOLO, "releaseId": release_id("best")}),
    )
    .await;
    assert_eq!(grabbed["outcome"], "grabbed");
}
