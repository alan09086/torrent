/// Piece and chunk arithmetic for BitTorrent downloads.
///
/// Manages the mapping between:
/// - **Pieces**: fixed-size blocks verified by SHA1 (except possibly the last piece)
/// - **Chunks**: sub-piece blocks requested from peers (typically 16 KiB)
/// - **Files**: the actual files on disk that pieces map across
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lengths {
    /// Total size of all files in bytes.
    total_length: u64,
    /// Size of each piece in bytes (last piece may be smaller).
    piece_length: u64,
    /// Size of each chunk/block in bytes (typically 16384).
    chunk_size: u32,
}

/// Default chunk size (16 KiB) — standard in BitTorrent.
pub const DEFAULT_CHUNK_SIZE: u32 = 16384;

impl Lengths {
    /// Create a new Lengths calculator.
    ///
    /// # Panics
    /// Panics if `piece_length` or `chunk_size` is 0.
    pub fn new(total_length: u64, piece_length: u64, chunk_size: u32) -> Self {
        assert!(piece_length > 0, "piece_length must be > 0");
        assert!(chunk_size > 0, "chunk_size must be > 0");
        Lengths {
            total_length,
            piece_length,
            chunk_size,
        }
    }

    /// Total size of all content.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Standard piece length.
    pub fn piece_length(&self) -> u64 {
        self.piece_length
    }

    /// Chunk/block size.
    pub fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// Total number of pieces.
    pub fn num_pieces(&self) -> u32 {
        if self.total_length == 0 {
            return 0;
        }
        self.total_length.div_ceil(self.piece_length) as u32
    }

    /// Actual length of a specific piece (last piece may be shorter).
    pub fn piece_size(&self, piece_index: u32) -> u32 {
        let num = self.num_pieces();
        if piece_index >= num {
            return 0;
        }
        if piece_index == num - 1 {
            // Last piece
            let remainder = self.total_length % self.piece_length;
            if remainder == 0 {
                self.piece_length as u32
            } else {
                remainder as u32
            }
        } else {
            self.piece_length as u32
        }
    }

    /// Number of chunks in a specific piece.
    pub fn chunks_in_piece(&self, piece_index: u32) -> u32 {
        let piece_size = self.piece_size(piece_index) as u64;
        if piece_size == 0 {
            return 0;
        }
        piece_size.div_ceil(self.chunk_size as u64) as u32
    }

    /// Offset and length of a specific chunk within a piece.
    ///
    /// Returns `(offset_within_piece, chunk_length)`.
    pub fn chunk_info(&self, piece_index: u32, chunk_index: u32) -> Option<(u32, u32)> {
        let piece_size = self.piece_size(piece_index);
        if piece_size == 0 {
            return None;
        }

        let offset = chunk_index * self.chunk_size;
        if offset >= piece_size {
            return None;
        }

        let remaining = piece_size - offset;
        let len = remaining.min(self.chunk_size);
        Some((offset, len))
    }

    /// Absolute byte offset for the start of a piece.
    pub fn piece_offset(&self, piece_index: u32) -> u64 {
        piece_index as u64 * self.piece_length
    }

    /// Map an absolute byte offset to (piece_index, offset_within_piece).
    pub fn byte_to_piece(&self, byte_offset: u64) -> Option<(u32, u32)> {
        if byte_offset >= self.total_length {
            return None;
        }
        let piece_index = (byte_offset / self.piece_length) as u32;
        let offset_in_piece = (byte_offset % self.piece_length) as u32;
        Some((piece_index, offset_in_piece))
    }

    /// Given file boundaries, determine which pieces a file spans.
    /// Returns `(first_piece, last_piece)` inclusive.
    pub fn file_pieces(&self, file_offset: u64, file_length: u64) -> Option<(u32, u32)> {
        if file_length == 0 || file_offset >= self.total_length {
            return None;
        }
        let first = (file_offset / self.piece_length) as u32;
        let last_byte = file_offset + file_length - 1;
        let last = (last_byte.min(self.total_length - 1) / self.piece_length) as u32;
        Some((first, last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_lengths() -> Lengths {
        // 1 MiB total, 256 KiB pieces, 16 KiB chunks
        Lengths::new(1048576, 262144, 16384)
    }

    #[test]
    fn num_pieces_exact_division() {
        let l = make_lengths();
        assert_eq!(l.num_pieces(), 4); // 1 MiB / 256 KiB = 4
    }

    #[test]
    fn num_pieces_with_remainder() {
        let l = Lengths::new(1000000, 262144, 16384);
        assert_eq!(l.num_pieces(), 4); // ceil(1000000 / 262144) = 4
    }

    #[test]
    fn piece_size_regular() {
        let l = make_lengths();
        assert_eq!(l.piece_size(0), 262144);
        assert_eq!(l.piece_size(1), 262144);
        assert_eq!(l.piece_size(3), 262144); // last piece, exact division
    }

    #[test]
    fn piece_size_last_piece_shorter() {
        let l = Lengths::new(1000000, 262144, 16384);
        assert_eq!(l.piece_size(0), 262144);
        assert_eq!(l.piece_size(3), 1000000 - 3 * 262144); // 213568
    }

    #[test]
    fn piece_size_out_of_bounds() {
        let l = make_lengths();
        assert_eq!(l.piece_size(4), 0);
        assert_eq!(l.piece_size(100), 0);
    }

    #[test]
    fn chunks_in_piece() {
        let l = make_lengths();
        assert_eq!(l.chunks_in_piece(0), 16); // 262144 / 16384 = 16
    }

    #[test]
    fn chunks_in_last_piece() {
        let l = Lengths::new(1000000, 262144, 16384);
        let last_piece_size = 1000000 - 3 * 262144; // 213568
        let expected_chunks = (last_piece_size + 16383) / 16384; // 14
        assert_eq!(l.chunks_in_piece(3), expected_chunks as u32);
    }

    #[test]
    fn chunk_info_regular() {
        let l = make_lengths();
        assert_eq!(l.chunk_info(0, 0), Some((0, 16384)));
        assert_eq!(l.chunk_info(0, 1), Some((16384, 16384)));
        assert_eq!(l.chunk_info(0, 15), Some((15 * 16384, 16384)));
    }

    #[test]
    fn chunk_info_last_chunk_shorter() {
        // 100000 byte total, 50000 byte pieces, 16384 chunks
        let l = Lengths::new(100000, 50000, 16384);
        // Piece 0: 50000 bytes, chunks: 0..16384, 16384..32768, 32768..49152 (16384), 49152..50000 (848)
        assert_eq!(l.chunk_info(0, 3), Some((49152, 848)));
    }

    #[test]
    fn chunk_info_out_of_bounds() {
        let l = make_lengths();
        assert_eq!(l.chunk_info(0, 16), None); // only 16 chunks (0..15)
        assert_eq!(l.chunk_info(4, 0), None); // piece doesn't exist
    }

    #[test]
    fn piece_offset() {
        let l = make_lengths();
        assert_eq!(l.piece_offset(0), 0);
        assert_eq!(l.piece_offset(1), 262144);
        assert_eq!(l.piece_offset(3), 786432);
    }

    #[test]
    fn byte_to_piece() {
        let l = make_lengths();
        assert_eq!(l.byte_to_piece(0), Some((0, 0)));
        assert_eq!(l.byte_to_piece(262143), Some((0, 262143)));
        assert_eq!(l.byte_to_piece(262144), Some((1, 0)));
        assert_eq!(l.byte_to_piece(1048575), Some((3, 262143)));
        assert_eq!(l.byte_to_piece(1048576), None); // past end
    }

    #[test]
    fn file_pieces_spanning() {
        let l = make_lengths();
        // File starting at 100000, length 500000 — spans pieces 0..2
        assert_eq!(l.file_pieces(100000, 500000), Some((0, 2)));
    }

    #[test]
    fn file_pieces_single_piece() {
        let l = make_lengths();
        // File entirely within piece 1
        assert_eq!(l.file_pieces(262144, 100), Some((1, 1)));
    }

    #[test]
    fn file_pieces_entire_torrent() {
        let l = make_lengths();
        assert_eq!(l.file_pieces(0, 1048576), Some((0, 3)));
    }

    #[test]
    fn zero_length_torrent() {
        let l = Lengths::new(0, 262144, 16384);
        assert_eq!(l.num_pieces(), 0);
    }

    #[test]
    fn tiny_torrent() {
        let l = Lengths::new(1, 262144, 16384);
        assert_eq!(l.num_pieces(), 1);
        assert_eq!(l.piece_size(0), 1);
        assert_eq!(l.chunks_in_piece(0), 1);
        assert_eq!(l.chunk_info(0, 0), Some((0, 1)));
    }
}
