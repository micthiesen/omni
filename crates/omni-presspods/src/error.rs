//! PressPods errors.
//!
//! One error type for the whole pipeline. Every variant names the operation
//! that failed and keeps enough of its cause to classify it: the durable job
//! queue retries only transient failures and stores a short summary.

use omni_ai::AiError;
use omni_core::process::ProcessError;
use omni_http::HttpError;
use omni_store::StoreError;

/// A PressPods failure: `"<operation>: <cause>"`.
#[derive(Debug, thiserror::Error)]
pub enum PressPodsError {
    /// An HTTP call failed (transport, status, size bound, SSRF guard).
    #[error("{operation}: {source}")]
    Http {
        operation: String,
        #[source]
        source: HttpError,
    },
    /// A language-model call failed.
    #[error("{operation}: {source}")]
    Ai {
        operation: String,
        #[source]
        source: AiError,
    },
    /// A remote service answered with a non-success status (TTS, STT).
    #[error("{operation}: {message}")]
    Status {
        operation: String,
        status: u16,
        /// Response body excerpt.
        body: String,
        message: String,
    },
    /// The docstore failed.
    #[error("{operation}: {source}")]
    Store {
        operation: String,
        #[source]
        source: StoreError,
    },
    /// Filesystem I/O failed.
    #[error("{operation}: {source}")]
    Io {
        operation: String,
        #[source]
        source: std::io::Error,
    },
    /// ffmpeg / ffprobe could not run or exited unsuccessfully.
    #[error("{operation}: {message}")]
    Process {
        operation: String,
        message: String,
        #[source]
        source: Option<ProcessError>,
    },
    /// Data that failed validation.
    #[error("{operation}: {message}")]
    InvalidData { operation: String, message: String },
    /// Any other expected failure.
    #[error("{operation}: {message}")]
    Failed {
        operation: String,
        message: String,
        /// Explicit retry hint; `None` defers to classification.
        retryable: Option<bool>,
    },
    /// The operation exceeded its deadline. This is not a transient-network
    /// signal and does not trigger the job queue's backoff retry.
    #[error("{operation}: timed out")]
    Timeout { operation: String },
    /// The self-hosted speech server did not answer at the connection level
    /// (no route, refused, no response in time). The host is a laptop that
    /// sleeps and changes address, so this is an outage to wait out, not a
    /// fault of the article.
    #[error("{operation}: speech server unreachable at {url}: {reason}")]
    SpeechUnreachable {
        operation: String,
        /// The server's origin, without userinfo, path or query.
        url: String,
        reason: String,
        /// No response in time, as opposed to a connection that failed
        /// outright; a busy server looks the same.
        timed_out: bool,
    },
}

impl PressPodsError {
    pub fn failed(operation: impl Into<String>, message: impl Into<String>) -> Self {
        PressPodsError::Failed {
            operation: operation.into(),
            message: message.into(),
            retryable: None,
        }
    }

    pub fn invalid(operation: impl Into<String>, message: impl Into<String>) -> Self {
        PressPodsError::InvalidData {
            operation: operation.into(),
            message: message.into(),
        }
    }

    pub fn http(operation: impl Into<String>, source: HttpError) -> Self {
        PressPodsError::Http {
            operation: operation.into(),
            source,
        }
    }

    pub fn ai(operation: impl Into<String>, source: AiError) -> Self {
        PressPodsError::Ai {
            operation: operation.into(),
            source,
        }
    }

    pub fn store(operation: impl Into<String>, source: StoreError) -> Self {
        PressPodsError::Store {
            operation: operation.into(),
            source,
        }
    }

    pub fn io(operation: impl Into<String>, source: std::io::Error) -> Self {
        PressPodsError::Io {
            operation: operation.into(),
            source,
        }
    }

    pub fn status(operation: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
        let body = body.into();
        let excerpt = omni_core::js::utf16_slice(&body, 0, 200).into_owned();
        PressPodsError::Status {
            operation: operation.into(),
            status,
            message: format!("HTTP {status}: {excerpt}"),
            body,
        }
    }

    pub fn timeout(operation: impl Into<String>) -> Self {
        PressPodsError::Timeout {
            operation: operation.into(),
        }
    }

    /// A connection-level failure against the speech server at `base_url`,
    /// or `None` when the server answered (any status) or the failure is not
    /// about reaching it.
    pub fn speech_unreachable(
        operation: impl Into<String>,
        base_url: &str,
        source: &HttpError,
        timeout: std::time::Duration,
    ) -> Option<Self> {
        let reason = match source {
            HttpError::Timeout => format!("no response within {} s", timeout.as_secs()),
            HttpError::Network(message) => message.clone(),
            _ => return None,
        };
        let url = url::Url::parse(base_url)
            .ok()
            .map(|url| url.origin().ascii_serialization())
            .filter(|origin| origin != "null")
            .unwrap_or_else(|| "the configured PRESSPODS_TTS_URL".to_owned());
        // The HTTP client appends the origin to network errors; say it once.
        let reason = reason
            .strip_suffix(&format!(" ({url})"))
            .map(str::to_owned)
            .unwrap_or(reason);
        Some(PressPodsError::SpeechUnreachable {
            operation: operation.into(),
            url,
            reason,
            timed_out: matches!(source, HttpError::Timeout),
        })
    }

    pub fn is_speech_unreachable(&self) -> bool {
        matches!(self, PressPodsError::SpeechUnreachable { .. })
    }

    /// The connection itself failed (no route, refused), so asking again
    /// right away cannot work.
    pub fn is_speech_connect_failure(&self) -> bool {
        matches!(
            self,
            PressPodsError::SpeechUnreachable {
                timed_out: false,
                ..
            }
        )
    }

    /// The failing operation.
    pub fn operation(&self) -> &str {
        match self {
            PressPodsError::Http { operation, .. }
            | PressPodsError::Ai { operation, .. }
            | PressPodsError::Status { operation, .. }
            | PressPodsError::Store { operation, .. }
            | PressPodsError::Io { operation, .. }
            | PressPodsError::Process { operation, .. }
            | PressPodsError::InvalidData { operation, .. }
            | PressPodsError::Failed { operation, .. }
            | PressPodsError::SpeechUnreachable { operation, .. }
            | PressPodsError::Timeout { operation } => operation,
        }
    }

    /// The cause's own message, without the operation.
    pub fn cause_message(&self) -> String {
        match self {
            PressPodsError::Http { source, .. } => source.to_string(),
            PressPodsError::Ai { source, .. } => source.to_string(),
            PressPodsError::Store { source, .. } => source.to_string(),
            PressPodsError::Io { source, .. } => source.to_string(),
            PressPodsError::Status { message, .. }
            | PressPodsError::Process { message, .. }
            | PressPodsError::InvalidData { message, .. }
            | PressPodsError::Failed { message, .. } => message.clone(),
            PressPodsError::Timeout { .. } => "timed out".to_owned(),
            PressPodsError::SpeechUnreachable { url, reason, .. } => {
                format!("speech server unreachable at {url}: {reason}")
            }
        }
    }

    /// The HTTP status of the cause, when it has one.
    pub fn status_code(&self) -> Option<u16> {
        match self {
            PressPodsError::Status { status, .. } => Some(*status),
            PressPodsError::Http {
                source: HttpError::Status { status, .. },
                ..
            } => Some(*status),
            PressPodsError::Ai {
                source: AiError::Provider { status, .. },
                ..
            } => Some(*status),
            PressPodsError::Ai {
                source: AiError::Http(HttpError::Status { status, .. }),
                ..
            } => Some(*status),
            _ => None,
        }
    }

    /// Whether the job queue retries this failure: provider errors use the
    /// model client's rule, HTTP statuses retry on 429 and 5xx, network
    /// failures and HTTP timeouts retry, and everything else (bad article,
    /// extraction failure, invalid data, local I/O, an operation deadline)
    /// fails permanently.
    pub fn is_retryable(&self) -> bool {
        match self {
            PressPodsError::Ai { source, .. } => source.is_retryable(),
            PressPodsError::Http { source, .. } => match source {
                HttpError::Timeout | HttpError::Network(_) => true,
                HttpError::Status { status, .. } => is_retryable_status(*status),
                _ => false,
            },
            PressPodsError::Status { status, .. } => is_retryable_status(*status),
            PressPodsError::Failed { retryable, .. } => retryable.unwrap_or(false),
            PressPodsError::SpeechUnreachable { .. } => true,
            PressPodsError::Timeout { .. }
            | PressPodsError::Store { .. }
            | PressPodsError::Io { .. }
            | PressPodsError::Process { .. }
            | PressPodsError::InvalidData { .. } => false,
        }
    }

    /// The stored failure summary: `"<status>: <body or message>"` when the
    /// cause itself carries a status (TTS/STT status errors, provider errors),
    /// else `"<name>: <message>"` capped at [`SUMMARY_UNITS`]. HTTP client
    /// failures keep the established wording (`HTTPError: Response code 503
    /// (Service Unavailable)`, `RequestError: ...`).
    pub fn summary(&self) -> String {
        match self {
            PressPodsError::Status { status, body, .. } => {
                let body = omni_core::js::utf16_slice(body, 0, 200);
                let detail = if body.is_empty() {
                    self.cause_message()
                } else {
                    body.into_owned()
                };
                return format!("{status}: {detail}");
            }
            // A provider error carries its status and its own message.
            PressPodsError::Ai {
                source: AiError::Provider { status, message },
                ..
            } => return format!("{status}: {message}"),
            PressPodsError::Ai {
                source: AiError::Http(HttpError::Status { status, body }),
                ..
            } => return format!("{status}: {}", omni_core::js::utf16_slice(body, 0, 200)),
            PressPodsError::SpeechUnreachable { .. } => {
                return omni_core::js::utf16_slice(&self.cause_message(), 0, SUMMARY_UNITS)
                    .into_owned();
            }
            _ => {}
        }
        let text = format!("{}: {}", self.cause_name(), self.summary_message());
        omni_core::js::utf16_slice(&text, 0, SUMMARY_UNITS).into_owned()
    }

    /// The message half of [`Self::summary`].
    fn summary_message(&self) -> String {
        match self {
            PressPodsError::Http {
                source: HttpError::Status { status, .. },
                ..
            } => {
                let reason = axum::http::StatusCode::from_u16(*status)
                    .ok()
                    .and_then(|s| s.canonical_reason())
                    .unwrap_or("Unknown");
                format!("Response code {status} ({reason})")
            }
            PressPodsError::Http {
                source: HttpError::Network(message),
                ..
            } => message.clone(),
            _ => self.cause_message(),
        }
    }

    /// The error name the stored summary prints for this cause (consistent
    /// with existing rows).
    fn cause_name(&self) -> &'static str {
        match self {
            PressPodsError::Http { source, .. } => match source {
                HttpError::Timeout => "TimeoutError",
                HttpError::Network(_) => "RequestError",
                HttpError::Status { .. } => "HTTPError",
                _ => "Error",
            },
            PressPodsError::Ai { .. } => "AI_APICallError",
            PressPodsError::Store { .. } => "SqliteError",
            PressPodsError::Timeout { .. } => "TimeoutError",
            _ => "Error",
        }
    }
}

/// Cap on a stored failure summary, in UTF-16 units. Sized so a failed
/// retrieval can name all eight retrievers with distinct 60-character reasons.
const SUMMARY_UNITS: usize = 700;

/// HTTP statuses worth an automatic retry: 429 and 5xx.
pub fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

#[cfg(test)]
mod errors_spec {
    //! Retry classification and summaries for each typed error variant.
    use super::*;

    fn with_status(status: u16) -> PressPodsError {
        PressPodsError::status("call", status, "")
    }

    fn with_response_status(status: u16) -> PressPodsError {
        PressPodsError::http(
            "call",
            HttpError::Status {
                status,
                body: String::new(),
            },
        )
    }

    #[test]
    fn retries_429_and_5xx_statuses() {
        assert!(with_status(429).is_retryable());
        assert!(with_status(500).is_retryable());
        assert!(with_status(503).is_retryable());
    }

    #[test]
    fn does_not_retry_4xx_client_errors() {
        assert!(!with_status(400).is_retryable());
        assert!(!with_status(401).is_retryable());
    }

    #[test]
    fn recognizes_http_client_status_codes() {
        assert!(with_response_status(429).is_retryable());
        assert!(with_response_status(503).is_retryable());
        assert!(!with_response_status(400).is_retryable());
    }

    #[test]
    fn retries_known_network_error_names() {
        let error = PressPodsError::http("call", HttpError::Network("connect failed".into()));
        assert!(error.is_retryable());
        let provider = PressPodsError::ai(
            "call",
            AiError::Http(HttpError::Network("connection reset".into())),
        );
        assert!(provider.is_retryable());
    }

    #[test]
    fn retries_node_network_error_codes() {
        let error = PressPodsError::http("call", HttpError::Network("ECONNRESET".into()));
        assert!(error.is_retryable());
        assert!(PressPodsError::http("call", HttpError::Timeout).is_retryable());
    }

    #[test]
    fn does_not_retry_plain_errors_or_non_errors() {
        assert!(!PressPodsError::failed("call", "nope").is_retryable());
        assert!(!PressPodsError::invalid("call", "string").is_retryable());
    }

    #[test]
    fn includes_status_code_and_body_when_present() {
        let error = PressPodsError::status("call", 500, "internal");
        assert_eq!(error.summary(), "500: internal");
    }

    #[test]
    fn deadline_timeouts_are_permanent() {
        assert!(!PressPodsError::timeout("rate article").is_retryable());
    }

    #[test]
    fn http_client_failures_keep_their_summary_wording() {
        assert_eq!(
            with_response_status(503).summary(),
            "HTTPError: Response code 503 (Service Unavailable)"
        );
        assert_eq!(
            PressPodsError::http("call", HttpError::Network("read ECONNRESET".into())).summary(),
            "RequestError: read ECONNRESET"
        );
        assert_eq!(
            PressPodsError::ai(
                "call",
                AiError::Provider {
                    status: 429,
                    message: "slow down".into()
                }
            )
            .summary(),
            "429: slow down"
        );
    }

    #[test]
    fn connection_failures_against_the_speech_server_name_it_once() {
        let timeout = std::time::Duration::from_secs(240);
        let error = PressPodsError::speech_unreachable(
            "synthesize Higgs chunk",
            "http://user:pw@10.0.0.9:8000/base?key=1",
            &HttpError::Network(
                "tcp connect error: No route to host (http://10.0.0.9:8000)".into(),
            ),
            timeout,
        )
        .unwrap();
        assert!(error.is_speech_unreachable() && error.is_retryable());
        assert!(error.is_speech_connect_failure());
        assert_eq!(
            error.summary(),
            "speech server unreachable at http://10.0.0.9:8000: tcp connect error: No route to host"
        );
        let slow = PressPodsError::speech_unreachable(
            "call",
            "http://10.0.0.9:8000",
            &HttpError::Timeout,
            timeout,
        )
        .unwrap();
        assert!(slow.is_speech_unreachable() && !slow.is_speech_connect_failure());
        assert_eq!(
            slow.summary(),
            "speech server unreachable at http://10.0.0.9:8000: no response within 240 s"
        );
    }

    #[test]
    fn an_answer_from_the_speech_server_is_not_an_outage() {
        let timeout = std::time::Duration::from_secs(5);
        for answered in [
            HttpError::Status {
                status: 400,
                body: "bad request".into(),
            },
            HttpError::Status {
                status: 503,
                body: String::new(),
            },
            HttpError::TooLarge { limit: 1 },
        ] {
            assert!(
                PressPodsError::speech_unreachable("call", "http://h:1", &answered, timeout)
                    .is_none()
            );
        }
    }

    #[test]
    fn falls_back_to_name_and_message() {
        assert_eq!(
            PressPodsError::failed("call", "kaput").summary(),
            "Error: kaput"
        );
    }
}
