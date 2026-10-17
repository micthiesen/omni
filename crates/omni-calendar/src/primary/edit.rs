//! Pure planners: create, patch (series, one occurrence, this and following)
//! and delete, each returning the exact bodies to write. Untouched properties
//! keep their original bytes ([`super::ics`]).

use std::collections::{BTreeMap, BTreeSet};

use jiff::civil::{Date, DateTime, Time, Weekday};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use omni_core::digest::sha256_hex;

use super::PrimaryError;
use super::client::Precondition;
use super::expand;
use super::ics::{Component, IcsDoc, Item, Property};
use super::model::{self, EventView, IcalDuration, SchedulingRole};
use super::rrule::{Freq, RRule, Until, WeekdayNum};
use super::store::StepKind;
use super::time::{self, EventTime, Zones, compatible, format_utc, is_nonexistent};

const PRODID: &str = "-//omni//primary//EN";

/// A zone for a timed value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Zone {
    Utc,
    Iana(String),
}

impl Zone {
    pub fn parse(name: &str) -> Option<Zone> {
        if name == "UTC" {
            return Some(Zone::Utc);
        }
        TimeZone::get(name)
            .ok()
            .map(|_| Zone::Iana(name.to_owned()))
    }

    fn tz(&self) -> TimeZone {
        match self {
            Zone::Utc => TimeZone::UTC,
            Zone::Iana(name) => TimeZone::get(name).unwrap_or(TimeZone::UTC),
        }
    }

    fn time(&self, local: DateTime) -> Result<EventTime, PrimaryError> {
        match self {
            Zone::Utc => Ok(EventTime::Utc(
                local
                    .to_zoned(TimeZone::UTC)
                    .map_err(|e| PrimaryError::invalid(e.to_string()))?
                    .timestamp(),
            )),
            Zone::Iana(name) => {
                if is_nonexistent(&self.tz(), local) {
                    return Err(PrimaryError::coded(
                        "nonexistent_local_time",
                        format!("{local} does not exist in {name} (clocks skip it)"),
                    ));
                }
                Ok(EventTime::Zoned {
                    local,
                    tzid: name.clone(),
                })
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimedEnd {
    At(DateTime, Zone),
    Minutes(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Timing {
    /// Inclusive last day.
    AllDay { start: Date, last: Date },
    Timed {
        start: DateTime,
        zone: Zone,
        end: TimedEnd,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimingPatch {
    Full(Timing),
    /// Move to another day, keeping time of day and duration.
    MoveDate(Date),
    /// Move to another local time, keeping the duration.
    MoveTime {
        local: DateTime,
        zone: Option<Zone>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alarm {
    /// Minutes before the start (negative = after).
    BeforeStart(i64),
    At(Timestamp),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecurrenceEnd {
    Never,
    Count(u32),
    Until(Date),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recurrence {
    pub freq: Freq,
    pub interval: u32,
    pub by_day: Vec<WeekdayNum>,
    pub by_month_day: Vec<i8>,
    pub by_month: Vec<i8>,
    pub by_set_pos: Vec<i16>,
    pub week_start: Option<Weekday>,
    pub end: RecurrenceEnd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewEvent {
    pub timing: Timing,
    pub title: String,
    pub notes: Option<String>,
    pub location: Option<String>,
    pub url: Option<String>,
    pub free: bool,
    pub alarms: Vec<Alarm>,
    pub recurrence: Option<Recurrence>,
}

/// Absent keeps a field; `Some(None)` clears it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patch {
    pub title: Option<String>,
    pub notes: Option<Option<String>>,
    pub location: Option<Option<String>>,
    pub url: Option<Option<String>>,
    pub free: Option<bool>,
    pub timing: Option<TimingPatch>,
    pub alarms: Option<Vec<Alarm>>,
    pub recurrence: Option<Option<Recurrence>>,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        *self == Patch::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Series,
    Occurrence,
    Following,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStatus {
    Created,
    Updated,
    Deleted,
    Unchanged,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanStatus::Created => "created",
            PlanStatus::Updated => "updated",
            PlanStatus::Deleted => "deleted",
            PlanStatus::Unchanged => "unchanged",
        }
    }
}

/// One remote write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedStep {
    pub kind: StepKind,
    pub event_id: String,
    pub precondition: Precondition,
    pub body: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub status: PlanStatus,
    pub steps: Vec<PlannedStep>,
    pub warnings: Vec<String>,
    pub scheduling_notified: bool,
    /// The event the result reports (`None` after a whole-series delete).
    pub result_event_id: Option<String>,
}

/// What the planners need besides the input.
pub struct EditCtx {
    pub now: Timestamp,
    pub default_tz: TimeZone,
    pub owner: BTreeSet<String>,
    /// The idempotency key (derives new hrefs and UIDs).
    pub key: String,
}

/// The current state of an existing resource.
pub struct Current<'a> {
    pub event_id: &'a str,
    pub etag: &'a str,
    pub ics: &'a str,
}

/// Derived href and UID for a resource created by an operation.
pub fn derived_ids(prefix: &str, key: &str) -> (String, String) {
    let digest = sha256_hex(format!("{prefix}:{key}").as_bytes());
    let h = &digest[..32];
    (
        format!("omni-agent-{h}.ics"),
        format!("omni-agent-{h}@omni-notify"),
    )
}

fn stamp(now: Timestamp) -> String {
    format_utc(now)
}

fn touch(comp: &mut Component, now: Timestamp) {
    comp.set(Property::new("DTSTAMP", stamp(now)));
    comp.set(Property::new("LAST-MODIFIED", stamp(now)));
}

fn bump_sequence(comp: &mut Component) {
    let next = EventView { comp }.sequence() + 1;
    comp.set(Property::new("SEQUENCE", next.to_string()));
}

fn set_time(comp: &mut Component, name: &str, value: &EventTime) {
    comp.set(value.to_property(name));
}

fn ensure_vtimezones(doc: &mut IcsDoc, times: &[&EventTime], until: Option<Timestamp>) {
    for t in times {
        let EventTime::Zoned { tzid, local } = t else {
            continue;
        };
        if doc.timezone(tzid).is_some() {
            continue;
        }
        let Ok(tz) = TimeZone::get(tzid) else {
            continue;
        };
        let from = compatible(&tz, *local).unwrap_or(Timestamp::UNIX_EPOCH);
        let until = until.unwrap_or_else(|| {
            from.checked_add(SignedDuration::from_hours(24 * 366 * 10))
                .unwrap_or(from)
        });
        let vt = time::generate_vtimezone(tzid, &tz, from, until.max(from));
        let index = doc
            .root
            .items
            .iter()
            .position(|item| matches!(item, Item::Child(c) if c.name == "VEVENT"))
            .unwrap_or(doc.root.items.len());
        doc.root.items.insert(index, Item::Child(vt));
    }
}

fn alarm_component(alarm: &Alarm, seed: &str, index: usize) -> Component {
    let mut c = Component::new("VALARM");
    let id = sha256_hex(format!("{seed}:alarm:{index}").as_bytes());
    let uid = format!(
        "{}-{}-{}-{}-{}",
        &id[0..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..32]
    )
    .to_ascii_uppercase();
    c.insert_prop(Property::new("UID", uid.clone()));
    c.insert_prop(Property::new("X-WR-ALARMUID", uid));
    c.insert_prop(Property::new("ACTION", "DISPLAY"));
    c.insert_prop(Property::text("DESCRIPTION", "Reminder"));
    match alarm {
        Alarm::BeforeStart(minutes) => {
            c.insert_prop(Property::new(
                "TRIGGER",
                IcalDuration::from_minutes(-minutes),
            ));
        }
        Alarm::At(ts) => {
            c.insert_prop(
                Property::new("TRIGGER", format_utc(*ts)).with_param("VALUE", "DATE-TIME"),
            );
        }
    }
    c
}

fn set_text(comp: &mut Component, name: &str, value: Option<&str>) {
    match value {
        Some(v) => comp.set(Property::text(name, v)),
        None => {
            comp.remove(name);
        }
    }
}

fn set_url(comp: &mut Component, value: Option<&str>) {
    match value {
        Some(v) => comp.set(Property::new("URL", v).with_param("VALUE", "URI")),
        None => {
            comp.remove("URL");
        }
    }
}

fn set_free(comp: &mut Component, free: bool) {
    comp.set(Property::new(
        "TRANSP",
        if free { "TRANSPARENT" } else { "OPAQUE" },
    ));
}

/// The start and end values of a timing input.
pub fn timing_values(timing: &Timing) -> Result<(EventTime, EventTime), PrimaryError> {
    match timing {
        Timing::AllDay { start, last } => {
            if last < start {
                return Err(PrimaryError::invalid("endDate must not precede startDate"));
            }
            let end = last
                .tomorrow()
                .map_err(|e| PrimaryError::invalid(e.to_string()))?;
            Ok((EventTime::Date(*start), EventTime::Date(end)))
        }
        Timing::Timed { start, zone, end } => {
            let start_value = zone.time(*start)?;
            let start_instant = compatible(&zone.tz(), *start)
                .ok_or_else(|| PrimaryError::invalid("start is out of range"))?;
            let end_value = match end {
                TimedEnd::At(local, end_zone) => end_zone.time(*local)?,
                TimedEnd::Minutes(minutes) => {
                    let instant = start_instant
                        .checked_add(SignedDuration::from_mins(*minutes))
                        .map_err(|e| PrimaryError::invalid(e.to_string()))?;
                    match zone {
                        Zone::Utc => EventTime::Utc(instant),
                        Zone::Iana(name) => EventTime::Zoned {
                            local: instant.to_zoned(zone.tz()).datetime(),
                            tzid: name.clone(),
                        },
                    }
                }
            };
            let end_instant = match &end_value {
                EventTime::Utc(ts) => *ts,
                EventTime::Zoned { local, tzid } => {
                    compatible(&TimeZone::get(tzid).unwrap_or(TimeZone::UTC), *local)
                        .ok_or_else(|| PrimaryError::invalid("end is out of range"))?
                }
                _ => start_instant,
            };
            if end_instant <= start_instant {
                return Err(PrimaryError::invalid("end must be after start"));
            }
            Ok((start_value, end_value))
        }
    }
}

/// The RRULE for a structured recurrence starting at `start`. A timed
/// `untilDate` becomes the UTC instant of the series' local start time on
/// that date; an all-day one stays a DATE.
pub fn rrule_for(
    recurrence: &Recurrence,
    start: &EventTime,
    zones: &Zones<'_>,
) -> Result<RRule, PrimaryError> {
    let mut rule = RRule::new(recurrence.freq);
    rule.interval = recurrence.interval.max(1);
    rule.by_day = recurrence.by_day.clone();
    rule.by_month_day = recurrence.by_month_day.clone();
    rule.by_month = recurrence.by_month.clone();
    rule.by_set_pos = recurrence.by_set_pos.clone();
    rule.wkst = recurrence.week_start;
    match &recurrence.end {
        RecurrenceEnd::Never => {}
        RecurrenceEnd::Count(n) => rule.count = Some(*n),
        RecurrenceEnd::Until(date) => {
            if *date < start.civil().date() {
                return Err(PrimaryError::invalid(
                    "untilDate must not precede the start date",
                ));
            }
            rule.until = Some(match start {
                EventTime::Date(_) => Until::Date(*date),
                EventTime::Floating(local) => Until::Local(date.to_datetime(local.time())),
                other => {
                    let local = date.to_datetime(other.civil().time());
                    let instant = zones
                        .instant(&other.with_civil(local))
                        .ok_or_else(|| PrimaryError::invalid("untilDate is out of range"))?;
                    Until::Utc(instant)
                }
            });
        }
    }
    Ok(rule)
}

fn until_horizon(rule: Option<&RRule>, start: Timestamp) -> Option<Timestamp> {
    match rule.and_then(|r| r.until.as_ref()) {
        Some(Until::Utc(ts)) => Some(*ts),
        Some(Until::Date(d)) => d
            .to_datetime(Time::midnight())
            .to_zoned(TimeZone::UTC)
            .ok()
            .map(|z| z.timestamp()),
        _ => start
            .checked_add(SignedDuration::from_hours(24 * 366 * 10))
            .ok(),
    }
}

/// Plans a new event at a key-derived href with `If-None-Match: *`.
pub fn plan_create(ctx: &EditCtx, input: &NewEvent) -> Result<Plan, PrimaryError> {
    let (event_id, uid) = derived_ids("calendar-create", &ctx.key);
    let (start, end) = timing_values(&input.timing)?;
    let body = build_new(ctx, &uid, &start, &end, input, None)?;
    Ok(Plan {
        status: PlanStatus::Created,
        steps: vec![PlannedStep {
            kind: StepKind::Put,
            event_id: event_id.clone(),
            precondition: Precondition::IfNoneMatchAny,
            body: Some(body),
        }],
        warnings: Vec::new(),
        scheduling_notified: false,
        result_event_id: Some(event_id),
    })
}

fn build_new(
    ctx: &EditCtx,
    uid: &str,
    start: &EventTime,
    end: &EventTime,
    input: &NewEvent,
    related_to: Option<&str>,
) -> Result<String, PrimaryError> {
    let mut root = Component::new("VCALENDAR");
    root.insert_prop(Property::new("VERSION", "2.0"));
    root.insert_prop(Property::new("PRODID", PRODID));
    root.insert_prop(Property::new("CALSCALE", "GREGORIAN"));
    let mut doc = IcsDoc::new(root);
    let mut event = Component::new("VEVENT");
    event.insert_prop(Property::new("UID", uid));
    event.insert_prop(Property::new("DTSTAMP", stamp(ctx.now)));
    event.insert_prop(Property::new("CREATED", stamp(ctx.now)));
    event.insert_prop(Property::new("LAST-MODIFIED", stamp(ctx.now)));
    event.insert_prop(Property::new("SEQUENCE", "0"));
    event.insert_prop(Property::text("SUMMARY", &input.title));
    event.insert_prop(start.to_property("DTSTART"));
    event.insert_prop(end.to_property("DTEND"));
    if let Some(location) = &input.location {
        event.insert_prop(Property::text("LOCATION", location));
    }
    if let Some(notes) = &input.notes {
        event.insert_prop(Property::text("DESCRIPTION", notes));
    }
    if let Some(url) = &input.url {
        event.insert_prop(Property::new("URL", url.as_str()).with_param("VALUE", "URI"));
    }
    event.insert_prop(Property::new(
        "TRANSP",
        if input.free { "TRANSPARENT" } else { "OPAQUE" },
    ));
    if let Some(related) = related_to {
        event.insert_prop(Property::new("RELATED-TO", related).with_param("RELTYPE", "SIBLING"));
    }
    let zones_doc = IcsDoc::new(Component::new("VCALENDAR"));
    let zones = Zones::new(&zones_doc, ctx.default_tz.clone());
    let rule = match &input.recurrence {
        Some(r) => Some(rrule_for(r, start, &zones)?),
        None => None,
    };
    if let Some(rule) = &rule {
        event.insert_prop(Property::new("RRULE", rule.to_value()));
    }
    for (i, alarm) in input.alarms.iter().enumerate() {
        event.push_child(alarm_component(alarm, uid, i));
    }
    let start_instant = zones.instant(start).unwrap_or(ctx.now);
    doc.root.push_child(event);
    ensure_vtimezones(
        &mut doc,
        &[start, end],
        until_horizon(rule.as_ref(), start_instant),
    );
    Ok(doc.serialize())
}

/// Write blockers for an existing resource.
pub fn write_blockers(doc: &IcsDoc, zones: &Zones<'_>) -> Vec<String> {
    let mut blockers = Vec::new();
    let Some(event) = model::primary_event(doc) else {
        blockers.push("no-event".to_owned());
        return blockers;
    };
    let unresolvable = doc.events().any(|comp| {
        let view = EventView { comp };
        [view.start(), view.dtend()]
            .into_iter()
            .flatten()
            .any(|t| t.tzid().is_some_and(|tzid| zones.iana(tzid).is_none()))
    });
    if unresolvable {
        blockers.push("unresolvable-time-zone".to_owned());
    }
    if event.start().is_none() {
        blockers.push("no-start".to_owned());
    }
    blockers
}

/// The most restrictive scheduling role across every VEVENT of the resource
/// (an override can carry attendees or an organizer the master lacks).
fn resource_role(doc: &IcsDoc, owner: &BTreeSet<String>) -> SchedulingRole {
    let roles: Vec<SchedulingRole> = doc
        .events()
        .map(|comp| EventView { comp }.scheduling_role(owner))
        .collect();
    if roles.contains(&SchedulingRole::Attendee) {
        SchedulingRole::Attendee
    } else if roles.contains(&SchedulingRole::Organizer) {
        SchedulingRole::Organizer
    } else {
        SchedulingRole::None
    }
}

/// Checks the scheduling role; returns whether iCloud may email attendees.
fn check_scheduling(
    doc: &IcsDoc,
    owner: &BTreeSet<String>,
    send: bool,
) -> Result<bool, PrimaryError> {
    match resource_role(doc, owner) {
        SchedulingRole::None => Ok(false),
        SchedulingRole::Attendee => Err(PrimaryError::coded(
            "invitation_read_only",
            "this event is an invitation from someone else; it cannot be changed or deleted here",
        )),
        SchedulingRole::Organizer if send => Ok(true),
        SchedulingRole::Organizer => Err(PrimaryError::coded(
            "scheduling_side_effect_refused",
            "this event has attendees and iCloud would email them; pass attendeeNotifications: \"send\" to accept that",
        )),
    }
}

fn parse_current(current: &Current<'_>) -> Result<IcsDoc, PrimaryError> {
    IcsDoc::parse(current.ics).map_err(|e| {
        PrimaryError::coded(
            "not_writable",
            format!("the stored event cannot be parsed ({e})"),
        )
    })
}

fn master_index(doc: &IcsDoc) -> Option<usize> {
    doc.root.items.iter().position(|item| {
        matches!(item, Item::Child(c) if c.name == "VEVENT" && c.prop("RECURRENCE-ID").is_none())
    })
}

fn master_mut(doc: &mut IcsDoc) -> Option<&mut Component> {
    let index = master_index(doc)?;
    match doc.root.items.get_mut(index) {
        Some(Item::Child(c)) => Some(c),
        _ => None,
    }
}

/// The canonical key of a requested recurrence ID against the master start.
fn requested_key(
    zones: &Zones<'_>,
    master_start: &EventTime,
    rid: &str,
) -> Result<(String, DateTime), PrimaryError> {
    let parsed = time::parse_key(rid)
        .ok_or_else(|| PrimaryError::invalid("recurrenceId is not a recognized occurrence key"))?;
    let civil = zones
        .to_master_civil(&parsed, master_start)
        .ok_or_else(|| PrimaryError::invalid("recurrenceId is out of range"))?;
    Ok((time::format_key(master_start, civil), civil))
}

/// Override VEVENTs by canonical key (index into `doc.root.items`).
fn override_indices(
    doc: &IcsDoc,
    zones: &Zones<'_>,
    master_start: &EventTime,
) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for (i, item) in doc.root.items.iter().enumerate() {
        let Item::Child(c) = item else { continue };
        if c.name != "VEVENT" {
            continue;
        }
        if let Some(rid) = (EventView { comp: c }).recurrence_id()
            && let Some(key) = zones.key(&rid, master_start)
        {
            out.insert(key, i);
        }
    }
    out
}

/// Whether `key` (civil `civil`) is a live instance: a raw instance not
/// excluded, or an existing override.
fn is_instance(
    doc: &IcsDoc,
    zones: &Zones<'_>,
    master: &EventView<'_>,
    start: &EventTime,
    key: &str,
    civil: DateTime,
) -> bool {
    if override_indices(doc, zones, start).contains_key(key) {
        return true;
    }
    let excluded = master
        .exdates()
        .iter()
        .any(|ex| zones.key(ex, start).as_deref() == Some(key));
    if excluded {
        return false;
    }
    let (instances, _, _) = expand::raw_instances(master, start, zones, civil);
    instances.iter().any(|(k, _)| k == key)
}

fn apply_fields(
    comp: &mut Component,
    patch: &Patch,
    master_before: Option<&EventView<'_>>,
) -> Result<(), PrimaryError> {
    // An override follows a series-wide text change only where it still
    // matched the master.
    let follows_of = |comp: &Component, name: &str| -> bool {
        match master_before {
            None => true,
            Some(m) => m.comp.text(name) == comp.text(name),
        }
    };
    let follows_summary = follows_of(comp, "SUMMARY");
    let follows_description = follows_of(comp, "DESCRIPTION");
    let follows_location = follows_of(comp, "LOCATION");
    if let Some(title) = &patch.title
        && follows_summary
    {
        comp.set(Property::text("SUMMARY", title));
    }
    if let Some(notes) = &patch.notes
        && follows_description
    {
        set_text(comp, "DESCRIPTION", notes.as_deref());
    }
    if let Some(location) = &patch.location
        && follows_location
    {
        if comp.text("LOCATION").as_deref() != location.as_deref() {
            comp.remove("X-APPLE-STRUCTURED-LOCATION");
        }
        set_text(comp, "LOCATION", location.as_deref());
    }
    if let Some(url) = &patch.url {
        let same = master_before.is_none_or(|m| m.url() == EventView { comp }.url());
        if same {
            set_url(comp, url.as_deref());
        }
    }
    if let Some(free) = patch.free {
        let same = master_before.is_none_or(|m| m.is_free() == EventView { comp }.is_free());
        if same {
            set_free(comp, free);
        }
    }
    if master_before.is_none()
        && let Some(alarms) = &patch.alarms
    {
        let unsupported = EventView { comp }
            .alarms()
            .iter()
            .any(|a| matches!(a.action.as_str(), "EMAIL" | "PROCEDURE"));
        if unsupported && !alarms.is_empty() {
            return Err(PrimaryError::coded(
                "unsupported_alarm_present",
                "the event has email or procedure alarms; pass alarms: [] to remove them first",
            ));
        }
        let uid = EventView { comp }.uid().unwrap_or_default();
        comp.retain_children(|c| c.name != "VALARM");
        for (i, alarm) in alarms.iter().enumerate() {
            comp.push_child(alarm_component(alarm, &uid, i));
        }
    }
    Ok(())
}

/// The new start and end for a timing patch applied to an event.
fn patched_timing(
    patch: &TimingPatch,
    event: &EventView<'_>,
    zones: &Zones<'_>,
) -> Result<(EventTime, EventTime), PrimaryError> {
    let old_start = event
        .start()
        .ok_or_else(|| PrimaryError::coded("not_writable", "the event has no start"))?;
    let span = event.span(zones);
    match patch {
        TimingPatch::Full(timing) => timing_values(timing),
        TimingPatch::MoveDate(date) => {
            let new_start = match &old_start {
                EventTime::Date(_) => EventTime::Date(*date),
                other => {
                    let local = date.to_datetime(other.civil().time());
                    match other {
                        EventTime::Zoned { tzid, .. } => match zones.iana(tzid) {
                            Some((_, tz)) if is_nonexistent(&tz, local) => {
                                return Err(PrimaryError::coded(
                                    "nonexistent_local_time",
                                    format!("{local} does not exist in {tzid}"),
                                ));
                            }
                            _ => other.with_civil(local),
                        },
                        _ => other.with_civil(local),
                    }
                }
            };
            let end = span
                .end_for(&new_start, event.dtend().as_ref(), zones)
                .ok_or_else(|| PrimaryError::invalid("the moved end is out of range"))?;
            Ok((new_start, end))
        }
        TimingPatch::MoveTime { local, zone } => {
            if old_start.is_date() {
                return Err(PrimaryError::invalid(
                    "moveStartTo with a time needs a timed event; send a full timing to change an all-day event",
                ));
            }
            let new_start = match zone {
                Some(zone) => zone.time(*local)?,
                None => match &old_start {
                    EventTime::Zoned { tzid, .. } => {
                        if let Some((_, tz)) = zones.iana(tzid)
                            && is_nonexistent(&tz, *local)
                        {
                            return Err(PrimaryError::coded(
                                "nonexistent_local_time",
                                format!("{local} does not exist in {tzid}"),
                            ));
                        }
                        old_start.with_civil(*local)
                    }
                    other => other.with_civil(*local),
                },
            };
            let end = span
                .end_for(&new_start, event.dtend().as_ref(), zones)
                .ok_or_else(|| PrimaryError::invalid("the moved end is out of range"))?;
            Ok((new_start, end))
        }
    }
}

fn same_frame(a: &EventTime, b: &EventTime) -> bool {
    match (a, b) {
        (EventTime::Date(_), EventTime::Date(_))
        | (EventTime::Utc(_), EventTime::Utc(_))
        | (EventTime::Floating(_), EventTime::Floating(_)) => true,
        (EventTime::Zoned { tzid: x, .. }, EventTime::Zoned { tzid: y, .. }) => x == y,
        _ => false,
    }
}

fn set_start_end(comp: &mut Component, start: &EventTime, end: &EventTime) {
    set_time(comp, "DTSTART", start);
    if comp.prop("DURATION").is_some() && comp.prop("DTEND").is_none() {
        comp.remove("DURATION");
    }
    set_time(comp, "DTEND", end);
}

fn finish(
    before: &IcsDoc,
    after: IcsDoc,
    current: &Current<'_>,
    notified: bool,
    warnings: Vec<String>,
) -> Plan {
    if model::semantic_mismatches(before, &after).is_empty() {
        return Plan {
            status: PlanStatus::Unchanged,
            steps: Vec::new(),
            warnings,
            scheduling_notified: false,
            result_event_id: Some(current.event_id.to_owned()),
        };
    }
    Plan {
        status: PlanStatus::Updated,
        steps: vec![PlannedStep {
            kind: StepKind::Put,
            event_id: current.event_id.to_owned(),
            precondition: Precondition::IfMatch(current.etag.to_owned()),
            body: Some(after.serialize()),
        }],
        warnings,
        scheduling_notified: notified,
        result_event_id: Some(current.event_id.to_owned()),
    }
}

/// Plans an update.
pub fn plan_update(
    ctx: &EditCtx,
    current: &Current<'_>,
    scope: Scope,
    recurrence_id: Option<&str>,
    patch: &Patch,
    drop_exceptions: bool,
    send_notifications: bool,
) -> Result<Plan, PrimaryError> {
    if patch.is_empty() {
        return Err(PrimaryError::coded(
            "empty_patch",
            "changes must contain at least one field",
        ));
    }
    let before = parse_current(current)?;
    let zones = Zones::new(&before, ctx.default_tz.clone());
    let blockers = write_blockers(&before, &zones);
    if !blockers.is_empty() {
        return Err(PrimaryError::coded(
            "not_writable",
            format!("the event cannot be edited ({})", blockers.join(", ")),
        ));
    }
    let master = model::master(&before)
        .ok_or_else(|| PrimaryError::coded("not_writable", "the event has no series master"))?;
    let notified = check_scheduling(&before, &ctx.owner, send_notifications)?;
    let start = master
        .start()
        .ok_or_else(|| PrimaryError::coded("not_writable", "the event has no start"))?;
    let recurring = !master.rrules().is_empty() || !master.rdates().is_empty();
    let rule_editable =
        master.rrules().len() <= 1 && master.rrule().is_none_or(|r| r.is_editable());
    if patch.recurrence.is_some() && !rule_editable {
        return Err(PrimaryError::coded(
            "unsupported_recurrence_edit",
            "this series uses a recurrence rule the structured editor cannot change",
        ));
    }
    let effective_scope = if !recurring { Scope::Series } else { scope };
    match effective_scope {
        Scope::Series => update_series(
            ctx,
            current,
            &before,
            &zones,
            patch,
            drop_exceptions,
            notified,
        ),
        Scope::Occurrence => {
            let rid = recurrence_id.ok_or_else(|| {
                PrimaryError::invalid("recurrenceId is required for scope occurrence")
            })?;
            if patch.recurrence.is_some() {
                return Err(PrimaryError::invalid(
                    "recurrence can only change with scope series or following",
                ));
            }
            update_occurrence(ctx, current, &before, &zones, &start, rid, patch, notified)
        }
        Scope::Following => {
            if !rule_editable {
                return Err(PrimaryError::coded(
                    "unsupported_recurrence_edit",
                    "this series uses a recurrence rule that cannot be split",
                ));
            }
            let rid = recurrence_id.ok_or_else(|| {
                PrimaryError::invalid("recurrenceId is required for scope following")
            })?;
            let (key, civil) = requested_key(&zones, &start, rid)?;
            if key == time::format_key(&start, start.civil()) {
                return update_series(
                    ctx,
                    current,
                    &before,
                    &zones,
                    patch,
                    drop_exceptions,
                    notified,
                );
            }
            if !is_instance(&before, &zones, &master, &start, &key, civil) {
                return Err(PrimaryError::coded(
                    "not_found",
                    "recurrenceId is not an occurrence of this event",
                ));
            }
            split_following(
                ctx, current, &before, &zones, &start, civil, patch, notified,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn update_series(
    ctx: &EditCtx,
    current: &Current<'_>,
    before: &IcsDoc,
    zones: &Zones<'_>,
    patch: &Patch,
    drop_exceptions: bool,
    notified: bool,
) -> Result<Plan, PrimaryError> {
    let master_view = model::master(before)
        .ok_or_else(|| PrimaryError::coded("not_writable", "the event has no series master"))?;
    let old_start = master_view
        .start()
        .ok_or_else(|| PrimaryError::coded("not_writable", "the event has no start"))?;
    let mut after = before.clone();
    let new_timing = match &patch.timing {
        Some(t) => Some(patched_timing(t, &master_view, zones)?),
        None => None,
    };
    let master_index = master_index(&after)
        .ok_or_else(|| PrimaryError::coded("not_writable", "the event has no series master"))?;
    let overrides_before = override_indices(before, zones, &old_start);
    let mut warnings = Vec::new();
    let mut time_changed = false;
    {
        let Some(Item::Child(master)) = after.root.items.get_mut(master_index) else {
            return Err(PrimaryError::coded(
                "not_writable",
                "the event has no series master",
            ));
        };
        apply_fields(master, patch, None)?;
        if let Some((start, end)) = &new_timing {
            set_start_end(master, start, end);
            time_changed = true;
        }
        match &patch.recurrence {
            Some(Some(recurrence)) => {
                let start = new_timing.as_ref().map_or(&old_start, |(s, _)| s);
                let rule = rrule_for(recurrence, start, zones)?;
                master.set(Property::new("RRULE", rule.to_value()));
                time_changed = true;
            }
            Some(None) => {
                master.remove("RRULE");
                master.remove("RDATE");
                master.remove("EXDATE");
                time_changed = true;
            }
            None => {}
        }
    }
    if patch.recurrence == Some(None) {
        after
            .root
            .retain_children(|c| !(c.name == "VEVENT" && c.prop("RECURRENCE-ID").is_some()));
    } else if time_changed && (!overrides_before.is_empty() || !master_view.exdates().is_empty()) {
        warnings.extend(rekey_exceptions(
            &mut after,
            before,
            zones,
            &old_start,
            new_timing.as_ref().map(|(s, _)| s),
            drop_exceptions,
        )?);
    }
    if let Some(Item::Child(master)) = after.root.items.get_mut(master_index) {
        touch(master, ctx.now);
        if time_changed {
            bump_sequence(master);
        }
    }
    // Text changes reach overrides that still matched the master.
    let master_before = master_view;
    for item in &mut after.root.items {
        if let Item::Child(c) = item
            && c.name == "VEVENT"
            && c.prop("RECURRENCE-ID").is_some()
        {
            let field_patch = Patch {
                timing: None,
                alarms: None,
                recurrence: None,
                ..patch.clone()
            };
            if field_patch != Patch::default() {
                let snapshot = c.clone();
                apply_fields(c, &field_patch, Some(&master_before))?;
                if *c != snapshot {
                    touch(c, ctx.now);
                }
            }
        }
    }
    if let Some((start, end)) = &new_timing {
        let horizon = model::master(&after)
            .and_then(|m| m.rrule())
            .and_then(|r| until_horizon(Some(&r), zones.instant(start).unwrap_or(ctx.now)));
        ensure_vtimezones(&mut after, &[start, end], horizon);
    }
    Ok(finish(before, after, current, notified, warnings))
}

/// Shifts or validates EXDATEs and override RECURRENCE-IDs after the series
/// start or rule changed.
fn rekey_exceptions(
    after: &mut IcsDoc,
    before: &IcsDoc,
    zones: &Zones<'_>,
    old_start: &EventTime,
    new_start: Option<&EventTime>,
    drop_exceptions: bool,
) -> Result<Vec<String>, PrimaryError> {
    let new_start_value = new_start.cloned().unwrap_or_else(|| old_start.clone());
    let rule_changed =
        model::master(before).map(|m| m.rrules()) != model::master(after).map(|m| m.rrules());
    let delta = if same_frame(old_start, &new_start_value) {
        Some(new_start_value.civil().duration_since(old_start.civil()))
    } else {
        None
    };
    let shift = |civil: DateTime| -> Option<DateTime> {
        match delta {
            Some(d) if !rule_changed => civil.checked_add(d).ok(),
            Some(_) => Some(civil),
            None => None,
        }
    };
    let Some(master_before) = model::master(before) else {
        return Ok(Vec::new());
    };
    // Exceptions as (old civil, new civil).
    let exdates: Vec<(DateTime, Option<DateTime>)> = master_before
        .exdates()
        .iter()
        .filter_map(|ex| zones.to_master_civil(ex, old_start))
        .map(|c| (c, shift(c)))
        .collect();
    let overrides: Vec<(usize, DateTime, Option<DateTime>)> =
        override_indices(before, zones, old_start)
            .values()
            .filter_map(|i| match before.root.items.get(*i) {
                Some(Item::Child(c)) => EventView { comp: c }
                    .recurrence_id()
                    .and_then(|rid| zones.to_master_civil(&rid, old_start))
                    .map(|civil| (*i, civil, shift(civil))),
                _ => None,
            })
            .collect();
    let latest = exdates
        .iter()
        .filter_map(|(_, n)| *n)
        .chain(overrides.iter().filter_map(|(_, _, n)| *n))
        .max();
    let Some(latest) = latest.or_else(|| exdates.first().map(|(c, _)| *c)) else {
        return Ok(Vec::new());
    };
    let after_master =
        model::master(after).ok_or_else(|| PrimaryError::coded("not_writable", "no master"))?;
    let new_start_time = after_master
        .start()
        .unwrap_or_else(|| new_start_value.clone());
    let scan_until = latest
        .checked_add(SignedDuration::from_hours(24))
        .unwrap_or(latest);
    let (instances, truncated, _) =
        expand::raw_instances(&after_master, &new_start_time, zones, scan_until);
    let members: BTreeSet<DateTime> = instances.iter().map(|(_, c)| *c).collect();
    let last_instance = instances.last().map(|(_, c)| *c);
    let finite = after_master
        .rrule()
        .is_some_and(|r| r.count.is_some() || r.until.is_some())
        && !truncated;
    let past_end = |civil: DateTime| finite && last_instance.is_some_and(|last| civil > last);
    let mut warnings = Vec::new();
    let mut nonmembers = 0usize;
    let mut keep_exdates: Vec<DateTime> = Vec::new();
    for (_, new) in &exdates {
        match new {
            Some(c) if members.contains(c) => keep_exdates.push(*c),
            Some(c) if past_end(*c) => {}
            _ => nonmembers += 1,
        }
    }
    let mut override_moves: Vec<(usize, Option<DateTime>)> = Vec::new();
    for (index, _, new) in &overrides {
        match new {
            Some(c) if members.contains(c) => override_moves.push((*index, Some(*c))),
            Some(c) if past_end(*c) => override_moves.push((*index, None)),
            _ => {
                nonmembers += 1;
                override_moves.push((*index, None));
            }
        }
    }
    if nonmembers > 0 && !drop_exceptions {
        return Err(PrimaryError::coded(
            "exceptions_present",
            format!(
                "{nonmembers} exception(s) (deleted or changed occurrences) would no longer fall on the series; pass dropExceptions: true to remove them"
            ),
        ));
    }
    if nonmembers > 0 {
        warnings.push(format!("dropped {nonmembers} exception(s)"));
    }
    // Rewrite EXDATEs on the master.
    if let Some(master) = master_mut(after) {
        master.remove("EXDATE");
        for civil in keep_exdates {
            master.insert_prop(new_start_time.with_civil(civil).to_property("EXDATE"));
        }
    }
    // Re-key or drop overrides (indices refer to `before`, which `after` mirrors).
    let mut drop_indices: BTreeSet<usize> = BTreeSet::new();
    for (index, new) in override_moves {
        match new {
            None => {
                drop_indices.insert(index);
            }
            Some(civil) => {
                if let Some(Item::Child(c)) = after.root.items.get_mut(index) {
                    let old_rid = EventView { comp: c }.recurrence_id();
                    let old_rid_civil = old_rid
                        .as_ref()
                        .and_then(|rid| zones.to_master_civil(rid, old_start));
                    let own_start_unchanged = EventView { comp: c }
                        .start()
                        .and_then(|s| zones.to_master_civil(&s, old_start))
                        == old_rid_civil;
                    c.set(
                        new_start_time
                            .with_civil(civil)
                            .to_property("RECURRENCE-ID"),
                    );
                    if own_start_unchanged {
                        let view_span = EventView { comp: c }.span(zones);
                        let start_value = new_start_time.with_civil(civil);
                        let end_like = EventView { comp: c }.dtend();
                        if let Some(end) = view_span.end_for(&start_value, end_like.as_ref(), zones)
                        {
                            set_start_end(c, &start_value, &end);
                        }
                    }
                }
            }
        }
    }
    let mut i = 0usize;
    after.root.items.retain(|_| {
        let keep = !drop_indices.contains(&i);
        i += 1;
        keep
    });
    Ok(warnings)
}

#[allow(clippy::too_many_arguments)]
fn update_occurrence(
    ctx: &EditCtx,
    current: &Current<'_>,
    before: &IcsDoc,
    zones: &Zones<'_>,
    start: &EventTime,
    rid: &str,
    patch: &Patch,
    notified: bool,
) -> Result<Plan, PrimaryError> {
    let master = model::master(before)
        .ok_or_else(|| PrimaryError::coded("not_writable", "the event has no series master"))?;
    let (key, civil) = requested_key(zones, start, rid)?;
    if !is_instance(before, zones, &master, start, &key, civil) {
        return Err(PrimaryError::coded(
            "not_found",
            "recurrenceId is not an occurrence of this event",
        ));
    }
    let mut after = before.clone();
    let existing = override_indices(before, zones, start).get(&key).copied();
    let index = match existing {
        Some(i) => i,
        None => {
            let mut override_comp = master.comp.clone();
            override_comp.retain_props(|p| {
                !matches!(
                    p.name.as_str(),
                    "RRULE"
                        | "RDATE"
                        | "EXDATE"
                        | "DTSTART"
                        | "DTEND"
                        | "DURATION"
                        | "RECURRENCE-ID"
                )
            });
            let instance_start = start.with_civil(civil);
            let span = master.span(zones);
            let end = span
                .end_for(&instance_start, master.dtend().as_ref(), zones)
                .ok_or_else(|| PrimaryError::invalid("the occurrence end is out of range"))?;
            override_comp.insert_prop(instance_start.to_property("RECURRENCE-ID"));
            override_comp.insert_prop(instance_start.to_property("DTSTART"));
            override_comp.insert_prop(end.to_property("DTEND"));
            after.root.push_child(override_comp);
            after.root.items.len() - 1
        }
    };
    let Some(Item::Child(target)) = after.root.items.get(index).cloned() else {
        return Err(PrimaryError::coded("not_found", "occurrence not found"));
    };
    let mut target = target;
    let old_times = (
        EventView { comp: &target }.start(),
        EventView { comp: &target }.dtend(),
    );
    apply_fields(&mut target, patch, None)?;
    if let Some(timing) = &patch.timing {
        let (s, e) = patched_timing(timing, &EventView { comp: &target }, zones)?;
        set_start_end(&mut target, &s, &e);
        ensure_vtimezones(&mut after, &[&s, &e], None);
    }
    let time_changed = (
        EventView { comp: &target }.start(),
        EventView { comp: &target }.dtend(),
    ) != old_times;
    touch(&mut target, ctx.now);
    let index = after
        .root
        .items
        .iter()
        .position(|item| matches!(item, Item::Child(c) if c.name == "VEVENT" && EventView { comp: c }.recurrence_id().and_then(|r| zones.key(&r, start)).as_deref() == Some(key.as_str())))
        .unwrap_or(index);
    let master_sequence = {
        let master = master_mut(&mut after)
            .ok_or_else(|| PrimaryError::coded("not_writable", "the event has no series master"))?;
        if time_changed {
            bump_sequence(master);
            touch(master, ctx.now);
        }
        EventView { comp: master }.sequence()
    };
    target.set(Property::new("SEQUENCE", master_sequence.to_string()));
    if let Some(slot) = after.root.items.get_mut(index) {
        *slot = Item::Child(target);
    }
    Ok(finish(before, after, current, notified, Vec::new()))
}

#[allow(clippy::too_many_arguments)]
fn split_following(
    ctx: &EditCtx,
    current: &Current<'_>,
    before: &IcsDoc,
    zones: &Zones<'_>,
    start: &EventTime,
    split_civil: DateTime,
    patch: &Patch,
    notified: bool,
) -> Result<Plan, PrimaryError> {
    let master = model::master(before)
        .ok_or_else(|| PrimaryError::coded("not_writable", "the event has no series master"))?;
    let rule = master.rrule().ok_or_else(|| {
        PrimaryError::coded(
            "unsupported_recurrence_edit",
            "only RRULE series can be split",
        )
    })?;
    let original_uid = master.uid().unwrap_or_default();
    let (new_event_id, new_uid) = derived_ids("calendar-split", &ctx.key);

    // The new series starts at the split occurrence.
    let split_start = start.with_civil(split_civil);
    let span = master.span(zones);
    let split_end = span
        .end_for(&split_start, master.dtend().as_ref(), zones)
        .ok_or_else(|| PrimaryError::invalid("the occurrence end is out of range"))?;
    let (instances, _, _) = expand::raw_instances(&master, start, zones, split_civil);
    let before_count = instances.iter().filter(|(_, c)| *c < split_civil).count() as u32;
    let mut new_rule = rule.clone();
    if let Some(count) = rule.count {
        if count <= before_count {
            return Err(PrimaryError::coded(
                "not_found",
                "recurrenceId is past the end of the series",
            ));
        }
        new_rule.count = Some(count - before_count);
    }

    let mut new_doc = IcsDoc::new({
        let mut root = Component::new("VCALENDAR");
        root.insert_prop(Property::new("VERSION", "2.0"));
        root.insert_prop(Property::new("PRODID", PRODID));
        root.insert_prop(Property::new("CALSCALE", "GREGORIAN"));
        root
    });
    for vt in before.timezones() {
        new_doc.root.push_child(vt.clone());
    }
    let mut new_master = master.comp.clone();
    new_master.retain_props(|p| {
        !matches!(
            p.name.as_str(),
            "UID"
                | "RRULE"
                | "EXDATE"
                | "RDATE"
                | "DTSTART"
                | "DTEND"
                | "DURATION"
                | "RELATED-TO"
                | "SEQUENCE"
                | "CREATED"
                | "DTSTAMP"
                | "LAST-MODIFIED"
        )
    });
    new_master.insert_prop(Property::new("UID", new_uid.clone()));
    new_master.insert_prop(Property::new("SEQUENCE", "0"));
    new_master.insert_prop(Property::new("CREATED", stamp(ctx.now)));
    new_master.insert_prop(Property::new("DTSTAMP", stamp(ctx.now)));
    new_master.insert_prop(Property::new("LAST-MODIFIED", stamp(ctx.now)));
    new_master.insert_prop(split_start.to_property("DTSTART"));
    new_master.insert_prop(split_end.to_property("DTEND"));
    new_master.insert_prop(Property::new("RRULE", new_rule.to_value()));
    new_master.insert_prop(
        Property::new("RELATED-TO", original_uid.as_str()).with_param("RELTYPE", "SIBLING"),
    );

    // Apply the patch to the new series.
    apply_fields(&mut new_master, patch, None)?;
    let (patched_start, patched_end) = match &patch.timing {
        Some(t) => patched_timing(t, &EventView { comp: &new_master }, zones)?,
        None => (split_start.clone(), split_end.clone()),
    };
    set_start_end(&mut new_master, &patched_start, &patched_end);
    let delta = same_frame(&split_start, &patched_start)
        .then(|| patched_start.civil().duration_since(split_civil));
    match &patch.recurrence {
        Some(Some(recurrence)) => {
            let rule = rrule_for(recurrence, &patched_start, zones)?;
            new_master.set(Property::new("RRULE", rule.to_value()));
        }
        Some(None) => {
            new_master.remove("RRULE");
        }
        None => {}
    }
    let keep_exceptions = patch.recurrence.is_none() && delta.is_some();
    let shifted =
        |civil: DateTime| -> Option<DateTime> { delta.and_then(|d| civil.checked_add(d).ok()) };
    // EXDATEs and overrides at or after the split move to the new series.
    if keep_exceptions {
        for ex in master.exdates() {
            if let Some(civil) = zones.to_master_civil(&ex, start)
                && civil >= split_civil
                && let Some(moved) = shifted(civil)
            {
                new_master.insert_prop(patched_start.with_civil(moved).to_property("EXDATE"));
            }
        }
    }
    new_doc.root.push_child(new_master);
    if keep_exceptions {
        for o in model::overrides(before) {
            let Some(civil) = o
                .recurrence_id()
                .and_then(|r| zones.to_master_civil(&r, start))
            else {
                continue;
            };
            if civil < split_civil {
                continue;
            }
            let Some(moved) = shifted(civil) else {
                continue;
            };
            let mut c = o.comp.clone();
            c.set(Property::new("UID", new_uid.clone()));
            c.set(patched_start.with_civil(moved).to_property("RECURRENCE-ID"));
            new_doc.root.push_child(c);
        }
    }
    let horizon = until_horizon(
        Some(&new_rule),
        zones.instant(&patched_start).unwrap_or(ctx.now),
    );
    ensure_vtimezones(&mut new_doc, &[&patched_start, &patched_end], horizon);

    let truncated_original = truncate_at(before, zones, start, split_civil, ctx.now)?;
    Ok(Plan {
        status: PlanStatus::Updated,
        steps: vec![
            PlannedStep {
                kind: StepKind::Put,
                event_id: new_event_id.clone(),
                precondition: Precondition::IfNoneMatchAny,
                body: Some(new_doc.serialize()),
            },
            PlannedStep {
                kind: StepKind::Put,
                event_id: current.event_id.to_owned(),
                precondition: Precondition::IfMatch(current.etag.to_owned()),
                body: Some(truncated_original.serialize()),
            },
        ],
        warnings: Vec::new(),
        scheduling_notified: notified,
        result_event_id: Some(new_event_id),
    })
}

/// The series ending just before `split_civil`: UNTIL one second before the
/// split instance (the previous day for all-day series), COUNT removed, and
/// later EXDATEs and overrides dropped.
fn truncate_at(
    before: &IcsDoc,
    zones: &Zones<'_>,
    start: &EventTime,
    split_civil: DateTime,
    now: Timestamp,
) -> Result<IcsDoc, PrimaryError> {
    let mut after = before.clone();
    let split_value = start.with_civil(split_civil);
    let until = match start {
        EventTime::Date(_) => Until::Date(
            split_civil
                .date()
                .yesterday()
                .map_err(|e| PrimaryError::invalid(e.to_string()))?,
        ),
        EventTime::Floating(_) => Until::Local(
            split_civil
                .checked_sub(SignedDuration::from_secs(1))
                .map_err(|e| PrimaryError::invalid(e.to_string()))?,
        ),
        _ => Until::Utc(
            zones
                .instant(&split_value)
                .and_then(|i| i.checked_sub(SignedDuration::from_secs(1)).ok())
                .ok_or_else(|| PrimaryError::invalid("the split time is out of range"))?,
        ),
    };
    let master_view = model::master(before)
        .ok_or_else(|| PrimaryError::coded("not_writable", "the event has no series master"))?;
    let mut rule = master_view.rrule().ok_or_else(|| {
        PrimaryError::coded(
            "unsupported_recurrence_edit",
            "only RRULE series can be split",
        )
    })?;
    rule.count = None;
    rule.until = Some(until);
    let kept_exdates: Vec<EventTime> = master_view
        .exdates()
        .into_iter()
        .filter(|ex| {
            zones
                .to_master_civil(ex, start)
                .is_some_and(|c| c < split_civil)
        })
        .collect();
    if let Some(master) = master_mut(&mut after) {
        master.set(Property::new("RRULE", rule.to_value()));
        if master_view.exdates().len() != kept_exdates.len() {
            master.remove("EXDATE");
            for ex in &kept_exdates {
                let civil = zones
                    .to_master_civil(ex, start)
                    .unwrap_or_else(|| ex.civil());
                master.insert_prop(start.with_civil(civil).to_property("EXDATE"));
            }
        }
        bump_sequence(master);
        touch(master, now);
    }
    after.root.retain_children(|c| {
        if c.name != "VEVENT" {
            return true;
        }
        match (EventView { comp: c }).recurrence_id() {
            None => true,
            Some(rid) => zones
                .to_master_civil(&rid, start)
                .is_none_or(|civil| civil < split_civil),
        }
    });
    Ok(after)
}

/// Plans a delete.
pub fn plan_delete(
    ctx: &EditCtx,
    current: &Current<'_>,
    scope: Scope,
    recurrence_id: Option<&str>,
    send_notifications: bool,
) -> Result<Plan, PrimaryError> {
    let before = parse_current(current)?;
    let zones = Zones::new(&before, ctx.default_tz.clone());
    if model::primary_event(&before).is_none() {
        return Err(PrimaryError::coded(
            "not_writable",
            "the resource has no event",
        ));
    }
    let notified = check_scheduling(&before, &ctx.owner, send_notifications)?;
    let series = || Plan {
        status: PlanStatus::Deleted,
        steps: vec![PlannedStep {
            kind: StepKind::Delete,
            event_id: current.event_id.to_owned(),
            precondition: Precondition::IfMatch(current.etag.to_owned()),
            body: None,
        }],
        warnings: Vec::new(),
        scheduling_notified: notified,
        result_event_id: None,
    };
    let Some(master) = model::master(&before) else {
        return Ok(series());
    };
    let recurring = !master.rrules().is_empty() || !master.rdates().is_empty();
    if !recurring || scope == Scope::Series {
        return Ok(series());
    }
    let blockers = write_blockers(&before, &zones);
    if !blockers.is_empty() {
        return Err(PrimaryError::coded(
            "not_writable",
            format!("the event cannot be edited ({})", blockers.join(", ")),
        ));
    }
    let start = master
        .start()
        .ok_or_else(|| PrimaryError::coded("not_writable", "the event has no start"))?;
    let rid = recurrence_id.ok_or_else(|| {
        PrimaryError::invalid("recurrenceId is required for scope occurrence or following")
    })?;
    let (key, civil) = requested_key(&zones, &start, rid)?;
    if !is_instance(&before, &zones, &master, &start, &key, civil) {
        return Err(PrimaryError::coded(
            "not_found",
            "recurrenceId is not an occurrence of this event",
        ));
    }
    let first_key = time::format_key(&start, start.civil());
    match scope {
        Scope::Following if key == first_key => Ok(series()),
        Scope::Following => {
            if master.rrule().is_none_or(|r| !r.is_editable()) {
                return Err(PrimaryError::coded(
                    "unsupported_recurrence_edit",
                    "this series uses a recurrence rule that cannot be truncated",
                ));
            }
            let after = truncate_at(&before, &zones, &start, civil, ctx.now)?;
            let mut plan = finish(&before, after, current, notified, Vec::new());
            plan.status = PlanStatus::Deleted;
            plan.result_event_id = Some(current.event_id.to_owned());
            Ok(plan)
        }
        _ => {
            // One occurrence: EXDATE it and drop its override.
            let horizon = civil
                .checked_add(SignedDuration::from_hours(24 * 366 * 200))
                .unwrap_or(civil);
            let finite = master
                .rrule()
                .is_some_and(|r| r.count.is_some() || r.until.is_some());
            if finite {
                let (all, truncated, _) = expand::raw_instances(&master, &start, &zones, horizon);
                let excluded: BTreeSet<String> = master
                    .exdates()
                    .iter()
                    .filter_map(|ex| zones.key(ex, &start))
                    .collect();
                let remaining = all
                    .iter()
                    .filter(|(k, _)| *k != key && !excluded.contains(k))
                    .count();
                if remaining == 0 && !truncated {
                    let mut plan = series();
                    plan.warnings.push(
                        "this is the only remaining occurrence, so the whole event is deleted"
                            .to_owned(),
                    );
                    return Ok(plan);
                }
            }
            let mut after = before.clone();
            let index = override_indices(&before, &zones, &start).get(&key).copied();
            if let Some(index) = index {
                after.root.items.remove(index);
            }
            if let Some(m) = master_mut(&mut after) {
                m.insert_prop(start.with_civil(civil).to_property("EXDATE"));
                bump_sequence(m);
                touch(m, ctx.now);
            }
            let mut plan = finish(&before, after, current, notified, Vec::new());
            plan.status = PlanStatus::Deleted;
            Ok(plan)
        }
    }
}
