use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};

/// Request to write data through the socket actor.
pub(crate) struct WriteRequest {
    pub data: Bytes,
    pub reply: oneshot::Sender<io::Result<usize>>,
}

/// Signal to close the connection.
pub(crate) enum CloseSignal {
    Close,
}

/// uTP stream implementing AsyncRead + AsyncWrite.
///
/// Acts as a bridge between the application and the socket actor.
/// Data received from the peer arrives via `data_rx`, and writes
/// are forwarded to the actor via `write_tx`.
pub struct UtpStream {
    /// Incoming data from peer (delivered in-order by the actor).
    pub(crate) data_rx: mpsc::Receiver<Bytes>,
    /// Channel to send write requests to the actor.
    pub(crate) write_tx: mpsc::Sender<WriteRequest>,
    /// Channel to signal connection close.
    pub(crate) close_tx: Option<mpsc::Sender<CloseSignal>>,
    /// Buffered data from a previous recv that wasn't fully consumed.
    read_buf: BytesMut,
    /// Remote peer address.
    remote_addr: SocketAddr,
    /// Whether we've seen EOF (data_rx closed).
    eof: bool,
}

impl UtpStream {
    /// Create a new UtpStream from actor-provided channels.
    pub(crate) fn new(
        data_rx: mpsc::Receiver<Bytes>,
        write_tx: mpsc::Sender<WriteRequest>,
        close_tx: mpsc::Sender<CloseSignal>,
        remote_addr: SocketAddr,
    ) -> Self {
        Self {
            data_rx,
            write_tx,
            close_tx: Some(close_tx),
            read_buf: BytesMut::new(),
            remote_addr,
            eof: false,
        }
    }

    /// Returns the remote peer's socket address.
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }
}

impl AsyncRead for UtpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // Drain read_buf first
        if !this.read_buf.is_empty() {
            let n = this.read_buf.len().min(buf.remaining());
            buf.put_slice(&this.read_buf.split_to(n));
            return Poll::Ready(Ok(()));
        }

        if this.eof {
            return Poll::Ready(Ok(())); // EOF
        }

        // Poll for more data from actor
        match this.data_rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let n = data.len().min(buf.remaining());
                buf.put_slice(&data[..n]);
                if n < data.len() {
                    this.read_buf.extend_from_slice(&data[n..]);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => {
                this.eof = true;
                Poll::Ready(Ok(())) // EOF
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        let data = Bytes::copy_from_slice(buf);
        let (reply_tx, _reply_rx) = oneshot::channel();

        let req = WriteRequest {
            data,
            reply: reply_tx,
        };

        // Use try_send: if the channel has capacity, queue it immediately.
        // If full, return WouldBlock (caller retries).
        match this.write_tx.try_send(req) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(mpsc::error::TrySendError::Full(_)) => Poll::Pending,
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "connection closed",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // uTP doesn't have a separate flush — data is sent immediately
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(close_tx) = this.close_tx.take() {
            // Best-effort: if the channel is closed, the actor already shut down
            let _ = close_tx.try_send(CloseSignal::Close);
        }
        Poll::Ready(Ok(()))
    }
}

// Compile-time assertions: UtpStream must be Send + Unpin.
fn _assert_stream_traits() {
    fn _assert<T: Send + Unpin>() {}
    _assert::<UtpStream>();
}
