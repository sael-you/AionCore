//! Crate-owned error; routes map it to `ApiError` with the wire code.

use aionui_api_types::BrowserErrorCode;

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    /// A failure the panel / settings UI can show by its code.
    #[error("{1}")]
    Coded(BrowserErrorCode, String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("cdp: {0}")]
    Cdp(String),
    #[error("{0}")]
    Internal(String),
}

impl BrowserError {
    pub fn coded(code: BrowserErrorCode, message: impl Into<String>) -> Self {
        Self::Coded(code, message.into())
    }

    /// The wire code, if this error has one.
    pub fn code(&self) -> Option<BrowserErrorCode> {
        match self {
            Self::Coded(code, _) => Some(*code),
            _ => None,
        }
    }
}
