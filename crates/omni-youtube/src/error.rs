//! YouTube failures. Every message names the next step; none carries a token,
//! authorization code or client secret.

use omni_http::HttpError;
use omni_store::StoreError;

/// A failed YouTube Data API call, decoded from Google's error body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiFailure {
    /// The API method, such as `playlists.list`.
    pub method: &'static str,
    pub status: u16,
    /// Google's `errors[0].reason` (or the `ErrorInfo` reason), such as `quotaExceeded`.
    pub reason: Option<String>,
    /// Google's message, bounded.
    pub message: String,
    /// Quota context added for quota failures.
    pub quota_note: Option<String>,
}

/// What an [`ApiFailure`] means for the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// The project's daily quota is used up (resets at midnight Pacific).
    QuotaExceeded,
    /// A per-minute or per-user rate limit; retry later.
    RateLimited,
    /// The YouTube Data API is not enabled in the Google Cloud project.
    ApiDisabled,
    NotFound,
    Forbidden,
    Invalid,
    Unauthorized,
    Server,
    Other,
}

impl ApiFailure {
    pub fn kind(&self) -> FailureKind {
        let reason = self.reason.as_deref().unwrap_or("");
        match reason {
            "quotaExceeded" | "dailyLimitExceeded" | "dailyLimitExceededUnreg" => {
                FailureKind::QuotaExceeded
            }
            "rateLimitExceeded" | "userRateLimitExceeded" | "RATE_LIMIT_EXCEEDED" => {
                FailureKind::RateLimited
            }
            "accessNotConfigured" | "SERVICE_DISABLED" => FailureKind::ApiDisabled,
            _ => match self.status {
                401 => FailureKind::Unauthorized,
                403 => FailureKind::Forbidden,
                404 => FailureKind::NotFound,
                429 => FailureKind::RateLimited,
                400 | 409 | 412 | 422 => FailureKind::Invalid,
                s if s >= 500 => FailureKind::Server,
                _ => FailureKind::Other,
            },
        }
    }

    fn hint(&self) -> &'static str {
        match (self.kind(), self.reason.as_deref()) {
            (FailureKind::QuotaExceeded, _) => {
                "The Google Cloud project's daily YouTube quota is used up; it resets at midnight Pacific time. Do not retry before then."
            }
            (FailureKind::RateLimited, _) => "Rate limited; retry in a minute.",
            (FailureKind::ApiDisabled, _) => {
                "Enable \"YouTube Data API v3\" in the Google Cloud project that owns the OAuth client."
            }
            (_, Some("manualSortRequired")) => {
                "The playlist is sorted automatically; set its sort to Manual in YouTube before moving or positioning items."
            }
            (_, Some("playlistContainsMaximumNumberOfVideos")) => {
                "The playlist has YouTube's maximum number of videos."
            }
            (_, Some("youtubeSignupRequired")) => {
                "The connected Google account has no YouTube channel; create one, then reconnect."
            }
            (_, Some("insufficientPermissions")) => {
                "The grant lacks the youtube scope; reconnect and allow access."
            }
            (FailureKind::NotFound, _) => {
                "Check the id; private items of other channels are not visible."
            }
            (FailureKind::Forbidden, _) => "YouTube refused the request for this account or item.",
            (FailureKind::Server, _) => "A YouTube server error; retry later.",
            _ => "",
        }
    }
}

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "YouTube {} failed (HTTP {}", self.method, self.status)?;
        if let Some(reason) = &self.reason {
            write!(f, ", {reason}")?;
        }
        write!(f, ")")?;
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        let hint = self.hint();
        if !hint.is_empty() {
            write!(f, ". {hint}")?;
        }
        if let Some(note) = &self.quota_note {
            write!(f, " {note}")?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum YoutubeError {
    #[error(
        "YouTube is not configured: set YOUTUBE_CLIENT_ID and YOUTUBE_CLIENT_SECRET in Omni's environment"
    )]
    NotConfigured,
    #[error(
        "no YouTube account is connected: open {connect_url} in a browser on the home network to connect one"
    )]
    NotConnected { connect_url: String },
    #[error(
        "the YouTube connection must be renewed ({reason}): open {connect_url} in a browser on the home network to reconnect"
    )]
    Reconnect { reason: String, connect_url: String },
    #[error(
        "Google rejected Omni's OAuth client ({0}): check YOUTUBE_CLIENT_ID and YOUTUBE_CLIENT_SECRET"
    )]
    Client(String),
    #[error("{0}")]
    Api(ApiFailure),
    #[error("YouTube request failed: {0}")]
    Http(HttpError),
    /// Refreshing the access token failed in transit; nothing was sent to YouTube.
    #[error("refreshing the YouTube access token failed ({0}); nothing was sent, retry later")]
    TokenRefresh(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Uncertain(String),
    #[error("YouTube returned an unexpected response: {0}")]
    Decode(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl YoutubeError {
    /// A stable code, shown as the `[code]` prefix of tool errors.
    pub fn code(&self) -> &'static str {
        match self {
            YoutubeError::NotConfigured => "not_configured",
            YoutubeError::NotConnected { .. } => "not_connected",
            YoutubeError::Reconnect { .. } => "reconnect_required",
            YoutubeError::Client(_) => "oauth_client_rejected",
            YoutubeError::Api(failure) => match failure.kind() {
                FailureKind::QuotaExceeded => "quota_exceeded",
                FailureKind::RateLimited => "rate_limited",
                FailureKind::ApiDisabled => "api_disabled",
                FailureKind::NotFound => "not_found",
                FailureKind::Forbidden => "forbidden",
                FailureKind::Invalid => "invalid_request",
                FailureKind::Unauthorized => "unauthorized",
                FailureKind::Server => "youtube_unavailable",
                FailureKind::Other => "youtube_error",
            },
            YoutubeError::Http(_) => "network",
            YoutubeError::TokenRefresh(_) => "network",
            YoutubeError::NotFound(_) => "not_found",
            YoutubeError::Invalid(_) => "invalid_input",
            YoutubeError::Refused(_) => "refused",
            YoutubeError::Uncertain(_) => "uncertain",
            YoutubeError::Decode(_) => "unexpected_response",
            YoutubeError::Store(_) => "store",
        }
    }

    /// Whether the same call may succeed later without any change.
    pub fn is_transient(&self) -> bool {
        match self {
            YoutubeError::Http(error) => error.is_transient(),
            YoutubeError::TokenRefresh(_) => true,
            YoutubeError::Api(failure) => matches!(
                failure.kind(),
                FailureKind::RateLimited | FailureKind::Server
            ),
            _ => false,
        }
    }

    /// Whether a write may have run although no answer arrived: settle by reading.
    pub fn outcome_unknown(&self) -> bool {
        match self {
            YoutubeError::Http(error) => matches!(
                error,
                HttpError::Timeout
                    | HttpError::Network(_)
                    | HttpError::TooLarge { .. }
                    | HttpError::Decode(_)
            ),
            YoutubeError::Api(failure) => failure.kind() == FailureKind::Server,
            YoutubeError::Decode(_) => true,
            _ => false,
        }
    }

    pub fn is_quota(&self) -> bool {
        matches!(self, YoutubeError::Api(f) if f.kind() == FailureKind::QuotaExceeded)
    }

    pub fn is_not_found(&self) -> bool {
        match self {
            YoutubeError::NotFound(_) => true,
            YoutubeError::Api(f) => f.kind() == FailureKind::NotFound,
            _ => false,
        }
    }
}
