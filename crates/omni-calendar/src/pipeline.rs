//! The `CalendarEvents` email handler.
//!
//! Filter every email, discover the calendar once (lazily, cached), then per
//! candidate: download PDFs, extract events with the model, and create, cancel
//! or update CalDAV events, recording one activity row per email.
//! The `error` (extraction failure), `no_matches` and final outcome activity
//! rows are all recorded.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use jiff::tz::TimeZone;
use omni_ai::{Ai, ModelRole};
use omni_alerts::{Pushover, PushoverChannel, PushoverMessage};
use omni_config::Config;
use omni_core::BoxError;
use omni_core::clock::SharedClock;
use omni_core::email::{EmailHandler, FetchedEmail, HandlerError};
use omni_store::Store;
use tokio::sync::Mutex;

use crate::caldav::{Caldav, CaldavSession, CreateOutcome, DeleteOutcome, UpdateOutcome};
use crate::error::{CaldavError, CalendarExtractionError, CalendarPersistenceError};
use crate::extraction::attachments::download_supported_attachments;
use crate::extraction::extract::{
    EmailContent, ExistingEventContext, ExtractionInput, extract_calendar_events,
};
use crate::extraction::sanitize::{
    MAX_LOCATION_CHARS, MAX_TITLE_CHARS, sanitize_time_zone, truncated,
};
use crate::extraction::schema::{EventAction, ExtractedEvent};
use crate::filter::{FilterResult, filter_calendar_candidate};
use crate::logfile::{RunLogFile, log_timestamp};
use crate::persistence::{
    self, CreateBlock, CreatedCalendarEvent, EventFields, EventHandles, compute_calendar_event_uid,
    compute_event_hash, has_event_changed, resolve_event_reference,
    resolve_explicit_event_reference,
};
use crate::support::{
    ActivityEntry, ActivityOutcome, AdmitTier, AttachmentSource, CostCents, EmailSupport, PIPELINE,
    SupportError, derive_items_outcome, sum_cost_cents,
};

const LOG: &str = "Main:CalendarEvents";

/// A batch failure: the dispatcher replays the batch (dedup makes that safe).
#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error(transparent)]
    Persistence(#[from] CalendarPersistenceError),
    #[error(transparent)]
    Support(#[from] SupportError),
}

/// One extracted event's create/cancel/update result.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ItemResult {
    /// Short result line for the activity record.
    line: String,
    ok: bool,
    /// Set when a retryable CalDAV failure (network error / 5xx) occurred.
    transient: Option<String>,
}

impl ItemResult {
    fn new(line: String, ok: bool) -> Self {
        Self {
            line,
            ok,
            transient: None,
        }
    }
}

/// What an update of a tracked event came to.
enum Applied {
    /// Updated, unchanged, or failed: the item is settled.
    Done(ItemResult),
    /// The tracked event no longer exists (retired since the snapshot, or
    /// deleted from the calendar); nothing was written.
    Gone(ItemResult),
}

/// Why an item operation stopped.
enum OpError {
    /// Aborts the batch.
    Persistence(CalendarPersistenceError),
    /// A transport-level CalDAV failure: retryable, recorded on the item.
    Caldav(CaldavError),
}

impl From<CalendarPersistenceError> for OpError {
    fn from(e: CalendarPersistenceError) -> Self {
        OpError::Persistence(e)
    }
}

impl From<CaldavError> for OpError {
    fn from(e: CaldavError) -> Self {
        OpError::Caldav(e)
    }
}

/// Network-shaped failures and server errors are retryable; 4xx are not.
fn is_transient_code(code: u16) -> bool {
    code >= 500
}

struct Candidate<'a> {
    email: &'a FetchedEmail,
    admit_reason: String,
    admit_tier: AdmitTier,
}

/// Everything the pipeline needs.
pub struct PipelineDeps {
    pub config: Arc<Config>,
    pub store: Store,
    pub clock: SharedClock,
    pub ai: Ai,
    pub pushover: Pushover,
    pub caldav: Caldav,
    pub support: Arc<dyn EmailSupport>,
    pub attachments: Arc<dyn AttachmentSource>,
}

pub struct CalendarEventPipeline {
    deps: PipelineDeps,
    tz: TimeZone,
    session: Mutex<Option<CaldavSession>>,
    /// Serializes event writes across concurrent batches.
    writes: Mutex<()>,
}

impl CalendarEventPipeline {
    pub fn new(deps: PipelineDeps) -> Self {
        let tz = TimeZone::get(&deps.config.tz).unwrap_or(TimeZone::UTC);
        Self {
            deps,
            tz,
            session: Mutex::new(None),
            writes: Mutex::new(()),
        }
    }

    fn triage_cost_for(&self, email_id: &str, tier: AdmitTier) -> CostCents {
        // Triage cost only counts toward this row when triage admitted it.
        (tier == AdmitTier::Triage).then(|| self.deps.support.triage_cost_cents(email_id))
    }

    async fn record(&self, entry: ActivityEntry) -> Result<(), PipelineError> {
        Ok(self.deps.support.record_activity(entry).await?)
    }

    async fn enqueue_retry(&self, email_id: &str, reason: &str) -> Result<(), PipelineError> {
        Ok(self
            .deps
            .support
            .enqueue_retry(PIPELINE, email_id, reason)
            .await?)
    }

    /// The cached session, discovering it on first use.
    async fn session(&self) -> Result<CaldavSession, CaldavError> {
        let mut cached = self.session.lock().await;
        if let Some(session) = cached.as_ref() {
            return Ok(session.clone());
        }
        let session = self.deps.caldav.discover().await?;
        *cached = Some(session.clone());
        Ok(session)
    }

    /// Processes a dispatched batch.
    pub async fn handle_emails(&self, emails: &[FetchedEmail]) -> Result<(), PipelineError> {
        let self_address = self.deps.config.email_self_address().map(str::to_owned);
        let mut candidates = Vec::new();
        for email in emails {
            let result = filter_calendar_candidate(
                email,
                self.deps.support.as_ref(),
                self_address.as_deref(),
            )
            .await?;
            match result {
                FilterResult::Pass { reason, admit_tier } => {
                    tracing::info!(
                        target: LOG,
                        "Candidate ({reason}): \"{}\" from {}",
                        email.subject,
                        email.from
                    );
                    candidates.push(Candidate {
                        email,
                        admit_reason: reason,
                        admit_tier,
                    });
                }
                FilterResult::Reject { reason } => {
                    tracing::info!(
                        target: LOG,
                        "Skipped ({reason}): \"{}\" from {}",
                        email.subject,
                        email.from
                    );
                    // A triage-rejected email still incurred a paid LLM call.
                    let mut entry = ActivityEntry::new(email, ActivityOutcome::Filtered);
                    entry.detail = Some(reason);
                    entry.cost_cents = Some(self.deps.support.triage_cost_cents(&email.id));
                    self.record(entry).await?;
                }
            }
        }
        if candidates.is_empty() {
            return Ok(());
        }

        let session = match self.session().await {
            Ok(session) => session,
            Err(error) => {
                tracing::error!(
                    target: LOG,
                    "Failed to discover calendar URL, skipping batch: {error}"
                );
                // The email cursor still advances, so queue the candidates for retry.
                let reason = format!("calendar discovery failed: {error}");
                for candidate in &candidates {
                    self.enqueue_retry(&candidate.email.id, &reason).await?;
                    let mut entry = ActivityEntry::new(candidate.email, ActivityOutcome::Error);
                    entry.detail = Some(reason.clone());
                    entry.admit_reason = Some(candidate.admit_reason.clone());
                    entry.admit_tier = Some(candidate.admit_tier);
                    entry.cost_cents = sum_cost_cents(&[
                        self.triage_cost_for(&candidate.email.id, candidate.admit_tier)
                    ]);
                    self.record(entry).await?;
                }
                return Ok(());
            }
        };

        for candidate in &candidates {
            let work = Box::pin(self.process_email(candidate, &session));
            self.deps
                .support
                .with_log_capture(format!("{PIPELINE}#{}", candidate.email.id), PIPELINE, work)
                .await?;
        }
        Ok(())
    }

    async fn run_log(&self) -> Option<RunLogFile> {
        let logs_path = self
            .deps
            .config
            .logs_path
            .as_deref()
            .filter(|p| !p.is_empty())?;
        let file = PathBuf::from(format!(
            "{logs_path}/calendar-events/{}.md",
            log_timestamp(self.deps.clock.now_ms(), &self.tz)
        ));
        RunLogFile::create(file).await
    }

    async fn existing_events(
        &self,
    ) -> Result<(EventHandles, Vec<ExistingEventContext>), PipelineError> {
        let recent =
            persistence::get_recent_events(&self.deps.store, self.deps.clock.now_ms(), &self.tz)
                .await?;
        // Each event gets a stable per-prompt handle the model echoes back. Fields are
        // re-sanitized so historical poisoned rows cannot re-enter prompts.
        let mut handles = HashMap::new();
        let mut contexts = Vec::with_capacity(recent.len());
        for (i, event) in recent.into_iter().enumerate() {
            let id = format!("evt_{}", i + 1);
            contexts.push(ExistingEventContext {
                id: id.clone(),
                title: truncated(&event.title, MAX_TITLE_CHARS),
                start_date: event.start_date.clone(),
                start_time: event.start_time.clone(),
                end_date: event.end_date.clone(),
                end_time: event.end_time.clone(),
                all_day: event.all_day.unwrap_or(false),
                location: event
                    .location
                    .as_deref()
                    .map(|l| truncated(l, MAX_LOCATION_CHARS)),
                time_zone: sanitize_time_zone(event.time_zone.as_deref()),
            });
            handles.insert(id, event);
        }
        Ok((handles, contexts))
    }

    async fn extract(
        &self,
        email: &FetchedEmail,
        contexts: &[ExistingEventContext],
        run_log: Option<&RunLogFile>,
    ) -> Result<crate::extraction::extract::ExtractionResult, CalendarExtractionError> {
        let downloaded =
            download_supported_attachments(self.deps.attachments.as_ref(), &email.attachments)
                .await;
        let model = self
            .deps
            .ai
            .model_for(&self.deps.config, ModelRole::CalendarExtraction)
            .map_err(|e| CalendarExtractionError::from_ai(&e))?;
        let input = ExtractionInput {
            email: EmailContent {
                subject: &email.subject,
                from: &email.from,
                text_body: &email.text_body,
            },
            attachments: &downloaded,
            local_time_zone: &self.deps.config.tz,
            existing_events: contexts,
            now_ms: self.deps.clock.now_ms(),
        };
        extract_calendar_events(&self.deps.ai, model.as_ref(), &input, run_log).await
    }

    async fn process_email(
        &self,
        candidate: &Candidate<'_>,
        session: &CaldavSession,
    ) -> Result<(), PipelineError> {
        let email = candidate.email;
        let triage_cost = self.triage_cost_for(&email.id, candidate.admit_tier);
        let activity = |outcome: ActivityOutcome| {
            let mut entry = ActivityEntry::new(email, outcome);
            entry.admit_reason = Some(candidate.admit_reason.clone());
            entry.admit_tier = Some(candidate.admit_tier);
            entry
        };
        let run_log = self.run_log().await;
        tracing::info!(
            target: LOG,
            "Extracting events from: \"{}\" (from: {})",
            email.subject,
            email.from
        );
        let (handles, contexts) = self.existing_events().await?;

        let extraction = match self.extract(email, &contexts, run_log.as_ref()).await {
            Ok(extraction) => extraction,
            Err(error) => {
                // A systemic failure alerts once per signature (in `report_systemic`),
                // not once per email through the ERROR log.
                if error.systemic.is_some() {
                    tracing::warn!(
                        target: LOG,
                        "Extraction failed for \"{}\" from {}: {error}",
                        email.subject,
                        email.from
                    );
                } else {
                    tracing::error!(
                        target: LOG,
                        "Extraction failed for \"{}\" from {}: {error}",
                        email.subject,
                        email.from
                    );
                }
                let mut entry = activity(ActivityOutcome::Error);
                entry.detail = Some(format!("extraction failed: {error}"));
                // Extraction failed, so its cost is unknown; only triage may count.
                entry.cost_cents = sum_cost_cents(&[triage_cost]);
                self.record(entry).await?;
                if let Some(signature) = &error.systemic {
                    self.deps
                        .support
                        .report_systemic(PIPELINE, &email.id, &error.to_string(), signature)
                        .await?;
                } else if error.transient {
                    self.enqueue_retry(&email.id, &error.to_string()).await?;
                }
                return Ok(());
            }
        };
        let cost_cents = sum_cost_cents(&[triage_cost, Some(extraction.cost_cents)]);

        if extraction.events.is_empty() {
            tracing::info!(target: LOG, "No calendar events found in \"{}\"", email.subject);
            let mut entry = activity(ActivityOutcome::NoMatches);
            entry.detail = Some("no calendar events found".to_owned());
            entry.cost_cents = cost_cents;
            self.record(entry).await?;
            return Ok(());
        }
        tracing::info!(
            target: LOG,
            "Found {} event(s) in \"{}\"",
            extraction.events.len(),
            email.subject
        );

        let reschedule_hint =
            persistence::mentions_reschedule(&format!("{}\n{}", email.subject, email.text_body));
        let mut items = Vec::new();
        let mut items_ok = Vec::new();
        let mut transient_failures = Vec::new();
        for event in &extraction.events {
            // An email that lists the same title more than once describes
            // several bookings, never a move of one of them.
            let reschedule =
                (!persistence::title_repeats(event, &extraction.events)).then_some(reschedule_hint);
            // The dispatcher, the retry task and a manual reprocess can hold
            // the same email at once: each item's check, write and record
            // runs alone, so the second sees the first's row.
            let _writing = self.writes.lock().await;
            let outcome = match event.action {
                EventAction::Create => {
                    self.handle_create(event, &email.id, &handles, reschedule, session)
                        .await
                }
                EventAction::Cancel => self.handle_cancel(event, &handles, session).await,
                EventAction::Update => {
                    self.handle_update(event, &handles, &email.id, reschedule, session)
                        .await
                }
            };
            let result = match outcome {
                Ok(result) => result,
                Err(OpError::Persistence(error)) => return Err(error.into()),
                Err(OpError::Caldav(error)) => {
                    // Transport failures are retryable; HTTP-level failures come back
                    // as item results instead.
                    let message = error.to_string();
                    tracing::error!(
                        target: LOG,
                        "Failed to process event \"{}\" ({}): {message}",
                        event.title,
                        event.action.as_str()
                    );
                    ItemResult {
                        line: format!(
                            "\"{}\" ({}): failed ({message})",
                            event.title,
                            event.action.as_str()
                        ),
                        ok: false,
                        transient: Some(message),
                    }
                }
            };
            items.push(result.line);
            items_ok.push(result.ok);
            if let Some(transient) = result.transient {
                transient_failures.push(transient);
            }
        }

        if !transient_failures.is_empty() {
            let reason = transient_failures.join("; ");
            tracing::warn!(
                target: LOG,
                "Transient CalDAV failure(s) for \"{}\"; queued for retry: {reason}",
                email.subject
            );
            self.enqueue_retry(&email.id, &reason).await?;
        }

        let mut entry = activity(derive_items_outcome(&items_ok));
        entry.items = Some(items);
        entry.cost_cents = cost_cents;
        self.record(entry).await
    }

    fn handle_create<'a>(
        &'a self,
        event: &'a ExtractedEvent,
        email_id: &'a str,
        handles: &'a EventHandles,
        reschedule: Option<bool>,
        session: &'a CaldavSession,
    ) -> BoxFuture<'a, Result<ItemResult, OpError>> {
        Box::pin(async move {
            let event_hash =
                compute_event_hash(&event.title, &event.start_date, event.start_time.as_deref());
            let label = format!("\"{}\" on {}", event.title, event.start_date);
            let existing = persistence::get_tracked_event(&self.deps.store, &event_hash).await?;
            if let Some(block) = persistence::create_block(existing.as_ref(), email_id) {
                let reason = match block {
                    CreateBlock::Duplicate => "duplicate, skipped",
                    CreateBlock::Deleted => "deleted from the calendar, not recreated",
                    CreateBlock::Superseded => "cancelled or moved by a later email, not recreated",
                };
                tracing::info!(
                    target: LOG,
                    "Not creating \"{}\" on {}: {reason}",
                    event.title,
                    event.start_date
                );
                return Ok(ItemResult::new(format!("{label}: {reason}"), true));
            }
            // A new time or date for an already tracked event updates it
            // instead of creating a second copy.
            if let Some(language) = reschedule
                && let Some(record) =
                    persistence::find_reschedule_target(event, handles.values(), language)
            {
                tracing::info!(
                    target: LOG,
                    "\"{}\" reschedules the event on {}; updating it",
                    event.title,
                    record.start_date
                );
                match self.apply_update(event, record, email_id, session).await? {
                    Applied::Done(result) => return Ok(result),
                    // The event this would have moved is gone, and it stays
                    // gone; the new date or time is a different event.
                    Applied::Gone(_) => {}
                }
            }
            let uid = compute_calendar_event_uid(&event_hash);
            let event_uid = match self
                .deps
                .caldav
                .writer()
                .create(session, event, &uid)
                .await?
            {
                CreateOutcome::Success { event_uid }
                | CreateOutcome::AlreadyExists { event_uid } => event_uid,
                CreateOutcome::Error { code, message } => {
                    tracing::error!(
                        target: LOG,
                        "Failed to create calendar event \"{}\": {message}",
                        event.title
                    );
                    return Ok(ItemResult {
                        line: format!("{label}: create failed ({message})"),
                        ok: false,
                        transient: is_transient_code(code).then_some(message),
                    });
                }
            };
            let record = CreatedCalendarEvent::from_event(
                event_hash,
                email_id.to_owned(),
                event_uid,
                event,
                self.deps.clock.now_ms(),
            );
            persistence::record_created_event(&self.deps.store, record).await?;
            self.notify("Calendar Event Created", event).await;
            tracing::info!(target: LOG, "Created: \"{}\" on {}", event.title, event.start_date);
            Ok(ItemResult::new(format!("{label}: created"), true))
        })
    }

    async fn handle_cancel(
        &self,
        event: &ExtractedEvent,
        handles: &EventHandles,
        session: &CaldavSession,
    ) -> Result<ItemResult, OpError> {
        // Cancels are destructive, so they require the explicit evt_N handle.
        let label = format!("\"{}\"", event.title);
        let Some(record) = resolve_explicit_event_reference(event.event_id.as_deref(), handles)
        else {
            tracing::warn!(
                target: LOG,
                "Cancel without explicit event reference: \"{}\" (skipping)",
                event.title
            );
            return Ok(ItemResult::new(
                format!("{label}: cancel without explicit reference, skipped"),
                false,
            ));
        };
        match self
            .deps
            .caldav
            .writer()
            .delete(session, &record.calendar_event_id)
            .await?
        {
            DeleteOutcome::Success | DeleteOutcome::NotFound => {}
            DeleteOutcome::Error { code, message } => {
                tracing::error!(
                    target: LOG,
                    "Failed to delete calendar event \"{}\": {message}",
                    event.title
                );
                return Ok(ItemResult {
                    line: format!("{label}: cancel failed ({message})"),
                    ok: false,
                    transient: is_transient_code(code).then_some(message),
                });
            }
        }
        persistence::mark_event_cancelled(&self.deps.store, &record.event_hash).await?;
        self.notify("Calendar Event Cancelled", event).await;
        tracing::info!(
            target: LOG,
            "Cancelled: \"{}\" on {}",
            event.title,
            record.start_date
        );
        Ok(ItemResult::new(
            format!("{label} on {}: cancelled", record.start_date),
            true,
        ))
    }

    async fn handle_update(
        &self,
        event: &ExtractedEvent,
        handles: &EventHandles,
        email_id: &str,
        reschedule: Option<bool>,
        session: &CaldavSession,
    ) -> Result<ItemResult, OpError> {
        let Some(record) = resolve_event_reference(
            event.event_id.as_deref(),
            &event.title,
            &event.start_date,
            handles,
            None,
        ) else {
            tracing::warn!(
                target: LOG,
                "Update requested for unknown event: \"{}\", treating as create",
                event.title
            );
            return self
                .handle_create(event, email_id, handles, reschedule, session)
                .await;
        };
        match self.apply_update(event, record, email_id, session).await? {
            Applied::Done(result) | Applied::Gone(result) => Ok(result),
        }
    }

    /// Merge-updates `record`'s calendar event with the email's fields and
    /// re-keys the tracked row.
    async fn apply_update(
        &self,
        event: &ExtractedEvent,
        record: &CreatedCalendarEvent,
        email_id: &str,
        session: &CaldavSession,
    ) -> Result<Applied, OpError> {
        let label = format!("\"{}\" on {}", event.title, event.start_date);

        // `record` is from the snapshot taken before extraction: an earlier
        // item of this email or a calendar tool may have retired it since.
        let current = persistence::get_tracked_event(&self.deps.store, &record.event_hash).await?;
        let Some(current) = current.filter(|row| !row.is_cancelled()) else {
            tracing::info!(
                target: LOG,
                "\"{}\" on {} is no longer tracked (skipping update)",
                event.title,
                record.start_date
            );
            return Ok(Applied::Gone(ItemResult::new(
                format!("{label}: event no longer tracked, skipped"),
                true,
            )));
        };

        // The model cannot see description/duration/reminderMinutes/recurrence, so
        // a full-PUT update would drop them: backfill from the stored record.
        let mut merged = event.clone();
        merged.description = event
            .description
            .clone()
            .or_else(|| record.description.clone());
        merged.duration = event.duration.clone().or_else(|| record.duration.clone());
        merged.reminder_minutes = event.reminder_minutes.or(record.reminder_minutes);
        merged.recurrence = event
            .recurrence
            .clone()
            .or_else(|| record.recurrence.clone());

        if !has_event_changed(record, &EventFields::from(&merged)) {
            tracing::info!(
                target: LOG,
                "No changes detected for \"{}\" on {} (skipping update)",
                event.title,
                event.start_date
            );
            return Ok(Applied::Done(ItemResult::new(
                format!("{label}: no changes, skipped"),
                true,
            )));
        }

        match self
            .deps
            .caldav
            .writer()
            .update(
                session,
                &record.to_event(),
                &merged,
                &record.calendar_event_id,
            )
            .await?
        {
            UpdateOutcome::Success { .. } => {}
            UpdateOutcome::Missing => {
                // Deleted in a calendar app: remember it so neither this
                // update nor a later email brings the event back.
                persistence::mark_calendar_event_deleted(
                    &self.deps.store,
                    &record.calendar_event_id,
                )
                .await?;
                tracing::info!(
                    target: LOG,
                    "\"{}\" on {} was deleted from the calendar (skipping update)",
                    event.title,
                    record.start_date
                );
                return Ok(Applied::Gone(ItemResult::new(
                    format!("{label}: deleted from the calendar, not recreated"),
                    true,
                )));
            }
            UpdateOutcome::Error { code, message } => {
                tracing::error!(
                    target: LOG,
                    "Failed to update calendar event \"{}\": {message}",
                    event.title
                );
                return Ok(Applied::Done(ItemResult {
                    line: format!("{label}: update failed ({message})"),
                    ok: false,
                    transient: is_transient_code(code).then_some(message),
                }));
            }
        }

        // Re-key to the updated identity unless that key belongs to a different
        // tracked event (the CalDAV event is already updated regardless).
        let new_hash = compute_event_hash(
            &merged.title,
            &merged.start_date,
            merged.start_time.as_deref(),
        );
        let collides = new_hash != record.event_hash
            && persistence::has_created_event(&self.deps.store, &new_hash).await?;
        if collides {
            tracing::warn!(
                target: LOG,
                "Update for \"{}\" collides with another tracked event's key; keeping existing key",
                merged.title
            );
        }
        let mut row = CreatedCalendarEvent::from_event(
            if collides {
                record.event_hash.clone()
            } else {
                new_hash
            },
            email_id.to_owned(),
            record.calendar_event_id.clone(),
            &merged,
            self.deps.clock.now_ms(),
        );
        if email_id != current.email_id {
            row.earlier_email_ids = current.writers();
        } else {
            row.earlier_email_ids = current.earlier_email_ids;
        }
        persistence::replace_created_event(&self.deps.store, row, &record.event_hash).await?;
        self.notify("Calendar Event Updated", &merged).await;
        tracing::info!(target: LOG, "Updated: \"{}\" on {}", event.title, event.start_date);
        Ok(Applied::Done(ItemResult::new(
            format!("{label}: updated"),
            true,
        )))
    }

    /// Pushover after a successful write and record; failures only warn.
    async fn notify(&self, title: &str, event: &ExtractedEvent) {
        let message = notification_text(event);
        let result = self
            .deps
            .pushover
            .send(
                PushoverChannel::Calendar,
                PushoverMessage {
                    message,
                    title: Some(title.to_owned()),
                    ..PushoverMessage::default()
                },
            )
            .await;
        if let Err(error) = result {
            tracing::warn!(target: LOG, "Failed to send notification: {error}");
        }
    }
}

/// `title\nstartDate[ (all day)| at HH:MM][\nlocation]`.
pub fn notification_text(event: &ExtractedEvent) -> String {
    let time_part = if event.all_day {
        "(all day)".to_owned()
    } else {
        match event.start_time.as_deref().filter(|t| !t.is_empty()) {
            Some(time) => format!("at {time}"),
            None => String::new(),
        }
    };
    let mut text = format!("{}\n{}", event.title, event.start_date);
    if !time_part.is_empty() {
        text.push(' ');
        text.push_str(&time_part);
    }
    if let Some(location) = event.location.as_deref().filter(|l| !l.is_empty()) {
        text.push('\n');
        text.push_str(location);
    }
    text
}

impl EmailHandler for CalendarEventPipeline {
    fn name(&self) -> &'static str {
        PIPELINE
    }

    fn handle<'a>(&'a self, emails: &'a [FetchedEmail]) -> BoxFuture<'a, Result<(), HandlerError>> {
        Box::pin(async move {
            self.handle_emails(emails).await.map_err(|error| {
                let message = error.to_string();
                HandlerError::transient(message, Some(Box::new(error) as BoxError))
            })
        })
    }
}
