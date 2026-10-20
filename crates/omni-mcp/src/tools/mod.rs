//! The MCP tools this package owns and the serving order of every package's tools.

pub mod claude_sessions;
pub mod events;
pub mod shell;
pub mod system;

use futures::future::BoxFuture;
use omni_api::tasks::TaskInfo;
use omni_mcp_kit::{ToolDefinition, ToolError};
use omni_store::LogLine;
use omni_tasks::TaskRegistry;
use omni_tasks::persistence::TaskRunData;
use serde_json::Value;

use crate::json::order_by_schema;

/// Every tool's name in `tools/list` order. The served set must equal this
/// list; each tool's contract lives in its package's `defs` module.
pub const TOOL_ORDER: [&str; 121] = [
    "list_reminder_lists",
    "get_reminder_list",
    "update_reminder_list",
    "get_reminder_recurrence",
    "create_reminder_recurrence",
    "update_reminder_recurrence",
    "remove_reminder_recurrence",
    "complete_recurring_reminder",
    "list_reminders",
    "get_reminder",
    "create_reminder",
    "update_reminder",
    "complete_reminder",
    "reopen_reminder",
    "delete_reminder",
    "system_status",
    "tasks_list",
    "task_run",
    "task_runs_list",
    "task_run_get",
    "livestreams_list",
    "livestream_get",
    "streamer_configs_list",
    "streamer_config_create",
    "streamer_config_update",
    "streamer_config_delete",
    "streamer_configs_reorder",
    "livestream_settings_update",
    "email_search",
    "email_get",
    "email_health",
    "email_activity_list",
    "email_activity_get",
    "email_reprocess",
    "email_rules_list",
    "email_rules_upsert",
    "email_rules_delete",
    "email_feedback_list",
    "email_feedback_set",
    "email_retry_list",
    "email_retry_clear",
    "calendar_status",
    "calendar_events_list",
    "calendar_events_search",
    "calendar_event_get",
    "calendar_event_preview",
    "calendar_write_status",
    "calendar_changes_list",
    "calendar_tracked_events_list",
    "calendar_event_create",
    "calendar_event_update",
    "calendar_event_delete",
    "email_draft_create",
    "email_send",
    "email_send_status",
    "email_sent_copy_repair",
    "email_archive_queue",
    "email_archive_status",
    "email_archive_cancel",
    "email_archive_restore",
    "events_status",
    "email_attachment_get",
    "media_catalog_search",
    "media_catalog_get",
    "media_catalog_browse",
    "media_library_list",
    "media_watchlist_list",
    "media_watchlist_add",
    "media_series_get",
    "media_series_add",
    "media_series_seasons_set",
    "media_series_search",
    "media_recommendations_list",
    "media_recommendation_get",
    "media_recommendation_feedback",
    "media_taste_read",
    "podcast_account_list",
    "podcast_account_search",
    "podcast_account_update",
    "podcast_recommendations_list",
    "podcast_recommendation_get",
    "podcast_recommendation_feedback",
    "podcast_taste_read",
    "presspods_list",
    "presspods_episode_get",
    "presspods_transcript_read",
    "presspods_submit",
    "presspods_retry",
    "presspods_delete",
    "parcels_list",
    "parcels_get",
    "pets_read",
    "costs_read",
    "get_printer_status",
    "print_document",
    "search_browser_history",
    "browse_browser_history",
    "get_browser_page",
    "set_browser_page_label",
    "notify_owner_blocked",
    "notify_owner_requested",
    "stylist_context_get",
    "stylist_records_query",
    "stylist_measurements_compare",
    "stylist_record_save",
    "stylist_event_append",
    "stylist_evidence_store",
    "stylist_evidence_get",
    "stylist_export",
    "stylist_archives_list",
    "stylist_restore",
    "stylist_fixture_purge",
    "claude_link_status",
    "claude_sessions_list",
    "claude_session_get",
    "claude_session_read",
    "claude_session_start",
    "claude_session_send",
    "claude_session_stop",
    "shell",
    "shell_usage",
];

/// A run with its log lines and dropped-line count.
pub type RunWithLogs = (TaskRunData, Vec<LogLine>, u64);

/// The task registry as the system tools see it (a seam for tests).
pub trait TaskControl: Send + Sync {
    fn list(&self) -> BoxFuture<'_, Result<Vec<TaskInfo>, String>>;
    /// Queues a manual run; the error is the user-facing message.
    fn run_now(&self, name: &str, input: Option<Value>) -> Result<String, String>;
    fn recent_runs<'a>(
        &'a self,
        task: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<TaskRunData>, String>>;
    /// A run with its log lines (live buffer first) and dropped count.
    fn run_logs<'a>(
        &'a self,
        run_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<RunWithLogs>, String>>;
}

impl TaskControl for TaskRegistry {
    fn list(&self) -> BoxFuture<'_, Result<Vec<TaskInfo>, String>> {
        Box::pin(async move { TaskRegistry::list(self).await.map_err(|e| e.to_string()) })
    }

    fn run_now(&self, name: &str, input: Option<Value>) -> Result<String, String> {
        TaskRegistry::run_now(self, name, input).map_err(|error| error.to_string())
    }

    fn recent_runs<'a>(
        &'a self,
        task: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<TaskRunData>, String>> {
        Box::pin(async move {
            TaskRegistry::recent_runs(self, task, limit)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn run_logs<'a>(
        &'a self,
        run_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<RunWithLogs>, String>> {
        Box::pin(async move {
            TaskRegistry::run_logs(self, run_id)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// Projects a value onto the tool's output schema: schema key
/// order, unknown keys stripped.
pub(crate) fn conform(tool: &dyn ToolDefinition, value: Value) -> Result<Value, ToolError> {
    let meta = tool.meta().map_err(|e| ToolError::output(e.to_string()))?;
    Ok(order_by_schema(
        value,
        &Value::Object((*meta.output_schema).clone()),
    ))
}

/// `truncate()`: the first `max` UTF-16 units and whether anything was cut.
pub(crate) fn truncate(text: &str, max: usize) -> (String, bool) {
    omni_mcp_kit::truncate_utf16(text, max)
}
