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
    PeerStrikeEntry,
    validate_resume_bitfield,
    // Smart banning (M25)
    BanConfig,
    // Piece selection (M12)
    build_wanted_pieces,
    // Tracker management (M24)
    TrackerInfo,
    TrackerStatus,
    // File streaming (M28)
    FileStream,
    // IP filtering (M29)
    IpFilter,
    IpFilterError,
    PortFilter,
    parse_dat,
    parse_p2p,
    // Proxy support (M29)
    ProxyType,
    ProxyConfig,
    // Disk I/O (M26)
    DiskConfig,
    DiskHandle,
    DiskManagerHandle,
    DiskJobFlags,
    DiskStats,
    // Error types
    Error,
    Result,
};
