//! Convenience re-exports for `use ferrite::prelude::*`.
//!
//! Imports the most commonly needed types for building BitTorrent
//! applications with ferrite.

// Builder types
pub use crate::client::{ClientBuilder, AddTorrentParams};

// Session management
pub use crate::session::{
    SessionHandle, TorrentHandle,
    TorrentState, TorrentStats, TorrentInfo,
};

// Core types
pub use crate::core::{Id20, Magnet, TorrentMetaV1};

// Storage
pub use crate::storage::{TorrentStorage, FilesystemStorage};

// Unified error
pub use crate::error::{Error, Result};

// Resume data
pub use crate::core::FastResumeData;
pub use crate::session::SessionState;
