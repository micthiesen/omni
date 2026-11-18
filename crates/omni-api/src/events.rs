//! MCP Events payloads published by subsystems through the
//! `omni_runtime::ports::EventPublisher` port (`docs/mcp-events.md`).
//!
//! Each payload carries identifiers, state and at most one short title; read
//! details through the event's polling tools. Timestamps are RFC 3339 UTC
//! strings. Nullable fields are always present.

use serde::{Deserialize, Serialize};

pub const LIVESTREAM_STATUS_CHANGED: &str = "livestream.status_changed";
pub const PRESSPODS_JOB_FINISHED: &str = "presspods.job_finished";
pub const TASK_RUN_FINISHED: &str = "task.run_finished";
pub const CALENDAR_EVENT_CHANGED: &str = "calendar.event_changed";
pub const CALENDAR_EVENT_STARTING: &str = "calendar.event_starting";
pub const RECIPES_INBOX_ADDED: &str = "recipes.inbox_added";
pub const RECIPES_COOK_FINISHED: &str = "recipes.cook_finished";

/// Longest title, in characters, any event payload carries.
pub const TITLE_LIMIT: usize = 200;

/// `text` trimmed and cut to [`TITLE_LIMIT`] characters; `None` when empty.
pub fn bounded_title(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.chars().take(TITLE_LIMIT).collect())
}

/// `text` trimmed and cut to [`TITLE_LIMIT`] characters, plus whether it was
/// cut; `None` when empty.
pub fn bounded_title_flagged(text: &str) -> (Option<String>, bool) {
    let trimmed = text.trim();
    (
        bounded_title(trimmed),
        trimmed.chars().count() > TITLE_LIMIT,
    )
}

/// An aggregate livestream edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LivestreamTransition {
    #[serde(rename = "went_live")]
    WentLive,
    #[serde(rename = "went_offline")]
    WentOffline,
}

impl LivestreamTransition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WentLive => "went_live",
            Self::WentOffline => "went_offline",
        }
    }
}

/// `livestream.status_changed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LivestreamStatusChanged {
    pub streamer_id: String,
    pub display_name: String,
    pub transition: LivestreamTransition,
    pub tier: crate::streamers::StreamerTier,
    /// The session's primary platform.
    pub platform: String,
    pub title: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub viewer_count: Option<i64>,
    pub max_viewer_count: Option<i64>,
}

/// How a PressPods job ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresspodsOutcome {
    Published,
    Failed,
}

/// `presspods.job_finished`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PresspodsJobFinished {
    pub outcome: PresspodsOutcome,
    pub episode_id: Option<String>,
    pub job_id: Option<String>,
    pub title: Option<String>,
    /// The article URL's hostname only.
    pub article_url_host: Option<String>,
    pub duration_seconds: Option<f64>,
    pub attempts: Option<i64>,
}

/// A finished run's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskRunOutcome {
    Success,
    Error,
    /// Completed but skipped its real work because an upstream failed.
    Degraded,
}

/// `task.run_finished`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRunFinished {
    pub run_id: String,
    pub task_name: String,
    pub trigger: crate::runs::RunTrigger,
    pub status: TaskRunOutcome,
    pub started_at: String,
    pub finished_at: String,
}

/// How a primary-calendar resource changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarChangeKind {
    Created,
    Updated,
    Deleted,
}

/// Who wrote a calendar change: Omni's calendar tools or anything else
/// (devices, other clients, the email pipeline).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarChangeOrigin {
    Omni,
    External,
}

/// `calendar.event_changed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEventChanged {
    pub event_id: String,
    pub uid: Option<String>,
    pub change_kind: CalendarChangeKind,
    pub summary: Option<String>,
    pub summary_truncated: bool,
    /// The next occurrence's start at or after detection, else the first.
    pub start: Option<String>,
    pub all_day: bool,
    pub recurring: bool,
    /// Empty for created and deleted.
    pub changed_fields: Vec<String>,
    /// The new ETag; `None` for a deletion.
    pub version: Option<String>,
    pub origin: CalendarChangeOrigin,
    pub detected_at: String,
}

/// What a `calendar.event_starting` publication fired for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalendarStartTrigger {
    Start,
    Alarm,
}

/// `calendar.event_starting`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEventStarting {
    pub event_id: String,
    pub uid: String,
    /// The occurrence's original start key; `None` for a single event.
    pub recurrence_id: Option<String>,
    pub summary: Option<String>,
    pub summary_truncated: bool,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub time_zone: Option<String>,
    pub trigger: CalendarStartTrigger,
    /// The subscription's lead (`"15"`) for a start trigger.
    pub lead_minutes: Option<String>,
    /// The alarm's identity for an alarm trigger.
    pub alarm_id: Option<String>,
    pub fire_at: String,
    /// Published after its fire time because Omni was not running then.
    pub late: bool,
    pub has_location: bool,
    /// The subscription tuple's `includeAllDay` (`"true"` or `"false"`).
    pub include_all_day: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_trimmed_and_bounded() {
        assert_eq!(bounded_title("  "), None);
        assert_eq!(bounded_title(" Hi "), Some("Hi".to_owned()));
        let long = "é".repeat(TITLE_LIMIT + 5);
        assert_eq!(
            bounded_title(&long).map(|t| t.chars().count()),
            Some(TITLE_LIMIT)
        );
    }
}

/// `recipes.inbox_added`: captured material is waiting for an agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipesInboxAdded {
    pub item_id: String,
    /// `photo`, `url` or `text`.
    pub kind: String,
    pub title: Option<String>,
}

/// `recipes.cook_finished`: a cook session ended and counted as a cook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipesCookFinished {
    pub session_id: String,
    pub recipe_id: String,
    pub title: Option<String>,
    /// Adjustments on the recipe that no revision has folded in yet.
    pub open_adjustments: u32,
}
