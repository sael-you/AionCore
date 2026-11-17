//! Crate-owned error; routes map it to `ApiError` with the wire code.

use aionui_api_types::devices::DeviceErrorCode;

#[derive(Debug, thiserror::Error)]
pub enum DevicesError {
    #[error("{1}")]
    Coded(DeviceErrorCode, String),
    #[error("database: {0}")]
    Db(#[from] aionui_db::DbError),
    #[error("{0}")]
    Internal(String),
}

impl DevicesError {
    pub fn coded(code: DeviceErrorCode, message: impl Into<String>) -> Self {
        Self::Coded(code, message.into())
    }

    pub fn not_found() -> Self {
        Self::coded(DeviceErrorCode::DeviceNotFound, "no such device")
    }

    pub fn code(&self) -> Option<DeviceErrorCode> {
        match self {
            Self::Coded(code, _) => Some(*code),
            _ => None,
        }
    }
}
