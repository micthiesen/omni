//! Turn notifications for Claude Code sessions started with `notify`.
//!
//! A caller names a stored event target (`chief`, `business`; see
//! [`super::targets`]) when it starts or continues a session. The session
//! watcher then queues each finished turn of that session as a notice in the
//! event journal, once per session and revision, so the caller learns about
//! it without polling and without hearing about sessions it did not ask for.
//! The journal sends it with its retries; the `Idempotency-Key` stays
//! `omni-claude-turn:<session>:<revision>` so Rakazo collapses repeats.
//! Rows of `mcp-claude-turn-notification` were the old outbox; a boot step
//! moves pending ones into the journal.

use std::collections::HashSet;

use omni_core::clock::SharedClock;
use omni_runtime::ports::{NoticeStatus, TargetNotice};
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps, EntityWrite, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::service::McpEventService;

const LOG: &str = "MCP:ClaudeNotify";
const SUMMARY_CHARS: usize = 1500;
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

/// `mcp-claude-turn-notification`: the old per-turn outbox row (read only to
/// move pending rows into the journal, and by the compat audit).
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

/// The journal key (and `Idempotency-Key`) of one turn's notice.
pub fn notice_key(session_id: &str, revision: f64) -> String {
    format!("omni-claude-turn:{}", notification_id(session_id, revision))
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
    /// The turn has not ended: it is paused on a prompt in the session.
    pub blocked: bool,
    /// What a blocked turn waits for, when the host names it.
    pub waiting_for: Option<String>,
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
            "blocked": self.blocked,
            "waitingFor": self.waiting_for,
            "revision": crate::json::js_number(self.revision),
            "finishedAt": omni_core::js::to_iso_string(self.finished_at),
            "summary": summary,
            "summaryTruncated": truncated,
        })
    }
}

/// Session notify targets and their notices; cheap to clone.
#[derive(Clone)]
pub struct ClaudeNotify {
    store: Store,
    clock: SharedClock,
    events: McpEventService,
}

impl ClaudeNotify {
    pub fn new(store: Store, clock: SharedClock, events: McpEventService) -> Self {
        Self {
            store,
            clock,
            events,
        }
    }

    /// Whether `target` is a stored event target.
    pub async fn is_configured(&self, target: &str) -> bool {
        matches!(self.events.store().target(target).await, Ok(Some(_)))
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
                if !self.is_configured(target).await {
                    tracing::warn!(
                        target: LOG,
                        session = %short(session_id),
                        notify = %target,
                        "Claude session notify target is not a stored event target; its turns \
                         wait until it is created"
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

    /// Queues one finished turn for the session's target, once per session
    /// and revision. Returns whether a new notice was queued.
    pub async fn turn_finished(&self, turn: &FinishedTurn) -> Result<bool, StoreError> {
        let Some(target) = self.target_of(&turn.session_id).await? else {
            return Ok(false);
        };
        let queued = self
            .events
            .notify_target(TargetNotice {
                target: target.clone(),
                key: notice_key(&turn.session_id, turn.revision),
                name: super::catalog::CLAUDE_TURN_FINISHED.to_owned(),
                body: turn.body(),
            })
            .await?;
        if queued {
            tracing::info!(
                target: LOG,
                session = %short(&turn.session_id),
                revision = turn.revision,
                notify = %target,
                "Claude session turn finished; notifying"
            );
        }
        Ok(queued)
    }

    /// Moves pending rows of the old outbox into the journal (boot); how
    /// many. Settled old rows stay until retention removes them.
    pub async fn migrate_pending(&self) -> Result<usize, StoreError> {
        let rows = self
            .store
            .read(|docs| docs.get_all::<ClaudeTurnNotification>())
            .await?;
        let mut moved = 0;
        for row in rows {
            if row.status != NotificationStatus::Pending {
                continue;
            }
            let Some(body) = row
                .body
                .as_deref()
                .and_then(|b| serde_json::from_str(b).ok())
            else {
                continue;
            };
            self.events
                .notify_target(TargetNotice {
                    target: row.target.clone(),
                    key: notice_key(&row.session_id, row.revision),
                    name: super::catalog::CLAUDE_TURN_FINISHED.to_owned(),
                    body,
                })
                .await?;
            let settled = ClaudeTurnNotification {
                status: NotificationStatus::Delivered,
                last_error: Some("moved_to_journal".to_owned()),
                body: None,
                updated_at: self.clock.now_ms(),
                ..row
            };
            self.store
                .write(move |tx| tx.upsert(&settled, UpsertOpts::default()))
                .await?;
            moved += 1;
        }
        Ok(moved)
    }

    /// Drops old-outbox rows past a week (they are history only).
    pub async fn prune_legacy(&self) -> Result<(), StoreError> {
        let cutoff = self.clock.now_ms() - 7 * 24 * 60 * 60_000;
        self.store
            .write(move |tx| {
                for row in tx.get_all::<ClaudeTurnNotification>()? {
                    if row.updated_at < cutoff && row.status != NotificationStatus::Pending {
                        tx.delete::<ClaudeTurnNotification>(&row.id)?;
                    }
                }
                Ok(())
            })
            .await
    }

    /// `events_status`'s `claudeNotifications` section: target names (never
    /// URLs or keys), counts and the latest outcomes, from the journal.
    pub async fn status(&self) -> Result<Value, StoreError> {
        let sessions = self
            .store
            .read(|docs| docs.count::<ClaudeSessionNotify>())
            .await?;
        let mut rows: Vec<_> = self
            .events
            .store()
            .deliveries()
            .await?
            .into_iter()
            .filter(|row| row.is_notice() && row.name == super::catalog::CLAUDE_TURN_FINISHED)
            .collect();
        let states: Vec<NoticeStatus> = rows
            .iter()
            .map(|row| super::service::notice_status_of(row.status))
            .collect();
        let count = |status: NoticeStatus| states.iter().filter(|s| **s == status).count();
        let mut result = json!({
            "pending": count(NoticeStatus::Pending),
            "delivered": count(NoticeStatus::Delivered),
            "ignored": count(NoticeStatus::Ignored),
            "failed": count(NoticeStatus::Failed),
            "unconfigured": count(NoticeStatus::Unconfigured),
        });
        rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at));
        rows.truncate(STATUS_RECENT);
        let recent: Vec<Value> = rows
            .iter()
            .map(|row| {
                // `omni-claude-turn:<session>:<revision>`: the body is gone
                // once a notice settles.
                let (session, revision) = row
                    .idempotency_key
                    .as_deref()
                    .and_then(|key| key.strip_prefix("omni-claude-turn:"))
                    .and_then(|rest| rest.rsplit_once(':'))
                    .map_or((None, None), |(session, revision)| {
                        (Some(session.to_owned()), revision.parse::<f64>().ok())
                    });
                json!({
                    "sessionId": session,
                    "shortId": session.as_deref().map(short),
                    "revision": revision.map(crate::json::js_number),
                    "target": row.target,
                    "status": serde_json::to_value(super::service::notice_status_of(row.status)).unwrap_or(Value::Null),
                    "attempts": row.attempts,
                    "lastStatus": row.last_status,
                    "lastError": row.last_error,
                    "createdAt": omni_core::js::to_iso_string(row.created_at),
                    "updatedAt": omni_core::js::to_iso_string(row.updated_at),
                })
            })
            .collect();
        let targets: Vec<Value> = self
            .events
            .store()
            .targets()
            .await?
            .into_iter()
            .map(|t| Value::String(t.id))
            .collect();
        if let Some(map) = result.as_object_mut() {
            map.insert("targets".to_owned(), Value::Array(targets));
            map.insert("sessions".to_owned(), json!(sessions));
            map.insert("recent".to_owned(), Value::Array(recent));
        }
        Ok(result)
    }
}

fn short(session_id: &str) -> &str {
    session_id.get(..8).unwrap_or(session_id)
}
