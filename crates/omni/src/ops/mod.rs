//! Ops routes owned by the binary: health, tasks, task runs,
//! run logs and their SSE tail, the dashboard snapshot and its SSE hub,
//! costs, and the data manager.

pub mod dashboard;
pub mod run_logs;

use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use jiff::tz::TimeZone;
use omni_ai::costs::CostEventData;
use omni_api::costs::CostRange;
use omni_api::data::EntitiesResponse;
use omni_runtime::AppContext;
use omni_store::EntityOps as _;
use omni_tasks::RunNowError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::data_manager::{DataManager, DeleteResult};
use crate::json::{error, js_json, js_json_of};

pub use dashboard::Dashboard;

const LOG: &str = "Main:Server";

/// Shared state of the ops routes.
#[derive(Clone)]
pub struct OpsState {
    pub ctx: AppContext,
    pub dashboard: Dashboard,
    pub data: DataManager,
    pub tz: TimeZone,
}

impl OpsState {
    pub fn new(ctx: AppContext, data: DataManager) -> Self {
        let tz = TimeZone::get(&ctx.config.tz).unwrap_or(TimeZone::UTC);
        Self {
            dashboard: Dashboard::new(ctx.clone()),
            ctx,
            data,
            tz,
        }
    }
}

/// Every ops route at its absolute path.
pub fn router(state: OpsState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/tasks", get(tasks))
        .route("/api/tasks/{name}/run", post(run_task))
        .route("/api/task-runs", get(task_runs))
        .route("/api/task-runs/{run_id}/logs", get(run_logs::logs))
        .route("/api/task-runs/{run_id}/logs/stream", get(run_logs::stream))
        .route("/api/snapshot", get(dashboard::snapshot_route))
        .route("/api/events", get(dashboard::events_route))
        .route("/api/costs", get(costs))
        .route("/api/data/entities", get(data_entities))
        .route(
            "/api/data/entities/{slug}",
            get(data_entity).delete(delete_data_row),
        )
        .with_state(state)
}

fn internal(error: impl std::fmt::Display) -> Response {
    omni_server_kit::ApiError::internal(error).into_response()
}

async fn health(State(state): State<OpsState>) -> Response {
    let body = omni_api::build::HealthResponse {
        status: "ok".to_owned(),
        build: state.dashboard.build().clone(),
    };
    js_json_of(StatusCode::OK, &body)
}

async fn tasks(State(state): State<OpsState>) -> Response {
    match state.ctx.tasks.list().await {
        Ok(tasks) => js_json_of(StatusCode::OK, &json!({ "tasks": tasks })),
        Err(e) => internal(e),
    }
}

pub fn run_now_error_response(error: RunNowError) -> Response {
    match error {
        RunNowError::NotFound { .. } => error_response(StatusCode::NOT_FOUND, &error),
        RunNowError::AlreadyRunning { .. } => error_response(StatusCode::CONFLICT, &error),
        RunNowError::ManualInputUnsupported { .. } => {
            error_response(StatusCode::BAD_REQUEST, &error)
        }
        other => internal(other),
    }
}

fn error_response(status: StatusCode, e: &RunNowError) -> Response {
    error(status, e.to_string())
}

async fn run_task(State(state): State<OpsState>, Path(name): Path<String>) -> Response {
    match state.ctx.tasks.run_now(&name, None) {
        Ok(run_id) => {
            tracing::info!(target: LOG, "Manual run requested for \"{name}\"");
            js_json(StatusCode::ACCEPTED, &json!({ "runId": run_id }))
        }
        Err(e) => run_now_error_response(e),
    }
}

#[derive(Debug, Default, Deserialize)]
struct RunsQuery {
    task: Option<String>,
    limit: Option<String>,
}

/// `Number(param)` as a positive integer, else `None`.
pub fn positive_integer(param: Option<&str>) -> Option<u64> {
    // `Number(undefined)` is NaN; `Number("")` is 0: neither is accepted.
    let n = omni_core::js::string_to_number(param?);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (n.is_finite() && n.fract() == 0.0 && n > 0.0).then(|| n.min(u64::MAX as f64) as u64)
}

async fn task_runs(State(state): State<OpsState>, query: Query<RunsQuery>) -> Response {
    let limit = positive_integer(query.limit.as_deref()).map_or(50, |n| n.min(200));
    let task = query.task.as_deref().filter(|t| !t.is_empty());
    #[allow(clippy::cast_possible_truncation)]
    let limit = limit as usize;
    match omni_tasks::persistence::get_runs(&state.ctx.store, task, limit).await {
        Ok(runs) => {
            let runs: Vec<omni_api::runs::Run> = runs.iter().map(Into::into).collect();
            js_json_of(StatusCode::OK, &json!({ "runs": runs }))
        }
        Err(e) => internal(e),
    }
}

#[derive(Debug, Default, Deserialize)]
struct CostsQuery {
    days: Option<String>,
}

/// `?days=`: `all`, or a number equal to 7, 30 or 90 (default 30).
pub fn cost_range(days: Option<&str>) -> Option<CostRange> {
    let value = days.unwrap_or("30");
    if value == "all" {
        return Some(CostRange::All);
    }
    let n = omni_core::js::string_to_number(value);
    [7u32, 30, 90]
        .into_iter()
        .find(|d| f64::from(*d) == n)
        .map(CostRange::Days)
}

async fn costs(State(state): State<OpsState>, query: Query<CostsQuery>) -> Response {
    let Some(range) = cost_range(query.days.as_deref()) else {
        return error(StatusCode::BAD_REQUEST, "days must be 7, 30, 90, or all");
    };
    let events = match state
        .ctx
        .store
        .read(|docs| docs.get_all::<CostEventData>())
        .await
    {
        Ok(events) => events,
        Err(e) => return internal(e),
    };
    let summary = omni_ai::costs::summarize(&events, range, state.ctx.clock.now_ms(), &state.tz);
    js_json_of(StatusCode::OK, &summary)
}

async fn data_entities(State(state): State<OpsState>) -> Response {
    let entities = match state.data.list().await {
        Ok(entities) => entities,
        Err(e) => return internal(e),
    };
    match state.data.storage(&entities).await {
        Ok(storage) => js_json_of(StatusCode::OK, &EntitiesResponse { entities, storage }),
        Err(e) => internal(e),
    }
}

async fn data_entity(State(state): State<OpsState>, Path(slug): Path<String>) -> Response {
    match state.data.rows(&slug).await {
        Ok(Some(rows)) => js_json_of(StatusCode::OK, &rows),
        Ok(None) => error(StatusCode::NOT_FOUND, "Unknown entity"),
        Err(e) => internal(e),
    }
}

/// Reads a JSON body with the 64 KiB cap; invalid JSON reads as `null`.
async fn read_body(request: Request) -> Result<Value, Response> {
    let too_large = || error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
    let declared = request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let limit = omni_server_kit::JSON_BODY_LIMIT;
    if declared.is_some_and(|len| len > limit as u64) {
        return Err(too_large());
    }
    let bytes = axum::body::to_bytes(request.into_body(), limit)
        .await
        .map_err(|_| too_large())?;
    Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn delete_data_row(
    State(state): State<OpsState>,
    Path(slug): Path<String>,
    request: Request,
) -> Response {
    let body = match read_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(key) = body.get("key").and_then(Value::as_object).cloned() else {
        return error(StatusCode::BAD_REQUEST, "A primary key object is required");
    };
    match state.data.delete(&slug, &key).await {
        Ok(None) => error(StatusCode::NOT_FOUND, "Unknown entity"),
        Ok(Some(DeleteResult::InvalidKey)) => error(
            StatusCode::BAD_REQUEST,
            "The primary key does not match this entity",
        ),
        Ok(Some(DeleteResult::NotFound)) => error(StatusCode::NOT_FOUND, "Row not found"),
        Ok(Some(DeleteResult::Blocked(reason))) => error(StatusCode::CONFLICT, reason),
        Ok(Some(DeleteResult::Deleted(_))) => {
            let shown = omni_core::js::json_stringify(&Value::Object(key));
            tracing::info!(target: LOG, key = %shown, "Deleted row from \"{slug}\"");
            state.dashboard.broadcast().await;
            js_json(StatusCode::OK, &json!({ "deleted": true }))
        }
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_parse_like_number() {
        assert_eq!(positive_integer(Some("5")), Some(5));
        assert_eq!(positive_integer(Some("5.0")), Some(5));
        assert_eq!(positive_integer(Some(" 7 ")), Some(7));
        assert_eq!(positive_integer(Some("0")), None);
        assert_eq!(positive_integer(Some("1.5")), None);
        assert_eq!(positive_integer(Some("")), None);
        assert_eq!(positive_integer(None), None);
    }

    #[test]
    fn cost_ranges_follow_the_range_rule() {
        assert_eq!(cost_range(None), Some(CostRange::Days(30)));
        assert_eq!(cost_range(Some("all")), Some(CostRange::All));
        assert_eq!(cost_range(Some("7")), Some(CostRange::Days(7)));
        assert_eq!(cost_range(Some("90.0")), Some(CostRange::Days(90)));
        assert_eq!(cost_range(Some("14")), None);
        assert_eq!(cost_range(Some("")), None);
    }
}
