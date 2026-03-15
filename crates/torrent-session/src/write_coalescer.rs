use bytes::{Bytes, BytesMut};
use torrent_core::Lengths;

/// Result of a flush operation — tells the caller what to write to disk.
pub(crate) struct FlushRequest {
    pub piece: u32,
    /// Byte offset within the piece (always 0 for full-piece flush).
    pub begin: u32,
    /// The coalesced piece data (`BytesMut` frozen into `Bytes`).
    pub data: Bytes,
}

/// Per-peer write coalescer that buffers incoming 16 KiB blocks and flushes
/// them as a single contiguous write when a piece is complete.
///
/// Reduces disk syscalls by ~32x (one write per piece instead of one per block).
/// Defers `BytesMut` allocation until the first `start_piece()` call.
pub(crate) struct WriteCoalescer {
    lengths: Lengths,
    /// Accumulation buffer. Starts at zero capacity; allocated with the correct
    /// piece size on the first block of each new piece.
    buf: BytesMut,
    /// Which piece we are currently accumulating, if any.
    current_piece: Option<u32>,
    /// Number of blocks received for the current piece.
    blocks_received: u32,
    /// Next expected `begin` offset (for sequential-order debug assertions).
    expected_offset: u32,
}

impl WriteCoalescer {
    /// Create an empty coalescer. No allocation occurs until the first block
    /// arrives.
    pub fn new(lengths: &Lengths) -> Self {
        Self {
            lengths: lengths.clone(),
            buf: BytesMut::new(),
            current_piece: None,
            blocks_received: 0,
            expected_offset: 0,
        }
    }

    /// Accumulate a block into the coalescer.
    ///
    /// Returns `Some(FlushRequest)` when either:
    /// - The current piece is complete (all chunks received), or
    /// - A block arrives for a *different* piece (the old piece is flushed as
    ///   a partial write and the new piece begins).
    pub fn add_block(&mut self, piece: u32, begin: u32, data: &[u8]) -> Option<FlushRequest> {
        match self.current_piece {
            None => {
                // First block ever, or after a flush — start a new piece.
                self.start_piece(piece, begin, data);
                self.check_complete()
            }
            Some(cur) if cur == piece => {
                // Same piece — append sequentially.
                debug_assert!(
                    begin == self.expected_offset,
                    "out-of-order block: expected begin={}, got begin={begin} for piece {piece}",
                    self.expected_offset,
                );
                self.buf.extend_from_slice(data);
                self.blocks_received = self.blocks_received.saturating_add(1);
                self.expected_offset = self
                    .expected_offset
                    .saturating_add(data.len() as u32);
                self.check_complete()
            }
            Some(_cur) => {
                // Different piece — flush the old one, start the new one.
                let flushed = self.flush_current();
                self.start_piece(piece, begin, data);

                // If the new piece is also immediately complete (e.g. a
                // single-block last piece), we cannot return two flush requests
                // from one call.  The caller will observe the completion on
                // their next add_block() or explicit flush().  Return the
                // partial flush of the *old* piece.
                flushed
            }
        }
    }

    /// Flush any buffered partial data (e.g., on peer disconnect).
    ///
    /// Returns `None` if the coalescer is empty.
    pub fn flush(&mut self) -> Option<FlushRequest> {
        self.flush_current()
    }

    /// Returns `true` if no piece is currently being buffered.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.current_piece.is_none()
    }

    // -- private helpers --

    /// Begin accumulating a new piece, allocating the buffer to the exact
    /// piece size.
    fn start_piece(&mut self, piece: u32, begin: u32, data: &[u8]) {
        let piece_size = self.lengths.piece_size(piece) as usize;

        // Reuse the existing allocation if it is large enough; otherwise
        // allocate a fresh buffer.  `BytesMut::with_capacity` is
        // preferred over `reserve` on an empty buf because it avoids a
        // potential realloc of a too-small leftover allocation.
        if self.buf.capacity() >= piece_size {
            self.buf.clear();
        } else {
            self.buf = BytesMut::with_capacity(piece_size);
        }

        debug_assert!(
            begin == 0,
            "first block of piece {piece} should start at offset 0, got {begin}",
        );

        self.buf.extend_from_slice(data);
        self.current_piece = Some(piece);
        self.blocks_received = 1;
        self.expected_offset = data.len() as u32;
    }

    /// If the current piece has received all its chunks, freeze the buffer and
    /// return a `FlushRequest`.
    fn check_complete(&mut self) -> Option<FlushRequest> {
        let piece = self.current_piece?;
        let expected_blocks = self.lengths.chunks_in_piece(piece);

        if self.blocks_received >= expected_blocks {
            self.flush_current()
        } else {
            None
        }
    }

    /// Drain the internal state and return the buffered data as a
    /// `FlushRequest`.  Returns `None` if there is nothing buffered.
    ///
    /// Uses `Bytes::copy_from_slice` to create an independent copy of the
    /// buffered data, then `clear()` to reset the BytesMut length while
    /// retaining its capacity.  This lets `start_piece()` reuse the same
    /// allocation for the next piece instead of allocating fresh 512 KiB
    /// every time.
    fn flush_current(&mut self) -> Option<FlushRequest> {
        let piece = self.current_piece.take()?;
        let data = Bytes::copy_from_slice(&self.buf);
        self.buf.clear();
        self.blocks_received = 0;
        self.expected_offset = 0;
        Some(FlushRequest {
            piece,
            begin: 0,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: u32 = 16384;
    const PIECE_LEN: u64 = 512 * 1024; // 512 KiB

    /// Helper: create `Lengths` for a standard torrent with 512 KiB pieces.
    fn standard_lengths(total: u64) -> Lengths {
        Lengths::new(total, PIECE_LEN, CHUNK)
    }

    /// Helper: generate a block payload of the given size filled with a
    /// recognisable byte pattern.
    fn block_data(fill: u8, len: usize) -> Vec<u8> {
        vec![fill; len]
    }

    #[test]
    fn coalescing_correctness_full_piece() {
        // 512 KiB piece, 16 KiB chunks → 32 blocks.
        let lengths = standard_lengths(PIECE_LEN * 4);
        let mut wc = WriteCoalescer::new(&lengths);

        let blocks_per_piece = lengths.chunks_in_piece(0);
        assert_eq!(blocks_per_piece, 32);

        // Feed blocks 0..30 — all should return None.
        for i in 0..blocks_per_piece - 1 {
            let begin = i * CHUNK;
            let result = wc.add_block(0, begin, &block_data(i as u8, CHUNK as usize));
            assert!(
                result.is_none(),
                "block {i} should not trigger flush"
            );
        }

        // Feed the 32nd block — should trigger a full-piece flush.
        let last_begin = (blocks_per_piece - 1) * CHUNK;
        let result = wc.add_block(
            0,
            last_begin,
            &block_data(31, CHUNK as usize),
        );
        let flush = result.expect("32nd block should trigger flush");
        assert_eq!(flush.piece, 0);
        assert_eq!(flush.begin, 0);
        assert_eq!(flush.data.len(), PIECE_LEN as usize);
        assert!(wc.is_empty());
    }

    #[test]
    fn piece_switch_flushes_partial() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let mut wc = WriteCoalescer::new(&lengths);

        // Feed 10 blocks for piece 0.
        for i in 0..10u32 {
            let begin = i * CHUNK;
            let result = wc.add_block(0, begin, &block_data(0xAA, CHUNK as usize));
            assert!(result.is_none());
        }

        // Feed a block for piece 1 — should flush piece 0 as partial.
        let flush = wc
            .add_block(1, 0, &block_data(0xBB, CHUNK as usize))
            .expect("piece switch should flush old piece");
        assert_eq!(flush.piece, 0);
        assert_eq!(flush.begin, 0);
        assert_eq!(flush.data.len(), 10 * CHUNK as usize);

        // Coalescer should now have piece 1 started.
        assert!(!wc.is_empty());
        assert_eq!(wc.current_piece, Some(1));
        assert_eq!(wc.blocks_received, 1);
    }

    #[test]
    fn piece_boundary_flush_then_new_piece() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let mut wc = WriteCoalescer::new(&lengths);

        let blocks_per_piece = lengths.chunks_in_piece(0);

        // Complete piece 0.
        for i in 0..blocks_per_piece {
            let begin = i * CHUNK;
            let result = wc.add_block(0, begin, &block_data(0xCC, CHUNK as usize));
            if i < blocks_per_piece - 1 {
                assert!(result.is_none());
            } else {
                let flush = result.expect("last block should flush");
                assert_eq!(flush.piece, 0);
                assert_eq!(flush.data.len(), PIECE_LEN as usize);
            }
        }

        // Coalescer should be empty after full-piece flush.
        assert!(wc.is_empty());

        // Add first block of piece 1 — should return None (new piece started).
        let result = wc.add_block(1, 0, &block_data(0xDD, CHUNK as usize));
        assert!(result.is_none());
        assert!(!wc.is_empty());
    }

    #[test]
    fn flush_on_disconnect() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let mut wc = WriteCoalescer::new(&lengths);

        // Feed 15 blocks for piece 0.
        for i in 0..15u32 {
            let begin = i * CHUNK;
            let result = wc.add_block(0, begin, &block_data(0xEE, CHUNK as usize));
            assert!(result.is_none());
        }

        assert!(!wc.is_empty());

        // Explicit flush (simulating disconnect).
        let flush = wc.flush().expect("should flush partial data");
        assert_eq!(flush.piece, 0);
        assert_eq!(flush.begin, 0);
        assert_eq!(flush.data.len(), 15 * CHUNK as usize);

        assert!(wc.is_empty());
    }

    #[test]
    fn last_piece_smaller() {
        // Total = 512 KiB * 3 + 16 KiB = 4 pieces, last piece is 16 KiB (1 block).
        let total = PIECE_LEN * 3 + u64::from(CHUNK);
        let lengths = standard_lengths(total);
        assert_eq!(lengths.num_pieces(), 4);
        assert_eq!(lengths.piece_size(3), CHUNK);
        assert_eq!(lengths.chunks_in_piece(3), 1);

        let mut wc = WriteCoalescer::new(&lengths);

        // Feed 1 block for the last piece — should flush immediately.
        let flush = wc
            .add_block(3, 0, &block_data(0xFF, CHUNK as usize))
            .expect("single-block piece should flush immediately");
        assert_eq!(flush.piece, 3);
        assert_eq!(flush.data.len(), CHUNK as usize);
        assert!(wc.is_empty());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "out-of-order block")]
    fn debug_assert_on_out_of_order() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let mut wc = WriteCoalescer::new(&lengths);

        // First block at offset 0 — fine.
        wc.add_block(0, 0, &block_data(0x00, CHUNK as usize));

        // Second block at offset 32768, skipping 16384 — should panic.
        wc.add_block(0, CHUNK * 2, &block_data(0x01, CHUNK as usize));
    }

    #[test]
    fn empty_coalescer_flush_returns_none() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let wc_mut = &mut WriteCoalescer::new(&lengths);

        assert!(wc_mut.is_empty());
        assert!(wc_mut.flush().is_none());
    }

    #[test]
    fn buffer_reuse_across_pieces() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let mut wc = WriteCoalescer::new(&lengths);

        // Complete piece 0 — triggers flush via check_complete
        let mut flush0 = None;
        for i in 0..32u32 {
            if let Some(f) = wc.add_block(0, i * CHUNK, &block_data(i as u8, CHUNK as usize)) {
                flush0 = Some(f);
            }
        }
        let flush0 = flush0.expect("piece 0 should have flushed");

        // After flush: buf should be empty but retain capacity for reuse
        assert!(wc.is_empty());
        assert!(
            wc.buf.capacity() >= PIECE_LEN as usize,
            "buffer should retain capacity after flush, got {}",
            wc.buf.capacity()
        );

        // Data independence: flushed Bytes must survive buffer reuse
        assert_eq!(flush0.data[0], 0); // first block fill byte
        assert_eq!(flush0.data.len(), PIECE_LEN as usize);

        // Start piece 1 — should reuse the existing buffer (no new alloc)
        let _ = wc.add_block(1, 0, &block_data(0xAA, CHUNK as usize));
        assert!(wc.buf.capacity() >= PIECE_LEN as usize);

        // Verify piece 0 data is still intact after piece 1 started writing
        assert_eq!(flush0.data[0], 0, "flush data must be independent of buffer reuse");
    }
}
