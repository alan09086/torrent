//! API error types and HTTP status code mapping.
//!
//! [`ApiError`] wraps session-layer errors and maps them to appropriate
//! HTTP status codes, producing a consistent JSON error response format.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// API error type with HTTP status mapping.
///
/// Each variant of the underlying session error is mapped to an HTTP status
/// code and a machine-readable code string. The error is serialized as:
///
/// ```json
/// { "error": "human message", "code": "NOT_FOUND" }
/// ```
#[derive(Debug)]
pub struct ApiError {
    pub(crate) status: StatusCode,
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

/// Convenience alias for API handler results.
pub type ApiResult<T> = Result<T, ApiError>;

/// JSON body for error responses.
#[derive(Serialize)]
struct ErrorBody {
    error: String,
    code: &'static str,
}

impl ApiError {
    /// Create a 400 Bad Request error from the API layer.
    ///
    /// Use this for request-level validation failures (e.g. malformed
    /// info hash hex, missing required fields) that do not originate
    /// from the session.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "INVALID_REQUEST",
            message: message.into(),
        }
    }

    /// Create a 404 Not Found error.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "NOT_FOUND",
            message: message.into(),
        }
    }

    /// Create a 501 Not Implemented error.
    ///
    /// Use this when a valid request targets a feature that exists in the
    /// protocol but has not yet been implemented (e.g. SHA-256 info hash
    /// lookup).
    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_IMPLEMENTED,
            code: "NOT_IMPLEMENTED",
            message: message.into(),
        }
    }

    /// Create a 500 Internal Server Error.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "INTERNAL_ERROR",
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: self.message,
            code: self.code,
        };
        (self.status, axum::Json(body)).into_response()
    }
}

impl From<torrent::session::Error> for ApiError {
    fn from(e: torrent::session::Error) -> Self {
        use torrent::session::Error;

        let (status, code) = match &e {
            Error::TorrentNotFound(_) => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            Error::DuplicateTorrent(_) => (StatusCode::CONFLICT, "DUPLICATE_TORRENT"),
            Error::InvalidSettings(_) | Error::Config(_) => {
                (StatusCode::BAD_REQUEST, "INVALID_REQUEST")
            }
            Error::MetadataNotReady(_) => (StatusCode::NOT_FOUND, "METADATA_NOT_READY"),
            Error::Shutdown => (StatusCode::SERVICE_UNAVAILABLE, "SHUTTING_DOWN"),
            Error::SessionAtCapacity(_) => (StatusCode::SERVICE_UNAVAILABLE, "SESSION_AT_CAPACITY"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
        };

        Self {
            status,
            code,
            message: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn bad_request_has_correct_status_and_code() {
        let err = ApiError::bad_request("invalid hex");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.code, "INVALID_REQUEST");
        assert_eq!(err.message, "invalid hex");
    }

    #[tokio::test]
    async fn into_response_produces_json() {
        let err = ApiError::bad_request("test error");
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let content_type = response
            .headers()
            .get("content-type")
            .expect("should have content-type header");
        assert!(
            content_type
                .to_str()
                .expect("content-type should be valid str")
                .contains("application/json"),
            "expected application/json content-type"
        );

        let body = to_bytes(response.into_body(), 4096)
            .await
            .expect("should read body");
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).expect("should be valid JSON");

        assert_eq!(parsed["error"], "test error");
        assert_eq!(parsed["code"], "INVALID_REQUEST");
    }

    #[test]
    fn torrent_not_found_maps_to_404() {
        let session_err = torrent::session::Error::TorrentNotFound(torrent::core::Id20::ZERO);
        let api_err = ApiError::from(session_err);
        assert_eq!(api_err.status, StatusCode::NOT_FOUND);
        assert_eq!(api_err.code, "NOT_FOUND");
    }

    #[test]
    fn duplicate_torrent_maps_to_409() {
        let session_err = torrent::session::Error::DuplicateTorrent(torrent::core::Id20::ZERO);
        let api_err = ApiError::from(session_err);
        assert_eq!(api_err.status, StatusCode::CONFLICT);
        assert_eq!(api_err.code, "DUPLICATE_TORRENT");
    }

    #[test]
    fn invalid_settings_maps_to_400() {
        let session_err = torrent::session::Error::InvalidSettings("bad value".to_owned());
        let api_err = ApiError::from(session_err);
        assert_eq!(api_err.status, StatusCode::BAD_REQUEST);
        assert_eq!(api_err.code, "INVALID_REQUEST");
    }

    #[test]
    fn config_error_maps_to_400() {
        let session_err = torrent::session::Error::Config("bad config".to_owned());
        let api_err = ApiError::from(session_err);
        assert_eq!(api_err.status, StatusCode::BAD_REQUEST);
        assert_eq!(api_err.code, "INVALID_REQUEST");
    }

    #[test]
    fn metadata_not_ready_maps_to_404() {
        let session_err = torrent::session::Error::MetadataNotReady(torrent::core::Id20::ZERO);
        let api_err = ApiError::from(session_err);
        assert_eq!(api_err.status, StatusCode::NOT_FOUND);
        assert_eq!(api_err.code, "METADATA_NOT_READY");
    }

    #[test]
    fn shutdown_maps_to_503() {
        let session_err = torrent::session::Error::Shutdown;
        let api_err = ApiError::from(session_err);
        assert_eq!(api_err.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(api_err.code, "SHUTTING_DOWN");
    }

    #[test]
    fn io_error_maps_to_500() {
        let session_err = torrent::session::Error::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "disk failed",
        ));
        let api_err = ApiError::from(session_err);
        assert_eq!(api_err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(api_err.code, "INTERNAL_ERROR");
    }
}
