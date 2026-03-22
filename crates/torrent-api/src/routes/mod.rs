//! HTTP route definitions for the torrent REST API.

pub mod session;
pub mod torrents;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
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
        .with_state(state)
}
