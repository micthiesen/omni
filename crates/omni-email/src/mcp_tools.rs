//! Email MCP tools: mailbox search/get through the
//! `EmailReader` port, health, pipeline activity, reprocess, sender rules,
//! feedback and the retry queue. Each tool's contract is declared in [`defs`].

pub mod defs;

use std::sync::Arc;

use omni_api::email::{EmailFeedbackVerdict, EmailPipelineName, RuleScope, RuleVerdict};
use omni_config::Config;
use omni_core::email::{EmailLinkMetadata, EmailOrigin, FetchedEmail};
use omni_mailer::SmtpConfig;
use omni_mcp_kit::{
    McpTool, Page, ToolContext, ToolError, ToolMetaError, paginate, truncate_utf16, typed_tool,
};
use omni_runtime::Ports;
use omni_runtime::ports::{EmailFolderScope, EmailReader, EmailSearch};
use omni_store::Store;
use serde::{Deserialize, Serialize};

use crate::activity::{self, EmailActivityData, KEEP_PER_PIPELINE};
use crate::activity_logs;
use crate::feedback::{self, EmailFeedbackData, NewFeedback};
use crate::reprocess::{ReprocessFailure, reprocess_activity};
use crate::retry;
use crate::routes::{builtin_rules, serialize_rule};
use crate::sender_rules::{
    self, RuleError, js_trim, matches_builtin_block, normalize_rule_pattern,
};

/// Pipeline names `email_health` probes on the `EmailRetryHandlers` port.
const KNOWN_PIPELINES: [&str; 2] = ["CalendarEvents", "ParcelTracker"];

/// Shared state of the email tools; cheap to clone.
#[derive(Clone)]
pub struct EmailTools {
    pub store: Store,
    pub ports: Ports,
    pub config: Arc<Config>,
}

fn execute_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::execute(error.to_string())
}

/// A port failure keeps whether trying again may help.
fn port_error(error: &omni_runtime::ports::PortError) -> ToolError {
    let retryable = matches!(
        error,
        omni_runtime::ports::PortError::Failed {
            transient: true,
            ..
        }
    );
    ToolError::execute(error.to_string()).retryable(retryable)
}

fn truncated(value: &str, max: usize) -> String {
    truncate_utf16(value, max).0
}

fn truncated_id(id: &str) -> String {
    truncated(id, 1_000)
}

fn active_reader(ports: &Ports) -> Result<Arc<dyn EmailReader>, ToolError> {
    ports.email_reader().ok_or_else(|| {
        ToolError::execute("Email monitoring is not active")
            .with_code("unavailable")
            .retryable(true)
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentSummary {
    attachment_id: Option<String>,
    part_id: Option<String>,
    disposition: Option<String>,
    content_id: Option<String>,
    name: String,
    mime_type: String,
    size: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EmailSummary {
    id: String,
    subject: String,
    from: String,
    to: Vec<String>,
    cc: Vec<String>,
    reply_to: Vec<String>,
    message_id: Option<String>,
    origin: Option<EmailOrigin>,
    in_reply_to: Option<String>,
    references: Vec<String>,
    received_at: String,
    excerpt: String,
    excerpt_truncated: bool,
    attachments: Vec<AttachmentSummary>,
}

fn bounded_list(values: Option<&Vec<String>>, max_items: usize, max_chars: usize) -> Vec<String> {
    values
        .map(|values| {
            values
                .iter()
                .take(max_items)
                .map(|v| truncated(v, max_chars))
                .collect()
        })
        .unwrap_or_default()
}

fn serialize_email(email: &FetchedEmail, max_excerpt_chars: usize) -> EmailSummary {
    let (excerpt, excerpt_truncated) = truncate_utf16(&email.text_body, max_excerpt_chars);
    let references = email.references.as_ref().map_or_else(Vec::new, |refs| {
        refs.iter()
            .skip(refs.len().saturating_sub(50))
            .map(|value| truncated(value, 1_000))
            .collect()
    });
    EmailSummary {
        id: email.id.clone(),
        subject: truncated(&email.subject, 500),
        from: truncated(&email.from, 500),
        to: bounded_list(email.to.as_ref(), 50, 320),
        cc: bounded_list(email.cc.as_ref(), 50, 320),
        reply_to: bounded_list(email.reply_to.as_ref(), 50, 320),
        message_id: email
            .message_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(|id| truncated(id, 1_000)),
        origin: email.origin.clone(),
        in_reply_to: email.in_reply_to.clone(),
        references,
        received_at: email.received_at.clone(),
        excerpt,
        excerpt_truncated,
        attachments: email
            .attachments
            .iter()
            .take(25)
            .map(|a| AttachmentSummary {
                attachment_id: a.attachment_id.clone(),
                part_id: a.part_id.clone(),
                disposition: a.disposition.clone(),
                content_id: a
                    .content_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .map(|id| truncated(id, 500)),
                name: truncated(&a.name, 300),
                mime_type: truncated(&a.mime_type, 200),
                size: a.size,
            })
            .collect(),
    }
}

/// An activity as served by the tools.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActivitySummary {
    activity_id: String,
    pipeline: EmailPipelineName,
    email_id: String,
    subject: String,
    from: String,
    received_at: i64,
    processed_at: i64,
    outcome: &'static str,
    detail: Option<String>,
    admit_reason: Option<String>,
    admit_tier: Option<&'static str>,
    cost_cents: Option<f64>,
    items: Vec<String>,
}

fn non_empty_truncated(value: Option<&str>, max: usize) -> Option<String> {
    value.filter(|v| !v.is_empty()).map(|v| truncated(v, max))
}

fn serialize_activity(activity: &EmailActivityData) -> ActivitySummary {
    ActivitySummary {
        activity_id: activity.activity_id.clone(),
        pipeline: activity.pipeline,
        email_id: activity.email_id.clone(),
        subject: activity.subject.clone(),
        from: activity.from.clone(),
        received_at: activity.received_at,
        processed_at: activity.processed_at,
        outcome: activity.outcome.as_str(),
        detail: non_empty_truncated(activity.detail.as_deref(), 1_000),
        admit_reason: non_empty_truncated(activity.admit_reason.as_deref(), 1_000),
        admit_tier: activity.admit_tier.map(|t| t.as_str()),
        cost_cents: activity.cost_cents.as_nullable(),
        items: activity
            .items
            .iter()
            .flatten()
            .take(50)
            .map(|item| truncated(item, 500))
            .collect(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackSummary {
    activity_id: String,
    pipeline: EmailPipelineName,
    email_id: String,
    subject: String,
    from: String,
    verdict: EmailFeedbackVerdict,
    note: Option<String>,
    created_at: i64,
}

fn serialize_feedback(row: &EmailFeedbackData) -> FeedbackSummary {
    FeedbackSummary {
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

async fn activity_or_error(
    store: &Store,
    activity_id: &str,
) -> Result<EmailActivityData, ToolError> {
    activity::get(store, activity_id)
        .await
        .map_err(execute_error)?
        .ok_or_else(|| unknown_activity(activity_id))
}

/// Callers pass a bare email or Message-ID hash; say what an id looks like.
fn unknown_activity(activity_id: &str) -> ToolError {
    ToolError::execute(format!("Unknown email activity: {activity_id}"))
        .with_code("not_found")
        .with_hint(
            "Activity ids are `ParcelTracker#<emailId>` or `CalendarEvents#<emailId>`, as \
             email_activity_list returns them; an email neither pipeline processed has none.",
        )
}

/// Epoch milliseconds of an RFC 3339 date-time with an offset.
fn parse_date_time(value: Option<&str>) -> Result<Option<i64>, ToolError> {
    value
        .map(|value| {
            value
                .parse::<jiff::Timestamp>()
                .map(|ts| ts.as_millisecond())
                .map_err(|_| ToolError::execute(format!("Invalid date-time: {value}")))
        })
        .transpose()
}

/// Trims an optional field, which must stay non-empty.
fn trimmed_field(value: Option<String>, field: &str) -> Result<Option<String>, ToolError> {
    value
        .map(|value| {
            let trimmed = js_trim(&value);
            if trimmed.is_empty() {
                Err(ToolError::input(format!(
                    "{field}: String must contain at least 1 character(s)"
                )))
            } else {
                Ok(trimmed.to_owned())
            }
        })
        .transpose()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchInput {
    query: Option<String>,
    from: Option<String>,
    to: Option<String>,
    subject: Option<String>,
    unread: Option<bool>,
    since: Option<String>,
    before: Option<String>,
    #[serde(default = "default_folder")]
    folder: EmailFolderScope,
    #[serde(default = "default_search_limit")]
    limit: u32,
    #[serde(default)]
    fresh: bool,
    #[serde(default = "default_excerpt_chars")]
    excerpt_chars: usize,
}

fn default_folder() -> EmailFolderScope {
    EmailFolderScope::All
}
fn default_search_limit() -> u32 {
    20
}
fn default_excerpt_chars() -> usize {
    500
}

#[derive(Serialize)]
struct SearchOutput {
    items: Vec<EmailSummary>,
    count: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetInput {
    email_id: String,
    #[serde(default = "default_body_chars")]
    body_chars: usize,
    #[serde(default)]
    fresh: bool,
}

fn default_body_chars() -> usize {
    8_000
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EmailWithMetadata {
    #[serde(flatten)]
    summary: EmailSummary,
    link_metadata: Option<EmailLinkMetadata>,
}

#[derive(Serialize)]
struct GetOutput {
    email: EmailWithMetadata,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ThreadInput {
    email_id: String,
    #[serde(default = "default_thread_body_chars")]
    body_chars: usize,
    #[serde(default = "default_thread_limit")]
    limit: usize,
    #[serde(default)]
    fresh: bool,
}

fn default_thread_body_chars() -> usize {
    2_000
}

fn default_thread_limit() -> usize {
    25
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadOutput {
    root_message_id: Option<String>,
    emails: Vec<EmailSummary>,
    count: usize,
    truncated: bool,
}

/// Ancestors named in a message's headers that are looked up one by one.
const THREAD_ANCESTORS: usize = 20;

/// The conversation's first Message-ID by the seed's own headers.
fn thread_root(seed: &FetchedEmail) -> Option<String> {
    seed.references
        .as_ref()
        .and_then(|refs| refs.first().cloned())
        .or_else(|| seed.in_reply_to.clone())
        .or_else(|| seed.message_id.clone())
        .filter(|id| !id.trim().is_empty())
}

/// Messages are one thread when one names the other, or both name the root.
fn in_thread(email: &FetchedEmail, root: &str, seed_id: Option<&str>) -> bool {
    let named = |id: &str| {
        email.message_id.as_deref() == Some(id)
            || email.in_reply_to.as_deref() == Some(id)
            || email
                .references
                .as_ref()
                .is_some_and(|refs| refs.iter().any(|r| r == id))
    };
    named(root) || seed_id.is_some_and(named)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthMonitoring {
    active: bool,
    transport: Option<String>,
    pipelines: Vec<String>,
    search_available: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthSmtp {
    configured: bool,
    configured_from: bool,
    provider: Option<&'static str>,
}

#[derive(Serialize)]
struct HealthDrafts {
    available: bool,
}

#[derive(Serialize)]
struct HealthCaldav {
    configured: bool,
    provider: Option<String>,
}

#[derive(Serialize)]
struct HealthOutput {
    monitoring: HealthMonitoring,
    smtp: HealthSmtp,
    drafts: HealthDrafts,
    caldav: HealthCaldav,
}

fn default_cursor() -> usize {
    0
}
fn default_page_limit() -> usize {
    25
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInput {
    #[serde(default = "default_cursor")]
    cursor: usize,
    #[serde(default = "default_page_limit")]
    limit: usize,
    pipeline: Option<EmailPipelineName>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityGetInput {
    activity_id: String,
    #[serde(default = "default_log_limit")]
    log_limit: usize,
}

fn default_log_limit() -> usize {
    100
}

#[derive(Serialize)]
struct LogEntry {
    timestamp: i64,
    level: &'static str,
    logger: String,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActivityGetOutput {
    activity: ActivitySummary,
    logs: Vec<LogEntry>,
    dropped: u64,
    logs_truncated: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityIdInput {
    activity_id: String,
}

#[derive(Serialize)]
struct ActivityOutput {
    activity: ActivitySummary,
}

#[derive(Serialize)]
struct RulesListOutput {
    rules: Vec<omni_api::email::EmailRule>,
    builtin: omni_api::email::BuiltinRules,
}

#[derive(Deserialize)]
struct RuleUpsertInput {
    pattern: String,
    scope: RuleScope,
    verdict: RuleVerdict,
}

#[derive(Serialize)]
struct RuleUpsertOutput {
    status: &'static str,
    rule: Option<omni_api::email::EmailRule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleDeleteInput {
    rule_id: String,
}

#[derive(Serialize)]
struct DeletedOutput {
    deleted: bool,
}

#[derive(Deserialize)]
struct FeedbackListInput {
    pipeline: Option<EmailPipelineName>,
    #[serde(default = "default_feedback_limit")]
    limit: usize,
}

fn default_feedback_limit() -> usize {
    50
}

#[derive(Serialize)]
struct FeedbackListOutput {
    items: Vec<FeedbackSummary>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackSetInput {
    activity_id: String,
    verdict: Option<EmailFeedbackVerdict>,
    note: Option<String>,
}

#[derive(Serialize)]
struct FeedbackSetOutput {
    feedback: Option<FeedbackSummary>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RetrySummary {
    retry_key: String,
    pipeline: String,
    email_id: String,
    reason: String,
    attempts: i64,
    next_attempt_at: i64,
    created_at: i64,
    awaiting_build: Option<String>,
    signature: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetryClearInput {
    pipeline: EmailPipelineName,
    email_id: String,
}

#[derive(Serialize)]
struct RetryClearOutput {
    cleared: bool,
}

impl EmailTools {
    async fn search(&self, input: SearchInput) -> Result<SearchOutput, ToolError> {
        let since_ms = parse_date_time(input.since.as_deref())?;
        let before_ms = parse_date_time(input.before.as_deref())?;
        if let (Some(since), Some(before)) = (since_ms, before_ms)
            && before <= since
        {
            return Err(ToolError::input("before must be later than since"));
        }
        let query = EmailSearch {
            query: trimmed_field(input.query, "query")?,
            from: trimmed_field(input.from, "from")?,
            to: trimmed_field(input.to, "to")?,
            subject: trimmed_field(input.subject, "subject")?,
            unread: input.unread,
            since_ms,
            before_ms,
            folder: Some(input.folder),
            limit: input.limit,
            fresh: input.fresh,
            references: None,
        };
        let reader = active_reader(&self.ports)?;
        if !reader.health().search_available {
            return Err(ToolError::execute(
                "The active email transport does not support mailbox search",
            ));
        }
        let emails = reader.search(&query).await.map_err(|e| port_error(&e))?;
        Ok(SearchOutput {
            count: emails.len(),
            items: emails
                .iter()
                .map(|email| serialize_email(email, input.excerpt_chars))
                .collect(),
        })
    }

    async fn get(&self, input: GetInput) -> Result<GetOutput, ToolError> {
        let reader = active_reader(&self.ports)?;
        let email = reader
            .fetch_by_id(&input.email_id, input.fresh)
            .await
            .map_err(|e| port_error(&e))?
            .ok_or_else(|| {
                ToolError::execute("Email no longer exists in the monitored mailbox")
                    .with_code("not_found")
            })?;
        Ok(GetOutput {
            email: EmailWithMetadata {
                summary: serialize_email(&email, input.body_chars),
                link_metadata: email.link_metadata.clone(),
            },
        })
    }

    async fn thread(&self, input: ThreadInput) -> Result<ThreadOutput, ToolError> {
        let reader = active_reader(&self.ports)?;
        let limit = input.limit.clamp(1, 50);
        let body_chars = input.body_chars.min(8_000);
        let seed = reader
            .fetch_by_id(&input.email_id, input.fresh)
            .await
            .map_err(|e| port_error(&e))?
            .ok_or_else(|| {
                ToolError::execute("Email no longer exists in the monitored mailbox")
                    .with_code("not_found")
            })?;
        let root = thread_root(&seed);
        let seed_id = seed.message_id.clone();
        let mut found: Vec<FetchedEmail> = vec![seed.clone()];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        seen.insert(seed.id.clone());
        // Ancestors: what the seed's headers name, looked up by Message-ID.
        let mut named: Vec<String> = seed.references.clone().unwrap_or_default();
        named.extend(seed.in_reply_to.clone());
        named.retain(|id| Some(id) != seed_id.as_ref());
        named.dedup();
        let skip = named.len().saturating_sub(THREAD_ANCESTORS);
        for id in named.into_iter().skip(skip) {
            if let Ok(Some(email)) = reader.fetch_by_id(&id, input.fresh).await
                && seen.insert(email.id.clone())
            {
                found.push(email);
            }
        }
        // Replies: messages whose References name the root or the seed.
        let mut anchors: Vec<String> = root.iter().cloned().collect();
        if let Some(id) = &seed_id
            && Some(id) != root.as_ref()
        {
            anchors.push(id.clone());
        }
        let mut truncated = false;
        for anchor in &anchors {
            for folder in [EmailFolderScope::All, EmailFolderScope::Sent] {
                let search = EmailSearch {
                    references: Some(anchor.clone()),
                    folder: Some(folder),
                    limit: u32::try_from(limit).unwrap_or(50),
                    fresh: input.fresh,
                    ..EmailSearch::default()
                };
                let Ok(emails) = reader.search(&search).await else {
                    continue;
                };
                truncated |= emails.len() >= limit;
                for email in emails {
                    if in_thread(
                        &email,
                        root.as_deref().unwrap_or_default(),
                        seed_id.as_deref(),
                    ) && seen.insert(email.id.clone())
                    {
                        found.push(email);
                    }
                }
            }
        }
        found.sort_by(|a, b| a.received_at.cmp(&b.received_at));
        if found.len() > limit {
            truncated = true;
            let seed_index = found.iter().position(|e| e.id == seed.id);
            let excess = found.len() - limit;
            found.drain(..excess);
            if seed_index.is_some_and(|i| i < excess) {
                found[0] = seed.clone();
                found.sort_by(|a, b| a.received_at.cmp(&b.received_at));
            }
        }
        let emails: Vec<EmailSummary> = found
            .iter()
            .map(|email| serialize_email(email, body_chars))
            .collect();
        Ok(ThreadOutput {
            root_message_id: root.map(|id| truncated_id(&id)),
            count: emails.len(),
            emails,
            truncated,
        })
    }

    fn health(&self) -> HealthOutput {
        let reader = self.ports.email_reader();
        let health = reader.as_ref().map(|reader| reader.health());
        let mut pipelines: Vec<String> = match self.ports.email_retry_handlers() {
            Some(handlers) => KNOWN_PIPELINES
                .iter()
                .filter(|name| handlers.handler(name).is_some())
                .map(|name| (*name).to_owned())
                .collect(),
            None => Vec::new(),
        };
        pipelines.sort();
        let compose = omni_mailer::resolve_compose_config(&self.config);
        let calendar = self
            .ports
            .calendar_connection()
            .map(|writer| writer.status());
        HealthOutput {
            monitoring: HealthMonitoring {
                active: health.is_some(),
                transport: health.as_ref().map(|h| h.transport.clone()),
                pipelines,
                search_available: health.as_ref().is_some_and(|h| h.search_available),
            },
            smtp: HealthSmtp {
                configured: compose.is_some(),
                configured_from: compose.is_some(),
                provider: compose.map(|c| match c {
                    SmtpConfig::Explicit { .. } => "smtp",
                    SmtpConfig::ICloud { .. } => "icloud",
                }),
            },
            drafts: HealthDrafts {
                available: health.as_ref().is_some_and(|h| h.drafts_available),
            },
            caldav: HealthCaldav {
                configured: calendar.as_ref().is_some_and(|s| s.configured),
                provider: calendar.and_then(|s| s.provider),
            },
        }
    }

    async fn activity_list(&self, input: PageInput) -> Result<Page<ActivitySummary>, ToolError> {
        let rows = activity::recent(&self.store, input.pipeline, KEEP_PER_PIPELINE * 2)
            .await
            .map_err(execute_error)?;
        Ok(paginate(
            rows.iter().map(serialize_activity).collect(),
            input.cursor,
            input.limit,
        ))
    }

    async fn activity_get(&self, input: ActivityGetInput) -> Result<ActivityGetOutput, ToolError> {
        let activity = activity_or_error(&self.store, &input.activity_id).await?;
        let stored = activity_logs::get(&self.store, &input.activity_id)
            .await
            .map_err(execute_error)?;
        let lines = stored
            .as_ref()
            .map(|s| s.lines.as_slice())
            .unwrap_or_default();
        let selected = if input.log_limit == 0 {
            &[][..]
        } else {
            &lines[lines.len().saturating_sub(input.log_limit)..]
        };
        Ok(ActivityGetOutput {
            activity: serialize_activity(&activity),
            logs: selected
                .iter()
                .map(|line| LogEntry {
                    timestamp: line.t,
                    level: line.level.as_str(),
                    logger: line.logger.clone(),
                    message: truncated(&line.msg, 4_000),
                })
                .collect(),
            dropped: stored.as_ref().map_or(0, |s| s.dropped),
            logs_truncated: selected.len() < lines.len(),
        })
    }

    async fn reprocess(&self, input: ActivityIdInput) -> Result<ActivityOutput, ToolError> {
        match reprocess_activity(&self.store, &self.ports, &input.activity_id).await {
            Ok(activity) => Ok(ActivityOutput {
                activity: serialize_activity(&activity),
            }),
            Err(ReprocessFailure::UnknownActivity(id)) => Err(unknown_activity(&id)),
            Err(ReprocessFailure::PipelinesInactive) => {
                Err(ToolError::execute("Email monitoring is not active"))
            }
            Err(ReprocessFailure::EmailGone) => Err(ToolError::execute(
                "Email no longer exists in the monitored mailbox",
            )),
            Err(other) => Err(execute_error(other)),
        }
    }

    async fn rules_list(&self) -> Result<RulesListOutput, ToolError> {
        let rules = sender_rules::list(&self.store)
            .await
            .map_err(execute_error)?;
        Ok(RulesListOutput {
            rules: rules.iter().map(serialize_rule).collect(),
            builtin: builtin_rules(),
        })
    }

    async fn rules_upsert(&self, input: RuleUpsertInput) -> Result<RuleUpsertOutput, ToolError> {
        if js_trim(&input.pattern).is_empty() {
            return Err(ToolError::input(
                "pattern: String must contain at least 1 character(s)",
            ));
        }
        let pattern = normalize_rule_pattern(&input.pattern);
        if input.verdict == RuleVerdict::Block && matches_builtin_block(&pattern, input.scope) {
            return Ok(RuleUpsertOutput {
                status: "builtin",
                rule: None,
            });
        }
        let result =
            sender_rules::upsert_checked(&self.store, &pattern, input.scope, input.verdict)
                .await
                .map_err(|e| match e {
                    RuleError::EmptyPattern => ToolError::execute(e.to_string()),
                    RuleError::Store(error) => execute_error(error),
                })?;
        let status = if result.already_exists {
            "exists"
        } else if result.merged {
            "merged"
        } else {
            "created"
        };
        Ok(RuleUpsertOutput {
            status,
            rule: Some(serialize_rule(&result.rule)),
        })
    }

    async fn rules_delete(&self, input: RuleDeleteInput) -> Result<DeletedOutput, ToolError> {
        let deleted = sender_rules::delete(&self.store, &input.rule_id)
            .await
            .map_err(execute_error)?;
        Ok(DeletedOutput { deleted })
    }

    async fn feedback_list(
        &self,
        input: FeedbackListInput,
    ) -> Result<FeedbackListOutput, ToolError> {
        let rows = feedback::list(&self.store, input.pipeline, input.limit)
            .await
            .map_err(execute_error)?;
        Ok(FeedbackListOutput {
            items: rows.iter().map(serialize_feedback).collect(),
        })
    }

    async fn feedback_set(&self, input: FeedbackSetInput) -> Result<FeedbackSetOutput, ToolError> {
        let note = input.note.map(|note| js_trim(&note).to_owned());
        let activity = activity_or_error(&self.store, &input.activity_id).await?;
        let Some(verdict) = input.verdict else {
            feedback::delete(&self.store, &activity.activity_id)
                .await
                .map_err(execute_error)?;
            return Ok(FeedbackSetOutput { feedback: None });
        };
        let row = feedback::record(
            &self.store,
            NewFeedback {
                pipeline: activity.pipeline,
                email_id: activity.email_id,
                subject: activity.subject,
                from: activity.from,
                verdict,
                note: note.filter(|note| !note.is_empty()),
            },
        )
        .await
        .map_err(execute_error)?;
        Ok(FeedbackSetOutput {
            feedback: Some(serialize_feedback(&row)),
        })
    }

    async fn retry_list(&self, input: PageInput) -> Result<Page<RetrySummary>, ToolError> {
        let mut rows = retry::get_all(&self.store).await.map_err(execute_error)?;
        rows.retain(|r| input.pipeline.is_none_or(|p| r.pipeline == p.as_str()));
        rows.sort_by_key(|r| r.next_attempt_at);
        let items = rows
            .into_iter()
            .map(|r| RetrySummary {
                reason: truncated(&r.reason, 1_000),
                retry_key: r.retry_key,
                pipeline: r.pipeline,
                email_id: r.email_id,
                attempts: r.attempts,
                next_attempt_at: r.next_attempt_at,
                created_at: r.created_at,
                awaiting_build: r.awaiting_build,
                signature: r.signature,
            })
            .collect();
        Ok(paginate(items, input.cursor, input.limit))
    }

    async fn retry_clear(&self, input: RetryClearInput) -> Result<RetryClearOutput, ToolError> {
        let pipeline = input.pipeline.as_str();
        let existed = retry::get(&self.store, &retry::retry_key(pipeline, &input.email_id))
            .await
            .map_err(execute_error)?
            .is_some();
        retry::clear(&self.store, pipeline, &input.email_id)
            .await
            .map_err(execute_error)?;
        Ok(RetryClearOutput { cleared: existed })
    }
}

macro_rules! tool {
    ($tools:expr, $def:expr, |$this:ident, $input:ident: $ty:ty| $body:expr) => {{
        let state = $tools.clone();
        typed_tool($def, move |$input: $ty, _cx: ToolContext| {
            let $this = state.clone();
            async move { $body }
        })?
    }};
}

/// The email tools, in serving order.
pub fn tools(state: &EmailTools) -> Result<Vec<McpTool>, ToolMetaError> {
    Ok(vec![
        tool!(state, &defs::EMAIL_SEARCH, |this, input: SearchInput| this
            .search(input)
            .await),
        tool!(state, &defs::EMAIL_GET, |this, input: GetInput| this
            .get(input)
            .await),
        tool!(
            state,
            &defs::EMAIL_THREAD_GET,
            |this, input: ThreadInput| this.thread(input).await
        ),
        tool!(state, &defs::EMAIL_HEALTH, |this, _input: EmptyInput| Ok::<
            _,
            ToolError,
        >(
            this.health()
        )),
        tool!(
            state,
            &defs::EMAIL_ACTIVITY_LIST,
            |this, input: PageInput| this.activity_list(input).await
        ),
        tool!(
            state,
            &defs::EMAIL_ACTIVITY_GET,
            |this, input: ActivityGetInput| this.activity_get(input).await
        ),
        tool!(
            state,
            &defs::EMAIL_REPROCESS,
            |this, input: ActivityIdInput| this.reprocess(input).await
        ),
        tool!(
            state,
            &defs::EMAIL_RULES_LIST,
            |this, _input: EmptyInput| this.rules_list().await
        ),
        tool!(
            state,
            &defs::EMAIL_RULES_UPSERT,
            |this, input: RuleUpsertInput| this.rules_upsert(input).await
        ),
        tool!(
            state,
            &defs::EMAIL_RULES_DELETE,
            |this, input: RuleDeleteInput| this.rules_delete(input).await
        ),
        tool!(
            state,
            &defs::EMAIL_FEEDBACK_LIST,
            |this, input: FeedbackListInput| this.feedback_list(input).await
        ),
        tool!(
            state,
            &defs::EMAIL_FEEDBACK_SET,
            |this, input: FeedbackSetInput| this.feedback_set(input).await
        ),
        tool!(state, &defs::EMAIL_RETRY_LIST, |this, input: PageInput| {
            this.retry_list(input).await
        }),
        tool!(
            state,
            &defs::EMAIL_RETRY_CLEAR,
            |this, input: RetryClearInput| this.retry_clear(input).await
        ),
    ])
}
