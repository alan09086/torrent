use std::collections::HashMap;

use ferrite_core::Lengths;

use crate::Bitfield;

/// Tracks per-piece chunk completion and piece-level have/don't-have state.
///
/// Only pieces that are actively downloading have chunk-level tracking
/// (stored in `in_progress`). Completed/verified pieces are tracked in `have`.
pub struct ChunkTracker {
    /// Piece-level completion bitfield.
    have: Bitfield,
    /// Per-piece chunk bitmaps for pieces currently being downloaded.
    in_progress: HashMap<u32, Bitfield>,
    /// Piece/chunk arithmetic.
    lengths: Lengths,
}

impl ChunkTracker {
    /// Create a new tracker with no pieces completed.
    pub fn new(lengths: Lengths) -> Self {
        let have = Bitfield::new(lengths.num_pieces());
        ChunkTracker {
            have,
            in_progress: HashMap::new(),
            lengths,
        }
    }

    /// Create a tracker from a previously-persisted bitfield (resume support).
    pub fn from_bitfield(have: Bitfield, lengths: Lengths) -> Self {
        ChunkTracker {
            have,
            in_progress: HashMap::new(),
            lengths,
        }
    }

    /// Record that a chunk has been received.
    ///
    /// Returns `true` if this was the final chunk completing the piece.
    pub fn chunk_received(&mut self, piece: u32, begin: u32) -> bool {
        if piece >= self.lengths.num_pieces() {
            return false;
        }

        let num_chunks = self.lengths.chunks_in_piece(piece);
        let chunk_index = begin / self.lengths.chunk_size();

        let chunk_bf = self
            .in_progress
            .entry(piece)
            .or_insert_with(|| Bitfield::new(num_chunks));

        chunk_bf.set(chunk_index);
        chunk_bf.all_set()
    }

    /// Mark a piece as verified (hash matched). Removes chunk-level tracking.
    pub fn mark_verified(&mut self, piece: u32) {
        self.have.set(piece);
        self.in_progress.remove(&piece);
    }

    /// Mark a piece as failed verification. Resets chunk-level tracking.
    pub fn mark_failed(&mut self, piece: u32) {
        self.in_progress.remove(&piece);
    }

    /// Clear a piece from the have bitfield (share mode eviction).
    /// Does NOT affect in_progress tracking.
    pub fn clear_piece(&mut self, piece: u32) {
        self.have.clear(piece);
    }

    /// Check whether a specific chunk has been received (but piece not yet verified).
    pub fn has_chunk(&self, piece: u32, begin: u32) -> bool {
        if self.have.get(piece) {
            return true;
        }
        let chunk_index = begin / self.lengths.chunk_size();
        self.in_progress
            .get(&piece)
            .is_some_and(|bf| bf.get(chunk_index))
    }

    /// Check whether a piece is fully verified.
    pub fn has_piece(&self, piece: u32) -> bool {
        self.have.get(piece)
    }

    /// Reference to the piece-level have bitfield.
    pub fn bitfield(&self) -> &Bitfield {
        &self.have
    }

    /// Return chunk offsets that are still missing for a piece.
    pub fn missing_chunks(&self, piece: u32) -> Vec<(u32, u32)> {
        if self.have.get(piece) {
            return Vec::new();
        }

        let num_chunks = self.lengths.chunks_in_piece(piece);

        match self.in_progress.get(&piece) {
            Some(bf) => bf
                .zeros()
                .filter_map(|ci| self.lengths.chunk_info(piece, ci))
                .collect(),
            None => (0..num_chunks)
                .filter_map(|ci| self.lengths.chunk_info(piece, ci))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tracker() -> ChunkTracker {
        // 100000 bytes, 50000 byte pieces (2 pieces), 16384 byte chunks
        // Piece 0: 50000 bytes → 4 chunks (16384, 16384, 16384, 848)
        // Piece 1: 50000 bytes → 4 chunks
        ChunkTracker::new(Lengths::new(100000, 50000, 16384))
    }

    #[test]
    fn new_all_missing() {
        let ct = make_tracker();
        assert!(!ct.has_piece(0));
        assert!(!ct.has_piece(1));
        assert_eq!(ct.bitfield().count_ones(), 0);
    }

    #[test]
    fn chunk_received() {
        let mut ct = make_tracker();
        // First chunk doesn't complete the piece
        assert!(!ct.chunk_received(0, 0));
        assert!(ct.has_chunk(0, 0));
        assert!(!ct.has_chunk(0, 16384));
    }

    #[test]
    fn piece_complete() {
        let mut ct = make_tracker();
        // Receive all 4 chunks of piece 0
        assert!(!ct.chunk_received(0, 0));
        assert!(!ct.chunk_received(0, 16384));
        assert!(!ct.chunk_received(0, 32768));
        // Last chunk completes the piece
        assert!(ct.chunk_received(0, 49152));
    }

    #[test]
    fn mark_verified() {
        let mut ct = make_tracker();
        ct.chunk_received(0, 0);
        ct.chunk_received(0, 16384);
        ct.chunk_received(0, 32768);
        ct.chunk_received(0, 49152);
        ct.mark_verified(0);
        assert!(ct.has_piece(0));
        assert!(ct.has_chunk(0, 0)); // has_chunk returns true for verified pieces
        assert_eq!(ct.bitfield().count_ones(), 1);
    }

    #[test]
    fn mark_failed_resets() {
        let mut ct = make_tracker();
        ct.chunk_received(0, 0);
        ct.chunk_received(0, 16384);
        ct.mark_failed(0);
        // Chunk state is gone, piece not verified
        assert!(!ct.has_piece(0));
        assert!(!ct.has_chunk(0, 0));
        // missing_chunks returns all chunks again
        assert_eq!(ct.missing_chunks(0).len(), 4);
    }

    #[test]
    fn has_chunk() {
        let mut ct = make_tracker();
        assert!(!ct.has_chunk(0, 0));
        ct.chunk_received(0, 0);
        assert!(ct.has_chunk(0, 0));
        assert!(!ct.has_chunk(0, 16384));
    }

    #[test]
    fn missing_chunks() {
        let mut ct = make_tracker();
        // All 4 chunks missing initially
        let missing = ct.missing_chunks(0);
        assert_eq!(missing.len(), 4);
        assert_eq!(missing[0], (0, 16384));
        assert_eq!(missing[1], (16384, 16384));

        // Receive first chunk → 3 missing
        ct.chunk_received(0, 0);
        let missing = ct.missing_chunks(0);
        assert_eq!(missing.len(), 3);
        assert_eq!(missing[0], (16384, 16384));
    }

    #[test]
    fn from_bitfield() {
        let lengths = Lengths::new(100000, 50000, 16384);
        let mut have = Bitfield::new(2);
        have.set(0);
        let ct = ChunkTracker::from_bitfield(have, lengths);
        assert!(ct.has_piece(0));
        assert!(!ct.has_piece(1));
        assert!(ct.missing_chunks(0).is_empty());
    }

    #[test]
    fn clear_piece_removes_from_have() {
        let mut ct = make_tracker();
        ct.mark_verified(0);
        assert!(ct.has_piece(0));
        ct.clear_piece(0);
        assert!(!ct.has_piece(0));
        assert_eq!(ct.bitfield().count_ones(), 0);
    }
}
