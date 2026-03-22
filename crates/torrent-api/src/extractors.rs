//! Request parameter extraction helpers.
//!
//! Utilities for parsing path parameters and query strings used across
//! API route handlers.

use torrent::core::{Id20, Id32};

use crate::error::{ApiError, ApiResult};

/// Parse an info hash from a hex string path parameter.
///
/// Accepts:
/// - **40-char hex** — interpreted as SHA-1, returns `Id20` directly.
/// - **64-char hex** — validated as SHA-256. Session lookup from `Id32` to
///   `Id20` is not yet implemented, so this currently returns an error after
///   validating the hex format.
///
/// # Errors
///
/// Returns [`ApiError`] (400 Bad Request) if the string is not valid hex,
/// has an unexpected length, or is a 64-char SHA-256 hash (not yet supported).
pub fn parse_info_hash(hash: &str) -> ApiResult<Id20> {
    match hash.len() {
        40 => Id20::from_hex(hash)
            .map_err(|_| ApiError::bad_request(format!("invalid SHA-1 info hash: {hash}"))),
        64 => {
            // Validate the hex is well-formed even though we cannot resolve
            // an Id32 to an Id20 without a session lookup.
            Id32::from_hex(hash)
                .map_err(|_| ApiError::bad_request(format!("invalid SHA-256 info hash: {hash}")))?;
            Err(ApiError::bad_request(
                "SHA-256 info hash lookup not yet supported",
            ))
        }
        _ => Err(ApiError::bad_request(format!(
            "info hash must be 40 or 64 hex chars, got {}",
            hash.len()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_sha1_hex_returns_id20() {
        let hex = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let id = parse_info_hash(hex).expect("valid 40-char hex should succeed");
        assert_eq!(id.to_hex(), hex);
    }

    #[test]
    fn invalid_sha1_hex_returns_bad_request() {
        let hex = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"; // 40 chars, not hex
        let err = parse_info_hash(hex).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("invalid SHA-1 info hash"));
    }

    #[test]
    fn valid_sha256_hex_returns_unsupported() {
        let hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let err = parse_info_hash(hex).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("not yet supported"));
    }

    #[test]
    fn invalid_sha256_hex_returns_bad_request() {
        let hex = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
        let err = parse_info_hash(hex).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("invalid SHA-256 info hash"));
    }

    #[test]
    fn wrong_length_returns_bad_request() {
        let err = parse_info_hash("abcdef").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("got 6"));
    }

    #[test]
    fn empty_string_returns_bad_request() {
        let err = parse_info_hash("").unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("got 0"));
    }
}
