//! Recipe failures. Every expected rejection carries a stable code that
//! tools render as `[code] message` (docs/recipes.md, "Errors").

use omni_store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum RecipesError {
    /// An expected rejection: `invalid_recipe`, `revision_conflict`,
    /// `op_failed`, `duplicate_source`, `unknown_tag`, `not_found`, `merged`,
    /// `undo_conflict`, `too_large`, `unsupported_media`, `fetch_failed`,
    /// `session_closed`, `validation`, `archive`.
    #[error("{message}")]
    Rejected { code: &'static str, message: String },
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("filesystem: {0}")]
    Io(String),
    #[error("stored data could not be decoded: {0}")]
    Corrupt(String),
}

pub type R<T> = Result<T, RecipesError>;

impl RecipesError {
    pub fn code(&self) -> &'static str {
        match self {
            RecipesError::Rejected { code, .. } => code,
            RecipesError::Store(_) => "store",
            RecipesError::Io(_) => "io",
            RecipesError::Corrupt(_) => "corrupt",
        }
    }

    /// `[code] message`, the tool-facing text.
    pub fn tool_text(&self) -> String {
        format!("[{}] {self}", self.code())
    }

    /// Whether the caller, not the server, is at fault.
    pub fn is_rejection(&self) -> bool {
        matches!(self, RecipesError::Rejected { .. })
    }
}

/// An expected rejection with `code`.
pub fn reject(code: &'static str, message: impl Into<String>) -> RecipesError {
    RecipesError::Rejected {
        code,
        message: message.into(),
    }
}

pub fn invalid(message: impl Into<String>) -> RecipesError {
    reject("validation", message)
}

pub fn not_found(message: impl Into<String>) -> RecipesError {
    reject("not_found", message)
}

pub fn io(error: impl std::fmt::Display) -> RecipesError {
    RecipesError::Io(error.to_string())
}

pub fn corrupt(error: impl std::fmt::Display) -> RecipesError {
    RecipesError::Corrupt(error.to_string())
}

pub fn sql(error: rusqlite::Error) -> RecipesError {
    RecipesError::Store(StoreError::Sqlite(error.to_string()))
}
