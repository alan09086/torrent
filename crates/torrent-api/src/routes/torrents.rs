//! Torrent CRUD endpoint handlers.
//!
//! Provides handlers for listing, adding, inspecting, removing, pausing,
//! and resuming torrents via the REST API.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;

use crate::error::{ApiError, ApiResult};

use super::AppState;

/// JSON request body for magnet URI additions.
#[derive(serde::Deserialize)]
struct AddMagnetRequest {
    uri: String,
}

/// List all active torrents.
///
/// Returns a JSON array of [`TorrentSummary`](torrent::session::TorrentSummary)
/// objects, one per torrent managed by the session.
pub async fn list_torrents(State(session): State<AppState>) -> ApiResult<impl IntoResponse> {
    let summaries = session.list_torrent_summaries().await?;
    Ok(Json(summaries))
}

/// Get detailed statistics for a single torrent.
///
/// The `hash` path parameter must be a 40-character hex-encoded SHA-1
/// info hash (64-character SHA-256 hashes are validated but not yet
/// supported for lookup).
pub async fn get_torrent(
    State(session): State<AppState>,
    Path(hash): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = crate::extractors::parse_info_hash(&hash)?;
    let stats = session.torrent_stats(id).await?;
    Ok(Json(stats))
}

/// Add a torrent via magnet URI or raw `.torrent` bytes.
///
/// Dispatch logic:
/// - If `Content-Type` starts with `application/json`, the body is parsed
///   as `{ "uri": "magnet:?..." }` and [`SessionHandle::add_magnet_uri`] is
///   called.
/// - Otherwise the body is treated as raw `.torrent` file bytes and
///   [`SessionHandle::add_torrent_bytes`] is called.
///
/// Returns **201 Created** with the resulting [`InfoHashes`](torrent::core::InfoHashes)
/// as JSON on success.
pub async fn add_torrent(
    State(session): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<impl IntoResponse> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));

    let info_hashes = if is_json {
        let req: AddMagnetRequest = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid JSON: {e}")))?;
        session.add_magnet_uri(&req.uri).await?
    } else {
        if body.is_empty() {
            return Err(ApiError::bad_request("empty request body"));
        }
        session.add_torrent_bytes(&body).await?
    };

    Ok((StatusCode::CREATED, Json(info_hashes)))
}

/// Remove a torrent from the session.
///
/// Returns **204 No Content** on success.
pub async fn delete_torrent(
    State(session): State<AppState>,
    Path(hash): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = crate::extractors::parse_info_hash(&hash)?;
    session.remove_torrent(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Pause an active torrent.
///
/// Returns **204 No Content** on success.
pub async fn pause_torrent(
    State(session): State<AppState>,
    Path(hash): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = crate::extractors::parse_info_hash(&hash)?;
    session.pause_torrent(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Resume a paused torrent.
///
/// Returns **204 No Content** on success.
pub async fn resume_torrent(
    State(session): State<AppState>,
    Path(hash): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = crate::extractors::parse_info_hash(&hash)?;
    session.resume_torrent(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
