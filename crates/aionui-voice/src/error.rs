//! Crate-owned errors; mapped to `ApiError` only in `routes.rs`.

use aionui_db::DbError;

#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    /// The Voice setting (kill switch) is off for this user.
    #[error("voice is disabled")]
    Disabled,
    /// One live call per user (§3.6.1): another device already admitted one.
    #[error("a voice call is already live for this user")]
    Busy,
    /// Today's budget cannot cover another response (§3.5).
    #[error("daily voice budget reached")]
    BudgetReached,
    /// No enabled OpenAI provider with an official base URL and a key.
    #[error("no eligible OpenAI provider is configured for voice")]
    NoProvider,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("rate limited")]
    RateLimited,
    /// The provider refused or failed; the message never contains secrets.
    #[error("voice provider error: {0}")]
    Provider(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<DbError> for VoiceError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound(msg) => VoiceError::NotFound(msg),
            other => VoiceError::Internal(other.to_string()),
        }
    }
}
