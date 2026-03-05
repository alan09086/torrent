/// Convenience result type for tracker operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur during tracker communication.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Tracker returned an error message.
    #[error("tracker returned error: {0}")]
    TrackerError(String),

    /// Invalid or malformed tracker response.
    #[error("invalid tracker response: {0}")]
    InvalidResponse(String),

    /// UDP tracker protocol error.
    #[error("UDP protocol error: {0}")]
    UdpProtocol(String),

    /// Request timed out.
    #[error("connection timed out")]
    Timeout,

    /// Bencode deserialization error.
    #[error("bencode: {0}")]
    Bencode(#[from] torrent_bencode::Error),

    /// HTTP request error.
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),

    /// Underlying I/O error.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    /// Invalid tracker URL.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    /// URL blocked by security policy (e.g., SSRF mitigation).
    #[error("URL security policy violation: {0}")]
    SecurityViolation(String),
}
