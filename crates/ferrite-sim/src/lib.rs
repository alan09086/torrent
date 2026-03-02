//! In-process network simulation for ferrite swarm integration testing.
//!
//! This crate provides a simulated network layer that replaces real TCP/UDP
//! sockets with in-memory channels, enabling deterministic, fast integration
//! tests of multi-peer BitTorrent swarms without real network traffic.

pub mod clock;
pub mod network;
pub mod transport;
pub mod swarm;

pub use clock::SimClock;
pub use network::{SimNetwork, LinkConfig};
pub use transport::sim_transport_factory;
pub use swarm::{SimSwarm, SimSwarmBuilder, make_test_torrent};
