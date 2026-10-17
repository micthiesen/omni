//! iCalendar bodies, byte-for-byte: CRLF
//! line endings, no line folding, and the escape set `\`, `;`, `,`, LF.

use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use omni_core::js::number_to_string;

use crate::extraction::schema::ExtractedEvent;

/// Builds the iCalendar body for an event. `default_tz` is `config.TZ`, used when
/// the event carries no time zone.
pub fn build_icalendar(
    event: &ExtractedEvent,
    uid: &str,
    generated_at_ms: i64,
    default_tz: &str,
) -> String {
    let tz = event.time_zone.as_deref().unwrap_or(default_tz);
    let mut lines: Vec<String> = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".to_owned(),
        "PRODID:-//omni//EN".to_owned(),
        "CALSCALE:GREGORIAN".to_owned(),
        "BEGIN:VEVENT".to_owned(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{}", format_utc(generated_at_ms)),
        format!("SUMMARY:{}", escape_ical(&event.title)),
    ];

    if event.all_day {
        lines.push(format!(
            "DTSTART;VALUE=DATE:{}",
            event.start_date.replace('-', "")
        ));
        // iCal all-day DTEND is exclusive, so single-day = start + 1 day.
        let end_date = event.end_date.as_deref().unwrap_or(&event.start_date);
        lines.push(format!(
            "DTEND;VALUE=DATE:{}",
            next_day(end_date).replace('-', "")
        ));
    } else {
        let start_time = event.start_time.as_deref().unwrap_or("00:00");
        let dtstart = format!(
            "{}T{}00",
            event.start_date.replace('-', ""),
            replace_first_colon(start_time)
        );
        lines.push(format!("DTSTART;TZID={tz}:{dtstart}"));
        let end_date = event.end_date.as_deref().unwrap_or(&event.start_date);
        if let Some(end_time) = event.end_time.as_deref().filter(|t| !t.is_empty()) {
            let dtend = format!(
                "{}T{}00",
                end_date.replace('-', ""),
                replace_first_colon(end_time)
            );
            lines.push(format!("DTEND;TZID={tz}:{dtend}"));
        } else if let Some(duration) = event.duration.as_deref().filter(|d| !d.is_empty()) {
            lines.push(format!("DURATION:{duration}"));
        } else {
            lines.push("DURATION:PT1H".to_owned());
        }
    }

    if let Some(recurrence) = &event.recurrence {
        let freq = recurrence.frequency.as_str().to_uppercase();
        let until = if event.all_day {
            recurrence.until.replace('-', "")
        } else {
            format_utc_until(
                &recurrence.until,
                event.start_time.as_deref().unwrap_or("00:00"),
                tz,
            )
        };
        lines.push(format!("RRULE:FREQ={freq};UNTIL={until}"));
    }

    if let Some(location) = event.location.as_deref().filter(|l| !l.is_empty()) {
        lines.push(format!("LOCATION:{}", escape_ical(location)));
    }
    if let Some(description) = event.description.as_deref().filter(|d| !d.is_empty()) {
        lines.push(format!("DESCRIPTION:{}", escape_ical(description)));
    }

    // Reminder alarm (LLM-chosen or default 30 min).
    if !event.all_day {
        let mins = event.reminder_minutes.unwrap_or(30.0);
        let trigger = if mins >= 60.0 {
            let rest = mins % 60.0;
            let minutes = if rest != 0.0 && !rest.is_nan() {
                format!("{}M", number_to_string(rest))
            } else {
                String::new()
            };
            format!("PT{}H{minutes}", number_to_string((mins / 60.0).floor()))
        } else {
            format!("PT{}M", number_to_string(mins))
        };
        lines.push("BEGIN:VALARM".to_owned());
        lines.push(format!("TRIGGER:-{trigger}"));
        lines.push("ACTION:DISPLAY".to_owned());
        lines.push("END:VALARM".to_owned());
    }

    lines.push("END:VEVENT".to_owned());
    lines.push("END:VCALENDAR".to_owned());
    lines.join("\r\n")
}

/// JS `String#replace(":", "")`: only the first occurrence.
fn replace_first_colon(value: &str) -> String {
    value.replacen(':', "", 1)
}

fn format_utc_parts(ts: Timestamp) -> String {
    ts.strftime("%Y%m%dT%H%M%SZ").to_string()
}

/// `YYYYMMDDTHHMMSSZ` of an epoch.
fn format_utc(ms: i64) -> String {
    match Timestamp::from_millisecond(ms) {
        Ok(ts) => format_utc_parts(ts),
        Err(_) => "NaNNaNNaNTNaNNaNNaNZ".to_owned(),
    }
}

/// RRULE UNTIL for a timed event: the event's wall-clock start time on the until
/// date, converted to UTC. The zone offset is
/// taken at the instant obtained by reading the wall-clock time as UTC, and an
/// unresolvable zone treats the wall-clock time as UTC.
fn format_utc_until(until_date: &str, start_time: &str, time_zone: &str) -> String {
    let Some(naive) = parse_naive_utc(until_date, start_time) else {
        return "NaNNaNNaNTNaNNaNNaNZ".to_owned();
    };
    let instant = match TimeZone::get(time_zone) {
        Ok(tz) => {
            let offset = tz.to_offset(naive);
            naive
                .checked_sub(jiff::SignedDuration::from_secs(i64::from(offset.seconds())))
                .unwrap_or(naive)
        }
        Err(_) => naive,
    };
    format_utc_parts(instant)
}

/// `new Date(`${date}T${time}:00Z`)` with V8's leniency (see [`lenient_date`];
/// `24:00` is the next midnight).
fn parse_naive_utc(date: &str, time: &str) -> Option<Timestamp> {
    let date = lenient_date(date)?;
    let bytes = time.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let hour: i64 = time.get(0..2)?.parse().ok()?;
    let minute: i64 = time.get(3..5)?.parse().ok()?;
    if !time.get(0..2)?.bytes().all(|b| b.is_ascii_digit())
        || !time.get(3..5)?.bytes().all(|b| b.is_ascii_digit())
        || minute > 59
        || hour > 24
        || (hour == 24 && minute != 0)
    {
        return None;
    }
    let midnight = DateTime::from_parts(date, jiff::civil::Time::midnight())
        .to_zoned(TimeZone::UTC)
        .ok()?
        .timestamp();
    midnight
        .checked_add(jiff::SignedDuration::from_secs(hour * 3600 + minute * 60))
        .ok()
}

/// A `YYYY-MM-DD` date as V8 reads it: day 01-31 is accepted for every month
/// and rolls over past the month's end; anything else is invalid.
fn lenient_date(date: &str) -> Option<Date> {
    let bytes = date.as_bytes();
    let well_formed = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
    if !well_formed {
        return None;
    }
    let year: i16 = date.get(0..4)?.parse().ok()?;
    let month: i8 = date.get(5..7)?.parse().ok()?;
    let day: i64 = date.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Date::new(year, month, 1)
        .ok()?
        .checked_add(jiff::Span::new().days(day - 1))
        .ok()
}

/// Adds one day to an ISO date (`"2026-03-20"` → `"2026-03-21"`) with V8's
/// leniency; a malformed value gives JS's `"NaN-NaN-NaN"`.
fn next_day(date: &str) -> String {
    lenient_date(date)
        .and_then(|d| d.tomorrow().ok())
        .map_or_else(
            || "NaN-NaN-NaN".to_owned(),
            |next| next.strftime("%Y-%m-%d").to_string(),
        )
}

/// Escapes iCalendar text values.
fn escape_ical(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_day_matches_v8_leniency() {
        assert_eq!(next_day("2026-03-08"), "2026-03-09");
        assert_eq!(next_day("2026-12-31"), "2027-01-01");
        assert_eq!(next_day("2026-02-31"), "2026-03-04");
        assert_eq!(next_day("2026-02-32"), "NaN-NaN-NaN");
        assert_eq!(next_day("2026-13-01"), "NaN-NaN-NaN");
        assert_eq!(next_day("2026-1-5"), "NaN-NaN-NaN");
        assert_eq!(next_day("2026-02-00"), "NaN-NaN-NaN");
    }

    #[test]
    fn until_parsing_matches_v8() {
        assert_eq!(
            format_utc_until("2026-02-30", "09:00", "UTC"),
            "20260302T090000Z"
        );
        assert_eq!(
            format_utc_until("2026-07-13", "24:00", "UTC"),
            "20260714T000000Z"
        );
        assert_eq!(
            format_utc_until("2026-07-13", "9:00", "UTC"),
            "NaNNaNNaNTNaNNaNNaNZ"
        );
        assert_eq!(
            format_utc_until("2026-07-13", "09:00", "Not/AZone"),
            "20260713T090000Z"
        );
    }

    #[test]
    fn escapes_backslash_semicolon_comma_and_newline() {
        assert_eq!(escape_ical("a\\b;c,d\ne\r"), "a\\\\b\\;c\\,d\\ne\r");
    }

    #[test]
    fn reminder_triggers_follow_js_number_formatting() {
        let mut event = ExtractedEvent::new(
            crate::extraction::schema::EventAction::Create,
            "X",
            "2026-07-06",
            false,
        );
        event.start_time = Some("09:00".to_owned());
        for (mins, expected) in [
            (None, "TRIGGER:-PT30M"),
            (Some(60.0), "TRIGGER:-PT1H"),
            (Some(90.0), "TRIGGER:-PT1H30M"),
            (Some(1440.0), "TRIGGER:-PT24H"),
            (Some(0.0), "TRIGGER:-PT0M"),
            (Some(90.5), "TRIGGER:-PT1H30.5M"),
        ] {
            event.reminder_minutes = mins;
            let ics = build_icalendar(&event, "u", 0, "America/Vancouver");
            assert!(ics.split("\r\n").any(|l| l == expected), "{mins:?}: {ics}");
        }
    }

    #[test]
    fn timed_event_defaults_to_one_hour() {
        let mut event = ExtractedEvent::new(
            crate::extraction::schema::EventAction::Create,
            "Dentist, cleaning; x",
            "2026-09-01",
            false,
        );
        event.start_time = Some("09:00".to_owned());
        let ics = build_icalendar(&event, "stable@omni-notify", 0, "America/Vancouver");
        assert_eq!(
            ics,
            [
                "BEGIN:VCALENDAR",
                "VERSION:2.0",
                "PRODID:-//omni//EN",
                "CALSCALE:GREGORIAN",
                "BEGIN:VEVENT",
                "UID:stable@omni-notify",
                "DTSTAMP:19700101T000000Z",
                "SUMMARY:Dentist\\, cleaning\\; x",
                "DTSTART;TZID=America/Vancouver:20260901T090000",
                "DURATION:PT1H",
                "BEGIN:VALARM",
                "TRIGGER:-PT30M",
                "ACTION:DISPLAY",
                "END:VALARM",
                "END:VEVENT",
                "END:VCALENDAR",
            ]
            .join("\r\n")
        );
    }
}
