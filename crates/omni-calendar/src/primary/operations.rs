//! Idempotent, verified writes to the primary calendar.
//!
//! Each write tool call carries an idempotency key. The plan (exact bodies
//! and preconditions) is reserved durably before the first request; each
//! step is marked `sending` before it goes out and finished by a verifying
//! read. A repeated key returns the recorded result, never writes again, and
//! an uncertain step is only ever reconciled by reading the resource back.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use omni_core::digest::sha256_hex;
use omni_http::SideEffectMode;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{Store, StoreError};
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::client::{DavClient, Precondition, ReadError, WriteError, error_names};
use super::edit::{self, Current, EditCtx, NewEvent, Patch, Plan, Scope};
use super::ics::IcsDoc;
use super::identity::PrimaryIdentity;
use super::model;
use super::store::{
    DELETED, ECHO_TTL_MS, OperationErrorRecord, OperationRecord, OperationState, OperationStep,
    StepKind, StepState, WriteEcho, echo_key,
};
use super::sync::event_url;
use super::{PrimaryCalendar, PrimaryError};
use crate::persistence;

const LOG: &str = "CalendarPrimary";

/// An uncertain step whose resource still shows the old content after this
/// long is settled as not applied.
const SETTLE_AFTER_MS: i64 = 10 * 60 * 1000;

static KEY_PATTERN: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{16,128}$").ok());

pub fn is_valid_key(key: &str) -> bool {
    KEY_PATTERN.as_ref().is_some_and(|re| re.is_match(key))
}

/// What a write tool asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteAction {
    Create(NewEvent),
    Update {
        event_id: String,
        expected_etag: Option<String>,
        scope: Scope,
        recurrence_id: Option<String>,
        patch: Patch,
        drop_exceptions: bool,
        send_notifications: bool,
    },
    Delete {
        event_id: String,
        expected_etag: Option<String>,
        scope: Scope,
        recurrence_id: Option<String>,
        send_notifications: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteRequest {
    pub tool: &'static str,
    pub idempotency_key: String,
    /// SHA-256 of the canonical tool input (a key reused with other input
    /// is refused).
    pub fingerprint: String,
    pub action: WriteAction,
}

/// The result of a write (stored on the operation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteOutcome {
    pub status: String,
    pub event_id: Option<String>,
    pub etag: Option<String>,
    /// `confirmed` or `uncertain`.
    pub state: String,
    pub verified: bool,
    pub scheduling_notified: bool,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub replayed: bool,
}

/// A planned write and the resources it starts from.
pub struct Prepared {
    pub plan: Plan,
    pub identity: PrimaryIdentity,
    /// The current text of each existing resource the plan writes.
    pub before: BTreeMap<String, String>,
}

fn read_failure(error: ReadError) -> PrimaryError {
    match error {
        ReadError::TooLarge(limit) => PrimaryError::coded(
            "event_too_large",
            format!("the event exceeds {limit} bytes and cannot be edited here"),
        ),
        ReadError::Transport(message) => PrimaryError::Transport {
            message,
            transient: true,
        },
    }
}

impl PrimaryCalendar {
    /// Reads one resource fresh from the server: `(etag, text)`, or `None`.
    pub async fn fetch(
        &self,
        identity: &PrimaryIdentity,
        event_id: &str,
    ) -> Result<Option<(String, String)>, PrimaryError> {
        let url = event_url(&identity.collection_url, event_id)?;
        let response = self
            .client(identity)
            .get(&url)
            .await
            .map_err(read_failure)?;
        match response.status {
            404 => Ok(None),
            401 | 403 => {
                self.invalidate_identity();
                Err(PrimaryError::coded(
                    "unauthorized",
                    format!("iCloud refused the read (HTTP {})", response.status),
                ))
            }
            s if (200..300).contains(&s) => {
                let etag = response.etag.clone().ok_or_else(|| {
                    PrimaryError::protocol("the server returned no ETag for the event")
                })?;
                Ok(Some((etag, response.text())))
            }
            s => Err(PrimaryError::Transport {
                message: format!("reading the event failed: HTTP {s}"),
                transient: s >= 500,
            }),
        }
    }

    /// Plans a write against fresh server state without sending anything.
    pub async fn prepare(&self, action: &WriteAction, key: &str) -> Result<Prepared, PrimaryError> {
        let identity = self.identity(false).await?;
        if !identity.writable {
            return Err(PrimaryError::coded(
                "calendar_read_only",
                "the primary calendar does not allow writes for this account",
            ));
        }
        let ctx = EditCtx {
            now: self.now(),
            default_tz: self.default_tz().clone(),
            owner: identity.owner_addresses.clone(),
            key: key.to_owned(),
        };
        let mut before = BTreeMap::new();
        let plan = match action {
            WriteAction::Create(input) => edit::plan_create(&ctx, input)?,
            WriteAction::Update {
                event_id,
                expected_etag,
                scope,
                recurrence_id,
                patch,
                drop_exceptions,
                send_notifications,
            } => {
                let (etag, text) = self
                    .current(&identity, event_id, expected_etag.as_deref())
                    .await?;
                let current = Current {
                    event_id,
                    etag: &etag,
                    ics: &text,
                };
                let plan = edit::plan_update(
                    &ctx,
                    &current,
                    *scope,
                    recurrence_id.as_deref(),
                    patch,
                    *drop_exceptions,
                    *send_notifications,
                )?;
                before.insert(event_id.clone(), text);
                plan
            }
            WriteAction::Delete {
                event_id,
                expected_etag,
                scope,
                recurrence_id,
                send_notifications,
            } => {
                let (etag, text) = self
                    .current(&identity, event_id, expected_etag.as_deref())
                    .await?;
                let current = Current {
                    event_id,
                    etag: &etag,
                    ics: &text,
                };
                let plan = edit::plan_delete(
                    &ctx,
                    &current,
                    *scope,
                    recurrence_id.as_deref(),
                    *send_notifications,
                )?;
                before.insert(event_id.clone(), text);
                plan
            }
        };
        Ok(Prepared {
            plan,
            identity,
            before,
        })
    }

    async fn current(
        &self,
        identity: &PrimaryIdentity,
        event_id: &str,
        expected_etag: Option<&str>,
    ) -> Result<(String, String), PrimaryError> {
        let Some((etag, text)) = self.fetch(identity, event_id).await? else {
            return Err(PrimaryError::coded(
                "not_found",
                "no event with this eventId exists in the primary calendar",
            ));
        };
        if let Some(expected) = expected_etag
            && expected != etag
        {
            return Err(PrimaryError::coded(
                "version_conflict",
                format!("the event changed since it was read; its current etag is {etag}"),
            ));
        }
        Ok((etag, text))
    }

    async fn load_operation(
        &self,
        key_hash: &str,
    ) -> Result<Option<OperationRecord>, PrimaryError> {
        let key = key_hash.to_owned();
        Ok(self
            .store()
            .read(move |docs| docs.get::<OperationRecord>(&key))
            .await?)
    }

    async fn save_operation(&self, record: &OperationRecord) -> Result<(), PrimaryError> {
        let mut record = record.clone();
        record.compact();
        let ttl = record.ttl_ms();
        self.store()
            .write(move |tx| {
                tx.upsert(
                    &record,
                    UpsertOpts {
                        expires_at: None,
                        ttl_ms: ttl,
                    },
                )
            })
            .await
            .map_err(PrimaryError::store)
    }

    /// Runs a write tool call.
    pub async fn execute(&self, request: WriteRequest) -> Result<WriteOutcome, PrimaryError> {
        if !is_valid_key(&request.idempotency_key) {
            return Err(PrimaryError::invalid(
                "idempotencyKey must be 16-128 characters of A-Z, a-z, 0-9, _ or -",
            ));
        }
        let key_hash = sha256_hex(request.idempotency_key.as_bytes());
        let _guard = self.write_lock().lock().await;
        if let Some(existing) = self.load_operation(&key_hash).await? {
            if existing.fingerprint != request.fingerprint || existing.tool != request.tool {
                return Err(PrimaryError::coded(
                    "idempotency_key_reused",
                    "this idempotencyKey was used for a different request; use a new key",
                ));
            }
            return self.replay(existing).await;
        }
        let prepared = self
            .prepare(&request.action, &request.idempotency_key)
            .await?;
        let now = self.now_ms();
        let plan = prepared.plan;
        let mut record = OperationRecord {
            key_hash: key_hash.clone(),
            idempotency_key: request.idempotency_key.clone(),
            fingerprint: request.fingerprint.clone(),
            tool: request.tool.to_owned(),
            state: OperationState::Reserved,
            steps: plan
                .steps
                .iter()
                .map(|step| OperationStep {
                    kind: step.kind,
                    event_id: step.event_id.clone(),
                    precondition: step.precondition.describe(),
                    body_sha256: step.body.as_deref().map(|b| sha256_hex(b.as_bytes())),
                    state: StepState::Planned,
                    result_etag: None,
                    http_status: None,
                    detail: None,
                    before_ics: prepared.before.get(&step.event_id).cloned(),
                })
                .collect(),
            planned_bodies: plan
                .steps
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.body.clone().map(|b| (i.to_string(), b)))
                .collect(),
            result: None,
            error: None,
            created_at: now,
            updated_at: now,
            extra: Default::default(),
        };
        if plan.steps.is_empty() {
            let outcome = WriteOutcome {
                status: plan.status.as_str().to_owned(),
                event_id: plan.result_event_id.clone(),
                etag: None,
                state: "confirmed".to_owned(),
                verified: true,
                scheduling_notified: false,
                warnings: plan.warnings.clone(),
                replayed: false,
            };
            record.state = OperationState::Confirmed;
            record.result = serde_json::to_value(&outcome).ok();
            self.save_operation(&record).await?;
            return Ok(outcome);
        }
        self.save_operation(&record).await?;

        let service = self.clone();
        let identity = prepared.identity;
        let work = async move {
            // A sync between a step's send and its echo would record Omni's
            // own write as external and deliver it to `origin: external`.
            let _sync = service.sync_lock().lock().await;
            service.run_steps(record, plan, identity).await
        };
        let outcome = match self.tracker() {
            Some(tracker) => omni_core::spawn::must_complete(tracker, work).await,
            None => work.await,
        }?;
        if outcome.state == "confirmed" {
            // Best effort: the change feed records the write as Omni's.
            if let Err(error) = self.sync_now().await {
                tracing::warn!(target: LOG, "Sync after write failed: {error}");
            }
        }
        Ok(outcome)
    }

    async fn run_steps(
        &self,
        mut record: OperationRecord,
        plan: Plan,
        identity: PrimaryIdentity,
    ) -> Result<WriteOutcome, PrimaryError> {
        let client = self.client(&identity);
        let collection = identity.collection_url.clone();
        let mut warnings = plan.warnings.clone();
        let mut verified_all = true;
        let mut last_etag: Option<String> = None;
        for (index, step) in plan.steps.iter().enumerate() {
            record.steps[index].state = StepState::Sending;
            record.updated_at = self.now_ms();
            self.save_operation(&record).await?;
            let url = event_url(&collection, &step.event_id)?;
            let sent = match (step.kind, &step.body, &step.precondition) {
                (StepKind::Put, Some(body), precondition) => {
                    client.put(&url, body.clone(), precondition).await
                }
                (StepKind::Delete, _, Precondition::IfMatch(etag)) => {
                    client.delete(&url, etag).await
                }
                _ => Err(WriteError::NotSent("invalid plan step".to_owned())),
            };
            let classified = classify(&sent);
            record.steps[index].http_status = sent.as_ref().ok().map(|r| r.status);
            match classified {
                Classified::Applied => {
                    record.steps[index].state = StepState::Acknowledged;
                    record.steps[index].result_etag =
                        sent.as_ref().ok().and_then(|r| r.etag.clone());
                }
                Classified::Rejected { code, message } => {
                    if matches!(code, "unauthorized" | "forbidden") {
                        self.invalidate_identity();
                    }
                    record.steps[index].state = StepState::Rejected;
                    record.steps[index].detail = Some(message.clone());
                    let compensation = if index > 0 {
                        self.compensate(&client, &collection, &mut record, index)
                            .await
                    } else {
                        None
                    };
                    record.state = if compensation.as_deref() == Some("uncertain") {
                        OperationState::Uncertain
                    } else {
                        OperationState::Failed
                    };
                    let message = match &compensation {
                        Some(outcome) => format!("{message} (earlier steps: {outcome})"),
                        None => message,
                    };
                    record.error = Some(OperationErrorRecord {
                        code: code.to_owned(),
                        message: message.clone(),
                    });
                    record.updated_at = self.now_ms();
                    self.save_operation(&record).await?;
                    return Err(PrimaryError::coded(code, message));
                }
                Classified::Uncertain(message) => {
                    record.steps[index].state = StepState::Uncertain;
                    record.steps[index].detail = Some(message.clone());
                    record.state = OperationState::Uncertain;
                    record.updated_at = self.now_ms();
                    self.save_operation(&record).await?;
                    tracing::warn!(target: LOG, "Calendar write outcome uncertain: {message}");
                    // Reconcile by reading only.
                    return self.reconcile(record, &identity).await;
                }
            }
            if client.is_recording() {
                verified_all = false;
                record.steps[index].state = StepState::Verified;
                continue;
            }
            match self.verify(&identity, &record, index).await? {
                Verification::Matches(etag) => {
                    record.steps[index].state = StepState::Verified;
                    record.steps[index].result_etag = etag.clone();
                    self.echo(&step.event_id, etag.as_deref(), &record.key_hash)
                        .await?;
                    if step.kind == StepKind::Put {
                        last_etag = etag;
                    }
                }
                Verification::Differs(detail) => {
                    verified_all = false;
                    record.steps[index].state = StepState::Verified;
                    warnings.push(detail);
                }
            }
            record.updated_at = self.now_ms();
            self.save_operation(&record).await?;
        }
        if client.is_recording() {
            warnings.push("side effects are recorded, not sent".to_owned());
        }
        let outcome = WriteOutcome {
            status: plan.status.as_str().to_owned(),
            event_id: plan.result_event_id.clone(),
            etag: if plan.result_event_id.is_some() {
                last_etag
            } else {
                None
            },
            state: "confirmed".to_owned(),
            verified: verified_all,
            scheduling_notified: plan.scheduling_notified,
            warnings,
            replayed: false,
        };
        record.state = OperationState::Confirmed;
        record.result = serde_json::to_value(&outcome).ok();
        record.updated_at = self.now_ms();
        self.save_confirmed(&record).await?;
        Ok(outcome)
    }

    /// Undoes acknowledged steps of a failed split (deleting the series the
    /// first step created). Returns a short description.
    async fn compensate(
        &self,
        client: &DavClient,
        collection: &omni_http::Url,
        record: &mut OperationRecord,
        failed: usize,
    ) -> Option<String> {
        for index in (0..failed).rev() {
            let step = &record.steps[index];
            if step.kind != StepKind::Put || step.precondition != "if-none-match" {
                return Some("uncertain".to_owned());
            }
            let Some(etag) = step.result_etag.clone() else {
                return Some("uncertain".to_owned());
            };
            let Ok(url) = event_url(collection, &step.event_id) else {
                return Some("uncertain".to_owned());
            };
            match client.delete(&url, &etag).await {
                Ok(response) if (200..300).contains(&response.status) || response.status == 404 => {
                    record.steps[index].detail = Some("rolled back".to_owned());
                }
                _ => return Some("uncertain".to_owned()),
            }
        }
        Some("rolled back".to_owned())
    }

    async fn verify(
        &self,
        identity: &PrimaryIdentity,
        record: &OperationRecord,
        index: usize,
    ) -> Result<Verification, PrimaryError> {
        let step = &record.steps[index];
        let fetched = match self.fetch(identity, &step.event_id).await {
            Ok(fetched) => fetched,
            Err(error) => {
                return Ok(Verification::Differs(format!(
                    "the write was acknowledged but could not be read back ({error})"
                )));
            }
        };
        match (step.kind, fetched) {
            (StepKind::Delete, None) => Ok(Verification::Matches(None)),
            (StepKind::Delete, Some(_)) => Ok(Verification::Differs(
                "the event is still present after the delete was acknowledged".to_owned(),
            )),
            (StepKind::Put, None) => Ok(Verification::Differs(
                "the written event was not found when read back".to_owned(),
            )),
            (StepKind::Put, Some((etag, text))) => {
                let intended = record
                    .planned_bodies
                    .get(&index.to_string())
                    .and_then(|b| IcsDoc::parse(b).ok());
                let fetched = IcsDoc::parse(&text).ok();
                match (intended, fetched) {
                    (Some(intended), Some(fetched)) => {
                        let mismatches = model::semantic_mismatches(&intended, &fetched);
                        if mismatches.is_empty() {
                            Ok(Verification::Matches(Some(etag)))
                        } else {
                            Ok(Verification::Differs(format!(
                                "the server copy differs from what was written in: {}",
                                mismatches.join(", ")
                            )))
                        }
                    }
                    _ => Ok(Verification::Differs(
                        "the written event could not be parsed when read back".to_owned(),
                    )),
                }
            }
        }
    }

    async fn echo(
        &self,
        event_id: &str,
        etag: Option<&str>,
        operation_key: &str,
    ) -> Result<(), PrimaryError> {
        let echo = WriteEcho {
            key: echo_key(event_id, etag.unwrap_or(DELETED)),
            event_id: event_id.to_owned(),
            etag: etag.unwrap_or(DELETED).to_owned(),
            operation_key: operation_key.to_owned(),
            created_at: self.now_ms(),
            extra: Default::default(),
        };
        self.store()
            .write(move |tx| {
                tx.upsert(
                    &echo,
                    UpsertOpts {
                        expires_at: None,
                        ttl_ms: Some(ECHO_TTL_MS),
                    },
                )
            })
            .await
            .map_err(PrimaryError::store)
    }

    /// Stores a confirmed operation and, in the same transaction, tombstones
    /// the tracked rows of every pipeline-created event it deleted whole, so
    /// no email recreates them. A delete is never reported confirmed without
    /// its tombstone: when this fails the operation keeps its earlier state
    /// and repeating the key (or reading its status) settles it without
    /// sending anything.
    async fn save_confirmed(&self, record: &OperationRecord) -> Result<(), PrimaryError> {
        // A recorded (unsent) delete removed nothing.
        let deleted = if self.inner.deps.mode == SideEffectMode::Record {
            BTreeSet::new()
        } else {
            deleted_event_ids(record)
        };
        let mut record = record.clone();
        record.compact();
        let ttl = record.ttl_ms();
        self.store()
            .write(move |tx| {
                tx.upsert(
                    &record,
                    UpsertOpts {
                        expires_at: None,
                        ttl_ms: ttl,
                    },
                )?;
                let tombstoned = persistence::mark_calendar_events_deleted_in(tx, &deleted)?;
                if tombstoned > 0 {
                    tracing::info!(
                        target: LOG,
                        "Recorded the deletion of {tombstoned} tracked email event row(s)"
                    );
                }
                Ok(())
            })
            .await
            .map_err(PrimaryError::store)
    }

    async fn replay(&self, record: OperationRecord) -> Result<WriteOutcome, PrimaryError> {
        match record.state {
            OperationState::Confirmed => {
                let mut outcome: WriteOutcome = record
                    .result
                    .clone()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .ok_or_else(|| {
                        PrimaryError::Store("the stored result is unreadable".to_owned())
                    })?;
                outcome.replayed = true;
                Ok(outcome)
            }
            OperationState::Failed => {
                let error = record.error.unwrap_or(OperationErrorRecord {
                    code: "operation_failed".to_owned(),
                    message: "the write failed".to_owned(),
                });
                Err(PrimaryError::coded(
                    "operation_failed",
                    format!(
                        "[{}] {} (recorded for this idempotencyKey)",
                        error.code, error.message
                    ),
                ))
            }
            OperationState::Reserved | OperationState::Uncertain => {
                let identity = self.identity(false).await?;
                let mut outcome = self.reconcile(record, &identity).await?;
                outcome.replayed = true;
                Ok(outcome)
            }
        }
    }

    /// Settles uncertain steps by reading the resources back. Never writes.
    pub async fn reconcile(
        &self,
        mut record: OperationRecord,
        identity: &PrimaryIdentity,
    ) -> Result<WriteOutcome, PrimaryError> {
        let now = self.now_ms();
        let mut settled_failed = false;
        for index in 0..record.steps.len() {
            if !matches!(
                record.steps[index].state,
                StepState::Sending | StepState::Uncertain
            ) {
                continue;
            }
            match self.verify(identity, &record, index).await? {
                Verification::Matches(etag) => {
                    record.steps[index].state = StepState::Verified;
                    record.steps[index].result_etag = etag.clone();
                    let event_id = record.steps[index].event_id.clone();
                    self.echo(&event_id, etag.as_deref(), &record.key_hash)
                        .await?;
                }
                Verification::Differs(_) => {
                    // Measured from the reservation (just before the send):
                    // `updated_at` moves on every reconcile, so polling the
                    // status would otherwise postpone settling forever.
                    let unchanged = self.still_before(identity, &record, index).await;
                    if unchanged && now - record.created_at > SETTLE_AFTER_MS {
                        record.steps[index].state = StepState::Rejected;
                        record.steps[index].detail = Some("not applied".to_owned());
                        settled_failed = true;
                    } else {
                        record.steps[index].state = StepState::Uncertain;
                    }
                }
            }
        }
        let pending = record
            .steps
            .iter()
            .any(|s| matches!(s.state, StepState::Sending | StepState::Uncertain));
        // Steps after an uncertain one were never sent; once that one settles
        // they are simply not applied.
        let unsent = record.steps.iter().any(|s| s.state == StepState::Planned);
        settled_failed |= record.steps.iter().any(|s| s.state == StepState::Rejected);
        let any_applied = record.steps.iter().any(|s| s.state == StepState::Verified);
        let result_step = record
            .steps
            .iter()
            .rev()
            .find(|s| s.kind == StepKind::Put && s.state == StepState::Verified);
        let status = match record.tool.as_str() {
            t if t.ends_with("create") => "created",
            t if t.ends_with("delete") => "deleted",
            _ => "updated",
        };
        let mut outcome = WriteOutcome {
            status: status.to_owned(),
            event_id: result_step.map(|s| s.event_id.clone()),
            etag: result_step.and_then(|s| s.result_etag.clone()),
            state: "uncertain".to_owned(),
            verified: false,
            scheduling_notified: false,
            warnings: Vec::new(),
            replayed: false,
        };
        if !pending && !settled_failed && !unsent {
            record.state = OperationState::Confirmed;
            outcome.state = "confirmed".to_owned();
            outcome.verified = true;
            record.result = serde_json::to_value(&outcome).ok();
        } else if (settled_failed || unsent) && !any_applied && !pending {
            record.state = OperationState::Failed;
            record.error = Some(OperationErrorRecord {
                code: "not_applied".to_owned(),
                message: "the write did not take effect; plan it again with a new idempotencyKey"
                    .to_owned(),
            });
        } else {
            record.state = OperationState::Uncertain;
            outcome.warnings.push(
                "the write's outcome is not yet known; check calendar_write_status later instead of retrying"
                    .to_owned(),
            );
        }
        record.updated_at = now;
        if record.state == OperationState::Confirmed {
            self.save_confirmed(&record).await?;
        } else {
            self.save_operation(&record).await?;
        }
        if record.state == OperationState::Failed {
            return Err(PrimaryError::coded(
                "not_applied",
                "the write did not take effect; plan it again with a new idempotencyKey",
            ));
        }
        Ok(outcome)
    }

    /// Whether the resource still shows the content from before the step.
    async fn still_before(
        &self,
        identity: &PrimaryIdentity,
        record: &OperationRecord,
        index: usize,
    ) -> bool {
        let step = &record.steps[index];
        let Ok(fetched) = self.fetch(identity, &step.event_id).await else {
            return false;
        };
        match (&step.before_ics, fetched) {
            (None, None) => true,
            (Some(before), Some((_, text))) => {
                match (IcsDoc::parse(before), IcsDoc::parse(&text)) {
                    (Ok(a), Ok(b)) => model::semantic_mismatches(&a, &b).is_empty(),
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// The stored operation for a key, reconciled by reading when uncertain.
    pub async fn write_status(&self, key: &str) -> Result<Option<OperationRecord>, PrimaryError> {
        if !is_valid_key(key) {
            return Err(PrimaryError::invalid(
                "idempotencyKey must be 16-128 characters of A-Z, a-z, 0-9, _ or -",
            ));
        }
        let key_hash = sha256_hex(key.as_bytes());
        let _guard = self.write_lock().lock().await;
        let Some(record) = self.load_operation(&key_hash).await? else {
            return Ok(None);
        };
        if matches!(
            record.state,
            OperationState::Reserved | OperationState::Uncertain
        ) && let Ok(identity) = self.identity(false).await
        {
            // The reconcile result is stored; errors only mean still unknown.
            let _ = self.reconcile(record, &identity).await;
        }
        self.load_operation(&key_hash).await
    }
}

/// The pipeline identities (UID, and resource name without `.ics`) of the
/// events an operation deleted whole. Deleting one occurrence or truncating a
/// series is a PUT and never counts.
fn deleted_event_ids(record: &OperationRecord) -> BTreeSet<String> {
    let deleted = || {
        record
            .steps
            .iter()
            .filter(|s| s.kind == StepKind::Delete && s.state == StepState::Verified)
    };
    let uids = deleted()
        .filter_map(|s| s.before_ics.as_deref())
        .filter_map(|text| IcsDoc::parse(text).ok())
        .filter_map(|doc| model::primary_event(&doc).and_then(|e| e.uid()));
    // The pipeline names its resources `<uid>.ics`; this covers a step whose
    // earlier copy is absent or unreadable.
    let names = deleted().filter_map(|s| s.event_id.strip_suffix(".ics").map(str::to_owned));
    uids.chain(names).collect()
}

enum Verification {
    Matches(Option<String>),
    Differs(String),
}

enum Classified {
    Applied,
    Rejected { code: &'static str, message: String },
    Uncertain(String),
}

fn classify(sent: &Result<super::client::DavResponse, WriteError>) -> Classified {
    match sent {
        Err(WriteError::NotSent(message)) => Classified::Rejected {
            code: "transport_not_sent",
            message: format!("the request was not sent: {message}"),
        },
        Err(WriteError::Uncertain(message)) => Classified::Uncertain(message.clone()),
        Ok(response) => match response.status {
            200..=299 => Classified::Applied,
            412 => Classified::Rejected {
                code: "version_conflict",
                message: "the event changed after it was read; read it again and re-plan"
                    .to_owned(),
            },
            404 => Classified::Rejected {
                code: "not_found",
                message: "the event no longer exists".to_owned(),
            },
            401 => Classified::Rejected {
                code: "unauthorized",
                message: "iCloud rejected the credentials".to_owned(),
            },
            403 if error_names(&response.text(), "no-uid-conflict") => Classified::Rejected {
                code: "uid_conflict",
                message: "an event with this UID already exists in another calendar".to_owned(),
            },
            403 => Classified::Rejected {
                code: "forbidden",
                message: format!("iCloud refused the write: {}", response.excerpt()),
            },
            s if s >= 500 => Classified::Uncertain(format!("HTTP {s}")),
            s => Classified::Rejected {
                code: "caldav_refused",
                message: format!(
                    "iCloud refused the write (HTTP {s}): {}",
                    response.excerpt()
                ),
            },
        },
    }
}

/// Boot reconcile: operations interrupted mid-flight become uncertain (a
/// step was sending) or failed (nothing was sent). Never sends anything.
pub async fn reconcile_interrupted(store: &Store, now_ms: i64) -> Result<u64, StoreError> {
    store
        .write(move |tx| {
            let mut changed = 0u64;
            for mut record in tx.get_all::<OperationRecord>()? {
                if record.state != OperationState::Reserved {
                    continue;
                }
                let sent = record
                    .steps
                    .iter()
                    .any(|s| !matches!(s.state, StepState::Planned));
                for step in &mut record.steps {
                    if step.state == StepState::Sending {
                        step.state = StepState::Uncertain;
                    }
                }
                if sent {
                    record.state = OperationState::Uncertain;
                } else {
                    record.state = OperationState::Failed;
                    record.error = Some(OperationErrorRecord {
                        code: "interrupted".to_owned(),
                        message: "the write was interrupted before anything was sent".to_owned(),
                    });
                }
                record.updated_at = now_ms;
                record.compact();
                let ttl = record.ttl_ms();
                tx.upsert(
                    &record,
                    UpsertOpts {
                        expires_at: None,
                        ttl_ms: ttl,
                    },
                )?;
                changed += 1;
            }
            Ok(changed)
        })
        .await
}
