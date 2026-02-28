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
    Alert, AlertKind, AlertCategory, AlertStream,
};

// Core types
pub use crate::core::{Id20, Magnet, TorrentMetaV1};

// Storage
pub use crate::storage::{TorrentStorage, FilesystemStorage, MmapStorage};
pub use crate::core::StorageMode;

// Unified error
pub use crate::error::{Error, Result};

// Resume data
pub use crate::core::FastResumeData;
pub use crate::session::SessionState;

// File priority (M12)
pub use crate::core::FilePriority;

// Encryption mode (M17)
pub use crate::wire::mse::EncryptionMode;

// Smart banning (M25)
pub use crate::session::BanConfig;

// File streaming (M28)
pub use crate::session::FileStream;

// IP filtering + proxy (M29)
pub use crate::session::{IpFilter, ProxyConfig, ProxyType};
