/// Crate-level result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors from torrent storage operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Piece index exceeds the torrent's piece count.
    #[error("piece index {index} out of range (num_pieces: {num_pieces})")]
    PieceOutOfRange {
        /// Requested piece index.
        index: u32,
        /// Total number of pieces in the torrent.
        num_pieces: u32,
    },

    /// Chunk position or length falls outside the piece.
    #[error("chunk out of range: piece {piece}, begin {begin}, length {length}")]
    ChunkOutOfRange {
        /// Piece index.
        piece: u32,
        /// Byte offset within the piece.
        begin: u32,
        /// Chunk length in bytes.
        length: u32,
    },

    /// Bitfield byte count does not match the expected piece count.
    #[error("invalid bitfield length: expected {expected} bytes, got {got}")]
    InvalidBitfieldLength {
        /// Expected byte count.
        expected: usize,
        /// Actual byte count.
        got: usize,
    },

    /// Spare bits after the last piece are set (malformed bitfield).
    #[error("trailing bits set in bitfield")]
    TrailingBitsSet,

    /// I/O error from the underlying storage.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
