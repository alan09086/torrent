# M51: Network Simulation Framework — Implementation Plan

**Status:** Draft
**Milestone:** M51
**Version:** 0.42.0
**New Crate:** `ferrite-sim`
**Estimated Tests:** ~20 new tests

## Overview

Full in-process network simulator for swarm-level integration testing. This milestone introduces a `NetworkTransport` trait that abstracts all TCP/UDP networking at the session level, a `SimNetwork` with configurable latency/bandwidth/loss/NAT, a deterministic virtual clock, and a `SimSwarm` test harness for multi-peer scenarios.

This is the most complex milestone in the roadmap. The implementation is split into 8 tasks with strict ordering.

## Architecture Summary

```
                        NetworkTransport trait
                       /                      \
              TokioTransport                SimTransport
              (real sockets)              (in-memory channels)
                                                |
                                            SimNetwork
                                    (latency, bandwidth, loss,
                                     NAT, partitioning, SimClock)
                                                |
                                            SimSwarm
                                    (N sessions, test harness)
```

### Networking Touch Points in Current Code

The following locations create real network sockets and must be abstracted:

1. **`session.rs` — `SessionHandle::start_with_plugins()`**
   - Line ~226: `ferrite_utp::UtpSocket::bind()` — creates a real UDP socket for uTP
   - Line ~238: `ferrite_utp::UtpSocket::bind()` — IPv6 uTP socket
   - Line ~267: `DhtHandle::start()` — creates a real UDP socket for DHT
   - Line ~282: `DhtHandle::start()` — IPv6 DHT socket
   - Line ~206: `LsdHandle::start()` — creates a real UDP multicast socket for LSD
   - Line ~253: `NatHandle::start()` — creates real sockets for NAT traversal

2. **`torrent.rs` — `TorrentHandle::from_torrent()`**
   - Line ~159: `TcpListener::bind()` — binds a real TCP listener for incoming peers
   - Line ~315: `TcpListener::bind()` — same for `from_magnet()`

3. **`torrent.rs` — `TorrentActor::try_connect_peers()`**
   - Line ~3098: `socket.connect(addr)` — uTP outbound connection
   - Line ~3139: `TcpStream::connect(addr)` — TCP outbound connection
   - Line ~3137: `proxy::connect_through_proxy()` — proxied TCP

4. **`torrent.rs` — `accept_incoming()`**
   - Line ~3267: `listener.accept()` — accepts incoming TCP connections

5. **`dht/actor.rs` — `DhtActor`**
   - Line ~110: `UdpSocket::bind()` — DHT UDP socket
   - All `socket.send_to()` / `socket.recv_from()` calls in the actor loop

6. **`utp/socket.rs` — `SocketActor`**
   - Line ~56: `UdpSocket::bind()` — uTP UDP socket
   - All `socket.send_to()` / `socket.recv_from()` calls in the actor loop

### Design Principles

1. **Zero overhead in production** — `TokioTransport` is a thin wrapper over real sockets, all methods are `#[inline]`
2. **Trait object avoidance** — use generics (`T: NetworkTransport`) at construction time, concrete types stored in actors
3. **Minimal refactoring** — the transport trait is injected at `SessionHandle` construction; actors store a cloneable transport handle
4. **Deterministic testing** — `SimClock` replaces `tokio::time` in sim mode; tests advance time manually
5. **Test-only dependency** — `ferrite-sim` depends on `ferrite-session` (dev-dependency direction); production code only knows about the trait

---

## Task 1: `NetworkTransport` Trait Definition

**File:** `crates/ferrite-session/src/transport.rs` (new)

### 1.1 Define the Trait

The trait must be object-safe enough for our use, but we use generics to avoid dynamic dispatch overhead.

```rust
// crates/ferrite-session/src/transport.rs

//! Network transport abstraction for TCP/UDP socket operations.
//!
//! Production code uses `TokioTransport` (real sockets). The simulation
//! framework provides `SimTransport` (in-memory channels with configurable
//! network characteristics).

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncWrite};

/// A bidirectional byte stream (TCP-like connection).
///
/// This trait alias captures the bounds that `run_peer()` requires.
pub trait TransportStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> TransportStream for T {}

/// A listener that accepts incoming TCP-like connections.
#[async_trait::async_trait]
pub trait TransportListener: Send + 'static {
    type Stream: TransportStream;

    /// Accept the next incoming connection.
    async fn accept(&mut self) -> std::io::Result<(Self::Stream, SocketAddr)>;

    /// Returns the local address this listener is bound to.
    fn local_addr(&self) -> std::io::Result<SocketAddr>;
}

/// A UDP-like socket for sending and receiving datagrams.
#[async_trait::async_trait]
pub trait TransportUdpSocket: Send + Sync + 'static {
    /// Send a datagram to the specified address.
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize>;

    /// Receive a datagram, returning the number of bytes read and the source address.
    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)>;

    /// Returns the local address this socket is bound to.
    fn local_addr(&self) -> std::io::Result<SocketAddr>;
}

/// Network transport factory — creates TCP listeners, TCP connections, and UDP sockets.
///
/// Each `SessionHandle` owns one transport instance. The transport determines
/// whether networking goes through real OS sockets or the simulation framework.
#[async_trait::async_trait]
pub trait NetworkTransport: Clone + Send + Sync + 'static {
    type Stream: TransportStream;
    type Listener: TransportListener<Stream = Self::Stream>;
    type UdpSocket: TransportUdpSocket;

    /// Open a TCP-like connection to a remote address.
    async fn connect(&self, addr: SocketAddr) -> std::io::Result<Self::Stream>;

    /// Bind a TCP-like listener on the specified address.
    async fn listen(&self, addr: SocketAddr) -> std::io::Result<Self::Listener>;

    /// Bind a UDP-like socket on the specified address.
    async fn bind_udp(&self, addr: SocketAddr) -> std::io::Result<Self::UdpSocket>;
}
```

**Decision: `async_trait` vs manual futures.**
We use `async_trait` (already a transitive dependency via tokio ecosystem) for clarity. The trait methods are called infrequently (connection setup, not per-packet hot path), so the boxing overhead is negligible.

**Alternative considered:** Making `NetworkTransport` not a trait at all, but instead an enum (`Transport::Tokio` / `Transport::Sim`). Rejected because it would bloat production code with simulation variants and prevent the sim crate from being a dev-dependency.

### 1.2 `TokioTransport` Implementation

```rust
// crates/ferrite-session/src/transport.rs (continued)

/// Production transport using real tokio TCP/UDP sockets.
#[derive(Clone, Debug)]
pub struct TokioTransport;

/// Wrapper around `tokio::net::TcpListener` implementing `TransportListener`.
pub struct TokioListener {
    inner: tokio::net::TcpListener,
}

#[async_trait::async_trait]
impl TransportListener for TokioListener {
    type Stream = tokio::net::TcpStream;

    async fn accept(&mut self) -> std::io::Result<(tokio::net::TcpStream, SocketAddr)> {
        self.inner.accept().await
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// Wrapper around `tokio::net::UdpSocket` implementing `TransportUdpSocket`.
pub struct TokioUdpSocket {
    inner: tokio::net::UdpSocket,
}

#[async_trait::async_trait]
impl TransportUdpSocket for TokioUdpSocket {
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
        self.inner.send_to(buf, target).await
    }

    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.inner.recv_from(buf).await
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

#[async_trait::async_trait]
impl NetworkTransport for TokioTransport {
    type Stream = tokio::net::TcpStream;
    type Listener = TokioListener;
    type UdpSocket = TokioUdpSocket;

    async fn connect(&self, addr: SocketAddr) -> std::io::Result<tokio::net::TcpStream> {
        tokio::net::TcpStream::connect(addr).await
    }

    async fn listen(&self, addr: SocketAddr) -> std::io::Result<TokioListener> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        Ok(TokioListener { inner: listener })
    }

    async fn bind_udp(&self, addr: SocketAddr) -> std::io::Result<TokioUdpSocket> {
        let socket = tokio::net::UdpSocket::bind(addr).await?;
        Ok(TokioUdpSocket { inner: socket })
    }
}
```

### 1.3 Register Module

Add to `crates/ferrite-session/src/lib.rs`:

```rust
pub mod transport;
```

And add to `crates/ferrite-session/Cargo.toml`:

```toml
async-trait = "0.1"
```

### 1.4 Tests (4 tests)

```rust
// crates/ferrite-session/src/transport.rs — #[cfg(test)] mod tests

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tokio_transport_tcp_round_trip() {
        let transport = TokioTransport;
        let mut listener = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let listen_addr = listener.local_addr().unwrap();

        let connect_handle = tokio::spawn({
            let transport = transport.clone();
            async move {
                transport.connect(listen_addr).await.unwrap()
            }
        });

        let (mut server_stream, _peer_addr) = listener.accept().await.unwrap();
        let mut client_stream = connect_handle.await.unwrap();

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        client_stream.write_all(b"hello").await.unwrap();
        client_stream.flush().await.unwrap();

        let mut buf = [0u8; 5];
        server_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn tokio_transport_udp_round_trip() {
        let transport = TokioTransport;
        let sock_a = transport.bind_udp("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let sock_b = transport.bind_udp("127.0.0.1:0".parse().unwrap()).await.unwrap();

        let addr_b = sock_b.local_addr().unwrap();
        sock_a.send_to(b"ping", addr_b).await.unwrap();

        let mut buf = [0u8; 4];
        let (n, from) = sock_b.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
        assert_eq!(from, sock_a.local_addr().unwrap());
    }

    #[tokio::test]
    async fn tokio_transport_listener_local_addr() {
        let transport = TokioTransport;
        let listener = transport.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert_ne!(addr.port(), 0); // OS assigned a port
        assert_eq!(addr.ip(), std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn tokio_transport_is_clone_and_send() {
        fn assert_bounds<T: Clone + Send + Sync + 'static>() {}
        assert_bounds::<TokioTransport>();
    }
}
```

### TDD Order

1. Write `transport.rs` with trait definitions and `TokioTransport` impl
2. Write tests
3. Run `cargo test -p ferrite-session -- transport` — all 4 pass
4. Run `cargo clippy -p ferrite-session -- -D warnings` — clean

---

## Task 2: Refactor `SessionHandle` to Accept Transport

**Files modified:**
- `crates/ferrite-session/src/session.rs`
- `crates/ferrite-session/src/torrent.rs`
- `crates/ferrite-session/src/types.rs`

### 2.1 Session-Level Transport Injection

The key insight is that `SessionHandle` and `SessionActor` do not need to be generic over transport. Instead, we store the transport as a trait object at the session level and pass it down. However, to avoid dynamic dispatch overhead, we take a different approach: the `SessionHandle::start()` method creates a `TokioTransport` internally. A new `SessionHandle::start_with_transport()` method accepts any `NetworkTransport`.

Since `SessionActor` and `TorrentActor` are private internal types and never appear in public API, we can make them generic over `T: NetworkTransport` without breaking any public interfaces.

**However**, making the actors generic would cause significant code duplication and monomorphization bloat. Instead, we use **type-erased transport handles** stored as closures/boxed traits at the point of use.

**Final approach: Transport as boxed trait object.**

We define:

```rust
/// Type-erased transport handle stored in SessionActor and passed to TorrentActor.
///
/// Wraps a `NetworkTransport` impl into a cloneable, type-erased form so that
/// SessionActor/TorrentActor remain non-generic (avoiding monomorphization bloat).
#[derive(Clone)]
pub(crate) struct TransportHandle {
    inner: Arc<dyn ErasedTransport>,
}
```

Where `ErasedTransport` is an internal trait that erases the associated types:

```rust
#[async_trait::async_trait]
trait ErasedTransport: Send + Sync + 'static {
    async fn connect(&self, addr: SocketAddr) -> std::io::Result<Box<dyn TransportStream>>;
    async fn listen(&self, addr: SocketAddr) -> std::io::Result<Box<dyn ErasedListener>>;
    async fn bind_udp(&self, addr: SocketAddr) -> std::io::Result<Box<dyn TransportUdpSocket>>;
}
```

**Wait** — `TransportStream` is not object-safe (it requires `Sized` for `AsyncRead`/`AsyncWrite`). Let's reconsider.

**Revised approach: Box<dyn AsyncRead + AsyncWrite + Unpin + Send>**

We define a type alias:

```rust
/// A boxed bidirectional byte stream.
pub type BoxStream = Box<dyn TransportStream>;

/// A boxed listener.
pub type BoxListener = Box<dyn TransportListener<Stream = BoxStream> + Send>;
```

But `TransportListener` has an associated type, so boxing it requires `dyn TransportListener<Stream = BoxStream>`, which won't work because `accept()` returns `Self::Stream` and we need it to be `BoxStream`.

**Simplest correct approach:** Define a concrete `BoxedTransport` that wraps any `NetworkTransport` into a type that works with dynamic dispatch. Use `Pin<Box<dyn Future>>` for the async methods.

Actually, let's step back. The cleanest approach that minimizes refactoring is:

### Revised Strategy: Thin Injection Layer

Instead of making SessionActor/TorrentActor generic or using complex type erasure, we:

1. Add a `TransportHandle` to `SessionActor` that provides three factory closures:
   - `tcp_connect: Arc<dyn Fn(SocketAddr) -> Pin<Box<dyn Future<Output = io::Result<Pin<Box<dyn TransportStream>>>>>> + Send + Sync>`
   - `tcp_listen: Arc<dyn Fn(SocketAddr) -> Pin<Box<dyn Future<Output = io::Result<...>>>> + Send + Sync>`
   - `udp_bind: Arc<dyn Fn(SocketAddr) -> Pin<Box<dyn Future<Output = io::Result<...>>>> + Send + Sync>`

This is getting too complex. Let's use the simplest approach that works:

### Final Strategy: Concrete Enum Transport

```rust
// crates/ferrite-session/src/transport.rs

/// Concrete transport used by SessionActor and TorrentActor.
///
/// This enum avoids generics on actors while supporting both production
/// (real sockets) and simulation (in-memory) modes.
#[derive(Clone)]
pub enum Transport {
    /// Real tokio sockets (production).
    Tokio,
    /// Simulation transport (test-only, provided by ferrite-sim).
    Sim(SimTransportHandle),
}

/// Handle to the simulation transport layer.
///
/// Cloneable, cheaply copyable (Arc internally).
#[derive(Clone)]
pub struct SimTransportHandle {
    inner: Arc<dyn SimTransportProvider>,
}

/// Provider trait implemented by ferrite-sim's SimNetwork.
///
/// Object-safe — all methods return boxed futures and boxed streams.
#[async_trait::async_trait]
pub(crate) trait SimTransportProvider: Send + Sync + 'static {
    /// Open a simulated TCP connection.
    async fn connect(&self, from: SocketAddr, to: SocketAddr) -> std::io::Result<BoxedStream>;

    /// Bind a simulated TCP listener.
    async fn listen(&self, addr: SocketAddr) -> std::io::Result<BoxedListener>;

    /// Bind a simulated UDP socket.
    async fn bind_udp(&self, addr: SocketAddr) -> std::io::Result<BoxedUdpSocket>;
}
```

**This is still complex.** Let me take the approach that libtorrent-rasterbar and libp2p use:

### FINAL Strategy: `NetworkTransport` Trait with Box Erasure at Session Boundary

The trait stays clean with associated types. At the `SessionHandle` construction boundary, we erase into boxed closures. The actors use concrete function pointers.

**Actually, the absolute simplest approach:**

The `TorrentActor` already accepts `impl AsyncRead + AsyncWrite + Unpin + Send + 'static` in `run_peer()` and `spawn_peer_from_stream()`. The only places that create sockets are:

1. `TcpListener::bind()` — in `from_torrent()` / `from_magnet()`
2. `TcpStream::connect()` — in `try_connect_peers()`
3. `UdpSocket::bind()` — in DHT and uTP actors

For M51, the most pragmatic approach is:

**Inject factory functions into `TorrentActor` and `SessionActor`:**

```rust
/// Factory for creating TCP connections, listeners, and UDP sockets.
///
/// In production, these use real tokio sockets. In simulation, ferrite-sim
/// provides implementations that route through SimNetwork.
#[derive(Clone)]
pub struct NetworkFactory {
    /// Create a TCP connection to the given address.
    pub connect_tcp: Arc<dyn Fn(SocketAddr) -> BoxConnectFuture + Send + Sync>,
    /// Bind a TCP listener on the given address.
    pub bind_tcp: Arc<dyn Fn(SocketAddr) -> BoxListenFuture + Send + Sync>,
    /// Bind a UDP socket on the given address.
    pub bind_udp: Arc<dyn Fn(SocketAddr) -> BoxUdpBindFuture + Send + Sync>,
}
```

This avoids generics, avoids complex trait hierarchies, and the boxed future overhead only applies at connection setup (not per-packet). Let's go with this.

### 2.2 Implementation

**File:** `crates/ferrite-session/src/transport.rs`

```rust
//! Network transport abstraction for testable socket operations.
//!
//! `NetworkFactory` provides factory functions for creating TCP connections,
//! TCP listeners, and UDP sockets. In production, these delegate to real tokio
//! sockets. The simulation framework (`ferrite-sim`) provides alternative
//! implementations that route through an in-memory `SimNetwork`.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};

// ── Type Aliases ──────────────────────────────────────────────────────

/// A boxed bidirectional byte stream (TCP-like).
pub type BoxedStream = Pin<Box<dyn AsyncRead + AsyncWrite + Unpin + Send + 'static>>;

/// A boxed future that resolves to a stream.
pub type BoxConnectFuture =
    Pin<Box<dyn Future<Output = std::io::Result<BoxedStream>> + Send>>;

/// A boxed TCP listener.
pub type BoxedListener = Box<dyn TransportListener + Send>;

/// A boxed future that resolves to a listener.
pub type BoxListenFuture =
    Pin<Box<dyn Future<Output = std::io::Result<BoxedListener>> + Send>>;

/// A boxed UDP socket.
pub type BoxedUdpSocket = Box<dyn TransportUdpSocket + Send + Sync>;

/// A boxed future that resolves to a UDP socket.
pub type BoxUdpBindFuture =
    Pin<Box<dyn Future<Output = std::io::Result<BoxedUdpSocket>> + Send>>;

// ── Listener Trait ────────────────────────────────────────────────────

/// A TCP-like listener that accepts incoming connections.
#[async_trait::async_trait]
pub trait TransportListener: Send + 'static {
    /// Accept the next incoming connection.
    async fn accept(&mut self) -> std::io::Result<(BoxedStream, SocketAddr)>;

    /// Returns the local address this listener is bound to.
    fn local_addr(&self) -> std::io::Result<SocketAddr>;
}

/// Wrapper for `tokio::net::TcpListener`.
pub struct TokioListener {
    inner: tokio::net::TcpListener,
}

#[async_trait::async_trait]
impl TransportListener for TokioListener {
    async fn accept(&mut self) -> std::io::Result<(BoxedStream, SocketAddr)> {
        let (stream, addr) = self.inner.accept().await?;
        Ok((Box::pin(stream), addr))
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

// ── UDP Socket Trait ──────────────────────────────────────────────────

/// A UDP-like socket for datagrams.
#[async_trait::async_trait]
pub trait TransportUdpSocket: Send + Sync + 'static {
    /// Send a datagram to the given address.
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize>;

    /// Receive a datagram.
    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)>;

    /// Local address.
    fn local_addr(&self) -> std::io::Result<SocketAddr>;
}

/// Wrapper for `tokio::net::UdpSocket`.
pub struct TokioUdpSocket {
    inner: tokio::net::UdpSocket,
}

#[async_trait::async_trait]
impl TransportUdpSocket for TokioUdpSocket {
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
        self.inner.send_to(buf, target).await
    }

    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.inner.recv_from(buf).await
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

// ── Network Factory ───────────────────────────────────────────────────

/// Factory for creating network connections and sockets.
///
/// Stored in `SessionActor` and passed to `TorrentActor`. Cloneable and Send+Sync.
///
/// # Production
/// Use `NetworkFactory::tokio()` — delegates to real tokio sockets.
///
/// # Simulation
/// `ferrite-sim` constructs a custom factory that routes through `SimNetwork`.
#[derive(Clone)]
pub struct NetworkFactory {
    connect_tcp: Arc<dyn Fn(SocketAddr) -> BoxConnectFuture + Send + Sync>,
    bind_tcp: Arc<dyn Fn(SocketAddr) -> BoxListenFuture + Send + Sync>,
    bind_udp: Arc<dyn Fn(SocketAddr) -> BoxUdpBindFuture + Send + Sync>,
}

impl NetworkFactory {
    /// Production factory using real tokio sockets.
    pub fn tokio() -> Self {
        Self {
            connect_tcp: Arc::new(|addr| {
                Box::pin(async move {
                    let stream = tokio::net::TcpStream::connect(addr).await?;
                    Ok(Box::pin(stream) as BoxedStream)
                })
            }),
            bind_tcp: Arc::new(|addr| {
                Box::pin(async move {
                    let listener = tokio::net::TcpListener::bind(addr).await?;
                    Ok(Box::new(TokioListener { inner: listener }) as BoxedListener)
                })
            }),
            bind_udp: Arc::new(|addr| {
                Box::pin(async move {
                    let socket = tokio::net::UdpSocket::bind(addr).await?;
                    Ok(Box::new(TokioUdpSocket { inner: socket }) as BoxedUdpSocket)
                })
            }),
        }
    }

    /// Create a custom factory with user-provided closures.
    ///
    /// Used by `ferrite-sim` to inject simulated networking.
    pub fn custom(
        connect_tcp: impl Fn(SocketAddr) -> BoxConnectFuture + Send + Sync + 'static,
        bind_tcp: impl Fn(SocketAddr) -> BoxListenFuture + Send + Sync + 'static,
        bind_udp: impl Fn(SocketAddr) -> BoxUdpBindFuture + Send + Sync + 'static,
    ) -> Self {
        Self {
            connect_tcp: Arc::new(connect_tcp),
            bind_tcp: Arc::new(bind_tcp),
            bind_udp: Arc::new(bind_udp),
        }
    }

    /// Open a TCP-like connection to the given address.
    pub fn connect_tcp(&self, addr: SocketAddr) -> BoxConnectFuture {
        (self.connect_tcp)(addr)
    }

    /// Bind a TCP-like listener on the given address.
    pub fn bind_tcp(&self, addr: SocketAddr) -> BoxListenFuture {
        (self.bind_tcp)(addr)
    }

    /// Bind a UDP-like socket on the given address.
    pub fn bind_udp(&self, addr: SocketAddr) -> BoxUdpBindFuture {
        (self.bind_udp)(addr)
    }
}

impl std::fmt::Debug for NetworkFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkFactory").finish_non_exhaustive()
    }
}
```

### 2.3 Wire Transport into `SessionActor`

Modify `SessionHandle::start_with_plugins()` to accept a `NetworkFactory`:

```rust
// session.rs — add new constructor

impl SessionHandle {
    /// Start a new session with the given settings and no plugins.
    pub async fn start(settings: Settings) -> crate::Result<Self> {
        Self::start_full(settings, Arc::new(Vec::new()), NetworkFactory::tokio()).await
    }

    /// Start a new session with the given settings and extension plugins.
    pub async fn start_with_plugins(
        settings: Settings,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
    ) -> crate::Result<Self> {
        Self::start_full(settings, plugins, NetworkFactory::tokio()).await
    }

    /// Start a session with a custom network transport factory.
    ///
    /// Used by the simulation framework to inject in-memory networking.
    pub async fn start_with_transport(
        settings: Settings,
        transport: NetworkFactory,
    ) -> crate::Result<Self> {
        Self::start_full(settings, Arc::new(Vec::new()), transport).await
    }

    /// Full constructor (internal).
    async fn start_full(
        settings: Settings,
        plugins: Arc<Vec<Box<dyn crate::extension::ExtensionPlugin>>>,
        transport: NetworkFactory,
    ) -> crate::Result<Self> {
        // ... existing start_with_plugins body, but store `transport` in SessionActor
        // and pass to TorrentHandle constructors
    }
}
```

Add `transport: NetworkFactory` field to `SessionActor` struct. Pass it through to `TorrentHandle::from_torrent()` and `from_magnet()`.

### 2.4 Wire Transport into `TorrentActor`

Add `transport: NetworkFactory` field to `TorrentActor`. Use it in:

1. **`from_torrent()` / `from_magnet()`** — replace `TcpListener::bind()` with `transport.bind_tcp()`
2. **`try_connect_peers()`** — replace `TcpStream::connect()` with `transport.connect_tcp()`
3. **`accept_incoming()`** — use the `BoxedListener` stored from transport

The `accept_incoming()` function currently takes `&Option<TcpListener>`. Change it to take `&mut Option<BoxedListener>`:

```rust
async fn accept_incoming(
    listener: &mut Option<BoxedListener>,
) -> std::io::Result<(BoxedStream, SocketAddr)> {
    match listener {
        Some(l) => l.accept().await,
        None => std::future::pending().await,
    }
}
```

And the listener field in `TorrentActor` changes from `Option<TcpListener>` to `Option<BoxedListener>`.

### 2.5 Changes Summary

| File | Change |
|------|--------|
| `session.rs` | Add `transport: NetworkFactory` to `SessionActor`, add `start_with_transport()`, `start_full()` |
| `torrent.rs` | Add `transport: NetworkFactory` to `TorrentActor`, change `listener` type, update `try_connect_peers()`, `accept_incoming()` |
| `types.rs` | No changes needed |
| `transport.rs` | New file with all transport abstractions |
| `lib.rs` | Add `pub mod transport;` |
| `Cargo.toml` | Add `async-trait = "0.1"` |

### 2.6 Backward Compatibility

- `SessionHandle::start()` and `start_with_plugins()` are unchanged — they internally use `NetworkFactory::tokio()`
- All existing tests continue to work unchanged
- New public API: `SessionHandle::start_with_transport(settings, factory)`
- New public types: `NetworkFactory`, `BoxedStream`, `BoxedListener`, `BoxedUdpSocket`, `TransportListener`, `TransportUdpSocket`

### TDD Order

1. Write `transport.rs` with trait + `TokioTransport` + `NetworkFactory`
2. Add module to `lib.rs`, add `async-trait` dep
3. Run existing tests — all pass (no functional changes yet)
4. Modify `SessionActor` to store `NetworkFactory`
5. Modify `TorrentActor` to accept and use `NetworkFactory`
6. Replace `TcpListener::bind()` calls in `from_torrent()`/`from_magnet()`
7. Replace `TcpStream::connect()` in `try_connect_peers()`
8. Replace `accept_incoming()` signature
9. Run all existing tests — all pass (same behavior, different plumbing)

---

## Task 3: `ferrite-sim` Crate Scaffold

**New crate:** `crates/ferrite-sim/`

### 3.1 Create Crate

```
crates/ferrite-sim/
  Cargo.toml
  src/
    lib.rs
    network.rs      -- SimNetwork
    clock.rs        -- SimClock
    link.rs         -- LinkConfig, per-link simulation
    nat.rs          -- NAT simulation
    swarm.rs        -- SimSwarm test harness
    transport.rs    -- SimTransport (NetworkFactory impl)
```

### 3.2 Cargo.toml

```toml
[package]
name = "ferrite-sim"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Network simulation framework for ferrite integration tests"

[dependencies]
ferrite-session = { path = "../ferrite-session" }
ferrite-core = { path = "../ferrite-core" }
ferrite-storage = { path = "../ferrite-storage" }

tokio = { workspace = true }
tracing = { workspace = true }
async-trait = "0.1"
bytes = { workspace = true }

[dev-dependencies]
pretty_assertions = { workspace = true }
```

### 3.3 Add to Workspace

In root `Cargo.toml`, add `"crates/ferrite-sim"` to the `members` array.

### 3.4 `lib.rs`

```rust
//! Network simulation framework for ferrite integration testing.
//!
//! Provides an in-process simulated network (`SimNetwork`) with configurable
//! latency, bandwidth, packet loss, and NAT behaviour. Combined with `SimSwarm`,
//! enables deterministic swarm-level tests without real network traffic.

pub mod clock;
pub mod link;
pub mod nat;
pub mod network;
pub mod swarm;
pub mod transport;

pub use clock::SimClock;
pub use link::LinkConfig;
pub use nat::NatType;
pub use network::SimNetwork;
pub use swarm::SimSwarm;
pub use transport::sim_transport_factory;
```

---

## Task 4: `SimClock` — Deterministic Virtual Clock

**File:** `crates/ferrite-sim/src/clock.rs`

### 4.1 Design

`SimClock` is a shared, manually-advanced clock. It replaces `tokio::time::Instant` in simulation mode. The simulation advances time by calling `clock.advance(duration)`, which wakes up any pending timers.

```rust
//! Deterministic virtual clock for simulation tests.
//!
//! In simulation mode, time does not advance on its own — tests must call
//! `SimClock::advance()` to move time forward. All timers registered with
//! the clock fire immediately when their deadline is reached.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

/// A shared virtual clock for deterministic time control.
///
/// All `SimNetwork` operations and `SimSwarm` sessions reference this clock
/// for delay simulation and timeout handling.
#[derive(Clone)]
pub struct SimClock {
    inner: Arc<Mutex<ClockInner>>,
    notify: Arc<Notify>,
}

struct ClockInner {
    elapsed: Duration,
}

impl SimClock {
    /// Create a new clock starting at time zero.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ClockInner {
                elapsed: Duration::ZERO,
            })),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Current elapsed time since simulation start.
    pub fn now(&self) -> Duration {
        self.inner.lock().unwrap().elapsed
    }

    /// Advance the clock by the given duration.
    ///
    /// Wakes all pending sleep futures whose deadline has been reached.
    pub fn advance(&self, duration: Duration) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.elapsed += duration;
        }
        // Wake all pending sleepers so they can check their deadline
        self.notify.notify_waiters();
    }

    /// Sleep until the clock reaches `self.now() + duration`.
    ///
    /// Returns immediately if the clock is already past the deadline.
    pub async fn sleep(&self, duration: Duration) {
        let deadline = self.now() + duration;
        loop {
            if self.now() >= deadline {
                return;
            }
            self.notify.notified().await;
        }
    }

    /// Create an interval that ticks every `period`.
    pub fn interval(&self, period: Duration) -> SimInterval {
        SimInterval {
            clock: self.clone(),
            period,
            next_tick: self.now() + period,
        }
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new()
    }
}

/// A repeating interval timer driven by `SimClock`.
pub struct SimInterval {
    clock: SimClock,
    period: Duration,
    next_tick: Duration,
}

impl SimInterval {
    /// Wait for the next tick.
    pub async fn tick(&mut self) {
        loop {
            let now = self.clock.now();
            if now >= self.next_tick {
                self.next_tick = now + self.period;
                return;
            }
            self.clock.notify.notified().await;
        }
    }
}
```

### 4.2 Tests (2 tests)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clock_advance_and_sleep() {
        let clock = SimClock::new();
        assert_eq!(clock.now(), Duration::ZERO);

        let clock2 = clock.clone();
        let handle = tokio::spawn(async move {
            clock2.sleep(Duration::from_millis(100)).await;
            clock2.now()
        });

        // Sleep hasn't resolved yet
        tokio::task::yield_now().await;
        assert!(!handle.is_finished());

        // Advance past deadline
        clock.advance(Duration::from_millis(100));
        let woke_at = handle.await.unwrap();
        assert!(woke_at >= Duration::from_millis(100));
    }

    #[tokio::test]
    async fn interval_fires_on_advance() {
        let clock = SimClock::new();
        let mut interval = clock.interval(Duration::from_millis(50));

        // First tick should resolve immediately after advance
        let clock2 = clock.clone();
        let handle = tokio::spawn(async move {
            interval.tick().await;
            clock2.now()
        });

        clock.advance(Duration::from_millis(50));
        let t = handle.await.unwrap();
        assert!(t >= Duration::from_millis(50));
    }
}
```

---

## Task 5: `SimNetwork` — In-Memory Network Simulator

**Files:**
- `crates/ferrite-sim/src/link.rs`
- `crates/ferrite-sim/src/nat.rs`
- `crates/ferrite-sim/src/network.rs`

### 5.1 `LinkConfig` — Per-Link Network Parameters

```rust
// crates/ferrite-sim/src/link.rs

//! Per-link network configuration: latency, bandwidth, packet loss.

use std::time::Duration;

/// Configuration for a simulated network link between two nodes.
#[derive(Debug, Clone)]
pub struct LinkConfig {
    /// One-way latency (each direction).
    pub latency: Duration,
    /// Maximum bandwidth in bytes/second (0 = unlimited).
    pub bandwidth: u64,
    /// Packet loss rate (0.0 = no loss, 1.0 = all lost). Applies to UDP only.
    pub loss_rate: f64,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            latency: Duration::from_millis(20),
            bandwidth: 0, // unlimited
            loss_rate: 0.0,
        }
    }
}

impl LinkConfig {
    /// LAN-like link: 1ms latency, no loss, 100 MB/s bandwidth.
    pub fn lan() -> Self {
        Self {
            latency: Duration::from_millis(1),
            bandwidth: 100 * 1024 * 1024,
            loss_rate: 0.0,
        }
    }

    /// Broadband-like link: 20ms latency, no loss, 10 MB/s bandwidth.
    pub fn broadband() -> Self {
        Self {
            latency: Duration::from_millis(20),
            bandwidth: 10 * 1024 * 1024,
            loss_rate: 0.0,
        }
    }

    /// Lossy link: 50ms latency, 5% loss, 1 MB/s bandwidth.
    pub fn lossy() -> Self {
        Self {
            latency: Duration::from_millis(50),
            bandwidth: 1024 * 1024,
            loss_rate: 0.05,
        }
    }
}
```

### 5.2 `NatType` — NAT Simulation

```rust
// crates/ferrite-sim/src/nat.rs

//! NAT type simulation for realistic peer connectivity testing.

/// Simulated NAT behavior for a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatType {
    /// No NAT — node is directly reachable (public IP).
    None,
    /// Full cone NAT — any external host can send to the mapped port.
    FullCone,
    /// Port-restricted NAT — only hosts the node has sent to can reply.
    PortRestricted,
    /// Symmetric NAT — different mapping for each destination.
    Symmetric,
}

impl Default for NatType {
    fn default() -> Self {
        NatType::None
    }
}
```

### 5.3 `SimNetwork` — Core Simulator

```rust
// crates/ferrite-sim/src/network.rs

//! In-memory network simulator with configurable topology.
//!
//! `SimNetwork` manages virtual nodes, each with an assigned IP address.
//! TCP connections between nodes are simulated using in-memory duplex channels
//! with optional latency injection. UDP datagrams are routed through the
//! network with configurable loss.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::io::{DuplexStream};
use tokio::sync::mpsc;

use crate::clock::SimClock;
use crate::link::LinkConfig;
use crate::nat::NatType;

/// A simulated network.
///
/// Manages virtual nodes, TCP connection establishment, and UDP routing.
/// All operations are in-memory with optional latency/loss simulation.
#[derive(Clone)]
pub struct SimNetwork {
    inner: Arc<Mutex<NetworkInner>>,
    clock: SimClock,
}

struct NetworkInner {
    /// Registered nodes, keyed by their virtual IP address.
    nodes: HashMap<IpAddr, NodeState>,
    /// Default link config for node pairs without explicit config.
    default_link: LinkConfig,
    /// Per-link overrides: (min_ip, max_ip) -> LinkConfig.
    link_overrides: HashMap<(IpAddr, IpAddr), LinkConfig>,
    /// Partitioned groups: nodes in different groups cannot communicate.
    /// Empty = no partitioning.
    partitions: Vec<Vec<IpAddr>>,
    /// Next virtual IP to assign.
    next_ip: u32,
}

struct NodeState {
    nat: NatType,
    /// TCP listeners: port -> sender for incoming connections.
    tcp_listeners: HashMap<u16, mpsc::Sender<(DuplexStream, SocketAddr)>>,
    /// UDP sockets: port -> sender for incoming datagrams.
    udp_sockets: HashMap<u16, mpsc::Sender<(Vec<u8>, SocketAddr)>>,
    /// NAT mapping: (external seen by dest) -> internal port
    /// Only used for PortRestricted and Symmetric NATs.
    nat_mappings: HashMap<SocketAddr, u16>,
    /// Next ephemeral port.
    next_port: u16,
}

impl SimNetwork {
    /// Create a new simulated network with the given default link config.
    pub fn new(default_link: LinkConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(NetworkInner {
                nodes: HashMap::new(),
                default_link,
                link_overrides: HashMap::new(),
                partitions: Vec::new(),
                next_ip: 0x0A000001, // 10.0.0.1
            })),
            clock: SimClock::new(),
        }
    }

    /// Create with a custom clock.
    pub fn with_clock(default_link: LinkConfig, clock: SimClock) -> Self {
        Self {
            inner: Arc::new(Mutex::new(NetworkInner {
                nodes: HashMap::new(),
                default_link,
                link_overrides: HashMap::new(),
                partitions: Vec::new(),
                next_ip: 0x0A000001,
            })),
            clock,
        }
    }

    /// Access the shared clock.
    pub fn clock(&self) -> &SimClock {
        &self.clock
    }

    /// Register a new node and return its virtual IP address.
    pub fn add_node(&self) -> IpAddr {
        self.add_node_with_nat(NatType::None)
    }

    /// Register a node with a specific NAT type.
    pub fn add_node_with_nat(&self, nat: NatType) -> IpAddr {
        let mut inner = self.inner.lock().unwrap();
        let ip_u32 = inner.next_ip;
        inner.next_ip += 1;
        let ip = IpAddr::V4(Ipv4Addr::from(ip_u32));
        inner.nodes.insert(ip, NodeState {
            nat,
            tcp_listeners: HashMap::new(),
            udp_sockets: HashMap::new(),
            nat_mappings: HashMap::new(),
            next_port: 10000,
        });
        ip
    }

    /// Set link config between two specific nodes.
    pub fn set_link(&self, a: IpAddr, b: IpAddr, config: LinkConfig) {
        let key = if a < b { (a, b) } else { (b, a) };
        let mut inner = self.inner.lock().unwrap();
        inner.link_overrides.insert(key, config);
    }

    /// Partition the network into groups. Nodes in different groups cannot
    /// communicate. Pass empty vec to remove partitioning.
    pub fn partition(&self, groups: Vec<Vec<IpAddr>>) {
        let mut inner = self.inner.lock().unwrap();
        inner.partitions = groups;
    }

    /// Remove all partitions — restore full connectivity.
    pub fn heal_partition(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.partitions.clear();
    }

    /// Get the link config between two nodes.
    fn link_config(&self, a: IpAddr, b: IpAddr) -> LinkConfig {
        let inner = self.inner.lock().unwrap();
        let key = if a < b { (a, b) } else { (b, a) };
        inner.link_overrides.get(&key).cloned().unwrap_or_else(|| inner.default_link.clone())
    }

    /// Check if two nodes can communicate (not partitioned).
    fn can_communicate(&self, a: IpAddr, b: IpAddr) -> bool {
        let inner = self.inner.lock().unwrap();
        if inner.partitions.is_empty() {
            return true;
        }
        // Find which group each belongs to
        let group_a = inner.partitions.iter().position(|g| g.contains(&a));
        let group_b = inner.partitions.iter().position(|g| g.contains(&b));
        match (group_a, group_b) {
            (Some(ga), Some(gb)) => ga == gb,
            _ => true, // Nodes not in any partition group can communicate with everyone
        }
    }

    // --- TCP operations ---

    /// Register a TCP listener for a node on a specific port.
    /// Returns a receiver for incoming connections.
    pub(crate) fn register_tcp_listener(
        &self,
        ip: IpAddr,
        port: u16,
    ) -> mpsc::Receiver<(DuplexStream, SocketAddr)> {
        let (tx, rx) = mpsc::channel(64);
        let mut inner = self.inner.lock().unwrap();
        if let Some(node) = inner.nodes.get_mut(&ip) {
            node.tcp_listeners.insert(port, tx);
        }
        rx
    }

    /// Allocate an ephemeral port for a node.
    pub(crate) fn alloc_port(&self, ip: IpAddr) -> u16 {
        let mut inner = self.inner.lock().unwrap();
        if let Some(node) = inner.nodes.get_mut(&ip) {
            let port = node.next_port;
            node.next_port += 1;
            port
        } else {
            0
        }
    }

    /// Initiate a TCP connection from `from_ip` to `to_addr`.
    /// Returns a DuplexStream for the client side.
    pub(crate) async fn tcp_connect(
        &self,
        from_ip: IpAddr,
        to_addr: SocketAddr,
    ) -> std::io::Result<DuplexStream> {
        if !self.can_communicate(from_ip, to_addr.ip()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "network partition",
            ));
        }

        let link = self.link_config(from_ip, to_addr.ip());

        // Apply latency
        if link.latency > std::time::Duration::ZERO {
            self.clock.sleep(link.latency).await;
        }

        // Find the listener
        let listener_tx = {
            let inner = self.inner.lock().unwrap();
            inner
                .nodes
                .get(&to_addr.ip())
                .and_then(|n| n.tcp_listeners.get(&to_addr.port()))
                .cloned()
        };

        let listener_tx = listener_tx.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                format!("no listener at {}", to_addr),
            )
        })?;

        // Create duplex stream pair (64KB buffer simulates kernel buffer)
        let (client_stream, server_stream) = tokio::io::duplex(65536);

        let from_port = self.alloc_port(from_ip);
        let from_addr = SocketAddr::new(from_ip, from_port);

        listener_tx
            .send((server_stream, from_addr))
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "listener closed",
                )
            })?;

        Ok(client_stream)
    }

    // --- UDP operations ---

    /// Register a UDP socket for a node on a specific port.
    pub(crate) fn register_udp_socket(
        &self,
        ip: IpAddr,
        port: u16,
    ) -> mpsc::Receiver<(Vec<u8>, SocketAddr)> {
        let (tx, rx) = mpsc::channel(256);
        let mut inner = self.inner.lock().unwrap();
        if let Some(node) = inner.nodes.get_mut(&ip) {
            node.udp_sockets.insert(port, tx);
        }
        rx
    }

    /// Send a UDP datagram from `from_addr` to `to_addr`.
    pub(crate) async fn udp_send(
        &self,
        from_addr: SocketAddr,
        to_addr: SocketAddr,
        data: &[u8],
    ) -> std::io::Result<usize> {
        if !self.can_communicate(from_addr.ip(), to_addr.ip()) {
            // Silently drop (UDP doesn't error on unreachable)
            return Ok(data.len());
        }

        let link = self.link_config(from_addr.ip(), to_addr.ip());

        // Packet loss simulation
        if link.loss_rate > 0.0 {
            let mut rng_buf = [0u8; 8];
            ferrite_core::random_bytes(&mut rng_buf);
            let roll = u64::from_le_bytes(rng_buf) as f64 / u64::MAX as f64;
            if roll < link.loss_rate {
                // Packet lost
                return Ok(data.len());
            }
        }

        // Apply latency
        if link.latency > std::time::Duration::ZERO {
            self.clock.sleep(link.latency).await;
        }

        // Find the receiver
        let udp_tx = {
            let inner = self.inner.lock().unwrap();
            inner
                .nodes
                .get(&to_addr.ip())
                .and_then(|n| n.udp_sockets.get(&to_addr.port()))
                .cloned()
        };

        if let Some(tx) = udp_tx {
            let _ = tx.send((data.to_vec(), from_addr)).await;
        }
        // UDP doesn't error on no receiver
        Ok(data.len())
    }
}
```

### 5.4 Tests (3 tests)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tcp_connect_and_exchange() {
        let net = SimNetwork::new(LinkConfig::default());
        let ip_a = net.add_node();
        let ip_b = net.add_node();

        // B listens on port 6881
        let mut rx = net.register_tcp_listener(ip_b, 6881);
        let to_addr = SocketAddr::new(ip_b, 6881);

        let net2 = net.clone();
        let connect_handle = tokio::spawn(async move {
            net2.tcp_connect(ip_a, to_addr).await.unwrap()
        });

        // Accept on B
        let (mut server_stream, peer_addr) = rx.recv().await.unwrap();
        assert_eq!(peer_addr.ip(), ip_a);

        let mut client_stream = connect_handle.await.unwrap();

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        client_stream.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        server_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn partition_blocks_connection() {
        let net = SimNetwork::new(LinkConfig { latency: std::time::Duration::ZERO, ..Default::default() });
        let ip_a = net.add_node();
        let ip_b = net.add_node();
        let ip_c = net.add_node();

        // Partition: [A, B] and [C]
        net.partition(vec![vec![ip_a, ip_b], vec![ip_c]]);

        net.register_tcp_listener(ip_c, 6881);
        let result = net.tcp_connect(ip_a, SocketAddr::new(ip_c, 6881)).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::ConnectionRefused);

        // A can still reach B
        net.register_tcp_listener(ip_b, 6881);
        let result = net.tcp_connect(ip_a, SocketAddr::new(ip_b, 6881)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn udp_send_receive() {
        let net = SimNetwork::new(LinkConfig { latency: std::time::Duration::ZERO, ..Default::default() });
        let ip_a = net.add_node();
        let ip_b = net.add_node();

        let mut rx = net.register_udp_socket(ip_b, 6881);

        let from_addr = SocketAddr::new(ip_a, 12345);
        let to_addr = SocketAddr::new(ip_b, 6881);
        net.udp_send(from_addr, to_addr, b"ping").await.unwrap();

        let (data, from) = rx.recv().await.unwrap();
        assert_eq!(data, b"ping");
        assert_eq!(from, from_addr);
    }
}
```

---

## Task 6: `SimTransport` — NetworkFactory for Simulation

**File:** `crates/ferrite-sim/src/transport.rs`

This bridges `SimNetwork` to `NetworkFactory` from ferrite-session.

```rust
//! Simulation transport factory for ferrite-session.
//!
//! Creates a `NetworkFactory` that routes all TCP/UDP operations through
//! a `SimNetwork` instance.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use ferrite_session::transport::{
    BoxConnectFuture, BoxListenFuture, BoxUdpBindFuture,
    BoxedListener, BoxedStream, BoxedUdpSocket,
    NetworkFactory, TransportListener, TransportUdpSocket,
};

use crate::network::SimNetwork;

/// Create a `NetworkFactory` for a specific node in a `SimNetwork`.
///
/// The returned factory routes all TCP connections and UDP sockets through
/// the simulation network, appearing as traffic from the given `node_ip`.
pub fn sim_transport_factory(network: SimNetwork, node_ip: IpAddr) -> NetworkFactory {
    let net = network.clone();
    let ip = node_ip;

    let net_connect = net.clone();
    let net_listen = net.clone();
    let net_udp = net.clone();

    NetworkFactory::custom(
        // connect_tcp
        move |addr: SocketAddr| -> BoxConnectFuture {
            let net = net_connect.clone();
            Box::pin(async move {
                let stream = net.tcp_connect(ip, addr).await?;
                Ok(Box::pin(stream) as BoxedStream)
            })
        },
        // bind_tcp
        move |addr: SocketAddr| -> BoxListenFuture {
            let net = net_listen.clone();
            Box::pin(async move {
                let port = addr.port();
                let actual_port = if port == 0 {
                    net.alloc_port(ip)
                } else {
                    port
                };
                let rx = net.register_tcp_listener(ip, actual_port);
                let local_addr = SocketAddr::new(ip, actual_port);
                Ok(Box::new(SimListener { rx, local_addr }) as BoxedListener)
            })
        },
        // bind_udp
        move |addr: SocketAddr| -> BoxUdpBindFuture {
            let net = net_udp.clone();
            Box::pin(async move {
                let port = addr.port();
                let actual_port = if port == 0 {
                    net.alloc_port(ip)
                } else {
                    port
                };
                let rx = net.register_udp_socket(ip, actual_port);
                let local_addr = SocketAddr::new(ip, actual_port);
                Ok(Box::new(SimUdpSocket {
                    network: net,
                    local_addr,
                    rx: Arc::new(tokio::sync::Mutex::new(rx)),
                }) as BoxedUdpSocket)
            })
        },
    )
}

/// Simulated TCP listener.
struct SimListener {
    rx: mpsc::Receiver<(tokio::io::DuplexStream, SocketAddr)>,
    local_addr: SocketAddr,
}

#[async_trait::async_trait]
impl TransportListener for SimListener {
    async fn accept(&mut self) -> std::io::Result<(BoxedStream, SocketAddr)> {
        match self.rx.recv().await {
            Some((stream, addr)) => Ok((Box::pin(stream), addr)),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "listener closed",
            )),
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}

/// Simulated UDP socket.
struct SimUdpSocket {
    network: SimNetwork,
    local_addr: SocketAddr,
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<(Vec<u8>, SocketAddr)>>>,
}

#[async_trait::async_trait]
impl TransportUdpSocket for SimUdpSocket {
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
        self.network.udp_send(self.local_addr, target, buf).await
    }

    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        let mut rx = self.rx.lock().await;
        match rx.recv().await {
            Some((data, from)) => {
                let len = data.len().min(buf.len());
                buf[..len].copy_from_slice(&data[..len]);
                Ok((len, from))
            }
            None => Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "socket closed",
            )),
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.local_addr)
    }
}
```

### Tests (1 test)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::LinkConfig;

    #[tokio::test]
    async fn sim_factory_tcp_round_trip() {
        let net = SimNetwork::new(LinkConfig {
            latency: std::time::Duration::ZERO,
            ..Default::default()
        });
        let ip_a = net.add_node();
        let ip_b = net.add_node();

        let factory_a = sim_transport_factory(net.clone(), ip_a);
        let factory_b = sim_transport_factory(net.clone(), ip_b);

        // B listens
        let mut listener = factory_b.bind_tcp(SocketAddr::new(ip_b, 6881)).await.unwrap();
        let listen_addr = listener.local_addr().unwrap();

        // A connects
        let connect_handle = tokio::spawn(async move {
            factory_a.connect_tcp(listen_addr).await.unwrap()
        });

        let (mut server, _addr) = listener.accept().await.unwrap();
        let mut client = connect_handle.await.unwrap();

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        client.write_all(b"sim test").await.unwrap();
        let mut buf = [0u8; 8];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"sim test");
    }
}
```

---

## Task 7: `SimSwarm` — Test Harness

**File:** `crates/ferrite-sim/src/swarm.rs`

```rust
//! High-level test harness for multi-peer torrent simulations.
//!
//! `SimSwarm` spins up N `SessionHandle` instances connected through a
//! `SimNetwork`, seeds a torrent at one node, and provides methods to
//! run the simulation until completion or timeout.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ferrite_core::{
    torrent_from_bytes, sha1, Id20, Lengths, TorrentMetaV1, DEFAULT_CHUNK_SIZE,
};
use ferrite_session::{SessionHandle, Settings};
use ferrite_storage::{Bitfield, MemoryStorage};

use crate::link::LinkConfig;
use crate::network::SimNetwork;
use crate::transport::sim_transport_factory;

/// A running simulation swarm.
pub struct SimSwarm {
    network: SimNetwork,
    sessions: Vec<SessionHandle>,
    node_ips: Vec<IpAddr>,
}

/// Per-node statistics snapshot.
#[derive(Debug, Clone)]
pub struct NodeStats {
    pub ip: IpAddr,
    pub downloaded: u64,
    pub uploaded: u64,
    pub pieces_have: u32,
    pub pieces_total: u32,
    pub peers_connected: usize,
}

/// Builder for configuring and constructing a `SimSwarm`.
pub struct SimSwarmBuilder {
    peer_count: usize,
    link_config: LinkConfig,
    settings_override: Option<Box<dyn Fn(&mut Settings)>>,
}

impl SimSwarmBuilder {
    /// Create a builder for a swarm with the given number of peers.
    pub fn new(peer_count: usize) -> Self {
        Self {
            peer_count,
            link_config: LinkConfig::default(),
            settings_override: None,
        }
    }

    /// Set the default link configuration for all peer pairs.
    pub fn link_config(mut self, config: LinkConfig) -> Self {
        self.link_config = config;
        self
    }

    /// Apply custom settings modifications to each session.
    pub fn settings(mut self, f: impl Fn(&mut Settings) + 'static) -> Self {
        self.settings_override = Some(Box::new(f));
        self
    }

    /// Build the swarm, starting all sessions.
    pub async fn build(self) -> SimSwarm {
        let network = SimNetwork::new(self.link_config);
        let mut sessions = Vec::new();
        let mut node_ips = Vec::new();

        for i in 0..self.peer_count {
            let ip = network.add_node();
            node_ips.push(ip);

            let mut settings = Settings::default();
            // Disable features that require real network access
            settings.enable_dht = false;
            settings.enable_lsd = false;
            settings.enable_upnp = false;
            settings.enable_natpmp = false;
            settings.enable_utp = false;
            settings.listen_port = 6881;
            settings.download_dir = std::path::PathBuf::from(format!("/tmp/sim-node-{}", i));

            if let Some(ref f) = self.settings_override {
                f(&mut settings);
            }

            let transport = sim_transport_factory(network.clone(), ip);
            let session = SessionHandle::start_with_transport(settings, transport)
                .await
                .expect("session start failed");
            sessions.push(session);
        }

        SimSwarm {
            network,
            sessions,
            node_ips,
        }
    }
}

impl SimSwarm {
    /// Create a builder for a new swarm.
    pub fn builder(peer_count: usize) -> SimSwarmBuilder {
        SimSwarmBuilder::new(peer_count)
    }

    /// Access the underlying network for partition/link manipulation.
    pub fn network(&self) -> &SimNetwork {
        &self.network
    }

    /// Get the session handle for a specific node.
    pub fn session(&self, index: usize) -> &SessionHandle {
        &self.sessions[index]
    }

    /// Get the IP address of a specific node.
    pub fn node_ip(&self, index: usize) -> IpAddr {
        self.node_ips[index]
    }

    /// Number of nodes in the swarm.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether the swarm is empty.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Seed a torrent at the given node.
    ///
    /// Creates a test torrent from the given data, adds it to the seeder's
    /// session with pre-populated storage, and returns the torrent metadata
    /// for adding to leecher sessions.
    pub async fn seed(
        &self,
        data: &[u8],
        piece_length: u64,
        seeder_index: usize,
    ) -> (TorrentMetaV1, Id20) {
        let meta = make_test_torrent(data, piece_length);
        let info_hash = meta.info_hash;

        let lengths = Lengths::new(data.len() as u64, piece_length, DEFAULT_CHUNK_SIZE);
        let storage = Arc::new(MemoryStorage::new(lengths));
        // Pre-populate storage with data
        let mut offset = 0usize;
        while offset < data.len() {
            let end = (offset + DEFAULT_CHUNK_SIZE as usize).min(data.len());
            let chunk = bytes::Bytes::copy_from_slice(&data[offset..end]);
            let piece_idx = offset as u64 / piece_length;
            let begin = (offset as u64 % piece_length) as u32;
            storage.write_chunk(piece_idx as u32, begin, &chunk).unwrap();
            offset = end;
        }

        let tm: ferrite_core::TorrentMeta = meta.clone().into();
        self.sessions[seeder_index]
            .add_torrent(tm, Some(storage))
            .await
            .expect("seed failed");

        (meta, info_hash)
    }

    /// Add peers to a session's torrent.
    ///
    /// Tells the node at `node_index` about peers at the given node indices
    /// for the specified torrent.
    pub async fn introduce_peers(
        &self,
        node_index: usize,
        info_hash: Id20,
        peer_indices: &[usize],
    ) {
        let peers: Vec<SocketAddr> = peer_indices
            .iter()
            .map(|&i| SocketAddr::new(self.node_ips[i], 6881))
            .collect();

        // The session's torrent needs to know about these peers
        // This requires going through the torrent handle's add_peers
        // For now, use the session-level approach via TrackerManager or direct peer injection
        // We'll need to extend SessionHandle or use alerts
    }

    /// Collect per-node statistics for a torrent.
    pub async fn stats(&self, info_hash: Id20) -> Vec<Option<NodeStats>> {
        let mut results = Vec::new();
        for (i, session) in self.sessions.iter().enumerate() {
            match session.torrent_stats(info_hash).await {
                Ok(stats) => results.push(Some(NodeStats {
                    ip: self.node_ips[i],
                    downloaded: stats.downloaded,
                    uploaded: stats.uploaded,
                    pieces_have: stats.pieces_have,
                    pieces_total: stats.pieces_total,
                    peers_connected: stats.peers_connected,
                })),
                Err(_) => results.push(None),
            }
        }
        results
    }

    /// Run until all nodes have completed the torrent, or timeout.
    ///
    /// Returns `true` if all completed, `false` on timeout.
    pub async fn run_until_complete(
        &self,
        info_hash: Id20,
        timeout: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut interval = tokio::time::interval(Duration::from_millis(100));

        loop {
            if tokio::time::Instant::now() > deadline {
                return false;
            }

            interval.tick().await;

            let stats = self.stats(info_hash).await;
            let all_complete = stats.iter().all(|s| {
                s.as_ref()
                    .map(|ns| ns.pieces_have == ns.pieces_total && ns.pieces_total > 0)
                    .unwrap_or(false)
            });

            if all_complete {
                return true;
            }
        }
    }

    /// Shut down all sessions.
    pub async fn shutdown(&self) {
        for session in &self.sessions {
            let _ = session.shutdown().await;
        }
    }
}

/// Create a test torrent from raw data.
fn make_test_torrent(data: &[u8], piece_length: u64) -> TorrentMetaV1 {
    use serde::Serialize;

    let mut pieces = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let end = (offset + piece_length as usize).min(data.len());
        let hash = sha1(&data[offset..end]);
        pieces.extend_from_slice(hash.as_bytes());
        offset = end;
    }

    #[derive(Serialize)]
    struct Info<'a> {
        length: u64,
        name: &'a str,
        #[serde(rename = "piece length")]
        piece_length: u64,
        #[serde(with = "serde_bytes")]
        pieces: &'a [u8],
    }

    #[derive(Serialize)]
    struct Torrent<'a> {
        info: Info<'a>,
    }

    let t = Torrent {
        info: Info {
            length: data.len() as u64,
            name: "sim_test",
            piece_length,
            pieces: &pieces,
        },
    };

    let bytes = ferrite_bencode::to_bytes(&t).unwrap();
    torrent_from_bytes(&bytes).unwrap()
}
```

---

## Task 8: Integration Test Scenarios (~20 tests)

**File:** `crates/ferrite-sim/tests/swarm_tests.rs`

These tests verify the full simulation framework end-to-end. Each test creates a `SimSwarm`, seeds data, and verifies behavior.

### 8.1 Core Swarm Tests

```rust
//! Swarm-level integration tests using the simulation framework.

use std::time::Duration;

use ferrite_sim::{LinkConfig, NatType, SimNetwork, SimSwarm};

/// 1. Basic swarm: 1 seed + 3 leechers, verify all complete.
#[tokio::test]
async fn basic_swarm_1_seed_3_leechers() {
    let swarm = SimSwarm::builder(4)
        .link_config(LinkConfig::lan())
        .build()
        .await;

    let data = vec![0xAB; 65536]; // 64 KB
    let (meta, info_hash) = swarm.seed(&data, 16384, 0).await;

    // Add torrent to leechers
    for i in 1..4 {
        let tm: ferrite_core::TorrentMeta = meta.clone().into();
        swarm.session(i).add_torrent(tm, None).await.unwrap();
    }

    // Introduce peers (tell leechers about the seeder)
    // ... peer introduction via manual add_peers

    let completed = swarm.run_until_complete(info_hash, Duration::from_secs(30)).await;
    assert!(completed, "not all leechers completed");

    let stats = swarm.stats(info_hash).await;
    for (i, stat) in stats.iter().enumerate() {
        let s = stat.as_ref().unwrap();
        assert_eq!(s.pieces_have, s.pieces_total, "node {i} incomplete");
    }

    swarm.shutdown().await;
}

/// 2. Rarest-first verification: pieces distribute evenly across leechers.
#[tokio::test]
async fn rarest_first_distribution() {
    // ... similar setup, check piece distribution diversity
}

/// 3. Choking algorithm: verify tit-for-tat behaviour.
#[tokio::test]
async fn choking_tit_for_tat() {
    // ... verify faster uploaders get unchoked
}

/// 4. End-game mode: verify duplicate requests in final phase.
#[tokio::test]
async fn end_game_mode() {
    // ... small torrent, verify completion even with one slow peer
}

/// 5. Network partition: leecher in isolated partition cannot complete.
#[tokio::test]
async fn network_partition_prevents_download() {
    let swarm = SimSwarm::builder(3)
        .link_config(LinkConfig::lan())
        .build()
        .await;

    let data = vec![0xCD; 32768];
    let (meta, info_hash) = swarm.seed(&data, 16384, 0).await;

    // Add to leecher 1 (same partition as seeder)
    let tm1: ferrite_core::TorrentMeta = meta.clone().into();
    swarm.session(1).add_torrent(tm1, None).await.unwrap();

    // Add to leecher 2 (different partition)
    let tm2: ferrite_core::TorrentMeta = meta.clone().into();
    swarm.session(2).add_torrent(tm2, None).await.unwrap();

    // Partition: [0, 1] and [2]
    swarm.network().partition(vec![
        vec![swarm.node_ip(0), swarm.node_ip(1)],
        vec![swarm.node_ip(2)],
    ]);

    // Only node 1 should complete
    tokio::time::sleep(Duration::from_secs(5)).await;

    let stats = swarm.stats(info_hash).await;
    let node1 = stats[1].as_ref().unwrap();
    let node2 = stats[2].as_ref().unwrap();
    assert_eq!(node1.pieces_have, node1.pieces_total, "node 1 should complete");
    assert_eq!(node2.pieces_have, 0, "node 2 should have nothing (partitioned)");

    swarm.shutdown().await;
}

/// 6. Partition heal: download resumes after partition is removed.
#[tokio::test]
async fn partition_heal_resumes_download() {
    // ... partition, wait, heal, verify completion
}

/// 7. Smart banning: peer sending bad data gets banned.
#[tokio::test]
async fn smart_ban_bad_peer() {
    // ... inject fault: one node sends corrupted pieces
}

/// 8. Latency impact: higher latency = slower download.
#[tokio::test]
async fn latency_affects_throughput() {
    // ... compare completion time with 1ms vs 100ms latency
}

/// 9. Packet loss: UDP-based features degrade gracefully.
#[tokio::test]
async fn packet_loss_graceful_degradation() {
    // ... 10% loss, verify completion (just slower)
}

/// 10. Large swarm: 10 peers, 1 MB torrent.
#[tokio::test]
async fn large_swarm_10_peers() {
    // ... 1 seed + 9 leechers, 1 MB data, verify all complete
}

/// 11. Super seeding: initial seeder reveals pieces gradually.
#[tokio::test]
async fn super_seeding_mode() {
    // ... enable super_seeding on seeder, verify all pieces propagate
}

/// 12. Sequential download: pieces arrive in order.
#[tokio::test]
async fn sequential_download_ordering() {
    // ... enable sequential_download, verify piece arrival order
}

/// 13. Seed ratio limit: seeder stops after reaching ratio.
#[tokio::test]
async fn seed_ratio_limit_stops_upload() {
    // ... set ratio limit to 1.0, verify seeder pauses
}

/// 14. Multiple torrents: concurrent downloads.
#[tokio::test]
async fn concurrent_torrents() {
    // ... seed 2 different torrents, verify both complete
}

/// 15. Peer disconnection recovery: leecher reconnects after seeder restart.
#[tokio::test]
async fn peer_reconnection_after_disconnect() {
    // ... seeder goes away and comes back
}

/// 16. Empty torrent: zero-length torrent completes immediately.
#[tokio::test]
async fn zero_length_torrent() {
    // ... edge case
}

/// 17. Single-piece torrent: simplest possible transfer.
#[tokio::test]
async fn single_piece_torrent() {
    let swarm = SimSwarm::builder(2)
        .link_config(LinkConfig::lan())
        .build()
        .await;

    let data = vec![0xFF; 16384]; // exactly one piece
    let (meta, info_hash) = swarm.seed(&data, 16384, 0).await;

    let tm: ferrite_core::TorrentMeta = meta.into();
    swarm.session(1).add_torrent(tm, None).await.unwrap();

    let completed = swarm.run_until_complete(info_hash, Duration::from_secs(10)).await;
    assert!(completed);

    swarm.shutdown().await;
}

/// 18. Bandwidth cap: transfer rate respects configured limit.
#[tokio::test]
async fn bandwidth_cap_enforced() {
    // ... set 100 KB/s bandwidth, measure transfer time
}

/// 19. Many-piece torrent: hundreds of pieces.
#[tokio::test]
async fn many_pieces_torrent() {
    // ... 256 pieces of 16 KB each
}

/// 20. Peer churn: peers joining and leaving mid-transfer.
#[tokio::test]
async fn peer_churn_during_transfer() {
    // ... add/remove peers during download
}
```

---

## Implementation Order

| # | Task | Files | Tests | Depends On |
|---|------|-------|-------|------------|
| 1 | `NetworkTransport` trait + `TokioTransport` | `transport.rs` (new) | 4 | - |
| 2 | Refactor `SessionHandle`/`TorrentActor` to use `NetworkFactory` | `session.rs`, `torrent.rs` | 0 (existing pass) | 1 |
| 3 | `ferrite-sim` crate scaffold | New crate | 0 | 2 |
| 4 | `SimClock` | `clock.rs` | 2 | 3 |
| 5 | `SimNetwork` + `LinkConfig` + `NatType` | `network.rs`, `link.rs`, `nat.rs` | 3 | 4 |
| 6 | `SimTransport` (`NetworkFactory` for sim) | `transport.rs` | 1 | 5 |
| 7 | `SimSwarm` test harness | `swarm.rs` | 0 | 6 |
| 8 | Integration test scenarios | `tests/swarm_tests.rs` | ~20 | 7 |

**Critical path:** Tasks 1-2 (transport abstraction) are the riskiest — they touch the session and torrent actor internals. Tasks 3-7 are additive. Task 8 is pure testing.

## Risk Assessment

1. **Transport refactoring breaks existing tests** — Mitigated by keeping `SessionHandle::start()` signature unchanged; only internal plumbing changes.

2. **`BoxedStream` performance** — The boxing happens only at connection setup, not per-message. `run_peer()` already accepts `impl AsyncRead + AsyncWrite` generically. The `BoxedStream` satisfies this bound.

3. **uTP integration** — The uTP socket creates its own UDP socket internally. For simulation, we either:
   - Disable uTP in sim mode (simplest, used in initial implementation)
   - Provide a `UtpSocket::bind_with_udp()` that accepts a `TransportUdpSocket` (future enhancement)
   The initial implementation disables uTP in simulation (`settings.enable_utp = false`).

4. **DHT integration** — Same as uTP: DHT actor creates its own UDP socket. For M51, DHT is disabled in sim mode. A future milestone could make `DhtActor` accept a `TransportUdpSocket`.

5. **Deterministic clock** — The initial implementation uses `tokio::time` for the `run_until_complete()` polling loop. Full deterministic time (replacing all `tokio::time` inside session/torrent actors) is deferred — it would require pervasive changes to every `tokio::time::interval()`, `tokio::time::timeout()`, and `tokio::time::sleep()` call. The `SimClock` is available for future use.

## Dependencies Added

| Crate | Dependency | Why |
|-------|-----------|-----|
| `ferrite-session` | `async-trait = "0.1"` | Transport trait definitions |
| `ferrite-sim` | `ferrite-session`, `ferrite-core`, `ferrite-storage` | Core library access |
| `ferrite-sim` | `async-trait = "0.1"` | Trait impl for sim transport |
| `ferrite-sim` | `serde`, `serde_bytes` (workspace) | Test torrent creation |
| `ferrite-sim` | `ferrite-bencode` | Test torrent serialization |

## Version Bump

Workspace version: `0.41.0` -> `0.42.0` in root `Cargo.toml`.

## Commit Plan

```
feat: add NetworkTransport trait and TokioTransport impl (M51)
feat: refactor SessionHandle/TorrentActor to use NetworkFactory (M51)
feat: add ferrite-sim crate with SimClock (M51)
feat: add SimNetwork with per-link latency/loss/partitioning (M51)
feat: add SimTransport bridging SimNetwork to NetworkFactory (M51)
feat: add SimSwarm test harness (M51)
feat: add swarm integration test scenarios (M51)
```

## Facade Updates

After M51, add to `crates/ferrite/src/lib.rs`:

```rust
// ferrite-sim is a dev/test utility, NOT re-exported through the facade.
// Users import it directly: `use ferrite_sim::SimSwarm;`
```

No facade changes needed — `ferrite-sim` is a standalone test utility crate.

## CLAUDE.md Updates

Add to the Architecture section:
```
- **ferrite-sim**: Network simulation framework for swarm-level integration testing (test utility, not re-exported)
```

Update the crate count: "12-crate workspace" (was 11).

## Post-Implementation Verification

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

All 895 existing tests must continue to pass. Target: ~20 new tests in ferrite-sim, bringing total to ~915.
