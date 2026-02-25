use ferrite_core::Id20;

use crate::Result;

/// Storage backend for torrent piece data.
///
/// `&self` (not `&mut self`) — implementations use interior mutability
/// so they can be shared via `Arc` across threads.
///
/// Parameters use wire-protocol coordinates directly:
/// `(piece, begin, length)` matching BEP 3 request/piece messages.
pub trait TorrentStorage: Send + Sync {
    /// Write a chunk of data at `(piece, begin)`.
    fn write_chunk(&self, piece: u32, begin: u32, data: &[u8]) -> Result<()>;

    /// Read a chunk of data from `(piece, begin, length)`.
    fn read_chunk(&self, piece: u32, begin: u32, length: u32) -> Result<Vec<u8>>;

    /// Read an entire piece.
    fn read_piece(&self, piece: u32) -> Result<Vec<u8>>;

    /// Verify a piece by comparing its SHA1 hash against `expected`.
    ///
    /// Default implementation reads the full piece and hashes it.
    fn verify_piece(&self, piece: u32, expected: &Id20) -> Result<bool> {
        let data = self.read_piece(piece)?;
        Ok(ferrite_core::sha1(&data) == *expected)
    }
}
