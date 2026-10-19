//! The calendar tools' contracts: metadata, policy and the wire types their
//! input and output schemas derive from (`omni_mcp_kit::schema`). The same
//! types decode input and encode output in [`super`]. After changing anything
//! here run `cargo xtask mcp-golden` and review the snapshot diff.

use omni_mcp_kit::schema::nullable;
use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const READ: Annotations = Annotations {
    read_only_hint: true,
    destructive_hint: false,
    idempotent_hint: true,
    open_world_hint: false,
};

const READ_POLICY: Policy = Policy {
    side_effects: &[],
    cost: "None; at most a few CalDAV reads",
    recommended: ApprovalPolicy::Allow,
};

pub static CALENDAR_STATUS: ToolDef<StatusInput, StatusOutput> = ToolDef::new(ToolInfo {
    name: "calendar_status",
    title: "Inspect Calendar Status",
    description: "Report whether the primary iCloud calendar is identified, writable and in sync, plus the change-feed cursor and tracked email-event counts. Never reveals calendar URLs or credentials.",
    annotations: READ,
    policy: READ_POLICY,
});

pub static CALENDAR_EVENTS_LIST: ToolDef<EventsListInput, EventsListOutput> = ToolDef::new(
    ToolInfo {
        name: "calendar_events_list",
        title: "List Calendar Events",
        description: "List event occurrences in the primary iCloud calendar overlapping a time window (recurring series expanded), sorted by start. Times are local to each event's time zone.",
        annotations: READ,
        policy: READ_POLICY,
    },
);

pub static CALENDAR_EVENTS_SEARCH: ToolDef<EventsSearchInput, EventsListOutput> = ToolDef::new(
    ToolInfo {
        name: "calendar_events_search",
        title: "Search Calendar Events",
        description: "Find events in the primary iCloud calendar whose title, location or notes contain the query, within a window (default 30 days back to a year ahead). A recurring series appears once, at its first matching occurrence.",
        annotations: READ,
        policy: READ_POLICY,
    },
);

pub static CALENDAR_EVENT_GET: ToolDef<EventGetInput, EventDetail> = ToolDef::new(ToolInfo {
    name: "calendar_event_get",
    title: "Get Calendar Event",
    description: "Get one event of the primary iCloud calendar by eventId: details, recurrence, exceptions, alarms, attendees, whether it can be edited, its etag and its next occurrences.",
    annotations: READ,
    policy: READ_POLICY,
});

pub static CALENDAR_EVENT_PREVIEW: ToolDef<PreviewInput, PreviewOutput> = ToolDef::new(ToolInfo {
    name: "calendar_event_preview",
    title: "Preview Calendar Change",
    description: "Plan a create, update or delete in the primary iCloud calendar against its current state and show the exact writes, changed fields and warnings without changing anything. Pass the idempotencyKey you will use so the planned eventIds match.",
    annotations: READ,
    policy: READ_POLICY,
});

pub static CALENDAR_WRITE_STATUS: ToolDef<WriteStatusInput, WriteStatusOutput> = ToolDef::new(
    ToolInfo {
        name: "calendar_write_status",
        title: "Check Calendar Write",
        description: "Report what happened to a calendar write by its idempotencyKey. An uncertain write is checked by reading the event back; it is never sent again.",
        annotations: READ,
        policy: READ_POLICY,
    },
);

pub static CALENDAR_CHANGES_LIST: ToolDef<ChangesListInput, ChangesListOutput> = ToolDef::new(
    ToolInfo {
        name: "calendar_changes_list",
        title: "List Calendar Changes",
        description: "List changes detected in the primary iCloud calendar after a cursor (created, updated, deleted), oldest first: the polling form of the calendar.event_changed event. By default changes written by Omni's calendar tools are skipped (origin any includes them). Keep nextCursor to continue. Changes are kept 30 days.",
        annotations: READ,
        policy: READ_POLICY,
    },
);

pub static CALENDAR_TRACKED_EVENTS_LIST: ToolDef<TrackedListInput, TrackedListOutput> =
    ToolDef::new(ToolInfo {
        name: "calendar_tracked_events_list",
        title: "List Email-Created Events",
        description: "List the events Omni created from email (the extraction pipeline's dedup records) with bounded filtering and pagination.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: false,
        },
        policy: Policy {
            side_effects: &[],
            cost: "None",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static CALENDAR_EVENT_CREATE: ToolDef<CreateInput, WriteOutput> = ToolDef::new(ToolInfo {
    name: "calendar_event_create",
    title: "Create Calendar Event",
    description: "Create an event in the primary iCloud calendar. Repeating the call with the same idempotencyKey returns the first result instead of creating another event. Requires approval.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &["Creates an event in the primary iCloud calendar"],
        cost: "No paid API; a few CalDAV requests",
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static CALENDAR_EVENT_UPDATE: ToolDef<UpdateInput, WriteOutput> = ToolDef::new(ToolInfo {
    name: "calendar_event_update",
    title: "Update Calendar Event",
    description: "Change an event in the primary iCloud calendar: the whole series, one occurrence, or this and following occurrences. Only the given fields change; everything else is kept. Invitations from others are read-only, and events with attendees need attendeeNotifications \"send\". Requires approval.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: true,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &[
            "Changes an event in the primary iCloud calendar",
            "With attendeeNotifications \"send\", iCloud emails the attendees",
        ],
        cost: "No paid API; a few CalDAV requests",
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static CALENDAR_EVENT_DELETE: ToolDef<DeleteInput, WriteOutput> = ToolDef::new(ToolInfo {
    name: "calendar_event_delete",
    title: "Delete Calendar Event",
    description: "Delete an event from the primary iCloud calendar: the whole series, one occurrence, or this and following occurrences. Invitations from others cannot be deleted here. Requires approval.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: true,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &[
            "Deletes an event or occurrences from the primary iCloud calendar",
            "With attendeeNotifications \"send\", iCloud emails the attendees",
        ],
        cost: "No paid API; a few CalDAV requests",
        recommended: ApprovalPolicy::RequireApproval,
    },
});

/// Every calendar tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 11] = [
    &CALENDAR_STATUS,
    &CALENDAR_EVENTS_LIST,
    &CALENDAR_EVENTS_SEARCH,
    &CALENDAR_EVENT_GET,
    &CALENDAR_EVENT_PREVIEW,
    &CALENDAR_WRITE_STATUS,
    &CALENDAR_CHANGES_LIST,
    &CALENDAR_TRACKED_EVENTS_LIST,
    &CALENDAR_EVENT_CREATE,
    &CALENDAR_EVENT_UPDATE,
    &CALENDAR_EVENT_DELETE,
];

// ---- shared ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatusInput {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackedCounts {
    pub active: u64,
    pub cancelled: u64,
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatusOutput {
    pub configured: bool,
    /// `ready`, `not_configured`, `identity_error` or `sync_error`.
    pub state: String,
    pub message: Option<String>,
    pub error_code: Option<String>,
    /// The calendar's display name.
    pub calendar_name: String,
    /// Whether iCloud reports it as the account's default calendar (null when
    /// not reported).
    pub is_server_default: Option<bool>,
    /// Whether the email pipeline writes to this same calendar.
    pub pipeline_targets_primary: Option<bool>,
    pub writable: Option<bool>,
    pub supports_sync: Option<bool>,
    pub last_sync_at: Option<String>,
    pub last_full_sync_at: Option<String>,
    pub event_count: u64,
    /// The newest change-feed cursor.
    pub change_cursor: String,
    /// The time zone used when none is given.
    pub default_time_zone: String,
    pub tracked: TrackedCounts,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Occurrence {
    pub event_id: String,
    /// Identifies this occurrence of a recurring series (pass it with scope
    /// occurrence or following); null for a one-off event.
    pub recurrence_id: Option<String>,
    pub title: String,
    /// `YYYY-MM-DD` for all-day events, else local `YYYY-MM-DDTHH:MM:SS`.
    pub start: String,
    /// Exclusive end in the same form (all-day: the day after the last day).
    pub end: String,
    pub start_utc: String,
    pub end_utc: String,
    pub all_day: bool,
    /// All-day only: the inclusive last day.
    pub last_date: Option<String>,
    /// IANA zone (or `UTC`) of start and end; null for all-day and floating.
    pub time_zone: Option<String>,
    pub location: Option<String>,
    pub recurring: bool,
    /// This occurrence was changed individually.
    pub is_exception: bool,
    pub has_alarms: bool,
    /// `none`, `organizer` (Michael invited others) or `attendee` (an
    /// invitation; read-only).
    pub scheduling_role: String,
    pub free: bool,
    pub status: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsListInput {
    /// Window start: `YYYY-MM-DD` (local midnight) or `YYYY-MM-DDTHH:MM`, in
    /// the default time zone. Default: today.
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}(:\d{2})?)?$"))]
    pub from: Option<String>,
    /// Window end (exclusive), same forms. Default: 14 days after from; at
    /// most 366 days after it.
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}(:\d{2})?)?$"))]
    pub to: Option<String>,
    #[schemars(range(min = 1, max = 500))]
    pub limit: Option<u32>,
    /// Sync with iCloud first even if the local copy is recent.
    pub fresh: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsSearchInput {
    #[schemars(length(min = 1, max = 200))]
    pub query: String,
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}(:\d{2})?)?$"))]
    pub from: Option<String>,
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}(:\d{2})?)?$"))]
    pub to: Option<String>,
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventsListOutput {
    pub events: Vec<Occurrence>,
    pub from: String,
    pub to: String,
    /// More occurrences exist than were returned, or a series could not be
    /// fully expanded.
    pub truncated: bool,
    pub synced_at: Option<String>,
    /// The last sync failed; the data is from syncedAt.
    pub stale: bool,
    pub default_time_zone: String,
}

// ---- event detail ----

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventGetInput {
    #[schemars(length(min = 1, max = 220))]
    pub event_id: String,
    /// Read from iCloud instead of the local copy.
    pub fresh: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecurrenceOut {
    /// The RRULE text.
    pub rule: String,
    /// Whether the structured fields describe the rule fully (otherwise the
    /// recurrence is read-only).
    pub editable: bool,
    pub frequency: Option<Frequency>,
    pub interval: u32,
    pub by_weekday: Vec<String>,
    pub by_month_day: Vec<i32>,
    pub by_month: Vec<i32>,
    pub by_set_pos: Vec<i32>,
    pub count: Option<u32>,
    pub until_date: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangedOccurrence {
    pub recurrence_id: String,
    pub title: String,
    pub start: String,
    pub time_zone: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Exceptions {
    /// recurrenceIds of deleted occurrences.
    pub deleted: Vec<String>,
    pub changed: Vec<ChangedOccurrence>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AlarmOut {
    /// `DISPLAY`, `AUDIO`, `EMAIL`, ...
    pub action: String,
    /// Minutes before the start (negative: after).
    pub minutes_before: Option<i64>,
    /// Minutes before the end, for end-relative alarms.
    pub minutes_before_end: Option<i64>,
    pub at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Attendee {
    pub email: Option<String>,
    pub name: Option<String>,
    /// `ACCEPTED`, `DECLINED`, `TENTATIVE`, `NEEDS-ACTION`, ...
    pub status: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventDetail {
    pub event_id: String,
    /// Pass as `etag` to update or delete only this version.
    pub etag: String,
    pub title: String,
    pub notes: Option<String>,
    pub location: Option<String>,
    pub url: Option<String>,
    /// The (first) start, as in occurrences.
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub last_date: Option<String>,
    pub time_zone: Option<String>,
    pub free: bool,
    pub status: Option<String>,
    pub recurrence: Option<RecurrenceOut>,
    pub exceptions: Exceptions,
    pub alarms: Vec<AlarmOut>,
    pub organizer: Option<Attendee>,
    pub attendees: Vec<Attendee>,
    pub scheduling_role: String,
    /// Whether the edit tools can change it, and why not.
    pub writable: bool,
    pub blockers: Vec<String>,
    /// The next occurrences from now (up to 5).
    pub upcoming: Vec<Occurrence>,
    pub synced_at: Option<String>,
    pub stale: bool,
}

// ---- writes ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AlarmIn {
    /// Minutes before the start (negative: after). Give this or `at`.
    #[schemars(range(min = -10080, max = 40320))]
    pub minutes_before: Option<i64>,
    /// An absolute time with offset, e.g. `2026-10-02T08:00:00-07:00`.
    #[schemars(length(max = 40))]
    pub at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecurrenceIn {
    pub frequency: Frequency,
    #[schemars(range(min = 1, max = 999))]
    pub interval: Option<u32>,
    /// Weekday codes, optionally with an ordinal: `MO`, `2TU`, `-1FR`.
    #[schemars(length(max = 7))]
    pub by_weekday: Option<Vec<String>>,
    #[schemars(length(max = 31))]
    pub by_month_day: Option<Vec<i32>>,
    #[schemars(length(max = 12))]
    pub by_month: Option<Vec<i32>>,
    #[schemars(length(max = 10))]
    pub by_set_pos: Option<Vec<i32>>,
    /// Number of occurrences (or give untilDate; neither repeats forever).
    #[schemars(range(min = 1, max = 1000))]
    pub count: Option<u32>,
    /// The last day an occurrence may start on.
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}$"))]
    pub until_date: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateInput {
    /// A unique key per intended change (16-128 of A-Z a-z 0-9 _ -). Reuse it
    /// only to retry the same request.
    #[schemars(pattern(r"^[A-Za-z0-9_-]{16,128}$"))]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 500))]
    pub title: String,
    /// `YYYY-MM-DD` for an all-day event, else local `YYYY-MM-DDTHH:MM`.
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}(:\d{2})?)?$"))]
    pub start: String,
    /// All-day: the inclusive last day. Timed: the local end time. Omit for
    /// one day, or one hour (or durationMinutes).
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}(:\d{2})?)?$"))]
    pub end: Option<String>,
    #[schemars(range(min = 1, max = 44640))]
    pub duration_minutes: Option<u32>,
    /// IANA zone or `UTC` for a timed event. Default: the default time zone.
    #[schemars(length(min = 1, max = 64))]
    pub time_zone: Option<String>,
    #[schemars(length(max = 8000))]
    pub notes: Option<String>,
    #[schemars(length(max = 1000))]
    pub location: Option<String>,
    #[schemars(length(max = 2000))]
    pub url: Option<String>,
    /// Show as free instead of busy.
    pub free: Option<bool>,
    #[schemars(length(max = 5))]
    pub alarms: Option<Vec<AlarmIn>>,
    pub recurrence: Option<RecurrenceIn>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Series,
    Occurrence,
    Following,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AttendeeNotifications {
    /// Accept that iCloud emails the attendees.
    Send,
    /// Refuse any change that would email attendees (the default).
    Refuse,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Changes {
    #[schemars(length(min = 1, max = 500))]
    pub title: Option<String>,
    /// Null removes the notes.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "::serde_with::rust::double_option::serialize",
        deserialize_with = "::serde_with::rust::double_option::deserialize"
    )]
    #[schemars(length(max = 8000), transform = nullable)]
    pub notes: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "::serde_with::rust::double_option::serialize",
        deserialize_with = "::serde_with::rust::double_option::deserialize"
    )]
    #[schemars(length(max = 1000), transform = nullable)]
    pub location: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "::serde_with::rust::double_option::serialize",
        deserialize_with = "::serde_with::rust::double_option::deserialize"
    )]
    #[schemars(length(max = 2000), transform = nullable)]
    pub url: Option<Option<String>>,
    pub free: Option<bool>,
    /// New timing, as in create (with end, durationMinutes, timeZone).
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}(:\d{2})?)?$"))]
    pub start: Option<String>,
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}(:\d{2})?)?$"))]
    pub end: Option<String>,
    #[schemars(range(min = 1, max = 44640))]
    pub duration_minutes: Option<u32>,
    #[schemars(length(min = 1, max = 64))]
    pub time_zone: Option<String>,
    /// Move keeping the length: `YYYY-MM-DD` keeps the time of day,
    /// `YYYY-MM-DDTHH:MM` sets the local start.
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}(T\d{2}:\d{2}(:\d{2})?)?$"))]
    pub move_start_to: Option<String>,
    /// Replaces all alarms ([] removes them).
    #[schemars(length(max = 5))]
    pub alarms: Option<Vec<AlarmIn>>,
    /// Null stops repeating (series scope only).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "::serde_with::rust::double_option::serialize",
        deserialize_with = "::serde_with::rust::double_option::deserialize"
    )]
    #[schemars(transform = nullable)]
    pub recurrence: Option<Option<RecurrenceIn>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateInput {
    #[schemars(pattern(r"^[A-Za-z0-9_-]{16,128}$"))]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 220))]
    pub event_id: String,
    /// Only change this version (from calendar_event_get).
    #[schemars(length(min = 1, max = 200))]
    pub etag: Option<String>,
    /// Default series.
    pub scope: Option<Scope>,
    /// The occurrence for scope occurrence or following.
    #[schemars(length(min = 8, max = 80))]
    pub recurrence_id: Option<String>,
    pub changes: Changes,
    /// Remove deleted or changed occurrences that no longer fit a moved series.
    pub drop_exceptions: Option<bool>,
    pub attendee_notifications: Option<AttendeeNotifications>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteInput {
    #[schemars(pattern(r"^[A-Za-z0-9_-]{16,128}$"))]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 220))]
    pub event_id: String,
    #[schemars(length(min = 1, max = 200))]
    pub etag: Option<String>,
    pub scope: Option<Scope>,
    #[schemars(length(min = 8, max = 80))]
    pub recurrence_id: Option<String>,
    pub attendee_notifications: Option<AttendeeNotifications>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriteOutput {
    /// `created`, `updated`, `deleted` or `unchanged`.
    pub status: String,
    /// The event to read next (a split's new series); null after deleting a
    /// whole event.
    pub event_id: Option<String>,
    pub etag: Option<String>,
    /// `confirmed`, or `uncertain` (check calendar_write_status; do not retry
    /// with a new key).
    pub state: String,
    /// The written event was read back and matches.
    pub verified: bool,
    /// iCloud emailed attendees.
    pub scheduling_notified: bool,
    pub warnings: Vec<String>,
    /// This idempotencyKey was already used; this is the recorded result.
    pub replayed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewInput {
    /// Exactly one of create, update or delete.
    pub create: Option<CreateInput>,
    pub update: Option<UpdateInput>,
    pub delete: Option<DeleteInput>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlannedWrite {
    /// `PUT` or `DELETE`.
    pub method: String,
    pub event_id: String,
    /// `if-match:<etag>` or `if-none-match`.
    pub precondition: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewOutput {
    pub status: String,
    pub writes: Vec<PlannedWrite>,
    pub changed_fields: Vec<String>,
    pub warnings: Vec<String>,
    pub scheduling_notified: bool,
    /// The bodies of the PUTs, in order (each capped at 64 KiB).
    #[serde(rename = "iCalendar")]
    pub i_calendar: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriteStatusInput {
    #[schemars(pattern(r"^[A-Za-z0-9_-]{16,128}$"))]
    pub idempotency_key: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriteStep {
    pub method: String,
    pub event_id: String,
    /// `planned`, `sending`, `acknowledged`, `verified`, `rejected` or
    /// `uncertain`.
    pub state: String,
    pub http_status: Option<u16>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriteError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WriteStatusOutput {
    pub found: bool,
    /// `reserved`, `confirmed`, `failed` or `uncertain`.
    pub state: Option<String>,
    pub tool: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub steps: Vec<WriteStep>,
    pub result: Option<WriteOutput>,
    pub error: Option<WriteError>,
}

// ---- change feed ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangesListInput {
    /// From a previous nextCursor (or calendar_status changeCursor). Omit for
    /// the oldest kept change.
    #[schemars(pattern(r"^\d{1,16}$"))]
    pub cursor: Option<String>,
    #[schemars(range(min = 1, max = 200))]
    pub limit: Option<u32>,
    /// `external` (default) skips changes written by Omni's calendar tools;
    /// `any` includes them.
    pub origin: Option<ChangeOriginFilter>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChangeOriginFilter {
    #[default]
    External,
    Any,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventSnapshot {
    pub title: Option<String>,
    pub start: Option<String>,
    pub start_utc: Option<String>,
    pub all_day: bool,
    pub recurring: bool,
    pub time_zone: Option<String>,
    pub location: Option<String>,
    pub status: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Change {
    pub cursor: String,
    pub event_id: String,
    pub uid: Option<String>,
    /// `created`, `updated` or `deleted`.
    pub kind: String,
    /// `omni` (written by these tools) or `external`.
    pub origin: String,
    pub changed_fields: Vec<String>,
    /// The resulting version (etag); `None` for a deletion.
    pub version: Option<String>,
    pub detected_at: String,
    pub before: Option<EventSnapshot>,
    pub after: Option<EventSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangesListOutput {
    pub changes: Vec<Change>,
    pub next_cursor: String,
    pub has_more: bool,
}

// ---- tracked (email-created) events ----

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TrackedStatusFilter {
    #[default]
    Active,
    Cancelled,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackedListInput {
    #[schemars(range(max = 100000))]
    pub cursor: Option<u32>,
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u32>,
    #[schemars(length(min = 1, max = 200))]
    pub query: Option<String>,
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}$"))]
    pub from: Option<String>,
    #[schemars(pattern(r"^\d{4}-\d{2}-\d{2}$"))]
    pub through: Option<String>,
    pub status: Option<TrackedStatusFilter>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackedEvent {
    pub event_hash: String,
    /// The event's resource name in the primary calendar, when it is there.
    pub event_id: String,
    pub source_email_id: String,
    pub title: String,
    pub start_date: String,
    pub start_time: Option<String>,
    pub end_date: Option<String>,
    pub end_time: Option<String>,
    pub all_day: bool,
    pub location: Option<String>,
    pub time_zone: Option<String>,
    pub created_at: String,
    /// `active` or `cancelled`.
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackedListOutput {
    pub items: Vec<TrackedEvent>,
    pub next_cursor: Option<u32>,
    pub total: u64,
}
