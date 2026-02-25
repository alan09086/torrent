pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("tracker returned error: {0}")]
    TrackerError(String),

    #[error("invalid tracker response: {0}")]
    InvalidResponse(String),

    #[error("UDP protocol error: {0}")]
    UdpProtocol(String),

    #[error("connection timed out")]
    Timeout,

    #[error("bencode: {0}")]
    Bencode(#[from] ferrite_bencode::Error),

    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid URL: {0}")]
    InvalidUrl(String),
}
