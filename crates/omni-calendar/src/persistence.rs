//! Tracked calendar events: the
//! `calendar-created-event` entity, keyed by a normalized content hash, used for
//! create dedup and cancel/update matching.

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use jiff::tz::TimeZone;
use omni_core::digest::sha256_hex;
use omni_store::cbor::{self, Extra, JsValue};
use omni_store::entity::{self, Entity, EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{DocMeta, DocOps as _, DocWrite as _, Store, Tx};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::CalendarPersistenceError;
use crate::extraction::schema::{EventRecurrence, ExtractedEvent};

const LOG: &str = "Main:CalendarEvents";

/// Local tombstone state; absent means active.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventStatus {
    Cancelled,
}

/// `calendar-created-event`, keyed by `eventHash`. Field order matches the
/// stored rows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedCalendarEvent {
    pub event_hash: String,
    pub email_id: String,
    pub calendar_event_id: String,
    pub title: String,
    pub start_date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    /// Absent on rows written before the field existed (27 production rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all_day: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder_minutes: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<EventRecurrence>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<EventStatus>,
    /// Set (with `status: cancelled`) when the calendar event itself was
    /// deleted outside the email pipeline: by a calendar tool, or found
    /// missing by an update. The pipeline never recreates this exact event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<i64>,
    /// Emails that wrote this event before `email_id` did (updates and
    /// reschedules), oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier_email_ids: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for CreatedCalendarEvent {
    const NAME: &'static str = "calendar-created-event";
    type Key = String;
    fn key(&self) -> String {
        self.event_hash.clone()
    }
}

impl CreatedCalendarEvent {
    pub fn is_cancelled(&self) -> bool {
        self.status == Some(EventStatus::Cancelled)
    }

    /// Whether `email_id` wrote this event, now or before a later update.
    pub fn written_by(&self, email_id: &str) -> bool {
        self.email_id == email_id || self.earlier_email_ids.iter().any(|id| id == email_id)
    }

    /// The writers a row replacing this one (written by another email)
    /// carries forward, bounded.
    pub fn writers(&self) -> Vec<String> {
        const KEPT: usize = 20;
        let mut ids = self.earlier_email_ids.clone();
        if !ids.contains(&self.email_id) {
            ids.push(self.email_id.clone());
        }
        let excess = ids.len().saturating_sub(KEPT);
        ids.split_off(excess)
    }

    /// Whether the calendar event was deleted from the calendar itself.
    pub fn is_deleted(&self) -> bool {
        self.is_cancelled() && self.deleted_at.is_some()
    }

    /// A new active record of `event` (all extracted fields, no tombstone).
    pub fn from_event(
        event_hash: String,
        email_id: String,
        calendar_event_id: String,
        event: &ExtractedEvent,
        created_at: i64,
    ) -> Self {
        Self {
            event_hash,
            email_id,
            calendar_event_id,
            title: event.title.clone(),
            start_date: event.start_date.clone(),
            start_time: event.start_time.clone(),
            end_date: event.end_date.clone(),
            end_time: event.end_time.clone(),
            all_day: Some(event.all_day),
            location: event.location.clone(),
            time_zone: event.time_zone.clone(),
            description: event.description.clone(),
            duration: event.duration.clone(),
            reminder_minutes: event.reminder_minutes,
            recurrence: event.recurrence.clone(),
            created_at,
            status: None,
            deleted_at: None,
            earlier_email_ids: Vec::new(),
            extra: Extra::default(),
        }
    }
}

impl CreatedCalendarEvent {
    /// The event this row describes, as the pipeline last wrote it (the base
    /// of a merge update).
    pub fn to_event(&self) -> ExtractedEvent {
        let mut event = ExtractedEvent::new(
            crate::extraction::schema::EventAction::Create,
            self.title.clone(),
            self.start_date.clone(),
            self.all_day.unwrap_or(self.start_time.is_none()),
        );
        event.start_time = self.start_time.clone();
        event.end_date = self.end_date.clone();
        event.end_time = self.end_time.clone();
        event.location = self.location.clone();
        event.time_zone = self.time_zone.clone();
        event.description = self.description.clone();
        event.duration = self.duration.clone();
        event.reminder_minutes = self.reminder_minutes;
        event.recurrence = self.recurrence.clone();
        event
    }
}

static NON_WORD: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}\s\x{FEFF}]").ok());
static SPACES: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"[\s\x{FEFF}]+").ok());

/// Normalizes a title for identity comparison: strips emoji, arrows and other
/// punctuation and collapses casing and whitespace, so the same event
/// re-extracted with a slightly different title still dedups.
pub fn normalize_title(title: &str) -> String {
    let lower = title.to_lowercase();
    let (Some(non_word), Some(spaces)) = (NON_WORD.as_ref(), SPACES.as_ref()) else {
        return lower;
    };
    let stripped = non_word.replace_all(&lower, " ");
    let collapsed = spaces.replace_all(&stripped, " ");
    collapsed
        .trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
        .to_owned()
}

/// `"<normalizeTitle>|<startDate>|<startTime|allday>"`.
pub fn compute_event_hash(title: &str, start_date: &str, start_time: Option<&str>) -> String {
    format!(
        "{}|{start_date}|{}",
        normalize_title(title),
        start_time.unwrap_or("allday")
    )
}

/// Stable CalDAV resource identity for a logical event: a replay after the
/// server accepted the PUT but before local persistence uses the same resource
/// URL, so `If-None-Match` turns it into `already_exists`.
pub fn compute_calendar_event_uid(event_hash: &str) -> String {
    let digest = sha256_hex(event_hash.as_bytes());
    format!("omni-{}@omni-notify", &digest[..32])
}

/// The MCP tool's UID scheme (`mcp-<sha256[..32]>@omni-notify`).
pub fn compute_mcp_event_uid(event_hash: &str) -> String {
    let digest = sha256_hex(event_hash.as_bytes());
    format!("mcp-{}@omni-notify", &digest[..32])
}

/// UTC `YYYY-MM-DD` of the local wall-clock `now` shifted by `days` calendar
/// days (JS `setDate(getDate() + days)` then `toISOString().slice(0, 10)`).
fn shifted_date_stamp(now_ms: i64, days: i64, tz: &TimeZone) -> String {
    let Ok(now) = jiff::Timestamp::from_millisecond(now_ms) else {
        return String::new();
    };
    let shifted = now
        .to_zoned(tz.clone())
        .checked_add(jiff::Span::new().days(days))
        .map(|z| z.timestamp())
        .unwrap_or(now);
    shifted.strftime("%Y-%m-%d").to_string()
}

/// Active events within a window of 7 days past through `future_days` ahead.
pub fn select_recent_events(
    all: Vec<CreatedCalendarEvent>,
    future_days: i64,
    now_ms: i64,
    tz: &TimeZone,
) -> Vec<CreatedCalendarEvent> {
    let past = shifted_date_stamp(now_ms, -7, tz);
    let future = shifted_date_stamp(now_ms, future_days, tz);
    all.into_iter()
        .filter(|e| {
            !e.is_cancelled() && e.start_date.as_str() >= past.as_str() && e.start_date <= future
        })
        .collect()
}

/// From active candidates sharing a normalized title, the one on `start_date`;
/// otherwise a lone candidate only, never an arbitrary pick among several.
pub fn pick_by_start_date<'a>(
    active: &[&'a CreatedCalendarEvent],
    start_date: &str,
) -> Option<&'a CreatedCalendarEvent> {
    if let Some(exact) = active.iter().find(|e| e.start_date == start_date) {
        return Some(exact);
    }
    match active {
        [only] => Some(only),
        _ => None,
    }
}

/// An active event by normalized title + start date within `candidates` (the
/// windowed set shown to the model, never the full store), falling back to a
/// lone title-only match.
pub fn find_event<'a>(
    title: &str,
    start_date: &str,
    candidates: impl IntoIterator<Item = &'a CreatedCalendarEvent>,
) -> Option<&'a CreatedCalendarEvent> {
    let normalized = normalize_title(title);
    let active: Vec<&CreatedCalendarEvent> = candidates
        .into_iter()
        .filter(|e| !e.is_cancelled() && normalize_title(&e.title) == normalized)
        .collect();
    pick_by_start_date(&active, start_date)
}

/// How far a reschedule may move an event.
pub const RESCHEDULE_WINDOW_DAYS: i64 = 45;

static RESCHEDULE_WORDS: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(re-?schedul\w*|postpon\w*|moved|new (date|time)|date (has )?changed|time (has )?changed|changed to|updated (date|time|appointment))\b",
    )
    .ok()
});

/// Whether an email talks about moving an event.
pub fn mentions_reschedule(text: &str) -> bool {
    RESCHEDULE_WORDS
        .as_ref()
        .is_some_and(|re| re.is_match(text))
}

fn days_apart(a: &str, b: &str) -> Option<i64> {
    let a = a.parse::<jiff::civil::Date>().ok()?;
    let b = b.parse::<jiff::civil::Date>().ok()?;
    Some(i64::from(a.until(b).ok()?.get_days()).abs())
}

/// Whether another create or update in the same extraction has `event`'s
/// normalized title (several bookings, so none of them is a reschedule).
pub fn title_repeats(event: &ExtractedEvent, batch: &[ExtractedEvent]) -> bool {
    let title = normalize_title(&event.title);
    batch
        .iter()
        .filter(|other| {
            !matches!(other.action, crate::extraction::schema::EventAction::Cancel)
                && normalize_title(&other.title) == title
        })
        .count()
        > 1
}

/// The tracked event that a `create` actually reschedules: an active event
/// with the same normalized title that is not this exact event and either
/// (a) is on the same day at a different time (both timed, not recurring), or
/// (b) is on another day within [`RESCHEDULE_WINDOW_DAYS`] when the email
/// talks about rescheduling. Several candidates create as before.
pub fn find_reschedule_target<'a>(
    event: &ExtractedEvent,
    candidates: impl IntoIterator<Item = &'a CreatedCalendarEvent>,
    reschedule_language: bool,
) -> Option<&'a CreatedCalendarEvent> {
    if event.recurrence.is_some() {
        return None;
    }
    let title = normalize_title(&event.title);
    let hash = compute_event_hash(&event.title, &event.start_date, event.start_time.as_deref());
    let mut matches = candidates.into_iter().filter(|c| {
        if c.is_cancelled() || normalize_title(&c.title) != title || c.event_hash == hash {
            return false;
        }
        if c.start_date == event.start_date {
            return c.recurrence.is_none()
                && c.start_time.is_some()
                && event.start_time.is_some()
                && c.start_time != event.start_time;
        }
        reschedule_language
            && days_apart(&c.start_date, &event.start_date)
                .is_some_and(|d| d <= RESCHEDULE_WINDOW_DAYS)
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// Per-prompt handles (`evt_N`) to the events shown to the model.
pub type EventHandles = HashMap<String, CreatedCalendarEvent>;

/// Strict resolution through the explicit `evt_N` handle, without any
/// title/date fallback. Cancels use this so a title-only match (a receipt
/// echoing an upcoming appointment) can never delete an event.
pub fn resolve_explicit_event_reference<'a>(
    event_id: Option<&str>,
    by_id: &'a EventHandles,
) -> Option<&'a CreatedCalendarEvent> {
    let event_id = event_id.filter(|id| !id.is_empty())?;
    // Tolerate the handle echoed with brackets or whitespace ("[evt_2]").
    let handle: String = event_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    by_id.get(&handle)
}

/// A title + start date lookup used when no explicit handle resolves.
pub type EventFallback<'f, 'a> = &'f dyn Fn(&str, &str) -> Option<&'a CreatedCalendarEvent>;

/// Which stored event an update refers to: the explicit handle first, then
/// `fallback` (by default title + start date over the windowed handles).
pub fn resolve_event_reference<'a>(
    event_id: Option<&str>,
    title: &str,
    start_date: &str,
    by_id: &'a EventHandles,
    fallback: Option<EventFallback<'_, 'a>>,
) -> Option<&'a CreatedCalendarEvent> {
    if let Some(matched) = resolve_explicit_event_reference(event_id, by_id) {
        return Some(matched);
    }
    match fallback {
        Some(fallback) => fallback(title, start_date),
        None => find_event(title, start_date, by_id.values()),
    }
}

/// The comparable fields of an event (`hasEventChanged`'s second argument).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventFields<'a> {
    pub title: &'a str,
    pub start_date: &'a str,
    pub start_time: Option<&'a str>,
    pub end_date: Option<&'a str>,
    pub end_time: Option<&'a str>,
    /// `None` only for legacy records without `allDay`.
    pub all_day: Option<bool>,
    pub location: Option<&'a str>,
    pub time_zone: Option<&'a str>,
    pub description: Option<&'a str>,
    pub duration: Option<&'a str>,
    pub reminder_minutes: Option<f64>,
    pub recurrence: Option<&'a EventRecurrence>,
}

impl<'a> From<&'a ExtractedEvent> for EventFields<'a> {
    fn from(e: &'a ExtractedEvent) -> Self {
        Self {
            title: &e.title,
            start_date: &e.start_date,
            start_time: e.start_time.as_deref(),
            end_date: e.end_date.as_deref(),
            end_time: e.end_time.as_deref(),
            all_day: Some(e.all_day),
            location: e.location.as_deref(),
            time_zone: e.time_zone.as_deref(),
            description: e.description.as_deref(),
            duration: e.duration.as_deref(),
            reminder_minutes: e.reminder_minutes,
            recurrence: e.recurrence.as_ref(),
        }
    }
}

impl<'a> From<&'a CreatedCalendarEvent> for EventFields<'a> {
    fn from(e: &'a CreatedCalendarEvent) -> Self {
        Self {
            title: &e.title,
            start_date: &e.start_date,
            start_time: e.start_time.as_deref(),
            end_date: e.end_date.as_deref(),
            end_time: e.end_time.as_deref(),
            all_day: e.all_day,
            location: e.location.as_deref(),
            time_zone: e.time_zone.as_deref(),
            description: e.description.as_deref(),
            duration: e.duration.as_deref(),
            reminder_minutes: e.reminder_minutes,
            recurrence: e.recurrence.as_ref(),
        }
    }
}

fn recurrence_key(recurrence: Option<&EventRecurrence>) -> String {
    recurrence.map_or_else(String::new, |r| {
        format!("{}|{}", r.frequency.as_str(), r.until)
    })
}

/// Whether an event differs meaningfully from the stored record (cosmetic title
/// drift is ignored).
pub fn has_event_changed(record: &CreatedCalendarEvent, event: &EventFields<'_>) -> bool {
    normalize_title(&record.title) != normalize_title(event.title)
        || record.start_date != event.start_date
        || record.start_time.as_deref() != event.start_time
        || record.end_date.as_deref() != event.end_date
        || record.end_time.as_deref() != event.end_time
        || record.all_day != event.all_day
        || record.location.as_deref() != event.location
        || record.time_zone.as_deref() != event.time_zone
        || record.description.as_deref() != event.description
        || record.duration.as_deref() != event.duration
        || record.reminder_minutes != event.reminder_minutes
        || recurrence_key(record.recurrence.as_ref()) != recurrence_key(event.recurrence)
}

fn pk(event_hash: &str) -> Result<String, omni_store::StoreError> {
    entity::pk::<CreatedCalendarEvent>(&event_hash.to_owned())
}

fn raw_meta(now: i64) -> DocMeta {
    DocMeta {
        entity: Some(CreatedCalendarEvent::NAME.to_owned()),
        version: CreatedCalendarEvent::VERSION,
        expires_at: None,
        updated_at: Some(now),
    }
}

/// Every tracked event (corrupt rows skipped).
pub async fn get_tracked_events(
    store: &Store,
) -> Result<Vec<CreatedCalendarEvent>, CalendarPersistenceError> {
    store
        .read(|docs| docs.get_all::<CreatedCalendarEvent>())
        .await
        .map_err(CalendarPersistenceError::wrap(
            "list tracked calendar events",
        ))
}

pub async fn get_tracked_event(
    store: &Store,
    event_hash: &str,
) -> Result<Option<CreatedCalendarEvent>, CalendarPersistenceError> {
    let key = event_hash.to_owned();
    store
        .read(move |docs| docs.get::<CreatedCalendarEvent>(&key))
        .await
        .map_err(CalendarPersistenceError::wrap(
            "read tracked calendar event",
        ))
}

/// Whether an active (not cancelled) record exists for the hash.
pub async fn has_created_event(
    store: &Store,
    event_hash: &str,
) -> Result<bool, CalendarPersistenceError> {
    let key = event_hash.to_owned();
    store
        .read(move |docs| {
            Ok(docs
                .get::<CreatedCalendarEvent>(&key)?
                .is_some_and(|record| !record.is_cancelled()))
        })
        .await
        .map_err(CalendarPersistenceError::wrap(
            "check created calendar event",
        ))
}

/// Why a create of this exact event (its hash) must not write anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateBlock {
    /// An active record exists.
    Duplicate,
    /// The event was deleted from the calendar; no email recreates it.
    Deleted,
    /// The record was cancelled or moved away by a later email, and this is
    /// an email that wrote it being processed again.
    Superseded,
}

/// The tracked row's verdict on creating its event from `email_id`. Only the
/// exact event is blocked: another date or time is a different hash, and
/// another email may book a cancelled slot again.
pub fn create_block(record: Option<&CreatedCalendarEvent>, email_id: &str) -> Option<CreateBlock> {
    let record = record?;
    if !record.is_cancelled() {
        return Some(CreateBlock::Duplicate);
    }
    if record.is_deleted() {
        return Some(CreateBlock::Deleted);
    }
    record
        .written_by(email_id)
        .then_some(CreateBlock::Superseded)
}

pub async fn record_created_event(
    store: &Store,
    data: CreatedCalendarEvent,
) -> Result<(), CalendarPersistenceError> {
    store
        .write(move |tx| tx.upsert(&data, UpsertOpts::default()))
        .await
        .map_err(CalendarPersistenceError::wrap(
            "record created calendar event",
        ))
}

/// Writes the replacement row, then (in the same transaction) tombstones the
/// previous identity when the hash changed.
pub async fn replace_created_event(
    store: &Store,
    data: CreatedCalendarEvent,
    previous_event_hash: &str,
) -> Result<(), CalendarPersistenceError> {
    let previous = previous_event_hash.to_owned();
    store
        .write(move |tx| {
            tx.upsert(&data, UpsertOpts::default())?;
            if data.event_hash == previous {
                return Ok(());
            }
            set_cancelled_raw(tx, &previous)
        })
        .await
        .map_err(CalendarPersistenceError::wrap(
            "replace tracked calendar event",
        ))
}

/// `{...stored, status: "cancelled"}` on the raw value, keeping every field.
fn set_cancelled_raw(tx: &mut Tx<'_>, event_hash: &str) -> Result<(), omni_store::StoreError> {
    let pk = pk(event_hash)?;
    let Some(row) = tx.get_raw_row(&pk)? else {
        return Ok(());
    };
    let mut value = row.decode()?;
    let Some(object) = value.as_object_mut() else {
        return Err(omni_store::StoreError::CorruptRow {
            pk,
            reason: "payload is not an object".to_owned(),
        });
    };
    object.insert("status".to_owned(), JsValue::String("cancelled".to_owned()));
    let now = tx.now_ms();
    tx.upsert_doc(&pk, &value, raw_meta(now))
}

/// Tombstones, as deleted from the calendar, the active tracked row of each
/// given calendar event (`ids` hold UIDs or resource names without `.ics`).
/// Rows an earlier reschedule or cancellation already retired describe other
/// times and stay as they are. Runs inside the caller's transaction; returns
/// how many rows changed.
pub fn mark_calendar_events_deleted_in(
    tx: &mut Tx<'_>,
    ids: &BTreeSet<String>,
) -> Result<u64, omni_store::StoreError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let now = tx.now_ms();
    let mut changed = 0u64;
    for stored in tx.get_raw_rows_by_prefix(&format!("${}#", CreatedCalendarEvent::NAME))? {
        if stored.expires_at.is_some_and(|at| at <= now) {
            continue;
        }
        let Ok(mut value) = stored.decode() else {
            continue;
        };
        let Some(object) = value.as_object_mut() else {
            continue;
        };
        let tracked = object
            .get("calendarEventId")
            .and_then(JsValue::as_str)
            .is_some_and(|id| ids.contains(id));
        let retired = object.get("status").and_then(JsValue::as_str) == Some("cancelled");
        if !tracked || retired {
            continue;
        }
        object.insert("status".to_owned(), JsValue::String("cancelled".to_owned()));
        object.insert("deletedAt".to_owned(), JsValue::Int(i128::from(now)));
        tx.upsert_doc(&stored.pk, &value, raw_meta(now))?;
        changed += 1;
    }
    Ok(changed)
}

/// [`mark_calendar_events_deleted_in`] for one calendar event, in its own
/// transaction.
pub async fn mark_calendar_event_deleted(
    store: &Store,
    calendar_event_id: &str,
) -> Result<u64, CalendarPersistenceError> {
    let ids = BTreeSet::from([calendar_event_id.to_owned()]);
    store
        .write(move |tx| mark_calendar_events_deleted_in(tx, &ids))
        .await
        .map_err(CalendarPersistenceError::wrap(
            "mark calendar event deleted",
        ))
}

/// Marks an event cancelled, keeping the record to prevent re-creation.
pub async fn mark_event_cancelled(
    store: &Store,
    event_hash: &str,
) -> Result<(), CalendarPersistenceError> {
    let key = event_hash.to_owned();
    store
        .write(move |tx| set_cancelled_raw(tx, &key))
        .await
        .map_err(CalendarPersistenceError::wrap(
            "mark calendar event cancelled",
        ))
}

/// Active events for the extraction prompt: 7 days back through a year ahead
/// (a 90-day horizon once hid a September event announced in April).
pub async fn get_recent_events(
    store: &Store,
    now_ms: i64,
    tz: &TimeZone,
) -> Result<Vec<CreatedCalendarEvent>, CalendarPersistenceError> {
    Ok(select_recent_events(
        get_tracked_events(store).await?,
        365,
        now_ms,
        tz,
    ))
}

/// Re-keys every stored event whose persisted `eventHash` predates the current
/// normalization, in one transaction; returns how many rows moved. Idempotent.
pub async fn reconcile_event_hashes(store: &Store) -> Result<u64, CalendarPersistenceError> {
    store
        .write(|tx| {
            let now = tx.now_ms();
            let mut rekeyed = 0u64;
            for stored in tx.get_raw_rows_by_prefix(&format!("${}#", CreatedCalendarEvent::NAME))? {
                if stored.expires_at.is_some_and(|at| at <= now) {
                    continue;
                }
                let Ok(mut value) = stored.decode() else {
                    continue;
                };
                let Some(object) = value.as_object_mut() else {
                    continue;
                };
                let (Some(title), Some(start_date)) = (
                    object.get("title").and_then(JsValue::as_str),
                    object.get("startDate").and_then(JsValue::as_str),
                ) else {
                    tracing::warn!(target: LOG, "Skipping hash reconcile of {}: no title/startDate", stored.pk);
                    continue;
                };
                let start_time = object.get("startTime").and_then(JsValue::as_str);
                let expected = compute_event_hash(title, start_date, start_time);
                let current = object.get("eventHash").and_then(JsValue::as_str);
                if current == Some(expected.as_str()) {
                    continue;
                }
                let old_pk = match current {
                    Some(hash) => pk(hash)?,
                    None => stored.pk.clone(),
                };
                object.insert("eventHash".to_owned(), JsValue::String(expected.clone()));
                tx.upsert_doc(&pk(&expected)?, &value, raw_meta(now))?;
                tx.delete_doc(&old_pk)?;
                rekeyed += 1;
            }
            Ok(rekeyed)
        })
        .await
        .map_err(CalendarPersistenceError::wrap("reconcile calendar event hashes"))
}

/// Decodes a raw row into the typed entity (compat audit helper).
pub fn decode_row(value: JsValue) -> Result<CreatedCalendarEvent, cbor::DecodeError> {
    cbor::from_value(value)
}
