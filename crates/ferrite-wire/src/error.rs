pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid handshake: {0}")]
    InvalidHandshake(String),

    #[error("invalid message ID {0}")]
    InvalidMessageId(u8),

    #[error("message too short: expected at least {expected} bytes, got {got}")]
    MessageTooShort { expected: usize, got: usize },

    #[error("message too large: {size} bytes (max {max})")]
    MessageTooLarge { size: usize, max: usize },

    #[error("invalid extended message: {0}")]
    InvalidExtended(String),

    #[error("encryption handshake failed: {0}")]
    EncryptionHandshakeFailed(String),

    #[error("unsupported crypto method")]
    UnsupportedCryptoMethod,

    #[error("encryption required but peer does not support it")]
    EncryptionRequired,

    #[error("bencode: {0}")]
    Bencode(#[from] ferrite_bencode::Error),

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
