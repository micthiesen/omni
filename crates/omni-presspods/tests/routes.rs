//! The PressPods HTTP surface end to end (status codes, headers and bodies).
//! Byte-range parsing cases are unit tests in `src/routes.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::{Harness, HarnessOptions, harness};
use omni_presspods::model::{JobStatus, PressPodsJob};
use omni_store::entity::{EntityWrite, UpsertOpts};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const TOKEN: &str = "secret-presspods-token";

async fn setup() -> (Harness, Router) {
    let h = harness(HarnessOptions {
        env: vec![
            ("PRESSPODS_AUTH_TOKEN", TOKEN.to_owned()),
            (
                "PRESSPODS_PUBLIC_URL",
                "https://pods.example.test".to_owned(),
            ),
        ],
        ..HarnessOptions::default()
    })
    .await;
    let router = omni_presspods::routes::router(h.service.clone()).unwrap();
    (h, router)
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "omni.boris");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_owned())))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

#[tokio::test]
async fn routes_are_absent_without_an_auth_token() {
    let h = harness(HarnessOptions::default()).await;
    assert!(omni_presspods::routes::router(h.service.clone()).is_none());
}

#[tokio::test]
async fn public_submission_requires_the_token_and_a_public_url() {
    let (h, router) = setup().await;
    let body = r#"{"url":"https://example.com/story\nhttps://example.com/story"}"#;
    let reply = send(&router, Method::POST, "/pods/episodes", &[], Some(body)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.json(), json!({ "error": "Unauthorized" }));

    let reply = send(
        &router,
        Method::POST,
        "/pods/episodes?authToken=wrong",
        &[],
        Some(body),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    let reply = send(
        &router,
        Method::POST,
        "/pods/episodes",
        &[("x-auth-token", TOKEN)],
        Some("{oops"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json(),
        json!({ "error": "Body must be JSON: { url: string }" })
    );

    let reply = send(
        &router,
        Method::POST,
        &format!("/pods/episodes?authToken={TOKEN}"),
        &[],
        Some(r#"{"url":"http://127.0.0.1/x"}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    let reply = send(
        &router,
        Method::POST,
        &format!("/pods/episodes?authToken={TOKEN}"),
        &[],
        Some(body),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let jobs = h.service.persistence().get_all_jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].url, "https://example.com/story");
    assert_eq!(reply.json(), json!({ "jobId": jobs[0].job_id }));
}

#[tokio::test]
async fn rss_serves_the_feed_with_an_etag_and_honors_if_none_match() {
    let (h, router) = setup().await;
    let episode = common::episode("https://example.com/a", 1_767_225_600_000);
    h.service
        .persist_episode_with_audio(&episode, b"mp3")
        .await
        .unwrap();

    assert_eq!(
        send(&router, Method::GET, "/pods/rss", &[], None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    let reply = send(
        &router,
        Method::GET,
        &format!("/pods/rss?authToken={TOKEN}"),
        &[],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let etag = format!("\"{}\"", episode.episode_id);
    assert_eq!(reply.header("etag"), Some(etag.as_str()));
    assert_eq!(reply.header("cache-control"), Some("no-cache"));
    assert_eq!(
        reply.header("content-type"),
        Some("application/xml; charset=utf-8")
    );
    let xml = String::from_utf8(reply.body).unwrap();
    assert!(xml.contains(&format!(
        "<enclosure url=\"https://pods.example.test/pods/audio/{}\" length=\"1\" type=\"audio/mpeg\"/>",
        episode.audio_file
    )));

    let reply = send(
        &router,
        Method::GET,
        "/pods/rss",
        &[("x-auth-token", TOKEN), ("if-none-match", &etag)],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_MODIFIED);
    assert!(reply.body.is_empty());

    let reply = send(
        &router,
        Method::HEAD,
        "/pods/rss",
        &[("x-auth-token", TOKEN)],
        None,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.is_empty());
    assert_eq!(reply.header("etag"), Some(etag.as_str()));
}

#[tokio::test]
async fn audio_supports_ranges_head_and_rejects_bad_names() {
    let (h, router) = setup().await;
    let episode = common::episode("https://example.com/a", 1);
    h.service
        .persist_episode_with_audio(&episode, b"0123456789")
        .await
        .unwrap();
    let uri = format!("/pods/audio/{}", episode.audio_file);

    let full = send(&router, Method::GET, &uri, &[], None).await;
    assert_eq!(full.status, StatusCode::OK);
    assert_eq!(full.body, b"0123456789");
    assert_eq!(full.header("accept-ranges"), Some("bytes"));
    assert_eq!(full.header("content-type"), Some("audio/mpeg"));
    assert_eq!(
        full.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(full.header("content-length"), Some("10"));

    let partial = send(&router, Method::GET, &uri, &[("range", "bytes=2-5")], None).await;
    assert_eq!(partial.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(partial.body, b"2345");
    assert_eq!(partial.header("content-range"), Some("bytes 2-5/10"));
    assert_eq!(partial.header("content-length"), Some("4"));

    let suffix = send(&router, Method::GET, &uri, &[("range", "bytes=-3")], None).await;
    assert_eq!(suffix.body, b"789");

    let invalid = send(&router, Method::GET, &uri, &[("range", "bytes=10-")], None).await;
    assert_eq!(invalid.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(invalid.header("content-range"), Some("bytes */10"));

    let head = send(&router, Method::HEAD, &uri, &[], None).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-length"), Some("10"));
    assert!(head.body.is_empty());

    assert_eq!(
        send(
            &router,
            Method::GET,
            "/pods/audio/..%2Fescape.mp3",
            &[],
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&router, Method::GET, "/pods/audio/missing.mp3", &[], None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn logo_is_served_immutable() {
    let (_h, router) = setup().await;
    let reply = send(&router, Method::GET, "/pods/logo.jpeg", &[], None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("image/jpeg"));
    assert_eq!(reply.body, b"\xff\xd8LOGO");
}

async fn put_job(h: &Harness, job: PressPodsJob) {
    h.service
        .persistence()
        .store()
        .write(move |tx| tx.upsert(&job, UpsertOpts::default()))
        .await
        .unwrap();
}

fn job(id: &str, status: JobStatus) -> PressPodsJob {
    PressPodsJob {
        job_id: id.into(),
        url: "https://example.com/j".into(),
        normalized_url: None,
        status,
        attempts: 6,
        next_attempt_at: 0,
        last_error: Some("boom".into()),
        created_at: 10,
        updated_at: 20,
        claimed_at: None,
        last_run_id: None,
        speech_unreachable_since: None,
        speech_unreachable_checks: None,
        extra: Default::default(),
    }
}

#[tokio::test]
async fn web_ui_lists_details_and_deletes_episodes() {
    let (h, router) = setup().await;
    let episode = omni_presspods::model::PressPodsEpisode {
        duration_seconds: Some(61.0),
        costs: Some(omni_presspods::model::Costs {
            llm_cents: 1.234,
            tts_cents: 2.0,
            ..Default::default()
        }),
        ..common::episode("https://example.com/a", 5)
    };
    h.service
        .persist_episode_with_audio(&episode, b"mp3")
        .await
        .unwrap();
    put_job(&h, job("j1", JobStatus::Failed)).await;

    let list = send(&router, Method::GET, "/api/press-pods/episodes", &[], None)
        .await
        .json();
    assert_eq!(
        list["episodes"][0],
        json!({
            "episodeId": episode.episode_id, "title": "t", "author": null, "publication": null,
            "domain": null, "articleUrl": "https://example.com/a", "leadImageUrl": null,
            "excerpt": null, "voiceName": null, "synthesizedSeconds": null, "chapters": null,
            "audioUrl": format!("/pods/audio/{}", episode.audio_file), "durationSeconds": 61,
            "fileBytes": 1, "retrieverName": null, "retrieverSeconds": null,
            "retrieverAttempts": null, "costCents": 3.23, "createdAt": 5, "publishedAt": null,
            "runId": null
        })
    );
    assert_eq!(
        list["jobs"][0],
        json!({
            "jobId": "j1", "url": "https://example.com/j", "status": "failed", "attempts": 6,
            "nextAttemptAt": null, "lastError": "boom", "createdAt": 10, "updatedAt": 20,
            "lastRunId": null
        })
    );

    let detail = send(
        &router,
        Method::GET,
        &format!("/api/press-pods/episodes/{}", episode.episode_id),
        &[],
        None,
    )
    .await;
    let detail = detail.json();
    assert_eq!(detail["episode"]["content"], "c");
    assert_eq!(detail["episode"]["chunks"], Value::Null);
    assert_eq!(detail["episode"]["costs"]["llmCents"], 1.234);

    let missing = send(
        &router,
        Method::GET,
        "/api/press-pods/episodes/nope",
        &[],
        None,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json(), json!({ "error": "Unknown episode" }));

    let deleted = send(
        &router,
        Method::DELETE,
        &format!("/api/press-pods/episodes/{}", episode.episode_id),
        &[],
        None,
    )
    .await;
    assert_eq!(deleted.json(), json!({ "deleted": true }));
    assert!(
        !h.service
            .audio()
            .episode_audio_path(&episode.audio_file)
            .unwrap()
            .exists()
    );
    let again = send(
        &router,
        Method::DELETE,
        &format!("/api/press-pods/episodes/{}", episode.episode_id),
        &[],
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn web_ui_submits_and_retries() {
    let (h, router) = setup().await;
    let bad = send(
        &router,
        Method::POST,
        "/api/press-pods/submit",
        &[],
        Some(r#"{"url":"nope"}"#),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        bad.json(),
        json!({ "error": "A valid article URL is required" })
    );

    let ok = send(
        &router,
        Method::POST,
        "/api/press-pods/submit",
        &[],
        Some(r#"{"url":"https://example.com/x"}"#),
    )
    .await;
    assert_eq!(ok.status, StatusCode::ACCEPTED);
    assert_eq!(ok.json()["job"]["status"], "queued");
    assert_eq!(ok.json()["job"]["nextAttemptAt"], Value::Null);

    let episode = common::episode("https://example.com/regen", 1);
    h.service
        .persist_episode_with_audio(&episode, b"mp3")
        .await
        .unwrap();
    let regen = send(
        &router,
        Method::POST,
        &format!("/api/press-pods/episodes/{}/retry", episode.episode_id),
        &[],
        None,
    )
    .await;
    assert_eq!(regen.status, StatusCode::ACCEPTED);
    assert_eq!(regen.json()["job"]["url"], "https://example.com/regen");
    let missing = send(
        &router,
        Method::POST,
        "/api/press-pods/episodes/nope/retry",
        &[],
        None,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn job_retry_and_dismissal_follow_status_rules() {
    let (h, router) = setup().await;
    put_job(&h, job("failed", JobStatus::Failed)).await;
    put_job(&h, job("busy", JobStatus::Processing)).await;

    let unknown = send(
        &router,
        Method::POST,
        "/api/press-pods/jobs/none/retry",
        &[],
        None,
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.json(), json!({ "error": "Unknown job" }));
    let conflict = send(
        &router,
        Method::POST,
        "/api/press-pods/jobs/busy/retry",
        &[],
        None,
    )
    .await;
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert_eq!(
        conflict.json(),
        json!({ "error": "Only failed jobs can be retried" })
    );
    let retried = send(
        &router,
        Method::POST,
        "/api/press-pods/jobs/failed/retry",
        &[],
        None,
    )
    .await;
    assert_eq!(retried.status, StatusCode::OK);
    assert_eq!(retried.json()["job"]["attempts"], 0);
    assert_eq!(retried.json()["job"]["lastError"], Value::Null);

    let busy = send(
        &router,
        Method::DELETE,
        "/api/press-pods/jobs/busy",
        &[],
        None,
    )
    .await;
    assert_eq!(busy.status, StatusCode::CONFLICT);
    assert_eq!(
        busy.json(),
        json!({ "error": "Job is currently processing" })
    );
    let work_id = omni_presspods::storage::checkpoint_work_id("https://example.com/j");
    h.service
        .audio()
        .write_chunk_checkpoint(&work_id, "k.wav", b"wav")
        .await;
    let dismissed = send(
        &router,
        Method::DELETE,
        "/api/press-pods/jobs/failed",
        &[],
        None,
    )
    .await;
    assert_eq!(dismissed.json(), json!({ "deleted": true }));
    assert!(
        h.service
            .audio()
            .read_chunk_checkpoint(&work_id, "k.wav")
            .await
            .is_none()
    );
    assert!(
        h.service
            .persistence()
            .get_job("failed")
            .await
            .unwrap()
            .is_none()
    );
}
