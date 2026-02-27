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
    // Alert system (M15)
    Alert,
    AlertKind,
    AlertCategory,
    AlertStream,
    // Persistence (M11)
    SessionState,
    DhtNodeEntry,
    validate_resume_bitfield,
    // Piece selection (M12)
    build_wanted_pieces,
    // Error types
    Error,
    Result,
};
