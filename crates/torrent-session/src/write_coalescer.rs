use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use torrent_core::Lengths;

use crate::piece_buffer_pool::PieceBufferPool;

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
///
/// # M99: Pool integration
///
/// When constructed with an `Option<Arc<PieceBufferPool>>`, the coalescer can
/// accept pre-allocated buffers via [`set_buffer`] and return them on flush for
/// recycling. Without a pool, the coalescer self-allocates (existing M98
/// behavior).
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
    /// M99: Optional pool reference for buffer lifecycle management.
    pool: Option<Arc<PieceBufferPool>>,
    /// M99: Buffer staged by the caller for the next `start_piece()` call.
    staged_buf: Option<BytesMut>,
}

impl WriteCoalescer {
    /// Create an empty coalescer. No allocation occurs until the first block
    /// arrives.
    ///
    /// When `pool` is `Some`, the coalescer expects callers to supply buffers
    /// via [`set_buffer`] and will return them on flush for recycling. When
    /// `pool` is `None`, the coalescer self-allocates (existing M98 behavior).
    pub fn new(lengths: &Lengths, pool: Option<Arc<PieceBufferPool>>) -> Self {
        Self {
            lengths: lengths.clone(),
            buf: BytesMut::new(),
            current_piece: None,
            blocks_received: 0,
            expected_offset: 0,
            pool,
            staged_buf: None,
        }
    }

    /// Stage a pool-provided buffer for the next piece.
    ///
    /// Called by the peer task after acquiring a buffer+permit from the pool.
    /// The staged buffer is consumed by `start_piece()` on the next new piece.
    pub fn set_buffer(&mut self, buf: BytesMut) {
        self.staged_buf = Some(buf);
    }

    /// Returns `true` if the caller should acquire a new pool buffer for `piece`.
    ///
    /// This is the case when:
    /// - We have a pool (otherwise the coalescer self-allocates)
    /// - No buffer is currently staged
    /// - The piece differs from what we're currently accumulating (or we have nothing)
    pub fn needs_buffer_for(&self, piece: u32) -> bool {
        self.pool.is_some()
            && self.staged_buf.is_none()
            && self.current_piece != Some(piece)
    }

    /// Accumulate a block into the coalescer.
    ///
    /// Returns `Some((FlushRequest, Option<BytesMut>))` when either:
    /// - The current piece is complete (all chunks received), or
    /// - A block arrives for a *different* piece (the old piece is flushed as
    ///   a partial write and the new piece begins).
    ///
    /// The `Option<BytesMut>` is the returned pool buffer (if pool is active).
    /// When there is no pool, it is `None`.
    pub fn add_block(
        &mut self,
        piece: u32,
        begin: u32,
        data: &[u8],
    ) -> Option<(FlushRequest, Option<BytesMut>)> {
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
    /// Returns `None` if the coalescer is empty. The `Option<BytesMut>` in the
    /// tuple is the returned pool buffer for recycling (or `None` without a
    /// pool).
    pub fn flush(&mut self) -> Option<(FlushRequest, Option<BytesMut>)> {
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

        // M99: Use staged pool buffer if available.
        if let Some(mut staged) = self.staged_buf.take() {
            staged.clear();
            self.buf = staged;
        } else if self.buf.capacity() >= piece_size {
            // Reuse the existing allocation if it is large enough.
            self.buf.clear();
        } else {
            // Allocate a fresh buffer. `BytesMut::with_capacity` is
            // preferred over `reserve` on an empty buf because it avoids a
            // potential realloc of a too-small leftover allocation.
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
    fn check_complete(&mut self) -> Option<(FlushRequest, Option<BytesMut>)> {
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
    /// buffered data. When a pool is active, the `BytesMut` is extracted and
    /// returned for recycling. Without a pool, the buffer is cleared in-place
    /// so `start_piece()` can reuse the same allocation.
    fn flush_current(&mut self) -> Option<(FlushRequest, Option<BytesMut>)> {
        let piece = self.current_piece.take()?;
        let data = Bytes::copy_from_slice(&self.buf);

        // If pool-managed, return the BytesMut for recycling instead of
        // retaining it.
        let returned_buf = if self.pool.is_some() {
            let mut buf = std::mem::replace(&mut self.buf, BytesMut::new());
            buf.clear();
            Some(buf)
        } else {
            // No pool — retain buffer for reuse (existing M98 behavior).
            self.buf.clear();
            None
        };

        self.blocks_received = 0;
        self.expected_offset = 0;
        Some((FlushRequest { piece, begin: 0, data }, returned_buf))
    }
}

impl Drop for WriteCoalescer {
    fn drop(&mut self) {
        if let Some(ref pool) = self.pool {
            // Return the active buffer if it has pool-sized capacity.
            if self.buf.capacity() > 0 {
                let buf = std::mem::replace(&mut self.buf, BytesMut::new());
                pool.release_buffer(buf);
            }
            // Return staged buffer.
            if let Some(buf) = self.staged_buf.take() {
                pool.release_buffer(buf);
            }
        }
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
        // 512 KiB piece, 16 KiB chunks -> 32 blocks.
        let lengths = standard_lengths(PIECE_LEN * 4);
        let mut wc = WriteCoalescer::new(&lengths, None);

        let blocks_per_piece = lengths.chunks_in_piece(0);
        assert_eq!(blocks_per_piece, 32);

        // Feed blocks 0..30 -- all should return None.
        for i in 0..blocks_per_piece - 1 {
            let begin = i * CHUNK;
            let result = wc.add_block(0, begin, &block_data(i as u8, CHUNK as usize));
            assert!(
                result.is_none(),
                "block {i} should not trigger flush"
            );
        }

        // Feed the 32nd block -- should trigger a full-piece flush.
        let last_begin = (blocks_per_piece - 1) * CHUNK;
        let result = wc.add_block(
            0,
            last_begin,
            &block_data(31, CHUNK as usize),
        );
        let (flush, _ret_buf) = result.expect("32nd block should trigger flush");
        assert_eq!(flush.piece, 0);
        assert_eq!(flush.begin, 0);
        assert_eq!(flush.data.len(), PIECE_LEN as usize);
        assert!(wc.is_empty());
    }

    #[test]
    fn piece_switch_flushes_partial() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let mut wc = WriteCoalescer::new(&lengths, None);

        // Feed 10 blocks for piece 0.
        for i in 0..10u32 {
            let begin = i * CHUNK;
            let result = wc.add_block(0, begin, &block_data(0xAA, CHUNK as usize));
            assert!(result.is_none());
        }

        // Feed a block for piece 1 -- should flush piece 0 as partial.
        let (flush, _ret_buf) = wc
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
        let mut wc = WriteCoalescer::new(&lengths, None);

        let blocks_per_piece = lengths.chunks_in_piece(0);

        // Complete piece 0.
        for i in 0..blocks_per_piece {
            let begin = i * CHUNK;
            let result = wc.add_block(0, begin, &block_data(0xCC, CHUNK as usize));
            if i < blocks_per_piece - 1 {
                assert!(result.is_none());
            } else {
                let (flush, _ret_buf) = result.expect("last block should flush");
                assert_eq!(flush.piece, 0);
                assert_eq!(flush.data.len(), PIECE_LEN as usize);
            }
        }

        // Coalescer should be empty after full-piece flush.
        assert!(wc.is_empty());

        // Add first block of piece 1 -- should return None (new piece started).
        let result = wc.add_block(1, 0, &block_data(0xDD, CHUNK as usize));
        assert!(result.is_none());
        assert!(!wc.is_empty());
    }

    #[test]
    fn flush_on_disconnect() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let mut wc = WriteCoalescer::new(&lengths, None);

        // Feed 15 blocks for piece 0.
        for i in 0..15u32 {
            let begin = i * CHUNK;
            let result = wc.add_block(0, begin, &block_data(0xEE, CHUNK as usize));
            assert!(result.is_none());
        }

        assert!(!wc.is_empty());

        // Explicit flush (simulating disconnect).
        let (flush, _ret_buf) = wc.flush().expect("should flush partial data");
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

        let mut wc = WriteCoalescer::new(&lengths, None);

        // Feed 1 block for the last piece -- should flush immediately.
        let (flush, _ret_buf) = wc
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
        let mut wc = WriteCoalescer::new(&lengths, None);

        // First block at offset 0 -- fine.
        wc.add_block(0, 0, &block_data(0x00, CHUNK as usize));

        // Second block at offset 32768, skipping 16384 -- should panic.
        wc.add_block(0, CHUNK * 2, &block_data(0x01, CHUNK as usize));
    }

    #[test]
    fn empty_coalescer_flush_returns_none() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let wc_mut = &mut WriteCoalescer::new(&lengths, None);

        assert!(wc_mut.is_empty());
        assert!(wc_mut.flush().is_none());
    }

    #[test]
    fn buffer_reuse_across_pieces() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let mut wc = WriteCoalescer::new(&lengths, None);

        // Complete piece 0 -- triggers flush via check_complete
        let mut flush0 = None;
        for i in 0..32u32 {
            if let Some((f, _ret_buf)) =
                wc.add_block(0, i * CHUNK, &block_data(i as u8, CHUNK as usize))
            {
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

        // Start piece 1 -- should reuse the existing buffer (no new alloc)
        let _ = wc.add_block(1, 0, &block_data(0xAA, CHUNK as usize));
        assert!(wc.buf.capacity() >= PIECE_LEN as usize);

        // Verify piece 0 data is still intact after piece 1 started writing
        assert_eq!(flush0.data[0], 0, "flush data must be independent of buffer reuse");
    }

    // -- M99: Pool integration tests --

    #[test]
    fn pool_set_buffer_used_by_start_piece() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let pool = Arc::new(PieceBufferPool::new(4, PIECE_LEN as usize));
        let mut wc = WriteCoalescer::new(&lengths, Some(Arc::clone(&pool)));

        // Stage a pool buffer.
        let (buf, _permit) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime")
            .block_on(pool.acquire())
            .expect("acquire failed");
        wc.set_buffer(buf);

        // needs_buffer_for should be false now (we have a staged buf).
        assert!(!wc.needs_buffer_for(0));

        // Add a block -- should consume the staged buffer.
        let result = wc.add_block(0, 0, &block_data(0xAA, CHUNK as usize));
        assert!(result.is_none()); // not complete yet
        assert!(wc.staged_buf.is_none()); // staged was consumed
    }

    #[test]
    fn pool_flush_returns_buffer() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let pool = Arc::new(PieceBufferPool::new(4, PIECE_LEN as usize));
        let mut wc = WriteCoalescer::new(&lengths, Some(Arc::clone(&pool)));

        // Stage and add blocks.
        let (buf, _permit) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime")
            .block_on(pool.acquire())
            .expect("acquire failed");
        wc.set_buffer(buf);

        wc.add_block(0, 0, &block_data(0xBB, CHUNK as usize));

        // Explicit flush should return the buffer.
        let (flush, ret_buf) = wc.flush().expect("should flush");
        assert_eq!(flush.piece, 0);
        assert!(ret_buf.is_some(), "pool-managed flush should return buffer");
        let ret_buf = ret_buf.expect("already checked is_some");
        assert!(ret_buf.capacity() >= PIECE_LEN as usize);
        assert_eq!(ret_buf.len(), 0); // should be cleared
    }

    #[test]
    fn needs_buffer_for_correct_logic() {
        let lengths = standard_lengths(PIECE_LEN * 4);

        // Without pool -- never needs buffer.
        let wc_no_pool = WriteCoalescer::new(&lengths, None);
        assert!(!wc_no_pool.needs_buffer_for(0));

        // With pool -- needs buffer for new piece.
        let pool = Arc::new(PieceBufferPool::new(4, PIECE_LEN as usize));
        let mut wc = WriteCoalescer::new(&lengths, Some(Arc::clone(&pool)));
        assert!(wc.needs_buffer_for(0)); // no current piece, no staged buf

        // Stage a buffer -- no longer needs one.
        let (buf, _permit) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime")
            .block_on(pool.acquire())
            .expect("acquire failed");
        wc.set_buffer(buf);
        assert!(!wc.needs_buffer_for(0));
    }

    #[test]
    fn no_pool_flush_returns_no_buffer() {
        let lengths = standard_lengths(PIECE_LEN * 4);
        let mut wc = WriteCoalescer::new(&lengths, None);

        wc.add_block(0, 0, &block_data(0xAA, CHUNK as usize));
        let (flush, ret_buf) = wc.flush().expect("should flush");
        assert_eq!(flush.piece, 0);
        assert!(ret_buf.is_none(), "no-pool flush should not return buffer");
    }
}
