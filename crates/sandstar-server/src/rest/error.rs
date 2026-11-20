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
    /// 503 — the engine did not answer in time (R-28); retry later.
    ServiceUnavailable(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AppError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
        };
        let body = serde_json::json!({ "err": message });
        (status, Json(body)).into_response()
    }
}

/// Backward-compatible: `String` errors map to `Internal`.
impl From<String> for AppError {
    fn from(s: String) -> Self {
        // Classify known error patterns
        if super::is_engine_timeout(&s) {
            Self::ServiceUnavailable(s)
        } else if s.contains("not found") || s.contains("NotFound") {
            Self::NotFound(s)
        } else if s.contains("read-only") {
            Self::Forbidden(s)
        } else if s.starts_with("invalid write level")
            || s.starts_with("invalid value for channel")
            || s.starts_with("invalid write duration")
        {
            // Engine input validation (EngineError Display): the request is
            // wrong, not the server (R-10).
            Self::BadRequest(s)
        } else {
            Self::Internal(s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(msg: &str) -> StatusCode {
        AppError::from(msg.to_string()).into_response().status()
    }

    #[test]
    fn engine_timeout_maps_to_503() {
        assert_eq!(
            status_of(super::super::ENGINE_TIMEOUT_ERR),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status_of("engine timeout; write may still be applied"),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn other_engine_errors_keep_their_codes() {
        assert_eq!(
            status_of("engine stopped"),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(status_of("channel 9 not found"), StatusCode::NOT_FOUND);
        assert_eq!(status_of("read-only mode"), StatusCode::FORBIDDEN);
    }

    #[test]
    fn engine_input_validation_maps_to_400() {
        use sandstar_engine::EngineError;
        for e in [
            EngineError::InvalidWriteLevel(0),
            EngineError::NonFiniteValue(7),
            EngineError::InvalidWriteDuration(7, 1e300),
        ] {
            assert_eq!(status_of(&e.to_string()), StatusCode::BAD_REQUEST, "{e}");
        }
        // A config problem on the channel is the server's, not the request's.
        let fault = EngineError::ConversionFault(7, "low 10 is above high 0".into());
        assert_eq!(
            status_of(&fault.to_string()),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
