//! The tools' output values. The same types derive the served output schemas
//! (`mcp::defs`), so a result can never drift from its contract.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Privacy {
    Private,
    Unlisted,
    Public,
}

impl Privacy {
    pub fn as_str(self) -> &'static str {
        match self {
            Privacy::Private => "private",
            Privacy::Unlisted => "unlisted",
            Privacy::Public => "public",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "private" => Some(Privacy::Private),
            "unlisted" => Some(Privacy::Unlisted),
            "public" => Some(Privacy::Public),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct Playlist {
    pub playlist_id: String,
    pub title: String,
    pub description: String,
    pub privacy: Option<Privacy>,
    #[schemars(description = "Videos in the playlist, as YouTube counts them")]
    pub item_count: Option<u64>,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub published_at: Option<String>,
    pub thumbnail_url: Option<String>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct PlaylistItem {
    #[schemars(description = "The entry's id; remove and move take this id")]
    pub playlist_item_id: String,
    #[schemars(description = "Zero-based position in the playlist")]
    pub position: Option<u64>,
    pub video_id: Option<String>,
    pub title: String,
    #[schemars(
        description = "False for deleted or private videos, which YouTube lists without details"
    )]
    pub available: bool,
    #[schemars(description = "The video's channel")]
    pub video_channel_id: Option<String>,
    pub video_channel_title: Option<String>,
    pub video_published_at: Option<String>,
    #[schemars(description = "When the video was added to the playlist")]
    pub added_at: Option<String>,
    pub note: Option<String>,
    pub privacy: Option<Privacy>,
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct LiveDetails {
    pub scheduled_start: Option<String>,
    pub actual_start: Option<String>,
    pub actual_end: Option<String>,
    pub concurrent_viewers: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct Video {
    pub video_id: String,
    pub title: String,
    #[schemars(description = "The description, cut at 3000 characters")]
    pub description: String,
    pub description_truncated: bool,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub published_at: Option<String>,
    pub duration_seconds: Option<u64>,
    #[schemars(description = "hd or sd")]
    pub definition: Option<String>,
    #[schemars(
        description = "Whether the video has captions. Caption text is not available through this integration"
    )]
    pub has_captions: Option<bool>,
    pub privacy: Option<Privacy>,
    pub embeddable: Option<bool>,
    pub made_for_kids: Option<bool>,
    #[schemars(description = "none, live or upcoming")]
    pub live_broadcast_content: Option<String>,
    pub live: Option<LiveDetails>,
    pub view_count: Option<u64>,
    pub like_count: Option<u64>,
    pub comment_count: Option<u64>,
    #[schemars(description = "At most 30 tags")]
    pub tags: Vec<String>,
    pub category_id: Option<String>,
    pub default_language: Option<String>,
    pub default_audio_language: Option<String>,
    pub thumbnail_url: Option<String>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct Channel {
    pub channel_id: String,
    pub title: String,
    #[schemars(description = "The channel's handle or custom URL, such as @name")]
    pub handle: Option<String>,
    #[schemars(description = "The description, cut at 2000 characters")]
    pub description: String,
    pub published_at: Option<String>,
    pub country: Option<String>,
    #[schemars(description = "Null when the channel hides it; YouTube rounds it")]
    pub subscriber_count: Option<u64>,
    pub video_count: Option<u64>,
    pub view_count: Option<u64>,
    #[schemars(
        description = "The channel's public uploads playlist; list it with youtube_playlist_items_list"
    )]
    pub uploads_playlist_id: Option<String>,
    pub thumbnail_url: Option<String>,
    pub url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchType {
    Video,
    Channel,
    Playlist,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct SearchResult {
    #[serde(rename = "type")]
    pub kind: SearchType,
    #[schemars(description = "The video, channel or playlist id")]
    pub id: String,
    pub title: String,
    pub description: String,
    pub channel_id: Option<String>,
    pub channel_title: Option<String>,
    pub published_at: Option<String>,
    #[schemars(description = "none, live or upcoming")]
    pub live_broadcast_content: Option<String>,
    #[schemars(description = "Videos only")]
    pub duration_seconds: Option<u64>,
    #[schemars(description = "Videos only")]
    pub view_count: Option<u64>,
    pub thumbnail_url: Option<String>,
    pub url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    NotConfigured,
    NotConnected,
    Connected,
    ReconnectRequired,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct QuotaStatus {
    #[schemars(description = "YouTube's quota day (America/Los_Angeles), YYYY-MM-DD")]
    pub day: String,
    #[schemars(
        description = "Shared-pool units of Omni's own requests today (reads 1, writes 50; searches excluded). An estimate of what this app sent, not the project's total: other apps of the same Google Cloud project also count, and only the Cloud console shows actual usage"
    )]
    pub estimated_units_used: u64,
    #[schemars(
        description = "Google's default daily units for every method except search.list and videos.insert"
    )]
    pub default_daily_units: u64,
    #[schemars(description = "search.list calls Omni sent today; search has its own bucket")]
    pub search_calls: u64,
    #[schemars(description = "Google's default daily search.list calls")]
    pub default_daily_searches: u64,
    #[schemars(description = "Midnight Pacific time (daylight saving included), when both reset")]
    pub resets_at: String,
    #[schemars(description = "Requests today per API method that got a response")]
    pub calls: BTreeMap<String, u64>,
    #[schemars(description = "When YouTube reported the shared pool exhausted today")]
    pub quota_exceeded_at: Option<String>,
    #[schemars(description = "When YouTube reported the search bucket exhausted today")]
    pub search_quota_exceeded_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct ConnectedChannel {
    pub channel_id: String,
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct Status {
    pub state: ConnectionState,
    #[schemars(description = "Open on the home network to connect or reconnect the account")]
    pub connect_url: String,
    pub channel: Option<ConnectedChannel>,
    pub scopes: Vec<String>,
    pub connected_at: Option<String>,
    #[schemars(description = "Why the connection needs attention")]
    pub problem: Option<String>,
    pub quota: QuotaStatus,
    #[schemars(description = "What the YouTube Data API does not give access to")]
    pub not_available: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct PlaylistsPage {
    pub playlists: Vec<Playlist>,
    #[schemars(description = "Pass as pageToken for the next page; null on the last page")]
    pub next_page_token: Option<String>,
    #[schemars(
        description = "YouTube's reported count, which can exceed the playlists it returns across all pages; it is not a count of listable playlists"
    )]
    pub total_results: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct PlaylistItemsPage {
    pub playlist_id: String,
    pub items: Vec<PlaylistItem>,
    #[schemars(description = "Pass as pageToken for the next page; null on the last page")]
    pub next_page_token: Option<String>,
    pub total_results: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CreateOutcome {
    Created,
    AlreadyCreated,
    Recorded,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct CreateOutput {
    pub outcome: CreateOutcome,
    pub playlist: Option<Playlist>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateOutcome {
    Updated,
    Unchanged,
    Recorded,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct UpdateOutput {
    pub outcome: UpdateOutcome,
    #[schemars(description = "The fields that changed: title, description, privacy")]
    pub changed: Vec<String>,
    pub playlist: Option<Playlist>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeleteOutcome {
    Deleted,
    AlreadyDeleted,
    Recorded,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct DeleteOutput {
    pub outcome: DeleteOutcome,
    pub playlist_id: String,
    pub title: String,
    pub item_count: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AddOutcome {
    Added,
    AlreadyPresent,
    NotFound,
    Uncertain,
    Failed,
    NotAttempted,
    Recorded,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct AddResult {
    pub video_id: String,
    pub outcome: AddOutcome,
    pub playlist_item_id: Option<String>,
    pub title: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct ItemsAddOutput {
    pub playlist_id: String,
    pub added: u32,
    pub results: Vec<AddResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoveOutcome {
    Removed,
    NotFound,
    Uncertain,
    Failed,
    NotAttempted,
    Recorded,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct RemoveResult {
    pub playlist_item_id: Option<String>,
    pub video_id: Option<String>,
    pub title: Option<String>,
    pub outcome: RemoveOutcome,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct ItemsRemoveOutput {
    pub playlist_id: String,
    pub removed: u32,
    pub results: Vec<RemoveResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MoveOutcome {
    Moved,
    Unchanged,
    Recorded,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct MoveOutput {
    pub outcome: MoveOutcome,
    pub playlist_item_id: String,
    pub playlist_id: String,
    pub video_id: String,
    pub title: String,
    pub from: Option<u64>,
    pub to: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct SearchOutput {
    pub query: String,
    #[serde(rename = "type")]
    pub kind: SearchType,
    pub results: Vec<SearchResult>,
    pub next_page_token: Option<String>,
    #[schemars(description = "YouTube's rough estimate")]
    pub total_results: Option<u64>,
    #[schemars(description = "Searches this call used from the separate daily search bucket")]
    pub search_calls: u64,
    #[schemars(
        description = "Shared-pool units this call used (the videos.list for durations and views)"
    )]
    pub pool_units: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct VideosOutput {
    pub videos: Vec<Video>,
    #[schemars(description = "Requested ids YouTube did not return (deleted, private or wrong)")]
    pub missing: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct ChannelsOutput {
    pub channels: Vec<Channel>,
    pub missing: Vec<String>,
}
