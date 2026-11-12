//! The `youtube_*` MCP tools: thin adapters over [`Youtube`].

pub mod defs;

use std::future::Future;
use std::time::Duration;

use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolMetaError, typed_tool};

use crate::error::YoutubeError;
use crate::ids;
use crate::model::{Privacy, SearchType};
use crate::service::{
    ChannelLookup, CreatePlaylist, PlaylistScope, RemoveTarget, SearchParams, UpdatePlaylist,
    Youtube,
};
use crate::transcripts::model::{
    TranscriptInfo, TranscriptRead, TranscriptSearchOutput, TranscriptsList,
};
use crate::transcripts::text::{self, PageRequest};
use crate::transcripts::{RETENTION_MS, TranscriptError, Transcripts};

/// `[code] message`, so clients can tell quota, connection and input failures apart.
fn tool_error(error: &YoutubeError) -> ToolError {
    let message = format!("[{}] {error}", error.code());
    match error {
        YoutubeError::Invalid(_) => ToolError::input(message),
        _ => ToolError::execute(message),
    }
}

fn input(message: impl Into<String>) -> ToolError {
    ToolError::input(format!("[invalid_input] {}", message.into()))
}

fn parse_all(
    raw: &[String],
    parse: fn(&str) -> Result<String, YoutubeError>,
) -> Result<Vec<String>, ToolError> {
    raw.iter()
        .map(|r| parse(r).map_err(|e| tool_error(&e)))
        .collect()
}

fn rfc3339(field: &str, raw: Option<String>) -> Result<Option<String>, ToolError> {
    match raw {
        None => Ok(None),
        Some(raw) => raw
            .parse::<jiff::Timestamp>()
            .map(|t| Some(t.to_string()))
            .map_err(|_| {
                input(format!(
                    "{field}: expected an RFC 3339 time such as 2026-01-01T00:00:00Z"
                ))
            }),
    }
}

fn tool<I, O, F, Fut>(
    def: &'static omni_mcp_kit::ToolDef<I, O>,
    youtube: &Youtube,
    f: F,
) -> Result<McpTool, ToolMetaError>
where
    I: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    O: schemars::JsonSchema + serde::Serialize + Send + 'static,
    F: Fn(Youtube, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
{
    let youtube = youtube.clone();
    typed_tool(def, move |input: I, _cx: ToolContext| {
        f(youtube.clone(), input)
    })
}

fn transcript_error(error: &TranscriptError) -> ToolError {
    ToolError::execute(format!("[{}] {error}", error.code()))
}

fn transcript_tool<I, O, F, Fut>(
    def: &'static omni_mcp_kit::ToolDef<I, O>,
    transcripts: &Transcripts,
    f: F,
) -> Result<McpTool, ToolMetaError>
where
    I: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    O: schemars::JsonSchema + serde::Serialize + Send + 'static,
    F: Fn(Transcripts, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
{
    let transcripts = transcripts.clone();
    typed_tool(def, move |input: I, _cx: ToolContext| {
        f(transcripts.clone(), input)
    })
}

const DEFAULT_MAX_CHARS: u32 = 30_000;

/// The `youtube_transcript*` tools.
pub fn transcript_tools(transcripts: &Transcripts) -> Result<Vec<McpTool>, ToolMetaError> {
    Ok(vec![
        transcript_tool(
            &defs::YOUTUBE_TRANSCRIPT_START,
            transcripts,
            |transcripts, i: defs::TranscriptStartInput| async move {
                let id = ids::video_id(&i.video).map_err(|e| tool_error(&e))?;
                let mut row = transcripts
                    .start(&id, i.force.unwrap_or(false))
                    .await
                    .map_err(|e| transcript_error(&e))?;
                let wait = i.wait_seconds.unwrap_or(25);
                if row.state.is_active() && wait > 0 {
                    row = transcripts
                        .wait(&id, Duration::from_secs(u64::from(wait)))
                        .await
                        .map_err(|e| transcript_error(&e))?;
                }
                // Only a finished transcript's text: partial text is read
                // on purpose, with youtube_transcript_get.
                let page = if row.state.is_active() {
                    text::Page::default()
                } else {
                    text::page(
                        &row.segments,
                        &text::flags(&row),
                        PageRequest {
                            cursor: 0,
                            start_ms: None,
                            end_ms: None,
                            max_chars: i.max_chars.unwrap_or(DEFAULT_MAX_CHARS) as usize,
                        },
                    )
                };
                Ok(TranscriptRead::of(&row, page))
            },
        )?,
        transcript_tool(
            &defs::YOUTUBE_TRANSCRIPT_GET,
            transcripts,
            |transcripts, i: defs::TranscriptGetInput| async move {
                let id = ids::video_id(&i.video).map_err(|e| tool_error(&e))?;
                if let (Some(start), Some(end)) = (i.start_seconds, i.end_seconds)
                    && start >= end
                {
                    return Err(input("startSeconds must be before endSeconds"));
                }
                let row = match i.wait_seconds.unwrap_or(0) {
                    0 => transcripts.get(&id).await,
                    wait => {
                        transcripts
                            .wait(&id, Duration::from_secs(u64::from(wait)))
                            .await
                    }
                }
                .map_err(|e| transcript_error(&e))?;
                let page = text::page(
                    &row.segments,
                    &text::flags(&row),
                    PageRequest {
                        cursor: i.cursor.unwrap_or(0) as usize,
                        start_ms: i.start_seconds.map(|s| i64::from(s) * 1000),
                        end_ms: i.end_seconds.map(|s| i64::from(s) * 1000),
                        max_chars: i.max_chars.unwrap_or(DEFAULT_MAX_CHARS) as usize,
                    },
                );
                Ok(TranscriptRead::of(&row, page))
            },
        )?,
        transcript_tool(
            &defs::YOUTUBE_TRANSCRIPT_SEARCH,
            transcripts,
            |transcripts, i: defs::TranscriptSearchInput| async move {
                let id = ids::video_id(&i.video).map_err(|e| tool_error(&e))?;
                if !i.query.chars().any(char::is_alphanumeric) {
                    return Err(input("query: give at least one word"));
                }
                let row = transcripts
                    .get(&id)
                    .await
                    .map_err(|e| transcript_error(&e))?;
                let (hits, total) =
                    text::search(&row.segments, &i.query, i.limit.unwrap_or(10) as usize);
                Ok(TranscriptSearchOutput::of(&row, hits, total))
            },
        )?,
        transcript_tool(
            &defs::YOUTUBE_TRANSCRIPTS_LIST,
            transcripts,
            |transcripts, i: defs::TranscriptsListInput| async move {
                let (rows, total) = transcripts
                    .list(i.limit.unwrap_or(20) as usize)
                    .await
                    .map_err(|e| transcript_error(&e))?;
                Ok(TranscriptsList {
                    transcripts: rows.iter().map(TranscriptInfo::of).collect(),
                    total: total as u64,
                    retention_days: u64::try_from(RETENTION_MS / 86_400_000).unwrap_or(0),
                    speech_available: transcripts.speech_available(),
                })
            },
        )?,
    ])
}

pub fn tools(youtube: &Youtube, transcripts: &Transcripts) -> Result<Vec<McpTool>, ToolMetaError> {
    let mut tools = account_tools(youtube)?;
    tools.extend(transcript_tools(transcripts)?);
    Ok(tools)
}

fn account_tools(youtube: &Youtube) -> Result<Vec<McpTool>, ToolMetaError> {
    Ok(vec![
        tool(
            &defs::YOUTUBE_STATUS,
            youtube,
            |yt, _: defs::StatusInput| async move { yt.status().await.map_err(|e| tool_error(&e)) },
        )?,
        tool(
            &defs::YOUTUBE_PLAYLISTS_LIST,
            youtube,
            |yt, i: defs::PlaylistsListInput| async move {
                let scope = match (i.channel_id, i.playlist_ids) {
                    (Some(_), Some(_)) => {
                        return Err(input("give channelId or playlistIds, not both"));
                    }
                    (Some(channel), None) => PlaylistScope::Channel(
                        ids::channel_id(&channel).map_err(|e| tool_error(&e))?,
                    ),
                    (None, Some(list)) => PlaylistScope::Ids(parse_all(&list, ids::playlist_id)?),
                    (None, None) => PlaylistScope::Mine,
                };
                yt.playlists(scope, i.page_token, i.limit.unwrap_or(25))
                    .await
                    .map_err(|e| tool_error(&e))
            },
        )?,
        tool(
            &defs::YOUTUBE_PLAYLIST_ITEMS_LIST,
            youtube,
            |yt, i: defs::PlaylistItemsListInput| async move {
                let id = ids::playlist_id(&i.playlist_id).map_err(|e| tool_error(&e))?;
                yt.playlist_items(&id, i.page_token, i.limit.unwrap_or(50))
                    .await
                    .map_err(|e| tool_error(&e))
            },
        )?,
        tool(
            &defs::YOUTUBE_PLAYLIST_CREATE,
            youtube,
            |yt, i: defs::PlaylistCreateInput| async move {
                let title = i.title.trim().to_owned();
                if title.is_empty() {
                    return Err(input("title: must not be blank"));
                }
                yt.create_playlist(CreatePlaylist {
                    title,
                    description: i.description.unwrap_or_default(),
                    privacy: i.privacy.unwrap_or(Privacy::Private),
                    idempotency_key: i.idempotency_key,
                    allow_duplicate_title: i.allow_duplicate_title.unwrap_or(false),
                })
                .await
                .map_err(|e| tool_error(&e))
            },
        )?,
        tool(
            &defs::YOUTUBE_PLAYLIST_UPDATE,
            youtube,
            |yt, i: defs::PlaylistUpdateInput| async move {
                let playlist_id = ids::playlist_id(&i.playlist_id).map_err(|e| tool_error(&e))?;
                let title = i.title.map(|t| t.trim().to_owned());
                if title.as_deref() == Some("") {
                    return Err(input("title: must not be blank"));
                }
                if title.is_none() && i.description.is_none() && i.privacy.is_none() {
                    return Err(input("give at least one of title, description or privacy"));
                }
                yt.update_playlist(UpdatePlaylist {
                    playlist_id,
                    title,
                    description: i.description,
                    privacy: i.privacy,
                })
                .await
                .map_err(|e| tool_error(&e))
            },
        )?,
        tool(
            &defs::YOUTUBE_PLAYLIST_DELETE,
            youtube,
            |yt, i: defs::PlaylistDeleteInput| async move {
                let id = ids::playlist_id(&i.playlist_id).map_err(|e| tool_error(&e))?;
                yt.delete_playlist(&id, &i.confirm_title)
                    .await
                    .map_err(|e| tool_error(&e))
            },
        )?,
        tool(
            &defs::YOUTUBE_PLAYLIST_ITEMS_ADD,
            youtube,
            |yt, i: defs::PlaylistItemsAddInput| async move {
                let playlist = ids::playlist_id(&i.playlist_id).map_err(|e| tool_error(&e))?;
                let videos = parse_all(&i.video_ids, ids::video_id)?;
                yt.add_items(&playlist, videos, i.position.map(u64::from))
                    .await
                    .map_err(|e| tool_error(&e))
            },
        )?,
        tool(
            &defs::YOUTUBE_PLAYLIST_ITEMS_REMOVE,
            youtube,
            |yt, i: defs::PlaylistItemsRemoveInput| async move {
                let playlist = ids::playlist_id(&i.playlist_id).map_err(|e| tool_error(&e))?;
                let target = match (i.playlist_item_ids, i.video_ids) {
                    (Some(items), None) => {
                        RemoveTarget::Items(parse_all(&items, ids::playlist_item_id)?)
                    }
                    (None, Some(videos)) => {
                        RemoveTarget::Videos(parse_all(&videos, ids::video_id)?)
                    }
                    _ => return Err(input("give exactly one of playlistItemIds or videoIds")),
                };
                yt.remove_items(&playlist, target)
                    .await
                    .map_err(|e| tool_error(&e))
            },
        )?,
        tool(
            &defs::YOUTUBE_PLAYLIST_ITEM_MOVE,
            youtube,
            |yt, i: defs::PlaylistItemMoveInput| async move {
                let id = ids::playlist_item_id(&i.playlist_item_id).map_err(|e| tool_error(&e))?;
                yt.move_item(&id, u64::from(i.position))
                    .await
                    .map_err(|e| tool_error(&e))
            },
        )?,
        tool(
            &defs::YOUTUBE_SEARCH,
            youtube,
            |yt, i: defs::SearchInput| async move {
                let query = i.query.trim().to_owned();
                if query.is_empty() {
                    return Err(input("query: must not be blank"));
                }
                let kind = i.kind.unwrap_or(SearchType::Video);
                if i.duration.is_some() && kind != SearchType::Video {
                    return Err(input("duration applies only to type video"));
                }
                let channel_id = match i.channel_id {
                    Some(raw) => Some(ids::channel_id(&raw).map_err(|e| tool_error(&e))?),
                    None => None,
                };
                yt.search(SearchParams {
                    query,
                    kind,
                    order: i.order.map(|o| o.as_str().to_owned()),
                    channel_id,
                    published_after: rfc3339("publishedAfter", i.published_after)?,
                    published_before: rfc3339("publishedBefore", i.published_before)?,
                    duration: i.duration.map(|d| d.as_str().to_owned()),
                    region_code: i.region_code.map(|r| r.to_ascii_uppercase()),
                    relevance_language: i.relevance_language,
                    limit: i.limit.unwrap_or(10),
                    page_token: i.page_token,
                })
                .await
                .map_err(|e| tool_error(&e))
            },
        )?,
        tool(
            &defs::YOUTUBE_VIDEOS_GET,
            youtube,
            |yt, i: defs::VideosGetInput| async move {
                let mut videos = Vec::new();
                for id in parse_all(&i.video_ids, ids::video_id)? {
                    if !videos.contains(&id) {
                        videos.push(id);
                    }
                }
                yt.videos(videos).await.map_err(|e| tool_error(&e))
            },
        )?,
        tool(
            &defs::YOUTUBE_CHANNELS_GET,
            youtube,
            |yt, i: defs::ChannelsGetInput| async move {
                let lookup = match (i.channel_ids, i.handle, i.mine.unwrap_or(false)) {
                    (Some(list), None, false) => {
                        ChannelLookup::Ids(parse_all(&list, ids::channel_id)?)
                    }
                    (None, Some(handle), false) => {
                        ChannelLookup::Handle(ids::handle(&handle).map_err(|e| tool_error(&e))?)
                    }
                    (None, None, true) => ChannelLookup::Mine,
                    _ => {
                        return Err(input(
                            "give exactly one of channelIds, handle or mine: true",
                        ));
                    }
                };
                yt.channels(lookup).await.map_err(|e| tool_error(&e))
            },
        )?,
    ])
}
