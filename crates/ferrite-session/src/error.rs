pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("wire: {0}")]
    Wire(#[from] ferrite_wire::Error),

    #[error("storage: {0}")]
    Storage(#[from] ferrite_storage::Error),

    #[error("tracker: {0}")]
    Tracker(#[from] ferrite_tracker::Error),

    #[error("DHT: {0}")]
    Dht(#[from] ferrite_dht::Error),

    #[error("core: {0}")]
    Core(#[from] ferrite_core::Error),

    #[error("connection: {0}")]
    Connection(String),

    #[error("metadata info_hash mismatch")]
    MetadataHashMismatch,

    #[error("piece {index} hash verification failed")]
    PieceHashFailed { index: u32 },

    #[error("invalid peer data: {0}")]
    InvalidPeerData(String),

    #[error("torrent not found: {0}")]
    TorrentNotFound(ferrite_core::Id20),

    #[error("duplicate torrent: {0}")]
    DuplicateTorrent(ferrite_core::Id20),

    #[error("session at capacity ({0} torrents)")]
    SessionAtCapacity(usize),

    #[error("session shutting down")]
    Shutdown,

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
