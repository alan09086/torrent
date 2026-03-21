//! BitTorrent session management: peers, torrents, orchestration.
//!
//! Re-exports from [`torrent_session`].

pub use torrent_session::{
    // Alert system (M15)
    Alert,
    AlertCategory,
    AlertKind,
    AlertStream,
    // Smart banning (M25)
    BanConfig,
    ChokingAlgorithm,
    DhtNodeEntry,
    DisabledDiskIo,
    // Disk I/O (M26)
    DiskConfig,
    DiskHandle,
    // Pluggable disk I/O (M49)
    DiskIoBackend,
    DiskIoStats,
    DiskJobFlags,
    DiskManagerHandle,
    DiskStats,
    // Error types
    Error,
    // Extension plugin interface (M32d)
    ExtensionPlugin,
    // File info
    FileInfo,
    FileMode,
    FileStatus,
    // File streaming (M28)
    FileStream,
    // I2P support (M41)
    I2pDestination,
    I2pDestinationError,
    // IP filtering (M29)
    IpFilter,
    IpFilterError,
    // Session stats (M50)
    MetricKind,
    // Mixed-mode bandwidth allocation (M45)
    MixedModeAlgorithm,
    NUM_METRICS,
    PartialPieceInfo,
    // Peer introspection (M53)
    PeerInfo,
    // Peer source tracking (M32a)
    PeerSource,
    PeerStrikeEntry,
    PortFilter,
    ProxyConfig,
    // Proxy support (M29)
    ProxyType,
    Result,
    // Choking algorithms (M43)
    SeedChokingAlgorithm,
    SessionCounters,
    // Session manager
    SessionHandle,
    // Persistence (M11)
    SessionState,
    SessionStats,
    SessionStatsMetric,
    Settings,
    // Storage factory type alias
    StorageFactory,
    TorrentConfig,
    // Torrent flags (M53)
    TorrentFlags,
    // Torrent handle and state
    TorrentHandle,
    TorrentInfo,
    TorrentState,
    TorrentStats,
    TorrentSummary,
    // Tracker management (M24)
    TrackerInfo,
    TrackerStatus,
    // Piece selection (M12)
    build_wanted_pieces,
    parse_dat,
    parse_p2p,
    session_stats_metrics,
    validate_resume_bitfield,
};
