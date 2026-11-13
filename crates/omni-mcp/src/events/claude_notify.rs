//! Turn notifications for Claude Code sessions started with `notify`.
//!
//! A caller names a server-side target (`CLAUDE_NOTIFY_TARGETS`) when it
//! starts or continues a session. The session watcher then posts each finished
//! turn of that session to the target, once per session and revision, so the
//! caller learns about it without polling and without hearing about sessions
//! it did not ask for. Targets are Rakazo bot webhooks: a JSON body,
//! `Authorization: Bearer <key>` and an `Idempotency-Key` that Rakazo uses to
//! collapse repeated deliveries, which is why an uncertain attempt (timeout,
//! dropped connection) may be retried.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use omni_config::{ClaudeNotifyTarget, ClaudeNotifyTargets};
use omni_core::clock::SharedClock;
use omni_http::{HttpClient, HttpError, Method, RedirectRule, SideEffectMode};
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps, EntityWrite, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const LOG: &str = "MCP:ClaudeNotify";
/// The webhook body is a short, fixed-shape record; Rakazo accepts 64 KiB.
const MAX_RESPONSE_BYTES: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const SUMMARY_CHARS: usize = 1500;
const MAX_ATTEMPTS: i64 = 8;
const FIRST_BACKOFF_MS: i64 = 30_000;
const MAX_BACKOFF_MS: i64 = 30 * 60_000;
const MAX_DUE_PER_PASS: usize = 10;
const RETENTION_MS: i64 = 7 * 24 * 60 * 60_000;
const STATUS_RECENT: usize = 10;

/// `mcp-claude-session-notify`: the target one session's finished turns go to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSessionNotify {
    pub session_id: String,
    pub target: String,
    pub set_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for ClaudeSessionNotify {
    const NAME: &'static str = "mcp-claude-session-notify";
    type Key = String;
    fn key(&self) -> String {
        self.session_id.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationStatus {
    Pending,
    Delivered,
    /// Accepted, but the target bot had no active webhook routine to run.
    Ignored,
    Failed,
    /// The session names a target `CLAUDE_NOTIFY_TARGETS` does not define.
    Unconfigured,
}

impl NotificationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationStatus::Pending => "pending",
            NotificationStatus::Delivered => "delivered",
            NotificationStatus::Ignored => "ignored",
            NotificationStatus::Failed => "failed",
            NotificationStatus::Unconfigured => "unconfigured",
        }
    }
}

/// `mcp-claude-turn-notification`: one finished turn owed to one target,
/// keyed by session and revision so a turn is notified at most once.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeTurnNotification {
    pub id: String,
    pub session_id: String,
    pub short_id: Option<String>,
    pub revision: f64,
    pub target: String,
    pub status: NotificationStatus,
    pub attempts: i64,
    pub next_attempt_at: i64,
    pub last_status: Option<i64>,
    pub last_error: Option<String>,
    /// The JSON body; dropped once the notification settles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for ClaudeTurnNotification {
    const NAME: &'static str = "mcp-claude-turn-notification";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

pub fn notification_id(session_id: &str, revision: f64) -> String {
    format!("{session_id}:{}", omni_core::js::number_to_string(revision))
}

/// What one finished turn reports. Holds no secrets; text is already
/// scrubbed of host details by the caller.
#[derive(Clone, Debug, PartialEq)]
pub struct FinishedTurn {
    pub session_id: String,
    pub short_id: Option<String>,
    pub project: Option<String>,
    pub title: Option<String>,
    pub status: String,
    pub state: Option<String>,
    pub revision: f64,
    pub finished_at: i64,
    pub summary: Option<String>,
}

impl FinishedTurn {
    pub fn body(&self) -> Value {
        let (summary, truncated) = match &self.summary {
            Some(text) => {
                let (text, cut) = crate::tools::truncate(text, SUMMARY_CHARS);
                (Some(text), cut)
            }
            None => (None, false),
        };
        json!({
            "event": super::catalog::CLAUDE_TURN_FINISHED,
            "sessionId": self.session_id,
            "shortId": self.short_id,
            "project": self.project,
            "title": self.title,
            "status": self.status,
            "state": self.state,
            "revision": crate::json::js_number(self.revision),
            "finishedAt": omni_core::js::to_iso_string(self.finished_at),
            "summary": summary,
            "summaryTruncated": truncated,
        })
    }
}

/// One answer from a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyResponse {
    pub status: u16,
    /// Rakazo's `{"ok": true, "ignored": true}`: no webhook routine ran.
    pub ignored: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotifyTransportError {
    #[error("timeout")]
    Timeout,
    #[error("network")]
    Network,
}

/// The network leg; tests replace it.
pub trait NotifyTransport: Send + Sync {
    fn post<'a>(
        &'a self,
        target: &'a ClaudeNotifyTarget,
        idempotency_key: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<NotifyResponse, NotifyTransportError>>;
}

/// The production transport: the LAN-capable client (targets come from
/// configuration, not callers), no redirects, ten-second timeout. In
/// `SideEffectMode::Record` nothing is sent.
pub struct LiveNotifyTransport {
    client: HttpClient,
    mode: SideEffectMode,
}

impl LiveNotifyTransport {
    pub fn new(client: HttpClient, mode: SideEffectMode) -> Self {
        Self { client, mode }
    }
}

impl NotifyTransport for LiveNotifyTransport {
    fn post<'a>(
        &'a self,
        target: &'a ClaudeNotifyTarget,
        idempotency_key: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<NotifyResponse, NotifyTransportError>> {
        Box::pin(async move {
            if self.mode == SideEffectMode::Record {
                tracing::info!(
                    target: LOG,
                    host = target.url.host_str().unwrap_or(""),
                    "Recorded Claude session notification (not sent)"
                );
                return Ok(NotifyResponse {
                    status: 200,
                    ignored: false,
                });
            }
            let response = self
                .client
                .request(Method::POST, target.url.clone())
                .redirect(RedirectRule::None)
                .timeout(REQUEST_TIMEOUT)
                .bearer_auth(&target.token)
                .header("content-type", "application/json")
                .header("idempotency-key", idempotency_key)
                .body(body.to_owned())
                .send_bounded(MAX_RESPONSE_BYTES)
                .await
                .map_err(|error| match error {
                    HttpError::Timeout => NotifyTransportError::Timeout,
                    _ => NotifyTransportError::Network,
                })?;
            let ignored = serde_json::from_slice::<Value>(&response.body)
                .ok()
                .and_then(|value| value.get("ignored").and_then(Value::as_bool))
                .unwrap_or(false);
            Ok(NotifyResponse {
                status: response.status.as_u16(),
                ignored,
            })
        })
    }
}

/// How one attempt ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    Delivered,
    Ignored,
    /// Worth another attempt: a timeout, network failure, 408, 425, 429 or 5xx.
    Retry(String),
    /// The target refused it for good (another 4xx, or a 3xx it never follows).
    Rejected(String),
}

/// The pure decision over one target answer.
pub fn classify(result: &Result<NotifyResponse, NotifyTransportError>) -> AttemptOutcome {
    match result {
        Err(error) => AttemptOutcome::Retry(error.to_string()),
        Ok(response) if (200..300).contains(&response.status) => {
            if response.ignored {
                AttemptOutcome::Ignored
            } else {
                AttemptOutcome::Delivered
            }
        }
        Ok(response) if matches!(response.status, 408 | 425 | 429) || response.status >= 500 => {
            AttemptOutcome::Retry(format!("http_{}", response.status))
        }
        Ok(response) => AttemptOutcome::Rejected(format!("http_{}", response.status)),
    }
}

/// Delay before attempt `attempts + 1`: 30 s doubling to 30 minutes.
pub fn backoff_ms(attempts: i64) -> i64 {
    let exponent = u32::try_from(attempts.saturating_sub(1).clamp(0, 16)).unwrap_or(16);
    FIRST_BACKOFF_MS
        .saturating_mul(1_i64 << exponent)
        .min(MAX_BACKOFF_MS)
}

/// Session notify targets and their deliveries; cheap to clone.
#[derive(Clone)]
pub struct ClaudeNotify {
    store: Store,
    clock: SharedClock,
    targets: Arc<BTreeMap<String, ClaudeNotifyTarget>>,
    transport: Arc<dyn NotifyTransport>,
}

impl ClaudeNotify {
    pub fn new(
        store: Store,
        clock: SharedClock,
        targets: Option<ClaudeNotifyTargets>,
        transport: Arc<dyn NotifyTransport>,
    ) -> Self {
        Self {
            store,
            clock,
            targets: Arc::new(targets.map(|t| t.0).unwrap_or_default()),
            transport,
        }
    }

    pub fn is_configured(&self, target: &str) -> bool {
        self.targets.contains_key(target)
    }

    /// Sets (`Some`) or clears (`None`) the target for one session's later
    /// turns. An unconfigured target is stored, with a warning, so the
    /// session itself never fails over it; its turns are recorded as
    /// `unconfigured`.
    pub async fn set_target(
        &self,
        session_id: &str,
        target: Option<&str>,
    ) -> Result<(), StoreError> {
        let key = session_id.to_owned();
        match target {
            None => {
                self.store
                    .write(move |tx| tx.delete::<ClaudeSessionNotify>(&key).map(|_| ()))
                    .await
            }
            Some(target) => {
                if !self.is_configured(target) {
                    tracing::warn!(
                        target: LOG,
                        session = %short(session_id),
                        notify = %target,
                        "Claude session notify target is not configured in CLAUDE_NOTIFY_TARGETS; \
                         its turns will not be delivered"
                    );
                }
                let row = ClaudeSessionNotify {
                    session_id: key,
                    target: target.to_owned(),
                    set_at: self.clock.now_ms(),
                    extra: Extra::default(),
                };
                self.store
                    .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
                    .await
            }
        }
    }

    pub async fn target_of(&self, session_id: &str) -> Result<Option<String>, StoreError> {
        let key = session_id.to_owned();
        Ok(self
            .store
            .read(move |docs| docs.get::<ClaudeSessionNotify>(&key))
            .await?
            .map(|row| row.target))
    }

    /// Whether any session asked for notifications, so the watcher must poll.
    pub async fn any_sessions(&self) -> Result<bool, StoreError> {
        Ok(self
            .store
            .read(|docs| docs.count::<ClaudeSessionNotify>())
            .await?
            > 0)
    }

    pub async fn forget_session(&self, session_id: &str) -> Result<(), StoreError> {
        self.set_target(session_id, None).await
    }

    /// Drops targets of sessions the watcher neither lists nor tracks that
    /// were set before `before`, so a session that vanished does not keep the
    /// watcher polling the host.
    pub async fn prune_targets(
        &self,
        known: &HashSet<String>,
        before: i64,
    ) -> Result<(), StoreError> {
        let rows = self
            .store
            .read(|docs| docs.get_all::<ClaudeSessionNotify>())
            .await?;
        let stale: Vec<String> = rows
            .into_iter()
            .filter(|row| row.set_at < before && !known.contains(&row.session_id))
            .map(|row| row.session_id)
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        self.store
            .write(move |tx| {
                for id in &stale {
                    tx.delete::<ClaudeSessionNotify>(id)?;
                }
                Ok(())
            })
            .await
    }

    /// Records one finished turn for the session's target, once per session
    /// and revision, then tries to deliver it. Returns whether a new
    /// notification was recorded.
    pub async fn turn_finished(&self, turn: &FinishedTurn) -> Result<bool, StoreError> {
        let Some(target) = self.target_of(&turn.session_id).await? else {
            return Ok(false);
        };
        let now = self.clock.now_ms();
        let configured = self.is_configured(&target);
        let id = notification_id(&turn.session_id, turn.revision);
        let row = ClaudeTurnNotification {
            id: id.clone(),
            session_id: turn.session_id.clone(),
            short_id: turn.short_id.clone(),
            revision: turn.revision,
            target: target.clone(),
            status: if configured {
                NotificationStatus::Pending
            } else {
                NotificationStatus::Unconfigured
            },
            attempts: 0,
            next_attempt_at: now,
            last_status: None,
            last_error: None,
            body: configured.then(|| omni_core::js::json_stringify(&turn.body())),
            created_at: now,
            updated_at: now,
            extra: Extra::default(),
        };
        let inserted = self
            .store
            .write(move |tx| {
                if tx.has::<ClaudeTurnNotification>(&row.id)? {
                    return Ok(false);
                }
                tx.upsert(&row, UpsertOpts::default())?;
                Ok(true)
            })
            .await?;
        if !inserted {
            return Ok(false);
        }
        if configured {
            tracing::info!(
                target: LOG,
                session = %short(&turn.session_id),
                revision = turn.revision,
                notify = %target,
                "Claude session turn finished; notifying"
            );
            self.deliver_due().await?;
        } else {
            tracing::warn!(
                target: LOG,
                session = %short(&turn.session_id),
                revision = turn.revision,
                notify = %target,
                "Claude session turn finished but its notify target is not configured in \
                 CLAUDE_NOTIFY_TARGETS; not delivered"
            );
        }
        Ok(true)
    }

    /// Attempts every due pending notification (a few per pass) and prunes
    /// settled ones past retention.
    pub async fn deliver_due(&self) -> Result<usize, StoreError> {
        let now = self.clock.now_ms();
        let rows = self
            .store
            .read(|docs| docs.get_all::<ClaudeTurnNotification>())
            .await?;
        let stale: Vec<String> = rows
            .iter()
            .filter(|row| row.status != NotificationStatus::Pending)
            .filter(|row| row.updated_at < now - RETENTION_MS)
            .map(|row| row.id.clone())
            .collect();
        if !stale.is_empty() {
            self.store
                .write(move |tx| {
                    for id in &stale {
                        tx.delete::<ClaudeTurnNotification>(id)?;
                    }
                    Ok(())
                })
                .await?;
        }
        let mut due: Vec<ClaudeTurnNotification> = rows
            .into_iter()
            .filter(|row| row.status == NotificationStatus::Pending && row.next_attempt_at <= now)
            .collect();
        due.sort_by_key(|row| row.next_attempt_at);
        due.truncate(MAX_DUE_PER_PASS);
        let attempted = due.len();
        for row in due {
            self.attempt(row).await?;
        }
        Ok(attempted)
    }

    async fn attempt(&self, mut row: ClaudeTurnNotification) -> Result<(), StoreError> {
        let now = self.clock.now_ms();
        let Some(target) = self.targets.get(&row.target).cloned() else {
            row.status = NotificationStatus::Unconfigured;
            row.body = None;
            row.updated_at = now;
            tracing::warn!(
                target: LOG,
                session = %short(&row.session_id),
                notify = %row.target,
                "Claude session notify target is no longer configured; not delivered"
            );
            return self.save(row).await;
        };
        let Some(body) = row.body.clone() else {
            row.status = NotificationStatus::Failed;
            row.last_error = Some("missing_body".to_owned());
            row.updated_at = now;
            return self.save(row).await;
        };
        // Reserve the attempt first: a crash mid-request waits out the backoff
        // instead of resending at once (the target's idempotency key covers a
        // resend either way).
        row.attempts += 1;
        row.next_attempt_at = now + backoff_ms(row.attempts);
        row.updated_at = now;
        self.save(row.clone()).await?;
        let key = format!("omni-claude-turn:{}", row.id);
        let result = self.transport.post(&target, &key, &body).await;
        let now = self.clock.now_ms();
        row.updated_at = now;
        row.last_status = result.as_ref().ok().map(|r| i64::from(r.status));
        let host = target.url.host_str().unwrap_or("").to_owned();
        match classify(&result) {
            AttemptOutcome::Delivered => {
                row.status = NotificationStatus::Delivered;
                row.last_error = None;
                row.body = None;
                tracing::info!(
                    target: LOG,
                    session = %short(&row.session_id),
                    revision = row.revision,
                    notify = %row.target,
                    attempts = row.attempts,
                    "Claude session notification delivered"
                );
            }
            AttemptOutcome::Ignored => {
                row.status = NotificationStatus::Ignored;
                row.last_error = None;
                row.body = None;
                tracing::warn!(
                    target: LOG,
                    session = %short(&row.session_id),
                    notify = %row.target,
                    host = %host,
                    "Claude session notification accepted, but the target bot has no active \
                     webhook routine"
                );
            }
            AttemptOutcome::Retry(error) if row.attempts < MAX_ATTEMPTS => {
                tracing::info!(
                    target: LOG,
                    session = %short(&row.session_id),
                    notify = %row.target,
                    host = %host,
                    attempts = row.attempts,
                    error = %error,
                    "Claude session notification failed; will retry"
                );
                row.last_error = Some(error);
            }
            AttemptOutcome::Retry(error) | AttemptOutcome::Rejected(error) => {
                tracing::warn!(
                    target: LOG,
                    session = %short(&row.session_id),
                    notify = %row.target,
                    host = %host,
                    attempts = row.attempts,
                    error = %error,
                    "Claude session notification failed; giving up"
                );
                row.status = NotificationStatus::Failed;
                row.last_error = Some(error);
                row.body = None;
            }
        }
        self.save(row).await
    }

    async fn save(&self, row: ClaudeTurnNotification) -> Result<(), StoreError> {
        self.store
            .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
            .await
    }

    /// `events_status`'s `claudeNotifications` section: target names (never
    /// URLs or keys), counts and the latest outcomes.
    pub async fn status(&self) -> Result<Value, StoreError> {
        let (sessions, mut rows) = self
            .store
            .read(|docs| {
                Ok((
                    docs.count::<ClaudeSessionNotify>()?,
                    docs.get_all::<ClaudeTurnNotification>()?,
                ))
            })
            .await?;
        let count = |status: NotificationStatus| rows.iter().filter(|r| r.status == status).count();
        let counts = json!({
            "pending": count(NotificationStatus::Pending),
            "delivered": count(NotificationStatus::Delivered),
            "ignored": count(NotificationStatus::Ignored),
            "failed": count(NotificationStatus::Failed),
            "unconfigured": count(NotificationStatus::Unconfigured),
        });
        rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at));
        rows.truncate(STATUS_RECENT);
        let recent: Vec<Value> = rows
            .iter()
            .map(|row| {
                json!({
                    "sessionId": row.session_id,
                    "shortId": row.short_id,
                    "revision": crate::json::js_number(row.revision),
                    "target": row.target,
                    "status": row.status.as_str(),
                    "attempts": row.attempts,
                    "lastStatus": row.last_status,
                    "lastError": row.last_error,
                    "createdAt": omni_core::js::to_iso_string(row.created_at),
                    "updatedAt": omni_core::js::to_iso_string(row.updated_at),
                })
            })
            .collect();
        let mut result = counts;
        if let Some(map) = result.as_object_mut() {
            map.insert(
                "targets".to_owned(),
                Value::Array(self.targets.keys().cloned().map(Value::String).collect()),
            );
            map.insert("sessions".to_owned(), json!(sessions));
            map.insert("recent".to_owned(), Value::Array(recent));
        }
        Ok(result)
    }
}

fn short(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(status: u16, ignored: bool) -> Result<NotifyResponse, NotifyTransportError> {
        Ok(NotifyResponse { status, ignored })
    }

    #[test]
    fn classifies_target_answers() {
        assert_eq!(classify(&answer(200, false)), AttemptOutcome::Delivered);
        assert_eq!(classify(&answer(200, true)), AttemptOutcome::Ignored);
        for status in [408, 425, 429, 500, 502, 503] {
            assert!(matches!(
                classify(&answer(status, false)),
                AttemptOutcome::Retry(_)
            ));
        }
        for status in [301, 400, 401, 404, 409, 413] {
            assert!(matches!(
                classify(&answer(status, false)),
                AttemptOutcome::Rejected(_)
            ));
        }
        assert!(matches!(
            classify(&Err(NotifyTransportError::Timeout)),
            AttemptOutcome::Retry(_)
        ));
    }

    #[test]
    fn backoff_doubles_to_thirty_minutes() {
        assert_eq!(backoff_ms(1), 30_000);
        assert_eq!(backoff_ms(2), 60_000);
        assert_eq!(backoff_ms(4), 240_000);
        assert_eq!(backoff_ms(7), MAX_BACKOFF_MS);
        assert_eq!(backoff_ms(40), MAX_BACKOFF_MS);
    }
}
