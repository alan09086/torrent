/// Convenience result type for uTP operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur during uTP communication.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid or malformed packet.
    #[error("invalid packet: {0}")]
    InvalidPacket(String),

    /// Unsupported uTP protocol version.
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(u8),

    /// Connection refused by remote peer.
    #[error("connection refused")]
    ConnectionRefused,

    /// Connection reset by remote peer.
    #[error("connection reset by peer")]
    ConnectionReset,

    /// Connection timed out.
    #[error("connection timed out")]
    Timeout,

    /// Connection is closed.
    #[error("connection closed")]
    Closed,

    /// Too many concurrent connections.
    #[error("too many connections")]
    TooManyConnections,

    /// Socket is shutting down.
    #[error("socket shutting down")]
    Shutdown,

    /// Underlying I/O error.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
