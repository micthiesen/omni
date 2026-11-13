//! `claude.session.turn_finished` publisher and session `notify` trigger.
//!
//! Diffs the host's session list against the last state Omni recorded. It
//! polls only while a subscription for the event is active or a session asked
//! to be notified, and the host is online, so the host's tools stay the only
//! way to observe sessions otherwise.
//!
//! A turn has finished when the host's runtime `status` is no longer `busy`
//! and the transcript's last item is not a prompt still waiting to start. The
//! session's `state` is not used: Claude Code writes it from a model summary
//! a few seconds after each turn, so it lags the turn and can stay `working`
//! after the turn ended.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::clock::SharedClock;
use omni_runtime::ports::ClaudeSessionNotifier;
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps, EntityWrite, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::catalog::CLAUDE_TURN_FINISHED;
use super::claude_notify::{ClaudeNotify, FinishedTurn};
use super::service::{McpEventService, PublishInput};
use crate::host::{ClaudeHost, HostCommand, HostError};
use crate::json::js_number;
use crate::tools::claude_sessions::scrub_host_details;

const LIST_LIMIT: u32 = 100;
/// Sessions that left the running list are looked up individually, a few per pass.
const MAX_STATUS_LOOKUPS: usize = 5;
/// Transcript tails read per pass to confirm a turn ended; the rest wait a pass.
const MAX_TAIL_LOOKUPS: usize = 5;
/// A session that cannot be read this many polls in a row is forgotten.
const MAX_MISSES: i64 = 10;
const WATCH_RETENTION_MS: i64 = 7 * 24 * 60 * 60_000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// A turn last seen this long ago is history, not news, when polling resumes.
const STALE_TURN_MS: i64 = 24 * 60 * 60_000;

/// `mcp-claude-session-watch`: the last turn state Omni saw for one session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSessionWatch {
    pub session_id: String,
    pub id: Option<String>,
    pub project: Option<String>,
    pub revision: f64,
    pub settled: bool,
    /// Omni began a turn at `revision`; only a later revision can finish it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<bool>,
    /// Consecutive failed lookups after the session left the running list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub misses: Option<i64>,
    /// The host reported the session not running a turn (idle or stopped)
    /// at `revision`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet: Option<bool>,
    pub seen_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for ClaudeSessionWatch {
    const NAME: &'static str = "mcp-claude-session-watch";
    type Key = String;
    fn key(&self) -> String {
        self.session_id.clone()
    }
}

/// One session summary as the host's session client reports it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    pub status: String,
    #[serde(default)]
    pub state: Option<String>,
    pub revision: f64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub last_assistant: Option<String>,
}

impl SessionSummary {
    fn decode(raw: &Value) -> Option<Self> {
        serde_json::from_value(raw.clone()).ok()
    }
}

/// The previous observation `finished_turn` compares against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TurnBaseline {
    pub revision: f64,
    pub settled: bool,
    pub started: bool,
    pub quiet: bool,
}

impl TurnBaseline {
    fn of(watch: &ClaudeSessionWatch) -> Self {
        Self {
            revision: watch.revision,
            settled: watch.settled,
            started: watch.started.unwrap_or(false),
            quiet: watch.quiet.unwrap_or(false),
        }
    }
}

/// The host is running a turn: `busy`, or `running` while a launch settles.
fn is_active(status: &str) -> bool {
    matches!(status, "busy" | "running")
}

/// What can be decided from the session list alone. `None` means the
/// transcript's last item decides ([`settled_by_tail`]).
pub fn settled_from_status(
    previous: Option<TurnBaseline>,
    status: &str,
    revision: f64,
) -> Option<bool> {
    if is_active(status) {
        return Some(false);
    }
    let Some(previous) = previous else {
        // First sight only sets a baseline; its earlier turns are not news.
        return Some(true);
    };
    if previous.started && revision <= previous.revision {
        // Omni's turn has not reached the transcript yet, unless the session
        // stopped before it ever did (twice in a row, so a launch's brief
        // stopped report does not count): settled, with no turn to report.
        return Some(status == "stopped" && previous.quiet);
    }
    if previous.settled && !previous.started && revision == previous.revision {
        return Some(true);
    }
    None
}

/// A quiet session settled once its transcript ends in anything but a
/// prompt: a prompt last means the turn has not started, which the host
/// reports as idle (or briefly stopped) for a moment after a start or send.
/// A prompt still last one poll later, quiet at the same revision, is a turn
/// that will not run (interrupted, or stopped before it began): settled.
pub fn settled_by_tail(
    previous: Option<TurnBaseline>,
    revision: f64,
    last_kind: Option<&str>,
) -> bool {
    match last_kind {
        Some("user") => previous.is_some_and(|p| p.quiet && p.revision == revision),
        Some(_) => true,
        None => false,
    }
}

/// A finished turn is a settled session whose transcript moved past the turn
/// Omni last saw in progress, or that settled since Omni last saw it working.
/// A session seen for the first time only sets the baseline. A turn Omni just
/// started can report idle before it begins, so it needs a later revision.
pub fn finished_turn(previous: Option<TurnBaseline>, settled: bool, revision: f64) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    if !settled {
        return false;
    }
    if previous.started {
        return revision > previous.revision;
    }
    !previous.settled || revision > previous.revision
}

/// The transcript's last item, when the watcher had to read it.
#[derive(Clone, Debug, Default, PartialEq)]
struct Tail {
    kind: Option<String>,
    timestamp: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WatcherError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Host(#[from] HostError),
    #[error("the Claude Code host returned a malformed session list")]
    MalformedList,
}

/// A turn Omni just started (`noteTurnStarted` input).
pub use omni_runtime::ports::ClaudeTurnStarted as StartedTurn;

/// Publishes `claude.session.turn_finished` and notifies session targets;
/// cheap to clone.
#[derive(Clone)]
pub struct ClaudeSessionWatcher {
    events: McpEventService,
    host: Arc<dyn ClaudeHost>,
    store: Store,
    clock: SharedClock,
    notify: Option<ClaudeNotify>,
}

impl ClaudeSessionWatcher {
    pub fn new(
        events: McpEventService,
        host: Arc<dyn ClaudeHost>,
        store: Store,
        clock: SharedClock,
    ) -> Self {
        Self {
            events,
            host,
            store,
            clock,
            notify: None,
        }
    }

    /// Also notify each session's `notify` target of its finished turns.
    #[must_use]
    pub fn with_notify(mut self, notify: ClaudeNotify) -> Self {
        self.notify = Some(notify);
        self
    }

    pub fn notify(&self) -> Option<&ClaudeNotify> {
        self.notify.as_ref()
    }

    /// Records a turn Omni just started, so a turn that ends before the next
    /// poll still produces an event.
    pub async fn note_turn_started(&self, turn: StartedTurn) -> Result<(), StoreError> {
        let row = ClaudeSessionWatch {
            session_id: turn.session_id,
            id: turn.id,
            project: turn.project,
            revision: turn.revision,
            settled: false,
            started: Some(true),
            misses: None,
            quiet: None,
            seen_at: self.clock.now_ms(),
            extra: Extra::default(),
        };
        self.upsert(row).await
    }

    async fn upsert(&self, row: ClaudeSessionWatch) -> Result<(), StoreError> {
        self.store
            .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
            .await
    }

    async fn delete(&self, session_id: &str) -> Result<(), StoreError> {
        let key = session_id.to_owned();
        self.store
            .write(move |tx| tx.delete::<ClaudeSessionWatch>(&key).map(|_| ()))
            .await
    }

    pub async fn poll(&self) -> Result<(), WatcherError> {
        if let Some(notify) = &self.notify {
            // Retries do not need the host.
            notify.deliver_due().await?;
        }
        let subscribed = self
            .events
            .has_active_subscription(CLAUDE_TURN_FINISHED)
            .await?;
        let notifying = match &self.notify {
            Some(notify) => notify.any_sessions().await?,
            None => false,
        };
        if !subscribed && !notifying {
            return Ok(());
        }
        let link = self.host.status();
        if !link.online || link.disabled {
            return Ok(());
        }
        let mut args = Map::new();
        args.insert("all".to_owned(), Value::Bool(false));
        args.insert("limit".to_owned(), json!(LIST_LIMIT));
        let listed = self
            .host
            .execute(HostCommand::List, args, COMMAND_TIMEOUT)
            .await?;
        let sessions = listed
            .get("sessions")
            .and_then(Value::as_array)
            .ok_or(WatcherError::MalformedList)?;
        let watches: HashMap<String, ClaudeSessionWatch> = self
            .store
            .read(|docs| docs.get_all::<ClaudeSessionWatch>())
            .await?
            .into_iter()
            .map(|watch| (watch.session_id.clone(), watch))
            .collect();
        let mut seen = HashSet::new();
        let mut tails = MAX_TAIL_LOOKUPS;
        for raw in sessions {
            let Some(summary) = SessionSummary::decode(raw) else {
                continue;
            };
            seen.insert(summary.session_id.clone());
            self.observe(watches.get(&summary.session_id), &summary, &mut tails)
                .await?;
        }
        // A stopped session leaves the running list; read the ones mid-turn,
        // least-missed first so one unreadable session cannot starve the rest.
        let mut missing: Vec<&ClaudeSessionWatch> = watches
            .values()
            .filter(|watch| !watch.settled && !seen.contains(&watch.session_id))
            .collect();
        missing.sort_by_key(|watch| watch.misses.unwrap_or(0));
        for watch in missing.into_iter().take(MAX_STATUS_LOOKUPS) {
            let mut args = Map::new();
            args.insert(
                "session".to_owned(),
                Value::String(watch.session_id.clone()),
            );
            let status = self
                .host
                .execute(HostCommand::Status, args, COMMAND_TIMEOUT)
                .await;
            let summary = status
                .as_ref()
                .ok()
                .and_then(|data| SessionSummary::decode(&Value::Object(data.clone())));
            if let Some(summary) = summary {
                self.observe(Some(watch), &summary, &mut tails).await?;
                continue;
            }
            let misses = watch.misses.unwrap_or(0) + 1;
            let gone = status
                .as_ref()
                .is_err_and(|error| error.code == "not_found");
            if gone {
                self.forget(&watch.session_id).await?;
            } else if misses >= MAX_MISSES {
                // Unreadable is not gone: keep its notify target for when it
                // is listed again (orphaned targets expire below).
                self.delete(&watch.session_id).await?;
            } else {
                self.upsert(ClaudeSessionWatch {
                    misses: Some(misses),
                    ..watch.clone()
                })
                .await?;
            }
        }
        let now = self.clock.now_ms();
        for watch in watches.values() {
            if !seen.contains(&watch.session_id) && watch.seen_at < now - WATCH_RETENTION_MS {
                self.forget(&watch.session_id).await?;
            }
        }
        if let Some(notify) = &self.notify {
            let mut known = seen;
            known.extend(watches.into_keys());
            notify
                .prune_targets(&known, now - WATCH_RETENTION_MS)
                .await?;
        }
        Ok(())
    }

    async fn forget(&self, session_id: &str) -> Result<(), StoreError> {
        self.delete(session_id).await?;
        match &self.notify {
            Some(notify) => notify.forget_session(session_id).await,
            None => Ok(()),
        }
    }

    /// The transcript's last item at exactly `revision`; `None` when it could
    /// not be read or moved on meanwhile (the next pass decides).
    async fn tail(&self, session_id: &str, revision: f64) -> Option<Tail> {
        let mut args = Map::new();
        args.insert("session".to_owned(), Value::String(session_id.to_owned()));
        args.insert("limit".to_owned(), json!(1));
        let data = self
            .host
            .execute(HostCommand::Read, args, COMMAND_TIMEOUT)
            .await
            .ok()?;
        if data.get("revision").and_then(Value::as_f64) != Some(revision) {
            return None;
        }
        let last = data
            .get("items")
            .and_then(Value::as_array)
            .and_then(|items| items.last());
        Some(Tail {
            kind: last
                .and_then(|item| item.get("kind"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            timestamp: last
                .and_then(|item| item.get("timestamp"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    async fn observe(
        &self,
        previous: Option<&ClaudeSessionWatch>,
        summary: &SessionSummary,
        tails: &mut usize,
    ) -> Result<(), StoreError> {
        let baseline = previous.map(TurnBaseline::of);
        let mut tail = None;
        let settled = match settled_from_status(baseline, &summary.status, summary.revision) {
            Some(settled) => settled,
            None => {
                if *tails == 0 {
                    return Ok(());
                }
                *tails -= 1;
                let Some(read) = self.tail(&summary.session_id, summary.revision).await else {
                    return Ok(());
                };
                let settled = settled_by_tail(baseline, summary.revision, read.kind.as_deref());
                tail = Some(read);
                settled
            }
        };
        let now = self.clock.now_ms();
        let project = summary
            .project
            .clone()
            .or_else(|| previous.and_then(|p| p.project.clone()));
        let id = summary
            .id
            .clone()
            .or_else(|| previous.and_then(|p| p.id.clone()));
        let recent = previous.is_some_and(|p| p.seen_at > now - STALE_TURN_MS);
        if recent && finished_turn(baseline, settled, summary.revision) {
            let key = format!(
                "{CLAUDE_TURN_FINISHED}:{}:{}",
                summary.session_id,
                omni_core::js::number_to_string(summary.revision)
            );
            let data = json!({
                "sessionId": summary.session_id,
                "id": id,
                "project": project,
                "status": summary.status,
                "revision": js_number(summary.revision),
            });
            self.events
                .publish(PublishInput {
                    name: CLAUDE_TURN_FINISHED.to_owned(),
                    receipt_key: omni_core::digest::sha256_hex(&key),
                    event_key: key,
                    timestamp: omni_core::js::to_iso_string(now),
                    data: data.as_object().cloned().unwrap_or_default(),
                })
                .await?;
            if let Some(notify) = &self.notify {
                let finished_at = tail
                    .as_ref()
                    .and_then(|t| t.timestamp.as_deref())
                    .and_then(|t| t.parse::<jiff::Timestamp>().ok())
                    .map_or(now, |t| t.as_millisecond());
                let host = self.host.status().host;
                let scrub = |text: &Option<String>| {
                    text.as_ref().and_then(|text| {
                        scrub_host_details(&Value::String(text.clone()), host.as_deref())
                            .as_str()
                            .map(str::to_owned)
                    })
                };
                notify
                    .turn_finished(&FinishedTurn {
                        session_id: summary.session_id.clone(),
                        short_id: id.clone(),
                        project: project.clone(),
                        title: scrub(&summary.title),
                        status: summary.status.clone(),
                        state: summary.state.clone(),
                        revision: summary.revision,
                        finished_at,
                        // A turn that never ran has no reply of its own.
                        summary: if tail.as_ref().and_then(|t| t.kind.as_deref()) == Some("user") {
                            None
                        } else {
                            scrub(&summary.last_assistant)
                        },
                    })
                    .await?;
            }
        }
        // Omni's turn that has not reached the transcript keeps its baseline.
        let pending_start =
            baseline.is_some_and(|b| b.started && summary.revision <= b.revision && !settled);
        let (revision, started) = match (pending_start, baseline) {
            (true, Some(b)) => (b.revision, Some(true)),
            _ => (summary.revision, None),
        };
        self.upsert(ClaudeSessionWatch {
            session_id: summary.session_id.clone(),
            id,
            project,
            revision,
            settled,
            started,
            misses: None,
            quiet: (!is_active(&summary.status)).then_some(true),
            seen_at: now,
            extra: Extra::default(),
        })
        .await
    }
}

impl ClaudeSessionNotifier for ClaudeSessionWatcher {
    fn note_turn_started<'a>(&'a self, turn: &'a StartedTurn) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Err(error) = ClaudeSessionWatcher::note_turn_started(self, turn.clone()).await {
                tracing::debug!(target: "MCP:ClaudeSessions", error = %error, "Turn start not recorded");
            }
        })
    }
}
