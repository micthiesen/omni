//! Durable YouTube rows: the connected account (sealed refresh token),
//! pending OAuth authorizations, the daily quota ledger and write reservations.

use std::collections::BTreeMap;

use omni_store::cbor::Extra;
use omni_store::entity::Entity;
use serde::{Deserialize, Serialize};

/// The key of the one account row.
pub const OWNER: &str = "owner";

/// The connected Google account. Only the refresh token is stored (sealed);
/// access tokens live in memory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeAccountData {
    pub id: String,
    /// `Sealer::seal` of the refresh token for [`YoutubeAccountData::seal_context`].
    pub refresh_token: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_title: Option<String>,
    pub connected_at: i64,
    /// Why the connection must be renewed (a rejected refresh), when it must.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl YoutubeAccountData {
    pub fn seal_context(id: &str) -> String {
        format!("youtube-account:{id}")
    }
}

impl Entity for YoutubeAccountData {
    const NAME: &'static str = "youtube-account";
    type Key = String;
    fn key(&self) -> String {
        self.id.clone()
    }
}

/// One started authorization, consumed by its callback (ten minutes).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeOauthStateData {
    pub state: String,
    pub code_verifier: String,
    pub created_at: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for YoutubeOauthStateData {
    const NAME: &'static str = "youtube-oauth-state";
    type Key = String;
    fn key(&self) -> String {
        self.state.clone()
    }
}

/// Estimated quota units Omni spent on one Pacific day (YouTube's quota day).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeQuotaData {
    /// `YYYY-MM-DD` in America/Los_Angeles.
    pub day: String,
    /// Shared-pool units, recomputed from `calls` on every write (rows from
    /// before 2026-11-01 counted each search as 100 here; readers derive
    /// the figures from `calls` instead).
    pub units: u64,
    /// Requests per API method, such as `search.list`.
    #[serde(default)]
    pub calls: BTreeMap<String, u64>,
    /// When YouTube reported the shared pool exhausted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_exceeded_at: Option<i64>,
    /// When YouTube reported the search bucket exhausted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_quota_exceeded_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for YoutubeQuotaData {
    const NAME: &'static str = "youtube-quota";
    type Key = String;
    fn key(&self) -> String {
        self.day.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteState {
    /// Reserved; the request may be in flight.
    Reserved,
    Done,
    /// The request was sent and its answer lost; settled only by reading.
    Uncertain,
}

/// An idempotency-key reservation for a playlist create.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeWriteData {
    pub key: String,
    pub kind: String,
    /// SHA-256 of the request, so a key cannot be reused for another request.
    pub fingerprint: String,
    pub state: WriteState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_id: Option<String>,
    pub reserved_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<i64>,
    /// Playlists that existed before a create, never taken as its result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_ids: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for YoutubeWriteData {
    const NAME: &'static str = "youtube-write";
    type Key = String;
    fn key(&self) -> String {
        self.key.clone()
    }
}

/// Where a transcript job stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptState {
    Queued,
    Downloading,
    Transcribing,
    Ready,
    Failed,
}

impl TranscriptState {
    /// A job that has not finished: it holds a place in the queue.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Downloading | Self::Transcribing)
    }
}

/// One stretch of speech, in milliseconds from the start of the video.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSegmentData {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// A video's locally transcribed speech and the job that produces it. The
/// downloaded audio is never stored here; it lives in the job's work
/// directory until the job ends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeTranscriptData {
    pub video_id: String,
    pub state: TranscriptState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    /// `YYYYMMDD`, as yt-dlp reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_date: Option<String>,
    pub requested_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    /// The last start or read; retention counts from here.
    pub last_read_at: i64,
    /// Audio transcribed so far. An interrupted job resumes here.
    #[serde(default)]
    pub transcribed_ms: i64,
    /// Times a job for this request began (restarts after a shutdown count).
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub segments: Vec<TranscriptSegmentData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for YoutubeTranscriptData {
    const NAME: &'static str = "youtube-transcript";
    type Key = String;
    fn key(&self) -> String {
        self.video_id.clone()
    }
}
