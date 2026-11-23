//! Streamer routes: `GET /api/streamers`,
//! `GET /api/trigger-channels`, `GET /api/streamers/:id/metrics`,
//! `GET /api/streamers/:id/sessions`, and the Kick relay:
//! `GET /api/streamers/:id/relay` (status), `POST` to it (start; a mutation,
//! so the same-origin guard keeps other sites from starting relays),
//! `GET /api/streamers/:id/relay/:generation/index.m3u8` and
//! `GET /api/streamers/:id/relay/:generation/:sequence.ts`.

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use omni_api::streamers::{
    StreamRelayStatus, StreamSessionsResponse, StreamerMetricsResponse, StreamersResponse,
    TriggerChannelsResponse, paths,
};
use omni_server_kit::{ApiError, ApiResult};
use omni_store::Store;

use crate::display::{display_views, metrics_view, session_view};
use crate::error::LiveError;
use crate::metrics::{get_platform_viewer_metrics, get_viewer_metrics};
use crate::platform::Platform;
use crate::relay::{KickRelays, PlaylistLookup};
use crate::sessions::get_sessions;
use crate::streamers::Roster;
use crate::trigger_channels::to_trigger_channels;

#[derive(Clone)]
pub struct LiveRoutesState {
    pub store: Store,
    pub roster: Roster,
    pub relays: KickRelays,
}

/// Logged once (at ERROR) by `ApiError`, rendered as the opaque 500.
fn internal(error: LiveError) -> ApiError {
    ApiError::internal(error)
}

fn unknown_streamer() -> ApiError {
    ApiError::not_found("Unknown streamer")
}

async fn streamers(State(state): State<LiveRoutesState>) -> ApiResult<StreamersResponse> {
    let streamers = display_views(&state.store, &state.roster.snapshot())
        .await
        .map_err(internal)?;
    Ok(Json(StreamersResponse { streamers }))
}

async fn trigger_channels(State(state): State<LiveRoutesState>) -> Json<TriggerChannelsResponse> {
    Json(TriggerChannelsResponse {
        channels: to_trigger_channels(&state.roster.snapshot()),
    })
}

async fn metrics(
    State(state): State<LiveRoutesState>,
    Path(id): Path<String>,
) -> ApiResult<StreamerMetricsResponse> {
    if !state.roster.contains(&id) {
        return Err(unknown_streamer());
    }
    let aggregate = get_viewer_metrics(&state.store, &id)
        .await
        .map_err(internal)?;
    let platforms = get_platform_viewer_metrics(&state.store, &id)
        .await
        .map_err(internal)?;
    Ok(Json(metrics_view(&aggregate, &platforms)))
}

async fn sessions(
    State(state): State<LiveRoutesState>,
    Path(id): Path<String>,
) -> ApiResult<StreamSessionsResponse> {
    if !state.roster.contains(&id) {
        return Err(unknown_streamer());
    }
    let data = get_sessions(&state.store, &id).await.map_err(internal)?;
    Ok(Json(StreamSessionsResponse {
        sessions: data.sessions.iter().rev().map(session_view).collect(),
    }))
}

/// The streamer's Kick channel slug.
fn kick_slug(state: &LiveRoutesState, id: &str) -> Result<String, ApiError> {
    let streamer = state.roster.get(id).ok_or_else(unknown_streamer)?;
    streamer
        .bindings
        .iter()
        .find(|b| b.platform == Platform::Kick && !b.username.is_empty())
        .map(|b| b.username.to_ascii_lowercase())
        .ok_or_else(|| ApiError::not_found("Streamer has no Kick channel"))
}

async fn relay_status(
    State(state): State<LiveRoutesState>,
    Path(id): Path<String>,
) -> ApiResult<StreamRelayStatus> {
    let slug = kick_slug(&state, &id)?;
    Ok(Json(state.relays.status(&slug, |generation| {
        paths::streamer_relay_playlist(&id, generation)
    })))
}

async fn relay_start(
    State(state): State<LiveRoutesState>,
    Path(id): Path<String>,
) -> ApiResult<StreamRelayStatus> {
    let slug = kick_slug(&state, &id)?;
    Ok(Json(state.relays.start(&slug, |generation| {
        paths::streamer_relay_playlist(&id, generation)
    })))
}

async fn relay_playlist(
    State(state): State<LiveRoutesState>,
    Path((id, generation)): Path<(String, u64)>,
) -> Response {
    let slug = match kick_slug(&state, &id) {
        Ok(slug) => slug,
        Err(error) => return error.into_response(),
    };
    match state.relays.playlist(&slug, generation) {
        PlaylistLookup::Ready(text) => (
            [
                (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            text,
        )
            .into_response(),
        PlaylistLookup::NotReady => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Relay is still buffering")
                .into_response()
        }
        PlaylistLookup::Gone => ApiError::not_found("Relay stopped").into_response(),
    }
}

async fn relay_segment(
    State(state): State<LiveRoutesState>,
    Path((id, generation, file)): Path<(String, u64, String)>,
) -> Response {
    let slug = match kick_slug(&state, &id) {
        Ok(slug) => slug,
        Err(error) => return error.into_response(),
    };
    let Some(sequence) = file.strip_suffix(".ts").and_then(|s| s.parse().ok()) else {
        return ApiError::not_found("Unknown segment").into_response();
    };
    match state.relays.segment(&slug, generation, sequence) {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "video/mp2t"),
                (header::CACHE_CONTROL, "private, max-age=60"),
            ],
            bytes,
        )
            .into_response(),
        None => ApiError::not_found("Unknown segment").into_response(),
    }
}

/// The streamer routes with state applied.
pub fn router(state: LiveRoutesState) -> Router {
    Router::new()
        .route("/api/streamers", get(streamers))
        .route("/api/trigger-channels", get(trigger_channels))
        .route("/api/streamers/{id}/metrics", get(metrics))
        .route("/api/streamers/{id}/sessions", get(sessions))
        .route(
            "/api/streamers/{id}/relay",
            get(relay_status).post(relay_start),
        )
        .route(
            "/api/streamers/{id}/relay/{generation}/index.m3u8",
            get(relay_playlist),
        )
        .route(
            "/api/streamers/{id}/relay/{generation}/{segment}",
            get(relay_segment),
        )
        .with_state(state)
}
