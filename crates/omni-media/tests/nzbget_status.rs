//! NZBGet status read straight from its JSON-RPC API: pauses (including
//! NZBGet's own low-disk-space pause), disk space, post-processing stages,
//! failed history and log warnings, without exposing NZB source URLs.
#![allow(clippy::expect_used)]

use omni_media::nzbget::{NzbGet, NzbGetError};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn answer(server: &MockServer, rpc: &str, result: Value) {
    Mock::given(method("POST"))
        .and(path("/jsonrpc"))
        .and(body_partial_json(json!({"method": rpc})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"version": "1.1", "id": 1, "result": result})),
        )
        .mount(server)
        .await;
}

fn status(paused: bool) -> Value {
    json!({
        "RemainingSizeMB": 4200, "DownloadRate": 0, "AverageDownloadRate": 72_688_471,
        "DownloadLimit": 0, "PostJobCount": 1, "ServerPaused": false,
        "DownloadPaused": paused, "PostPaused": false, "ScanPaused": false,
        "ServerStandBy": paused, "QuotaReached": false,
        "FreeDiskSpaceMB": 900, "TotalDiskSpaceMB": 62_218_567,
        "FreeInterDiskSpaceMB": 261_758, "TotalInterDiskSpaceMB": 466_890,
        "ResumeTime": 0,
        "NewsServers": [{"ID": 1, "Active": true}, {"ID": 2, "Active": false}]
    })
}

fn group() -> Value {
    json!({
        "NZBID": 41, "NZBName": "Clones.2002.2160p.Remux-GRP", "Kind": "NZB",
        "Category": "radarr", "Status": "UNPACKING", "FileSizeMB": 60_000,
        "RemainingSizeMB": 0, "Health": 1000, "CriticalHealth": 925,
        "PostInfoText": "Unpacking Clones.2002.2160p.Remux-GRP", "PostStageProgress": 455,
        "ActiveDownloads": 0,
        "Parameters": [{"Name": "drone", "Value": "abc123"}]
    })
}

fn history() -> Value {
    json!([
        {
            "NZBID": 30, "Name": "Old.Failure", "Category": "sonarr", "Status": "FAILURE/HEALTH",
            "ParStatus": "NONE", "UnpackStatus": "NONE", "MoveStatus": "NONE",
            "DeleteStatus": "HEALTH", "MarkStatus": "NONE", "Health": 412,
            "FileSizeMB": 2000, "HistoryTime": 1_792_900_000,
            "URL": "https://indexer.example/getnzb?id=1&apikey=s3cret",
            "Parameters": [{"Name": "drone", "Value": "def456"}]
        },
        {
            "NZBID": 33, "Name": "Dupe", "Category": "radarr", "Status": "DELETED/DUPE",
            "HistoryTime": 1_792_960_000, "Parameters": []
        },
        {
            "NZBID": 31, "Name": "Done", "Category": "radarr", "Status": "SUCCESS/ALL",
            "HistoryTime": 1_792_950_000, "Parameters": []
        },
        {
            "NZBID": 32, "Name": "Recent.Failure", "Category": "radarr", "Status": "FAILURE/UNPACK",
            "ParStatus": "SUCCESS", "UnpackStatus": "FAILURE", "MoveStatus": "NONE",
            "DeleteStatus": "NONE", "MarkStatus": "NONE", "Health": 1000,
            "FileSizeMB": 8000, "HistoryTime": 1_792_940_000, "Parameters": []
        }
    ])
}

fn log() -> Value {
    json!([
        {"ID": 1, "Kind": "INFO", "Time": 1_792_930_000, "Text": "Downloading Clones"},
        {"ID": 2, "Kind": "WARNING", "Time": 1_792_930_100, "Text": "Low disk space on /downloads. Pausing download"},
        {"ID": 3, "Kind": "ERROR", "Time": 1_792_930_200, "Text": "Could not fetch https://indexer.example/getnzb?apikey=s3cret"}
    ])
}

async fn nzbget(paused: bool) -> MockServer {
    let server = MockServer::start().await;
    answer(&server, "version", json!("26.3")).await;
    answer(&server, "status", status(paused)).await;
    answer(&server, "listgroups", json!([group()])).await;
    answer(&server, "history", history()).await;
    answer(&server, "log", log()).await;
    server
}

#[tokio::test]
async fn a_low_disk_space_pause_is_named_with_disk_post_processing_and_failures() {
    let server = nzbget(true).await;
    let client = NzbGet::new(omni_testkit::no_network(), Some(server.uri()));

    let status = client.status().await.expect("status");

    assert_eq!(status.version.as_deref(), Some("26.3"));
    assert!(status.download_paused);
    assert!(status.paused_for_disk_space);
    assert_eq!(status.free_disk_space_mb, Some(900));
    assert_eq!(
        status.download_limit_bytes_per_sec, None,
        "0 means unlimited"
    );
    assert_eq!(status.news_servers.len(), 2);
    assert!(!status.news_servers[1].active);

    let download = &status.downloads[0];
    assert_eq!(download.download_id.as_deref(), Some("abc123"));
    assert_eq!(download.status, "UNPACKING");
    assert!((download.post_stage_percent.expect("percent") - 45.5).abs() < f64::EPSILON);
    assert!((download.health_percent - 100.0).abs() < f64::EPSILON);

    let failures: Vec<&str> = status.failures.iter().map(|f| f.status.as_str()).collect();
    assert_eq!(
        failures,
        vec!["FAILURE/UNPACK", "FAILURE/HEALTH"],
        "newest first, successes and duplicate handling dropped"
    );
    assert_eq!(status.failures[1].download_id.as_deref(), Some("def456"));
    assert_eq!(status.failures[1].finished_at, 1_792_900_000_000);

    let kinds: Vec<&str> = status.messages.iter().map(|m| m.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["ERROR", "WARNING"],
        "newest first, info dropped"
    );
    let serialized = serde_json::to_string(&status).expect("json");
    assert!(!serialized.contains("s3cret"), "{serialized}");
}

#[tokio::test]
async fn a_running_queue_is_not_reported_as_a_disk_space_pause() {
    let server = nzbget(false).await;
    let client = NzbGet::new(omni_testkit::no_network(), Some(server.uri()));

    let status = client.status().await.expect("status");

    assert!(!status.download_paused);
    assert!(
        !status.paused_for_disk_space,
        "an old warning does not make a running queue paused"
    );
}

#[tokio::test]
async fn a_broken_or_missing_nzbget_is_an_error_not_an_empty_status() {
    let unconfigured = NzbGet::new(omni_testkit::no_network(), None);
    assert_eq!(unconfigured.status().await, Err(NzbGetError::NotConfigured));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/jsonrpc"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let client = NzbGet::new(omni_testkit::no_network(), Some(server.uri()));
    assert!(matches!(
        client.status().await,
        Err(NzbGetError::Http {
            status_code: 401,
            ..
        })
    ));
}

#[tokio::test]
async fn an_unreadable_history_still_reports_the_pause_and_disk_space() {
    let server = MockServer::start().await;
    answer(&server, "version", json!("26.3")).await;
    answer(&server, "status", status(true)).await;
    answer(&server, "listgroups", json!([])).await;
    answer(&server, "log", log()).await;
    Mock::given(method("POST"))
        .and(path("/jsonrpc"))
        .and(body_partial_json(json!({"method": "history"})))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let client = NzbGet::new(omni_testkit::no_network(), Some(server.uri()));

    let status = client.status().await.expect("status");

    assert!(status.paused_for_disk_space);
    assert!(status.failures.is_empty());
    assert_eq!(status.unreadable, vec!["history"]);
}
