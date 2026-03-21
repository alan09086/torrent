use torrent_core::{Id20, Id32};

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
        Ok(torrent_core::sha1(&data) == *expected)
    }

    /// Verify a piece by comparing its SHA-256 hash against `expected` (v2).
    ///
    /// Default implementation reads the full piece and hashes it.
    fn verify_piece_v2(&self, piece: u32, expected: &Id32) -> Result<bool> {
        let data = self.read_piece(piece)?;
        Ok(torrent_core::sha256(&data) == *expected)
    }

    /// Hash a single 16 KiB block with SHA-256 for Merkle verification.
    ///
    /// Default implementation reads the chunk and hashes it.
    fn hash_block(&self, piece: u32, begin: u32, length: u32) -> Result<Id32> {
        let data = self.read_chunk(piece, begin, length)?;
        Ok(torrent_core::sha256(&data))
    }

    /// Write a block from two slices without concatenation (vectored write).
    ///
    /// The two slices `s0` and `s1` represent contiguous data that may be split
    /// across a ring-buffer wrap boundary. The total write length is
    /// `s0.len() + s1.len()`.
    ///
    /// Default implementation concatenates and delegates to [`write_chunk`].
    /// Backends with file-level I/O should override this to avoid the copy.
    ///
    /// [`write_chunk`]: TorrentStorage::write_chunk
    fn write_chunk_vectored(&self, piece: u32, begin: u32, s0: &[u8], s1: &[u8]) -> Result<()> {
        if s1.is_empty() {
            self.write_chunk(piece, begin, s0)
        } else {
            let mut combined = Vec::with_capacity(s0.len() + s1.len());
            combined.extend_from_slice(s0);
            combined.extend_from_slice(s1);
            self.write_chunk(piece, begin, &combined)
        }
    }

    /// Return filesystem metadata for io_uring fd management.
    ///
    /// Filesystem-backed implementations return the base directory, file paths,
    /// and file map. Non-filesystem backends (memory, mmap) return `None`.
    fn filesystem_info(&self) -> Option<(&std::path::Path, &[std::path::PathBuf], &crate::file_map::FileMap)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStorage;
    use torrent_core::{Lengths, sha256};

    #[test]
    fn verify_piece_v2_correct_hash() {
        let data = vec![0xABu8; 32768];
        let lengths = Lengths::new(32768, 32768, 16384);
        let storage = MemoryStorage::new(lengths);
        storage.write_chunk(0, 0, &data[..16384]).unwrap();
        storage.write_chunk(0, 16384, &data[16384..]).unwrap();

        let expected = sha256(&data);
        assert!(storage.verify_piece_v2(0, &expected).unwrap());
    }

    #[test]
    fn verify_piece_v2_wrong_hash() {
        let data = vec![0xABu8; 16384];
        let lengths = Lengths::new(16384, 16384, 16384);
        let storage = MemoryStorage::new(lengths);
        storage.write_chunk(0, 0, &data).unwrap();

        let wrong = Id32::ZERO;
        assert!(!storage.verify_piece_v2(0, &wrong).unwrap());
    }

    #[test]
    fn hash_block_returns_correct_sha256() {
        let data = vec![0xCDu8; 16384];
        let lengths = Lengths::new(16384, 16384, 16384);
        let storage = MemoryStorage::new(lengths);
        storage.write_chunk(0, 0, &data).unwrap();

        let hash = storage.hash_block(0, 0, 16384).unwrap();
        assert_eq!(hash, sha256(&data));
    }

    #[test]
    fn hash_block_partial_last_block() {
        let data = vec![0xEFu8; 20000];
        let lengths = Lengths::new(20000, 20000, 16384);
        let storage = MemoryStorage::new(lengths);
        storage.write_chunk(0, 0, &data[..16384]).unwrap();
        storage.write_chunk(0, 16384, &data[16384..]).unwrap();

        let hash = storage.hash_block(0, 16384, 3616).unwrap();
        assert_eq!(hash, sha256(&data[16384..]));
    }
}
