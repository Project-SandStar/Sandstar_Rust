//! REST API error handling with granular HTTP status codes.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// Application error type for REST handlers.
pub enum AppError {
    /// 404 — resource not found (e.g., channel does not exist).
    NotFound(String),
    /// 400 — invalid request (e.g., bad filter syntax, missing param).
    BadRequest(String),
    /// 403 — action forbidden (e.g., write rejected in read-only mode).
    Forbidden(String),
    /// 500 — internal server error.
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        let body = serde_json::json!({ "err": message });
        (status, Json(body)).into_response()
    }
}

/// Backward-compatible: `String` errors map to `Internal`.
impl From<String> for AppError {
    fn from(s: String) -> Self {
        // Classify known error patterns
        if s.contains("not found") || s.contains("NotFound") {
            Self::NotFound(s)
        } else if s.contains("read-only") {
            Self::Forbidden(s)
        } else {
            Self::Internal(s)
        }
    }
}
