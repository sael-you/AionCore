use aionui_db::DbError;

/// Market service errors. Messages name keys, ids and rules, never a value.
#[derive(Debug, thiserror::Error)]
pub enum MarketError {
    /// A rule (R1–R11) or input check failed (HTTP 400, `MCP_MARKET_INVALID`).
    #[error("{0}")]
    Invalid(String),
    /// A template refresh needs a required secret that is not set yet
    /// (HTTP 409, `MCP_TEMPLATE_SECRET_MISSING`); the instance is unchanged.
    #[error("{0}")]
    TemplateSecretMissing(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    /// The caller's credential channel may not perform this write (HTTP 403).
    #[error("{0}")]
    Forbidden(String),
    /// The catalogue or another dependency is not available (HTTP 503).
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("internal error: {0}")]
    Internal(String),
    /// A kit request refused with a kit-specific code (C27).
    #[error("{1}")]
    Kit(aionui_api_types::KitErrorCode, String),
}

pub(crate) fn kit_error(code: aionui_api_types::KitErrorCode, msg: impl Into<String>) -> MarketError {
    MarketError::Kit(code, msg.into())
}

impl From<DbError> for MarketError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound(msg) => Self::NotFound(msg),
            DbError::Conflict(msg) => Self::Conflict(msg),
            // Query/IO details stay in the log, not in responses.
            other => {
                tracing::error!(error = %other, "mcp market: database error");
                Self::Internal("database error".to_owned())
            }
        }
    }
}

impl From<serde_json::Error> for MarketError {
    fn from(err: serde_json::Error) -> Self {
        Self::Internal(format!("invalid stored JSON: {err}"))
    }
}

pub(crate) fn invalid(msg: impl Into<String>) -> MarketError {
    MarketError::Invalid(msg.into())
}
