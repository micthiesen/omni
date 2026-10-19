//! Gateway failures with a kind and a next step for the owner.

use omni_api::gateway::{GatewayErrorKind, GatewayErrorView};
use omni_store::StoreError;
use omni_store::cbor::Extra;

use super::client::ClientError;
use super::model::ErrorRow;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct GatewayError {
    pub kind: GatewayErrorKind,
    pub message: String,
    pub hint: Option<String>,
    /// A call was sent and may have run (timeout, cut connection).
    pub outcome_unknown: bool,
}

impl GatewayError {
    pub fn new(kind: GatewayErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            hint: None,
            outcome_unknown: false,
        }
    }

    /// Marks a sent call whose outcome is unknown, so nobody retries it blindly.
    pub fn uncertain(mut self) -> Self {
        self.outcome_unknown = true;
        self.hint = Some(
            "The call may still have run: check whether it took effect before retrying.".into(),
        );
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(GatewayErrorKind::Config, message)
    }

    pub fn needs_auth(&self) -> bool {
        matches!(
            self.kind,
            GatewayErrorKind::AuthRequired | GatewayErrorKind::AuthRejected
        )
    }

    pub fn to_row(&self, at: i64) -> ErrorRow {
        ErrorRow {
            kind: kind_name(self.kind).to_owned(),
            message: self.message.clone(),
            hint: self.hint.clone(),
            at,
            extra: Extra::default(),
        }
    }

    /// `message` plus the hint, for tool results.
    pub fn describe(&self) -> String {
        match &self.hint {
            Some(hint) => format!("{} {hint}", self.message),
            None => self.message.clone(),
        }
    }
}

pub fn kind_name(kind: GatewayErrorKind) -> &'static str {
    match kind {
        GatewayErrorKind::Network => "network",
        GatewayErrorKind::Timeout => "timeout",
        GatewayErrorKind::AuthRequired => "auth_required",
        GatewayErrorKind::AuthRejected => "auth_rejected",
        GatewayErrorKind::Protocol => "protocol",
        GatewayErrorKind::Upstream => "upstream",
        GatewayErrorKind::Process => "process",
        GatewayErrorKind::Config => "config",
        GatewayErrorKind::Unsupported => "unsupported",
    }
}

fn kind_from_name(name: &str) -> GatewayErrorKind {
    match name {
        "network" => GatewayErrorKind::Network,
        "timeout" => GatewayErrorKind::Timeout,
        "auth_required" => GatewayErrorKind::AuthRequired,
        "auth_rejected" => GatewayErrorKind::AuthRejected,
        "protocol" => GatewayErrorKind::Protocol,
        "upstream" => GatewayErrorKind::Upstream,
        "process" => GatewayErrorKind::Process,
        "unsupported" => GatewayErrorKind::Unsupported,
        _ => GatewayErrorKind::Config,
    }
}

pub fn view(row: &ErrorRow) -> GatewayErrorView {
    GatewayErrorView {
        kind: kind_from_name(&row.kind),
        message: row.message.clone(),
        hint: row.hint.clone(),
        at: row.at,
    }
}

impl From<StoreError> for GatewayError {
    fn from(error: StoreError) -> Self {
        GatewayError::new(
            GatewayErrorKind::Config,
            format!("Omni storage failed: {error}"),
        )
    }
}

/// The gateway's view of a transport failure; `oauth` selects the auth hint.
pub fn from_client(error: &ClientError, oauth: bool) -> GatewayError {
    use GatewayErrorKind as K;
    let reconnect = if oauth {
        "Reconnect it on Omni's MCP gateway page."
    } else {
        "Check its credentials on Omni's MCP gateway page."
    };
    match error {
        ClientError::Unauthorized { .. } => {
            GatewayError::new(K::AuthRequired, "The integration needs authentication.")
                .hint(reconnect)
        }
        ClientError::Forbidden { .. } => GatewayError::new(
            K::AuthRejected,
            "The integration refused the stored credentials (HTTP 403).",
        )
        .hint(reconnect),
        ClientError::Timeout => {
            GatewayError::new(K::Timeout, "The integration did not answer in time.")
                .hint("Retry, or raise the integration's timeout if its tools are slow.")
        }
        ClientError::Cancelled => GatewayError::new(K::Timeout, "The request was cancelled."),
        ClientError::Network(message) => GatewayError::new(
            K::Network,
            format!("Could not reach the integration: {message}"),
        )
        .hint("Check that the integration is up and its URL is right."),
        ClientError::Http { status, body } if *status >= 500 => GatewayError::new(
            K::Upstream,
            format!("The integration failed (HTTP {status}){}", excerpt(body)),
        )
        .hint("This is usually temporary; retry shortly."),
        ClientError::Http { status, body } => GatewayError::new(
            K::Protocol,
            format!(
                "The integration rejected the request (HTTP {status}){}",
                excerpt(body)
            ),
        ),
        ClientError::SessionExpired => {
            GatewayError::new(K::Protocol, "The integration's session expired.")
        }
        ClientError::Protocol(message) => {
            GatewayError::new(K::Protocol, format!("Unexpected response: {message}"))
        }
        ClientError::TooLarge(limit) => GatewayError::new(
            K::Protocol,
            format!(
                "The integration's response exceeds {} MiB.",
                limit / (1024 * 1024)
            ),
        ),
        ClientError::Process(message) => GatewayError::new(K::Process, message.clone())
            .hint("Check the server's variables on Omni's MCP gateway page."),
        ClientError::Blocked(message) => GatewayError::new(
            K::Config,
            format!("Omni will not connect to this URL: {message}"),
        )
        .hint("Integrations must use a public HTTPS address."),
        ClientError::Unsupported(message) => GatewayError::new(K::Unsupported, message.clone()),
    }
}

fn excerpt(body: &str) -> String {
    let text: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        String::new()
    } else {
        format!(": {}", text.chars().take(200).collect::<String>())
    }
}
