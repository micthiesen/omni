//! iCalendar bodies with recurrence. Every caller passes an explicit
//! deterministic UID (pipeline `omni-<sha256>`, MCP `mcp-<sha256>`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_calendar::caldav::ics::build_icalendar;
use omni_calendar::extraction::schema::{
    EventAction, EventRecurrence, ExtractedEvent, RecurrenceFrequency,
};

/// `Date.UTC(2026, 8, 1, 12, 34, 56)`.
const GENERATED_AT: i64 = 1_788_266_096_000;

fn evt() -> ExtractedEvent {
    let mut e = ExtractedEvent::new(
        EventAction::Create,
        "🚧 Elevator Maintenance",
        "2026-07-06",
        false,
    );
    e.start_time = Some("09:00".to_owned());
    e.end_time = Some("16:00".to_owned());
    e.time_zone = Some("America/Vancouver".to_owned());
    e
}

fn ics_lines(event: &ExtractedEvent) -> Vec<String> {
    build_icalendar(
        event,
        "uid-1@omni-notify",
        GENERATED_AT,
        "America/Vancouver",
    )
    .split("\r\n")
    .map(str::to_owned)
    .collect()
}

fn recurrence(frequency: RecurrenceFrequency, until: &str) -> Option<EventRecurrence> {
    Some(EventRecurrence {
        frequency,
        until: until.to_owned(),
    })
}

#[test]
fn renders_the_supplied_generation_time_and_deterministic_uid_inputs() {
    let lines = ics_lines(&evt());
    assert!(lines.contains(&"DTSTAMP:20260901T123456Z".to_owned()));
    assert!(lines.contains(&"UID:uid-1@omni-notify".to_owned()));
}

#[test]
fn emits_rrule_with_a_utc_until_covering_the_last_occurrence_for_timed_events() {
    let mut e = evt();
    e.recurrence = recurrence(RecurrenceFrequency::Daily, "2026-07-13");
    let lines = ics_lines(&e);
    // 09:00 America/Vancouver on Jul 13 is PDT (UTC-7) → 16:00Z.
    assert!(lines.contains(&"RRULE:FREQ=DAILY;UNTIL=20260713T160000Z".to_owned()));
    assert!(lines.contains(&"DTSTART;TZID=America/Vancouver:20260706T090000".to_owned()));
    assert!(lines.contains(&"DTEND;TZID=America/Vancouver:20260706T160000".to_owned()));
}

#[test]
fn converts_until_correctly_for_zones_ahead_of_utc() {
    let mut e = evt();
    e.start_date = "2026-12-01".to_owned();
    e.time_zone = Some("Asia/Tokyo".to_owned());
    e.recurrence = recurrence(RecurrenceFrequency::Weekly, "2026-12-15");
    // 09:00 Asia/Tokyo (UTC+9, no DST) → 00:00Z the same day.
    assert!(ics_lines(&e).contains(&"RRULE:FREQ=WEEKLY;UNTIL=20261215T000000Z".to_owned()));
}

#[test]
fn emits_a_date_format_until_for_all_day_recurring_events() {
    let mut e = evt();
    e.start_time = None;
    e.end_time = None;
    e.all_day = true;
    e.recurrence = recurrence(RecurrenceFrequency::Monthly, "2026-10-01");
    let lines = ics_lines(&e);
    assert!(lines.contains(&"RRULE:FREQ=MONTHLY;UNTIL=20261001".to_owned()));
    assert!(lines.contains(&"DTSTART;VALUE=DATE:20260706".to_owned()));
    assert!(lines.contains(&"DTEND;VALUE=DATE:20260707".to_owned()));
    assert!(!lines.iter().any(|l| l == "BEGIN:VALARM"));
}

#[test]
fn emits_no_rrule_when_recurrence_is_absent_or_null() {
    // `null` and absent are both `None` after decoding.
    let lines = ics_lines(&evt());
    assert!(!lines.iter().any(|l| l.starts_with("RRULE")));
}

#[test]
fn full_body_is_byte_exact() {
    let mut e = evt();
    e.location = Some("Building A, Lobby; East".to_owned());
    e.description = Some("Line 1\nLine 2 \\ end".to_owned());
    e.reminder_minutes = Some(720.0);
    let ics = build_icalendar(&e, "uid-1@omni-notify", GENERATED_AT, "America/Vancouver");
    assert_eq!(
        ics,
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//omni//EN\r\nCALSCALE:GREGORIAN\r\n\
BEGIN:VEVENT\r\nUID:uid-1@omni-notify\r\nDTSTAMP:20260901T123456Z\r\n\
SUMMARY:🚧 Elevator Maintenance\r\nDTSTART;TZID=America/Vancouver:20260706T090000\r\n\
DTEND;TZID=America/Vancouver:20260706T160000\r\nLOCATION:Building A\\, Lobby\\; East\r\n\
DESCRIPTION:Line 1\\nLine 2 \\\\ end\r\nBEGIN:VALARM\r\nTRIGGER:-PT12H\r\nACTION:DISPLAY\r\n\
END:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR"
    );
}
