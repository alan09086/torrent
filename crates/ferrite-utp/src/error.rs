pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid packet: {0}")]
    InvalidPacket(String),

    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(u8),

    #[error("connection refused")]
    ConnectionRefused,

    #[error("connection reset by peer")]
    ConnectionReset,

    #[error("connection timed out")]
    Timeout,

    #[error("connection closed")]
    Closed,

    #[error("too many connections")]
    TooManyConnections,

    #[error("socket shutting down")]
    Shutdown,

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
