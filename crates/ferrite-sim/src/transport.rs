//! SimTransport — NetworkFactory implementation for simulation.
//!
//! Provides [`sim_transport_factory`], which creates a [`NetworkFactory`] that
//! routes all TCP traffic through a [`SimNetwork`] instead of real sockets.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU16, Ordering};

use ferrite_session::transport::{BoxedStream, NetworkFactory, TransportListener};
use tokio::sync::mpsc;

use crate::network::{SimConnection, SimNetwork};

// ---------------------------------------------------------------------------
// Ephemeral port allocator
// ---------------------------------------------------------------------------

/// Global atomic counter for allocating unique ephemeral source ports.
static NEXT_EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(49152);

/// Allocate the next ephemeral port for outbound connections.
fn next_port() -> u16 {
    NEXT_EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// SimListener
// ---------------------------------------------------------------------------

/// A simulated TCP listener backed by an mpsc channel.
///
/// Receives [`SimConnection`]s from the [`SimNetwork`] when remote nodes
/// connect to this listener's address.
struct SimListener {
    /// The local address this listener is bound to.
    addr: SocketAddr,
    /// Receiver for incoming simulated connections.
    rx: mpsc::Receiver<SimConnection>,
}

impl TransportListener for SimListener {
    fn accept(
        &mut self,
    ) -> Pin<Box<dyn std::future::Future<Output = io::Result<(BoxedStream, SocketAddr)>> + Send + '_>>
    {
        Box::pin(async move {
            match self.rx.recv().await {
                Some(conn) => Ok((conn.stream, conn.remote_addr)),
                None => Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "listener channel closed",
                )),
            }
        })
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr)
    }
}

// ---------------------------------------------------------------------------
// Factory constructor
// ---------------------------------------------------------------------------

/// Create a [`NetworkFactory`] that routes all TCP through the given [`SimNetwork`].
///
/// Each node should have a unique virtual IP obtained from [`SimNetwork::add_node()`].
/// The returned factory:
/// - `bind_tcp(addr)` — registers a listener in the `SimNetwork`, returns a [`SimListener`]
/// - `connect_tcp(addr)` — routes through the `SimNetwork` to reach the listener at `addr`
/// - `is_simulated` — `true`
#[allow(clippy::type_complexity)]
pub fn sim_transport_factory(network: &SimNetwork, node_ip: IpAddr) -> NetworkFactory {
    let net_bind = network.clone();
    let node_ip_bind = node_ip;

    let net_connect = network.clone();
    let node_ip_connect = node_ip;

    let bind_fn = Box::new(move |addr: SocketAddr| {
        let net = net_bind.clone();
        // If the caller binds to 0.0.0.0 (unspecified), remap to the node's virtual IP.
        let effective_addr = if addr.ip().is_unspecified() {
            SocketAddr::new(node_ip_bind, addr.port())
        } else {
            addr
        };
        Box::pin(async move {
            let rx = net.register_listener(effective_addr);
            let listener = SimListener {
                addr: effective_addr,
                rx,
            };
            Ok(Box::new(listener) as Box<dyn TransportListener>)
        }) as Pin<Box<dyn std::future::Future<Output = io::Result<Box<dyn TransportListener>>> + Send>>
    });

    let connect_fn = Box::new(move |to_addr: SocketAddr| {
        let net = net_connect.clone();
        let from_addr = SocketAddr::new(node_ip_connect, next_port());
        Box::pin(async move { net.connect_tcp(from_addr, to_addr).await })
            as Pin<Box<dyn std::future::Future<Output = io::Result<BoxedStream>> + Send>>
    });

    NetworkFactory::new(bind_fn, connect_fn, true)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn sim_transport_factory_is_simulated() {
        let net = SimNetwork::new();
        let ip = net.add_node();
        let factory = sim_transport_factory(&net, ip);
        assert!(factory.is_simulated());
    }

    #[tokio::test]
    async fn sim_transport_bind_and_connect() {
        let net = SimNetwork::new();
        let ip1 = net.add_node();
        let ip2 = net.add_node();

        let factory1 = sim_transport_factory(&net, ip1);
        let factory2 = sim_transport_factory(&net, ip2);

        // Node 2 binds a listener
        let listen_addr = SocketAddr::new(ip2, 6881);
        let mut listener = factory2.bind_tcp(listen_addr).await.unwrap();
        let local = listener.local_addr().unwrap();
        assert_eq!(local, listen_addr);

        // Spawn accept task
        let accept_handle = tokio::spawn(async move { listener.accept().await.unwrap() });

        // Node 1 connects to node 2's listener
        let mut client = factory1.connect_tcp(listen_addr).await.unwrap();

        let (mut server_stream, peer_addr) = accept_handle.await.unwrap();
        assert_eq!(peer_addr.ip(), ip1);

        // Bidirectional data flow: client -> server
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        server_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        // server -> client
        server_stream.write_all(b"pong").await.unwrap();
        let mut buf2 = [0u8; 4];
        client.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"pong");
    }
}
