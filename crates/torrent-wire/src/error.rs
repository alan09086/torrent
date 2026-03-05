/// Crate-level result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors from BitTorrent wire protocol operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid handshake data.
    #[error("invalid handshake: {0}")]
    InvalidHandshake(String),

    /// Unrecognized message ID byte.
    #[error("invalid message ID {0}")]
    InvalidMessageId(u8),

    /// Message payload shorter than required.
    #[error("message too short: expected at least {expected} bytes, got {got}")]
    MessageTooShort {
        /// Minimum required byte count.
        expected: usize,
        /// Actual byte count received.
        got: usize,
    },

    /// Message exceeds the maximum allowed size.
    #[error("message too large: {size} bytes (max {max})")]
    MessageTooLarge {
        /// Actual message size in bytes.
        size: usize,
        /// Configured maximum size in bytes.
        max: usize,
    },

    /// Malformed BEP 10 extension message.
    #[error("invalid extended message: {0}")]
    InvalidExtended(String),

    /// MSE/PE encryption handshake failure.
    #[error("encryption handshake failed: {0}")]
    EncryptionHandshakeFailed(String),

    /// Peer proposed an unsupported crypto method.
    #[error("unsupported crypto method")]
    UnsupportedCryptoMethod,

    /// Encryption required but peer does not support it.
    #[error("encryption required but peer does not support it")]
    EncryptionRequired,

    /// Bencode parsing error.
    #[error("bencode: {0}")]
    Bencode(#[from] torrent_bencode::Error),

    /// TLS/SSL error.
    #[error("SSL/TLS: {0}")]
    Ssl(String),

    /// I/O error.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
