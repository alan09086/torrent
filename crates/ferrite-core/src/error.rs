/// Result type alias for ferrite-core operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors from core BitTorrent operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Bencode parsing error.
    #[error("bencode: {0}")]
    Bencode(#[from] ferrite_bencode::Error),

    /// Invalid magnet link.
    #[error("invalid magnet link: {0}")]
    InvalidMagnet(String),

    /// Invalid hex string.
    #[error("invalid hex: {0}")]
    InvalidHex(String),

    /// Invalid hash length.
    #[error("invalid hash length: expected {expected}, got {got}")]
    InvalidHashLength { expected: usize, got: usize },

    /// Torrent metainfo is malformed.
    #[error("invalid torrent: {0}")]
    InvalidTorrent(String),
}
