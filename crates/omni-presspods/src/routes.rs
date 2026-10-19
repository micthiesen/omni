//! The PressPods HTTP surface.
//!
//! `/pods/*` is exposed publicly through the reverse proxy for the iOS
//! Shortcut and podcast clients: submissions and the feed require the auth
//! token (`?authToken=` or `x-auth-token`), audio files rely on unguessable
//! content-addressed names (podcast apps cannot send headers on enclosure
//! fetches). `/api/press-pods/*` serves the same-origin web UI.

use std::io::SeekFrom;
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use omni_api::presspods as api;
use omni_server_kit::{ApiError, JSON_BODY_LIMIT, api_error, json_response};
use regex::Regex;
use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::model::{
    Chapter, ChunkStat, Costs, JobStatus, PressPodsEpisode, PressPodsJob, RetrieverAttempt,
};
use crate::persistence::job_normalized_url;
use crate::rss::{build_feed, feed_etag};
use crate::service::PressPods;
use crate::storage::{checkpoint_work_id, is_audio_file_name};
use crate::submit::first_line_public_url;

const LOG: &str = "PressPods";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

#[derive(Clone)]
struct RouteState {
    service: PressPods,
    auth_token: Arc<str>,
}

/// Whether a raw path may be served on the public PressPods host
/// (`PRESSPODS_PUBLIC_URL`): the token-checked feed, content-addressed audio
/// and the artwork podcast apps fetch. Everything else, including public
/// submission, stays on the LAN.
pub fn is_public_path(path: &str) -> bool {
    path == "/pods/rss"
        || path == "/pods/logo.jpeg"
        || path
            .strip_prefix("/pods/audio/")
            .is_some_and(crate::storage::is_audio_file_name)
}

/// The router; `None` without `PRESSPODS_AUTH_TOKEN` (no routes are mounted).
pub fn router(service: PressPods) -> Option<Router> {
    let token = service
        .config()
        .presspods_auth_token
        .clone()
        .filter(|t| !t.is_empty())?;
    let state = RouteState {
        service,
        auth_token: Arc::from(token),
    };
    Some(
        Router::new()
            .route("/pods/episodes", post(submit_public))
            .route("/pods/rss", get(rss).head(rss))
            .route("/pods/audio/{file}", get(audio).head(audio))
            .route("/pods/logo.jpeg", get(logo))
            .route("/api/press-pods/episodes", get(list))
            .route(
                "/api/press-pods/episodes/{id}",
                get(detail).delete(delete_episode),
            )
            .route("/api/press-pods/episodes/{id}/retry", post(retry_episode))
            .route("/api/press-pods/submit", post(submit_ui))
            .route("/api/press-pods/jobs/{job_id}/retry", post(retry_job))
            .route(
                "/api/press-pods/jobs/{job_id}",
                axum::routing::delete(delete_job),
            )
            .with_state(state),
    )
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

/// JS `Math.round(x * 100) / 100`.
fn round_cents(x: f64) -> f64 {
    (x * 100.0 + 0.5).floor() / 100.0
}

pub(crate) fn cost_cents(costs: Option<&Costs>) -> Option<f64> {
    costs.map(|c| round_cents(c.llm_cents + c.tts_cents))
}

fn chapter_dto(c: &Chapter) -> api::PressPodsChapter {
    api::PressPodsChapter {
        start_time_seconds: c.start_time_seconds,
        title: c.title.clone(),
    }
}

fn attempt_dto(a: &RetrieverAttempt) -> api::PressPodsRetrieverAttempt {
    match a {
        RetrieverAttempt::Success {
            name,
            content_rating,
            text_chars,
            ..
        } => api::PressPodsRetrieverAttempt::Success {
            name: name.clone(),
            success: true,
            content_rating: *content_rating,
            text_chars: *text_chars,
        },
        RetrieverAttempt::Failure { name, error, .. } => api::PressPodsRetrieverAttempt::Failure {
            name: name.clone(),
            success: false,
            error: error.clone(),
        },
    }
}

fn chunk_dto(c: &ChunkStat) -> api::PressPodsChunkStat {
    api::PressPodsChunkStat {
        index: c.index,
        section_index: c.section_index,
        section_title: c.section_title.clone(),
        text: c.text.clone(),
        char_count: c.char_count,
        duration_seconds: c.duration_seconds,
        start_time_seconds: c.start_time_seconds,
        sec_per_char: c.sec_per_char,
        attempts: c.attempts,
        coverage: c.coverage,
        word_ratio: c.word_ratio,
        expected_words: c.expected_words,
        resplit: c.resplit,
        resplit_depth: c.resplit_depth,
    }
}

fn costs_dto(c: &Costs) -> api::PressPodsCosts {
    api::PressPodsCosts {
        llm_cents: c.llm_cents,
        tts_cents: c.tts_cents,
        detail_cents: c
            .detail_cents
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        detail_tokens: c
            .detail_tokens
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    api::PressPodsTokenCounts {
                        input: v.input,
                        output: v.output,
                    },
                )
            })
            .collect(),
        detail_chars: c
            .detail_chars
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
    }
}

/// An episode in the list payload.
pub fn episode_dto(e: &PressPodsEpisode) -> api::PressPodsEpisode {
    api::PressPodsEpisode {
        episode_id: e.episode_id.clone(),
        title: e.title.clone(),
        author: e.author.clone(),
        publication: e.publication.clone(),
        domain: e.domain.clone(),
        article_url: e.article_url.clone(),
        lead_image_url: e.lead_image_url.clone(),
        excerpt: e.excerpt.clone(),
        voice_name: e.voice_name.clone(),
        synthesized_seconds: e.synthesized_seconds,
        chapters: e
            .chapters
            .as_ref()
            .map(|c| c.iter().map(chapter_dto).collect()),
        audio_url: format!("/pods/audio/{}", e.audio_file),
        duration_seconds: e.duration_seconds,
        file_bytes: e.file_bytes,
        retriever_name: e.retriever_name.clone(),
        retriever_seconds: e.retriever_seconds,
        retriever_attempts: e
            .retriever_attempts
            .as_ref()
            .map(|a| a.iter().map(attempt_dto).collect()),
        cost_cents: cost_cents(e.costs.as_ref()),
        created_at: e.created_at,
        published_at: e.published_at,
        run_id: e.run_id.clone(),
    }
}

pub fn episode_detail_dto(e: &PressPodsEpisode) -> api::PressPodsEpisodeDetail {
    api::PressPodsEpisodeDetail {
        episode: episode_dto(e),
        content: e.content.clone(),
        author_gender: e.author_gender.map(|g| g.as_str().to_owned()),
        voice_provider: e.voice_provider.clone(),
        chunks: e.chunks.as_ref().map(|c| c.iter().map(chunk_dto).collect()),
        costs: e.costs.as_ref().map(costs_dto),
    }
}

pub fn job_dto(j: &PressPodsJob) -> api::PressPodsJob {
    api::PressPodsJob {
        job_id: j.job_id.clone(),
        url: j.url.clone(),
        status: match j.status {
            JobStatus::Queued => api::PressPodsJobStatus::Queued,
            JobStatus::Processing => api::PressPodsJobStatus::Processing,
            JobStatus::Failed => api::PressPodsJobStatus::Failed,
        },
        attempts: j.attempts,
        next_attempt_at: (j.next_attempt_at != 0).then_some(j.next_attempt_at),
        last_error: j.last_error.clone(),
        created_at: j.created_at,
        updated_at: j.updated_at,
        last_run_id: j.last_run_id.clone(),
    }
}

/// Integral floats as JS prints them (`1` not `1.0`).
pub(crate) fn js_numbers(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Number(n) => match n.as_f64() {
            #[allow(clippy::cast_possible_truncation)]
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 => {
                Value::from(f as i64)
            }
            _ => Value::Number(n),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(js_numbers).collect()),
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, js_numbers(v))).collect())
        }
        other => other,
    }
}

/// A JSON response with JS number formatting.
fn json<T: serde::Serialize>(status: StatusCode, body: &T) -> Response {
    match serde_json::to_value(body) {
        Ok(value) => json_response(status, js_numbers(value).to_string()),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Public routes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenQuery {
    #[serde(rename = "authToken")]
    auth_token: Option<String>,
}

fn authorized(state: &RouteState, query: &TokenQuery, headers: &HeaderMap) -> bool {
    let provided = query.auth_token.clone().or_else(|| {
        headers
            .get("x-auth-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    });
    match provided.filter(|p| !p.is_empty()) {
        Some(provided) => {
            omni_core::digest::ct_eq_sha256(provided.as_bytes(), state.auth_token.as_bytes())
        }
        None => false,
    }
}

fn unauthorized() -> Response {
    api_error(StatusCode::UNAUTHORIZED, "Unauthorized")
}

/// The submitted URL from `{ url }` (first line, trimmed, public syntax).
fn decode_submit_url(body: &Bytes) -> Option<String> {
    if body.len() > JSON_BODY_LIMIT {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let url = value.get("url")?.as_str()?;
    first_line_public_url(url).ok()
}

async fn submit_public(
    State(state): State<RouteState>,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorized(&state, &query, &headers) {
        return unauthorized();
    }
    let Some(url) = decode_submit_url(&body) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "Body must be JSON: { url: string }",
        );
    };
    match state.service.submit_episode_url(&url).await {
        Ok(job) => json(
            StatusCode::ACCEPTED,
            &api::PressPodsSubmitResponse { job_id: job.job_id },
        ),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

/// Public origin for enclosure URLs: config, else the forwarded headers.
fn base_url(state: &RouteState, headers: &HeaderMap) -> String {
    if let Some(url) = state
        .service
        .config()
        .presspods_public_url
        .as_deref()
        .filter(|u| !u.is_empty())
    {
        return url.to_owned();
    }
    let header_str = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let host = header_str("x-forwarded-host")
        .or_else(|| header_str("host"))
        .unwrap_or("localhost");
    let proto = header_str("x-forwarded-proto").unwrap_or("http");
    format!("{proto}://{host}")
}

async fn rss(
    State(state): State<RouteState>,
    method: Method,
    Query(query): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&state, &query, &headers) {
        return unauthorized();
    }
    let episodes = match state.service.persistence().get_all_episodes().await {
        Ok(episodes) => episodes,
        Err(error) => return ApiError::internal(error).into_response(),
    };
    let etag = feed_etag(&episodes);
    let mut response = if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        (StatusCode::NOT_MODIFIED, Body::empty()).into_response()
    } else if method == Method::HEAD {
        (StatusCode::OK, Body::empty()).into_response()
    } else {
        let feed = build_feed(
            &base_url(&state, &headers),
            &episodes,
            state.service.deps.clock.now_ms(),
        );
        (StatusCode::OK, Body::from(feed)).into_response()
    };
    let out = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&etag) {
        out.insert(header::ETAG, value);
    }
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    response
}

/// A parsed `Range` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteRange {
    Range { start: u64, end: u64 },
    Invalid,
}

static RANGE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^bytes=(\d*)-(\d*)$").ok());

/// `None` without a header, `Invalid` for a malformed or
/// unsatisfiable range; the end is clamped to the file.
pub fn parse_byte_range(header: Option<&str>, size: u64) -> Option<ByteRange> {
    let header = header.filter(|h| !h.is_empty())?;
    let Some(captures) = RANGE.as_ref().and_then(|re| re.captures(header)) else {
        return Some(ByteRange::Invalid);
    };
    let number = |i: usize| captures.get(i).map(|m| m.as_str()).unwrap_or("");
    let (start, end) = (number(1), number(2));
    // Digit strings beyond u64 are larger than any file.
    let parse = |s: &str| s.parse::<u64>().unwrap_or(u64::MAX);
    if start.is_empty() && end.is_empty() {
        return Some(ByteRange::Invalid);
    }
    if start.is_empty() {
        let suffix = parse(end);
        if suffix == 0 || size == 0 {
            return Some(ByteRange::Invalid);
        }
        return Some(ByteRange::Range {
            start: size.saturating_sub(suffix),
            end: size - 1,
        });
    }
    let start = parse(start);
    let end = if end.is_empty() {
        size.saturating_sub(1)
    } else {
        parse(end).min(size.saturating_sub(1))
    };
    if start >= size || start > end {
        return Some(ByteRange::Invalid);
    }
    Some(ByteRange::Range { start, end })
}

fn with_header(mut response: Response, name: header::HeaderName, value: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
    response
}

async fn audio(
    State(state): State<RouteState>,
    method: Method,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, Body::empty()).into_response();
    if !is_audio_file_name(&file) {
        return not_found();
    }
    let Ok(path) = state.service.audio().episode_audio_path(&file) else {
        return not_found();
    };
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return not_found();
    };
    let size = meta.len();
    let base = |status: StatusCode, body: Body| {
        let mut response = (status, body).into_response();
        let out = response.headers_mut();
        out.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        out.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
        out.insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
        response
    };
    let range = parse_byte_range(
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        size,
    );
    if range == Some(ByteRange::Invalid) {
        return with_header(
            base(StatusCode::RANGE_NOT_SATISFIABLE, Body::empty()),
            header::CONTENT_RANGE,
            &format!("bytes */{size}"),
        );
    }
    if method == Method::HEAD {
        return with_header(
            base(StatusCode::OK, Body::empty()),
            header::CONTENT_LENGTH,
            &size.to_string(),
        );
    }
    let Ok(mut file_handle) = tokio::fs::File::open(&path).await else {
        return not_found();
    };
    match range {
        Some(ByteRange::Range { start, end }) => {
            if file_handle.seek(SeekFrom::Start(start)).await.is_err() {
                return not_found();
            }
            let length = end - start + 1;
            let stream = tokio_util::io::ReaderStream::new(file_handle.take(length));
            let response = base(StatusCode::PARTIAL_CONTENT, Body::from_stream(stream));
            let response = with_header(
                response,
                header::CONTENT_RANGE,
                &format!("bytes {start}-{end}/{size}"),
            );
            with_header(response, header::CONTENT_LENGTH, &length.to_string())
        }
        _ => {
            let stream = tokio_util::io::ReaderStream::new(file_handle);
            with_header(
                base(StatusCode::OK, Body::from_stream(stream)),
                header::CONTENT_LENGTH,
                &size.to_string(),
            )
        }
    }
}

async fn logo(State(state): State<RouteState>) -> Response {
    match tokio::fs::read(&state.service.deps.logo_path).await {
        Ok(bytes) => {
            let mut response = (StatusCode::OK, Body::from(bytes)).into_response();
            let out = response.headers_mut();
            out.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
            out.insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
            response
        }
        Err(_) => (StatusCode::NOT_FOUND, Body::empty()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Web UI routes
// ---------------------------------------------------------------------------

async fn list(State(state): State<RouteState>) -> Response {
    let persistence = state.service.persistence();
    let (episodes, jobs) = tokio::join!(persistence.get_all_episodes(), persistence.get_all_jobs());
    match (episodes, jobs) {
        (Ok(episodes), Ok(jobs)) => json(
            StatusCode::OK,
            &api::PressPodsListResponse {
                episodes: episodes.iter().map(episode_dto).collect(),
                jobs: jobs.iter().map(job_dto).collect(),
            },
        ),
        (Err(error), _) | (_, Err(error)) => ApiError::internal(error).into_response(),
    }
}

async fn detail(State(state): State<RouteState>, Path(id): Path<String>) -> Response {
    match state.service.persistence().get_episode(&id).await {
        Ok(Some(episode)) => json(
            StatusCode::OK,
            &api::PressPodsEpisodeResponse {
                episode: episode_detail_dto(&episode),
            },
        ),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "Unknown episode"),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

async fn delete_episode(State(state): State<RouteState>, Path(id): Path<String>) -> Response {
    match state.service.persistence().delete_episode(&id).await {
        Ok(Some(deleted)) => {
            state
                .service
                .audio()
                .delete_episode_audio(&deleted.audio_file)
                .await;
            tracing::info!(target: LOG, "Deleted episode {} (\"{}\")", deleted.episode_id, deleted.title);
            json(
                StatusCode::OK,
                &api::PressPodsDeletedResponse { deleted: true },
            )
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "Unknown episode"),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

async fn retry_episode(State(state): State<RouteState>, Path(id): Path<String>) -> Response {
    let episode = match state.service.persistence().get_episode(&id).await {
        Ok(Some(episode)) => episode,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "Unknown episode"),
        Err(error) => return ApiError::internal(error).into_response(),
    };
    match state.service.submit_episode_url(&episode.article_url).await {
        Ok(job) => json(
            StatusCode::ACCEPTED,
            &api::PressPodsJobResponse { job: job_dto(&job) },
        ),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

async fn submit_ui(State(state): State<RouteState>, body: Bytes) -> Response {
    let Some(url) = decode_submit_url(&body) else {
        return api_error(StatusCode::BAD_REQUEST, "A valid article URL is required");
    };
    match state.service.submit_episode_url(&url).await {
        Ok(job) => json(
            StatusCode::ACCEPTED,
            &api::PressPodsJobResponse { job: job_dto(&job) },
        ),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

async fn retry_job(State(state): State<RouteState>, Path(job_id): Path<String>) -> Response {
    let persistence = state.service.persistence();
    match persistence.get_job(&job_id).await {
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "Unknown job"),
        Ok(Some(job)) if job.status != JobStatus::Failed => {
            return api_error(StatusCode::CONFLICT, "Only failed jobs can be retried");
        }
        Ok(Some(_)) => {}
        Err(error) => return ApiError::internal(error).into_response(),
    }
    match persistence.requeue_job_now(&job_id).await {
        Ok(Some(job)) => match state.service.kick_worker() {
            Ok(()) => json(
                StatusCode::OK,
                &api::PressPodsJobResponse { job: job_dto(&job) },
            ),
            Err(error) => ApiError::internal(error).into_response(),
        },
        Ok(None) => api_error(StatusCode::NOT_FOUND, "Unknown job"),
        Err(error) => ApiError::internal(error).into_response(),
    }
}

async fn delete_job(State(state): State<RouteState>, Path(job_id): Path<String>) -> Response {
    let persistence = state.service.persistence();
    let job = match persistence.get_job(&job_id).await {
        Ok(Some(job)) => job,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "Unknown job"),
        Err(error) => return ApiError::internal(error).into_response(),
    };
    if job.status == JobStatus::Processing {
        return api_error(StatusCode::CONFLICT, "Job is currently processing");
    }
    if let Err(error) = persistence.delete_job(&job_id).await {
        return ApiError::internal(error).into_response();
    }
    // Dismissing a job gives up on it, including its resume cache.
    state
        .service
        .audio()
        .clear_chunk_checkpoints(&checkpoint_work_id(&job_normalized_url(&job)))
        .await;
    json(
        StatusCode::OK,
        &api::PressPodsDeletedResponse { deleted: true },
    )
}

#[cfg(test)]
mod routes_spec {
    //! Byte-range parsing cases.
    use super::*;

    fn range(start: u64, end: u64) -> Option<ByteRange> {
        Some(ByteRange::Range { start, end })
    }

    #[test]
    fn returns_undefined_without_a_range_header() {
        assert_eq!(parse_byte_range(None, 100), None);
    }

    #[test]
    fn parses_a_bounded_range() {
        assert_eq!(parse_byte_range(Some("bytes=0-49"), 100), range(0, 49));
    }

    #[test]
    fn clamps_the_end_to_the_file_size() {
        assert_eq!(parse_byte_range(Some("bytes=50-1000"), 100), range(50, 99));
    }

    #[test]
    fn parses_an_open_ended_range() {
        assert_eq!(parse_byte_range(Some("bytes=10-"), 100), range(10, 99));
    }

    #[test]
    fn parses_a_suffix_range() {
        assert_eq!(parse_byte_range(Some("bytes=-25"), 100), range(75, 99));
    }

    #[test]
    fn rejects_out_of_bounds_and_malformed_ranges() {
        for header in ["bytes=100-", "bytes=-0", "bytes=-", "chunks=0-5"] {
            assert_eq!(
                parse_byte_range(Some(header), 100),
                Some(ByteRange::Invalid),
                "{header}"
            );
        }
    }

    #[test]
    fn js_numbers_drop_integral_fractions() {
        let value = serde_json::json!({"a": 1.0, "b": [2.5, 3.0], "c": 7});
        assert_eq!(
            js_numbers(value).to_string(),
            r#"{"a":1,"b":[2.5,3],"c":7}"#
        );
    }

    #[test]
    fn rounds_cost_cents_like_math_round() {
        assert_eq!(round_cents(1.234), 1.23);
        assert_eq!(round_cents(1.235), 1.24);
        assert_eq!(round_cents(0.3049), 0.3);
    }
}
