/// Convenience alias for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur during session operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Wire protocol error from the peer connection layer.
    #[error("wire: {0}")]
    Wire(#[from] torrent_wire::Error),

    /// Storage/disk I/O error.
    #[error("storage: {0}")]
    Storage(#[from] torrent_storage::Error),

    /// Tracker communication error.
    #[error("tracker: {0}")]
    Tracker(#[from] torrent_tracker::Error),

    /// DHT operation error.
    #[error("DHT: {0}")]
    Dht(#[from] torrent_dht::Error),

    /// Core library error (metainfo parsing, hashing, etc.).
    #[error("core: {0}")]
    Core(#[from] torrent_core::Error),

    /// Peer connection failed.
    #[error("connection: {0}")]
    Connection(
        /// Error description.
        String,
    ),

    /// Received metadata whose info hash does not match the expected value.
    #[error("metadata info_hash mismatch")]
    MetadataHashMismatch,

    /// A piece failed hash verification.
    #[error("piece {index} hash verification failed")]
    PieceHashFailed {
        /// Zero-based piece index that failed verification.
        index: u32,
    },

    /// Received invalid peer data from a tracker or PEX message.
    #[error("invalid peer data: {0}")]
    InvalidPeerData(
        /// Description of the invalid data.
        String,
    ),

    /// The requested torrent is not in the session.
    #[error("torrent not found: {0}")]
    TorrentNotFound(
        /// Info hash of the missing torrent.
        torrent_core::Id20,
    ),

    /// Attempted to add a torrent that already exists in the session.
    #[error("duplicate torrent: {0}")]
    DuplicateTorrent(
        /// Info hash of the duplicate torrent.
        torrent_core::Id20,
    ),

    /// The session has reached its maximum torrent capacity.
    #[error("session at capacity ({0} torrents)")]
    SessionAtCapacity(
        /// Current number of torrents in the session.
        usize,
    ),

    /// Torrent metadata has not been received yet (magnet link still resolving).
    #[error("metadata not yet available for {0}")]
    MetadataNotReady(
        /// Info hash of the torrent awaiting metadata.
        torrent_core::Id20,
    ),

    /// File index is out of range for the torrent.
    #[error("file index {index} out of range (torrent has {count} files)")]
    InvalidFileIndex {
        /// The requested file index.
        index: usize,
        /// Total number of files in the torrent.
        count: usize,
    },

    /// Attempted to stream a file that has `Skip` priority.
    #[error("file has Skip priority (index {index})")]
    FileSkipped {
        /// The file index with `Skip` priority.
        index: usize,
    },

    /// Piece index is out of range for the torrent.
    #[error("piece index {index} out of range (torrent has {num_pieces} pieces)")]
    InvalidPieceIndex {
        /// The requested piece index.
        index: u32,
        /// Total number of pieces in the torrent.
        num_pieces: u32,
    },

    /// Configuration error (e.g. invalid proxy settings).
    #[error("configuration error: {0}")]
    Config(
        /// Error description.
        String,
    ),

    /// Settings validation failed.
    #[error("invalid settings: {0}")]
    InvalidSettings(
        /// Description of the invalid setting.
        String,
    ),

    /// The session is shutting down and cannot accept new commands.
    #[error("session shutting down")]
    Shutdown,

    /// DHT is disabled in session settings.
    #[error("DHT is disabled")]
    DhtDisabled,

    /// I2P SAM protocol error.
    #[error("I2P: {0}")]
    I2p(#[from] crate::i2p::SamError),

    /// Operating system I/O error.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
