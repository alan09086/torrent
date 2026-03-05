use std::fmt;

/// Result type alias for bencode operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur during bencode serialization or deserialization.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Integer overflow or invalid integer value.
    #[error("invalid integer at position {position}: {detail}")]
    InvalidInteger {
        /// Byte offset where the error occurred.
        position: usize,
        /// Description of the problem.
        detail: String,
    },

    /// Invalid byte string (bad length prefix, truncated, etc).
    #[error("invalid byte string at position {position}: {detail}")]
    InvalidByteString {
        /// Byte offset where the error occurred.
        position: usize,
        /// Description of the problem.
        detail: String,
    },

    /// Unexpected byte encountered during parsing.
    #[error("unexpected byte {byte:#04x} at position {position}, expected {expected}")]
    UnexpectedByte {
        /// The byte that was found.
        byte: u8,
        /// Byte offset where the error occurred.
        position: usize,
        /// What was expected instead.
        expected: &'static str,
    },

    /// Input ended prematurely.
    #[error("unexpected end of input at position {position}: {context}")]
    UnexpectedEof {
        /// Byte offset where the error occurred.
        position: usize,
        /// Description of what was being parsed.
        context: String,
    },

    /// Dictionary keys are not in sorted order (BEP 3 violation).
    #[error("dictionary keys not sorted at position {position}")]
    UnsortedKeys {
        /// Byte offset of the out-of-order key.
        position: usize,
    },

    /// Trailing bytes after the top-level value.
    #[error("{count} trailing byte(s) after value at position {position}")]
    TrailingData {
        /// Byte offset where trailing data begins.
        position: usize,
        /// Number of trailing bytes.
        count: usize,
    },

    /// The key in `find_dict_key_span` was not found.
    #[error("key {key:?} not found in dictionary")]
    KeyNotFound {
        /// The key that was not found.
        key: String,
    },

    /// Input is not a dictionary (for `find_dict_key_span`).
    #[error("expected dictionary at position {position}")]
    NotADictionary {
        /// Byte offset where the non-dictionary value was found.
        position: usize,
    },

    /// Custom error from serde.
    #[error("{0}")]
    Custom(String),
}

impl serde::ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::Custom(msg.to_string())
    }
}

impl serde::de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::Custom(msg.to_string())
    }
}
