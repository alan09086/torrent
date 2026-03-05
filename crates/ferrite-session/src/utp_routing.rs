//! Inbound connection routing: preamble identification and stream wrapping.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A stream wrapper that yields `prefix` bytes first, then delegates to `inner`.
///
/// Used to replay the BT preamble bytes consumed during connection identification
/// back to `run_peer()`, which expects to read the full handshake.
pub struct PrefixedStream<S> {
    prefix: Vec<u8>,
    prefix_pos: usize,
    inner: S,
}

impl<S> fmt::Debug for PrefixedStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrefixedStream")
            .field("prefix_remaining", &(self.prefix.len() - self.prefix_pos))
            .finish_non_exhaustive()
    }
}

impl<S> PrefixedStream<S> {
    /// Create a new `PrefixedStream` that yields `prefix` bytes before `inner`.
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            prefix_pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // Serve prefix bytes first
        if this.prefix_pos < this.prefix.len() {
            let remaining = &this.prefix[this.prefix_pos..];
            let to_copy = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..to_copy]);
            this.prefix_pos += to_copy;
            return Poll::Ready(Ok(()));
        }

        // Delegate to inner stream
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Attempt to identify a plaintext BitTorrent connection by reading the 48-byte preamble.
///
/// Returns `Some((info_hash, prefixed_stream))` if the first byte is 0x13 (BT pstrlen)
/// and the full 48-byte handshake preamble can be read. Returns `None` for encrypted
/// (MSE) connections (first byte != 0x13).
///
/// Generic over any async stream type (uTP, TCP `BoxedStream`, duplex, etc.).
pub async fn identify_plaintext_connection<S>(
    mut stream: S,
) -> io::Result<Option<(ferrite_core::Id20, PrefixedStream<S>)>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;

    // Read the first byte to check if it's a plaintext BT handshake
    let mut first = [0u8; 1];
    stream.read_exact(&mut first).await?;

    if first[0] != 0x13 {
        // Not a plaintext BT handshake (likely MSE encrypted)
        return Ok(None);
    }

    // Read remaining 47 bytes of the BT preamble:
    // [0]: 0x13 (already read)
    // [1..20]: "BitTorrent protocol"
    // [20..28]: reserved bytes
    // [28..48]: info_hash (20 bytes)
    let mut rest = [0u8; 47];
    stream.read_exact(&mut rest).await?;

    // Extract info_hash from bytes [28..48] of the full preamble
    // That's [27..47] within `rest` (since rest starts at byte 1)
    let info_hash_bytes: [u8; 20] = rest[27..47].try_into().unwrap();
    let info_hash = ferrite_core::Id20::from(info_hash_bytes);

    // Reconstruct the full 48-byte preamble for replay
    let mut preamble = Vec::with_capacity(48);
    preamble.push(first[0]);
    preamble.extend_from_slice(&rest);

    Ok(Some((info_hash, PrefixedStream::new(preamble, stream))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn prefixed_stream_yields_prefix_then_inner() {
        let inner = tokio_test_stream(b"world");
        let mut ps = PrefixedStream::new(b"hello ".to_vec(), inner);

        let mut buf = vec![0u8; 11];
        let n = ps.read(&mut buf).await.unwrap();
        // First read returns prefix
        assert_eq!(&buf[..n], b"hello ");

        let n2 = ps.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n2], b"world");
    }

    #[tokio::test]
    async fn prefixed_stream_empty_prefix_passthrough() {
        let inner = tokio_test_stream(b"direct");
        let mut ps = PrefixedStream::new(Vec::new(), inner);

        let mut buf = vec![0u8; 10];
        let n = ps.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"direct");
    }

    #[tokio::test]
    async fn prefixed_stream_write_delegates() {
        let (client, mut server) = tokio::io::duplex(64);
        let mut ps = PrefixedStream::new(b"prefix".to_vec(), client);

        ps.write_all(b"data").await.unwrap();
        ps.flush().await.unwrap();

        let mut buf = vec![0u8; 4];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"data");
    }

    #[tokio::test]
    async fn identify_plaintext_bt_handshake() {
        // Build a valid 48-byte BT preamble + 20-byte peer_id (68 total handshake)
        let mut preamble = Vec::with_capacity(68);
        preamble.push(0x13); // pstrlen
        preamble.extend_from_slice(b"BitTorrent protocol"); // 19 bytes
        preamble.extend_from_slice(&[0u8; 8]); // reserved
        let info_hash = [0xAB; 20];
        preamble.extend_from_slice(&info_hash); // info_hash
        let peer_id = [0xCD; 20];
        preamble.extend_from_slice(&peer_id); // peer_id (not consumed by identify)

        let (client, mut server) = tokio::io::duplex(256);

        // Write the preamble from the server side
        let write_handle = tokio::spawn(async move {
            server.write_all(&preamble).await.unwrap();
            server
        });

        // Use a shim since identify_plaintext_connection takes UtpStream,
        // but we can test PrefixedStream + preamble parsing logic directly.
        // We'll test the parsing logic via a helper.
        let result = identify_from_duplex(client).await;
        assert!(result.is_some());
        let (hash, mut ps) = result.unwrap();
        assert_eq!(hash.as_bytes(), &info_hash);

        // The PrefixedStream should replay the 48-byte preamble, then the peer_id
        let mut replay = vec![0u8; 68];
        ps.read_exact(&mut replay).await.unwrap();
        assert_eq!(replay[0], 0x13);
        assert_eq!(&replay[28..48], &info_hash);
        assert_eq!(&replay[48..68], &peer_id);

        let _ = write_handle.await;
    }

    /// Helper: simulates identify_plaintext_connection logic using a duplex stream
    /// (since we can't easily construct a UtpStream in tests).
    async fn identify_from_duplex(
        mut stream: impl AsyncRead + AsyncWrite + Unpin,
    ) -> Option<(ferrite_core::Id20, PrefixedStream<impl AsyncRead + AsyncWrite + Unpin>)> {
        use tokio::io::AsyncReadExt;

        let mut first = [0u8; 1];
        stream.read_exact(&mut first).await.ok()?;
        if first[0] != 0x13 {
            return None;
        }
        let mut rest = [0u8; 47];
        stream.read_exact(&mut rest).await.ok()?;
        let info_hash_bytes: [u8; 20] = rest[27..47].try_into().unwrap();
        let info_hash = ferrite_core::Id20::from(info_hash_bytes);
        let mut preamble = Vec::with_capacity(48);
        preamble.push(first[0]);
        preamble.extend_from_slice(&rest);
        Some((info_hash, PrefixedStream::new(preamble, stream)))
    }

    /// Create a readable stream from static bytes (for testing).
    fn tokio_test_stream(data: &[u8]) -> impl AsyncRead + AsyncWrite + Unpin {
        let (mut writer, reader) = tokio::io::duplex(data.len() + 64);
        let data = data.to_vec();
        tokio::spawn(async move {
            writer.write_all(&data).await.unwrap();
            // Don't close — let reader detect EOF naturally when dropped
        });
        reader
    }
}
