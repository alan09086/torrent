//! Async vectored-read abstraction for ring-buffer I/O.
//!
//! Provides [`AsyncReadVectored`], a trait that extends [`AsyncRead`] with
//! scatter-gather reads into multiple `IoSliceMut` buffers.  The companion
//! [`VectoredCompat`] wrapper gives every plain `AsyncRead` a correct (but
//! non-vectored) fallback implementation.

use std::io::IoSliceMut;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncRead;

/// Async vectored read — fills multiple non-contiguous buffers in one call.
pub(crate) trait AsyncReadVectored: AsyncRead + Unpin {
    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>>;
}

/// Async convenience method.
pub(crate) async fn read_vectored<R: AsyncReadVectored>(
    reader: &mut R,
    bufs: &mut [IoSliceMut<'_>],
) -> std::io::Result<usize> {
    std::future::poll_fn(|cx| Pin::new(&mut *reader).poll_read_vectored(cx, bufs)).await
}

/// Fallback wrapper: fills only the first non-empty `IoSliceMut` via `AsyncRead`.
pub(crate) struct VectoredCompat<T>(pub T);

impl<T: AsyncRead + Unpin> AsyncRead for VectoredCompat<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl<T: AsyncRead + Unpin> AsyncReadVectored for VectoredCompat<T> {
    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let first = match bufs.iter_mut().find(|s| !s.is_empty()) {
            Some(s) => &mut **s,
            None => return Poll::Ready(Ok(0)),
        };
        let mut rbuf = tokio::io::ReadBuf::new(first);
        std::task::ready!(Pin::new(&mut self.get_mut().0).poll_read(cx, &mut rbuf)?);
        Poll::Ready(Ok(rbuf.filled().len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn vectored_compat_fills_first_slice() {
        let (client, mut server) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            server.write_all(b"hello world").await.unwrap();
            server.shutdown().await.unwrap();
        });

        let mut reader = VectoredCompat(client);
        let mut buf1 = [0u8; 5];
        let mut buf2 = [0u8; 6];
        let mut iov = [IoSliceMut::new(&mut buf1), IoSliceMut::new(&mut buf2)];
        let n = read_vectored(&mut reader, &mut iov).await.unwrap();
        assert!(n > 0);
        assert!(n <= 5, "should only fill the first slice");
        assert_eq!(&buf1[..n], &b"hello"[..n]);
    }

    #[tokio::test]
    async fn vectored_compat_empty_slices() {
        let (client, _server) = tokio::io::duplex(1024);
        let mut reader = VectoredCompat(client);
        let mut iov: [IoSliceMut<'_>; 0] = [];
        let n = read_vectored(&mut reader, &mut iov).await.unwrap();
        assert_eq!(n, 0);
    }
}
