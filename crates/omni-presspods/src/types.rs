//! In-memory pipeline types.

use serde::Serialize;

use crate::error::PressPodsError;
use crate::model::{AuthorGender, RetrieverAttempt};

/// A retrieved article.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Article {
    pub title: Option<String>,
    pub text: String,
    pub author: Option<String>,
    pub domain: Option<String>,
    pub url: String,
    /// Epoch milliseconds.
    pub published_at: Option<i64>,
    pub lead_image_url: Option<String>,
}

/// `MetadataInfo`: the metadata model's validated view of an article.
#[derive(Clone, Debug, PartialEq)]
pub struct MetadataInfo {
    pub is_valid_article: bool,
    pub title: Option<String>,
    pub author: Option<String>,
    pub author_gender: Option<AuthorGender>,
    pub coauthors: Option<Vec<String>>,
    pub publication: Option<String>,
    /// Epoch milliseconds of a valid `publishedAtISO`.
    pub published_at: Option<i64>,
    pub lead_image_url: Option<String>,
    pub short_summary: Option<String>,
    pub content_rating: f64,
}

/// One retriever's outcome after rating. A short-lived list of at most seven,
/// so the size difference between variants does not matter.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum RetrieverResult {
    Success {
        retriever_name: String,
        article: Article,
        metadata: MetadataInfo,
    },
    Failure {
        retriever_name: String,
        error: PressPodsError,
    },
}

impl RetrieverResult {
    pub fn retriever_name(&self) -> &str {
        match self {
            RetrieverResult::Success { retriever_name, .. }
            | RetrieverResult::Failure { retriever_name, .. } => retriever_name,
        }
    }
}

/// The persisted message of a failed attempt: a
/// retriever's own failure keeps its operation (`"retrieve article with
/// Wayback: No archived snapshot ..."`), a rating outcome is the bare cause.
pub fn attempt_error(error: &PressPodsError) -> String {
    if error.operation() == crate::retrievers::RATING_OPERATION {
        error.cause_message()
    } else {
        error.to_string()
    }
}

/// One retriever's failure in a few words, for the job's stored error: the
/// HTTP status alone (response bodies are whole challenge pages), else the
/// first line of the cause, capped at 60 characters.
pub fn failure_reason(error: &PressPodsError) -> String {
    if let PressPodsError::Http {
        source: omni_http::HttpError::Status { status, .. },
        ..
    } = error
    {
        return format!("HTTP {status}");
    }
    let cause = error.cause_message();
    let line = cause.lines().next().unwrap_or("").trim();
    if line.chars().count() > 60 {
        format!("{}...", line.chars().take(57).collect::<String>())
    } else {
        line.to_owned()
    }
}

/// The compact per-retriever outcome persisted
/// on the episode (full results carry every article text).
pub fn summarize_retriever_attempts(results: &[RetrieverResult]) -> Vec<RetrieverAttempt> {
    results
        .iter()
        .map(|result| match result {
            RetrieverResult::Success {
                retriever_name,
                article,
                metadata,
            } => RetrieverAttempt::Success {
                name: retriever_name.clone(),
                content_rating: metadata.content_rating,
                text_chars: i64::try_from(omni_core::js::utf16_len(&article.text))
                    .unwrap_or(i64::MAX),
                extra: Default::default(),
            },
            RetrieverResult::Failure {
                retriever_name,
                error,
            } => RetrieverAttempt::Failure {
                name: retriever_name.clone(),
                error: attempt_error(error),
                extra: Default::default(),
            },
        })
        .collect()
}
