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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStorage;
    use torrent_core::{sha256, Lengths};

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
