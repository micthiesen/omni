//! Removing stuck or failed downloads through Radarr's or Sonarr's queue:
//! removed from the client and blocklisted, verified by reading the queue
//! back, recorded so a removal is never sent twice, and never resent when
//! its outcome is uncertain.
#![allow(clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use omni_http::SideEffectMode;
use omni_mcp_kit::{McpTool, ToolContext, ToolOutput};
use omni_media::arr::inspect::{ArrInspect, ArrService};
use omni_media::arr::queue::{ArrQueue, RemoveOutcome, RemoveRequest};
use omni_media::arr::{ArrConfig, ArrHttp};
use omni_media::mcp::media_tools;
use omni_testkit::TEST_EPOCH_MS;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const QUEUE_ID: i64 = 77;
const MOVIE_ID: i64 = 958;
const TITLE: &str = "Phantom.Menace.1999.2160p.Remux-GRP";

fn config(url: &str) -> ArrConfig {
    ArrConfig {
        url: Some(url.to_owned()),
        api_key: Some("radarr-key".to_owned()),
        root_folder_path: Some("/movies".to_owned()),
        quality_profile_id: Some(10),
    }
}

fn stuck(state: &str) -> Value {
    json!({
        "id": QUEUE_ID, "movieId": MOVIE_ID, "title": TITLE, "downloadId": "abc123",
        "movie": {"title": "Star Wars: Episode I - The Phantom Menace", "tmdbId": 1893},
        "status": "completed", "trackedDownloadStatus": "warning",
        "trackedDownloadState": state,
        "statusMessages": [{"title": TITLE, "messages": ["No files found are eligible for import"]}],
        "downloadClient": "NZBGet", "protocol": "usenet"
    })
}

fn page(records: Vec<Value>) -> Value {
    json!({"totalRecords": records.len(), "records": records})
}

/// A Radarr whose queue lists `records` for the first `reads` reads and is
/// empty afterwards, and whose blocklist names the release.
async fn radarr(records: Vec<Value>, reads: u64) -> MockServer {
    radarr_blocked_at(records, reads, TEST_EPOCH_MS).await
}

async fn radarr_blocked_at(records: Vec<Value>, reads: u64, blocked_ms: i64) -> MockServer {
    let blocked_at = jiff::Timestamp::from_millisecond(blocked_ms)
        .expect("timestamp")
        .to_string();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(records)))
        .up_to_n_times(reads)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(Vec::new())))
        .with_priority(5)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/blocklist"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "totalRecords": 1,
            "records": [
                {"id": 1, "movieId": MOVIE_ID, "sourceTitle": TITLE, "date": blocked_at},
                {"id": 2, "seriesId": 12, "sourceTitle": "Show.S01.1080p-GRP", "date": blocked_at}
            ]
        })))
        .mount(&server)
        .await;
    server
}

async fn delete_answers(server: &MockServer, status: u16) {
    Mock::given(method("DELETE"))
        .and(path(format!("/api/v3/queue/{QUEUE_ID}")))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

async fn deletes(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .expect("recording")
        .into_iter()
        .filter(|r| r.method.as_str() == "DELETE")
        .collect()
}

fn queue_over(h: &common::Harness, server: &MockServer, mode: SideEffectMode) -> ArrQueue {
    ArrQueue::new(
        ArrInspect::new(
            ArrHttp::new(omni_testkit::no_network(), mode),
            config(&server.uri()),
            ArrConfig::default(),
        ),
        h.app.ctx.store.clone(),
        h.app.ctx.tracker.clone(),
    )
}

fn request() -> RemoveRequest {
    RemoveRequest {
        service: ArrService::Radarr,
        queue_ids: BTreeSet::from([QUEUE_ID]),
        blocklist: true,
        redownload: false,
        force: false,
        dry_run: false,
    }
}

async fn call(tools: &[McpTool], name: &str, input: Value) -> Result<Value, String> {
    let tool = tools
        .iter()
        .find(|t| t.meta.name == name)
        .unwrap_or_else(|| panic!("missing tool {name}"));
    let cx = ToolContext {
        call_id: "1".to_owned(),
        cancel: CancellationToken::new(),
    };
    match tool.handler.call(input, cx).await {
        Ok(
            ToolOutput::Structured(map)
            | ToolOutput::Custom {
                structured: map, ..
            },
        ) => Ok(Value::Object(map)),
        Err(error) => Err(format!("{error:?}")),
    }
}

#[tokio::test]
async fn a_failed_download_is_removed_from_the_client_blocklisted_and_verified_gone() {
    let server = radarr(vec![stuck("importBlocked")], 1).await;
    delete_answers(&server, 200).await;
    let mut h = common::Harness::new().await;
    h.services.queue = queue_over(&h, &server, SideEffectMode::Live);
    let tools = media_tools(Arc::new(h.services.clone())).expect("tools");

    let result = call(
        &tools,
        "media_downloads_remove",
        json!({"service": "radarr", "queueIds": [QUEUE_ID]}),
    )
    .await
    .expect("removal");

    let item = &result["downloads"][0];
    assert_eq!(item["outcome"], "removed");
    assert_eq!(item["blocklisted"], true);
    assert_eq!(item["download"]["downloadId"], "abc123");
    assert_eq!(result["blocklist"], true, "blocklisting is the default");
    let sent = deletes(&server).await;
    assert_eq!(sent.len(), 1);
    let query: Vec<(String, String)> = sent[0].url.query_pairs().into_owned().collect();
    for (key, value) in [
        ("removeFromClient", "true"),
        ("blocklist", "true"),
        ("skipRedownload", "true"),
    ] {
        assert!(
            query.contains(&(key.to_owned(), value.to_owned())),
            "{key}={value} in {query:?}"
        );
    }

    let again = call(
        &tools,
        "media_downloads_remove",
        json!({"service": "radarr", "queueIds": [QUEUE_ID]}),
    )
    .await
    .expect("second call");
    assert_eq!(again["downloads"][0]["outcome"], "already_removed");
    assert_eq!(deletes(&server).await.len(), 1, "nothing is sent twice");
}

#[tokio::test]
async fn redownload_asks_the_service_to_search_and_requires_blocklisting() {
    let server = radarr(vec![stuck("importBlocked")], 1).await;
    Mock::given(method("DELETE"))
        .and(path(format!("/api/v3/queue/{QUEUE_ID}")))
        .and(query_param("skipRedownload", "false"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let mut h = common::Harness::new().await;
    h.services.queue = queue_over(&h, &server, SideEffectMode::Live);
    let tools = media_tools(Arc::new(h.services.clone())).expect("tools");

    let refused = call(
        &tools,
        "media_downloads_remove",
        json!({"service": "radarr", "queueIds": [QUEUE_ID], "blocklist": false, "redownload": true}),
    )
    .await;
    assert!(refused.is_err(), "the same release would be grabbed again");
    assert!(deletes(&server).await.is_empty());

    let result = call(
        &tools,
        "media_downloads_remove",
        json!({"service": "radarr", "queueIds": [QUEUE_ID], "redownload": true}),
    )
    .await
    .expect("removal");
    assert_eq!(result["downloads"][0]["outcome"], "removed");
    assert_eq!(result["redownload"], true);
}

#[tokio::test]
async fn a_dry_run_lists_the_download_and_sends_nothing() {
    let server = radarr(vec![stuck("importBlocked")], 10).await;
    delete_answers(&server, 200).await;
    let h = common::Harness::new().await;
    let queue = queue_over(&h, &server, SideEffectMode::Live);

    let summary = queue
        .remove(RemoveRequest {
            dry_run: true,
            ..request()
        })
        .await
        .expect("summary");

    assert_eq!(summary.downloads[0].outcome, RemoveOutcome::WouldRemove);
    let download = summary.downloads[0].download.as_ref().expect("download");
    assert_eq!(download.title.as_deref(), Some(TITLE));
    assert!(deletes(&server).await.is_empty());

    let real = queue.remove(request()).await.expect("summary");
    assert_eq!(
        real.downloads[0].outcome,
        RemoveOutcome::StillQueued,
        "a dry run records nothing, and a queue that still lists it is reported"
    );
}

#[tokio::test]
async fn an_uncertain_removal_is_never_resent_without_force() {
    let server = radarr(vec![stuck("importBlocked")], 10).await;
    delete_answers(&server, 503).await;
    let h = common::Harness::new().await;
    let queue = queue_over(&h, &server, SideEffectMode::Live);

    let first = queue.remove(request()).await.expect("summary");
    assert_eq!(first.downloads[0].outcome, RemoveOutcome::Uncertain);

    let second = queue.remove(request()).await.expect("summary");
    assert_eq!(second.downloads[0].outcome, RemoveOutcome::AlreadyRequested);
    assert!(second.downloads[0].requested_at.is_some());
    assert_eq!(deletes(&server).await.len(), 1);

    let forced = queue
        .remove(RemoveRequest {
            force: true,
            ..request()
        })
        .await
        .expect("summary");
    assert_eq!(forced.downloads[0].outcome, RemoveOutcome::Uncertain);
    assert_eq!(deletes(&server).await.len(), 2, "force sends again");
}

#[tokio::test]
async fn an_uncertain_removal_settles_by_reading_the_queue() {
    // Listed for the pre-check only: the read-back finds it gone.
    let server = radarr(vec![stuck("importBlocked")], 1).await;
    delete_answers(&server, 504).await;
    let h = common::Harness::new().await;
    let queue = queue_over(&h, &server, SideEffectMode::Live);

    let summary = queue.remove(request()).await.expect("summary");
    assert_eq!(summary.downloads[0].outcome, RemoveOutcome::Removed);
    assert_eq!(summary.downloads[0].blocklisted, Some(true));

    let again = queue.remove(request()).await.expect("summary");
    assert_eq!(again.downloads[0].outcome, RemoveOutcome::AlreadyRemoved);
    assert_eq!(deletes(&server).await.len(), 1);
}

#[tokio::test]
async fn a_refused_removal_releases_its_record() {
    let server = radarr(vec![stuck("importBlocked")], 10).await;
    delete_answers(&server, 400).await;
    let h = common::Harness::new().await;
    let queue = queue_over(&h, &server, SideEffectMode::Live);

    let first = queue.remove(request()).await.expect("summary");
    assert_eq!(first.downloads[0].outcome, RemoveOutcome::Rejected);
    let second = queue.remove(request()).await.expect("summary");
    assert_eq!(
        second.downloads[0].outcome,
        RemoveOutcome::Rejected,
        "a refusal certainly did nothing, so the next call may send again"
    );
    assert_eq!(deletes(&server).await.len(), 2);
}

#[tokio::test]
async fn a_download_being_imported_or_absent_is_left_alone() {
    let server = radarr(vec![stuck("importing")], 10).await;
    delete_answers(&server, 200).await;
    let h = common::Harness::new().await;
    let queue = queue_over(&h, &server, SideEffectMode::Live);

    let summary = queue
        .remove(RemoveRequest {
            queue_ids: BTreeSet::from([QUEUE_ID, 5]),
            ..request()
        })
        .await
        .expect("summary");

    let outcome = |id: i64| {
        summary
            .downloads
            .iter()
            .find(|d| d.queue_id == id)
            .map(|d| d.outcome)
    };
    assert_eq!(outcome(QUEUE_ID), Some(RemoveOutcome::Importing));
    assert_eq!(outcome(5), Some(RemoveOutcome::NotFound));
    assert!(deletes(&server).await.is_empty());
}

#[tokio::test]
async fn record_mode_records_the_removal_instead_of_sending_it() {
    let server = radarr(vec![stuck("importBlocked")], 10).await;
    delete_answers(&server, 200).await;
    let h = common::Harness::new().await;
    let http = ArrHttp::new(omni_testkit::no_network(), SideEffectMode::Record);
    let queue = ArrQueue::new(
        ArrInspect::new(http.clone(), config(&server.uri()), ArrConfig::default()),
        h.app.ctx.store.clone(),
        h.app.ctx.tracker.clone(),
    );

    let summary = queue.remove(request()).await.expect("summary");

    assert_eq!(summary.downloads[0].outcome, RemoveOutcome::Recorded);
    assert!(deletes(&server).await.is_empty());
    let recorded = http.recorded();
    assert_eq!(recorded.len(), 1);
    assert!(
        recorded[0]
            .url
            .contains("/api/v3/queue/77?removeFromClient=true")
    );
}

#[tokio::test]
async fn an_unconfigured_service_is_an_error() {
    let h = common::Harness::new().await;
    let tools = media_tools(Arc::new(h.services.clone())).expect("tools");
    let result = call(
        &tools,
        "media_downloads_remove",
        json!({"service": "sonarr", "queueIds": [1]}),
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn one_removal_clears_every_queue_item_of_a_season_pack() {
    let episode = |id: i64, number: i64| {
        json!({
            "id": id, "seriesId": 12, "seasonNumber": 1, "title": "Show.S01.1080p-GRP",
            "downloadId": "pack1", "episode": {"seasonNumber": 1, "episodeNumber": number},
            "trackedDownloadStatus": "warning", "trackedDownloadState": "importBlocked"
        })
    };
    let server = radarr(vec![episode(101, 1), episode(102, 2)], 1).await;
    Mock::given(method("DELETE"))
        .and(path("/api/v3/queue/101"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v3/queue/102"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let h = common::Harness::new().await;
    let queue = ArrQueue::new(
        ArrInspect::new(
            ArrHttp::new(omni_testkit::no_network(), SideEffectMode::Live),
            ArrConfig::default(),
            config(&server.uri()),
        ),
        h.app.ctx.store.clone(),
        h.app.ctx.tracker.clone(),
    );

    let summary = queue
        .remove(RemoveRequest {
            service: ArrService::Sonarr,
            queue_ids: BTreeSet::from([101, 102]),
            ..request()
        })
        .await
        .expect("summary");

    let outcomes: Vec<RemoveOutcome> = summary.downloads.iter().map(|d| d.outcome).collect();
    assert_eq!(
        outcomes,
        vec![RemoveOutcome::Removed, RemoveOutcome::Removed]
    );
    assert_eq!(deletes(&server).await.len(), 1, "the pack is removed once");
}

#[tokio::test]
async fn a_queue_item_the_service_no_longer_knows_is_settled_by_reading() {
    let server = radarr(vec![stuck("importBlocked")], 1).await;
    delete_answers(&server, 404).await;
    let h = common::Harness::new().await;
    let queue = queue_over(&h, &server, SideEffectMode::Live);

    let summary = queue.remove(request()).await.expect("summary");

    assert_eq!(summary.downloads[0].outcome, RemoveOutcome::Removed);
}

#[tokio::test]
async fn an_older_blocklisting_of_the_same_release_does_not_count() {
    let day = 24 * 60 * 60 * 1000;
    let server = radarr_blocked_at(vec![stuck("importBlocked")], 1, TEST_EPOCH_MS - day).await;
    delete_answers(&server, 200).await;
    let h = common::Harness::new().await;
    let queue = queue_over(&h, &server, SideEffectMode::Live);

    let summary = queue.remove(request()).await.expect("summary");

    assert_eq!(summary.downloads[0].outcome, RemoveOutcome::Removed);
    assert_eq!(summary.downloads[0].blocklisted, Some(false));
}
