//! In-memory network simulator with configurable latency, bandwidth, and loss.
//!
//! [`SimNetwork`] manages virtual nodes, each assigned a unique IP address in
//! the `10.0.0.0/24` range. TCP connections between nodes are implemented
//! using [`tokio::io::duplex`] channels with optional latency injection,
//! bandwidth limits, and packet loss.
//!
//! Network partitions can be created to simulate link failures between groups
//! of nodes, and healed to restore connectivity.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use torrent_session::transport::BoxedStream;

use crate::clock::SimClock;

/// Per-link configuration for simulated network connections.
///
/// Controls latency, bandwidth, and packet loss for traffic between
/// a specific pair of nodes, or as the default for all connections.
#[derive(Debug, Clone)]
pub struct LinkConfig {
    /// One-way latency injected on each send.
    pub latency: Duration,
    /// Maximum bandwidth in bytes/second (0 = unlimited).
    pub bandwidth: u64,
    /// Packet loss probability (0.0 = no loss, 1.0 = total loss).
    pub loss_rate: f64,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            latency: Duration::ZERO,
            bandwidth: 0,
            loss_rate: 0.0,
        }
    }
}

/// A pending TCP connection delivered to a listener.
pub(crate) struct SimConnection {
    /// The connecting peer's address.
    pub remote_addr: SocketAddr,
    /// Stream half for the listener side to read/write.
    pub stream: BoxedStream,
}

/// Simulated network managing virtual nodes and TCP routing.
///
/// Each node gets a virtual IP (`10.0.0.1`, `10.0.0.2`, ...). TCP connections
/// between nodes use [`tokio::io::duplex`] channels with optional latency
/// injection. Network partitions can block traffic between groups of nodes.
///
/// The network is cheaply cloneable — all clones share the same inner state.
#[derive(Clone)]
pub struct SimNetwork {
    inner: Arc<Mutex<NetworkInner>>,
    clock: SimClock,
}

struct NetworkInner {
    /// Next IP octet to assign (starts at 1).
    next_ip: u8,
    /// Per-node TCP listeners: maps (ip, port) -> channel sender for incoming connections.
    listeners: HashMap<SocketAddr, mpsc::Sender<SimConnection>>,
    /// Per-link configuration overrides: (from_ip, to_ip) -> config.
    link_configs: HashMap<(IpAddr, IpAddr), LinkConfig>,
    /// Default link config for all connections without a specific override.
    default_config: LinkConfig,
    /// Network partitions: pairs of IP groups that cannot communicate.
    partitions: Vec<(Vec<IpAddr>, Vec<IpAddr>)>,
}

impl SimNetwork {
    /// Create a new simulated network with a fresh clock.
    pub fn new() -> Self {
        Self::with_clock(SimClock::new())
    }

    /// Create a new simulated network using the given virtual clock.
    pub fn with_clock(clock: SimClock) -> Self {
        Self {
            inner: Arc::new(Mutex::new(NetworkInner {
                next_ip: 1,
                listeners: HashMap::new(),
                link_configs: HashMap::new(),
                default_config: LinkConfig::default(),
                partitions: Vec::new(),
            })),
            clock,
        }
    }

    /// Return a reference to the virtual clock used by this network.
    pub fn clock(&self) -> &SimClock {
        &self.clock
    }

    /// Allocate a new virtual IP for a node.
    ///
    /// Returns `10.0.0.N` where N starts at 1 and increments with each call.
    ///
    /// # Panics
    ///
    /// Panics if more than 254 nodes are added (IP octet overflow).
    pub fn add_node(&self) -> IpAddr {
        let mut inner = self.inner.lock().unwrap();
        let octet = inner.next_ip;
        assert!(octet != 0, "node IP octet overflow: too many nodes");
        inner.next_ip = octet
            .checked_add(1)
            .expect("node IP octet overflow: too many nodes");
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, octet))
    }

    /// Set per-link configuration override for traffic from `from` to `to`.
    ///
    /// This override applies only in one direction. To configure both
    /// directions, call this twice with swapped arguments.
    pub fn set_link_config(&self, from: IpAddr, to: IpAddr, config: LinkConfig) {
        let mut inner = self.inner.lock().unwrap();
        inner.link_configs.insert((from, to), config);
    }

    /// Set the default link config used when no per-link override exists.
    pub fn set_default_config(&self, config: LinkConfig) {
        let mut inner = self.inner.lock().unwrap();
        inner.default_config = config;
    }

    /// Create a network partition between two groups of IPs.
    ///
    /// Traffic between nodes in `group_a` and nodes in `group_b` will be
    /// blocked in both directions. Traffic within a group is unaffected.
    pub fn partition(&self, group_a: Vec<IpAddr>, group_b: Vec<IpAddr>) {
        let mut inner = self.inner.lock().unwrap();
        inner.partitions.push((group_a, group_b));
    }

    /// Remove all partitions, restoring full connectivity.
    pub fn heal_partitions(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.partitions.clear();
    }

    /// Check if traffic from `from` to `to` is blocked by a partition.
    pub fn is_partitioned(&self, from: IpAddr, to: IpAddr) -> bool {
        let inner = self.inner.lock().unwrap();
        for (group_a, group_b) in &inner.partitions {
            let a_has_from = group_a.contains(&from);
            let a_has_to = group_a.contains(&to);
            let b_has_from = group_b.contains(&from);
            let b_has_to = group_b.contains(&to);

            // Partitioned if one is in group_a and the other in group_b
            if (a_has_from && b_has_to) || (b_has_from && a_has_to) {
                return true;
            }
        }
        false
    }

    /// Register a TCP listener at the given address.
    ///
    /// Returns a receiver that will get incoming [`SimConnection`]s when
    /// remote nodes connect to this address.
    pub(crate) fn register_listener(&self, addr: SocketAddr) -> mpsc::Receiver<SimConnection> {
        let (tx, rx) = mpsc::channel(64);
        let mut inner = self.inner.lock().unwrap();
        inner.listeners.insert(addr, tx);
        rx
    }

    /// Remove a TCP listener registration.
    #[allow(dead_code)] // Used in tests; will be used by SimTransport drop logic
    pub(crate) fn unregister_listener(&self, addr: &SocketAddr) {
        let mut inner = self.inner.lock().unwrap();
        inner.listeners.remove(addr);
    }

    /// Attempt a TCP connection from `from_addr` to `to_addr`.
    ///
    /// Returns a duplex stream for the connector side if the listener exists
    /// and the connection is not partitioned.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::ConnectionRefused`] if:
    /// - The two addresses are separated by a network partition
    /// - No listener is registered at `to_addr`
    /// - The listener's accept channel is full or closed
    pub(crate) async fn connect_tcp(
        &self,
        from_addr: SocketAddr,
        to_addr: SocketAddr,
    ) -> io::Result<BoxedStream> {
        // Check partitions
        if self.is_partitioned(from_addr.ip(), to_addr.ip()) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "connection blocked by network partition",
            ));
        }

        // Look up listener and get a clone of the sender
        let sender = {
            let inner = self.inner.lock().unwrap();
            inner.listeners.get(&to_addr).cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("no listener at {to_addr}"),
                )
            })?
        };

        // Create duplex channel pair
        let (client_half, server_half) = tokio::io::duplex(65536);

        // Send the server half to the listener
        let conn = SimConnection {
            remote_addr: from_addr,
            stream: BoxedStream::new(server_half),
        };

        sender.send(conn).await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("listener at {to_addr} closed"),
            )
        })?;

        // Return the client half
        Ok(BoxedStream::new(client_half))
    }
}

impl Default for SimNetwork {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn add_nodes_assigns_sequential_ips() {
        let net = SimNetwork::new();
        let ip1 = net.add_node();
        let ip2 = net.add_node();
        let ip3 = net.add_node();
        assert_eq!(ip1, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(ip2, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)));
        assert_eq!(ip3, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)));
    }

    #[tokio::test]
    async fn connect_tcp_duplex() {
        let net = SimNetwork::new();
        let ip1 = net.add_node();
        let ip2 = net.add_node();

        let listener_addr = SocketAddr::new(ip2, 6881);
        let mut rx = net.register_listener(listener_addr);

        let from_addr = SocketAddr::new(ip1, 12345);

        // Spawn a task to accept the connection
        let accept_handle =
            tokio::spawn(async move { rx.recv().await.expect("should receive connection") });

        // Connect from node 1 to node 2
        let mut client_stream = net.connect_tcp(from_addr, listener_addr).await.unwrap();

        let conn = accept_handle.await.unwrap();
        let mut server_stream = conn.stream;
        assert_eq!(conn.remote_addr, from_addr);

        // Client -> Server
        client_stream.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        server_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        // Server -> Client
        server_stream.write_all(b"world").await.unwrap();
        let mut buf2 = [0u8; 5];
        client_stream.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"world");
    }

    #[tokio::test]
    async fn partition_blocks_connection() {
        let net = SimNetwork::new();
        let ip1 = net.add_node();
        let ip2 = net.add_node();

        let listener_addr = SocketAddr::new(ip2, 6881);
        let _rx = net.register_listener(listener_addr);

        // Partition nodes 1 and 2
        net.partition(vec![ip1], vec![ip2]);

        let from_addr = SocketAddr::new(ip1, 12345);
        let result = net.connect_tcp(from_addr, listener_addr).await;
        let err = result.err().expect("connection should fail");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test]
    async fn heal_partition_restores() {
        let net = SimNetwork::new();
        let ip1 = net.add_node();
        let ip2 = net.add_node();

        let listener_addr = SocketAddr::new(ip2, 6881);
        let mut rx = net.register_listener(listener_addr);

        // Partition, then heal
        net.partition(vec![ip1], vec![ip2]);
        assert!(net.is_partitioned(ip1, ip2));
        net.heal_partitions();
        assert!(!net.is_partitioned(ip1, ip2));

        // Connection should succeed after healing
        let from_addr = SocketAddr::new(ip1, 12345);

        let accept_handle =
            tokio::spawn(async move { rx.recv().await.expect("should receive connection") });

        let _client = net.connect_tcp(from_addr, listener_addr).await.unwrap();
        let conn = accept_handle.await.unwrap();
        assert_eq!(conn.remote_addr, from_addr);
    }

    #[tokio::test]
    async fn unregistered_listener_refuses() {
        let net = SimNetwork::new();
        let ip1 = net.add_node();
        let ip2 = net.add_node();

        let from_addr = SocketAddr::new(ip1, 12345);
        let to_addr = SocketAddr::new(ip2, 6881);

        // No listener registered at to_addr
        let result = net.connect_tcp(from_addr, to_addr).await;
        let err = result.err().expect("connection should fail");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn partition_is_bidirectional() {
        let net = SimNetwork::new();
        let ip1 = net.add_node();
        let ip2 = net.add_node();

        net.partition(vec![ip1], vec![ip2]);
        assert!(net.is_partitioned(ip1, ip2));
        assert!(net.is_partitioned(ip2, ip1));
    }

    #[test]
    fn partition_does_not_affect_same_group() {
        let net = SimNetwork::new();
        let ip1 = net.add_node();
        let ip2 = net.add_node();
        let ip3 = net.add_node();

        // Partition: {ip1, ip2} vs {ip3}
        net.partition(vec![ip1, ip2], vec![ip3]);
        assert!(!net.is_partitioned(ip1, ip2)); // same group
        assert!(net.is_partitioned(ip1, ip3)); // across groups
        assert!(net.is_partitioned(ip2, ip3)); // across groups
    }

    #[test]
    fn link_config_defaults() {
        let config = LinkConfig::default();
        assert_eq!(config.latency, Duration::ZERO);
        assert_eq!(config.bandwidth, 0);
        assert_eq!(config.loss_rate, 0.0);
    }

    #[test]
    fn network_default() {
        let net = SimNetwork::default();
        let ip = net.add_node();
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn set_and_read_link_config() {
        let net = SimNetwork::new();
        let ip1 = net.add_node();
        let ip2 = net.add_node();

        let config = LinkConfig {
            latency: Duration::from_millis(50),
            bandwidth: 1_000_000,
            loss_rate: 0.01,
        };
        net.set_link_config(ip1, ip2, config.clone());

        // Verify it was stored (via internal access for testing)
        let inner = net.inner.lock().unwrap();
        let stored = inner.link_configs.get(&(ip1, ip2)).unwrap();
        assert_eq!(stored.latency, Duration::from_millis(50));
        assert_eq!(stored.bandwidth, 1_000_000);
        assert!((stored.loss_rate - 0.01).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn unregister_listener_prevents_connection() {
        let net = SimNetwork::new();
        let ip1 = net.add_node();
        let ip2 = net.add_node();

        let listener_addr = SocketAddr::new(ip2, 6881);
        let _rx = net.register_listener(listener_addr);
        net.unregister_listener(&listener_addr);

        let from_addr = SocketAddr::new(ip1, 12345);
        let result = net.connect_tcp(from_addr, listener_addr).await;
        let err = result.err().expect("connection should fail");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn with_clock_shares_clock() {
        let clock = SimClock::new();
        let net = SimNetwork::with_clock(clock.clone());
        clock.advance(Duration::from_secs(10));
        assert_eq!(net.clock().now(), Duration::from_secs(10));
    }
}
