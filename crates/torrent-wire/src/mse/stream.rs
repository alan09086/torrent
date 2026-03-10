//! Encrypted stream wrapper implementing AsyncRead + AsyncWrite.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::cipher::Rc4;

/// A stream that optionally encrypts/decrypts all data with RC4.
///
/// When ciphers are None, data passes through unmodified (plaintext mode).
pub struct MseStream<S> {
    inner: S,
    read_cipher: Option<Rc4>,
    write_cipher: Option<Rc4>,
    write_buf: Vec<u8>,
}

impl<S> MseStream<S> {
    /// Create a plaintext stream (no encryption).
    pub fn plaintext(inner: S) -> Self {
        MseStream {
            inner,
            read_cipher: None,
            write_cipher: None,
            write_buf: Vec::new(),
        }
    }

    /// Create an encrypted stream with RC4 ciphers.
    pub(crate) fn encrypted(inner: S, read_cipher: Rc4, write_cipher: Rc4) -> Self {
        MseStream {
            inner,
            read_cipher: Some(read_cipher),
            write_cipher: Some(write_cipher),
            write_buf: Vec::new(),
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for MseStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();

        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if let Some(cipher) = &mut this.read_cipher {
                    let filled = buf.filled_mut();
                    cipher.apply(&mut filled[before..]);
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for MseStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if let Some(cipher) = &mut this.write_cipher {
            this.write_buf.clear();
            this.write_buf.extend_from_slice(buf);
            cipher.apply(&mut this.write_buf);
            Pin::new(&mut this.inner).poll_write(cx, &this.write_buf)
        } else {
            Pin::new(&mut this.inner).poll_write(cx, buf)
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn plaintext_passthrough() {
        let (client, server) = tokio::io::duplex(1024);
        let mut client = MseStream::plaintext(client);
        let mut server = MseStream::plaintext(server);

        client.write_all(b"hello").await.unwrap();
        client.flush().await.unwrap();

        let mut buf = [0u8; 5];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn encrypted_roundtrip() {
        let key_a = b"key for direction A!";
        let key_b = b"key for direction B!";

        let (raw_client, raw_server) = tokio::io::duplex(1024);

        // Client: encrypt with A, decrypt with B
        let mut client = MseStream::encrypted(
            raw_client,
            Rc4::new(key_b), // read (decrypt) = B
            Rc4::new(key_a), // write (encrypt) = A
        );

        // Server: decrypt with A, encrypt with B
        let mut server = MseStream::encrypted(
            raw_server,
            Rc4::new(key_a), // read (decrypt) = A
            Rc4::new(key_b), // write (encrypt) = B
        );

        // Client -> Server
        client.write_all(b"client to server").await.unwrap();
        client.flush().await.unwrap();

        let mut buf = [0u8; 16];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"client to server");

        // Server -> Client
        server.write_all(b"server to client").await.unwrap();
        server.flush().await.unwrap();

        let mut buf = [0u8; 16];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"server to client");
    }
}
