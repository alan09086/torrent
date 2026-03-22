//! HTTP route definitions for the torrent REST API.
//!
//! Endpoints will be added in subsequent tasks.

use axum::Router;
use torrent::session::SessionHandle;

/// Build the axum router with all API routes.
///
/// Accepts a [`SessionHandle`] that is shared across all route handlers
/// via axum's state extraction.
pub fn build_router(_session: SessionHandle) -> Router {
    Router::new()
    // Endpoints will be added in Tasks 3-5.
}
