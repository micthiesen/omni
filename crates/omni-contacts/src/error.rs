use omni_store::StoreError;

use crate::dav::ReadError;
use crate::edit::EditError;

/// A contacts operation failed. Tool errors carry the code in brackets.
#[derive(Debug, thiserror::Error)]
pub enum ContactsError {
    #[error("[not_configured] iCloud contacts need ICLOUD_USERNAME and ICLOUD_APP_PASSWORD")]
    NotConfigured,
    #[error("[{code}] {message}")]
    Coded { code: &'static str, message: String },
    #[error("[transport] {message}")]
    Transport { message: String, transient: bool },
    #[error("[store] {0}")]
    Store(#[from] StoreError),
}

impl ContactsError {
    pub fn coded(code: &'static str, message: impl Into<String>) -> Self {
        Self::Coded {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::coded("invalid_input", message)
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::coded("protocol", message)
    }

    pub fn code(&self) -> &str {
        match self {
            ContactsError::NotConfigured => "not_configured",
            ContactsError::Coded { code, .. } => code,
            ContactsError::Transport { .. } => "transport",
            ContactsError::Store(_) => "store",
        }
    }

    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ContactsError::Transport {
                transient: true,
                ..
            }
        )
    }

    /// The HTTP status of a failed read as an error.
    pub fn status(status: u16, what: &str) -> Self {
        match status {
            401 | 403 => Self::coded(
                "unauthorized",
                format!("iCloud refused {what} (HTTP {status}); check the app-specific password"),
            ),
            404 => Self::coded("not_found", format!("{what}: not found")),
            s => Self::Transport {
                message: format!("{what} failed: HTTP {s}"),
                transient: s >= 500 || s == 429,
            },
        }
    }
}

impl From<EditError> for ContactsError {
    fn from(error: EditError) -> Self {
        Self::coded(error.code(), error.to_string())
    }
}

impl From<ReadError> for ContactsError {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::TooLarge(limit) => Self::coded(
                "too_large",
                format!("the iCloud response exceeds {limit} bytes"),
            ),
            ReadError::Untrusted(origin) => Self::coded(
                "untrusted_host",
                format!("refusing to send iCloud credentials to {origin}"),
            ),
            ReadError::Transport(message) => Self::Transport {
                message,
                transient: true,
            },
        }
    }
}
