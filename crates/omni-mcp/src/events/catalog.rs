//! The MCP Events catalog.
//!
//! Each event declares its arguments, payload schema and the rule that matches
//! a payload to a subscription. Arguments are string-valued filters kept in a
//! canonical form; subscription identity depends on [`canonical_event_arguments`].

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

pub const EMAIL_RECEIVED: &str = "email.received";
pub const CLAUDE_TURN_FINISHED: &str = "claude.session.turn_finished";
pub use omni_api::events::{
    CALENDAR_EVENT_CHANGED, CALENDAR_EVENT_STARTING, LIVESTREAM_STATUS_CHANGED,
    PRESSPODS_JOB_FINISHED, TASK_RUN_FINISHED,
};

/// Validated subscription arguments (insertion ordered).
pub type EventArguments = IndexMap<String, String>;

/// Which event a definition describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    EmailReceived,
    ClaudeTurnFinished,
    LivestreamStatusChanged,
    PresspodsJobFinished,
    TaskRunFinished,
    CalendarEventChanged,
    CalendarEventStarting,
}

/// One catalog entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventDefinition {
    pub kind: EventKind,
    pub name: &'static str,
    pub description: &'static str,
}

const EMAIL_DESCRIPTION: &str = "A new iCloud message arrived in the selected Inbox or Archive mailbox. Read full mail with email tools using messageId.";
const CLAUDE_DESCRIPTION: &str = "A Claude Code session on the Claude Code host finished its turn or stopped. Omni checks the host about every 15 seconds while a subscription is active. Read the result with claude_session_get (includeResult) using sessionId.";

const LIVESTREAM_DESCRIPTION: &str = "A streamer went live or offline (aggregate edges across their platforms; a primary-platform switch is silent). Omni checks every 20 seconds, background-tier streamers every minute. Read details with livestream_get using streamerId.";
const PRESSPODS_DESCRIPTION: &str = "A PressPods episode job finished: the episode was published or the job failed permanently. Retryable failures do not fire. Read the episode with presspods_episode_get or failed jobs with presspods_list.";
const TASK_RUN_DESCRIPTION: &str = "A scheduled or manual Omni task run finished. Omni checks run history every 30 seconds while a subscription is active. Read the error and logs with task_run_get using runId.";

const CALENDAR_CHANGED_DESCRIPTION: &str = "An event in the primary iCloud calendar was created, updated or deleted, by any client. By default changes written by Omni's calendar tools are skipped (origin any includes them); email-pipeline writes count as external. Omni checks for changes about every minute while a subscription is active. Read details with calendar_event_get using eventId.";
const CALENDAR_STARTING_DESCRIPTION: &str = "An occurrence in the primary iCloud calendar is about to start (leadMinutes before its start) or one of its alerts is due. Fires once per occurrence and lead time; a rescheduled occurrence fires again for its new time. Omni checks every 30 seconds while a subscription is active. Read details with calendar_event_get using eventId (recurrenceId names the occurrence).";

/// `calendar.event_starting` leads, in minutes.
pub const CALENDAR_LEAD_MINUTES: [&str; 8] = ["0", "5", "10", "15", "30", "60", "120", "1440"];

pub const EVENT_DEFINITIONS: [EventDefinition; 7] = [
    EventDefinition {
        kind: EventKind::EmailReceived,
        name: EMAIL_RECEIVED,
        description: EMAIL_DESCRIPTION,
    },
    EventDefinition {
        kind: EventKind::ClaudeTurnFinished,
        name: CLAUDE_TURN_FINISHED,
        description: CLAUDE_DESCRIPTION,
    },
    EventDefinition {
        kind: EventKind::LivestreamStatusChanged,
        name: LIVESTREAM_STATUS_CHANGED,
        description: LIVESTREAM_DESCRIPTION,
    },
    EventDefinition {
        kind: EventKind::PresspodsJobFinished,
        name: PRESSPODS_JOB_FINISHED,
        description: PRESSPODS_DESCRIPTION,
    },
    EventDefinition {
        kind: EventKind::TaskRunFinished,
        name: TASK_RUN_FINISHED,
        description: TASK_RUN_DESCRIPTION,
    },
    EventDefinition {
        kind: EventKind::CalendarEventChanged,
        name: CALENDAR_EVENT_CHANGED,
        description: CALENDAR_CHANGED_DESCRIPTION,
    },
    EventDefinition {
        kind: EventKind::CalendarEventStarting,
        name: CALENDAR_EVENT_STARTING,
        description: CALENDAR_STARTING_DESCRIPTION,
    },
];

pub fn event_definition(name: &str) -> Option<&'static EventDefinition> {
    EVENT_DEFINITIONS.iter().find(|d| d.name == name)
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$`.
fn is_project_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && value.len() <= 100
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A full Claude Code session ID, as the payload's `sessionId` carries it:
/// a lowercase UUID, `^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$`.
fn is_session_id(value: &str) -> bool {
    value.len() == 36
        && value.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_digit() || matches!(c, 'a'..='f'),
        })
}

/// `^[A-Za-z0-9][A-Za-z0-9._:%-]{0,199}$`: configured and discovered streamer IDs.
fn is_streamer_id(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && value.len() <= 200
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '%' | '-'))
}

/// An exact task name: 1-100 characters, no control characters, no padding.
fn is_task_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 100
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

/// The argument object when every key is allowed and every value is a string.
fn string_arguments<'a>(raw: &'a Value, allowed: &[&str]) -> Option<&'a Map<String, Value>> {
    let args = raw.as_object()?;
    args.iter()
        .all(|(key, value)| allowed.contains(&key.as_str()) && value.is_string())
        .then_some(args)
}

/// A string-enum argument, its default when omitted.
fn choice(args: &Map<String, Value>, key: &str, values: &[&str], default: &str) -> Option<String> {
    match args.get(key).and_then(Value::as_str) {
        None => Some(default.to_owned()),
        Some(value) => values.contains(&value).then(|| value.to_owned()),
    }
}

/// An optional exact-identifier argument: `Some(None)` when omitted, `None` when invalid.
fn identifier(
    args: &Map<String, Value>,
    key: &str,
    valid: fn(&str) -> bool,
) -> Option<Option<String>> {
    match args.get(key).and_then(Value::as_str) {
        None => Some(None),
        Some(value) => valid(value).then(|| Some(value.to_owned())),
    }
}

/// Canonical arguments: defaults filled, absent identifiers omitted.
fn canonical(entries: Vec<(&str, Option<String>)>) -> EventArguments {
    entries
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value)))
        .collect()
}

fn data_str<'a>(data: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    data.get(key).and_then(Value::as_str)
}

/// An exact-match filter; an absent argument matches everything.
fn same(args: &EventArguments, key: &str, data: &Map<String, Value>, field: &str) -> bool {
    args.get(key)
        .is_none_or(|wanted| data_str(data, field) == Some(wanted.as_str()))
}

/// A string-enum filter where `any` (or an absent argument) matches everything.
fn selected(args: &EventArguments, key: &str, data: &Map<String, Value>, field: &str) -> bool {
    args.get(key)
        .is_none_or(|wanted| wanted == "any" || data_str(data, field) == Some(wanted.as_str()))
}

/// A nullable payload field.
fn nullable(kind: &str) -> Value {
    json!({"type": [kind, "null"]})
}

fn timestamp() -> Value {
    json!({"type": "string", "format": "date-time"})
}

impl EventDefinition {
    /// The arguments in canonical form, or `None` when they are invalid.
    pub fn parse_arguments(&self, raw: &Value) -> Option<EventArguments> {
        let args = raw.as_object()?;
        match self.kind {
            EventKind::EmailReceived => {
                if args.len() != 1 {
                    return None;
                }
                let folder = args.get("folder")?.as_str()?;
                matches!(folder, "inbox" | "archive")
                    .then(|| IndexMap::from([("folder".to_owned(), folder.to_owned())]))
            }
            EventKind::ClaudeTurnFinished => {
                let args = string_arguments(raw, &["project", "exactSessionId"])?;
                Some(canonical(vec![
                    ("project", identifier(args, "project", is_project_name)?),
                    (
                        "exactSessionId",
                        identifier(args, "exactSessionId", is_session_id)?,
                    ),
                ]))
            }
            EventKind::LivestreamStatusChanged => {
                let args = string_arguments(raw, &["streamer", "transition", "includeBackground"])?;
                Some(canonical(vec![
                    ("streamer", identifier(args, "streamer", is_streamer_id)?),
                    (
                        "transition",
                        Some(choice(
                            args,
                            "transition",
                            &["any", "went_live", "went_offline"],
                            "any",
                        )?),
                    ),
                    (
                        "includeBackground",
                        Some(choice(
                            args,
                            "includeBackground",
                            &["false", "true"],
                            "false",
                        )?),
                    ),
                ]))
            }
            EventKind::PresspodsJobFinished => {
                let args = string_arguments(raw, &["outcome"])?;
                Some(canonical(vec![(
                    "outcome",
                    Some(choice(
                        args,
                        "outcome",
                        &["any", "published", "failed"],
                        "any",
                    )?),
                )]))
            }
            EventKind::TaskRunFinished => {
                let args = string_arguments(raw, &["task", "status"])?;
                let task = identifier(args, "task", is_task_name)?;
                let status = choice(
                    args,
                    "status",
                    &["error", "error_or_degraded", "any"],
                    "error",
                )?;
                // Every run of every task would be a flood (the live check alone
                // runs every 20 seconds), so `any` needs an exact task.
                if status == "any" && task.is_none() {
                    return None;
                }
                Some(canonical(vec![("task", task), ("status", Some(status))]))
            }
            EventKind::CalendarEventChanged => {
                let args = string_arguments(raw, &["origin", "kinds"])?;
                Some(canonical(vec![
                    (
                        "origin",
                        Some(choice(args, "origin", &["external", "any"], "external")?),
                    ),
                    (
                        "kinds",
                        Some(choice(
                            args,
                            "kinds",
                            &["all", "created", "updated", "deleted"],
                            "all",
                        )?),
                    ),
                ]))
            }
            EventKind::CalendarEventStarting => {
                let args = string_arguments(raw, &["trigger", "leadMinutes", "includeAllDay"])?;
                let trigger = choice(args, "trigger", &["start", "alarm"], "start")?;
                let lead = if trigger == "start" {
                    Some(choice(args, "leadMinutes", &CALENDAR_LEAD_MINUTES, "15")?)
                } else if args.contains_key("leadMinutes") {
                    // An alarm fires at its own trigger time.
                    return None;
                } else {
                    None
                };
                Some(canonical(vec![
                    ("trigger", Some(trigger)),
                    ("leadMinutes", lead),
                    (
                        "includeAllDay",
                        Some(choice(args, "includeAllDay", &["false", "true"], "false")?),
                    ),
                ]))
            }
        }
    }

    /// Whether an event with this payload belongs to a subscription's arguments.
    pub fn matches(&self, args: &EventArguments, data: &Map<String, Value>) -> bool {
        match self.kind {
            EventKind::EmailReceived => {
                data.get("folder").and_then(Value::as_str) == args.get("folder").map(String::as_str)
            }
            EventKind::ClaudeTurnFinished => {
                let project_ok = match args.get("project") {
                    None => true,
                    Some(project) if project.is_empty() => true,
                    Some(project) => data_str(data, "project") == Some(project.as_str()),
                };
                project_ok && same(args, "exactSessionId", data, "sessionId")
            }
            EventKind::LivestreamStatusChanged => {
                same(args, "streamer", data, "streamerId")
                    && selected(args, "transition", data, "transition")
                    && (args.get("includeBackground").map(String::as_str) == Some("true")
                        || data_str(data, "tier") != Some("background"))
            }
            EventKind::PresspodsJobFinished => selected(args, "outcome", data, "outcome"),
            EventKind::TaskRunFinished => {
                let status = data_str(data, "status");
                same(args, "task", data, "taskName")
                    && match args.get("status").map(String::as_str) {
                        Some("any") => true,
                        Some("error_or_degraded") => matches!(status, Some("error" | "degraded")),
                        _ => status == Some("error"),
                    }
            }
            EventKind::CalendarEventChanged => {
                let origin_ok = match args.get("origin").map(String::as_str) {
                    Some("any") => true,
                    _ => data_str(data, "origin") == Some("external"),
                };
                let kind_ok = args.get("kinds").is_none_or(|wanted| {
                    wanted == "all" || data_str(data, "changeKind") == Some(wanted.as_str())
                });
                origin_ok && kind_ok
            }
            EventKind::CalendarEventStarting => {
                // The scanner publishes once per subscription tuple, so each
                // subscription receives exactly the publications of its tuple.
                let trigger = args.get("trigger").map_or("start", String::as_str);
                let include_all_day = args.get("includeAllDay").map_or("false", String::as_str);
                data_str(data, "trigger") == Some(trigger)
                    && data_str(data, "includeAllDay") == Some(include_all_day)
                    && (include_all_day == "true"
                        || data.get("allDay").and_then(Value::as_bool) != Some(true))
                    && (trigger != "start"
                        || data_str(data, "leadMinutes")
                            == Some(args.get("leadMinutes").map_or("15", String::as_str)))
            }
        }
    }

    pub fn input_schema(&self) -> Value {
        match self.kind {
            EventKind::EmailReceived => json!({
                "type": "object",
                "properties": {"folder": {"type": "string", "enum": ["inbox", "archive"]}},
                "required": ["folder"],
                "additionalProperties": false,
            }),
            EventKind::ClaudeTurnFinished => json!({
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Only sessions in this project (see claude_link_status)",
                    },
                    "exactSessionId": {
                        "type": "string",
                        "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
                        "description": "Only this session: its full sessionId from claude_sessions_list or claude_session_start, not the short id or a prefix. With project, both must match.",
                    },
                },
                "additionalProperties": false,
            }),
            EventKind::LivestreamStatusChanged => json!({
                "type": "object",
                "properties": {
                    "streamer": {
                        "type": "string",
                        "pattern": "^[A-Za-z0-9][A-Za-z0-9._:%-]{0,199}$",
                        "description": "Only this exact streamer ID (see livestreams_list)",
                    },
                    "transition": {
                        "type": "string",
                        "enum": ["any", "went_live", "went_offline"],
                        "default": "any",
                    },
                    "includeBackground": {
                        "type": "string",
                        "enum": ["false", "true"],
                        "default": "false",
                        "description": "Include background-tier streamers, whose notifications are muted",
                    },
                },
                "additionalProperties": false,
            }),
            EventKind::PresspodsJobFinished => json!({
                "type": "object",
                "properties": {
                    "outcome": {
                        "type": "string",
                        "enum": ["any", "published", "failed"],
                        "default": "any",
                    },
                },
                "additionalProperties": false,
            }),
            EventKind::TaskRunFinished => json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 100,
                        "description": "Only runs of this exact task name (see tasks_list); required with status any",
                    },
                    "status": {
                        "type": "string",
                        "enum": ["error", "error_or_degraded", "any"],
                        "default": "error",
                        "description": "error: failed runs; error_or_degraded: also runs that skipped their work because an upstream failed; any: every finished run",
                    },
                },
                "additionalProperties": false,
            }),
            EventKind::CalendarEventChanged => json!({
                "type": "object",
                "properties": {
                    "origin": {
                        "type": "string",
                        "enum": ["external", "any"],
                        "default": "external",
                        "description": "external: skip changes written by Omni's calendar tools (avoids reacting to your own writes); any: include them",
                    },
                    "kinds": {
                        "type": "string",
                        "enum": ["all", "created", "updated", "deleted"],
                        "default": "all",
                    },
                },
                "additionalProperties": false,
            }),
            EventKind::CalendarEventStarting => json!({
                "type": "object",
                "properties": {
                    "trigger": {
                        "type": "string",
                        "enum": ["start", "alarm"],
                        "default": "start",
                        "description": "start: leadMinutes before each occurrence starts; alarm: when one of the event's own alerts is due",
                    },
                    "leadMinutes": {
                        "type": "string",
                        "enum": CALENDAR_LEAD_MINUTES,
                        "default": "15",
                        "description": "Minutes before the start (trigger start only)",
                    },
                    "includeAllDay": {
                        "type": "string",
                        "enum": ["false", "true"],
                        "default": "false",
                        "description": "Include all-day events, which start at local midnight",
                    },
                },
                "additionalProperties": false,
            }),
        }
    }

    pub fn payload_schema(&self) -> Value {
        match self.kind {
            EventKind::EmailReceived => json!({
                "type": "object",
                "properties": {
                    "messageId": {"type": "string"},
                    "folder": {"type": "string", "enum": ["inbox", "archive"]},
                    "uidValidity": {"type": "string"},
                    "uid": {"type": "integer"},
                },
                "required": ["messageId", "folder", "uidValidity", "uid"],
                "additionalProperties": false,
            }),
            EventKind::ClaudeTurnFinished => json!({
                "type": "object",
                "properties": {
                    "sessionId": {"type": "string"},
                    "id": {"type": ["string", "null"]},
                    "project": {"type": ["string", "null"]},
                    "status": {"type": "string"},
                    "revision": {"type": "integer"},
                },
                "required": ["sessionId", "id", "project", "status", "revision"],
                "additionalProperties": false,
            }),
            EventKind::LivestreamStatusChanged => json!({
                "type": "object",
                "properties": {
                    "streamerId": {"type": "string"},
                    "displayName": {"type": "string", "maxLength": 200},
                    "transition": {"type": "string", "enum": ["went_live", "went_offline"]},
                    "tier": {"type": "string", "enum": ["primary", "background"]},
                    "platform": {"type": "string"},
                    "title": {"type": ["string", "null"], "maxLength": 200},
                    "startedAt": timestamp(),
                    "endedAt": {"type": ["string", "null"], "format": "date-time"},
                    "viewerCount": nullable("integer"),
                    "maxViewerCount": nullable("integer"),
                },
                "required": [
                    "streamerId", "displayName", "transition", "tier", "platform", "title",
                    "startedAt", "endedAt", "viewerCount", "maxViewerCount",
                ],
                "additionalProperties": false,
            }),
            EventKind::PresspodsJobFinished => json!({
                "type": "object",
                "properties": {
                    "outcome": {"type": "string", "enum": ["published", "failed"]},
                    "episodeId": nullable("string"),
                    "jobId": nullable("string"),
                    "title": {"type": ["string", "null"], "maxLength": 200},
                    "articleUrlHost": nullable("string"),
                    "durationSeconds": nullable("number"),
                    "attempts": nullable("integer"),
                },
                "required": [
                    "outcome", "episodeId", "jobId", "title", "articleUrlHost",
                    "durationSeconds", "attempts",
                ],
                "additionalProperties": false,
            }),
            EventKind::TaskRunFinished => json!({
                "type": "object",
                "properties": {
                    "runId": {"type": "string"},
                    "taskName": {"type": "string"},
                    "trigger": {
                        "type": "string",
                        "enum": ["schedule", "manual", "startup", "catchup"],
                    },
                    "status": {"type": "string", "enum": ["success", "error", "degraded"]},
                    "startedAt": timestamp(),
                    "finishedAt": timestamp(),
                },
                "required": ["runId", "taskName", "trigger", "status", "startedAt", "finishedAt"],
                "additionalProperties": false,
            }),
            EventKind::CalendarEventChanged => json!({
                "type": "object",
                "properties": {
                    "eventId": {"type": "string"},
                    "uid": nullable("string"),
                    "changeKind": {"type": "string", "enum": ["created", "updated", "deleted"]},
                    "summary": {"type": ["string", "null"], "maxLength": 200},
                    "summaryTruncated": {"type": "boolean"},
                    "start": {"type": ["string", "null"], "format": "date-time"},
                    "allDay": {"type": "boolean"},
                    "recurring": {"type": "boolean"},
                    "changedFields": {
                        "type": "array",
                        "maxItems": 13,
                        "items": {
                            "type": "string",
                            "enum": [
                                "start", "end", "title", "location", "notes", "url",
                                "recurrence", "exceptions", "alarms", "attendees",
                                "availability", "status", "other",
                            ],
                        },
                    },
                    "version": nullable("string"),
                    "origin": {"type": "string", "enum": ["omni", "external"]},
                    "detectedAt": timestamp(),
                },
                "required": [
                    "eventId", "uid", "changeKind", "summary", "summaryTruncated", "start",
                    "allDay", "recurring", "changedFields", "version", "origin", "detectedAt",
                ],
                "additionalProperties": false,
            }),
            EventKind::CalendarEventStarting => json!({
                "type": "object",
                "properties": {
                    "eventId": {"type": "string"},
                    "uid": {"type": "string"},
                    "recurrenceId": nullable("string"),
                    "summary": {"type": ["string", "null"], "maxLength": 200},
                    "summaryTruncated": {"type": "boolean"},
                    "start": timestamp(),
                    "end": timestamp(),
                    "allDay": {"type": "boolean"},
                    "timeZone": nullable("string"),
                    "trigger": {"type": "string", "enum": ["start", "alarm"]},
                    "leadMinutes": {"type": ["string", "null"], "enum": [
                        "0", "5", "10", "15", "30", "60", "120", "1440", null,
                    ]},
                    "alarmId": nullable("string"),
                    "fireAt": timestamp(),
                    "late": {"type": "boolean"},
                    "hasLocation": {"type": "boolean"},
                    "includeAllDay": {"type": "string", "enum": ["false", "true"]},
                },
                "required": [
                    "eventId", "uid", "recurrenceId", "summary", "summaryTruncated", "start",
                    "end", "allDay", "timeZone", "trigger", "leadMinutes", "alarmId", "fireAt",
                    "late", "hasLocation", "includeAllDay",
                ],
                "additionalProperties": false,
            }),
        }
    }
}

/// The `events/list` catalog.
pub fn event_catalog() -> Value {
    Value::Array(
        EVENT_DEFINITIONS
            .iter()
            .map(|definition| {
                json!({
                    "name": definition.name,
                    "description": definition.description,
                    "delivery": ["webhook"],
                    "inputSchema": definition.input_schema(),
                    "payloadSchema": definition.payload_schema(),
                })
            })
            .collect(),
    )
}

/// Stable JSON with keys sorted by `localeCompare`; subscription identity depends on it.
pub fn canonical_event_arguments(args: &EventArguments) -> String {
    let mut entries: Vec<(&String, &String)> = args.iter().collect();
    entries.sort_by(|a, b| omni_core::js::locale_compare(a.0, b.0));
    let object: Map<String, Value> = entries
        .into_iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    omni_core::js::json_stringify(&Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_arguments_need_exactly_one_folder() {
        let email = &EVENT_DEFINITIONS[0];
        assert!(email.parse_arguments(&json!({"folder": "inbox"})).is_some());
        assert!(email.parse_arguments(&json!({"folder": "sent"})).is_none());
        assert!(email.parse_arguments(&json!({})).is_none());
        assert!(
            email
                .parse_arguments(&json!({"folder": "inbox", "x": 1}))
                .is_none()
        );
    }

    #[test]
    fn claude_arguments_take_an_optional_safe_project() {
        let claude = &EVENT_DEFINITIONS[1];
        assert_eq!(claude.parse_arguments(&json!({})), Some(IndexMap::new()));
        assert!(
            claude
                .parse_arguments(&json!({"project": "../etc"}))
                .is_none()
        );
        assert!(
            claude
                .parse_arguments(&json!({"project": "omni"}))
                .is_some()
        );
        assert!(claude.parse_arguments(&json!({"other": "x"})).is_none());
    }

    const SESSION: &str = "068e5d6d-877d-46cb-8514-ee47317af8c6";

    #[test]
    fn claude_arguments_take_an_optional_full_session_id() {
        let claude = definition(CLAUDE_TURN_FINISHED);
        assert_eq!(
            claude.parse_arguments(&json!({"exactSessionId": SESSION})),
            Some(args(&[("exactSessionId", SESSION)]))
        );
        for rejected in [
            json!("068e5d6d"),
            json!("068e5d6d-877d-46cb-8514"),
            json!(SESSION.to_uppercase()),
            json!(format!(" {SESSION}")),
            json!(format!("{SESSION}0")),
            json!("068e5d6d-877d-46cb-8514+ee47317af8c6"),
            json!(""),
            json!(null),
            json!(["068e5d6d-877d-46cb-8514-ee47317af8c6"]),
        ] {
            assert!(
                claude
                    .parse_arguments(&json!({"exactSessionId": rejected}))
                    .is_none(),
                "{rejected} should be rejected"
            );
        }
        let both = claude
            .parse_arguments(&json!({"project": "omni", "exactSessionId": SESSION}))
            .unwrap();
        assert_eq!(
            canonical_event_arguments(&both),
            format!(r#"{{"exactSessionId":"{SESSION}","project":"omni"}}"#)
        );
        // Project-only and unfiltered subscriptions keep their identities.
        let project = claude.parse_arguments(&json!({"project": "omni"})).unwrap();
        assert_eq!(canonical_event_arguments(&project), r#"{"project":"omni"}"#);
        assert_eq!(
            canonical_event_arguments(&claude.parse_arguments(&json!({})).unwrap()),
            "{}"
        );
    }

    #[test]
    fn claude_matching_honors_project_and_exact_session() {
        let claude = definition(CLAUDE_TURN_FINISHED);
        let other = "a47778af-36a9-4c0b-a5fb-0b87fcb8e4b0";
        let turn = |session: &str, project: Value| {
            data(json!({"sessionId": session, "id": &session[..8], "project": project}))
        };
        let parse = |raw: Value| claude.parse_arguments(&raw).unwrap();

        let all = parse(json!({}));
        assert!(claude.matches(&all, &turn(other, json!(null))));

        let project = parse(json!({"project": "omni"}));
        assert!(claude.matches(&project, &turn(SESSION, json!("omni"))));
        assert!(claude.matches(&project, &turn(other, json!("omni"))));
        assert!(!claude.matches(&project, &turn(SESSION, json!("dotfiles"))));
        assert!(!claude.matches(&project, &turn(SESSION, json!(null))));

        let session = parse(json!({"exactSessionId": SESSION}));
        assert!(claude.matches(&session, &turn(SESSION, json!("omni"))));
        assert!(claude.matches(&session, &turn(SESSION, json!(null))));
        assert!(!claude.matches(&session, &turn(other, json!("omni"))));
        assert!(!claude.matches(&session, &data(json!({"id": &SESSION[..8]}))));

        let both = parse(json!({"project": "omni", "exactSessionId": SESSION}));
        assert!(claude.matches(&both, &turn(SESSION, json!("omni"))));
        assert!(!claude.matches(&both, &turn(SESSION, json!("dotfiles"))));
        assert!(!claude.matches(&both, &turn(SESSION, json!(null))));
        assert!(!claude.matches(&both, &turn(other, json!("omni"))));
    }

    fn definition(name: &str) -> &'static EventDefinition {
        event_definition(name).unwrap()
    }

    fn args(pairs: &[(&str, &str)]) -> EventArguments {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn data(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn omitted_arguments_canonicalize_to_their_defaults() {
        let live = definition(LIVESTREAM_STATUS_CHANGED);
        let empty = live.parse_arguments(&json!({})).unwrap();
        let explicit = live
            .parse_arguments(&json!({"transition": "any", "includeBackground": "false"}))
            .unwrap();
        assert_eq!(
            canonical_event_arguments(&empty),
            canonical_event_arguments(&explicit)
        );
        assert_eq!(
            canonical_event_arguments(&empty),
            r#"{"includeBackground":"false","transition":"any"}"#
        );
        let pods = definition(PRESSPODS_JOB_FINISHED);
        assert_eq!(
            pods.parse_arguments(&json!({})),
            Some(args(&[("outcome", "any")]))
        );
        let runs = definition(TASK_RUN_FINISHED);
        assert_eq!(
            runs.parse_arguments(&json!({})),
            Some(args(&[("status", "error")]))
        );
    }

    #[test]
    fn invalid_arguments_are_rejected() {
        let live = definition(LIVESTREAM_STATUS_CHANGED);
        assert!(
            live.parse_arguments(&json!({"transition": "live"}))
                .is_none()
        );
        assert!(
            live.parse_arguments(&json!({"includeBackground": true}))
                .is_none()
        );
        assert!(live.parse_arguments(&json!({"streamer": "../x"})).is_none());
        assert!(live.parse_arguments(&json!({"other": "x"})).is_none());
        assert!(
            live.parse_arguments(&json!({"streamer": "dgg:youtube:abc%20d"}))
                .is_some()
        );
        let pods = definition(PRESSPODS_JOB_FINISHED);
        assert!(
            pods.parse_arguments(&json!({"outcome": "retrying"}))
                .is_none()
        );
        let runs = definition(TASK_RUN_FINISHED);
        assert!(runs.parse_arguments(&json!({"status": "any"})).is_none());
        assert!(
            runs.parse_arguments(&json!({"task": " PressPods"}))
                .is_none()
        );
        assert_eq!(
            runs.parse_arguments(&json!({"task": "PressPods", "status": "any"})),
            Some(args(&[("task", "PressPods"), ("status", "any")]))
        );
    }

    #[test]
    fn livestream_matching_honors_streamer_transition_and_tier() {
        let live = definition(LIVESTREAM_STATUS_CHANGED);
        let event = |id: &str, transition: &str, tier: &str| {
            data(json!({"streamerId": id, "transition": transition, "tier": tier}))
        };
        let all = live.parse_arguments(&json!({})).unwrap();
        assert!(live.matches(&all, &event("a", "went_live", "primary")));
        assert!(!live.matches(&all, &event("a", "went_live", "background")));
        let background = live
            .parse_arguments(&json!({"includeBackground": "true"}))
            .unwrap();
        assert!(live.matches(&background, &event("a", "went_live", "background")));
        let one = live
            .parse_arguments(&json!({"streamer": "a", "transition": "went_offline"}))
            .unwrap();
        assert!(live.matches(&one, &event("a", "went_offline", "primary")));
        assert!(!live.matches(&one, &event("a", "went_live", "primary")));
        assert!(!live.matches(&one, &event("b", "went_offline", "primary")));
    }

    #[test]
    fn presspods_and_task_matching() {
        let pods = definition(PRESSPODS_JOB_FINISHED);
        let failed = data(json!({"outcome": "failed"}));
        let published = pods
            .parse_arguments(&json!({"outcome": "published"}))
            .unwrap();
        assert!(!pods.matches(&published, &failed));
        assert!(pods.matches(&pods.parse_arguments(&json!({})).unwrap(), &failed));

        let runs = definition(TASK_RUN_FINISHED);
        let ok = data(json!({"taskName": "PressPods", "status": "success"}));
        let error = data(json!({"taskName": "PressPods", "status": "error"}));
        let errors = runs.parse_arguments(&json!({})).unwrap();
        assert!(runs.matches(&errors, &error));
        assert!(!runs.matches(&errors, &ok));
        let degraded = data(json!({"taskName": "PressPods", "status": "degraded"}));
        assert!(!runs.matches(&errors, &degraded));
        let soft = runs
            .parse_arguments(&json!({"status": "error_or_degraded"}))
            .unwrap();
        assert!(runs.matches(&soft, &degraded));
        assert!(!runs.matches(&soft, &ok));
        let every = runs
            .parse_arguments(&json!({"task": "PressPods", "status": "any"}))
            .unwrap();
        assert!(runs.matches(&every, &ok));
        assert!(!runs.matches(&every, &data(json!({"taskName": "X", "status": "error"}))));
    }

    #[test]
    fn calendar_arguments_canonicalize_and_validate() {
        let changed = definition(CALENDAR_EVENT_CHANGED);
        let empty = changed.parse_arguments(&json!({})).unwrap();
        let explicit = changed
            .parse_arguments(&json!({"origin": "external", "kinds": "all"}))
            .unwrap();
        assert_eq!(
            canonical_event_arguments(&empty),
            canonical_event_arguments(&explicit)
        );
        assert!(
            changed
                .parse_arguments(&json!({"origin": "omni"}))
                .is_none()
        );
        assert!(
            changed
                .parse_arguments(&json!({"kinds": "moved"}))
                .is_none()
        );

        let starting = definition(CALENDAR_EVENT_STARTING);
        assert_eq!(
            starting.parse_arguments(&json!({})),
            Some(args(&[
                ("trigger", "start"),
                ("leadMinutes", "15"),
                ("includeAllDay", "false"),
            ]))
        );
        assert_eq!(
            starting.parse_arguments(&json!({"trigger": "alarm"})),
            Some(args(&[("trigger", "alarm"), ("includeAllDay", "false")]))
        );
        assert!(
            starting
                .parse_arguments(&json!({"trigger": "alarm", "leadMinutes": "15"}))
                .is_none()
        );
        assert!(
            starting
                .parse_arguments(&json!({"leadMinutes": "7"}))
                .is_none()
        );
        assert!(
            starting
                .parse_arguments(&json!({"leadMinutes": 15}))
                .is_none()
        );
    }

    #[test]
    fn calendar_matching_honors_origin_kinds_and_the_starting_tuple() {
        let changed = definition(CALENDAR_EVENT_CHANGED);
        let change = |origin: &str, kind: &str| data(json!({"origin": origin, "changeKind": kind}));
        let external = changed.parse_arguments(&json!({})).unwrap();
        assert!(changed.matches(&external, &change("external", "updated")));
        assert!(!changed.matches(&external, &change("omni", "updated")));
        let any = changed.parse_arguments(&json!({"origin": "any"})).unwrap();
        assert!(changed.matches(&any, &change("omni", "created")));
        let deletes = changed
            .parse_arguments(&json!({"kinds": "deleted"}))
            .unwrap();
        assert!(!changed.matches(&deletes, &change("external", "created")));
        assert!(changed.matches(&deletes, &change("external", "deleted")));

        let starting = definition(CALENDAR_EVENT_STARTING);
        let fired = |trigger: &str, lead: Value, include: &str, all_day: bool| {
            data(json!({
                "trigger": trigger, "leadMinutes": lead,
                "includeAllDay": include, "allDay": all_day,
            }))
        };
        let default = starting.parse_arguments(&json!({})).unwrap();
        assert!(starting.matches(&default, &fired("start", json!("15"), "false", false)));
        assert!(!starting.matches(&default, &fired("start", json!("5"), "false", false)));
        assert!(!starting.matches(&default, &fired("start", json!("15"), "true", false)));
        assert!(!starting.matches(&default, &fired("start", json!("15"), "false", true)));
        assert!(!starting.matches(&default, &fired("alarm", Value::Null, "false", false)));
        let all_day = starting
            .parse_arguments(&json!({"includeAllDay": "true", "leadMinutes": "60"}))
            .unwrap();
        assert!(starting.matches(&all_day, &fired("start", json!("60"), "true", true)));
        let alarms = starting
            .parse_arguments(&json!({"trigger": "alarm"}))
            .unwrap();
        assert!(starting.matches(&alarms, &fired("alarm", Value::Null, "false", false)));
    }

    #[test]
    fn canonical_arguments_sort_keys() {
        let args = IndexMap::from([
            ("project".to_owned(), "b".to_owned()),
            ("folder".to_owned(), "a".to_owned()),
        ]);
        assert_eq!(
            canonical_event_arguments(&args),
            r#"{"folder":"a","project":"b"}"#
        );
    }
}
