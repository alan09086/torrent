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

// BEP 53 file selection (M36)
pub use crate::core::FileSelection;

// Encryption mode (M17)
pub use crate::wire::mse::EncryptionMode;

// Settings pack (M31)
pub use crate::session::Settings;

// Smart banning (M25)
pub use crate::session::BanConfig;

// File streaming (M28)
pub use crate::session::FileStream;

// IP filtering + proxy (M29)
pub use crate::session::{IpFilter, ProxyConfig, ProxyType};

// Peer source tracking (M32a)
pub use crate::session::PeerSource;

// Extension plugin interface (M32d)
pub use crate::session::ExtensionPlugin;

// Torrent creation (M30)
pub use crate::core::CreateTorrent;

// BitTorrent v2 (M33, BEP 52)
pub use crate::core::{InfoHashes, TorrentMetaV2, TorrentMeta, Id32};

// Hybrid v1+v2 torrents (M35, BEP 52)
pub use crate::core::TorrentVersion;

// BEP 52 Hash Picker (M34a)
pub use crate::core::{HashPicker, SetBlockResult};
