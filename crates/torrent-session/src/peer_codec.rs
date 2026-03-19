//! Ring-buffer based codec for the BitTorrent peer wire protocol.
//!
//! Replaces `tokio_util::codec::FramedRead`/`FramedWrite` with fixed-size
//! buffers that never reallocate on the hot path.

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use torrent_wire::Message;

// ---------------------------------------------------------------------------
// ReadBuf — 32 KiB ring buffer
// ---------------------------------------------------------------------------

/// Size of the fixed ring buffer in bytes.
const BUF_LEN: usize = 32 * 1024;

/// A fixed-size 32 KiB ring buffer for reading peer wire data.
///
/// Data is stored in a contiguous `Box<[u8; BUF_LEN]>` and accessed via
/// `start` (read cursor) and `len` (readable byte count). When data wraps
/// past the end of the backing array the two-slice accessors expose both
/// halves.
pub(crate) struct ReadBuf {
    buf: Box<[u8; BUF_LEN]>,
    /// Index of the first readable byte (modulo `BUF_LEN`).
    start: usize,
    /// Number of readable bytes currently stored.
    len: usize,
}

impl ReadBuf {
    /// Create a new zeroed read buffer.
    pub(crate) fn new() -> Self {
        Self {
            buf: Box::new([0u8; BUF_LEN]),
            start: 0,
            len: 0,
        }
    }

    /// Number of readable bytes in the buffer.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when no more bytes can be written.
    #[inline]
    #[allow(dead_code)] // Used in tests; retained for completeness.
    pub(crate) fn is_full(&self) -> bool {
        self.len == BUF_LEN
    }

    /// Returns two borrowed slices covering `len` bytes starting at `offset`
    /// bytes from the read cursor.  Does NOT consume.
    ///
    /// The first slice may contain all the data (second empty) or the data
    /// may wrap around the ring boundary.
    ///
    /// # Panics
    ///
    /// Panics (debug) if `offset + len` exceeds the buffered data length.
    pub(crate) fn readable_slices_at(&self, offset: usize, len: usize) -> (&[u8], &[u8]) {
        debug_assert!(
            offset + len <= self.len,
            "readable_slices_at: offset={offset} len={len} buf_len={}",
            self.len,
        );
        if len == 0 {
            return (&[], &[]);
        }
        let start = (self.start + offset) % BUF_LEN;
        let end = start + len;
        if end <= BUF_LEN {
            (&self.buf[start..end], &[])
        } else {
            (&self.buf[start..], &self.buf[..end - BUF_LEN])
        }
    }

    /// Two slices covering all readable data. The second slice is non-empty
    /// only when the readable region wraps around the end of the backing
    /// array.
    #[allow(dead_code)] // Used in tests and as_double_buf(); retained for completeness.
    pub(crate) fn readable_slices(&self) -> (&[u8], &[u8]) {
        if self.len == 0 {
            return (&[], &[]);
        }
        let end = self.start + self.len;
        if end <= BUF_LEN {
            (&self.buf[self.start..end], &[])
        } else {
            (&self.buf[self.start..], &self.buf[..end - BUF_LEN])
        }
    }

    /// First contiguous writable region. This is the region from the write
    /// cursor to either the end of the backing array or the start of the
    /// readable region, whichever comes first.
    pub(crate) fn unfilled_contiguous(&mut self) -> &mut [u8] {
        if self.len == BUF_LEN {
            return &mut [];
        }
        let write_pos = (self.start + self.len) % BUF_LEN;
        if write_pos >= self.start {
            &mut self.buf[write_pos..BUF_LEN]
        } else {
            &mut self.buf[write_pos..self.start]
        }
    }

    /// Both writable regions. When the write cursor is past the read cursor
    /// the first slice runs to the end of the array and the second slice
    /// covers `[0..start)`. Uses `split_at_mut` to safely produce two
    /// mutable borrows.
    #[allow(dead_code)] // Used in tests; retained for vectored I/O future use.
    pub(crate) fn unfilled_ioslices(&mut self) -> (&mut [u8], &mut [u8]) {
        let available = BUF_LEN - self.len;
        if available == 0 {
            return (&mut [], &mut []);
        }
        let write_pos = (self.start + self.len) % BUF_LEN;
        if write_pos >= self.start {
            // Case 1: write_pos is at or after start.
            // Writable: [write_pos..BUF_LEN) and [0..start).
            // But if start == 0 (and write_pos == len), only [write_pos..BUF_LEN).
            let (left, right) = self.buf.split_at_mut(write_pos);
            // right = [write_pos..BUF_LEN], left = [0..write_pos]
            // Writable in right: all of it. Writable in left: [0..start).
            let first = right; // [write_pos..BUF_LEN]
            let second = &mut left[..self.start];
            (first, second)
        } else {
            // Case 2: write_pos < start — single contiguous region.
            (&mut self.buf[write_pos..self.start], &mut [])
        }
    }

    /// Record that `n` bytes were written into the unfilled region.
    ///
    /// # Panics
    ///
    /// Panics (debug) if `n` would overflow the buffer.
    pub(crate) fn mark_filled(&mut self, n: usize) {
        debug_assert!(
            self.len + n <= BUF_LEN,
            "mark_filled overflow: len={} n={n} cap={BUF_LEN}",
            self.len,
        );
        self.len += n;
    }

    /// Advance the read cursor by `n` bytes. When the buffer empties, resets
    /// `start` to 0 so the next fill is contiguous.
    ///
    /// # Panics
    ///
    /// Panics (debug) if `n > self.len`.
    pub(crate) fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.len, "consume underflow: n={n} len={}", self.len,);
        self.start = (self.start + n) % BUF_LEN;
        self.len -= n;
        if self.len == 0 {
            self.start = 0;
        }
    }

    /// Copy `dst.len()` bytes from the ring into `dst`, consuming them.
    ///
    /// Handles the wrap boundary with up to two `copy_from_slice` calls.
    ///
    /// # Panics
    ///
    /// Panics (debug) if `dst.len() > self.len`.
    pub(crate) fn consume_into(&mut self, dst: &mut [u8]) {
        let n = dst.len();
        debug_assert!(
            n <= self.len,
            "consume_into underflow: n={n} len={}",
            self.len,
        );

        let end = self.start + n;
        if end <= BUF_LEN {
            dst.copy_from_slice(&self.buf[self.start..end]);
        } else {
            let first = BUF_LEN - self.start;
            dst[..first].copy_from_slice(&self.buf[self.start..]);
            dst[first..].copy_from_slice(&self.buf[..n - first]);
        }
        self.consume(n);
    }

    /// Extract `n` bytes as an owned `Bytes`, consuming them from the ring.
    ///
    /// Skips zeroing the intermediate buffer since `consume_into` immediately
    /// writes all `n` bytes.
    #[allow(clippy::uninit_vec)]
    #[allow(dead_code)] // Retained for callers that need owned Bytes; try_decode() uses zero-copy path.
    pub(crate) fn consume_as_bytes(&mut self, n: usize) -> Bytes {
        let mut vec = Vec::with_capacity(n);
        // SAFETY: consume_into writes exactly `n` bytes via copy_from_slice,
        // fully initializing every element before the Vec is read.
        unsafe { vec.set_len(n) };
        self.consume_into(&mut vec);
        Bytes::from(vec)
    }

    /// Peek at a single byte at `offset` from the read cursor without consuming.
    #[inline]
    pub(crate) fn peek_byte_at(&self, offset: usize) -> u8 {
        debug_assert!(
            offset < self.len,
            "peek_byte_at: offset={offset} len={}",
            self.len
        );
        self.buf[(self.start + offset) % BUF_LEN]
    }

    /// Peek at a big-endian `u32` at `offset` from the read cursor without consuming.
    #[inline]
    pub(crate) fn peek_u32_be_at(&self, offset: usize) -> u32 {
        debug_assert!(
            offset + 4 <= self.len,
            "peek_u32_be_at: offset={offset} len={}",
            self.len
        );
        let s = self.start + offset;
        u32::from_be_bytes([
            self.buf[s % BUF_LEN],
            self.buf[(s + 1) % BUF_LEN],
            self.buf[(s + 2) % BUF_LEN],
            self.buf[(s + 3) % BUF_LEN],
        ])
    }

    /// Peek at a big-endian `u16` at `offset` from the read cursor without consuming.
    #[inline]
    pub(crate) fn peek_u16_be_at(&self, offset: usize) -> u16 {
        debug_assert!(
            offset + 2 <= self.len,
            "peek_u16_be_at: offset={offset} len={}",
            self.len
        );
        let s = self.start + offset;
        u16::from_be_bytes([self.buf[s % BUF_LEN], self.buf[(s + 1) % BUF_LEN]])
    }

    /// Peek at the first 4 bytes as a big-endian `u32` without consuming.
    ///
    /// # Panics
    ///
    /// Panics (debug) if fewer than 4 bytes are readable.
    pub(crate) fn peek_u32_be(&self) -> u32 {
        debug_assert!(
            self.len >= 4,
            "peek_u32_be: need 4 bytes, have {}",
            self.len,
        );
        let b0 = self.buf[(self.start) % BUF_LEN];
        let b1 = self.buf[(self.start + 1) % BUF_LEN];
        let b2 = self.buf[(self.start + 2) % BUF_LEN];
        let b3 = self.buf[(self.start + 3) % BUF_LEN];
        u32::from_be_bytes([b0, b1, b2, b3])
    }

    /// Create a [`DoubleBufHelper`] cursor over the current readable data.
    #[allow(dead_code)] // Used in tests; retained for future split-buffer parsing.
    pub(crate) fn as_double_buf(&self) -> DoubleBufHelper<'_> {
        let (a, b) = self.readable_slices();
        DoubleBufHelper::new(a, b)
    }
}

// ---------------------------------------------------------------------------
// DoubleBufHelper — split-buffer parser
// ---------------------------------------------------------------------------

/// A cursor over two contiguous slices that together represent a logically
/// contiguous byte sequence (the two halves of a ring buffer's readable
/// region).
#[allow(dead_code)] // Used in tests; retained for future split-buffer parsing.
pub(crate) struct DoubleBufHelper<'a> {
    buf_0: &'a [u8],
    buf_1: &'a [u8],
    pos: usize,
}

#[allow(dead_code)] // Used in tests; retained for future split-buffer parsing.
impl<'a> DoubleBufHelper<'a> {
    /// Create a new helper from the two readable slices.
    pub(crate) fn new(buf_0: &'a [u8], buf_1: &'a [u8]) -> Self {
        Self {
            buf_0,
            buf_1,
            pos: 0,
        }
    }

    /// Total bytes remaining from the current position.
    #[inline]
    pub(crate) fn remaining(&self) -> usize {
        self.buf_0.len() + self.buf_1.len() - self.pos
    }

    /// Read the byte at position `pos` in the combined view.
    #[inline]
    fn byte_at(&self, pos: usize) -> u8 {
        if pos < self.buf_0.len() {
            self.buf_0[pos]
        } else {
            self.buf_1[pos - self.buf_0.len()]
        }
    }

    /// Read `N` bytes into a fixed-size array, advancing the cursor.
    pub(crate) fn consume<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.byte_at(self.pos + i);
        }
        self.pos += N;
        out
    }

    /// Read a big-endian `u32`.
    #[inline]
    pub(crate) fn read_u32_be(&mut self) -> u32 {
        u32::from_be_bytes(self.consume::<4>())
    }

    /// Read a single byte.
    #[inline]
    pub(crate) fn read_u8(&mut self) -> u8 {
        self.consume::<1>()[0]
    }

    /// Return `IoSlice` pair covering up to `limit` bytes of remaining data.
    ///
    /// Useful for vectored writes (e.g., `write_vectored`) where the two
    /// ring-buffer halves can be sent without copying.
    pub(crate) fn as_ioslices(&self, limit: usize) -> [std::io::IoSlice<'_>; 2] {
        let remaining_0 = self.buf_0.len().saturating_sub(self.pos);
        let first_len = remaining_0.min(limit);
        let second_len = self.buf_1.len().min(limit.saturating_sub(first_len));

        let first_start = if self.pos < self.buf_0.len() {
            self.pos
        } else {
            self.buf_0.len()
        };

        [
            std::io::IoSlice::new(&self.buf_0[first_start..first_start + first_len]),
            std::io::IoSlice::new(&self.buf_1[..second_len]),
        ]
    }

    /// Extract `n` bytes as an owned `Bytes`, using bulk memcpy where
    /// possible (not byte-by-byte).
    pub(crate) fn consume_variable(&mut self, n: usize) -> Bytes {
        let mut vec = vec![0u8; n];
        let mut written = 0usize;

        // Phase 1: copy from buf_0 remainder.
        if self.pos < self.buf_0.len() {
            let avail = self.buf_0.len() - self.pos;
            let take = avail.min(n);
            vec[..take].copy_from_slice(&self.buf_0[self.pos..self.pos + take]);
            written = take;
        }

        // Phase 2: copy from buf_1 if needed.
        if written < n {
            let buf1_start = if self.pos > self.buf_0.len() {
                self.pos - self.buf_0.len()
            } else {
                0
            };
            let remaining = n - written;
            vec[written..].copy_from_slice(&self.buf_1[buf1_start..buf1_start + remaining]);
        }

        self.pos += n;
        Bytes::from(vec)
    }
}

// ---------------------------------------------------------------------------
// PeerReader — async message reader
// ---------------------------------------------------------------------------

/// Async reader that decodes BitTorrent peer wire messages from a ring buffer.
///
/// The internal [`ReadBuf`] is 32 KiB — large enough for all standard
/// messages. Messages larger than 32 KiB (rare) fall back to a temporary
/// heap allocation.
///
/// # Zero-copy decode (M110)
///
/// The reader exposes a two-phase API for zero-copy message decoding:
///
/// 1. [`fill_message()`](Self::fill_message) — async, reads from the socket
///    until a complete message is buffered.
/// 2. [`try_decode()`](Self::try_decode) — sync, returns `Message<&[u8]>` with
///    slices borrowing directly from the ring buffer.
/// 3. [`advance()`](Self::advance) — sync, advances the read cursor after the
///    caller has finished processing the message.
///
/// The legacy [`next_message()`](Self::next_message) method is retained for
/// callers that don't need zero-copy semantics — it returns `Message<Bytes>`
/// and advances the cursor automatically.
pub(crate) struct PeerReader<R> {
    reader: R,
    buf: ReadBuf,
    max_message_size: usize,
    /// Reusable buffer for messages whose payload wraps the ring boundary
    /// (Bitfield, Extended) or exceeds ring capacity. Avoids per-message
    /// allocation.
    oversized_buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> PeerReader<R> {
    /// Create a new reader wrapping `reader` with the given maximum message
    /// size.
    pub(crate) fn new(reader: R, max_message_size: usize) -> Self {
        Self {
            reader,
            buf: ReadBuf::new(),
            max_message_size,
            oversized_buf: Vec::new(),
        }
    }

    /// Read data from the socket into the ring buffer. Returns the number of
    /// bytes read, or 0 on EOF.
    async fn fill(&mut self) -> std::io::Result<usize> {
        let slice = self.buf.unfilled_contiguous();
        if slice.is_empty() {
            return Ok(0);
        }
        let n = self.reader.read(slice).await?;
        self.buf.mark_filled(n);
        Ok(n)
    }

    // -----------------------------------------------------------------------
    // Two-phase zero-copy API (M110)
    // -----------------------------------------------------------------------

    /// Fill the ring buffer from the socket until a complete message is
    /// available for decoding, or EOF is reached.
    ///
    /// Returns:
    /// - `Ok(FillStatus::Ready)` — a complete message (or oversized message)
    ///   is buffered and ready for [`try_decode()`](Self::try_decode).
    /// - `Ok(FillStatus::Eof)` — clean end-of-stream (no partial data).
    /// - `Ok(FillStatus::Oversized(length))` — the message payload exceeds
    ///   the ring buffer capacity (> 32 KiB). The caller should use
    ///   [`decode_oversized_into()`](Self::decode_oversized_into) to read it.
    ///
    /// # Errors
    ///
    /// - `torrent_wire::Error::MessageTooLarge` if the length prefix exceeds
    ///   `max_message_size`.
    /// - `torrent_wire::Error::Io` on socket errors or unexpected EOF with
    ///   a partial message buffered.
    pub(crate) async fn fill_message(&mut self) -> Result<FillStatus, torrent_wire::Error> {
        loop {
            if self.buf.len() >= 4 {
                let length = self.buf.peek_u32_be() as usize;

                if length > self.max_message_size {
                    return Err(torrent_wire::Error::MessageTooLarge {
                        size: length,
                        max: self.max_message_size,
                    });
                }

                // keep-alive (length == 0) — ready for try_decode
                if length == 0 {
                    return Ok(FillStatus::Ready);
                }

                // oversized message (> ring buffer capacity including 4-byte length prefix)
                if 4 + length > BUF_LEN {
                    return Ok(FillStatus::Oversized(length));
                }

                // complete message in ring?
                let total = 4 + length;
                if self.buf.len() >= total {
                    return Ok(FillStatus::Ready);
                }
            }

            let n = self.fill().await?;
            if n == 0 {
                if self.buf.len() == 0 {
                    return Ok(FillStatus::Eof);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected EOF with partial message buffered",
                )
                .into());
            }
        }
    }

    /// Decode the buffered message without advancing the ring cursor.
    ///
    /// Must only be called after [`fill_message()`](Self::fill_message)
    /// returns [`FillStatus::Ready`].
    ///
    /// Returns `(message, consumed_bytes)`. The `&[u8]` slices in the
    /// returned `Message` borrow from the ring buffer (or
    /// `self.oversized_buf` for wrapped non-data payloads). The caller
    /// **must** call [`advance(consumed)`](Self::advance) after processing
    /// the message.
    ///
    /// For `Piece` messages, `data_0` and `data_1` are the two ring-buffer
    /// halves; `data_1` is empty when the block does not straddle the ring
    /// boundary.
    pub(crate) fn try_decode(&mut self) -> Result<(Message<&[u8]>, usize), torrent_wire::Error> {
        let length = self.buf.peek_u32_be() as usize;
        let total = 4 + length;

        if length == 0 {
            return Ok((Message::KeepAlive, 4));
        }

        let id = self.buf.peek_byte_at(4);
        decode_from_ring_borrowed(&self.buf, &mut self.oversized_buf, id, length, total)
    }

    /// Advance the read cursor by `n` bytes.
    ///
    /// Called after the borrowed `Message` from [`try_decode()`] has been
    /// processed and is no longer referenced.
    #[inline]
    pub(crate) fn advance(&mut self, n: usize) {
        self.buf.consume(n);
    }

    /// Decode a message whose payload exceeds the ring buffer capacity.
    ///
    /// Must only be called after [`fill_message()`](Self::fill_message)
    /// returns [`FillStatus::Oversized(length)`].
    ///
    /// This reads the oversized payload into `self.oversized_buf`, draining
    /// any buffered data first, then reads the remainder from the socket.
    /// Returns an owned `Message<Bytes>` because the data must be copied
    /// into a contiguous buffer anyway.
    #[allow(clippy::uninit_vec)]
    pub(crate) async fn decode_oversized_into(
        &mut self,
        length: usize,
    ) -> Result<Message, torrent_wire::Error> {
        // Consume the 4-byte length prefix from the ring.
        self.buf.consume(4);

        self.oversized_buf.clear();
        self.oversized_buf.reserve(length);
        // SAFETY: consume_into and reader.read_exact together write all
        // `length` bytes, fully initializing the buffer.
        unsafe { self.oversized_buf.set_len(length) };
        let mut filled = 0;

        // Drain whatever is currently buffered.
        let buffered = self.buf.len().min(length);
        if buffered > 0 {
            self.buf.consume_into(&mut self.oversized_buf[..buffered]);
            filled = buffered;
        }

        // Read the rest directly from the socket (avoids split-borrow of
        // self.reader + self.oversized_buf through self.read_exact).
        if filled < length {
            self.reader
                .read_exact(&mut self.oversized_buf[filled..])
                .await?;
        }

        Message::from_payload(Bytes::from(self.oversized_buf.split_off(0)))
    }

    // -----------------------------------------------------------------------
    // Legacy convenience API
    // -----------------------------------------------------------------------

    /// Decode the next peer wire message (legacy convenience wrapper).
    ///
    /// Returns `Ok(None)` on clean EOF (no partial data buffered).
    /// Automatically advances the ring cursor after decoding.
    ///
    /// This allocates `Bytes` for data-carrying messages. For zero-copy
    /// decoding, use the two-phase [`fill_message()`] + [`try_decode()`] +
    /// [`advance()`] API instead.
    ///
    /// # Errors
    ///
    /// - `torrent_wire::Error::MessageTooLarge` if the length prefix exceeds
    ///   `max_message_size`.
    /// - `torrent_wire::Error::Io` on socket errors or unexpected EOF with
    ///   a partial message buffered.
    #[allow(dead_code)] // Retained as legacy convenience API; peer.rs now uses the three-phase zero-copy path.
    pub(crate) async fn next_message(&mut self) -> Result<Option<Message>, torrent_wire::Error> {
        match self.fill_message().await? {
            FillStatus::Eof => Ok(None),
            FillStatus::Oversized(length) => self.decode_oversized_into(length).await.map(Some),
            FillStatus::Ready => {
                let (msg, consumed) = self.try_decode()?;
                let owned = msg.to_owned_bytes();
                self.advance(consumed);
                Ok(Some(owned))
            }
        }
    }

    /// Read exactly `buf.len()` bytes, draining from the ring first and then
    /// reading from the socket as needed.
    #[allow(dead_code)] // Retained for potential future use; decode_oversized_into reads directly.
    async fn read_exact(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        let mut offset = 0;

        // Drain from ring buffer first.
        let from_ring = self.buf.len().min(buf.len());
        if from_ring > 0 {
            self.buf.consume_into(&mut buf[..from_ring]);
            offset = from_ring;
        }

        // Read remainder directly from socket.
        if offset < buf.len() {
            self.reader.read_exact(&mut buf[offset..]).await?;
        }
        Ok(())
    }
}

/// Status returned by [`PeerReader::fill_message()`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FillStatus {
    /// A complete message is buffered in the ring — call `try_decode()`.
    Ready,
    /// Clean end-of-stream (no partial data buffered).
    Eof,
    /// The message payload length exceeds ring capacity. The `usize` is the
    /// payload length (not including the 4-byte length prefix). Call
    /// `decode_oversized_into(length)`.
    Oversized(usize),
}

/// Decode a message from the ring buffer without consuming any bytes.
///
/// This is a free function to avoid borrow-checker conflicts: it takes
/// immutable access to `buf` (for reading slices) and mutable access to
/// `oversized_buf` (for assembling wrapped payloads) as separate parameters.
///
/// Returns `(message, total_consumed_bytes)`.
fn decode_from_ring_borrowed<'a>(
    buf: &'a ReadBuf,
    oversized_buf: &'a mut Vec<u8>,
    id: u8,
    length: usize,
    total: usize,
) -> Result<(Message<&'a [u8]>, usize), torrent_wire::Error> {
    match id {
        // ── Fixed-field messages: zero allocation ──────────────
        MSG_CHOKE => Ok((Message::Choke, total)),
        MSG_UNCHOKE => Ok((Message::Unchoke, total)),
        MSG_INTERESTED => Ok((Message::Interested, total)),
        MSG_NOT_INTERESTED => Ok((Message::NotInterested, total)),
        MSG_HAVE => {
            ensure_msg_len(length, 5)?;
            let index = buf.peek_u32_be_at(5);
            Ok((Message::Have { index }, total))
        }
        MSG_REQUEST => {
            ensure_msg_len(length, 13)?;
            let index = buf.peek_u32_be_at(5);
            let begin = buf.peek_u32_be_at(9);
            let len = buf.peek_u32_be_at(13);
            Ok((
                Message::Request {
                    index,
                    begin,
                    length: len,
                },
                total,
            ))
        }
        MSG_CANCEL => {
            ensure_msg_len(length, 13)?;
            let index = buf.peek_u32_be_at(5);
            let begin = buf.peek_u32_be_at(9);
            let len = buf.peek_u32_be_at(13);
            Ok((
                Message::Cancel {
                    index,
                    begin,
                    length: len,
                },
                total,
            ))
        }
        MSG_PORT => {
            ensure_msg_len(length, 3)?;
            let port = buf.peek_u16_be_at(5);
            Ok((Message::Port(port), total))
        }
        MSG_SUGGEST_PIECE => {
            ensure_msg_len(length, 5)?;
            let index = buf.peek_u32_be_at(5);
            Ok((Message::SuggestPiece(index), total))
        }
        MSG_HAVE_ALL => Ok((Message::HaveAll, total)),
        MSG_HAVE_NONE => Ok((Message::HaveNone, total)),
        MSG_REJECT_REQUEST => {
            ensure_msg_len(length, 13)?;
            let index = buf.peek_u32_be_at(5);
            let begin = buf.peek_u32_be_at(9);
            let len = buf.peek_u32_be_at(13);
            Ok((
                Message::RejectRequest {
                    index,
                    begin,
                    length: len,
                },
                total,
            ))
        }
        MSG_ALLOWED_FAST => {
            ensure_msg_len(length, 5)?;
            let index = buf.peek_u32_be_at(5);
            Ok((Message::AllowedFast(index), total))
        }

        // ── Data-carrying messages: zero-copy from ring ──────────────
        MSG_PIECE => {
            ensure_msg_len(length, 9)?;
            let index = buf.peek_u32_be_at(5);
            let begin = buf.peek_u32_be_at(9);
            let data_len = length - 9;
            // Offset 13 = 4 (prefix) + 1 (id) + 4 (index) + 4 (begin)
            let (data_0, data_1) = buf.readable_slices_at(13, data_len);
            Ok((
                Message::Piece {
                    index,
                    begin,
                    data_0,
                    data_1,
                },
                total,
            ))
        }
        MSG_BITFIELD => {
            let data_len = length - 1;
            // Offset 5 = 4 (prefix) + 1 (id)
            let (s0, s1) = buf.readable_slices_at(5, data_len);
            if s1.is_empty() {
                // Contiguous — borrow directly.
                Ok((Message::Bitfield(s0), total))
            } else {
                // Wraps around ring boundary — copy into oversized_buf.
                oversized_buf.clear();
                oversized_buf.reserve(data_len);
                oversized_buf.extend_from_slice(s0);
                oversized_buf.extend_from_slice(s1);
                Ok((Message::Bitfield(oversized_buf.as_slice()), total))
            }
        }
        MSG_EXTENDED => {
            ensure_msg_len(length, 2)?;
            let ext_id = buf.peek_byte_at(5);
            let payload_len = length - 2;
            // Offset 6 = 4 (prefix) + 1 (id) + 1 (ext_id)
            let (s0, s1) = buf.readable_slices_at(6, payload_len);
            if s1.is_empty() {
                Ok((
                    Message::Extended {
                        ext_id,
                        payload: s0,
                    },
                    total,
                ))
            } else {
                oversized_buf.clear();
                oversized_buf.reserve(payload_len);
                oversized_buf.extend_from_slice(s0);
                oversized_buf.extend_from_slice(s1);
                Ok((
                    Message::Extended {
                        ext_id,
                        payload: oversized_buf.as_slice(),
                    },
                    total,
                ))
            }
        }

        // ── Unknown/rare messages: fallback to from_payload via oversized_buf ──
        _ => {
            oversized_buf.clear();
            oversized_buf.reserve(length);
            let (s0, s1) = buf.readable_slices_at(4, length);
            oversized_buf.extend_from_slice(s0);
            oversized_buf.extend_from_slice(s1);
            // Parse through the owned path (allocates Bytes), then convert back.
            // This path is extremely rare — unknown message IDs.
            let owned = Message::from_payload(Bytes::copy_from_slice(oversized_buf))?;
            // Re-express as borrowed from oversized_buf. Since the unknown
            // message was parsed as owned Bytes, we fall through to the
            // fixed-field reconstruction — data-carrying unknown variants
            // are impossible since all known data variants are handled above.
            match owned {
                Message::KeepAlive => Ok((Message::KeepAlive, total)),
                Message::Choke => Ok((Message::Choke, total)),
                Message::Unchoke => Ok((Message::Unchoke, total)),
                Message::Interested => Ok((Message::Interested, total)),
                Message::NotInterested => Ok((Message::NotInterested, total)),
                Message::Have { index } => Ok((Message::Have { index }, total)),
                Message::Request {
                    index,
                    begin,
                    length: len,
                } => Ok((
                    Message::Request {
                        index,
                        begin,
                        length: len,
                    },
                    total,
                )),
                Message::Cancel {
                    index,
                    begin,
                    length: len,
                } => Ok((
                    Message::Cancel {
                        index,
                        begin,
                        length: len,
                    },
                    total,
                )),
                Message::Port(port) => Ok((Message::Port(port), total)),
                Message::SuggestPiece(index) => Ok((Message::SuggestPiece(index), total)),
                Message::HaveAll => Ok((Message::HaveAll, total)),
                Message::HaveNone => Ok((Message::HaveNone, total)),
                Message::RejectRequest {
                    index,
                    begin,
                    length: len,
                } => Ok((
                    Message::RejectRequest {
                        index,
                        begin,
                        length: len,
                    },
                    total,
                )),
                Message::AllowedFast(index) => Ok((Message::AllowedFast(index), total)),
                Message::HashRequest {
                    pieces_root,
                    base,
                    index,
                    count,
                    proof_layers,
                } => Ok((
                    Message::HashRequest {
                        pieces_root,
                        base,
                        index,
                        count,
                        proof_layers,
                    },
                    total,
                )),
                Message::HashReject {
                    pieces_root,
                    base,
                    index,
                    count,
                    proof_layers,
                } => Ok((
                    Message::HashReject {
                        pieces_root,
                        base,
                        index,
                        count,
                        proof_layers,
                    },
                    total,
                )),
                Message::Hashes {
                    pieces_root,
                    base,
                    index,
                    count,
                    proof_layers,
                    hashes,
                } => Ok((
                    Message::Hashes {
                        pieces_root,
                        base,
                        index,
                        count,
                        proof_layers,
                        hashes,
                    },
                    total,
                )),
                // Data-carrying variants that should have been caught above:
                Message::Bitfield(_) | Message::Piece { .. } | Message::Extended { .. } => {
                    // This branch is unreachable: all known data-carrying
                    // message IDs are handled in the explicit arms above.
                    // If we somehow get here, re-read the payload from
                    // oversized_buf.
                    Ok((Message::Bitfield(oversized_buf.as_slice()), total))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PeerWriter — fixed write buffer
// ---------------------------------------------------------------------------

// Wire protocol message IDs for inline decoding (BEP 3 + BEP 6).
const MSG_CHOKE: u8 = 0;
const MSG_UNCHOKE: u8 = 1;
const MSG_INTERESTED: u8 = 2;
const MSG_NOT_INTERESTED: u8 = 3;
const MSG_HAVE: u8 = 4;
const MSG_BITFIELD: u8 = 5;
const MSG_REQUEST: u8 = 6;
const MSG_PIECE: u8 = 7;
const MSG_CANCEL: u8 = 8;
const MSG_PORT: u8 = 9;
const MSG_SUGGEST_PIECE: u8 = 0x0D;
const MSG_HAVE_ALL: u8 = 0x0E;
const MSG_HAVE_NONE: u8 = 0x0F;
const MSG_REJECT_REQUEST: u8 = 0x10;
const MSG_ALLOWED_FAST: u8 = 0x11;
const MSG_EXTENDED: u8 = 20;

/// Validate that a message payload has at least `expected` bytes.
#[inline]
fn ensure_msg_len(length: usize, expected: usize) -> Result<(), torrent_wire::Error> {
    if length < expected {
        Err(torrent_wire::Error::MessageTooShort {
            expected,
            got: length,
        })
    } else {
        Ok(())
    }
}

/// Maximum encoded message size: 4 (length) + 1 (id) + 8 (index+begin) +
/// 16384 (block data) = 16397 bytes.
const MAX_MSG_LEN: usize = 4 + 1 + 8 + 16_384;

/// Async writer that encodes and sends BitTorrent peer wire messages using a
/// pre-allocated buffer.
pub(crate) struct PeerWriter<W> {
    writer: W,
    buf: BytesMut,
}

impl<W: AsyncWrite + Unpin> PeerWriter<W> {
    /// Create a new writer wrapping `writer`.
    pub(crate) fn new(writer: W) -> Self {
        Self {
            writer,
            buf: BytesMut::with_capacity(MAX_MSG_LEN),
        }
    }

    /// Encode and send a message.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the write or flush fails.
    pub(crate) async fn send(&mut self, msg: &Message) -> std::io::Result<()> {
        self.buf.clear();
        msg.encode_into(&mut self.buf);
        self.writer.write_all(&self.buf).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Flush the underlying writer.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the flush fails.
    #[allow(dead_code)] // Retained for explicit flush use cases.
    pub(crate) async fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush().await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    // -----------------------------------------------------------------------
    // ReadBuf tests (6)
    // -----------------------------------------------------------------------

    #[test]
    fn readbuf_empty() {
        let rb = ReadBuf::new();
        assert_eq!(rb.len(), 0);
        let (a, b) = rb.readable_slices();
        assert!(a.is_empty());
        assert!(b.is_empty());
    }

    #[test]
    fn readbuf_readable_after_fill() {
        let mut rb = ReadBuf::new();
        let slice = rb.unfilled_contiguous();
        slice[0] = 0xDE;
        slice[1] = 0xAD;
        slice[2] = 0xBE;
        slice[3] = 0xEF;
        rb.mark_filled(4);

        assert_eq!(rb.len(), 4);
        let (a, b) = rb.readable_slices();
        assert_eq!(a, &[0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(b.is_empty());
    }

    #[test]
    fn readbuf_advance_wraps() {
        let mut rb = ReadBuf::new();

        // Fill the entire buffer.
        let n = BUF_LEN;
        let slice = rb.unfilled_contiguous();
        assert_eq!(slice.len(), n);
        for (i, byte) in slice.iter_mut().enumerate() {
            *byte = (i & 0xFF) as u8;
        }
        rb.mark_filled(n);
        assert_eq!(rb.len(), n);

        // Consume all but the last 4 bytes, then add 8 new bytes — the new
        // data should wrap around.
        rb.consume(n - 4);
        assert_eq!(rb.len(), 4);
        assert_eq!(rb.start, n - 4);

        // Write 8 bytes into unfilled region (should wrap).
        let slice = rb.unfilled_contiguous();
        // The contiguous unfilled region is [n-4+4..BUF_LEN) which is empty
        // because start + len == BUF_LEN. Actually: write_pos = (start+len) %
        // BUF_LEN = (n-4+4) % n = 0.
        // So unfilled_contiguous gives [0..start) = [0..n-4).
        assert!(slice.len() >= 8);
        for (i, byte) in slice[..8].iter_mut().enumerate() {
            *byte = (0xA0 + i) as u8;
        }
        rb.mark_filled(8);
        assert_eq!(rb.len(), 12);

        // Verify readable data: last 4 original bytes + 8 new bytes.
        let (a, b) = rb.readable_slices();
        // a = [start..BUF_LEN] = last 4 bytes of original fill
        assert_eq!(a.len(), 4);
        // b = [0..8] = the 8 new bytes
        assert_eq!(b.len(), 8);
        for (i, byte) in b.iter().enumerate() {
            assert_eq!(*byte, (0xA0 + i) as u8);
        }
    }

    #[test]
    fn readbuf_unfilled_ioslices_contiguous() {
        let mut rb = ReadBuf::new();
        // Write 100 bytes at the start.
        let slice = rb.unfilled_contiguous();
        for byte in slice[..100].iter_mut() {
            *byte = 0xFF;
        }
        rb.mark_filled(100);

        // Unfilled should be one contiguous region: [100..BUF_LEN).
        let (a, b) = rb.unfilled_ioslices();
        assert_eq!(a.len(), BUF_LEN - 100);
        assert!(b.is_empty());
    }

    #[test]
    fn readbuf_unfilled_ioslices_wrapped() {
        let mut rb = ReadBuf::new();

        // Fill everything then consume most — push start forward.
        let slice = rb.unfilled_contiguous();
        for byte in slice.iter_mut() {
            *byte = 0x00;
        }
        rb.mark_filled(BUF_LEN);
        rb.consume(BUF_LEN - 10);
        // Now start=BUF_LEN-10, len=10, so readable wraps? No, 10 bytes from
        // BUF_LEN-10..BUF_LEN.
        // But len==10 means consume reset didn't fire because len != 0.
        // start = BUF_LEN-10, len = 10, write_pos = (BUF_LEN-10+10)%BUF_LEN = 0.
        // write_pos(0) < start(BUF_LEN-10) → single contiguous unfilled [0..BUF_LEN-10).
        // We want a wrapped case. Let's consume down to 4, then add some at the wrap.
        rb.consume(6);
        // start = BUF_LEN-4, len = 4, write_pos = 0.
        // Unfilled: [0..BUF_LEN-4) — still contiguous from the unfilled perspective.
        // To get two unfilled regions we need write_pos >= start.
        // That happens when data is at the beginning: start=0, len=N, write_pos=N.
        // Actually, two unfilled regions means write_pos >= start AND start > 0.
        // Example: start=5, len=3, write_pos=8.
        //   unfilled: [8..BUF_LEN) and [0..5). Two regions.

        // Reset and set up properly.
        let mut rb2 = ReadBuf::new();
        // Manually set up: start=5, len=3.
        rb2.start = 5;
        rb2.len = 3;
        // write_pos = (5+3) % BUF_LEN = 8
        let (a, b) = rb2.unfilled_ioslices();
        // First region: [8..BUF_LEN)
        assert_eq!(a.len(), BUF_LEN - 8);
        // Second region: [0..5)
        assert_eq!(b.len(), 5);
        // Total unfilled = BUF_LEN - 3
        assert_eq!(a.len() + b.len(), BUF_LEN - 3);
    }

    #[test]
    fn readbuf_full_buffer() {
        let mut rb = ReadBuf::new();
        let slice = rb.unfilled_contiguous();
        assert_eq!(slice.len(), BUF_LEN);
        for byte in slice[..BUF_LEN - 1].iter_mut() {
            *byte = 0xAA;
        }
        rb.mark_filled(BUF_LEN - 1);

        // One byte of unfilled space remaining.
        let uf = rb.unfilled_contiguous();
        assert_eq!(uf.len(), 1);
        assert!(!rb.is_full());

        // Fill the last byte.
        rb.mark_filled(1);
        assert!(rb.is_full());
        assert_eq!(rb.unfilled_contiguous().len(), 0);
    }

    // -----------------------------------------------------------------------
    // DoubleBufHelper tests (5)
    // -----------------------------------------------------------------------

    #[test]
    fn helper_consume_contiguous() {
        let data = [1u8, 2, 3, 4, 5];
        let mut h = DoubleBufHelper::new(&data, &[]);
        let got = h.consume::<4>();
        assert_eq!(got, [1, 2, 3, 4]);
        assert_eq!(h.remaining(), 1);
    }

    #[test]
    fn helper_consume_across_boundary() {
        let a = [1u8, 2];
        let b = [3u8, 4, 5];
        let mut h = DoubleBufHelper::new(&a, &b);
        let got = h.consume::<4>();
        assert_eq!(got, [1, 2, 3, 4]);
        assert_eq!(h.remaining(), 1);
    }

    #[test]
    fn helper_read_u32_be_split() {
        // Value 0x01020304 split as [0x01, 0x02] and [0x03, 0x04].
        let a = [0x01u8, 0x02];
        let b = [0x03u8, 0x04];
        let mut h = DoubleBufHelper::new(&a, &b);
        let val = h.read_u32_be();
        assert_eq!(val, 0x0102_0304);
    }

    #[test]
    fn helper_consume_variable_wrapped() {
        let a = [10u8, 20, 30];
        let b = [40u8, 50, 60, 70];
        let mut h = DoubleBufHelper::new(&a, &b);
        // Skip 1 byte first.
        let _ = h.read_u8();
        // Now consume 5 bytes across the boundary.
        let bytes = h.consume_variable(5);
        assert_eq!(&bytes[..], &[20, 30, 40, 50, 60]);
        assert_eq!(h.remaining(), 1);
    }

    #[test]
    fn helper_remaining() {
        let a = [0u8; 10];
        let b = [0u8; 5];
        let mut h = DoubleBufHelper::new(&a, &b);
        assert_eq!(h.remaining(), 15);
        let _ = h.consume::<4>();
        assert_eq!(h.remaining(), 11);
        let _ = h.consume_variable(3);
        assert_eq!(h.remaining(), 8);
    }

    // -----------------------------------------------------------------------
    // PeerReader tests (8)
    // -----------------------------------------------------------------------

    /// Encode a message to its full wire representation (length prefix + payload).
    fn encode_message(msg: &Message) -> Vec<u8> {
        let mut buf = BytesMut::new();
        msg.encode_into(&mut buf);
        buf.to_vec()
    }

    #[tokio::test]
    async fn reader_decode_contiguous_message() {
        let msg = Message::Have { index: 42 };
        let wire = encode_message(&msg);

        let (client, mut server) = duplex(64 * 1024);
        tokio::spawn(async move {
            server.write_all(&wire).await.unwrap();
            server.shutdown().await.unwrap();
        });

        let mut reader = PeerReader::new(client, 1 << 20);
        let decoded = reader
            .next_message()
            .await
            .expect("decode error")
            .expect("expected message, got None");
        assert_eq!(decoded, msg);

        // Next call should return None (EOF).
        let eof = reader.next_message().await.expect("decode error");
        assert!(eof.is_none());
    }

    #[tokio::test]
    async fn reader_decode_wrapped_message() {
        // Pre-load the ReadBuf so the message data wraps around the ring
        // boundary. We'll place `start` near the end so the message straddles.
        let msg = Message::Have { index: 99 };
        let wire = encode_message(&msg); // 9 bytes: 4 len + 1 id + 4 index

        // Create a dummy duplex (won't actually read from it since data is
        // pre-loaded).
        let (client, _server) = duplex(1024);
        let mut reader = PeerReader::new(client, 1 << 20);

        // Place message at the wrap boundary.
        let start = BUF_LEN - 5; // 5 bytes before end, so 4 bytes wrap
        reader.buf.start = start;
        reader.buf.len = wire.len();
        for (i, &byte) in wire.iter().enumerate() {
            reader.buf.buf[(start + i) % BUF_LEN] = byte;
        }

        let decoded = reader
            .next_message()
            .await
            .expect("decode error")
            .expect("expected message");
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn reader_decode_wrapped_length_prefix() {
        // The 4-byte length prefix itself wraps at the ring boundary.
        let msg = Message::Interested; // 5 bytes on wire: len=1, id=2
        let wire = encode_message(&msg);

        let (client, _server) = duplex(1024);
        let mut reader = PeerReader::new(client, 1 << 20);

        // Place start so the 4-byte length prefix straddles the boundary:
        // 2 bytes before end, 2 bytes at start.
        let start = BUF_LEN - 2;
        reader.buf.start = start;
        reader.buf.len = wire.len();
        for (i, &byte) in wire.iter().enumerate() {
            reader.buf.buf[(start + i) % BUF_LEN] = byte;
        }

        let decoded = reader
            .next_message()
            .await
            .expect("decode error")
            .expect("expected message");
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn reader_decode_keepalive() {
        // KeepAlive = 4 zero bytes.
        let wire = [0u8; 4];
        let (client, mut server) = duplex(1024);
        tokio::spawn(async move {
            server.write_all(&wire).await.unwrap();
            server.shutdown().await.unwrap();
        });

        let mut reader = PeerReader::new(client, 1 << 20);
        let decoded = reader
            .next_message()
            .await
            .expect("decode error")
            .expect("expected KeepAlive");
        assert_eq!(decoded, Message::KeepAlive);

        // EOF
        let eof = reader.next_message().await.expect("decode error");
        assert!(eof.is_none());
    }

    #[tokio::test]
    async fn reader_decode_piece_data_integrity() {
        // Create a 16 KiB piece message with known data.
        let mut piece_data = vec![0u8; 16_384];
        for (i, byte) in piece_data.iter_mut().enumerate() {
            *byte = (i % 251) as u8; // prime mod for varied pattern
        }
        let msg = Message::Piece {
            index: 7,
            begin: 0,
            data_0: Bytes::from(piece_data.clone()),
            data_1: Bytes::new(),
        };
        let wire = encode_message(&msg);

        let (client, mut server) = duplex(256 * 1024);
        tokio::spawn(async move {
            server.write_all(&wire).await.unwrap();
            server.shutdown().await.unwrap();
        });

        let mut reader = PeerReader::new(client, 1 << 20);
        let decoded = reader
            .next_message()
            .await
            .expect("decode error")
            .expect("expected Piece");

        if let Message::Piece {
            index,
            begin,
            data_0,
            data_1,
        } = decoded
        {
            assert_eq!(index, 7);
            assert_eq!(begin, 0);
            assert_eq!(data_0.len() + data_1.len(), 16_384);
            assert_eq!(&data_0[..], &piece_data[..]);
        } else {
            panic!("expected Piece message, got {decoded:?}");
        }
    }

    #[tokio::test]
    async fn reader_reject_oversized() {
        // Craft a wire frame with a length prefix exceeding max_message_size.
        let bad_len: u32 = 500_000;
        let wire = bad_len.to_be_bytes();

        let (client, mut server) = duplex(1024);
        tokio::spawn(async move {
            server.write_all(&wire).await.unwrap();
            // Keep the connection open so the reader can process the length.
            let _ = tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });

        let max = 100_000;
        let mut reader = PeerReader::new(client, max);
        let err = reader
            .next_message()
            .await
            .expect_err("should reject oversized message");

        match err {
            torrent_wire::Error::MessageTooLarge { size, max: m } => {
                assert_eq!(size, bad_len as usize);
                assert_eq!(m, max);
            }
            other => panic!("expected MessageTooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reader_multiple_messages_sequence() {
        let messages = [
            Message::Choke,
            Message::Unchoke,
            Message::Have { index: 1 },
            Message::Interested,
        ];

        let mut wire = Vec::new();
        for msg in &messages {
            let mut buf = BytesMut::new();
            msg.encode_into(&mut buf);
            wire.extend_from_slice(&buf);
        }

        let (client, mut server) = duplex(64 * 1024);
        tokio::spawn(async move {
            server.write_all(&wire).await.unwrap();
            server.shutdown().await.unwrap();
        });

        let mut reader = PeerReader::new(client, 1 << 20);
        for expected in &messages {
            let decoded = reader
                .next_message()
                .await
                .expect("decode error")
                .expect("expected message");
            assert_eq!(&decoded, expected);
        }

        // EOF
        let eof = reader.next_message().await.expect("decode error");
        assert!(eof.is_none());
    }

    #[tokio::test]
    async fn reader_oversized_fallback() {
        // Create a message larger than BUF_LEN (32 KiB) but smaller than
        // max_message_size. A Bitfield message is the easiest: length = 1 + N.
        let bitfield_len = BUF_LEN + 1000; // > 32 KiB
        let bitfield_data = vec![0xAA; bitfield_len];
        let msg: Message = Message::Bitfield(Bytes::from(bitfield_data.clone()));
        let wire = encode_message(&msg);

        let max = BUF_LEN * 4; // plenty of room
        let (client, mut server) = duplex(256 * 1024);
        tokio::spawn(async move {
            server.write_all(&wire).await.unwrap();
            server.shutdown().await.unwrap();
        });

        let mut reader = PeerReader::new(client, max);
        let decoded = reader
            .next_message()
            .await
            .expect("decode error")
            .expect("expected Bitfield");

        if let Message::Bitfield(data) = decoded {
            assert_eq!(data.len(), bitfield_len);
            assert!(data.iter().all(|&b| b == 0xAA));
        } else {
            panic!("expected Bitfield, got {decoded:?}");
        }
    }

    // -----------------------------------------------------------------------
    // PeerWriter tests (2)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn writer_send_small_message() {
        let msg = Message::Have { index: 42 };
        let expected = encode_message(&msg);

        let (client, mut server) = duplex(64 * 1024);
        let mut writer = PeerWriter::new(client);
        writer.send(&msg).await.expect("send failed");
        drop(writer); // close the write half

        let mut received = Vec::new();
        server
            .read_to_end(&mut received)
            .await
            .expect("read failed");
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn writer_send_piece_message() {
        let mut piece_data = vec![0u8; 16_384];
        for (i, byte) in piece_data.iter_mut().enumerate() {
            *byte = (i % 199) as u8; // different prime
        }
        let msg = Message::Piece {
            index: 3,
            begin: 16_384,
            data_0: Bytes::from(piece_data.clone()),
            data_1: Bytes::new(),
        };
        let expected = encode_message(&msg);

        let (client, mut server) = duplex(256 * 1024);
        let mut writer = PeerWriter::new(client);
        writer.send(&msg).await.expect("send failed");
        drop(writer);

        let mut received = Vec::new();
        server
            .read_to_end(&mut received)
            .await
            .expect("read failed");
        assert_eq!(received, expected);

        // Verify the piece data is intact.
        let decoded =
            Message::from_payload(Bytes::from(received[4..].to_vec())).expect("decode failed");
        if let Message::Piece {
            index,
            begin,
            data_0,
            data_1,
        } = decoded
        {
            assert_eq!(index, 3);
            assert_eq!(begin, 16_384);
            let _ = &data_1; // data_1 is empty after wire round-trip
            assert_eq!(&data_0[..], &piece_data[..]);
        } else {
            panic!("expected Piece, got {decoded:?}");
        }
    }

    // -----------------------------------------------------------------------
    // Zero-copy (M110): fill_message + try_decode + advance tests (6)
    // -----------------------------------------------------------------------

    /// Helper: pre-load wire bytes into a PeerReader's ring buffer, optionally
    /// at a specific `start` position to exercise wrap-around.
    fn preload_reader(wire: &[u8], start: usize) -> PeerReader<tokio::io::DuplexStream> {
        let (client, _server) = duplex(1024);
        let mut reader = PeerReader::new(client, 1 << 20);
        reader.buf.start = start;
        reader.buf.len = wire.len();
        for (i, &byte) in wire.iter().enumerate() {
            reader.buf.buf[(start + i) % BUF_LEN] = byte;
        }
        reader
    }

    #[tokio::test]
    async fn reader_decode_piece_borrowed() {
        // Piece with 100 bytes of data, contiguous (start=0).
        let data_len = 100;
        let mut piece_data = vec![0u8; data_len];
        for (i, byte) in piece_data.iter_mut().enumerate() {
            *byte = (i % 197) as u8;
        }
        let msg = Message::Piece {
            index: 42,
            begin: 8192,
            data_0: Bytes::from(piece_data.clone()),
            data_1: Bytes::new(),
        };
        let wire = encode_message(&msg);
        let mut reader = preload_reader(&wire, 0);

        let status = reader.fill_message().await.expect("fill error");
        assert_eq!(status, FillStatus::Ready);

        let (decoded, consumed) = reader.try_decode().expect("decode error");
        assert_eq!(consumed, wire.len());

        if let Message::Piece {
            index,
            begin,
            data_0,
            data_1,
        } = decoded
        {
            assert_eq!(index, 42);
            assert_eq!(begin, 8192);
            // Contiguous: all data in data_0, data_1 empty.
            assert_eq!(data_0.len(), data_len);
            assert!(data_1.is_empty());
            assert_eq!(data_0, &piece_data[..]);
        } else {
            panic!("expected Piece, got {decoded:?}");
        }

        reader.advance(consumed);
        assert_eq!(reader.buf.len(), 0);
    }

    #[tokio::test]
    async fn reader_decode_piece_wrapped_borrowed() {
        // Place a Piece message so its data straddles the ring boundary.
        let data_len = 100;
        let mut piece_data = vec![0u8; data_len];
        for (i, byte) in piece_data.iter_mut().enumerate() {
            *byte = (i % 179) as u8;
        }
        let msg = Message::Piece {
            index: 10,
            begin: 0,
            data_0: Bytes::from(piece_data.clone()),
            data_1: Bytes::new(),
        };
        let wire = encode_message(&msg);

        // Position the message so the 13-byte header is at the end of the ring
        // and the data wraps around to the beginning. Header = 4+1+4+4 = 13.
        // If start = BUF_LEN - 50, then 50 bytes fit at the end and the rest
        // wraps. The header (13 bytes) plus 37 bytes of data are in the tail,
        // and the remaining 63 bytes wrap.
        let start = BUF_LEN - 50;
        let mut reader = preload_reader(&wire, start);

        let status = reader.fill_message().await.expect("fill error");
        assert_eq!(status, FillStatus::Ready);

        let (decoded, consumed) = reader.try_decode().expect("decode error");
        assert_eq!(consumed, wire.len());

        if let Message::Piece {
            index,
            begin,
            data_0,
            data_1,
        } = decoded
        {
            assert_eq!(index, 10);
            assert_eq!(begin, 0);
            // Data wraps: data_0 has the tail portion, data_1 has the wrapped portion.
            assert!(!data_1.is_empty(), "data should wrap");
            assert_eq!(data_0.len() + data_1.len(), data_len);
            // Verify concatenated content matches original.
            let mut combined = Vec::new();
            combined.extend_from_slice(data_0);
            combined.extend_from_slice(data_1);
            assert_eq!(combined, piece_data);
        } else {
            panic!("expected Piece, got {decoded:?}");
        }

        reader.advance(consumed);
        assert_eq!(reader.buf.len(), 0);
    }

    #[tokio::test]
    async fn reader_decode_bitfield_borrowed() {
        // Bitfield message, contiguous (no wrap).
        let bitfield_data = vec![0xFF; 64];
        let msg = Message::Bitfield(Bytes::from(bitfield_data.clone()));
        let wire = encode_message(&msg);
        let mut reader = preload_reader(&wire, 0);

        let status = reader.fill_message().await.expect("fill error");
        assert_eq!(status, FillStatus::Ready);

        let (decoded, consumed) = reader.try_decode().expect("decode error");
        assert_eq!(consumed, wire.len());

        if let Message::Bitfield(data) = decoded {
            assert_eq!(data, &bitfield_data[..]);
        } else {
            panic!("expected Bitfield, got {decoded:?}");
        }

        reader.advance(consumed);
        assert_eq!(reader.buf.len(), 0);
    }

    #[tokio::test]
    async fn reader_decode_extended_borrowed() {
        // Extended message, contiguous (no wrap).
        let payload_data = b"d1:pi12345ee"; // bencode-ish
        let msg = Message::Extended {
            ext_id: 1,
            payload: Bytes::from_static(payload_data),
        };
        let wire = encode_message(&msg);
        let mut reader = preload_reader(&wire, 0);

        let status = reader.fill_message().await.expect("fill error");
        assert_eq!(status, FillStatus::Ready);

        let (decoded, consumed) = reader.try_decode().expect("decode error");
        assert_eq!(consumed, wire.len());

        if let Message::Extended { ext_id, payload } = decoded {
            assert_eq!(ext_id, 1);
            assert_eq!(payload, payload_data);
        } else {
            panic!("expected Extended, got {decoded:?}");
        }

        reader.advance(consumed);
        assert_eq!(reader.buf.len(), 0);
    }

    #[tokio::test]
    async fn reader_bitfield_wrap_copies_to_oversized() {
        // Bitfield message placed so the bitfield data wraps around the ring
        // boundary. This should trigger the oversized_buf copy path.
        let bitfield_data = vec![0xAA; 200];
        let msg = Message::Bitfield(Bytes::from(bitfield_data.clone()));
        let wire = encode_message(&msg);

        // Place so the 5-byte header (4 len + 1 id) is at the tail and the
        // 200-byte bitfield wraps. Start at BUF_LEN - 100, so 100 bytes are
        // in the tail (5 header + 95 data) and 105 bytes wrap.
        let start = BUF_LEN - 100;
        let mut reader = preload_reader(&wire, start);

        let status = reader.fill_message().await.expect("fill error");
        assert_eq!(status, FillStatus::Ready);

        let (decoded, consumed) = reader.try_decode().expect("decode error");
        assert_eq!(consumed, wire.len());

        if let Message::Bitfield(data) = decoded {
            assert_eq!(data, &bitfield_data[..]);
            // The data should have been copied into oversized_buf because it wraps.
            assert_eq!(reader.oversized_buf.len(), bitfield_data.len());
        } else {
            panic!("expected Bitfield, got {decoded:?}");
        }

        reader.advance(consumed);
        assert_eq!(reader.buf.len(), 0);
    }

    #[tokio::test]
    async fn reader_advance_after_message() {
        // Verify that advance(n) correctly moves the ring cursor and frees
        // space for subsequent messages.
        let msg1 = Message::Have { index: 1 };
        let msg2 = Message::Have { index: 2 };
        let wire1 = encode_message(&msg1);
        let wire2 = encode_message(&msg2);

        let mut combined = Vec::new();
        combined.extend_from_slice(&wire1);
        combined.extend_from_slice(&wire2);

        let mut reader = preload_reader(&combined, 0);

        // Decode first message.
        let status = reader.fill_message().await.expect("fill error");
        assert_eq!(status, FillStatus::Ready);
        let (decoded1, consumed1) = reader.try_decode().expect("decode error");
        assert_eq!(decoded1, Message::Have::<&[u8]> { index: 1 });
        assert_eq!(consumed1, wire1.len());

        // Before advance: buf still holds both messages.
        assert_eq!(reader.buf.len(), combined.len());

        reader.advance(consumed1);

        // After advance: buf holds only the second message.
        assert_eq!(reader.buf.len(), wire2.len());

        // Decode second message.
        let status = reader.fill_message().await.expect("fill error");
        assert_eq!(status, FillStatus::Ready);
        let (decoded2, consumed2) = reader.try_decode().expect("decode error");
        assert_eq!(decoded2, Message::Have::<&[u8]> { index: 2 });
        assert_eq!(consumed2, wire2.len());

        reader.advance(consumed2);
        assert_eq!(reader.buf.len(), 0);
    }

    // -----------------------------------------------------------------------
    // ReadBuf::readable_slices_at tests (2)
    // -----------------------------------------------------------------------

    #[test]
    fn readbuf_readable_slices_at_contiguous() {
        let mut rb = ReadBuf::new();
        let slice = rb.unfilled_contiguous();
        for (i, byte) in slice[..20].iter_mut().enumerate() {
            *byte = i as u8;
        }
        rb.mark_filled(20);

        // Read 5 bytes starting at offset 3.
        let (a, b) = rb.readable_slices_at(3, 5);
        assert_eq!(a, &[3, 4, 5, 6, 7]);
        assert!(b.is_empty());
    }

    #[test]
    fn readbuf_readable_slices_at_wrapping() {
        let mut rb = ReadBuf::new();
        // Fill entire buffer, consume most, then add new data at wrap.
        let slice = rb.unfilled_contiguous();
        for (i, byte) in slice.iter_mut().enumerate() {
            *byte = (i & 0xFF) as u8;
        }
        rb.mark_filled(BUF_LEN);
        rb.consume(BUF_LEN - 4);
        // start = BUF_LEN-4, len = 4

        // Add 8 new bytes that wrap around.
        let slice = rb.unfilled_contiguous();
        for (i, byte) in slice[..8].iter_mut().enumerate() {
            *byte = (0xB0 + i) as u8;
        }
        rb.mark_filled(8);
        // Now: start=BUF_LEN-4, len=12
        // Bytes at positions:
        //   offset 0..4 -> original tail bytes
        //   offset 4..12 -> new 0xB0..0xB7

        // Read 6 bytes starting at offset 2 — this should cross the boundary.
        let (a, b) = rb.readable_slices_at(2, 6);
        // offset 2 from start = BUF_LEN-2, 6 bytes = BUF_LEN-2..BUF_LEN+4
        // a = [BUF_LEN-2..BUF_LEN] = 2 bytes (original tail)
        // b = [0..4] = 4 bytes (new data)
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 4);
        // b should be the first 4 of the new 0xB0 series.
        assert_eq!(b, &[0xB0, 0xB1, 0xB2, 0xB3]);
    }
}
