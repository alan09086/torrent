//! BitTorrent session management: peers, torrents, orchestration.
//!
//! Re-exports from [`ferrite_session`].

pub use ferrite_session::{
    // Session manager
    SessionHandle,
    SessionConfig,
    SessionStats,
    // Torrent handle and state
    TorrentHandle,
    TorrentConfig,
    TorrentState,
    TorrentStats,
    TorrentInfo,
    // File info
    FileInfo,
    // Storage factory type alias
    StorageFactory,
    // Persistence (M11)
    SessionState,
    DhtNodeEntry,
    validate_resume_bitfield,
    // Error types
    Error,
    Result,
};
