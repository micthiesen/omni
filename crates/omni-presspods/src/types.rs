//! In-memory pipeline types.

use serde::Serialize;

use crate::error::PressPodsError;
use crate::model::{AuthorGender, RetrieverAttempt, RetrieverErrorClass, RetrieverOutcome};

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
        /// UTF-16 length of the text the retriever extracted, when it got
        /// that far (the rating rejected it or could not run).
        text_chars: Option<i64>,
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
                text_chars: text_chars(&article.text),
                extra: Default::default(),
            },
            RetrieverResult::Failure {
                retriever_name,
                error,
                ..
            } => RetrieverAttempt::Failure {
                name: retriever_name.clone(),
                error: attempt_error(error),
                extra: Default::default(),
            },
        })
        .collect()
}

/// UTF-16 length of an extracted text, as stored.
pub fn text_chars(text: &str) -> i64 {
    i64::try_from(omni_core::js::utf16_len(text)).unwrap_or(i64::MAX)
}

fn error_class(error: &PressPodsError) -> RetrieverErrorClass {
    if error.operation() == crate::retrievers::RATING_OPERATION {
        return if error.cause_message() == crate::retrievers::INVALID_ARTICLE {
            RetrieverErrorClass::InvalidArticle
        } else {
            RetrieverErrorClass::RatingFailed
        };
    }
    match error {
        PressPodsError::Timeout { .. }
        | PressPodsError::Http {
            source: omni_http::HttpError::Timeout,
            ..
        } => RetrieverErrorClass::Timeout,
        PressPodsError::Http {
            source: omni_http::HttpError::Network(_),
            ..
        } => RetrieverErrorClass::Network,
        _ if error.status_code().is_some() => RetrieverErrorClass::HttpStatus,
        _ => RetrieverErrorClass::Other,
    }
}

/// Every retriever's outcome for the attempt history: names, classes, lengths
/// and ratings only. `selected` names the winning retriever, if one won.
pub fn retriever_outcomes(
    results: &[RetrieverResult],
    selected: Option<&str>,
) -> Vec<RetrieverOutcome> {
    results
        .iter()
        .map(|result| match result {
            RetrieverResult::Success {
                retriever_name,
                article,
                metadata,
            } => RetrieverOutcome {
                name: retriever_name.clone(),
                ok: true,
                selected: (selected == Some(retriever_name.as_str())).then_some(true),
                error_class: None,
                http_status: None,
                error: None,
                text_chars: Some(text_chars(&article.text)),
                content_rating: Some(metadata.content_rating),
                extra: Default::default(),
            },
            RetrieverResult::Failure {
                retriever_name,
                error,
                text_chars,
            } => RetrieverOutcome {
                name: retriever_name.clone(),
                ok: false,
                selected: None,
                error_class: Some(error_class(error)),
                // Only the site's or proxy's own status, never the rating model's.
                http_status: (error_class(error) == RetrieverErrorClass::HttpStatus)
                    .then(|| error.status_code())
                    .flatten(),
                error: Some(failure_reason(error)),
                text_chars: *text_chars,
                content_rating: None,
                extra: Default::default(),
            },
        })
        .collect()
}
