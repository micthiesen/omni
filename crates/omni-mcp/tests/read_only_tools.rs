//! Which tools declare `readOnlyHint: true`. Unattended clients rely on it:
//! Rakazo's change-trigger routines run only tools marked read-only, and
//! approval policies relax for them. A tool is read-only only when a call
//! changes nothing anywhere (no write, send, queue, session or reservation);
//! a remote query that spends quota still counts as a read.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::collections::BTreeSet;

/// Every tool served with `readOnlyHint: true`, in serving order.
const READ_ONLY: [&str; 104] = [
    "list_reminder_lists",
    "get_reminder_list",
    "get_reminder_recurrence",
    "list_reminders",
    "get_reminder",
    "system_status",
    "tasks_list",
    "task_runs_list",
    "task_run_get",
    "livestreams_list",
    "livestream_get",
    "streamer_configs_list",
    "email_search",
    "email_get",
    "email_thread_get",
    "email_health",
    "email_activity_list",
    "email_activity_get",
    "email_rules_list",
    "email_feedback_list",
    "email_retry_list",
    "calendar_status",
    "calendar_events_list",
    "calendar_events_search",
    "calendar_event_get",
    "calendar_event_preview",
    "calendar_write_status",
    "calendar_changes_list",
    "calendar_tracked_events_list",
    "contacts_status",
    "contacts_search",
    "contacts_contact_get",
    "contacts_groups_list",
    "contacts_change_preview",
    "contacts_write_status",
    "email_send_status",
    "email_archive_status",
    "events_status",
    "email_attachment_get",
    "media_catalog_search",
    "media_catalog_get",
    "media_catalog_browse",
    "media_apple_music_resolve",
    "media_library_list",
    "media_watchlist_list",
    "media_series_get",
    "media_arr_profiles",
    "media_arr_status",
    "media_downloads_list",
    "media_nzbget_status",
    "media_movies_get",
    "media_movie_collections",
    "media_movie_releases",
    "archive_search",
    "library_additions_list",
    "library_items_list",
    "media_recommendations_list",
    "media_recommendation_get",
    "media_taste_read",
    "youtube_status",
    "youtube_playlists_list",
    "youtube_playlist_items_list",
    "youtube_search",
    "youtube_videos_get",
    "youtube_channels_get",
    "youtube_transcript_get",
    "youtube_transcript_search",
    "youtube_transcripts_list",
    "podcast_account_list",
    "podcast_account_search",
    "podcast_recommendations_list",
    "podcast_recommendation_get",
    "podcast_taste_read",
    "podcast_transcript_search",
    "podcast_transcript_get",
    "presspods_list",
    "presspods_episode_get",
    "presspods_transcript_read",
    "pets_read",
    "costs_read",
    "get_printer_status",
    "search_browser_history",
    "browse_browser_history",
    "get_browser_page",
    "amazon_search",
    "amazon_product",
    "stylist_context_get",
    "stylist_records_query",
    "stylist_measurements_compare",
    "stylist_evidence_get",
    "stylist_archives_list",
    "recipes_guide",
    "recipes_search",
    "recipes_get",
    "recipes_history",
    "recipes_image_get",
    "recipes_inbox_list",
    "recipes_inbox_get",
    "recipes_cook_get",
    "recipes_requests_list",
    "claude_link_status",
    "claude_sessions_list",
    "claude_session_get",
    "claude_session_read",
];

/// Tools whose names look like reads but that change something.
const READ_NAMED_WRITES: [(&str, &str); 3] = [
    ("update_reminder_list", "renames an iCloud reminder list"),
    (
        "media_series_search",
        "starts Sonarr searches that begin downloads",
    ),
    (
        "media_movies_search",
        "starts Radarr searches that begin downloads",
    ),
];

fn read_named(name: &str) -> bool {
    const PREFIXES: [&str; 4] = ["get_", "list_", "search_", "browse_"];
    const SUFFIXES: [&str; 9] = [
        "_list",
        "_get",
        "_search",
        "_status",
        "_read",
        "_preview",
        "_query",
        "_compare",
        "_releases",
    ];
    PREFIXES.iter().any(|p| name.starts_with(p)) || SUFFIXES.iter().any(|s| name.ends_with(s))
}

#[test]
fn exactly_the_audited_tools_are_read_only() {
    let served: Vec<_> = common::all_defs()
        .into_iter()
        .map(|def| def.meta().unwrap_or_else(|e| panic!("{e}")))
        .filter(|meta| meta.annotations.read_only_hint)
        .map(|meta| meta.name.as_str())
        .collect();
    assert_eq!(
        served, READ_ONLY,
        "a tool's readOnlyHint changed: update READ_ONLY only if every call of it changes nothing"
    );
}

#[test]
fn read_only_tools_are_never_destructive() {
    for def in common::all_defs() {
        let meta = def.meta().unwrap();
        if meta.annotations.read_only_hint {
            assert!(
                !meta.annotations.destructive_hint,
                "{} is read-only and destructive",
                meta.name
            );
        }
    }
}

#[test]
fn tools_named_like_reads_are_read_only_or_listed() {
    let exceptions: BTreeSet<&str> = READ_NAMED_WRITES.iter().map(|(name, _)| *name).collect();
    for def in common::all_defs() {
        let meta = def.meta().unwrap();
        if !read_named(&meta.name) {
            continue;
        }
        let excepted = exceptions.contains(meta.name.as_str());
        assert_ne!(
            meta.annotations.read_only_hint, excepted,
            "{}: a read-named tool must be read-only, or listed in READ_NAMED_WRITES with why it writes",
            meta.name
        );
    }
}
