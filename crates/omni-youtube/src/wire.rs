//! YouTube Data API v3 response shapes (only the fields Omni reads) and their
//! conversion into the tools' values. Every field is optional: YouTube omits
//! parts for deleted, private and region-blocked items.

use serde::Deserialize;

use crate::model::{
    Channel, LiveDetails, Playlist, PlaylistItem, Privacy, SearchResult, SearchType, Video,
};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResponse<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub page_info: Option<PageInfo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageInfo {
    #[serde(default)]
    pub total_results: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Thumb {
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Thumbnails {
    #[serde(default)]
    pub default: Option<Thumb>,
    #[serde(default)]
    pub medium: Option<Thumb>,
    #[serde(default)]
    pub high: Option<Thumb>,
    #[serde(default)]
    pub standard: Option<Thumb>,
    #[serde(default)]
    pub maxres: Option<Thumb>,
}

fn best_thumbnail(thumbnails: Option<&Thumbnails>) -> Option<String> {
    let t = thumbnails?;
    [&t.high, &t.standard, &t.maxres, &t.medium, &t.default]
        .into_iter()
        .find_map(|thumb| thumb.as_ref().and_then(|t| t.url.clone()))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusPart {
    #[serde(default)]
    pub privacy_status: Option<String>,
    #[serde(default)]
    pub embeddable: Option<bool>,
    #[serde(default)]
    pub made_for_kids: Option<bool>,
}

fn privacy(status: Option<&StatusPart>) -> Option<Privacy> {
    status?.privacy_status.as_deref().and_then(Privacy::parse)
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistSnippet {
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub default_language: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub thumbnails: Option<Thumbnails>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistContent {
    #[serde(default)]
    pub item_count: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistRes {
    pub id: String,
    #[serde(default)]
    pub snippet: Option<PlaylistSnippet>,
    #[serde(default)]
    pub status: Option<StatusPart>,
    #[serde(default)]
    pub content_details: Option<PlaylistContent>,
}

impl PlaylistRes {
    pub fn title(&self) -> &str {
        self.snippet
            .as_ref()
            .and_then(|s| s.title.as_deref())
            .unwrap_or("")
    }

    pub fn channel_id(&self) -> Option<&str> {
        self.snippet.as_ref()?.channel_id.as_deref()
    }

    pub fn description(&self) -> &str {
        self.snippet
            .as_ref()
            .and_then(|s| s.description.as_deref())
            .unwrap_or("")
    }

    pub fn privacy(&self) -> Option<Privacy> {
        privacy(self.status.as_ref())
    }

    pub fn to_model(&self) -> Playlist {
        let snippet = self.snippet.as_ref();
        Playlist {
            playlist_id: self.id.clone(),
            title: self.title().to_owned(),
            description: cut(self.description(), 1000).0,
            privacy: self.privacy(),
            item_count: self.content_details.as_ref().and_then(|c| c.item_count),
            channel_id: self.channel_id().map(str::to_owned),
            channel_title: snippet.and_then(|s| s.channel_title.clone()),
            published_at: snippet.and_then(|s| s.published_at.clone()),
            thumbnail_url: best_thumbnail(snippet.and_then(|s| s.thumbnails.as_ref())),
            url: format!("https://www.youtube.com/playlist?list={}", self.id),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceId {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub video_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistItemSnippet {
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub playlist_id: Option<String>,
    #[serde(default)]
    pub position: Option<u64>,
    #[serde(default)]
    pub resource_id: Option<ResourceId>,
    #[serde(default)]
    pub video_owner_channel_title: Option<String>,
    #[serde(default)]
    pub video_owner_channel_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistItemContent {
    #[serde(default)]
    pub video_id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub video_published_at: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistItemRes {
    pub id: String,
    #[serde(default)]
    pub snippet: Option<PlaylistItemSnippet>,
    #[serde(default)]
    pub content_details: Option<PlaylistItemContent>,
    #[serde(default)]
    pub status: Option<StatusPart>,
}

impl PlaylistItemRes {
    pub fn video_id(&self) -> Option<&str> {
        self.content_details
            .as_ref()
            .and_then(|c| c.video_id.as_deref())
            .or_else(|| {
                self.snippet
                    .as_ref()?
                    .resource_id
                    .as_ref()?
                    .video_id
                    .as_deref()
            })
    }

    pub fn playlist_id(&self) -> Option<&str> {
        self.snippet.as_ref()?.playlist_id.as_deref()
    }

    pub fn position(&self) -> Option<u64> {
        self.snippet.as_ref()?.position
    }

    pub fn title(&self) -> &str {
        self.snippet
            .as_ref()
            .and_then(|s| s.title.as_deref())
            .unwrap_or("")
    }

    pub fn to_model(&self) -> PlaylistItem {
        let snippet = self.snippet.as_ref();
        let content = self.content_details.as_ref();
        let title = self.title().to_owned();
        // YouTube keeps deleted and private videos in playlists as placeholders
        // without an owner channel.
        let available = snippet.is_some_and(|s| s.video_owner_channel_id.is_some())
            && title != "Deleted video"
            && title != "Private video";
        let video_id = self.video_id().map(str::to_owned);
        PlaylistItem {
            playlist_item_id: self.id.clone(),
            position: self.position(),
            url: video_id
                .as_ref()
                .map(|id| format!("https://www.youtube.com/watch?v={id}")),
            video_id,
            title,
            available,
            video_channel_id: snippet.and_then(|s| s.video_owner_channel_id.clone()),
            video_channel_title: snippet.and_then(|s| s.video_owner_channel_title.clone()),
            video_published_at: content.and_then(|c| c.video_published_at.clone()),
            added_at: snippet.and_then(|s| s.published_at.clone()),
            note: content
                .and_then(|c| c.note.clone())
                .filter(|n| !n.is_empty()),
            privacy: privacy(self.status.as_ref()),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSnippet {
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub category_id: Option<String>,
    #[serde(default)]
    pub live_broadcast_content: Option<String>,
    #[serde(default)]
    pub default_language: Option<String>,
    #[serde(default)]
    pub default_audio_language: Option<String>,
    #[serde(default)]
    pub thumbnails: Option<Thumbnails>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoContent {
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub definition: Option<String>,
    /// `"true"` or `"false"`.
    #[serde(default)]
    pub caption: Option<String>,
}

/// YouTube sends counts as decimal strings.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoStatistics {
    #[serde(default)]
    pub view_count: Option<String>,
    #[serde(default)]
    pub like_count: Option<String>,
    #[serde(default)]
    pub comment_count: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveStreaming {
    #[serde(default)]
    pub actual_start_time: Option<String>,
    #[serde(default)]
    pub actual_end_time: Option<String>,
    #[serde(default)]
    pub scheduled_start_time: Option<String>,
    #[serde(default)]
    pub concurrent_viewers: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoRes {
    pub id: String,
    #[serde(default)]
    pub snippet: Option<VideoSnippet>,
    #[serde(default)]
    pub content_details: Option<VideoContent>,
    #[serde(default)]
    pub status: Option<StatusPart>,
    #[serde(default)]
    pub statistics: Option<VideoStatistics>,
    #[serde(default)]
    pub live_streaming_details: Option<LiveStreaming>,
}

fn count(raw: Option<&String>) -> Option<u64> {
    raw?.parse().ok()
}

impl VideoRes {
    pub fn duration_seconds(&self) -> Option<u64> {
        parse_duration(self.content_details.as_ref()?.duration.as_deref()?)
    }

    pub fn view_count(&self) -> Option<u64> {
        count(self.statistics.as_ref()?.view_count.as_ref())
    }

    pub fn title(&self) -> &str {
        self.snippet
            .as_ref()
            .and_then(|s| s.title.as_deref())
            .unwrap_or("")
    }

    pub fn to_model(&self) -> Video {
        let snippet = self.snippet.as_ref();
        let content = self.content_details.as_ref();
        let stats = self.statistics.as_ref();
        let (description, description_truncated) = cut(
            snippet.and_then(|s| s.description.as_deref()).unwrap_or(""),
            3000,
        );
        let mut tags = snippet.and_then(|s| s.tags.clone()).unwrap_or_default();
        tags.truncate(30);
        Video {
            video_id: self.id.clone(),
            title: self.title().to_owned(),
            description,
            description_truncated,
            channel_id: snippet.and_then(|s| s.channel_id.clone()),
            channel_title: snippet.and_then(|s| s.channel_title.clone()),
            published_at: snippet.and_then(|s| s.published_at.clone()),
            duration_seconds: self.duration_seconds(),
            definition: content.and_then(|c| c.definition.clone()),
            has_captions: content
                .and_then(|c| c.caption.as_deref())
                .map(|c| c == "true"),
            privacy: privacy(self.status.as_ref()),
            embeddable: self.status.as_ref().and_then(|s| s.embeddable),
            made_for_kids: self.status.as_ref().and_then(|s| s.made_for_kids),
            live_broadcast_content: snippet.and_then(|s| s.live_broadcast_content.clone()),
            live: self.live_streaming_details.as_ref().map(|l| LiveDetails {
                scheduled_start: l.scheduled_start_time.clone(),
                actual_start: l.actual_start_time.clone(),
                actual_end: l.actual_end_time.clone(),
                concurrent_viewers: count(l.concurrent_viewers.as_ref()),
            }),
            view_count: self.view_count(),
            like_count: stats.and_then(|s| count(s.like_count.as_ref())),
            comment_count: stats.and_then(|s| count(s.comment_count.as_ref())),
            tags,
            category_id: snippet.and_then(|s| s.category_id.clone()),
            default_language: snippet.and_then(|s| s.default_language.clone()),
            default_audio_language: snippet.and_then(|s| s.default_audio_language.clone()),
            thumbnail_url: best_thumbnail(snippet.and_then(|s| s.thumbnails.as_ref())),
            url: format!("https://www.youtube.com/watch?v={}", self.id),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSnippet {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub custom_url: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub thumbnails: Option<Thumbnails>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelStatistics {
    #[serde(default)]
    pub view_count: Option<String>,
    #[serde(default)]
    pub subscriber_count: Option<String>,
    #[serde(default)]
    pub hidden_subscriber_count: Option<bool>,
    #[serde(default)]
    pub video_count: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelatedPlaylists {
    #[serde(default)]
    pub uploads: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelContent {
    #[serde(default)]
    pub related_playlists: Option<RelatedPlaylists>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelRes {
    pub id: String,
    #[serde(default)]
    pub snippet: Option<ChannelSnippet>,
    #[serde(default)]
    pub statistics: Option<ChannelStatistics>,
    #[serde(default)]
    pub content_details: Option<ChannelContent>,
}

impl ChannelRes {
    pub fn title(&self) -> Option<&str> {
        self.snippet.as_ref()?.title.as_deref()
    }

    pub fn to_model(&self) -> Channel {
        let snippet = self.snippet.as_ref();
        let stats = self.statistics.as_ref();
        let hidden = stats.and_then(|s| s.hidden_subscriber_count) == Some(true);
        Channel {
            channel_id: self.id.clone(),
            title: self.title().unwrap_or("").to_owned(),
            handle: snippet.and_then(|s| s.custom_url.clone()),
            description: cut(
                snippet.and_then(|s| s.description.as_deref()).unwrap_or(""),
                2000,
            )
            .0,
            published_at: snippet.and_then(|s| s.published_at.clone()),
            country: snippet.and_then(|s| s.country.clone()),
            subscriber_count: if hidden {
                None
            } else {
                stats.and_then(|s| count(s.subscriber_count.as_ref()))
            },
            video_count: stats.and_then(|s| count(s.video_count.as_ref())),
            view_count: stats.and_then(|s| count(s.view_count.as_ref())),
            uploads_playlist_id: self
                .content_details
                .as_ref()
                .and_then(|c| c.related_playlists.as_ref())
                .and_then(|r| r.uploads.clone()),
            thumbnail_url: best_thumbnail(snippet.and_then(|s| s.thumbnails.as_ref())),
            url: format!("https://www.youtube.com/channel/{}", self.id),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchId {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub video_id: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub playlist_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSnippet {
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub channel_title: Option<String>,
    #[serde(default)]
    pub live_broadcast_content: Option<String>,
    #[serde(default)]
    pub thumbnails: Option<Thumbnails>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRes {
    #[serde(default)]
    pub id: SearchId,
    #[serde(default)]
    pub snippet: Option<SearchSnippet>,
}

impl SearchRes {
    /// `None` for result kinds Omni does not serve.
    pub fn to_model(&self) -> Option<SearchResult> {
        let (kind, id, url) = match self.id.kind.as_deref() {
            Some("youtube#video") => {
                let id = self.id.video_id.clone()?;
                let url = format!("https://www.youtube.com/watch?v={id}");
                (SearchType::Video, id, url)
            }
            Some("youtube#channel") => {
                let id = self.id.channel_id.clone()?;
                let url = format!("https://www.youtube.com/channel/{id}");
                (SearchType::Channel, id, url)
            }
            Some("youtube#playlist") => {
                let id = self.id.playlist_id.clone()?;
                let url = format!("https://www.youtube.com/playlist?list={id}");
                (SearchType::Playlist, id, url)
            }
            _ => return None,
        };
        let snippet = self.snippet.as_ref();
        // search.list HTML-escapes its titles and descriptions; other methods do not.
        let text = |value: Option<&String>| unescape_html(value.map_or("", String::as_str));
        Some(SearchResult {
            kind,
            id,
            title: text(snippet.and_then(|s| s.title.as_ref())),
            description: text(snippet.and_then(|s| s.description.as_ref())),
            channel_id: snippet.and_then(|s| s.channel_id.clone()),
            channel_title: snippet
                .and_then(|s| s.channel_title.as_ref())
                .map(|t| unescape_html(t)),
            published_at: snippet.and_then(|s| s.published_at.clone()),
            live_broadcast_content: snippet.and_then(|s| s.live_broadcast_content.clone()),
            duration_seconds: None,
            view_count: None,
            thumbnail_url: best_thumbnail(snippet.and_then(|s| s.thumbnails.as_ref())),
            url,
        })
    }
}

/// Google's error body: `{"error": {"code", "message", "errors": [{"reason"}], "details"}}`.
#[derive(Debug, Default, Deserialize)]
pub struct ErrorBody {
    #[serde(default)]
    pub error: Option<ErrorObject>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ErrorObject {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub errors: Vec<ErrorItem>,
    #[serde(default)]
    pub details: Vec<ErrorDetail>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ErrorItem {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ErrorDetail {
    #[serde(default)]
    pub reason: Option<String>,
}

impl ErrorBody {
    pub fn reason(&self) -> Option<String> {
        let error = self.error.as_ref()?;
        error
            .errors
            .iter()
            .find_map(|e| e.reason.clone())
            .or_else(|| error.details.iter().find_map(|d| d.reason.clone()))
    }

    pub fn message(&self) -> String {
        let message = self
            .error
            .as_ref()
            .and_then(|e| e.message.as_deref())
            .unwrap_or("");
        cut(&strip_tags(message), 300).0
    }
}

/// Google's messages sometimes wrap ids in `<code>` or links in `<a>`.
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// The first `max` characters, and whether anything was cut.
pub fn cut(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((index, _)) => (format!("{}…", &text[..index]), true),
        None => (text.to_owned(), false),
    }
}

/// Decodes the entities `search.list` uses (`&amp;`, `&quot;`, `&#39;`, numeric).
pub fn unescape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let decoded = tail.find(';').filter(|end| *end <= 10).and_then(|end| {
            let entity = &tail[1..end];
            let c = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((c, end + 1))
        });
        match decoded {
            Some((c, consumed)) => {
                out.push(c);
                rest = &tail[consumed..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// ISO 8601 durations as YouTube writes them (`PT1H2M3S`, `P1DT2H`, `P0D`).
pub fn parse_duration(raw: &str) -> Option<u64> {
    let body = raw.strip_prefix('P')?;
    let (date, time) = match body.split_once('T') {
        Some((date, time)) => (date, time),
        None => (body, ""),
    };
    let mut total: u64 = 0;
    let mut add = |part: &str, units: &[(char, u64)]| -> Option<()> {
        let mut number = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() {
                number.push(c);
                continue;
            }
            let (_, seconds) = units.iter().find(|(unit, _)| *unit == c)?;
            let value: u64 = number.parse().ok()?;
            total = total.checked_add(value.checked_mul(*seconds)?)?;
            number.clear();
        }
        number.is_empty().then_some(())
    };
    add(date, &[('W', 604_800), ('D', 86_400)])?;
    add(time, &[('H', 3_600), ('M', 60), ('S', 1)])?;
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_as_youtube_writes_them() {
        assert_eq!(parse_duration("PT1H2M3S"), Some(3723));
        assert_eq!(parse_duration("PT45S"), Some(45));
        assert_eq!(parse_duration("P1DT1M"), Some(86_460));
        assert_eq!(parse_duration("P0D"), Some(0));
        assert_eq!(parse_duration("1H"), None);
        assert_eq!(parse_duration("PT1X"), None);
    }

    #[test]
    fn search_text_is_unescaped() {
        assert_eq!(
            unescape_html("Rock &amp; Roll &quot;Live&quot; &#39;99 &#x41; &bogus; a&b"),
            "Rock & Roll \"Live\" '99 A &bogus; a&b"
        );
    }

    #[test]
    fn cut_marks_truncation() {
        assert_eq!(cut("abcdef", 3), ("abc…".to_owned(), true));
        assert_eq!(cut("abc", 3), ("abc".to_owned(), false));
    }

    #[test]
    fn deleted_videos_are_unavailable_items() {
        let item: PlaylistItemRes = serde_json::from_value(serde_json::json!({
            "id": "UEx1",
            "snippet": {"title": "Deleted video", "position": 3, "playlistId": "PL1",
                        "resourceId": {"kind": "youtube#video", "videoId": "abcdefghijk"}},
            "contentDetails": {"videoId": "abcdefghijk"}
        }))
        .unwrap();
        let model = item.to_model();
        assert!(!model.available);
        assert_eq!(model.position, Some(3));
        assert_eq!(model.video_id.as_deref(), Some("abcdefghijk"));
    }

    #[test]
    fn error_reasons_come_from_errors_or_details() {
        let body: ErrorBody = serde_json::from_value(serde_json::json!({
            "error": {"code": 403, "message": "The request cannot be completed because you have exceeded your <a href=\"/youtube/v3/getting-started#quota\">quota</a>.",
                      "errors": [{"reason": "quotaExceeded", "domain": "youtube.quota"}]}
        }))
        .unwrap();
        assert_eq!(body.reason().as_deref(), Some("quotaExceeded"));
        assert!(body.message().ends_with("exceeded your quota."));
        let disabled: ErrorBody = serde_json::from_value(serde_json::json!({
            "error": {"code": 403, "message": "disabled", "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "SERVICE_DISABLED"}]}
        }))
        .unwrap();
        assert_eq!(disabled.reason().as_deref(), Some("SERVICE_DISABLED"));
    }
}
