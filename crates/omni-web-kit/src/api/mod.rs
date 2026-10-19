//! Typed API client functions. DTOs come from
//! `omni-api`.

pub mod client;

use omni_api::calendar::{CalendarEventsResponse, CalendarStatusResponse};
use omni_api::claude::{
    ClaudeActivityResponse, ClaudeLinkFailure, ClaudeSessionsResponse, ClaudeTranscriptResponse,
};
use omni_api::common::encode_uri_component as enc;
use omni_api::costs::{CostRange, CostsResponse};
use omni_api::data::{DeleteRowResponse, EntitiesResponse, EntityRowsResponse};
use omni_api::email::{
    DeletedResponse, EmailActivitiesResponse, EmailActivityLogsResponse, EmailActivityResponse,
    EmailFeedbackListResponse, EmailFeedbackResponse, EmailFeedbackVerdict, EmailPipelineName,
    EmailRuleInput, EmailRuleUpsertResponse, EmailRulesResponse,
};
use omni_api::gateway::{
    AuthorizationApproval, ClientTokenCreate, ClientTokenCreated, CredentialsUpdate,
    GatewayOverview, IntegrationCreate, IntegrationDetail, IntegrationPatch, OAuthStartResponse,
    ToolVisibilityUpdate,
};
use omni_api::intelligence::{FeedbackResponse, FeedbackVerdict, IntelligenceDetailsResponse};
use omni_api::mcp_activity::{McpActivityResponse, McpCallStatus};
use omni_api::media::{
    RecommendationFeedback, RecommendationResponse, RecommendationsResponse, RunResponse,
    TasteProfileResponse,
};
use omni_api::parcels::{PARCELS, ParcelsResponse};
use omni_api::pets::PetHealthResponse;
use omni_api::podcasts::{
    PodcastFeedback, PodcastRecommendationResponse, PodcastRecommendationsResponse,
    PodcastTasteProfileResponse,
};
use omni_api::presspods::{
    PressPodsDeletedResponse, PressPodsEpisodeResponse, PressPodsJobResponse, PressPodsListResponse,
};
use omni_api::runs::{RunLogsResponse, RunsResponse};
use omni_api::streamer_config::{
    LiveSettings, StreamerConfigCreate, StreamerConfigOrder, StreamerConfigPatch,
    StreamerConfigResponse, StreamerConfigView,
};
use omni_api::streamers::{StreamSessionsResponse, StreamerMetricsResponse};
use omni_api::stylist::StylistOverview;
use omni_api::tasks::{RunNowResponse, TasksResponse};
use serde_json::{Map, Value, json};

pub use client::{ApiClientError, NO_BODY, delete, get, patch, post, put};
pub use omni_api::data::{DataRow, ManagedDataSummary, ManagedEntitySummary};
pub use omni_api::snapshot::Snapshot;

/// Builds `?a=1&b=2` (`URLSearchParams` order and encoding of plain values).
fn query(params: &[(&str, Option<String>)]) -> String {
    let parts: Vec<String> = params
        .iter()
        .filter_map(|(key, value)| value.as_ref().map(|v| format!("{key}={}", form_encode(v))))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// `application/x-www-form-urlencoded` serialization, as `URLSearchParams` does.
pub fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ----- tasks, runs, snapshot, data -----

pub async fn fetch_tasks() -> Result<TasksResponse, ApiClientError> {
    get("/api/tasks").await
}

pub async fn fetch_snapshot() -> Result<Snapshot, ApiClientError> {
    get("/api/snapshot").await
}

pub async fn fetch_data_entities() -> Result<EntitiesResponse, ApiClientError> {
    get("/api/data/entities").await
}

pub async fn fetch_data_rows(slug: &str) -> Result<EntityRowsResponse, ApiClientError> {
    get(&format!("/api/data/entities/{}", enc(slug))).await
}

pub async fn delete_data_row(
    slug: &str,
    key: &DataRow,
) -> Result<DeleteRowResponse, ApiClientError> {
    delete(
        &format!("/api/data/entities/{}", enc(slug)),
        Some(&json!({ "key": key })),
    )
    .await
}

pub async fn fetch_task_runs(
    task: Option<&str>,
    limit: Option<u32>,
) -> Result<RunsResponse, ApiClientError> {
    let q = query(&[
        ("task", task.filter(|t| !t.is_empty()).map(str::to_owned)),
        ("limit", limit.map(|l| l.to_string())),
    ]);
    get(&format!("/api/task-runs{q}")).await
}

pub async fn fetch_run_logs(run_id: &str) -> Result<RunLogsResponse, ApiClientError> {
    get(&format!("/api/task-runs/{}/logs", enc(run_id))).await
}

pub fn run_log_stream_url(run_id: &str) -> String {
    format!("/api/task-runs/{}/logs/stream", enc(run_id))
}

/// `POST /api/tasks/:name/run`, or the recommendation run routes when a
/// `max_recommendations` override is given.
pub async fn run_task_request(
    name: &str,
    max_recommendations: Option<u32>,
) -> Result<RunNowResponse, ApiClientError> {
    if let Some(max) = max_recommendations {
        let path = if name == "PodcastRecs" {
            "/api/podcast-recommendations/run"
        } else {
            "/api/recommendations/run"
        };
        let accepted: RunResponse = post(path, Some(&json!({ "maxRecommendations": max }))).await?;
        return Ok(RunNowResponse {
            run_id: accepted.run_id,
        });
    }
    post(&format!("/api/tasks/{}/run", enc(name)), NO_BODY).await
}

// ----- personal: pets, parcels, calendar -----

pub async fn fetch_pet_health() -> Result<PetHealthResponse, ApiClientError> {
    get("/api/pets/health").await
}

/// Omni's cached Parcel read; never calls Parcel.
pub async fn fetch_parcels() -> Result<ParcelsResponse, ApiClientError> {
    get(PARCELS).await
}

pub async fn fetch_calendar_status() -> Result<CalendarStatusResponse, ApiClientError> {
    get(omni_api::calendar::paths::STATUS).await
}

/// Occurrences in `[from, to)`; both are local `YYYY-MM-DD` dates in the
/// calendar's zone.
pub async fn fetch_calendar_events(
    from: &str,
    to: &str,
) -> Result<CalendarEventsResponse, ApiClientError> {
    let q = query(&[("from", Some(from.to_owned())), ("to", Some(to.to_owned()))]);
    get(&format!("{}{q}", omni_api::calendar::paths::EVENTS)).await
}

/// `GET /api/stylist` for `namespace` (`real` or a `fixture-*` namespace).
pub fn stylist_path(namespace: &str) -> String {
    let q = query(&[("namespace", Some(namespace.to_owned()))]);
    format!("/api/stylist{q}")
}

/// The read-only stylist ledger overview. It never carries evidence bytes.
pub async fn fetch_stylist(namespace: &str) -> Result<StylistOverview, ApiClientError> {
    get(&stylist_path(namespace)).await
}

// ----- streamers -----

/// Configured streamers in display order, plus the global live settings.
pub async fn fetch_streamer_config() -> Result<StreamerConfigResponse, ApiClientError> {
    get("/api/streamer-config").await
}

pub async fn create_streamer_config(
    body: &StreamerConfigCreate,
) -> Result<StreamerConfigView, ApiClientError> {
    post("/api/streamer-config/streamers", Some(body)).await
}

/// Absent patch fields stay unchanged.
pub async fn update_streamer_config(
    id: &str,
    body: &StreamerConfigPatch,
) -> Result<StreamerConfigView, ApiClientError> {
    patch(
        &format!("/api/streamer-config/streamers/{}", enc(id)),
        Some(body),
    )
    .await
}

pub async fn delete_streamer_config(id: &str) -> Result<StreamerConfigView, ApiClientError> {
    delete(
        &format!("/api/streamer-config/streamers/{}", enc(id)),
        NO_BODY,
    )
    .await
}

/// `ids` lists every configured streamer in the new order.
pub async fn reorder_streamer_config(
    ids: Vec<String>,
) -> Result<StreamerConfigResponse, ApiClientError> {
    put(
        "/api/streamer-config/order",
        Some(&StreamerConfigOrder { ids }),
    )
    .await
}

pub async fn save_live_settings(settings: LiveSettings) -> Result<LiveSettings, ApiClientError> {
    put("/api/streamer-config/settings", Some(&settings)).await
}

pub async fn fetch_streamer_metrics(id: &str) -> Result<StreamerMetricsResponse, ApiClientError> {
    get(&format!("/api/streamers/{}/metrics", enc(id))).await
}

pub async fn fetch_streamer_sessions(id: &str) -> Result<StreamSessionsResponse, ApiClientError> {
    get(&format!("/api/streamers/{}/sessions", enc(id))).await
}

pub async fn fetch_livestream_intelligence_details(
    id: &str,
    limit: u32,
) -> Result<IntelligenceDetailsResponse, ApiClientError> {
    get(&format!(
        "/api/streamers/{}/intelligence-details?limit={limit}",
        enc(id)
    ))
    .await
}

pub async fn submit_livestream_feedback(
    streamer_id: &str,
    alert_id: &str,
    verdict: FeedbackVerdict,
    note: Option<&str>,
) -> Result<FeedbackResponse, ApiClientError> {
    let mut body = Map::new();
    body.insert("alertId".into(), json!(alert_id));
    body.insert(
        "verdict".into(),
        serde_json::to_value(verdict).unwrap_or(Value::Null),
    );
    if let Some(note) = note {
        body.insert("note".into(), json!(note));
    }
    post(
        &format!("/api/streamers/{}/intelligence-feedback", enc(streamer_id)),
        Some(&Value::Object(body)),
    )
    .await
}

// ----- media recommendations -----

pub async fn fetch_recommendations() -> Result<RecommendationsResponse, ApiClientError> {
    get("/api/recommendations").await
}

pub async fn fetch_taste_profile() -> Result<TasteProfileResponse, ApiClientError> {
    get("/api/recommendations/taste-profile").await
}

pub async fn fetch_recommendation(id: &str) -> Result<RecommendationResponse, ApiClientError> {
    get(&format!("/api/recommendations/{}", enc(id))).await
}

fn feedback_body<F: serde::Serialize>(feedback: Option<F>, note: Option<&str>) -> Value {
    let mut body = Map::new();
    if let Some(feedback) = feedback {
        body.insert(
            "feedback".into(),
            serde_json::to_value(feedback).unwrap_or(Value::Null),
        );
    }
    if let Some(note) = note {
        body.insert("note".into(), json!(note));
    }
    Value::Object(body)
}

pub async fn send_recommendation_feedback(
    id: &str,
    feedback: Option<RecommendationFeedback>,
    note: Option<&str>,
) -> Result<RecommendationResponse, ApiClientError> {
    post(
        &format!("/api/recommendations/{}/feedback", enc(id)),
        Some(&feedback_body(feedback, note)),
    )
    .await
}

// ----- podcasts -----

pub async fn fetch_podcast_recommendations()
-> Result<PodcastRecommendationsResponse, ApiClientError> {
    get("/api/podcast-recommendations").await
}

pub async fn fetch_podcast_taste_profile() -> Result<PodcastTasteProfileResponse, ApiClientError> {
    get("/api/podcast-recommendations/taste-profile").await
}

pub async fn fetch_podcast_recommendation(
    id: &str,
) -> Result<PodcastRecommendationResponse, ApiClientError> {
    get(&format!("/api/podcast-recommendations/{}", enc(id))).await
}

pub async fn send_podcast_recommendation_feedback(
    id: &str,
    feedback: Option<PodcastFeedback>,
    note: Option<&str>,
) -> Result<PodcastRecommendationResponse, ApiClientError> {
    post(
        &format!("/api/podcast-recommendations/{}/feedback", enc(id)),
        Some(&feedback_body(feedback, note)),
    )
    .await
}

// ----- PressPods -----

pub async fn fetch_press_pods() -> Result<PressPodsListResponse, ApiClientError> {
    get("/api/press-pods/episodes").await
}

pub async fn fetch_press_pods_episode(
    episode_id: &str,
) -> Result<PressPodsEpisodeResponse, ApiClientError> {
    get(&format!("/api/press-pods/episodes/{}", enc(episode_id))).await
}

pub async fn submit_press_pods_url(url: &str) -> Result<PressPodsJobResponse, ApiClientError> {
    post("/api/press-pods/submit", Some(&json!({ "url": url }))).await
}

pub async fn retry_press_pods_job(job_id: &str) -> Result<PressPodsJobResponse, ApiClientError> {
    post(
        &format!("/api/press-pods/jobs/{}/retry", enc(job_id)),
        NO_BODY,
    )
    .await
}

pub async fn dismiss_press_pods_job(
    job_id: &str,
) -> Result<PressPodsDeletedResponse, ApiClientError> {
    delete(&format!("/api/press-pods/jobs/{}", enc(job_id)), NO_BODY).await
}

pub async fn delete_press_pods_episode(
    episode_id: &str,
) -> Result<PressPodsDeletedResponse, ApiClientError> {
    delete(
        &format!("/api/press-pods/episodes/{}", enc(episode_id)),
        NO_BODY,
    )
    .await
}

pub async fn retry_press_pods_episode(
    episode_id: &str,
) -> Result<PressPodsJobResponse, ApiClientError> {
    post(
        &format!("/api/press-pods/episodes/{}/retry", enc(episode_id)),
        NO_BODY,
    )
    .await
}

// ----- costs -----

/// `?days=` value of a range.
pub fn cost_range_param(range: CostRange) -> String {
    match range {
        CostRange::Days(days) => days.to_string(),
        CostRange::All => "all".to_owned(),
    }
}

pub async fn fetch_costs(range: CostRange) -> Result<CostsResponse, ApiClientError> {
    get(&format!("/api/costs?days={}", cost_range_param(range))).await
}

// ----- email -----

pub async fn fetch_email_activity(
    pipeline: Option<EmailPipelineName>,
    limit: Option<u32>,
) -> Result<EmailActivitiesResponse, ApiClientError> {
    let q = query(&[
        ("pipeline", pipeline.map(|p| p.as_str().to_owned())),
        ("limit", limit.map(|l| l.to_string())),
    ]);
    get(&format!("/api/email-activity{q}")).await
}

pub async fn fetch_email_activity_logs(
    activity_id: &str,
) -> Result<EmailActivityLogsResponse, ApiClientError> {
    get(&format!("/api/email-activity/{}/logs", enc(activity_id))).await
}

pub async fn fetch_email_rules() -> Result<EmailRulesResponse, ApiClientError> {
    get("/api/email-rules").await
}

pub async fn create_email_rule(
    input: &EmailRuleInput,
) -> Result<EmailRuleUpsertResponse, ApiClientError> {
    post("/api/email-rules", Some(input)).await
}

pub async fn delete_email_rule(rule_id: &str) -> Result<DeletedResponse, ApiClientError> {
    delete(&format!("/api/email-rules/{}", enc(rule_id)), NO_BODY).await
}

pub async fn fetch_email_feedback() -> Result<EmailFeedbackListResponse, ApiClientError> {
    get("/api/email-feedback").await
}

pub async fn send_email_activity_feedback(
    activity_id: &str,
    verdict: Option<EmailFeedbackVerdict>,
    note: Option<&str>,
) -> Result<EmailFeedbackResponse, ApiClientError> {
    let mut body = Map::new();
    body.insert(
        "verdict".into(),
        serde_json::to_value(verdict).unwrap_or(Value::Null),
    );
    if let Some(note) = note {
        body.insert("note".into(), json!(note));
    }
    post(
        &format!("/api/email-activity/{}/feedback", enc(activity_id)),
        Some(&Value::Object(body)),
    )
    .await
}

pub async fn reprocess_email_activity(
    activity_id: &str,
) -> Result<EmailActivityResponse, ApiClientError> {
    post(
        &format!("/api/email-activity/{}/reprocess", enc(activity_id)),
        NO_BODY,
    )
    .await
}

pub async fn forget_parcel_delivery(
    tracking_number: &str,
) -> Result<DeletedResponse, ApiClientError> {
    delete(
        &format!("/api/parcel-tracker/deliveries/{}", enc(tracking_number)),
        NO_BODY,
    )
    .await
}

// ----- MCP and Claude Code activity -----

pub async fn fetch_mcp_activity(
    limit: Option<u32>,
    tool: Option<&str>,
    status: Option<McpCallStatus>,
    before: Option<i64>,
) -> Result<McpActivityResponse, ApiClientError> {
    let status = status.map(|s| match s {
        McpCallStatus::Running => "running",
        McpCallStatus::Ok => "ok",
        McpCallStatus::Error => "error",
        McpCallStatus::Interrupted => "interrupted",
    });
    let q = query(&[
        ("limit", limit.map(|l| l.to_string())),
        ("tool", tool.filter(|t| !t.is_empty()).map(str::to_owned)),
        ("status", status.map(str::to_owned)),
        ("before", before.map(|b| b.to_string())),
    ]);
    get(&format!("/api/mcp/activity{q}")).await
}

pub async fn fetch_claude_activity(limit: u32) -> Result<ClaudeActivityResponse, ApiClientError> {
    get(&format!("/api/claude/activity?limit={limit}")).await
}

/// The host link answered 503 (not configured, offline, disabled) or reported
/// a failure (502); carries the server's message and code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeLinkError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

/// `ApiClientError | ClaudeLinkError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeLinkGetError {
    Link(ClaudeLinkError),
    Client(ApiClientError),
}

impl ClaudeLinkGetError {
    pub fn message(&self) -> &str {
        match self {
            ClaudeLinkGetError::Link(link) => &link.message,
            ClaudeLinkGetError::Client(client) => client.message(),
        }
    }
}

fn read_link_error(response: &client::RawResponse) -> ClaudeLinkError {
    #[derive(serde::Deserialize)]
    struct Body {
        error: String,
        #[serde(default)]
        code: Option<String>,
    }
    match serde_json::from_str::<Body>(&response.body) {
        Ok(body) => ClaudeLinkError {
            status: response.status,
            code: body.code.unwrap_or_else(|| "unavailable".to_owned()),
            message: body.error,
        },
        Err(_) => ClaudeLinkError {
            status: response.status,
            code: "unavailable".to_owned(),
            message: format!("HTTP {}: {}", response.status, response.status_text),
        },
    }
}

/// Live host queries skip the GET restart retry: a 503 usually means the link
/// is offline or disabled, and the page polls again on its own schedule.
async fn claude_link_get<T: serde::de::DeserializeOwned>(
    path: &str,
) -> Result<T, ClaudeLinkGetError> {
    let response = client::send(client::Method::Get, path, None)
        .await
        .map_err(ClaudeLinkGetError::Client)?;
    if response.status == 502 || response.status == 503 {
        return Err(ClaudeLinkGetError::Link(read_link_error(&response)));
    }
    client::decode_response(path, &response).map_err(ClaudeLinkGetError::Client)
}

/// Kept so a [`ClaudeLinkFailure`] body is the documented error shape.
pub type ClaudeLinkFailureBody = ClaudeLinkFailure;

pub async fn fetch_claude_sessions(
    include_stopped: bool,
    limit: u32,
) -> Result<ClaudeSessionsResponse, ClaudeLinkGetError> {
    claude_link_get(&format!(
        "/api/claude/sessions?includeStopped={include_stopped}&limit={limit}"
    ))
    .await
}

pub async fn fetch_claude_transcript(
    session_id: &str,
    limit: u32,
    cursor: Option<i64>,
) -> Result<ClaudeTranscriptResponse, ClaudeLinkGetError> {
    let cursor = cursor.map(|c| format!("&cursor={c}")).unwrap_or_default();
    claude_link_get(&format!(
        "/api/claude/sessions/{}/transcript?limit={limit}{cursor}",
        enc(session_id)
    ))
    .await
}

// ----- MCP gateway -----

/// A mutation that answers `204 No Content` (never retried).
async fn send_no_content(method: client::Method, path: &str) -> Result<(), ApiClientError> {
    let response = client::send(method, path, None).await?;
    if response.ok() {
        Ok(())
    } else {
        Err(ApiClientError::Api {
            status: response.status,
            message: response.error_message(),
        })
    }
}

/// `/api/gateway/integrations/:id` plus `suffix` (`""`, `"/refresh"`, …).
pub fn gateway_integration_path(id: &str, suffix: &str) -> String {
    format!("/api/gateway/integrations/{}{suffix}", enc(id))
}

/// Integrations, client tokens, grants, pending authorizations and event
/// feeds. Credentials are reported by name only.
pub async fn fetch_gateway() -> Result<GatewayOverview, ApiClientError> {
    get("/api/gateway").await
}

pub async fn fetch_gateway_integration(id: &str) -> Result<IntegrationDetail, ApiClientError> {
    get(&gateway_integration_path(id, "")).await
}

pub async fn create_gateway_integration(
    body: &IntegrationCreate,
) -> Result<IntegrationDetail, ApiClientError> {
    post("/api/gateway/integrations", Some(body)).await
}

/// Absent fields stay unchanged; a new URL origin or auth kind clears the
/// stored credentials.
pub async fn update_gateway_integration(
    id: &str,
    body: &IntegrationPatch,
) -> Result<IntegrationDetail, ApiClientError> {
    patch(&gateway_integration_path(id, ""), Some(body)).await
}

pub async fn delete_gateway_integration(id: &str) -> Result<(), ApiClientError> {
    send_no_content(client::Method::Delete, &gateway_integration_path(id, "")).await
}

/// Replaces the stored headers or environment variables.
pub async fn put_gateway_credentials(
    id: &str,
    body: &CredentialsUpdate,
) -> Result<IntegrationDetail, ApiClientError> {
    put(&gateway_integration_path(id, "/credentials"), Some(body)).await
}

/// Clears stored headers, variables and OAuth tokens.
pub async fn clear_gateway_credentials(id: &str) -> Result<IntegrationDetail, ApiClientError> {
    delete(&gateway_integration_path(id, "/credentials"), NO_BODY).await
}

/// Re-reads the integration's catalog; can take up to a minute.
pub async fn refresh_gateway_integration(id: &str) -> Result<IntegrationDetail, ApiClientError> {
    post(&gateway_integration_path(id, "/refresh"), NO_BODY).await
}

pub async fn start_gateway_oauth(id: &str) -> Result<OAuthStartResponse, ApiClientError> {
    post(&gateway_integration_path(id, "/oauth/start"), NO_BODY).await
}

/// `body.hidden` is the full list of hidden tool names.
pub async fn set_gateway_tool_visibility(
    id: &str,
    body: &ToolVisibilityUpdate,
) -> Result<IntegrationDetail, ApiClientError> {
    put(&gateway_integration_path(id, "/tools"), Some(body)).await
}

/// The only response that carries the new token.
pub async fn create_gateway_client(
    body: &ClientTokenCreate,
) -> Result<ClientTokenCreated, ApiClientError> {
    post("/api/gateway/clients", Some(body)).await
}

pub async fn revoke_gateway_client(id: &str) -> Result<(), ApiClientError> {
    send_no_content(
        client::Method::Delete,
        &format!("/api/gateway/clients/{}", enc(id)),
    )
    .await
}

pub async fn revoke_gateway_grant(id: &str) -> Result<(), ApiClientError> {
    send_no_content(
        client::Method::Delete,
        &format!("/api/gateway/grants/{}", enc(id)),
    )
    .await
}

/// Approves a pending downstream OAuth authorization with the code its
/// sign-in page shows.
pub async fn approve_gateway_authorization(
    id: &str,
    user_code: &str,
) -> Result<(), ApiClientError> {
    let body = AuthorizationApproval {
        user_code: user_code.to_owned(),
    };
    let response = client::send(
        client::Method::Post,
        &format!("/api/gateway/authorizations/{}/approve", enc(id)),
        Some(serde_json::to_string(&body).unwrap_or_default()),
    )
    .await?;
    if response.ok() {
        Ok(())
    } else {
        Err(ApiClientError::Api {
            status: response.status,
            message: response.error_message(),
        })
    }
}

/// Denies a pending downstream OAuth authorization.
pub async fn deny_gateway_authorization(id: &str) -> Result<(), ApiClientError> {
    send_no_content(
        client::Method::Post,
        &format!("/api/gateway/authorizations/{}/deny", enc(id)),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_paths_encode_ids() {
        assert_eq!(
            gateway_integration_path("linear", "/oauth/start"),
            "/api/gateway/integrations/linear/oauth/start"
        );
        assert_eq!(
            gateway_integration_path("a/b", ""),
            "/api/gateway/integrations/a%2Fb"
        );
    }

    #[test]
    fn query_strings_match_url_search_params() {
        assert_eq!(
            query(&[("task", Some("Live Check".into())), ("limit", None)]),
            "?task=Live+Check"
        );
        assert_eq!(query(&[("task", None)]), "");
        assert_eq!(form_encode("a/b:c"), "a%2Fb%3Ac");
        assert_eq!(
            run_log_stream_url("LiveCheckTask:1"),
            "/api/task-runs/LiveCheckTask%3A1/logs/stream"
        );
        assert_eq!(cost_range_param(CostRange::All), "all");
        assert_eq!(
            stylist_path("fixture-fit-1"),
            "/api/stylist?namespace=fixture-fit-1"
        );
    }
}
