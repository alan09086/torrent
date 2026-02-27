//! Unified error type wrapping all per-crate errors.
//!
//! Each facade module also re-exports its crate's `Error` type for
//! fine-grained matching (e.g., `ferrite::wire::Error`). This unified
//! type allows catching any ferrite error with a single type.

/// Unified error type for the ferrite crate family.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Bencode serialization/deserialization error.
    #[error("bencode: {0}")]
    Bencode(#[from] ferrite_bencode::Error),

    /// Core type error (hash parsing, torrent validation, magnet links).
    #[error("core: {0}")]
    Core(#[from] ferrite_core::Error),

    /// Wire protocol error (handshake, message encoding).
    #[error("wire: {0}")]
    Wire(#[from] ferrite_wire::Error),

    /// Tracker communication error.
    #[error("tracker: {0}")]
    Tracker(#[from] ferrite_tracker::Error),

    /// DHT operation error.
    #[error("dht: {0}")]
    Dht(#[from] ferrite_dht::Error),

    /// Storage I/O error (pieces, chunks, files).
    #[error("storage: {0}")]
    Storage(#[from] ferrite_storage::Error),

    /// Session management error (peers, torrents).
    #[error("session: {0}")]
    Session(#[from] ferrite_session::Error),

    /// uTP transport error.
    #[error("utp: {0}")]
    Utp(#[from] ferrite_utp::Error),

    /// NAT port mapping error.
    #[error("nat: {0}")]
    Nat(#[from] ferrite_nat::Error),

    /// Raw I/O error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Unified result type for the ferrite crate family.
pub type Result<T> = std::result::Result<T, Error>;
