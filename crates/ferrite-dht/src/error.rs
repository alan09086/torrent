pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("KRPC error ({code}): {message}")]
    Krpc { code: i64, message: String },

    #[error("invalid KRPC message: {0}")]
    InvalidMessage(String),

    #[error("transaction ID mismatch: expected {expected}, got {got}")]
    TransactionMismatch { expected: u16, got: u16 },

    #[error("query timed out")]
    Timeout,

    #[error("invalid compact node info: {0}")]
    InvalidCompactNode(String),

    #[error("routing table full")]
    RoutingTableFull,

    #[error("DHT shutting down")]
    Shutdown,

    #[error("bencode: {0}")]
    Bencode(#[from] ferrite_bencode::Error),

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
