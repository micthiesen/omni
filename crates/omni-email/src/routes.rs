//! Email routes: activity, activity logs, reprocess, sender rules, feedback
//! and the retry queue.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use omni_api::email::{
    BuiltinRules, BuiltinSenderLists, DeletedResponse, EmailActivitiesResponse, EmailActivity,
    EmailActivityLogsResponse, EmailActivityResponse, EmailFailureSignature, EmailFeedback,
    EmailFeedbackListResponse, EmailFeedbackResponse, EmailFeedbackVerdict, EmailPipelineName,
    EmailRetriesResponse, EmailRetry, EmailRule, EmailRuleUpsertResponse, EmailRulesResponse,
    RuleScope, RuleUpsertStatus, RuleVerdict,
};
use omni_api::runs::RunLogLine;
use omni_core::js::{string_to_number, utf16_len};
use omni_runtime::Ports;
use omni_server_kit::{ApiError, JsonBody, api_error};
use omni_store::{LogLine, Store};
use serde::Deserialize;
use serde_json::Value;

use crate::activity::{self, EmailActivityData, KEEP_PER_PIPELINE};
use crate::activity_logs;
use crate::builtin;
use crate::feedback::{self, EmailFeedbackData, NewFeedback};
use crate::reprocess::{ReprocessFailure, reprocess_activity};
use crate::retry::{self, EmailRetryData};
use crate::sender_rules::{
    self, EmailRuleData, RuleError, matches_builtin_block, normalize_rule_pattern,
};
use crate::systemic::{self, EmailSystemicAlertData};

const LOG: &str = "Main:Server";

#[derive(Clone)]
pub struct EmailRoutesState {
    pub store: Store,
    pub ports: Ports,
}

pub fn router(state: EmailRoutesState) -> Router {
    Router::new()
        .route("/api/email-activity", get(list_activity))
        .route("/api/email-activity/{activity_id}/logs", get(activity_logs))
        .route(
            "/api/email-activity/{activity_id}/reprocess",
            post(reprocess),
        )
        .route(
            "/api/email-activity/{activity_id}/feedback",
            post(set_feedback),
        )
        .route("/api/email-feedback", get(list_feedback))
        .route("/api/email-retries", get(list_retries))
        .with_state(state)
}

/// Sender rules: an allow rule overrides the blacklist and triage, so
/// changes need a trusted browser (`omni_runtime::owner`).
pub fn rules_router(state: EmailRoutesState, owner: omni_runtime::owner::OwnerTrust) -> Router {
    Router::new()
        .route("/api/email-rules", get(list_rules).post(add_rule))
        .route("/api/email-rules/{rule_id}", delete(delete_rule))
        .route_layer(axum::middleware::from_fn_with_state(
            owner,
            omni_runtime::owner::require_trusted,
        ))
        .with_state(state)
}

pub fn serialize_activity(a: &EmailActivityData) -> EmailActivity {
    EmailActivity {
        activity_id: a.activity_id.clone(),
        pipeline: a.pipeline,
        email_id: a.email_id.clone(),
        subject: a.subject.clone(),
        from: a.from.clone(),
        received_at: a.received_at,
        processed_at: a.processed_at,
        outcome: a.outcome,
        detail: a.detail.clone(),
        admit_reason: a.admit_reason.clone(),
        admit_tier: a.admit_tier,
        cost_cents: a.cost_cents.as_nullable(),
        items: a.items.clone().unwrap_or_default(),
    }
}

pub fn serialize_rule(rule: &EmailRuleData) -> EmailRule {
    EmailRule {
        rule_id: rule.rule_id.clone(),
        pattern: rule.pattern.clone(),
        scope: rule.scope,
        verdict: rule.verdict,
        created_at: rule.created_at,
    }
}

pub fn serialize_feedback(row: &EmailFeedbackData) -> EmailFeedback {
    EmailFeedback {
        activity_id: row.activity_id.clone(),
        pipeline: row.pipeline,
        email_id: row.email_id.clone(),
        subject: row.subject.clone(),
        from: row.from.clone(),
        verdict: row.verdict,
        note: row.note.clone(),
        created_at: row.created_at,
    }
}

pub fn serialize_retry(row: &EmailRetryData) -> EmailRetry {
    EmailRetry {
        retry_key: row.retry_key.clone(),
        pipeline: row.pipeline.clone(),
        email_id: row.email_id.clone(),
        reason: row.reason.clone(),
        attempts: row.attempts,
        next_attempt_at: row.next_attempt_at,
        created_at: row.created_at,
        awaiting_build: row.awaiting_build.clone(),
        signature: row.signature.clone(),
    }
}

pub fn serialize_signature(row: &EmailSystemicAlertData) -> EmailFailureSignature {
    EmailFailureSignature {
        pipeline: row.pipeline.clone(),
        signature: row.signature.clone(),
        summary: row.summary.clone(),
        build: row.build.clone(),
        failures: row.failures,
        first_seen_at: row.first_seen_at,
        last_seen_at: row.last_seen_at,
        last_alerted_at: row.last_alerted_at,
    }
}

/// A task-run log line as served by the API.
pub fn serialize_log_line(line: &LogLine) -> RunLogLine {
    RunLogLine {
        t: line.t,
        level: match line.level {
            omni_core::LogLevel::Debug => omni_api::runs::LogLevel::Debug,
            omni_core::LogLevel::Info => omni_api::runs::LogLevel::Info,
            omni_core::LogLevel::Warn => omni_api::runs::LogLevel::Warn,
            omni_core::LogLevel::Error => omni_api::runs::LogLevel::Error,
        },
        logger: line.logger.clone(),
        msg: line.msg.clone(),
    }
}

/// The read-only built-in lists.
pub fn builtin_rules() -> BuiltinRules {
    let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    BuiltinRules {
        parcel: BuiltinSenderLists {
            blocked: owned(builtin::PARCEL_BLACKLISTED_SENDERS),
            auto_pass: owned(builtin::PARCEL_CARRIER_SENDER_DOMAINS),
        },
        calendar: BuiltinSenderLists {
            blocked: owned(builtin::CALENDAR_BLACKLISTED_SENDERS),
            auto_pass: owned(builtin::CALENDAR_AUTO_PASS_SENDERS),
        },
    }
}

#[derive(Deserialize)]
struct ActivityQuery {
    pipeline: Option<String>,
    limit: Option<String>,
}

/// `Number(limit ?? 100)`, floored and clamped to `1..=2000`; non-finite is 100.
fn activity_limit(raw: Option<&str>) -> usize {
    let value = raw.map_or(100.0, string_to_number);
    if !value.is_finite() {
        return 100;
    }
    let max = (KEEP_PER_PIPELINE * 2) as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let limit = value.floor().clamp(1.0, max) as usize;
    limit
}

async fn list_activity(
    State(state): State<EmailRoutesState>,
    Query(query): Query<ActivityQuery>,
) -> Result<Response, ApiError> {
    let pipeline = match query.pipeline.as_deref() {
        None => None,
        Some(value) => match EmailPipelineName::parse(value) {
            Some(pipeline) => Some(pipeline),
            None => return Err(ApiError::bad_request("Unknown pipeline")),
        },
    };
    let limit = activity_limit(query.limit.as_deref());
    let activities = activity::recent(&state.store, pipeline, limit)
        .await
        .map_err(ApiError::internal)?
        .iter()
        .map(serialize_activity)
        .collect();
    Ok(Json(EmailActivitiesResponse { activities }).into_response())
}

async fn activity_logs(
    State(state): State<EmailRoutesState>,
    Path(activity_id): Path<String>,
) -> Result<Json<EmailActivityLogsResponse>, ApiError> {
    let activity = activity::get(&state.store, &activity_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("Unknown activity"))?;
    let logs = activity_logs::get(&state.store, &activity_id)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(EmailActivityLogsResponse {
        activity: serialize_activity(&activity),
        lines: logs
            .as_ref()
            .map(|l| l.lines.iter().map(serialize_log_line).collect())
            .unwrap_or_default(),
        dropped: logs.map_or(0, |l| l.dropped),
    }))
}

async fn reprocess(
    State(state): State<EmailRoutesState>,
    Path(activity_id): Path<String>,
) -> Result<Json<EmailActivityResponse>, ApiError> {
    match reprocess_activity(&state.store, &state.ports, &activity_id).await {
        Ok(activity) => Ok(Json(EmailActivityResponse {
            activity: serialize_activity(&activity),
        })),
        Err(ReprocessFailure::UnknownActivity(_)) => Err(ApiError::not_found("Unknown activity")),
        Err(ReprocessFailure::PipelinesInactive | ReprocessFailure::PipelineInactive(_)) => {
            Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Email pipelines are not active",
            ))
        }
        Err(ReprocessFailure::EmailGone) => {
            Err(ApiError::not_found("Email no longer exists in the mailbox"))
        }
        Err(other) => Err(ApiError::internal(other)),
    }
}

async fn list_rules(
    State(state): State<EmailRoutesState>,
) -> Result<Json<EmailRulesResponse>, ApiError> {
    let rules = sender_rules::list(&state.store)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(EmailRulesResponse {
        rules: rules.iter().map(serialize_rule).collect(),
        builtin: builtin_rules(),
    }))
}

const RULE_INPUT_ERROR: &str = "pattern, scope, and verdict are required";

/// The `emailRuleSchema` decode: pattern 1..=200 UTF-16 units plus the two literals.
fn decode_rule_input(body: &Value) -> Option<(String, RuleScope, RuleVerdict)> {
    let pattern = body.get("pattern")?.as_str()?;
    let len = utf16_len(pattern);
    if !(1..=200).contains(&len) {
        return None;
    }
    let scope: RuleScope = serde_json::from_value(body.get("scope")?.clone()).ok()?;
    let verdict: RuleVerdict = serde_json::from_value(body.get("verdict")?.clone()).ok()?;
    Some((pattern.to_owned(), scope, verdict))
}

async fn add_rule(
    State(state): State<EmailRoutesState>,
    JsonBody(body): JsonBody<Value>,
) -> Result<Response, ApiError> {
    let Some((raw_pattern, scope, verdict)) = decode_rule_input(&body) else {
        return Err(ApiError::bad_request(RULE_INPUT_ERROR));
    };
    let pattern = normalize_rule_pattern(&raw_pattern);
    if pattern.is_empty() {
        return Err(ApiError::bad_request(RULE_INPUT_ERROR));
    }
    // A block rule a built-in list already covers is redundant; allow rules
    // are the escape hatch from built-ins and are never rejected this way.
    if verdict == RuleVerdict::Block && matches_builtin_block(&pattern, scope) {
        return Ok(Json(EmailRuleUpsertResponse {
            rule: None,
            status: RuleUpsertStatus::Builtin,
            message: Some("Already blocked by a built-in list".to_owned()),
        })
        .into_response());
    }
    let result = match sender_rules::upsert_checked(&state.store, &pattern, scope, verdict).await {
        Ok(result) => result,
        Err(RuleError::EmptyPattern) => return Err(ApiError::bad_request(RULE_INPUT_ERROR)),
        Err(RuleError::Store(error)) => return Err(ApiError::internal(error)),
    };
    let status = if result.already_exists {
        RuleUpsertStatus::Exists
    } else if result.merged {
        RuleUpsertStatus::Merged
    } else {
        RuleUpsertStatus::Created
    };
    tracing::info!(
        target: LOG,
        "Email rule {}: {} {} ({})",
        status_name(status),
        result.rule.verdict.as_str(),
        result.rule.pattern,
        result.rule.scope.as_str()
    );
    let code = if status == RuleUpsertStatus::Created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        code,
        Json(EmailRuleUpsertResponse {
            rule: Some(serialize_rule(&result.rule)),
            status,
            message: None,
        }),
    )
        .into_response())
}

fn status_name(status: RuleUpsertStatus) -> &'static str {
    match status {
        RuleUpsertStatus::Created => "created",
        RuleUpsertStatus::Merged => "merged",
        RuleUpsertStatus::Exists => "exists",
        RuleUpsertStatus::Builtin => "builtin",
    }
}

async fn delete_rule(
    State(state): State<EmailRoutesState>,
    Path(rule_id): Path<String>,
) -> Result<Json<DeletedResponse>, ApiError> {
    let deleted = sender_rules::delete(&state.store, &rule_id)
        .await
        .map_err(ApiError::internal)?;
    if !deleted {
        return Err(ApiError::not_found("Unknown rule"));
    }
    Ok(Json(DeletedResponse { deleted: true }))
}

const FEEDBACK_INPUT_ERROR: &str = "A verdict (not_relevant | missed | null) is required";

/// `{verdict: "not_relevant" | "missed" | null, note?: string (<= 500)}`; the
/// `verdict` key is required.
fn decode_feedback_input(body: &Value) -> Option<(Option<EmailFeedbackVerdict>, Option<String>)> {
    let object = body.as_object()?;
    let verdict = match object.get("verdict")? {
        Value::Null => None,
        other => Some(serde_json::from_value::<EmailFeedbackVerdict>(other.clone()).ok()?),
    };
    let note = match object.get("note") {
        None => None,
        Some(Value::String(note)) if utf16_len(note) <= 500 => Some(note.clone()),
        Some(_) => return None,
    };
    Some((verdict, note))
}

async fn set_feedback(
    State(state): State<EmailRoutesState>,
    Path(activity_id): Path<String>,
    JsonBody(body): JsonBody<Value>,
) -> Result<Response, ApiError> {
    let Some(activity) = activity::get(&state.store, &activity_id)
        .await
        .map_err(ApiError::internal)?
    else {
        return Ok(api_error(StatusCode::NOT_FOUND, "Unknown activity"));
    };
    let Some((verdict, note)) = decode_feedback_input(&body) else {
        return Ok(api_error(StatusCode::BAD_REQUEST, FEEDBACK_INPUT_ERROR));
    };
    let Some(verdict) = verdict else {
        feedback::delete(&state.store, &activity.activity_id)
            .await
            .map_err(ApiError::internal)?;
        return Ok(Json(EmailFeedbackResponse { feedback: None }).into_response());
    };
    let row = feedback::record(
        &state.store,
        NewFeedback {
            pipeline: activity.pipeline,
            email_id: activity.email_id.clone(),
            subject: activity.subject.clone(),
            from: activity.from.clone(),
            verdict,
            note,
        },
    )
    .await
    .map_err(ApiError::internal)?;
    tracing::info!(
        target: LOG,
        "Email feedback: {} for \"{}\" ({})",
        match row.verdict {
            EmailFeedbackVerdict::NotRelevant => "not_relevant",
            EmailFeedbackVerdict::Missed => "missed",
        },
        activity.subject,
        activity.pipeline
    );
    Ok(Json(EmailFeedbackResponse {
        feedback: Some(serialize_feedback(&row)),
    })
    .into_response())
}

async fn list_feedback(
    State(state): State<EmailRoutesState>,
) -> Result<Json<EmailFeedbackListResponse>, ApiError> {
    let rows = feedback::list(&state.store, None, 50)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(EmailFeedbackListResponse {
        feedback: rows.iter().map(serialize_feedback).collect(),
    }))
}

async fn list_retries(
    State(state): State<EmailRoutesState>,
) -> Result<Json<EmailRetriesResponse>, ApiError> {
    let mut retries = retry::get_all(&state.store)
        .await
        .map_err(ApiError::internal)?;
    retries.sort_by_key(|r| r.next_attempt_at);
    let mut signatures = systemic::alerts(&state.store)
        .await
        .map_err(ApiError::internal)?;
    signatures.sort_by_key(|s| std::cmp::Reverse(s.last_seen_at));
    Ok(Json(EmailRetriesResponse {
        current_build: systemic::current_build().await,
        retries: retries.iter().map(serialize_retry).collect(),
        signatures: signatures.iter().map(serialize_signature).collect(),
    }))
}
