//! `GET /api/failures`: one list of what is failing across Omni, for the
//! Failures page: tasks whose last run failed or was degraded, failed and
//! invalid tool calls and dead event deliveries from the last week, and
//! integrations in error.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use omni_api::failures::{FailureCounts, FailureItem, FailureKind, FailuresResponse, PATH};
use omni_api::mcp_activity::McpCallStatus;
use omni_api::runs::RunStatus;
use omni_runtime::AppContext;
use omni_server_kit::{ApiError, ApiResult};
use omni_store::entity::EntityOps as _;

use crate::activity::McpCallData;
use crate::events::persistence::DeliveryStatus;
use crate::events::service::McpEventService;
use crate::gateway::Gateway;

const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const MAX_ITEMS: usize = 200;
const DETAIL_CHARS: usize = 300;

#[derive(Clone)]
struct FailuresState {
    ctx: AppContext,
    events: McpEventService,
    gateway: Gateway,
}

pub fn router(ctx: AppContext, events: McpEventService, gateway: Gateway) -> Router {
    Router::new()
        .route(PATH, get(failures))
        .with_state(FailuresState {
            ctx,
            events,
            gateway,
        })
}

fn bounded(text: &str) -> String {
    crate::tools::truncate(text, DETAIL_CHARS).0
}

fn count(items: &[FailureItem], kind: FailureKind) -> u32 {
    u32::try_from(items.iter().filter(|i| i.kind == kind).count()).unwrap_or(u32::MAX)
}

async fn failures(State(state): State<FailuresState>) -> ApiResult<FailuresResponse> {
    let now = state.ctx.clock.now_ms();
    let since = now - WEEK_MS;
    let mut items = Vec::new();

    for task in state.ctx.tasks.list().await.map_err(ApiError::internal)? {
        let Some(run) = task.last_run else { continue };
        if !matches!(run.status, RunStatus::Error | RunStatus::Degraded) {
            continue;
        }
        items.push(FailureItem {
            kind: FailureKind::Task,
            title: task.display_name.unwrap_or_else(|| task.name.clone()),
            detail: bounded(run.error.as_deref().unwrap_or("failed")),
            at: run.finished_at.unwrap_or(run.started_at),
            link: format!(
                "/operations#inspect={}",
                omni_api::common::encode_uri_component(&task.name)
            ),
        });
    }

    let calls = state
        .ctx
        .store
        .read(|docs| docs.get_all::<McpCallData>())
        .await
        .map_err(ApiError::internal)?;
    for call in calls {
        if call.started_at < since
            || !matches!(
                call.status,
                McpCallStatus::Error | McpCallStatus::Invalid | McpCallStatus::Interrupted
            )
        {
            continue;
        }
        let mut detail = bounded(call.error.as_deref().unwrap_or("failed"));
        if let Some(caller) = &call.caller {
            detail.push_str(&format!(" · {caller}"));
        }
        items.push(FailureItem {
            kind: FailureKind::ToolCall,
            title: call.tool.clone(),
            detail,
            at: call.started_at,
            link: "/mcp-activity".to_owned(),
        });
    }

    let deliveries = state
        .events
        .store()
        .deliveries()
        .await
        .map_err(ApiError::internal)?;
    for row in deliveries {
        if row.status != DeliveryStatus::Failed || row.updated_at < since {
            continue;
        }
        let destination = row
            .target
            .clone()
            .unwrap_or_else(|| "a subscriber".to_owned());
        let reason = row.failure.map_or("failed", |f| f.as_str());
        let last = row
            .history
            .last()
            .and_then(|a| a.error.clone())
            .map(|e| format!(" ({e})"))
            .unwrap_or_default();
        items.push(FailureItem {
            kind: FailureKind::Delivery,
            title: format!("{} → {destination}", row.name),
            detail: format!("{reason}{last}, {} attempt(s)", row.attempts),
            at: row.updated_at,
            link: "/gateway#gateway-outbound".to_owned(),
        });
    }

    for runtime in state.gateway.runtimes() {
        let config = runtime.config();
        if !config.enabled {
            continue;
        }
        let health = runtime.health();
        let Some(error) = &health.error else { continue };
        items.push(FailureItem {
            kind: FailureKind::Integration,
            title: config.name.clone(),
            detail: bounded(&error.message),
            at: error.at,
            link: format!("/gateway?integration={}", config.id),
        });
    }

    items.sort_by_key(|item| std::cmp::Reverse(item.at));
    let counts = FailureCounts {
        tasks: count(&items, FailureKind::Task),
        tool_calls: count(&items, FailureKind::ToolCall),
        deliveries: count(&items, FailureKind::Delivery),
        integrations: count(&items, FailureKind::Integration),
    };
    items.truncate(MAX_ITEMS);
    Ok(Json(FailuresResponse {
        checked_at: now,
        items,
        counts,
    }))
}
