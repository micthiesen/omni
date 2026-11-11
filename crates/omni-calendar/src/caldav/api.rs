//! CalDAV event writes.
//!
//! Transport failures are `Err(CaldavError)`; HTTP-level failures come back as
//! `Error { code, message }` outcomes so callers can classify 5xx as retryable.
//! In [`SideEffectMode::Record`] writes are captured instead of sent.

use std::sync::{Arc, Mutex};

use omni_core::clock::SharedClock;
use omni_http::{HttpClient, Method, SideEffectMode, Url};

use super::http::{
    self, CALDAV_ERROR_MAX_BYTES, CALDAV_XML_MAX_BYTES, CaldavResponse, assert_trusted_caldav_url,
};
use super::ics::build_icalendar;
use super::merge;
use super::xml::{extract_calendar_collections, extract_response_hrefs_containing};
use crate::error::CaldavError;
use crate::extraction::schema::ExtractedEvent;

const LOG: &str = "CalDAV";

/// Merge attempts for an update that keeps hitting 412.
pub const MERGE_ATTEMPTS: usize = 3;
/// Cap for reading one calendar resource.
const CALDAV_RESOURCE_MAX_BYTES: usize = 256 * 1024;

/// Resolved CalDAV target: a calendar collection URL plus the auth to use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaldavSession {
    /// Absolute collection URL, ending with `/`.
    pub calendar_url: String,
    pub auth_header: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateOutcome {
    Success {
        event_uid: String,
    },
    /// 412: a resource with this UID already exists (a replayed create).
    AlreadyExists {
        event_uid: String,
    },
    Error {
        code: u16,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    Success {
        event_uid: String,
    },
    /// The event exists in none of the account's calendars: it was deleted.
    /// Nothing was written.
    Missing,
    Error {
        code: u16,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    Success,
    NotFound,
    Error { code: u16, message: String },
}

/// A write captured in [`SideEffectMode::Record`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedCaldavWrite {
    pub method: &'static str,
    pub url: String,
    pub body: Option<String>,
}

/// Issues CalDAV writes for a session.
#[derive(Clone)]
pub struct CaldavWriter {
    http: HttpClient,
    clock: SharedClock,
    mode: SideEffectMode,
    default_tz: String,
    recorded: Arc<Mutex<Vec<RecordedCaldavWrite>>>,
}

/// Where a missing event's UID was found.
enum MovedCopy {
    Found(Url),
    Absent,
    /// The lookup did not complete, so the event is neither known deleted
    /// nor found.
    Unknown {
        code: u16,
        message: String,
    },
}

/// Other calendars searched for a moved event, and matches read per calendar.
const MOVED_COPY_COLLECTIONS: usize = 25;
const MOVED_COPY_CANDIDATES: usize = 5;

const LIST_COLLECTIONS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <c:supported-calendar-component-set/>
  </d:prop>
</d:propfind>"#;

/// A `calendar-query` for the events around `start_date` (a day before
/// through two days after, so any time zone is covered), with their data.
/// iCloud refuses a UID `prop-filter` with 412, so the UID is matched in the
/// returned data instead. `None` when the date does not parse.
fn nearby_events_query(start_date: &str) -> Option<String> {
    let date = start_date.parse::<jiff::civil::Date>().ok()?;
    let from = date.yesterday().ok()?.strftime("%Y%m%d");
    let to = date
        .checked_add(jiff::Span::new().days(2))
        .ok()?
        .strftime("%Y%m%d");
    Some(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">
        <c:time-range start="{from}T000000Z" end="{to}T000000Z"/>
      </c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#
    ))
}

/// Whether an iCalendar body has a `UID` line with exactly this value.
fn carries_uid(ics: &str, uid: &str) -> bool {
    let unfolded = ics
        .replace("\r\n ", "")
        .replace("\r\n\t", "")
        .replace("\n ", "")
        .replace("\n\t", "");
    unfolded
        .lines()
        .any(|line| line.strip_prefix("UID:").map(str::trim_end) == Some(uid))
}

fn status_message(response: &CaldavResponse) -> String {
    format!("CalDAV {}: {}", response.status, response.reason)
}

fn event_url(session: &CaldavSession, uid: &str) -> Result<Url, CaldavError> {
    let raw = format!("{}{uid}.ics", session.calendar_url);
    Url::parse(&raw)
        .map_err(|e| CaldavError::new("build CalDAV event URL", format!("{raw}: {e}"), false))
}

impl CaldavWriter {
    pub fn new(
        http: HttpClient,
        clock: SharedClock,
        mode: SideEffectMode,
        default_tz: impl Into<String>,
    ) -> Self {
        Self {
            http,
            clock,
            mode,
            default_tz: default_tz.into(),
            recorded: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Writes captured in record mode, oldest first.
    pub fn recorded(&self) -> Vec<RecordedCaldavWrite> {
        self.recorded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The shared record-mode capture list (the primary calendar tools append
    /// to the same list).
    pub fn recorded_handle(&self) -> Arc<Mutex<Vec<RecordedCaldavWrite>>> {
        self.recorded.clone()
    }

    pub fn mode(&self) -> SideEffectMode {
        self.mode
    }

    fn record(&self, method: &'static str, url: &Url, body: Option<String>) {
        self.recorded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(RecordedCaldavWrite {
                method,
                url: url.to_string(),
                body,
            });
    }

    /// The iCalendar body this writer would send now.
    pub fn render(&self, event: &ExtractedEvent, uid: &str) -> String {
        build_icalendar(event, uid, self.clock.now_ms(), &self.default_tz)
    }

    /// PUTs a new event at `<collection><uid>.ics` with `If-None-Match: *`, so a
    /// replay after a lost acknowledgement reports `AlreadyExists` instead of
    /// creating a duplicate.
    pub async fn create(
        &self,
        session: &CaldavSession,
        event: &ExtractedEvent,
        uid: &str,
    ) -> Result<CreateOutcome, CaldavError> {
        let ics = self.render(event, uid);
        let url = event_url(session, uid)?;
        tracing::debug!(target: LOG, "CalDAV PUT {url}\n{ics}");
        let response = self
            .put(
                session,
                &url,
                ics.clone(),
                Some("*"),
                None,
                "create calendar event",
            )
            .await?;
        if matches!(response.status, 201 | 204) {
            tracing::info!(target: LOG, "Created calendar event: {} ({uid})", event.title);
            return Ok(CreateOutcome::Success {
                event_uid: uid.to_owned(),
            });
        }
        if response.status == 412 {
            tracing::info!(target: LOG, "Calendar event already exists: {uid}");
            return Ok(CreateOutcome::AlreadyExists {
                event_uid: uid.to_owned(),
            });
        }
        let text = http::error_text(&response, "create calendar event")?;
        tracing::error!(
            target: LOG,
            "CalDAV PUT failed: {} {} URL: {url}\nBody:\n{ics}\nResponse:\n{text}",
            response.status,
            response.reason
        );
        Ok(CreateOutcome::Error {
            code: response.status,
            message: status_message(&response),
        })
    }

    /// Updates an existing event without clobbering other edits: reads the
    /// current copy, applies only the field groups that differ between
    /// `base` (what the pipeline last wrote) and `event`, and writes with
    /// `If-Match`. A 412 re-reads and merges again, at most
    /// [`MERGE_ATTEMPTS`] times. A missing resource is never recreated: when
    /// the UID lives in another of the account's calendars (a cross-calendar
    /// move) that copy is merged in place, otherwise the event was deleted
    /// and the result is [`UpdateOutcome::Missing`].
    pub async fn update(
        &self,
        session: &CaldavSession,
        base: &ExtractedEvent,
        event: &ExtractedEvent,
        existing_uid: &str,
    ) -> Result<UpdateOutcome, CaldavError> {
        let now_ms = self.clock.now_ms();
        let base_ics = build_icalendar(base, existing_uid, now_ms, &self.default_tz);
        let new_ics = build_icalendar(event, existing_uid, now_ms, &self.default_tz);
        let home = event_url(session, existing_uid)?;
        let mut target = home.clone();
        let success = || {
            Ok(UpdateOutcome::Success {
                event_uid: existing_uid.to_owned(),
            })
        };
        for _ in 0..MERGE_ATTEMPTS {
            let current = self.get(session, &target).await?;
            if current.status == 404 {
                if target != home {
                    // The moved copy vanished too: start over at home.
                    target = home.clone();
                    continue;
                }
                // Gone from its calendar: deleted, or moved to another one. A
                // deleted event is never written back, so the copy is looked
                // up by reading instead of probing with a creating PUT.
                match self
                    .find_moved_copy(session, existing_uid, &base.start_date)
                    .await?
                {
                    MovedCopy::Found(copy) => {
                        tracing::warn!(
                            target: LOG,
                            "Calendar event {existing_uid} moved to another calendar; updating it there"
                        );
                        target = copy;
                        continue;
                    }
                    MovedCopy::Absent => {
                        tracing::info!(
                            target: LOG,
                            "Calendar event {existing_uid} was deleted; not recreating it"
                        );
                        return Ok(UpdateOutcome::Missing);
                    }
                    MovedCopy::Unknown { code, message } => {
                        return Ok(UpdateOutcome::Error { code, message });
                    }
                }
            }
            if !current.is_ok() {
                return Ok(UpdateOutcome::Error {
                    code: current.status,
                    message: status_message(&current),
                });
            }
            let Some(etag) = current.etag.clone() else {
                return Ok(UpdateOutcome::Error {
                    code: current.status,
                    message: "CalDAV GET returned no ETag".to_owned(),
                });
            };
            let now =
                jiff::Timestamp::from_millisecond(now_ms).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
            let merged = match merge::merge(&current.text(), &base_ics, &new_ics, now) {
                Ok(Some(merged)) => merged,
                Ok(None) => {
                    tracing::info!(target: LOG, "Calendar event already current: {existing_uid}");
                    return success();
                }
                Err(error) => {
                    return Ok(UpdateOutcome::Error {
                        code: 422,
                        message: format!("cannot merge the update: {error}"),
                    });
                }
            };
            tracing::debug!(target: LOG, "CalDAV PUT (merge) {target}\n{merged}");
            let response = self
                .put(
                    session,
                    &target,
                    merged,
                    None,
                    Some(&etag),
                    "update calendar event",
                )
                .await?;
            if matches!(response.status, 200 | 201 | 204) {
                tracing::info!(target: LOG, "Updated calendar event: {} ({existing_uid})", event.title);
                return success();
            }
            if response.status == 412 {
                tracing::info!(target: LOG, "Calendar event {existing_uid} changed meanwhile; merging again");
                continue;
            }
            let text = http::error_text(&response, "update calendar event")?;
            tracing::error!(
                target: LOG,
                "CalDAV PUT (update) failed: {} {} URL: {target}\nResponse:\n{text}",
                response.status,
                response.reason
            );
            return Ok(UpdateOutcome::Error {
                code: response.status,
                message: status_message(&response),
            });
        }
        Ok(UpdateOutcome::Error {
            code: 412,
            message: format!(
                "CalDAV 412: the event kept changing during {MERGE_ATTEMPTS} merge attempts"
            ),
        })
    }

    /// Reads one resource (bounded).
    async fn get(&self, session: &CaldavSession, url: &Url) -> Result<CaldavResponse, CaldavError> {
        http::request(
            &self.http,
            Method::GET,
            url,
            &[("Authorization", session.auth_header.as_str())],
            None,
            "read calendar event",
            CALDAV_RESOURCE_MAX_BYTES,
        )
        .await
    }

    /// Looks for the event with this UID in the account's other event
    /// calendars (the collections beside the session's one), reading only:
    /// the events around `start_date` are listed and a candidate counts only
    /// when its own body carries the UID. A copy that was also moved to
    /// another date is not found and counts as deleted.
    async fn find_moved_copy(
        &self,
        session: &CaldavSession,
        uid: &str,
        start_date: &str,
    ) -> Result<MovedCopy, CaldavError> {
        let collection = assert_trusted_caldav_url(&session.calendar_url)?;
        let unlisted = || MovedCopy::Unknown {
            code: 404,
            message: "the write calendar is not in the account's calendar list, so a missing \
                      event is not treated as deleted"
                .to_owned(),
        };
        let Ok(home) = collection.join("../") else {
            return Ok(unlisted());
        };
        if home == collection {
            return Ok(unlisted());
        }
        let listing = http::propfind(
            &self.http,
            &home,
            &session.auth_header,
            "1",
            LIST_COLLECTIONS,
        )
        .await?;
        let is_write_calendar =
            |url: &Url| url.path().trim_end_matches('/') == collection.path().trim_end_matches('/');
        let calendars: Vec<Url> = extract_calendar_collections(&listing.xml)
            .iter()
            .filter(|c| {
                c.components
                    .as_ref()
                    .is_none_or(|set| set.iter().any(|name| name == "VEVENT"))
            })
            .filter_map(|c| listing.url.join(&c.href).ok())
            .filter_map(|url| assert_trusted_caldav_url(url.as_str()).ok())
            .collect();
        // A stale or removed write calendar answers 404 for every event; that
        // must never read as "all of them were deleted".
        if !calendars.iter().any(is_write_calendar) {
            return Ok(unlisted());
        }
        let others: Vec<Url> = calendars
            .into_iter()
            .filter(|url| !is_write_calendar(url))
            .take(MOVED_COPY_COLLECTIONS)
            .collect();
        let report = Method::from_bytes(b"REPORT")
            .map_err(|e| CaldavError::new("find moved calendar event", e.to_string(), false))?;
        if others.is_empty() {
            return Ok(MovedCopy::Absent);
        }
        let Some(query) = nearby_events_query(start_date) else {
            return Ok(MovedCopy::Unknown {
                code: 422,
                message: format!("could not look for the moved event (start date {start_date:?})"),
            });
        };
        let needle = format!("UID:{uid}");
        for other in others {
            let response = http::request(
                &self.http,
                report.clone(),
                &other,
                &[
                    ("Content-Type", "application/xml; charset=utf-8"),
                    ("Authorization", session.auth_header.as_str()),
                    ("Depth", "1"),
                ],
                Some(query.clone()),
                "find moved calendar event",
                CALDAV_XML_MAX_BYTES,
            )
            .await?;
            if (400..500).contains(&response.status) {
                // A calendar that cannot be queried (subscribed, delegated)
                // is not where the pipeline's event went.
                tracing::warn!(
                    target: LOG,
                    "Skipping a calendar that refused the moved-event lookup: {}",
                    status_message(&response)
                );
                continue;
            }
            if !response.is_ok() {
                return Ok(MovedCopy::Unknown {
                    code: response.status,
                    message: format!(
                        "could not look for the moved event ({})",
                        status_message(&response)
                    ),
                });
            }
            for href in extract_response_hrefs_containing(&response.text(), &needle)
                .into_iter()
                .filter(|href| href.ends_with(".ics"))
                .take(MOVED_COPY_CANDIDATES)
            {
                let Some(candidate) = other
                    .join(&href)
                    .ok()
                    .and_then(|url| assert_trusted_caldav_url(url.as_str()).ok())
                    .filter(|url| url.path().starts_with(other.path()))
                else {
                    continue;
                };
                let copy = self.get(session, &candidate).await?;
                if copy.is_ok() && carries_uid(&copy.text(), uid) {
                    return Ok(MovedCopy::Found(candidate));
                }
            }
        }
        Ok(MovedCopy::Absent)
    }

    /// Deletes `<collection><uid>.ics`; 404 is `NotFound`.
    pub async fn delete(
        &self,
        session: &CaldavSession,
        uid: &str,
    ) -> Result<DeleteOutcome, CaldavError> {
        let url = event_url(session, uid)?;
        self.delete_url(session, &url, "delete calendar event")
            .await
    }

    async fn delete_url(
        &self,
        session: &CaldavSession,
        url: &Url,
        operation: &str,
    ) -> Result<DeleteOutcome, CaldavError> {
        tracing::debug!(target: LOG, "CalDAV DELETE {url}");
        if self.mode == SideEffectMode::Record {
            self.record("DELETE", url, None);
            return Ok(DeleteOutcome::Success);
        }
        let response = http::request(
            &self.http,
            Method::DELETE,
            url,
            &[("Authorization", session.auth_header.as_str())],
            None,
            operation,
            CALDAV_ERROR_MAX_BYTES,
        )
        .await?;
        if matches!(response.status, 200 | 204) {
            tracing::info!(target: LOG, "Deleted calendar event: {url}");
            return Ok(DeleteOutcome::Success);
        }
        if response.status == 404 {
            tracing::info!(target: LOG, "Calendar event already gone: {url}");
            return Ok(DeleteOutcome::NotFound);
        }
        let text = http::error_text(&response, operation)?;
        tracing::error!(
            target: LOG,
            "CalDAV DELETE failed: {} {} URL: {url}\nResponse:\n{text}",
            response.status,
            response.reason
        );
        Ok(DeleteOutcome::Error {
            code: response.status,
            message: status_message(&response),
        })
    }

    async fn put(
        &self,
        session: &CaldavSession,
        url: &Url,
        ics: String,
        if_none_match: Option<&str>,
        if_match: Option<&str>,
        operation: &str,
    ) -> Result<CaldavResponse, CaldavError> {
        if self.mode == SideEffectMode::Record {
            self.record("PUT", url, Some(ics));
            return Ok(CaldavResponse {
                status: 201,
                reason: "Created".to_owned(),
                location: None,
                etag: None,
                body: Vec::new(),
            });
        }
        let mut headers: Vec<(&'static str, &str)> = vec![
            ("Content-Type", "text/calendar; charset=utf-8"),
            ("Authorization", session.auth_header.as_str()),
        ];
        if let Some(value) = if_none_match {
            headers.push(("If-None-Match", value));
        }
        if let Some(value) = if_match {
            headers.push(("If-Match", value));
        }
        http::request(
            &self.http,
            Method::PUT,
            url,
            &headers,
            Some(ics),
            operation,
            CALDAV_ERROR_MAX_BYTES,
        )
        .await
    }
}
