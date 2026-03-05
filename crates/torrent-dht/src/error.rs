/// Crate-level result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors from DHT operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Remote node returned a KRPC error response.
    #[error("KRPC error ({code}): {message}")]
    Krpc {
        /// KRPC error code (e.g. 201 = generic, 203 = method unknown).
        code: i64,
        /// Human-readable error description.
        message: String,
    },

    /// Received a malformed KRPC message.
    #[error("invalid KRPC message: {0}")]
    InvalidMessage(String),

    /// Response transaction ID does not match the request.
    #[error("transaction ID mismatch: expected {expected}, got {got}")]
    TransactionMismatch {
        /// Expected transaction ID.
        expected: u16,
        /// Received transaction ID.
        got: u16,
    },

    /// Query timed out with no response.
    #[error("query timed out")]
    Timeout,

    /// Malformed compact node info bytes.
    #[error("invalid compact node info: {0}")]
    InvalidCompactNode(String),

    /// All k-buckets are full and no stale nodes to replace.
    #[error("routing table full")]
    RoutingTableFull,

    /// DHT actor has been shut down.
    #[error("DHT shutting down")]
    Shutdown,

    /// Bencode parsing error.
    #[error("bencode: {0}")]
    Bencode(#[from] torrent_bencode::Error),

    /// I/O error.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    /// BEP 44 value exceeds the 1000-byte limit.
    #[error("BEP 44: value too large ({size} bytes, max {max})")]
    Bep44ValueTooLarge {
        /// Actual value size in bytes.
        size: usize,
        /// Maximum allowed size.
        max: usize,
    },

    /// BEP 44 salt exceeds the 64-byte limit.
    #[error("BEP 44: salt too large ({size} bytes, max {max})")]
    Bep44SaltTooLarge {
        /// Actual salt size in bytes.
        size: usize,
        /// Maximum allowed size.
        max: usize,
    },

    /// BEP 44 ed25519 signature verification failed.
    #[error("BEP 44: invalid signature")]
    Bep44InvalidSignature,

    /// BEP 44 put rejected because sequence number is not newer.
    #[error("BEP 44: sequence number {got} not newer than {current}")]
    Bep44SequenceTooOld {
        /// Sequence number in the put request.
        got: i64,
        /// Current stored sequence number.
        current: i64,
    },

    /// BEP 44 CAS (compare-and-swap) failed because stored seq differs.
    #[error("BEP 44: CAS mismatch (expected seq {expected}, got {actual})")]
    Bep44CasMismatch {
        /// Expected sequence number from the CAS field.
        expected: i64,
        /// Actual stored sequence number.
        actual: i64,
    },
}
