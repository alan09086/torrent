//! HTTP route definitions for the torrent REST API.

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
        .with_state(state)
}
