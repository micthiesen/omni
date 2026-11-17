//! The six PressPods MCP tools against their
//! golden schemas: inputs are validated and outputs must satisfy the golden
//! output schema.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Harness, HarnessOptions, harness};
use omni_mcp_kit::registry::standalone_context;
use omni_mcp_kit::{McpTool, ToolError, ToolOutput, ToolPhase};
use omni_presspods::model::{Chapter, JobStatus, PressPodsJob};
use omni_store::entity::{EntityWrite, UpsertOpts};
use serde_json::{Value, json};

async fn setup() -> (Harness, Vec<McpTool>) {
    let h = harness(HarnessOptions::default()).await;
    let tools = omni_presspods::mcp::tools(&h.service).unwrap();
    (h, tools)
}

async fn call(tools: &[McpTool], name: &str, input: Value) -> Result<Value, ToolError> {
    let tool = tools.iter().find(|t| t.meta.name == name).unwrap();
    match tool.handler.call(input, standalone_context("test")).await? {
        ToolOutput::Structured(map)
        | ToolOutput::Custom {
            structured: map, ..
        } => Ok(Value::Object(map)),
    }
}

#[tokio::test]
async fn registers_the_six_tools_in_serving_order() {
    let (_h, tools) = setup().await;
    let names: Vec<&str> = tools.iter().map(|t| t.meta.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "presspods_list",
            "presspods_episode_get",
            "presspods_transcript_read",
            "presspods_submit",
            "presspods_retry",
            "presspods_delete"
        ]
    );
}

#[tokio::test]
async fn lists_and_filters_episodes_and_jobs() {
    let (h, tools) = setup().await;
    let mut first = common::episode("https://example.com/a", 2);
    first.title = "Rust Weekly".into();
    first.chapters = Some(vec![Chapter::new(0.0, "A"), Chapter::new(1.0, "B")]);
    let second = common::episode("https://example.com/b", 1);
    for e in [&first, &second] {
        h.service
            .persist_episode_with_audio(e, b"mp3")
            .await
            .unwrap();
    }
    let page = call(
        &tools,
        "presspods_list",
        json!({ "resource": "episodes", "limit": 1 }),
    )
    .await
    .unwrap();
    assert_eq!(page["resource"], "episodes");
    assert_eq!(page["total"], 2);
    assert_eq!(page["nextCursor"], 1);
    assert_eq!(page["items"][0]["episodeId"], first.episode_id.as_str());
    assert_eq!(page["items"][0]["chapterCount"], 2);
    assert!(page["items"][0].get("audioFile").is_none());
    let keys: Vec<&String> = page.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["resource", "items", "nextCursor", "total"]);

    let filtered = call(
        &tools,
        "presspods_list",
        json!({ "resource": "episodes", "query": "  rust " }),
    )
    .await
    .unwrap();
    assert_eq!(filtered["total"], 1);

    for (id, status) in [("q", JobStatus::Queued), ("f", JobStatus::Failed)] {
        let job = PressPodsJob {
            job_id: id.into(),
            url: format!("https://example.com/{id}"),
            normalized_url: None,
            status,
            attempts: 1,
            next_attempt_at: 0,
            last_error: None,
            created_at: 1,
            updated_at: 1,
            claimed_at: None,
            last_run_id: None,
            speech_unreachable_since: None,
            speech_unreachable_checks: None,
            extra: Default::default(),
        };
        h.service
            .persistence()
            .store()
            .write(move |tx| tx.upsert(&job, UpsertOpts::default()))
            .await
            .unwrap();
    }
    let jobs = call(
        &tools,
        "presspods_list",
        json!({ "resource": "jobs", "status": "failed" }),
    )
    .await
    .unwrap();
    assert_eq!(jobs["total"], 1);
    assert_eq!(jobs["items"][0]["jobId"], "f");

    let invalid = call(&tools, "presspods_list", json!({ "resource": "videos" }))
        .await
        .unwrap_err();
    assert_eq!(invalid.phase, ToolPhase::Input);
}

#[tokio::test]
async fn reads_episodes_and_bounded_transcripts() {
    let (h, tools) = setup().await;
    let mut episode = common::episode("https://example.com/a", 1);
    episode.content = "abcdefghij".into();
    h.service
        .persist_episode_with_audio(&episode, b"mp3")
        .await
        .unwrap();

    let got = call(
        &tools,
        "presspods_episode_get",
        json!({ "episodeId": episode.episode_id }),
    )
    .await
    .unwrap();
    assert_eq!(got["episode"]["title"], "t");
    let missing = call(
        &tools,
        "presspods_episode_get",
        json!({ "episodeId": "nope" }),
    )
    .await
    .unwrap_err();
    assert_eq!(missing.message, "PressPods episode not found");

    let page = call(
        &tools,
        "presspods_transcript_read",
        json!({ "episodeId": episode.episode_id, "offset": 2, "maxChars": 4 }),
    )
    .await
    .unwrap();
    assert_eq!(
        page,
        json!({
            "episodeId": episode.episode_id, "title": "t", "offset": 2, "text": "cdef",
            "nextOffset": 6, "totalChars": 10, "truncated": true
        })
    );
    let tail = call(
        &tools,
        "presspods_transcript_read",
        json!({ "episodeId": episode.episode_id, "offset": 6 }),
    )
    .await
    .unwrap();
    assert_eq!(tail["text"], "ghij");
    assert_eq!(tail["nextOffset"], Value::Null);
    let beyond = call(
        &tools,
        "presspods_transcript_read",
        json!({ "episodeId": episode.episode_id, "offset": 11 }),
    )
    .await
    .unwrap_err();
    assert_eq!(
        beyond.message,
        "Transcript offset is beyond the end of the episode"
    );
}

#[tokio::test]
async fn submits_retries_and_deletes() {
    let (h, tools) = setup().await;
    let submitted = call(
        &tools,
        "presspods_submit",
        json!({ "url": "https://example.com/new\nhttps://dup" }),
    )
    .await
    .unwrap();
    assert_eq!(submitted["job"]["url"], "https://example.com/new");
    let private = call(
        &tools,
        "presspods_submit",
        json!({ "url": "http://localhost/x" }),
    )
    .await
    .unwrap_err();
    assert_eq!(private.phase, ToolPhase::Input);

    let job_id = submitted["job"]["jobId"].as_str().unwrap().to_owned();
    let not_failed = call(
        &tools,
        "presspods_retry",
        json!({ "resource": "job", "jobId": job_id }),
    )
    .await
    .unwrap_err();
    assert_eq!(
        not_failed.message,
        "Only failed PressPods jobs can be retried"
    );
    let job = h
        .service
        .persistence()
        .get_job(&job_id)
        .await
        .unwrap()
        .unwrap();
    h.service
        .persistence()
        .record_job_failure(&job, "bad", false)
        .await
        .unwrap();
    let retried = call(
        &tools,
        "presspods_retry",
        json!({ "resource": "job", "jobId": job_id }),
    )
    .await
    .unwrap();
    assert_eq!(retried["job"]["status"], "queued");

    let episode = common::episode("https://example.com/old", 1);
    h.service
        .persist_episode_with_audio(&episode, b"mp3")
        .await
        .unwrap();
    let regen = call(
        &tools,
        "presspods_retry",
        json!({ "resource": "episode", "episodeId": episode.episode_id }),
    )
    .await
    .unwrap();
    assert_eq!(regen["job"]["url"], "https://example.com/old");

    let deleted = call(
        &tools,
        "presspods_delete",
        json!({ "resource": "episode", "episodeId": episode.episode_id }),
    )
    .await
    .unwrap();
    assert_eq!(deleted, json!({ "resource": "episode", "deleted": true }));
    assert!(
        !h.service
            .audio()
            .episode_audio_path(&episode.audio_file)
            .unwrap()
            .exists()
    );
    let dismissed = call(
        &tools,
        "presspods_delete",
        json!({ "resource": "job", "jobId": job_id }),
    )
    .await
    .unwrap();
    assert_eq!(dismissed, json!({ "resource": "job", "deleted": true }));
    let gone = call(
        &tools,
        "presspods_delete",
        json!({ "resource": "job", "jobId": job_id }),
    )
    .await
    .unwrap_err();
    assert_eq!(gone.message, "PressPods job not found");
}
