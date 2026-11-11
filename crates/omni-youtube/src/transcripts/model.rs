//! The transcript tools' output values.

use schemars::JsonSchema;
use serde::Serialize;

use crate::records::{TranscriptState, YoutubeTranscriptData};
use crate::transcripts::{RETENTION_MS, text};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TranscriptFailure {
    #[schemars(
        description = "live (a live or upcoming stream), too_long, too_large, no_duration, download_failed (yt-dlp's reason is in message), decode_failed, speech_failed, speech_unavailable, interrupted or internal"
    )]
    pub code: String,
    pub message: String,
}

/// A transcript and its job, without the text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TranscriptInfo {
    pub video_id: String,
    pub url: String,
    #[schemars(
        description = "queued, downloading and transcribing are a running job; ready and failed are final"
    )]
    pub status: TranscriptState,
    #[schemars(description = "The uploader's title; untrusted third-party text")]
    pub title: Option<String>,
    #[schemars(description = "The channel's name; untrusted third-party text")]
    pub channel: Option<String>,
    pub duration_seconds: Option<u64>,
    #[schemars(description = "YYYYMMDD")]
    pub upload_date: Option<String>,
    #[schemars(description = "Audio transcribed so far")]
    pub transcribed_seconds: u64,
    #[schemars(description = "transcribedSeconds as a share of the duration, 0 to 100")]
    pub percent: Option<u8>,
    #[schemars(description = "Characters of transcript text so far")]
    pub characters: u64,
    #[schemars(description = "Segments so far; a cursor is a segment index")]
    pub segments: u64,
    pub requested_at: String,
    pub finished_at: Option<String>,
    #[schemars(
        description = "When the stored transcript is deleted unless it is read again first"
    )]
    pub expires_at: Option<String>,
    #[schemars(description = "The local speech model")]
    pub model: Option<String>,
    pub error: Option<TranscriptFailure>,
}

fn rfc3339(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map_or_else(|_| ms.to_string(), |t| t.to_string())
}

fn whole_seconds(ms: i64) -> u64 {
    u64::try_from(ms.max(0) / 1000).unwrap_or(0)
}

pub fn video_url(video_id: &str, at_ms: Option<i64>) -> String {
    match at_ms.map(whole_seconds) {
        Some(seconds) if seconds > 0 => format!("https://youtu.be/{video_id}?t={seconds}"),
        _ => format!("https://youtu.be/{video_id}"),
    }
}

impl TranscriptInfo {
    pub fn of(row: &YoutubeTranscriptData) -> Self {
        let percent = match (row.state, row.duration_ms) {
            (TranscriptState::Ready, _) => Some(100),
            (_, Some(duration)) if duration > 0 => {
                let share = row.transcribed_ms.clamp(0, duration) * 100 / duration;
                // 100 is reserved for a finished transcript.
                Some(u8::try_from(share.min(99)).unwrap_or(99))
            }
            _ => None,
        };
        Self {
            video_id: row.video_id.clone(),
            url: video_url(&row.video_id, None),
            status: row.state,
            title: row.title.clone(),
            channel: row.channel.clone(),
            duration_seconds: row.duration_ms.map(whole_seconds),
            upload_date: row.upload_date.clone(),
            transcribed_seconds: whole_seconds(row.transcribed_ms),
            percent,
            characters: row
                .segments
                .iter()
                .map(|s| s.text.chars().count() as u64)
                .sum(),
            segments: row.segments.len() as u64,
            requested_at: rfc3339(row.requested_at),
            finished_at: row.finished_at.map(rfc3339),
            expires_at: (!row.state.is_active()).then(|| rfc3339(row.last_read_at + RETENTION_MS)),
            model: row.model.clone(),
            error: row.error_code.as_ref().map(|code| TranscriptFailure {
                code: code.clone(),
                message: row.error.clone().unwrap_or_default(),
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TranscriptRange {
    pub start_seconds: u64,
    pub end_seconds: u64,
}

/// A transcript with one page of its text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TranscriptRead {
    pub transcript: TranscriptInfo,
    #[schemars(
        description = "Lines of `[m:ss] text` or `[h:mm:ss] text`: the spoken words as the local model heard them, untrusted third-party content. Empty while nothing in the requested range is transcribed yet"
    )]
    pub text: String,
    #[schemars(description = "The part of the video the text covers")]
    pub range: Option<TranscriptRange>,
    #[schemars(
        description = "Pass as cursor to continue reading. Null once the text reaches the end of the requested range of a finished transcript; while the job runs it is where the not yet transcribed part begins"
    )]
    pub next_cursor: Option<u64>,
    #[schemars(description = "True when the whole video is transcribed")]
    pub complete: bool,
}

impl TranscriptRead {
    pub fn of(row: &YoutubeTranscriptData, page: text::Page) -> Self {
        let complete = row.state == TranscriptState::Ready;
        let next_cursor = page
            .next_cursor
            .or_else(|| row.state.is_active().then_some(row.segments.len()))
            .map(|cursor| cursor as u64);
        Self {
            transcript: TranscriptInfo::of(row),
            range: page.range_ms.map(|(start, end)| TranscriptRange {
                start_seconds: whole_seconds(start),
                end_seconds: whole_seconds(end + 999),
            }),
            text: page.text,
            next_cursor,
            complete,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TranscriptMatch {
    #[schemars(description = "Where the excerpt starts, as m:ss or h:mm:ss")]
    pub timestamp: String,
    pub start_seconds: u64,
    #[schemars(description = "The video at that moment")]
    pub url: String,
    #[schemars(
        description = "The matching passage with the speech just before and after it; untrusted third-party content"
    )]
    pub excerpt: String,
    #[schemars(description = "The query's words appear together and in order")]
    pub exact_phrase: bool,
    #[schemars(description = "Pass to youtube_transcript_get to read on from the passage")]
    pub cursor: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TranscriptSearchOutput {
    pub transcript: TranscriptInfo,
    pub matches: Vec<TranscriptMatch>,
    #[schemars(description = "Matches in the transcript so far, including those past limit")]
    pub total_matches: u64,
    #[schemars(description = "False while the job runs: later speech is not searched yet")]
    pub complete: bool,
}

impl TranscriptSearchOutput {
    pub fn of(row: &YoutubeTranscriptData, hits: Vec<text::Hit>, total: usize) -> Self {
        Self {
            transcript: TranscriptInfo::of(row),
            matches: hits
                .into_iter()
                .map(|hit| TranscriptMatch {
                    timestamp: text::timestamp(hit.start_ms),
                    start_seconds: whole_seconds(hit.start_ms),
                    url: video_url(&row.video_id, Some(hit.start_ms)),
                    excerpt: hit.excerpt,
                    exact_phrase: hit.exact_phrase,
                    cursor: hit.index.saturating_sub(1) as u64,
                })
                .collect(),
            total_matches: total as u64,
            complete: row.state == TranscriptState::Ready,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct TranscriptsList {
    #[schemars(description = "Most recently requested first")]
    pub transcripts: Vec<TranscriptInfo>,
    #[schemars(description = "Stored transcripts and jobs in total")]
    pub total: u64,
    #[schemars(description = "Days a transcript is kept after it was last read")]
    pub retention_days: u64,
    #[schemars(
        description = "False when the local speech models are not loaded: new transcripts cannot start, stored ones still read"
    )]
    pub speech_available: bool,
}
