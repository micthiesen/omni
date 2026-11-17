//! Persisted PressPods shapes.
//!
//! Field order matches the stored rows, so a new row encodes like an existing
//! one. Every struct carries
//! `extra` so read-modify-write never drops fields a newer writer added.

use indexmap::IndexMap;
use omni_store::Entity;
use omni_store::cbor::Extra;
use serde::{Deserialize, Serialize};

/// `authorGender`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AuthorGender {
    Male,
    Female,
    Unknown,
}

impl AuthorGender {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthorGender::Male => "male",
            AuthorGender::Female => "female",
            AuthorGender::Unknown => "unknown",
        }
    }
}

/// A chapter marker: title plus its start offset (seconds) into the final audio.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chapter {
    pub start_time_seconds: f64,
    pub title: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Chapter {
    pub fn new(start_time_seconds: f64, title: impl Into<String>) -> Self {
        Self {
            start_time_seconds,
            title: title.into(),
            extra: Extra::new(),
        }
    }
}

/// Per-chunk synthesis stats persisted on the episode for diagnostics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkStat {
    pub index: i64,
    pub section_index: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_title: Option<String>,
    pub text: String,
    pub char_count: i64,
    pub duration_seconds: f64,
    /// Offset into the final audio (includes the intro jingle), like [`Chapter`].
    pub start_time_seconds: f64,
    pub sec_per_char: f64,
    pub attempts: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub word_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_words: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resplit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resplit_depth: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Compact per-retriever outcome persisted on the episode.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawAttempt", into = "RawAttempt")]
pub enum RetrieverAttempt {
    Success {
        name: String,
        content_rating: f64,
        text_chars: i64,
        extra: Extra,
    },
    Failure {
        name: String,
        error: String,
        extra: Extra,
    },
}

impl RetrieverAttempt {
    pub fn name(&self) -> &str {
        match self {
            RetrieverAttempt::Success { name, .. } | RetrieverAttempt::Failure { name, .. } => name,
        }
    }
}

/// The wire shape of [`RetrieverAttempt`]; `success` discriminates.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAttempt {
    name: String,
    success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_rating: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text_chars: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten)]
    extra: Extra,
}

impl TryFrom<RawAttempt> for RetrieverAttempt {
    type Error = String;

    fn try_from(raw: RawAttempt) -> Result<Self, String> {
        if raw.success {
            match (raw.content_rating, raw.text_chars) {
                (Some(content_rating), Some(text_chars)) => Ok(RetrieverAttempt::Success {
                    name: raw.name,
                    content_rating,
                    text_chars,
                    extra: raw.extra,
                }),
                _ => Err("a successful retriever attempt needs contentRating and textChars".into()),
            }
        } else {
            match raw.error {
                Some(error) => Ok(RetrieverAttempt::Failure {
                    name: raw.name,
                    error,
                    extra: raw.extra,
                }),
                None => Err("a failed retriever attempt needs an error".into()),
            }
        }
    }
}

impl From<RetrieverAttempt> for RawAttempt {
    fn from(attempt: RetrieverAttempt) -> Self {
        match attempt {
            RetrieverAttempt::Success {
                name,
                content_rating,
                text_chars,
                extra,
            } => RawAttempt {
                name,
                success: true,
                content_rating: Some(content_rating),
                text_chars: Some(text_chars),
                error: None,
                extra,
            },
            RetrieverAttempt::Failure { name, error, extra } => RawAttempt {
                name,
                success: false,
                content_rating: None,
                text_chars: None,
                error: Some(error),
                extra,
            },
        }
    }
}

/// Token counts per cost detail key.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input: f64,
    pub output: f64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `Costs`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Costs {
    pub llm_cents: f64,
    pub tts_cents: f64,
    pub detail_cents: IndexMap<String, f64>,
    pub detail_tokens: IndexMap<String, TokenCounts>,
    pub detail_chars: IndexMap<String, f64>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `press-pods-episode`, keyed by `episodeId`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PressPodsEpisode {
    /// Random id; doubles as the (unguessable) audio file name stem.
    pub episode_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_gender: Option<AuthorGender>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    pub article_url: String,
    /// Canonical identity for dedup/replace; absent on old rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_image_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    /// The cleaned, narration-ready text that was synthesized.
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesized_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapters: Option<Vec<Chapter>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<ChunkStat>>,
    pub audio_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    pub file_bytes: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retriever_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retriever_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retriever_attempts: Option<Vec<RetrieverAttempt>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub costs: Option<Costs>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<i64>,
    /// Task run that produced this episode; links to its captured logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for PressPodsEpisode {
    const NAME: &'static str = "press-pods-episode";
    type Key = String;
    fn key(&self) -> String {
        self.episode_id.clone()
    }
}

/// `PressPodsJobStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Processing,
    Failed,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Processing => "processing",
            JobStatus::Failed => "failed",
        }
    }
}

/// `press-pods-job`, keyed by `jobId`: the durable submission queue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PressPodsJob {
    pub job_id: String,
    /// Original submitted URL: what the retrievers fetch (query string intact).
    pub url: String,
    /// Canonical identity for dedup and resubmit-as-retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_url: Option<String>,
    pub status: JobStatus,
    pub attempts: i64,
    /// Earliest time the job may run (0 = immediately).
    pub next_attempt_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Set while processing; used to detect crashed runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<String>,
    /// When the speech server was first found unreachable in the current
    /// outage; cleared when a preflight next passes. The give-up deadline
    /// counts from here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_unreachable_since: Option<i64>,
    /// Failed preflights in the current outage (drives the backoff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_unreachable_checks: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for PressPodsJob {
    const NAME: &'static str = "press-pods-job";
    type Key = String;
    fn key(&self) -> String {
        self.job_id.clone()
    }
}

/// How long an attempt-history row is kept (the docstore hides and then
/// removes expired rows).
pub const ATTEMPT_HISTORY_TTL_MS: i64 = 180 * 24 * 60 * 60 * 1000;

/// How a recorded job attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttemptOutcome {
    /// Failed and requeued with backoff.
    Retrying,
    /// Failed for good (permanent error or attempts exhausted).
    Failed,
    /// Produced an episode after earlier failures for the same article.
    Succeeded,
}

/// Why a retriever produced no usable article.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrieverErrorClass {
    /// The site or proxy answered with an HTTP error status.
    HttpStatus,
    Timeout,
    Network,
    /// Text was extracted but the rating model judged it not an article.
    InvalidArticle,
    /// Text was extracted but the rating model call failed.
    RatingFailed,
    /// Anything else: nothing extractable, a bot challenge, a blocked URL.
    #[serde(other)]
    Other,
}

/// One retriever's part in a recorded attempt. No article text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrieverOutcome {
    pub name: String,
    /// The extraction was rated a valid article.
    pub ok: bool,
    /// This extraction was the one narrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<RetrieverErrorClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// A short reason, never a response body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// UTF-16 length of the extracted text, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_chars: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_rating: Option<f64>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `press-pods-job-attempt`, keyed by `attemptId`: the append-only history of
/// failed attempts and of the successes that followed them. Rows are never
/// updated and are not tied to the job row, so deleting or resubmitting a job
/// leaves them in place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PressPodsJobAttempt {
    pub attempt_id: String,
    pub job_id: String,
    pub url: String,
    pub normalized_url: String,
    /// Lowercase host without a leading `www.`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Attempt number within the job's current retry budget (a manual retry
    /// or resubmit starts again at 1).
    pub attempt: i64,
    pub at: i64,
    pub outcome: AttemptOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The pipeline step that failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// Absent when the attempt ended before retrieval finished (a crash, a
    /// refused URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrievers: Option<Vec<RetrieverOutcome>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The episode a `succeeded` attempt produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for PressPodsJobAttempt {
    const NAME: &'static str = "press-pods-job-attempt";
    const DEFAULT_TTL_MS: Option<i64> = Some(ATTEMPT_HISTORY_TTL_MS);
    type Key = String;
    fn key(&self) -> String {
        self.attempt_id.clone()
    }
}
