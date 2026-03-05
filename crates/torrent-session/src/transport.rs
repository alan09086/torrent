//! Network transport abstraction layer.
//!
//! Provides [`NetworkFactory`] — a factory for creating TCP listeners and
//! connections using either real tokio sockets (production) or pluggable
//! in-memory channels (testing/simulation).
//!
//! The key abstraction is [`TransportListener`], an object-safe trait for
//! accepting inbound connections, and [`BoxedStream`], a type-erased
//! async read/write stream.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Type aliases — tame clippy::type_complexity
// ---------------------------------------------------------------------------

/// Boxed future returned by [`TransportListener::accept`].
type AcceptFuture<'a> =
    Pin<Box<dyn Future<Output = io::Result<(BoxedStream, SocketAddr)>> + Send + 'a>>;

/// Closure type for [`NetworkFactory`]'s bind operation.
type BindFn = Box<
    dyn Fn(
            SocketAddr,
        ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn TransportListener>>> + Send>>
        + Send
        + Sync,
>;

/// Closure type for [`NetworkFactory`]'s connect operation.
type ConnectFn = Box<
    dyn Fn(SocketAddr) -> Pin<Box<dyn Future<Output = io::Result<BoxedStream>> + Send>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// BoxedStream
// ---------------------------------------------------------------------------

/// A type-erased bidirectional async stream.
///
/// Wraps any `AsyncRead + AsyncWrite + Unpin + Send` type behind a single
/// trait object. This avoids the Rust limitation that `dyn` can only name
/// one non-auto trait.
pub struct BoxedStream {
    inner: Pin<Box<dyn StreamRw + Send>>,
}

impl std::fmt::Debug for BoxedStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxedStream").finish_non_exhaustive()
    }
}

/// Combined read/write supertrait for dyn compatibility.
trait StreamRw: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> StreamRw for T {}

impl BoxedStream {
    /// Wrap any async read/write stream into a [`BoxedStream`].
    pub fn new<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(stream: S) -> Self {
        Self {
            inner: Box::pin(stream),
        }
    }
}

impl AsyncRead for BoxedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.inner.as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for BoxedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.as_mut().poll_shutdown(cx)
    }
}

impl Unpin for BoxedStream {}

// ---------------------------------------------------------------------------
// TransportListener
// ---------------------------------------------------------------------------

/// An object-safe listener that accepts inbound connections.
///
/// Implemented by [`TokioListener`] for real TCP sockets; simulation backends
/// provide their own implementation backed by in-memory channels.
///
/// The `accept` method returns a boxed future for dyn compatibility.
pub trait TransportListener: Send + Sync {
    /// Accept the next inbound connection.
    fn accept(&mut self) -> AcceptFuture<'_>;

    /// Return the local address this listener is bound to.
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

// ---------------------------------------------------------------------------
// TokioListener
// ---------------------------------------------------------------------------

/// A [`TransportListener`] backed by a real [`tokio::net::TcpListener`].
pub struct TokioListener(pub TcpListener);

impl TransportListener for TokioListener {
    fn accept(&mut self) -> AcceptFuture<'_> {
        Box::pin(async move {
            let (stream, addr) = self.0.accept().await?;
            Ok((BoxedStream::new(stream), addr))
        })
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

// ---------------------------------------------------------------------------
// NetworkFactory
// ---------------------------------------------------------------------------

/// Factory for creating TCP listeners and outbound connections.
///
/// In production, use [`NetworkFactory::tokio()`] to get a factory that
/// delegates to real tokio networking. For simulation/testing, construct
/// via [`NetworkFactory::new()`] with custom closures that route through
/// in-memory channels.
pub struct NetworkFactory {
    bind_tcp: BindFn,
    connect_tcp: ConnectFn,
    is_simulated: bool,
}

impl NetworkFactory {
    /// Create a factory with custom bind/connect closures.
    ///
    /// This is the primary constructor for simulation backends.
    pub fn new(bind_tcp: BindFn, connect_tcp: ConnectFn, is_simulated: bool) -> Self {
        Self {
            bind_tcp,
            connect_tcp,
            is_simulated,
        }
    }

    /// Create a factory that uses real tokio TCP networking.
    pub fn tokio() -> Self {
        Self {
            bind_tcp: Box::new(|addr| {
                Box::pin(async move {
                    let listener = TcpListener::bind(addr).await?;
                    Ok(Box::new(TokioListener(listener)) as Box<dyn TransportListener>)
                })
            }),
            connect_tcp: Box::new(|addr| {
                Box::pin(async move {
                    let stream = TcpStream::connect(addr).await?;
                    Ok(BoxedStream::new(stream))
                })
            }),
            is_simulated: false,
        }
    }

    /// Bind a TCP listener on the given address.
    pub async fn bind_tcp(&self, addr: SocketAddr) -> io::Result<Box<dyn TransportListener>> {
        (self.bind_tcp)(addr).await
    }

    /// Open an outbound TCP connection to the given address.
    pub async fn connect_tcp(&self, addr: SocketAddr) -> io::Result<BoxedStream> {
        (self.connect_tcp)(addr).await
    }

    /// Returns `true` if this factory uses simulated networking.
    pub fn is_simulated(&self) -> bool {
        self.is_simulated
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn tokio_factory_creation() {
        let _factory = NetworkFactory::tokio();
    }

    #[test]
    fn tokio_factory_is_not_simulated() {
        let factory = NetworkFactory::tokio();
        assert!(!factory.is_simulated());
    }

    #[tokio::test]
    async fn tokio_bind_and_accept() {
        let factory = NetworkFactory::tokio();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = factory.bind_tcp(addr).await.unwrap();
        let local = listener.local_addr().unwrap();
        assert_ne!(local.port(), 0);
    }

    #[tokio::test]
    async fn tokio_connect_to_listener() {
        let factory = NetworkFactory::tokio();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = factory.bind_tcp(addr).await.unwrap();
        let local = listener.local_addr().unwrap();

        let accept_handle = tokio::spawn(async move { listener.accept().await.unwrap() });

        let mut client = factory.connect_tcp(local).await.unwrap();
        client.write_all(b"hello").await.unwrap();

        let (mut server_stream, peer_addr) = accept_handle.await.unwrap();
        assert_eq!(
            peer_addr.ip(),
            "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
        );

        let mut buf = [0u8; 5];
        server_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn custom_factory_is_simulated() {
        let factory = NetworkFactory::new(
            Box::new(|_addr| {
                Box::pin(async move { Err(io::Error::new(io::ErrorKind::Unsupported, "stub")) })
            }),
            Box::new(|_addr| {
                Box::pin(async move { Err(io::Error::new(io::ErrorKind::Unsupported, "stub")) })
            }),
            true,
        );
        assert!(factory.is_simulated());
    }
}
