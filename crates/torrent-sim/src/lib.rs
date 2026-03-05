#![warn(missing_docs)]
//! In-process network simulation for deterministic BitTorrent swarm testing.
//!
//! This crate provides a simulated network layer that replaces real TCP/UDP
//! sockets with in-memory channels, enabling deterministic, fast integration
//! tests of multi-peer BitTorrent swarms without real network traffic.

/// Virtual clock for deterministic time control.
pub mod clock;
/// Virtual network with configurable link parameters.
pub mod network;
/// Transport factory bridging simulated network to session I/O.
pub mod transport;
/// Multi-node simulated swarm builder.
pub mod swarm;

pub use clock::SimClock;
pub use network::{SimNetwork, LinkConfig};
pub use transport::sim_transport_factory;
pub use swarm::{SimSwarm, SimSwarmBuilder, make_test_torrent, make_seeded_storage};
