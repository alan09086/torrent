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
        debug_assert!(offset < self.len, "peek_byte_at: offset={offset} len={}", self.len);
        self.buf[(self.start + offset) % BUF_LEN]
    }

    /// Peek at a big-endian `u32` at `offset` from the read cursor without consuming.
    #[inline]
    pub(crate) fn peek_u32_be_at(&self, offset: usize) -> u32 {
        debug_assert!(offset + 4 <= self.len, "peek_u32_be_at: offset={offset} len={}", self.len);
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
        debug_assert!(offset + 2 <= self.len, "peek_u16_be_at: offset={offset} len={}", self.len);
        let s = self.start + offset;
        u16::from_be_bytes([
            self.buf[s % BUF_LEN],
            self.buf[(s + 1) % BUF_LEN],
        ])
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
pub(crate) struct PeerReader<R> {
    reader: R,
    buf: ReadBuf,
    max_message_size: usize,
}

impl<R: AsyncRead + Unpin> PeerReader<R> {
    /// Create a new reader wrapping `reader` with the given maximum message
    /// size.
    pub(crate) fn new(reader: R, max_message_size: usize) -> Self {
        Self {
            reader,
            buf: ReadBuf::new(),
            max_message_size,
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

    /// Decode the next peer wire message.
    ///
    /// Returns `Ok(None)` on clean EOF (no partial data buffered).
    ///
    /// # Errors
    ///
    /// - `torrent_wire::Error::MessageTooLarge` if the length prefix exceeds
    ///   `max_message_size`.
    /// - `torrent_wire::Error::Io` on socket errors or unexpected EOF with
    ///   a partial message buffered.
    pub(crate) async fn next_message(&mut self) -> Result<Option<Message>, torrent_wire::Error> {
        loop {
            // Step 1: need at least 4 bytes for the length prefix.
            if self.buf.len() >= 4 {
                let length = self.buf.peek_u32_be() as usize;

                // Step 2: validate against max.
                if length > self.max_message_size {
                    return Err(torrent_wire::Error::MessageTooLarge {
                        size: length,
                        max: self.max_message_size,
                    });
                }

                // Step 3: keep-alive.
                if length == 0 {
                    self.buf.consume(4);
                    return Ok(Some(Message::KeepAlive));
                }

                // Step 4: oversized message (> ring buffer capacity).
                if length > BUF_LEN {
                    return self.decode_oversized(length).await.map(Some);
                }

                // Step 5: we have the full message in the ring — decode inline.
                let total = 4 + length;
                if self.buf.len() >= total {
                    return self.decode_from_ring(length, total);
                }
            }

            // Step 6: need more data.
            let n = self.fill().await?;
            if n == 0 {
                // EOF
                if self.buf.len() == 0 {
                    return Ok(None);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "unexpected EOF with partial message buffered",
                )
                .into());
            }
        }
    }

    /// Decode a message whose payload exceeds the ring buffer capacity.
    ///
    /// This allocates a temporary `Vec<u8>` of `length` bytes, drains any
    /// buffered data first, then reads the remainder directly from the
    /// socket.
    #[allow(clippy::uninit_vec)]
    async fn decode_oversized(&mut self, length: usize) -> Result<Message, torrent_wire::Error> {
        // Consume the 4-byte length prefix from the ring.
        self.buf.consume(4);

        let mut vec = Vec::with_capacity(length);
        // SAFETY: consume_into and read_exact together write all `length` bytes
        // via copy_from_slice/read_exact, fully initializing the buffer.
        unsafe { vec.set_len(length) };
        let mut filled = 0;

        // Drain whatever is currently buffered.
        let buffered = self.buf.len().min(length);
        if buffered > 0 {
            self.buf.consume_into(&mut vec[..buffered]);
            filled = buffered;
        }

        // Read the rest directly from the socket.
        self.read_exact(&mut vec[filled..]).await?;

        Message::from_payload(Bytes::from(vec))
    }

    /// Decode a message whose data is fully present in the ring buffer.
    ///
    /// Simple messages (fixed fields only) are parsed inline — zero allocation.
    /// Data-carrying messages allocate via uninit `Vec` to skip zeroing.
    fn decode_from_ring(
        &mut self,
        length: usize,
        total: usize,
    ) -> Result<Option<Message>, torrent_wire::Error> {
        let id = self.buf.peek_byte_at(4);

        match id {
            // ── Fixed-field messages: zero allocation ──────────────
            MSG_CHOKE => {
                self.buf.consume(total);
                Ok(Some(Message::Choke))
            }
            MSG_UNCHOKE => {
                self.buf.consume(total);
                Ok(Some(Message::Unchoke))
            }
            MSG_INTERESTED => {
                self.buf.consume(total);
                Ok(Some(Message::Interested))
            }
            MSG_NOT_INTERESTED => {
                self.buf.consume(total);
                Ok(Some(Message::NotInterested))
            }
            MSG_HAVE => {
                ensure_msg_len(length, 5)?;
                let index = self.buf.peek_u32_be_at(5);
                self.buf.consume(total);
                Ok(Some(Message::Have { index }))
            }
            MSG_REQUEST => {
                ensure_msg_len(length, 13)?;
                let index = self.buf.peek_u32_be_at(5);
                let begin = self.buf.peek_u32_be_at(9);
                let len = self.buf.peek_u32_be_at(13);
                self.buf.consume(total);
                Ok(Some(Message::Request { index, begin, length: len }))
            }
            MSG_CANCEL => {
                ensure_msg_len(length, 13)?;
                let index = self.buf.peek_u32_be_at(5);
                let begin = self.buf.peek_u32_be_at(9);
                let len = self.buf.peek_u32_be_at(13);
                self.buf.consume(total);
                Ok(Some(Message::Cancel { index, begin, length: len }))
            }
            MSG_PORT => {
                ensure_msg_len(length, 3)?;
                let port = self.buf.peek_u16_be_at(5);
                self.buf.consume(total);
                Ok(Some(Message::Port(port)))
            }
            MSG_SUGGEST_PIECE => {
                ensure_msg_len(length, 5)?;
                let index = self.buf.peek_u32_be_at(5);
                self.buf.consume(total);
                Ok(Some(Message::SuggestPiece(index)))
            }
            MSG_HAVE_ALL => {
                self.buf.consume(total);
                Ok(Some(Message::HaveAll))
            }
            MSG_HAVE_NONE => {
                self.buf.consume(total);
                Ok(Some(Message::HaveNone))
            }
            MSG_REJECT_REQUEST => {
                ensure_msg_len(length, 13)?;
                let index = self.buf.peek_u32_be_at(5);
                let begin = self.buf.peek_u32_be_at(9);
                let len = self.buf.peek_u32_be_at(13);
                self.buf.consume(total);
                Ok(Some(Message::RejectRequest { index, begin, length: len }))
            }
            MSG_ALLOWED_FAST => {
                ensure_msg_len(length, 5)?;
                let index = self.buf.peek_u32_be_at(5);
                self.buf.consume(total);
                Ok(Some(Message::AllowedFast(index)))
            }

            // ── Data-carrying messages: parse header inline, alloc data only ──
            MSG_PIECE => {
                ensure_msg_len(length, 9)?;
                let index = self.buf.peek_u32_be_at(5);
                let begin = self.buf.peek_u32_be_at(9);
                self.buf.consume(13); // 4 prefix + 1 id + 4 index + 4 begin
                let data = self.buf.consume_as_bytes(length - 9);
                Ok(Some(Message::Piece { index, begin, data }))
            }
            MSG_BITFIELD => {
                self.buf.consume(5); // 4 prefix + 1 id
                let data = self.buf.consume_as_bytes(length - 1);
                Ok(Some(Message::Bitfield(data)))
            }
            MSG_EXTENDED => {
                ensure_msg_len(length, 2)?;
                let ext_id = self.buf.peek_byte_at(5);
                self.buf.consume(6); // 4 prefix + 1 id + 1 ext_id
                let payload = self.buf.consume_as_bytes(length - 2);
                Ok(Some(Message::Extended { ext_id, payload }))
            }

            // ── Unknown/rare messages: fallback to from_payload ──
            _ => {
                self.buf.consume(4);
                let payload = self.buf.consume_as_bytes(length);
                Message::from_payload(payload).map(Some)
            }
        }
    }

    /// Read exactly `buf.len()` bytes, draining from the ring first and then
    /// reading from the socket as needed.
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
            data: Bytes::from(piece_data.clone()),
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

        if let Message::Piece { index, begin, data } = decoded {
            assert_eq!(index, 7);
            assert_eq!(begin, 0);
            assert_eq!(data.len(), 16_384);
            assert_eq!(&data[..], &piece_data[..]);
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
        let msg = Message::Bitfield(Bytes::from(bitfield_data.clone()));
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
            data: Bytes::from(piece_data.clone()),
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
        if let Message::Piece { index, begin, data } = decoded {
            assert_eq!(index, 3);
            assert_eq!(begin, 16_384);
            assert_eq!(&data[..], &piece_data[..]);
        } else {
            panic!("expected Piece, got {decoded:?}");
        }
    }
}
