use std::sync::RwLock;

use torrent_core::Lengths;

use crate::Result;
use crate::error::Error;
use crate::storage::TorrentStorage;

/// In-memory storage backend.
///
/// Uses a flat byte buffer with `RwLock` for concurrent read access.
/// Useful for tests and magnet-link metadata downloads.
pub struct MemoryStorage {
    data: RwLock<Vec<u8>>,
    lengths: Lengths,
}

impl MemoryStorage {
    /// Create a new in-memory storage of the given total size.
    pub fn new(lengths: Lengths) -> Self {
        let size = lengths.total_length() as usize;
        MemoryStorage {
            data: RwLock::new(vec![0; size]),
            lengths,
        }
    }
}

impl TorrentStorage for MemoryStorage {
    fn write_chunk(&self, piece: u32, begin: u32, data: &[u8]) -> Result<()> {
        let offset = self.lengths.piece_offset(piece) + begin as u64;
        let mut buf = self.data.write().unwrap();

        let start = offset as usize;
        let end = start + data.len();
        if end > buf.len() {
            return Err(Error::ChunkOutOfRange {
                piece,
                begin,
                length: data.len() as u32,
            });
        }

        buf[start..end].copy_from_slice(data);
        Ok(())
    }

    fn read_chunk(&self, piece: u32, begin: u32, length: u32) -> Result<Vec<u8>> {
        let offset = self.lengths.piece_offset(piece) + begin as u64;
        let buf = self.data.read().unwrap();

        let start = offset as usize;
        let end = start + length as usize;
        if end > buf.len() {
            return Err(Error::ChunkOutOfRange {
                piece,
                begin,
                length,
            });
        }

        Ok(buf[start..end].to_vec())
    }

    fn read_piece(&self, piece: u32) -> Result<Vec<u8>> {
        let piece_size = self.lengths.piece_size(piece);
        if piece_size == 0 {
            return Err(Error::PieceOutOfRange {
                index: piece,
                num_pieces: self.lengths.num_pieces(),
            });
        }
        self.read_chunk(piece, 0, piece_size)
    }
}

#[cfg(test)]
mod tests {
    use torrent_core::{Id20, Lengths};

    use super::*;

    fn make_storage() -> MemoryStorage {
        // 100 bytes total, 50 byte pieces, 25 byte chunks
        MemoryStorage::new(Lengths::new(100, 50, 25))
    }

    #[test]
    fn write_read_chunk() {
        let s = make_storage();
        let data = vec![42u8; 25];
        s.write_chunk(0, 0, &data).unwrap();
        let read = s.read_chunk(0, 0, 25).unwrap();
        assert_eq!(read, data);
    }

    #[test]
    fn read_piece() {
        let s = make_storage();
        let chunk0 = vec![1u8; 25];
        let chunk1 = vec![2u8; 25];
        s.write_chunk(0, 0, &chunk0).unwrap();
        s.write_chunk(0, 25, &chunk1).unwrap();

        let piece = s.read_piece(0).unwrap();
        assert_eq!(piece.len(), 50);
        assert_eq!(&piece[..25], &chunk0[..]);
        assert_eq!(&piece[25..], &chunk1[..]);
    }

    #[test]
    fn verify_correct() {
        let s = make_storage();
        let data = vec![0u8; 50];
        s.write_chunk(0, 0, &data).unwrap();
        let expected = torrent_core::sha1(&data);
        assert!(s.verify_piece(0, &expected).unwrap());
    }

    #[test]
    fn verify_wrong() {
        let s = make_storage();
        let data = vec![0u8; 50];
        s.write_chunk(0, 0, &data).unwrap();
        let wrong = Id20::ZERO;
        assert!(!s.verify_piece(0, &wrong).unwrap());
    }

    #[test]
    fn zero_length_piece_out_of_range() {
        let s = make_storage();
        // Piece 5 doesn't exist (only 0 and 1)
        assert!(s.read_piece(5).is_err());
    }
}
