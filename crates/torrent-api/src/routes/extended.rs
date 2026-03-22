//! Extended torrent and peer management endpoint handlers.
//!
//! Provides handlers for detailed torrent inspection (info, peers, trackers),
//! torrent operations (reannounce, file priority), and peer ban management.

use std::net::IpAddr;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::error::{ApiError, ApiResult};

use super::AppState;

/// Get detailed metadata and state for a single torrent.
///
/// Returns a JSON [`TorrentInfo`](torrent::session::TorrentInfo) object
/// containing the torrent's name, files, piece count, and current state.
pub async fn get_torrent_info(
    State(session): State<AppState>,
    Path(hash): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = crate::extractors::parse_info_hash(&hash)?;
    let info = session.torrent_info(id).await?;
    Ok(Json(info))
}

/// Get the list of connected peers for a torrent.
///
/// Returns a JSON array of [`PeerInfo`](torrent::session::PeerInfo) objects,
/// one per connected peer, including address, client ID, and transfer stats.
pub async fn get_peers(
    State(session): State<AppState>,
    Path(hash): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = crate::extractors::parse_info_hash(&hash)?;
    let peers = session.get_peer_info(id).await?;
    Ok(Json(peers))
}

/// Get the tracker list for a torrent.
///
/// Returns a JSON array of [`TrackerInfo`](torrent::session::TrackerInfo)
/// objects with each tracker's URL, tier, and announce status.
pub async fn get_trackers(
    State(session): State<AppState>,
    Path(hash): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = crate::extractors::parse_info_hash(&hash)?;
    let trackers = session.tracker_list(id).await?;
    Ok(Json(trackers))
}

/// Force a re-announce to all trackers for a torrent.
///
/// Returns **204 No Content** on success.
pub async fn reannounce(
    State(session): State<AppState>,
    Path(hash): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let id = crate::extractors::parse_info_hash(&hash)?;
    session.force_reannounce(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// JSON request body for the set-file-priority endpoint.
#[derive(serde::Deserialize)]
pub struct SetPriorityRequest {
    priority: torrent::core::FilePriority,
}

/// Set the download priority for a single file within a torrent.
///
/// The `hash` path segment identifies the torrent and `idx` identifies the
/// zero-based file index. The request body must contain a JSON object with
/// a `priority` field set to one of the [`FilePriority`](torrent::core::FilePriority)
/// variants (`"Skip"`, `"Low"`, `"Normal"`, `"High"`).
///
/// Returns **204 No Content** on success.
pub async fn set_file_priority(
    State(session): State<AppState>,
    Path((hash, idx)): Path<(String, usize)>,
    Json(req): Json<SetPriorityRequest>,
) -> ApiResult<impl IntoResponse> {
    let id = crate::extractors::parse_info_hash(&hash)?;
    session.set_file_priority(id, idx, req.priority).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Get the list of currently banned peer IP addresses.
///
/// Returns a JSON array of [`IpAddr`] values.
pub async fn get_banned_peers(State(session): State<AppState>) -> ApiResult<impl IntoResponse> {
    let banned = session.banned_peers().await?;
    Ok(Json(banned))
}

/// JSON request body for the ban-peer endpoint.
#[derive(serde::Deserialize)]
pub struct BanPeerRequest {
    ip: IpAddr,
}

/// Ban a peer by IP address.
///
/// Accepts a JSON body with an `ip` field containing the address to ban.
///
/// Returns **204 No Content** on success.
pub async fn ban_peer(
    State(session): State<AppState>,
    Json(req): Json<BanPeerRequest>,
) -> ApiResult<impl IntoResponse> {
    session.ban_peer(req.ip).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove a peer IP address from the ban list.
///
/// The `ip` path segment is parsed as an [`IpAddr`]. Both IPv4 and IPv6
/// addresses are accepted.
///
/// Returns **204 No Content** on success (even if the IP was not banned).
pub async fn unban_peer(
    State(session): State<AppState>,
    Path(ip_str): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let ip: IpAddr = ip_str
        .parse()
        .map_err(|_| ApiError::bad_request(format!("invalid IP address: {ip_str}")))?;
    session.unban_peer(ip).await?;
    Ok(StatusCode::NO_CONTENT)
}
