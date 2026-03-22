//! HTTP route definitions for the torrent REST API.

pub mod events;
pub mod extended;
pub mod session;
pub mod torrents;

use std::sync::Arc;

use axum::Router;
use axum::routing::{any, delete, get, patch, post};
use torrent::session::SessionHandle;

/// Shared application state passed to every handler via axum's `State` extractor.
pub(crate) type AppState = Arc<SessionHandle>;

/// Build the axum router with all API routes.
///
/// Accepts a [`SessionHandle`] that is shared across all route handlers
/// via axum's state extraction.
pub fn build_router(session: SessionHandle) -> Router {
    let state: AppState = Arc::new(session);

    Router::new()
        // -- Torrent routes --
        .route(
            "/api/v1/torrents",
            get(torrents::list_torrents).post(torrents::add_torrent),
        )
        .route(
            "/api/v1/torrents/{hash}",
            get(torrents::get_torrent).delete(torrents::delete_torrent),
        )
        .route(
            "/api/v1/torrents/{hash}/pause",
            post(torrents::pause_torrent),
        )
        .route(
            "/api/v1/torrents/{hash}/resume",
            post(torrents::resume_torrent),
        )
        // -- Session routes --
        .route("/api/v1/session/stats", get(session::get_stats))
        .route("/api/v1/session/counters", get(session::get_counters))
        .route(
            "/api/v1/session/settings",
            get(session::get_settings).patch(session::patch_settings),
        )
        .route("/api/v1/session/shutdown", post(session::shutdown))
        // -- Extended torrent routes --
        .route(
            "/api/v1/torrents/{hash}/info",
            get(extended::get_torrent_info),
        )
        .route("/api/v1/torrents/{hash}/peers", get(extended::get_peers))
        .route(
            "/api/v1/torrents/{hash}/trackers",
            get(extended::get_trackers),
        )
        .route(
            "/api/v1/torrents/{hash}/reannounce",
            post(extended::reannounce),
        )
        .route(
            "/api/v1/torrents/{hash}/files/{idx}/priority",
            patch(extended::set_file_priority),
        )
        // -- Peer ban management routes --
        .route("/api/v1/peers/banned", get(extended::get_banned_peers))
        .route("/api/v1/peers/ban", post(extended::ban_peer))
        .route("/api/v1/peers/ban/{ip}", delete(extended::unban_peer))
        // -- WebSocket event stream --
        .route("/api/v1/events", any(events::ws_events))
        .with_state(state)
}
