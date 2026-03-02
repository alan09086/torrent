//! SimSwarm — test harness for multi-peer simulation scenarios.
//!
//! [`SimSwarmBuilder`] provides a fluent API for configuring and launching a
//! simulated swarm of BitTorrent sessions that communicate through a shared
//! [`SimNetwork`] instead of real TCP sockets.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ferrite_core::{TorrentMetaV1, StorageMode};
use ferrite_session::{SessionHandle, Settings};
use ferrite_session::transport::NetworkFactory;

use crate::network::{LinkConfig, SimNetwork};
use crate::transport::sim_transport_factory;

// ---------------------------------------------------------------------------
// SimSwarmBuilder
// ---------------------------------------------------------------------------

/// Builder for configuring a simulated swarm.
///
/// Defaults: `piece_size = 16384`, no link config (zero-latency, unlimited bandwidth).
pub struct SimSwarmBuilder {
    num_peers: usize,
    piece_size: u64,
    link_config: Option<LinkConfig>,
}

impl SimSwarmBuilder {
    /// Create a new builder for a swarm with `num_peers` nodes.
    pub fn new(num_peers: usize) -> Self {
        Self {
            num_peers,
            piece_size: 16384,
            link_config: None,
        }
    }

    /// Set the piece size used for test torrents in this swarm.
    pub fn piece_size(mut self, size: u64) -> Self {
        self.piece_size = size;
        self
    }

    /// Set a default link config applied to all connections in the network.
    pub fn link_config(mut self, config: LinkConfig) -> Self {
        self.link_config = Some(config);
        self
    }

    /// Build and start the simulated swarm.
    ///
    /// Creates a [`SimNetwork`], allocates virtual IPs for each peer,
    /// and starts a [`SessionHandle`] per peer using simulated transport.
    pub async fn build(self) -> SimSwarm {
        let network = SimNetwork::new();

        if let Some(config) = self.link_config {
            network.set_default_config(config);
        }

        let mut node_ips = Vec::with_capacity(self.num_peers);
        let mut factories = Vec::with_capacity(self.num_peers);
        let mut sessions = Vec::with_capacity(self.num_peers);

        for _ in 0..self.num_peers {
            let ip = network.add_node();
            let factory = Arc::new(sim_transport_factory(&network, ip));

            let settings = Settings {
                listen_port: 6881,
                download_dir: PathBuf::from("/tmp/ferrite-sim"),
                max_torrents: 10,
                enable_dht: false,
                enable_pex: false,
                enable_lsd: false,
                enable_fast_extension: true,
                enable_utp: false,
                enable_upnp: false,
                enable_natpmp: false,
                enable_holepunch: false,
                enable_ipv6: false,
                alert_channel_size: 64,
                disk_io_threads: 2,
                storage_mode: StorageMode::Sparse,
                disk_cache_size: 1024 * 1024,
                ..Settings::default()
            };

            let session = SessionHandle::start_with_transport(settings, factory.clone())
                .await
                .expect("failed to start simulated session");

            node_ips.push(ip);
            factories.push(factory);
            sessions.push(session);
        }

        SimSwarm {
            network,
            sessions,
            node_ips,
            factories,
        }
    }
}

// ---------------------------------------------------------------------------
// SimSwarm
// ---------------------------------------------------------------------------

/// A running simulated swarm of BitTorrent nodes.
///
/// Each node has its own [`SessionHandle`] and communicates through a shared
/// [`SimNetwork`] using in-memory duplex channels.
pub struct SimSwarm {
    network: SimNetwork,
    sessions: Vec<SessionHandle>,
    node_ips: Vec<IpAddr>,
    #[allow(dead_code)] // Retained for future use (e.g., manual connect calls)
    factories: Vec<Arc<NetworkFactory>>,
}

impl SimSwarm {
    /// Return a reference to the underlying simulated network.
    pub fn network(&self) -> &SimNetwork {
        &self.network
    }

    /// Return a reference to the session at the given index.
    ///
    /// # Panics
    ///
    /// Panics if `index >= num_peers`.
    pub fn session(&self, index: usize) -> &SessionHandle {
        &self.sessions[index]
    }

    /// Return the virtual IP of the node at the given index.
    ///
    /// # Panics
    ///
    /// Panics if `index >= num_peers`.
    pub fn node_ip(&self, index: usize) -> IpAddr {
        self.node_ips[index]
    }

    /// Shut down all sessions in the swarm.
    pub async fn shutdown(self) {
        for session in &self.sessions {
            let _ = session.shutdown().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Test torrent helper
// ---------------------------------------------------------------------------

/// Create a test torrent from raw data bytes.
///
/// Returns `(TorrentMetaV1, raw_bytes)` where `raw_bytes` is the bencoded
/// `.torrent` file content. Useful for seeding test data in simulation.
pub fn make_test_torrent(data: &[u8], piece_size: u64) -> (TorrentMetaV1, Vec<u8>) {
    use serde::Serialize;

    let mut pieces = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let end = (offset + piece_size as usize).min(data.len());
        let hash = ferrite_core::sha1(&data[offset..end]);
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
            name: "test",
            piece_length: piece_size,
            pieces: &pieces,
        },
    };

    let bytes = ferrite_bencode::to_bytes(&t).unwrap();
    let meta = ferrite_core::torrent_from_bytes(&bytes).unwrap();
    (meta, bytes)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[tokio::test]
    async fn sim_swarm_builder_creates_sessions() {
        let swarm = SimSwarmBuilder::new(3).build().await;

        // Verify 3 distinct node IPs
        let ips: HashSet<_> = (0..3).map(|i| swarm.node_ip(i)).collect();
        assert_eq!(ips.len(), 3, "all node IPs should be distinct");

        // Verify each session is accessible
        for i in 0..3 {
            let stats = swarm.session(i).session_stats().await.unwrap();
            assert_eq!(stats.active_torrents, 0);
        }

        swarm.shutdown().await;
    }

    #[test]
    fn make_test_torrent_produces_valid_metadata() {
        let data = vec![0xAB; 32768]; // 2 pieces at 16384
        let (meta, bytes) = make_test_torrent(&data, 16384);

        assert!(!bytes.is_empty());
        assert_eq!(meta.info.name, "test");
        assert_eq!(meta.info.piece_length, 16384);
        assert_eq!(meta.info.length, Some(32768));
        // 2 pieces = 40 bytes of SHA1 hashes
        assert_eq!(meta.info.pieces.len(), 40);
    }
}
