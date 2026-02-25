pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("piece index {index} out of range (num_pieces: {num_pieces})")]
    PieceOutOfRange { index: u32, num_pieces: u32 },

    #[error("chunk out of range: piece {piece}, begin {begin}, length {length}")]
    ChunkOutOfRange {
        piece: u32,
        begin: u32,
        length: u32,
    },

    #[error("invalid bitfield length: expected {expected} bytes, got {got}")]
    InvalidBitfieldLength { expected: usize, got: usize },

    #[error("trailing bits set in bitfield")]
    TrailingBitsSet,

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
