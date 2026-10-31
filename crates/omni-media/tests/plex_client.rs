//! The Plex client over a closure-backed `PlexGet`.
#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use omni_media::plex::{PlexClient, PlexGet, PlexMetadata, PlexParams, parse_external_ids};
use omni_media::types::{ExternalIds, InProgressItem, MediaItem, MediaType, WatchedItem};
use serde_json::{Value, json};

type Handler = dyn Fn(&str, &PlexParams) -> Result<Value, String> + Send + Sync;

#[derive(Clone)]
struct FnGet {
    handler: Arc<Handler>,
    calls: Arc<Mutex<Vec<(String, PlexParams)>>>,
}

impl FnGet {
    fn new(
        handler: impl Fn(&str, &PlexParams) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            handler: Arc::new(handler),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn calls(&self) -> Vec<(String, PlexParams)> {
        self.calls.lock().expect("lock").clone()
    }
}

impl PlexGet for FnGet {
    fn get<'a>(
        &'a self,
        path: &'a str,
        params: PlexParams,
    ) -> BoxFuture<'a, Result<Value, String>> {
        let result = (self.handler)(path, &params);
        self.calls
            .lock()
            .expect("lock")
            .push((path.to_owned(), params));
        Box::pin(async move { result })
    }
}

fn response(container: Value) -> Value {
    json!({ "MediaContainer": container })
}

fn param<'a>(params: &'a PlexParams, key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn client(get: &FnGet, account: Option<u64>) -> PlexClient {
    PlexClient::new(Arc::new(get.clone()), account)
}

#[test]
fn parses_modern_and_legacy_external_guids() {
    let metadata: PlexMetadata = serde_json::from_value(json!({
        "guid": "com.plexapp.agents.imdb://tt1234567?lang=en",
        "Guid": [{"id": "tmdb://42"}, {"id": "tvdb://99"}]
    }))
    .expect("metadata");
    assert_eq!(
        parse_external_ids(&metadata),
        Some(ExternalIds {
            imdb: Some("tt1234567".to_owned()),
            tmdb: Some(42),
            tvdb: Some(99),
        })
    );
}

#[tokio::test]
async fn paginates_history_and_aggregates_episodes_at_series_level() {
    let get = FnGet::new(|path, params| {
        if path == "/library/metadata/77" {
            return Ok(response(json!({"Metadata": [{
                "type": "show", "ratingKey": "77", "guid": "plex://show/show-id", "title": "A Show",
                "year": 2020, "leafCount": 10, "viewedLeafCount": 2,
                "Guid": [{"id": "tmdb://700"}, {"id": "tvdb://800"}]
            }]})));
        }
        if param(params, "X-Plex-Container-Start") == Some("0") {
            return Ok(response(json!({"totalSize": 2, "Metadata": [
                {"type": "episode", "ratingKey": "701", "grandparentKey": "/library/metadata/77",
                 "grandparentTitle": "A Show", "duration": 100, "viewOffset": 80, "viewedAt": 1000, "viewCount": 2},
                {"type": "episode", "ratingKey": "702", "grandparentKey": "/library/metadata/77",
                 "grandparentTitle": "A Show", "duration": 100, "viewOffset": 100, "viewedAt": 2000}
            ]})));
        }
        Err(format!("unexpected request {path}"))
    });
    let history = client(&get, None)
        .fetch_watch_history()
        .await
        .expect("history");
    assert_eq!(
        history,
        vec![WatchedItem {
            item: MediaItem {
                guid: "plex://show/show-id".to_owned(),
                title: "A Show".to_owned(),
                year: Some(2020),
                media_type: MediaType::Tv,
                external_ids: Some(ExternalIds {
                    tmdb: Some(700),
                    tvdb: Some(800),
                    imdb: None,
                }),
                title_slug: None,
            },
            viewed_at: 2_000_000,
            view_count: 1,
            completion: Some(0.2),
        }]
    );
    assert!(get.calls().contains(&(
        "/library/metadata/77".to_owned(),
        vec![("includeGuids".to_owned(), "1".to_owned())]
    )));
}

#[tokio::test]
async fn normalizes_movies_and_keeps_only_partial_continue_watching_items() {
    let get = FnGet::new(|path, _| {
        assert_eq!(path, "/hubs/home/continueWatching");
        Ok(response(json!({"Metadata": [
            {"type": "movie", "guid": "plex://movie/one", "title": "Movie One", "year": 2024,
             "duration": 1000, "viewOffset": 250, "lastViewedAt": 1234,
             "Guid": [{"id": "tmdb://12"}, {"id": "imdb://tt0000012"}]},
            {"type": "movie", "guid": "plex://movie/done", "title": "Done", "duration": 100, "viewOffset": 100}
        ]})))
    });
    assert_eq!(
        client(&get, None)
            .fetch_in_progress()
            .await
            .expect("in progress"),
        vec![InProgressItem {
            item: MediaItem {
                guid: "plex://movie/one".to_owned(),
                title: "Movie One".to_owned(),
                year: Some(2024),
                media_type: MediaType::Movie,
                external_ids: Some(ExternalIds {
                    tmdb: Some(12),
                    imdb: Some("tt0000012".to_owned()),
                    tvdb: None,
                }),
                title_slug: None,
            },
            progress: 0.25,
            last_viewed_at: 1_234_000,
        }]
    );
}

#[tokio::test]
async fn enriches_history_movies_through_bounded_metadata_detail_lookups() {
    let get = FnGet::new(|path, _| match path {
        "/status/sessions/history/all" => Ok(response(json!({"totalSize": 1, "Metadata": [
            {"type": "movie", "ratingKey": "9", "guid": "plex://movie/nine", "title": "Nine", "viewedAt": 100, "viewCount": 1}
        ]}))),
        "/library/metadata/9" => Ok(response(json!({"Metadata": [
            {"type": "movie", "ratingKey": "9", "guid": "plex://movie/nine", "title": "Nine", "Guid": [{"id": "tmdb://99"}]}
        ]}))),
        other => Err(format!("unexpected request {other}")),
    });
    let history = client(&get, None)
        .fetch_watch_history()
        .await
        .expect("history");
    assert_eq!(
        history[0].item.external_ids,
        Some(ExternalIds {
            tmdb: Some(99),
            ..ExternalIds::default()
        })
    );
}

#[tokio::test]
async fn loads_movie_and_show_sections_into_one_library_index() {
    let get = FnGet::new(|path, _| match path {
        "/library/sections" => Ok(response(json!({"Directory": [
            {"key": "1", "type": "movie"}, {"key": "2", "type": "show"}, {"key": "3", "type": "artist"}
        ]}))),
        "/library/sections/1/all" => Ok(response(json!({"Metadata": [
            {"type": "movie", "guid": "plex://movie/a", "title": "A", "Guid": [{"id": "tmdb://1"}]}
        ]}))),
        "/library/sections/2/all" => Ok(response(json!({"Metadata": [
            {"type": "show", "guid": "plex://show/b", "title": "B", "Guid": [{"id": "tvdb://2"}]}
        ]}))),
        other => Err(format!("unexpected request {other}")),
    });
    assert_eq!(
        client(&get, None)
            .fetch_library_index()
            .await
            .expect("library"),
        vec![
            MediaItem {
                guid: "plex://movie/a".to_owned(),
                title: "A".to_owned(),
                year: None,
                media_type: MediaType::Movie,
                external_ids: Some(ExternalIds {
                    tmdb: Some(1),
                    ..ExternalIds::default()
                }),
                title_slug: None,
            },
            MediaItem {
                guid: "plex://show/b".to_owned(),
                title: "B".to_owned(),
                year: None,
                media_type: MediaType::Tv,
                external_ids: Some(ExternalIds {
                    tvdb: Some(2),
                    ..ExternalIds::default()
                }),
                title_slug: None,
            },
        ]
    );
}

#[tokio::test]
async fn rejects_malformed_plex_responses() {
    let get = FnGet::new(|_, _| Ok(json!({})));
    let error = client(&get, None)
        .fetch_library_index()
        .await
        .expect_err("malformed");
    assert!(error.to_string().contains("MediaContainer"));
}

#[tokio::test]
async fn rejects_malformed_nested_plex_metadata_with_a_typed_integration_error() {
    let get = FnGet::new(|_, _| {
        Ok(response(
            json!({"Metadata": [{"type": "movie", "title": null}]}),
        ))
    });
    let error = client(&get, None)
        .fetch_in_progress()
        .await
        .expect_err("malformed");
    assert_eq!(error.operation, "decode Plex continue watching");
}

#[tokio::test]
async fn rejects_null_plex_containers_before_reading_nested_properties() {
    let get = FnGet::new(|_, _| Ok(json!({"MediaContainer": null})));
    let error = client(&get, None)
        .fetch_library_index()
        .await
        .expect_err("null container");
    assert_eq!(error.operation, "decode Plex library sections");
}

#[tokio::test]
async fn rejects_unscoped_history_containing_multiple_plex_accounts() {
    let get = FnGet::new(|_, _| {
        Ok(response(json!({"totalSize": 2, "Metadata": [
            {"type": "movie", "accountID": 1, "guid": "plex://movie/1", "title": "One"},
            {"type": "movie", "accountID": 2, "guid": "plex://movie/2", "title": "Two"}
        ]})))
    });
    let error = client(&get, None)
        .fetch_watch_history()
        .await
        .expect_err("ambiguous");
    assert!(error.to_string().contains("PLEX_ACCOUNT_ID"));
}

#[tokio::test]
async fn passes_the_configured_plex_account_filter_to_history_requests() {
    let get = FnGet::new(|_, _| Ok(response(json!({"totalSize": 0, "Metadata": []}))));
    client(&get, Some(7))
        .fetch_watch_history()
        .await
        .expect("history");
    let calls = get.calls();
    assert_eq!(calls[0].0, "/status/sessions/history/all");
    assert_eq!(param(&calls[0].1, "accountID"), Some("7"));
}

#[test]
fn keeps_an_earlier_id_when_a_later_guid_is_out_of_range() {
    let metadata: PlexMetadata = serde_json::from_value(json!({
        "Guid": [{"id": "tmdb://42"}, {"id": "tmdb://99999999999999999999"}]
    }))
    .expect("metadata");
    assert_eq!(
        parse_external_ids(&metadata),
        Some(ExternalIds {
            tmdb: Some(42),
            ..ExternalIds::default()
        })
    );
}

#[tokio::test]
async fn skips_history_episodes_with_an_empty_series_key() {
    let get = FnGet::new(|path, _| match path {
        "/status/sessions/history/all" => Ok(response(json!({
            "size": 1,
            "Metadata": [{
                "type": "episode",
                "grandparentRatingKey": "",
                "grandparentGuid": "plex://show/1",
                "grandparentTitle": "Show",
                "viewedAt": 10
            }]
        }))),
        other => Err(format!("unexpected {other}")),
    });
    let history = client(&get, None)
        .fetch_watch_history()
        .await
        .expect("history");
    assert!(history.is_empty());
}

#[tokio::test]
async fn lists_albums_of_the_named_music_library_only() {
    let get = FnGet::new(|path, params| match path {
        "/library/sections" => Ok(response(json!({"Directory": [
            {"key": "6", "type": "artist", "title": "Music"},
            {"key": "3", "type": "show", "title": "Audiobooks"},
            {"key": "4", "type": "artist", "title": "Audiobooks"}
        ]}))),
        "/library/sections/4/all" if param(params, "type") == Some("9") => {
            Ok(response(json!({"size": 1, "totalSize": 1, "Metadata": [
                {"type": "album", "title": "Project Hail Mary", "parentTitle": "Andy Weir",
                 "year": 2021, "addedAt": 1_700_000_000}
            ]})))
        }
        other => Err(format!("unexpected {other}")),
    });
    let plex = client(&get, None);

    let albums = plex
        .fetch_music_albums("Audiobooks")
        .await
        .expect("read")
        .expect("library");
    let missing = plex.fetch_music_albums("Podcasts").await.expect("read");

    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].parent_title.as_deref(), Some("Andy Weir"));
    assert_eq!(albums[0].added_at, Some(1_700_000_000.0));
    assert!(missing.is_none());
}
