use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized: missing or invalid bearer token")]
    Unauthorized,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("library not found: {0}")]
    LibraryNotFound(String),
    #[error("job not found: {0}")]
    JobNotFound(String),
    #[error("query failed: {0}")]
    QueryFailed(String),
    #[error("agent runtime failed: {0}")]
    Runtime(String),
    #[error("internal storage error: {0}")]
    Storage(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl AppError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::LibraryNotFound(_) | Self::JobNotFound(_) => StatusCode::NOT_FOUND,
            Self::QueryFailed(_) | Self::Runtime(_) => StatusCode::BAD_GATEWAY,
            Self::Storage(_) | Self::Io(_) | Self::Sqlite(_) | Self::Json(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status_code(),
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response()
    }
}

impl From<opencode_rs::OpencodeError> for AppError {
    fn from(error: opencode_rs::OpencodeError) -> Self {
        Self::Runtime(error.to_string())
    }
}

impl From<walkdir::Error> for AppError {
    fn from(error: walkdir::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<std::path::StripPrefixError> for AppError {
    fn from(error: std::path::StripPrefixError) -> Self {
        Self::Storage(error.to_string())
    }
}
